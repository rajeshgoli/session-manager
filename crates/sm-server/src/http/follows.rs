//! Owner follows (sm#1569): follow an agent or a queue job from the app and
//! get one phone notification when it finishes. Routes, the owner guard,
//! and the background pass that fires and delivers follows.

use super::*;
use crate::email::{EmailBridge, RegisteredEmailUser};
use crate::owner_docs::{doc_readable_path, OwnerDocStore};
use crate::owner_push::{
    self, follow_message_text, Follow, FollowMailer, FollowTarget, FollowWorld, JobView,
    Notification, OwnerPushStore, PushSender, PushTokenRegistration, ReportView, SessionView,
    TARGET_QUEUE_JOB, TARGET_SESSION,
};

const MAX_PUSH_TOKEN_CHARS: usize = 4096;
const MAX_FOLLOW_MESSAGE_CHARS: usize = 8000;
const MAX_DEVICE_FIELD_CHARS: usize = 200;

pub(super) fn push_store(state: &AppState) -> OwnerPushStore {
    OwnerPushStore::new(owner_push::push_db_path(&state.config))
}

/// The same guard chain as `POST /client/request-status`, returning the
/// follow owner: the signed-in email, or for a local call the first
/// allowlisted Google email.
fn owner_guard(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    method: &str,
    uri: &Uri,
) -> Result<String, ApiError> {
    let request_target = request_target_from_uri(uri);
    let access_context =
        ensure_mobile_cloudflare_access_from_parts(state, headers, Some(peer_addr))?;
    ensure_public_edge_assertion_from_parts(
        state,
        headers,
        Some(peer_addr),
        method,
        &request_target,
    )?;
    ensure_session_allowed_from_parts(&state.config, headers, Some(peer_addr), uri.path())?;
    let actor = request_actor_email_from_parts(&state.config, headers, Some(peer_addr));
    ensure_mobile_cloudflare_access_context_matches_optional_actor(
        state,
        access_context.as_ref(),
        actor.as_deref(),
    )?;
    Ok(follow_owner_id(&state.config, actor.as_deref()))
}

fn follow_owner_id(config: &AppConfig, actor: Option<&str>) -> String {
    match actor {
        Some(actor) if actor != LOCAL_BYPASS_ACTOR => actor.trim().to_ascii_lowercase(),
        _ => config
            .google_auth
            .allowlist_emails
            .iter()
            .map(|email| email.trim().to_ascii_lowercase())
            .find(|email| !email.is_empty())
            .unwrap_or_else(|| LOCAL_BYPASS_ACTOR.to_owned()),
    }
}

fn bad_request(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: detail.into(),
    }
}

fn conflict(detail: &str) -> ApiError {
    ApiError::Status {
        status: StatusCode::CONFLICT,
        detail: detail.to_owned(),
    }
}

fn follow_json(follow: &Follow) -> Result<Value, ApiError> {
    let mut value = serde_json::to_value(follow)?;
    value["state"] = json!(follow.state());
    Ok(value)
}

fn optional_field(value: Option<String>, name: &str) -> Result<Option<String>, ApiError> {
    let value = value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if value
        .as_ref()
        .is_some_and(|value| value.chars().count() > MAX_DEVICE_FIELD_CHARS)
    {
        return Err(bad_request(format!("{name} is too long")));
    }
    Ok(value)
}

#[derive(Debug, Deserialize)]
pub(super) struct PushTokenRequest {
    token: String,
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    device_name: Option<String>,
    #[serde(default)]
    app_version: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct DeletePushTokenRequest {
    token: String,
}

fn validated_token(token: &str) -> Result<String, ApiError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(bad_request("token is required"));
    }
    if token.chars().count() > MAX_PUSH_TOKEN_CHARS {
        return Err(bad_request("token is too long"));
    }
    Ok(token.to_owned())
}

pub(super) async fn put_push_token(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(payload): Json<PushTokenRequest>,
) -> Result<StatusCode, ApiError> {
    let user_id = owner_guard(&state, &headers, peer_addr, "PUT", &uri)?;
    let registration = PushTokenRegistration {
        user_id,
        token: validated_token(&payload.token)?,
        device_id: optional_field(payload.device_id, "device_id")?,
        device_name: optional_field(payload.device_name, "device_name")?
            .unwrap_or_else(|| "phone".to_owned()),
        app_version: optional_field(payload.app_version, "app_version")?.unwrap_or_default(),
    };
    push_store(&state).upsert_token(&registration, OffsetDateTime::now_utc())?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn delete_push_token(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(payload): Json<DeletePushTokenRequest>,
) -> Result<StatusCode, ApiError> {
    let user_id = owner_guard(&state, &headers, peer_addr, "DELETE", &uri)?;
    push_store(&state).delete_token(&user_id, &validated_token(&payload.token)?)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn send_test_push(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user_id = owner_guard(&state, &headers, peer_addr, "POST", &uri)?;
    let Some(sender) = state.push_sender.clone() else {
        return Err(ApiError::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "push not configured".to_owned(),
        });
    };
    let store = push_store(&state);
    let hostname = host_name();
    let (sent, failed) = tokio::task::spawn_blocking(move || {
        owner_push::send_test(
            &store,
            sender.as_ref(),
            &user_id,
            &hostname,
            OffsetDateTime::now_utc(),
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("test push task failed: {error}"))??;
    Ok(Json(json!({
        "sent": sent,
        "failed": failed
            .into_iter()
            .map(|(device_name, error)| json!({"device_name": device_name, "error": error}))
            .collect::<Vec<_>>(),
    })))
}

fn host_name() -> String {
    Command::new("/bin/hostname")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "sm-server".to_owned())
}

#[derive(Debug, Deserialize, Default)]
pub(super) struct FollowSessionRequest {
    #[serde(default)]
    message: Option<String>,
}

pub(super) async fn follow_session(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Path(identifier): Path<String>,
    Json(payload): Json<FollowSessionRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let user_id = owner_guard(&state, &headers, peer_addr, "POST", &uri)?;
    ensure_core_writes_enabled(&state)?;
    let session = resolve_session_or_registry_role(&state, &identifier)?
        .ok_or(ApiError::NotFound("Session not found"))?;
    if session.is_stopped() {
        return Err(conflict("session stopped"));
    }
    let message = payload
        .message
        .map(|message| message.trim().to_owned())
        .filter(|message| !message.is_empty());
    if message
        .as_ref()
        .is_some_and(|message| message.chars().count() > MAX_FOLLOW_MESSAGE_CHARS)
    {
        return Err(bad_request("message is too long"));
    }
    let target = FollowTarget::Session {
        session_id: session.id.clone(),
        session_name: session_display_name(session.clone()),
    };
    let store = push_store(&state);
    let (follow, created) = store.create_follow(
        &user_id,
        &target,
        message.as_deref(),
        OffsetDateTime::now_utc(),
    )?;
    if !created {
        return Ok((StatusCode::OK, Json(follow_json(&follow)?)));
    }
    if let Some(message) = message {
        if let Err(error) = send_follow_message(&state, &session, &follow_message_text(&message)) {
            store.delete_follow(&follow.id)?;
            return Err(error);
        }
    }
    Ok((StatusCode::CREATED, Json(follow_json(&follow)?)))
}

/// Delivers the `[sm follow]` message the way `POST /client/request-status`
/// delivers its prompt: an important system message with no sender session.
fn send_follow_message(
    state: &AppState,
    session: &SessionRecord,
    text: &str,
) -> Result<(), ApiError> {
    let payload = SendCoreInputRequest {
        text: text.to_owned(),
        delivery_mode: "important".to_owned(),
        sender_session_id: None,
        from_sm_send: false,
        timeout_seconds: None,
        notify_on_delivery: false,
        notify_after_seconds: None,
        notify_on_stop: false,
        remind_soft_threshold: None,
        remind_hard_threshold: None,
        remind_cancel_on_reply_session_id: None,
        parent_session_id: None,
    };
    let runtime = (state.config.rust_core.runtime_enabled && is_primary_node(&session.node))
        .then(|| TmuxRuntime::from_app_config(&state.config));
    let outcome = match runtime.as_ref() {
        Some(runtime) => {
            state
                .session_store
                .send_core_input_with_runtime(&session.id, payload, runtime)?
        }
        None => state.session_store.send_core_input(&session.id, payload)?,
    };
    match outcome {
        None => Err(ApiError::NotFound("Session not found")),
        Some(result) if matches!(result.status.as_str(), "stopped" | "retired" | "killed") => {
            Err(conflict("session stopped"))
        }
        Some(_) => Ok(()),
    }
}

pub(super) async fn unfollow_session(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Path(identifier): Path<String>,
) -> Result<StatusCode, ApiError> {
    let user_id = owner_guard(&state, &headers, peer_addr, "DELETE", &uri)?;
    let session_id = resolve_session_or_registry_role(&state, &identifier)?
        .map(|session| session.id)
        .unwrap_or(identifier);
    push_store(&state).cancel_active(
        &user_id,
        TARGET_SESSION,
        &session_id,
        OffsetDateTime::now_utc(),
    )?;
    Ok(StatusCode::NO_CONTENT)
}

fn queue_runner_db_path(state: &AppState) -> PathBuf {
    let queue_state_dir_config = state.config.queue_runner_state_dir();
    expand_home(&queue_state_dir_config.to_string_lossy()).join("queue_runner.db")
}

pub(super) async fn follow_queue_job(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Path(identifier): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let user_id = owner_guard(&state, &headers, peer_addr, "POST", &uri)?;
    let job =
        RetainedQueueStore::resolve_queue_job_from_path(&queue_runner_db_path(&state), &identifier)
            .map_err(queue_lookup_error)?
            .ok_or(ApiError::NotFound("Queue job not found"))?;
    if !matches!(job.state.as_str(), "pending" | "running") {
        return Err(conflict("job finished"));
    }
    let session_id = job
        .requester_session_id
        .clone()
        .or_else(|| job.notify_session_id.clone())
        .unwrap_or_default();
    let session_name = state
        .session_store
        .get_session(&session_id)?
        .map(session_display_name)
        .unwrap_or_else(|| {
            if session_id.is_empty() {
                "unknown agent".to_owned()
            } else {
                session_id.clone()
            }
        });
    let job_label = if job.label.trim().is_empty() {
        job.id.clone()
    } else {
        job.label.clone()
    };
    let target = FollowTarget::QueueJob {
        job_id: job.id,
        job_label,
        session_id,
        session_name,
    };
    let (follow, created) =
        push_store(&state).create_follow(&user_id, &target, None, OffsetDateTime::now_utc())?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(follow_json(&follow)?)))
}

pub(super) async fn unfollow_queue_job(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Path(identifier): Path<String>,
) -> Result<StatusCode, ApiError> {
    let user_id = owner_guard(&state, &headers, peer_addr, "DELETE", &uri)?;
    let job_id =
        RetainedQueueStore::resolve_queue_job_from_path(&queue_runner_db_path(&state), &identifier)
            .ok()
            .flatten()
            .map(|job| job.id)
            .unwrap_or(identifier);
    push_store(&state).cancel_active(
        &user_id,
        TARGET_QUEUE_JOB,
        &job_id,
        OffsetDateTime::now_utc(),
    )?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn list_follows(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user_id = owner_guard(&state, &headers, peer_addr, "GET", &uri)?;
    let follows = push_store(&state)
        .list_for_owner(&user_id, OffsetDateTime::now_utc())?
        .iter()
        .map(follow_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({
        "push_configured": state.push_sender.is_some(),
        "follows": follows,
    })))
}

pub(super) async fn ack_follow(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Path(follow_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let user_id = owner_guard(&state, &headers, peer_addr, "POST", &uri)?;
    if push_store(&state).ack(&user_id, &follow_id, OffsetDateTime::now_utc())? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound("Follow not found"))
    }
}

/// The server as the follow worker sees it.
struct AppFollowWorld<'a> {
    state: &'a AppState,
}

impl FollowWorld for AppFollowWorld<'_> {
    fn sessions(&self) -> anyhow::Result<Vec<SessionView>> {
        Ok(self
            .state
            .session_store
            .list_sessions(true)?
            .into_iter()
            .map(|session| SessionView {
                id: session.id.clone(),
                stopped: session.is_stopped(),
                task_completed_at: session.agent_task_completed_at.clone(),
                name: session_display_name(session),
            })
            .collect())
    }

    fn job(&self, job_id: &str) -> anyhow::Result<Option<JobView>> {
        Ok(RetainedQueueStore::get_queue_job_strict_from_path(
            &queue_runner_db_path(self.state),
            job_id,
        )?
        .map(|job| JobView {
            id: job.id,
            label: job.label,
            state: job.state,
            exit_code: job.exit_code,
            started_at: job.started_at,
            finished_at: job.finished_at,
        }))
    }

    fn reports(&self, session_id: &str) -> anyhow::Result<Vec<ReportView>> {
        Ok(
            OwnerDocStore::new(expand_home(&self.state.config.sm_send.db_path))
                .publishes_by_session(session_id, owner_push::REPORT_SCAN_LIMIT)?
                .into_iter()
                .map(|(doc, publish)| ReportView {
                    reader_path: doc_readable_path(&doc.repo, &doc.path, &publish.commit_sha),
                    doc_id: doc.id,
                    title: doc.title,
                    published_at: publish.published_at,
                })
                .collect(),
        )
    }
}

/// Follow emails go through the email bridge, sent as the followed agent so
/// a reply routes to it.
struct AppFollowMailer<'a> {
    state: &'a AppState,
}

impl FollowMailer for AppFollowMailer<'_> {
    fn send(&self, follow: &Follow, notification: &Notification) -> anyhow::Result<()> {
        let bridge = EmailBridge::load(&self.state.config)?;
        if !bridge.bridge_is_available() {
            anyhow::bail!("{}", bridge.availability_error_detail());
        }
        let recipient = owner_email_recipient(&bridge, &follow.user_id)?
            .ok_or_else(|| anyhow::anyhow!("no email address configured for {}", follow.user_id))?;
        let mut lines = vec![notification.body.clone(), String::new()];
        if let Some(reader_path) = &notification.reader_path {
            let base = docs::doc_browser_base_url(&self.state.config).unwrap_or_default();
            lines.push(format!("Open the report: {base}{reader_path}"));
        }
        if let (Some(job_id), Some(label)) = (&follow.job_id, &follow.job_label) {
            lines.push(format!("Job: {label} ({job_id})"));
        }
        lines.push(format!(
            "Agent: {} ({})",
            follow.session_name, follow.session_id
        ));
        let provider = self
            .state
            .session_store
            .get_session(&follow.session_id)
            .ok()
            .flatten()
            .map(|session| session.provider)
            .unwrap_or_else(|| "unknown".to_owned());
        bridge.send_agent_email(SendAgentEmailRequest {
            sender_session_id: follow.session_id.clone(),
            sender_name: follow.session_name.clone(),
            sender_provider: provider,
            to_users: vec![recipient],
            cc_users: Vec::new(),
            subject: Some(notification.title.clone()),
            body_text: lines.join("\n"),
            body_html: String::new(),
            body_markdown: false,
            auto_subject: false,
        })?;
        Ok(())
    }
}

/// The configured human whose email matches the follow owner, or the only
/// human with an email address when the owner is not an email.
fn owner_email_recipient(
    bridge: &EmailBridge,
    user_id: &str,
) -> anyhow::Result<Option<RegisteredEmailUser>> {
    let mut candidates = Vec::new();
    for human in bridge.list_humans() {
        if let Some(user) = bridge.lookup_human_email_user(&human.name)? {
            if user.email.eq_ignore_ascii_case(user_id) {
                return Ok(Some(user));
            }
            candidates.push(user);
        }
    }
    Ok((!user_id.contains('@') && candidates.len() == 1).then(|| candidates.remove(0)))
}

impl AppState {
    /// One pass of the follow worker: fire finished targets (when `sweep`),
    /// then deliver. Returns problems worth logging.
    pub fn run_follow_pass(&self, sweep: bool) -> anyhow::Result<Vec<String>> {
        let store = push_store(self);
        let world = AppFollowWorld { state: self };
        let now = OffsetDateTime::now_utc();
        if sweep {
            owner_push::sweep(&store, &world, now)?;
        }
        owner_push::deliver(
            &store,
            &world,
            self.push_sender.as_deref(),
            &AppFollowMailer { state: self },
            now,
        )
    }

    pub fn with_push_sender(mut self, sender: Option<Arc<dyn PushSender>>) -> Self {
        self.push_sender = sender;
        self
    }
}
