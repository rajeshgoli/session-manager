//! Owner follows (sm#1569): follow an agent or a queue job from the app and
//! get one phone notification when it finishes. Routes, the owner guard,
//! and the background pass that fires and delivers follows.

use super::*;
use crate::email::{EmailBridge, RegisteredEmailUser};
use crate::owner_docs::{doc_readable_path, OwnerDocStore};
use crate::owner_docs::{OwnerDoc, OwnerDocPublish, OwnerDocState};
use crate::owner_messages::{derive_message_state, OwnerMessage, OwnerMessageState};
use crate::owner_push::{
    self, follow_message_text, Follow, FollowMailer, FollowTarget, FollowWorld, JobView, MailError,
    NewNotice, Notice, NoticeMailer, NoticeWorld, Notification, OwnerPushStore, PushSender,
    PushTokenRegistration, ReportView, SessionView, NOTICE_MESSAGE, NOTICE_REVIEW_REQUESTED,
    TARGET_QUEUE_JOB, TARGET_SESSION,
};

const MAX_PUSH_TOKEN_CHARS: usize = 4096;
const MAX_FOLLOW_MESSAGE_CHARS: usize = 8000;
const MAX_DEVICE_FIELD_CHARS: usize = 200;

pub(super) fn push_store(state: &AppState) -> OwnerPushStore {
    OwnerPushStore::new(owner_push::push_db_path(&state.config))
}

/// The owner write guard chain (Cloudflare Access, public edge assertion,
/// owner bearer or local), returning the follow owner: the signed-in email,
/// or for a local call the first allowlisted Google email.
pub(super) fn owner_guard(
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

/// `owner_guard`, also accepting the owner's browser login (spec 1710 D3).
pub(super) fn owner_web_or_guard(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    method: &str,
    uri: &Uri,
) -> Result<String, ApiError> {
    if let Some(email) = super::owner_web_guard(state, headers, Some(peer_addr), method)? {
        return Ok(follow_owner_id(&state.config, Some(&email)));
    }
    owner_guard(state, headers, peer_addr, method, uri)
}

pub(super) fn follow_owner_id(config: &AppConfig, actor: Option<&str>) -> String {
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
    let user_id = owner_web_or_guard(&state, &headers, peer_addr, "POST", &uri)?;
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

/// Delivers the `[sm follow]` message as an important system message with no
/// sender session.
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
    let user_id = owner_web_or_guard(&state, &headers, peer_addr, "DELETE", &uri)?;
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
    let user_id = owner_web_or_guard(&state, &headers, peer_addr, "POST", &uri)?;
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
    let user_id = owner_web_or_guard(&state, &headers, peer_addr, "DELETE", &uri)?;
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
    let user_id = owner_web_or_guard(&state, &headers, peer_addr, "GET", &uri)?;
    let follows = push_store(&state)
        .list_for_owner(&user_id, OffsetDateTime::now_utc())?
        .iter()
        .map(follow_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({
        "push_configured": state.push_sender.is_some(),
        "owner_name": state.config.owner_name,
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
    let user_id = owner_web_or_guard(&state, &headers, peer_addr, "POST", &uri)?;
    if push_store(&state).ack(&user_id, &follow_id, OffsetDateTime::now_utc())? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound("Follow not found"))
    }
}

/// `POST /client/notices/{id}/ack`: the phone showed the notice (sm#1580).
pub(super) async fn ack_notice(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Path(notice_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let user_id = owner_guard(&state, &headers, peer_addr, "POST", &uri)?;
    if push_store(&state).ack_notice(&user_id, &notice_id, OffsetDateTime::now_utc())? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound("Notice not found"))
    }
}

/// `GET /client/notices`: the owner's notices from the last 7 days.
pub(super) async fn list_notices(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user_id = owner_guard(&state, &headers, peer_addr, "GET", &uri)?;
    let notices = push_store(&state).list_notices(&user_id, OffsetDateTime::now_utc())?;
    Ok(Json(json!({ "notices": notices })))
}

/// Whom a notice goes to (appendix F1): the identity the app's push tokens
/// are registered under. The human's email when it is an allowlisted
/// Google email, else the first allowlisted email, else the local bypass.
fn notice_user_id(state: &AppState, human: Option<&str>) -> String {
    let email = EmailBridge::load(&state.config).ok().and_then(|bridge| {
        let name = match human {
            Some(name) => name.to_owned(),
            None => bridge.list_humans().first()?.name.clone(),
        };
        bridge.lookup_human_email_user(&name).ok().flatten()
    });
    let allowlisted = email
        .map(|user| user.email.trim().to_ascii_lowercase())
        .filter(|email| {
            state
                .config
                .google_auth
                .allowlist_emails
                .iter()
                .any(|allowed| allowed.trim().eq_ignore_ascii_case(email))
        });
    allowlisted.unwrap_or_else(|| follow_owner_id(&state.config, None))
}

fn message_notice(state: &AppState, message: &OwnerMessage) -> NewNotice {
    NewNotice::message(
        &notice_user_id(state, Some(&message.human)),
        &message.sender_session_id,
        &message.sender_session_name,
        &message.id,
        &message.title,
        message.blocking,
    )
}

fn review_notice(state: &AppState, doc: &OwnerDoc, publish: &OwnerDocPublish) -> NewNotice {
    let session_name = state
        .session_store
        .get_session(&publish.session_id)
        .ok()
        .flatten()
        .map(session_display_name)
        .or_else(|| doc.author_session_name.clone())
        .unwrap_or_else(|| publish.session_id.clone());
    NewNotice::review_requested(
        &notice_user_id(state, None),
        &publish.session_id,
        &session_name,
        publish.id,
        &doc.title,
        &crate::owner_docs::doc_readable_path(&doc.repo, &doc.path, &publish.commit_sha),
    )
}

/// The notice for a new message. A failure is logged: the repair pass
/// creates the notice later.
pub(super) fn notice_new_message(state: &AppState, message: &OwnerMessage) {
    let notice = message_notice(state, message);
    if let Err(error) = push_store(state).create_notice(&notice, OffsetDateTime::now_utc()) {
        eprintln!(
            "Owner message {}: notice insert failed: {error:#}",
            message.id
        );
    }
}

/// The notice for a `--review` publish; as for messages, failures wait for
/// the repair pass.
pub(super) fn notice_review_publish(state: &AppState, doc: &OwnerDoc, publish: &OwnerDocPublish) {
    let notice = review_notice(state, doc, publish);
    if let Err(error) = push_store(state).create_notice(&notice, OffsetDateTime::now_utc()) {
        eprintln!(
            "Owner doc {}: review notice insert failed: {error:#}",
            doc.id
        );
    }
}

/// The server as notice delivery sees it.
struct AppNoticeWorld<'a> {
    state: &'a AppState,
}

impl AppNoticeWorld<'_> {
    fn message(&self, notice: &Notice) -> anyhow::Result<Option<OwnerMessage>> {
        super::messages::owner_message_store(self.state).get(&notice.subject_id)
    }
}

impl NoticeWorld for AppNoticeWorld<'_> {
    fn still_wanted(&self, notice: &Notice) -> anyhow::Result<bool> {
        match notice.kind.as_str() {
            NOTICE_MESSAGE => {
                let Some(message) = self.message(notice)? else {
                    return Ok(false);
                };
                let store = super::messages::owner_message_store(self.state);
                let sender_ended = self
                    .state
                    .session_store
                    .get_session(&message.sender_session_id)?
                    .is_none_or(|session| super::messages::session_ended(&session));
                Ok(matches!(
                    derive_message_state(&message, store.has_reply(&message.id)?, sender_ended),
                    OwnerMessageState::New | OwnerMessageState::Read | OwnerMessageState::NeedsYou
                ))
            }
            NOTICE_REVIEW_REQUESTED => {
                let Ok(publish_id) = notice.subject_id.parse::<i64>() else {
                    return Ok(false);
                };
                let store = OwnerDocStore::new(expand_home(&self.state.config.sm_send.db_path));
                let Some((doc, _)) = store.publish_by_id(publish_id)? else {
                    return Ok(false);
                };
                let latest = store.publishes(&doc.id)?.last().map(|publish| publish.id);
                Ok(latest == Some(publish_id)
                    && store
                        .summary(&doc.id)?
                        .is_some_and(|summary| summary.state == OwnerDocState::ReviewRequested))
            }
            crate::board::NOTICE_BOARD_READY => {
                super::board::board_store(self.state).ready_notice_wanted(&notice.subject_id)
            }
            // The lane has ended already: only opening the board clears it.
            crate::board::NOTICE_BOARD_LANE_DONE => Ok(true),
            _ => Ok(false),
        }
    }

    fn opened(&self, notice: &Notice) -> anyhow::Result<bool> {
        match notice.kind.as_str() {
            NOTICE_MESSAGE => Ok(self
                .message(notice)?
                .is_some_and(|message| message.first_viewed_at.is_some())),
            NOTICE_REVIEW_REQUESTED => {
                let Ok(publish_id) = notice.subject_id.parse::<i64>() else {
                    return Ok(false);
                };
                let store = OwnerDocStore::new(expand_home(&self.state.config.sm_send.db_path));
                let Some((doc, publish)) = store.publish_by_id(publish_id)? else {
                    return Ok(false);
                };
                Ok(store
                    .last_viewed_at(&doc.id, &publish.blob_sha)?
                    .as_deref()
                    .and_then(owner_push::parse_ts)
                    .zip(owner_push::parse_ts(&notice.created_at))
                    .is_some_and(|(viewed_at, created_at)| viewed_at >= created_at))
            }
            crate::board::NOTICE_BOARD_READY | crate::board::NOTICE_BOARD_LANE_DONE => {
                let seen = super::board::board_store(self.state).seen(&notice.user_id)?;
                Ok(crate::board::pushes::notice_opened(
                    &notice.subject_id,
                    &notice.created_at,
                    seen.as_ref(),
                ))
            }
            _ => Ok(false),
        }
    }

    fn unread_count(&self, notice: &Notice) -> anyhow::Result<i64> {
        let Some(message) = self.message(notice)? else {
            return Ok(0);
        };
        super::messages::owner_message_store(self.state)
            .unread_count(&message.sender_session_id, &message.human)
    }

    fn notice_candidates(&self, since: OffsetDateTime) -> anyhow::Result<Vec<NewNotice>> {
        let since = owner_push::format_ts(since);
        let mut candidates: Vec<NewNotice> = super::messages::owner_message_store(self.state)
            .created_since(&since)?
            .iter()
            .map(|message| message_notice(self.state, message))
            .collect();
        candidates.extend(
            OwnerDocStore::new(expand_home(&self.state.config.sm_send.db_path))
                .review_publishes_since(&since)?
                .iter()
                .map(|(doc, publish)| review_notice(self.state, doc, publish)),
        );
        Ok(candidates)
    }
}

/// The fallback email's body (appendix F4): the message markdown, or the
/// doc title, then the link.
fn notice_email_body(notice: &Notice, message_markdown: Option<&str>, base_url: &str) -> String {
    let lead = match message_markdown {
        Some(markdown) => markdown.to_owned(),
        None => notice.body.clone(),
    };
    format!("{lead}\n\nOpen in sm: {base_url}{}", notice.reader_path)
}

/// Notice emails go through the email bridge as the agent, so a reply to
/// the email reaches it.
struct AppNoticeMailer<'a> {
    state: &'a AppState,
}

impl NoticeMailer for AppNoticeMailer<'_> {
    fn send(&self, notice: &Notice) -> Result<(), MailError> {
        let unavailable = |detail: String| MailError::Unavailable(detail);
        let bridge = EmailBridge::load(&self.state.config)
            .map_err(|error| unavailable(format!("{error:#}")))?;
        if !bridge.bridge_is_available() {
            return Err(unavailable(bridge.availability_error_detail()));
        }
        let recipient = owner_email_recipient(&bridge, &notice.user_id)
            .map_err(|error| unavailable(format!("{error:#}")))?
            .ok_or_else(|| {
                unavailable(format!(
                    "no email address configured for {}",
                    notice.user_id
                ))
            })?;
        let markdown = if notice.kind == NOTICE_MESSAGE {
            super::messages::owner_message_store(self.state)
                .get(&notice.subject_id)
                .map_err(|error| MailError::Transient(format!("{error:#}")))?
                .map(|message| message.body_markdown)
        } else {
            None
        };
        let base = docs::doc_browser_base_url(&self.state.config).unwrap_or_default();
        let provider = self
            .state
            .session_store
            .get_session(&notice.session_id)
            .ok()
            .flatten()
            .map(|session| session.provider)
            .unwrap_or_else(|| "unknown".to_owned());
        bridge
            .send_agent_email(SendAgentEmailRequest {
                sender_session_id: notice.session_id.clone(),
                sender_name: notice.session_name.clone(),
                sender_provider: provider,
                to_users: vec![recipient],
                cc_users: Vec::new(),
                subject: Some(notice.email_subject()),
                body_text: notice_email_body(notice, markdown.as_deref(), &base),
                body_html: String::new(),
                body_markdown: markdown.is_some(),
                auto_subject: false,
            })
            .map_err(|error| MailError::Transient(format!("{error:#}")))?;
        Ok(())
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
    fn send(&self, follow: &Follow, notification: &Notification) -> Result<(), MailError> {
        let unavailable = |detail: String| MailError::Unavailable(detail);
        let bridge = EmailBridge::load(&self.state.config)
            .map_err(|error| unavailable(format!("{error:#}")))?;
        if !bridge.bridge_is_available() {
            return Err(unavailable(bridge.availability_error_detail()));
        }
        let recipient = owner_email_recipient(&bridge, &follow.user_id)
            .map_err(|error| unavailable(format!("{error:#}")))?
            .ok_or_else(|| {
                unavailable(format!(
                    "no email address configured for {}",
                    follow.user_id
                ))
            })?;
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
        bridge
            .send_agent_email(SendAgentEmailRequest {
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
            })
            .map_err(|error| MailError::Transient(format!("{error:#}")))?;
        Ok(())
    }
}

/// The configured human whose email matches the follow owner, or the only
/// human with an email address when the owner is not an email.
pub(super) fn owner_email_recipient(
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
        let mut problems = owner_push::deliver(
            &store,
            &world,
            self.push_sender.as_deref(),
            &AppFollowMailer { state: self },
            now,
        )?;
        // Notices ride the same pass, after follows (sm#1580); the repair
        // pass runs on sweep passes, every third.
        let notice_world = AppNoticeWorld { state: self };
        if sweep {
            owner_push::repair_notices(&store, &notice_world, now)?;
        }
        problems.extend(owner_push::deliver_notices(
            &store,
            &notice_world,
            self.push_sender.as_deref(),
            &AppNoticeMailer { state: self },
            now,
        )?);
        problems.extend(owner_push::withdraw_notices(
            &store,
            &notice_world,
            self.push_sender.as_deref(),
            now,
        )?);
        Ok(problems)
    }

    pub fn with_push_sender(mut self, sender: Option<Arc<dyn PushSender>>) -> Self {
        self.push_sender = sender;
        self
    }
}

/// `GET /client/queue/jobs/{id}/start-check`: what Start now would override
/// (sm#1627).
pub(super) async fn queue_job_start_check(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Path(identifier): Path<String>,
) -> Result<Json<Value>, ApiError> {
    owner_web_or_guard(&state, &headers, peer_addr, "GET", &uri)?;
    let job =
        RetainedQueueStore::resolve_queue_job_from_path(&queue_runner_db_path(&state), &identifier)
            .map_err(queue_lookup_error)?
            .ok_or(ApiError::NotFound("Queue job not found"))?;
    let queue_state_dir = expand_home(&state.config.queue_runner_state_dir().to_string_lossy());
    let utilization_db = expand_home(&state.config.utilization.db_path);
    let check = RetainedQueueStore::start_check_in_state_dir(
        &queue_state_dir,
        &job.id,
        queue_admission_policy(&state),
        |earlier| {
            crate::utilization::peak_running_rss(&utilization_db, earlier).unwrap_or_else(|error| {
                eprintln!("start check could not read past runs: {error:#}");
                None
            })
        },
    )?
    .ok_or(ApiError::NotFound("Queue job not found"))?;
    Ok(Json(
        serde_json::to_value(check).map_err(anyhow::Error::from)?,
    ))
}

/// `POST /client/queue/jobs/{id}/start`: the owner starts a queued job now,
/// past every admission rule (sm#1627). Only a signed-in owner may: agents on
/// this Mac reach sm as unauthenticated local requests and are refused.
pub(super) async fn force_start_queue_job(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Path(identifier): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if owner_web_guard(&state, &headers, Some(peer_addr), "POST")?.is_none() {
        owner_guard(&state, &headers, peer_addr, "POST", &uri)?;
        if authenticated_user(&headers, &state.config).is_none() {
            return Err(ApiError::Status {
                status: StatusCode::FORBIDDEN,
                detail: "Start now is for the owner, signed in to the sm app".to_owned(),
            });
        }
    }
    if !state.config.rust_core.runtime_enabled {
        return Err(conflict("the queue runtime is off on this server"));
    }
    let job =
        RetainedQueueStore::resolve_queue_job_from_path(&queue_runner_db_path(&state), &identifier)
            .map_err(queue_lookup_error)?
            .ok_or(ApiError::NotFound("Queue job not found"))?;
    let queue_state_dir = expand_home(&state.config.queue_runner_state_dir().to_string_lossy());
    let message_queue_db_path = expand_home(&state.config.sm_send.db_path);
    let (started, by_this_call) = RetainedQueueStore::force_start_queue_job_in_state_dir(
        &queue_state_dir,
        &message_queue_db_path,
        &job.id,
        state.config.queue_runner.cancel_grace_seconds,
        queue_admission_policy(&state),
    )?
    .ok_or(ApiError::NotFound("Queue job not found"))?;
    if !by_this_call {
        return Err(conflict(&format!(
            "job is no longer queued ({})",
            started.state
        )));
    }
    Ok(Json(queue_job_response(&state, started)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(kind: &str, reader_path: &str, body: &str) -> Notice {
        Notice {
            id: "not_aaaaaaaaaaaa".into(),
            user_id: "owner@example.com".into(),
            kind: kind.into(),
            session_id: "eng00001".into(),
            session_name: "sm-1679-engineer".into(),
            subject_id: "x".into(),
            title: "sm-1679-engineer needs you".into(),
            body: body.into(),
            reader_path: reader_path.into(),
            blocking: true,
            created_at: String::new(),
            notify_after: String::new(),
            push_attempts: 0,
            last_push_error: None,
            notified_at: None,
            notified_via: None,
            acked_at: None,
            email_sent_at: None,
        }
    }

    #[test]
    fn notice_email_shape() {
        let message = notice(
            NOTICE_MESSAGE,
            "/messages/msg_3f9a2c1d",
            "Keep the old fills table?",
        );
        assert_eq!(
            message.email_subject(),
            "sm-1679-engineer needs you: Keep the old fills table?"
        );
        assert_eq!(
            notice_email_body(
                &message,
                Some("# Keep the old fills table?\nDrop it after a week."),
                "https://sm.example.com"
            ),
            "# Keep the old fills table?\nDrop it after a week.\n\nOpen in sm: https://sm.example.com/messages/msg_3f9a2c1d"
        );
        let mut doc = notice(
            NOTICE_REVIEW_REQUESTED,
            "/docs/widgets/memo.html?version=aaaaaaaaaaaa",
            "Decision memo",
        );
        doc.title = "sm-1679-engineer asks for your review".into();
        assert_eq!(
            doc.email_subject(),
            "sm-1679-engineer asks for your review: Decision memo"
        );
        assert_eq!(
            notice_email_body(&doc, None, "https://sm.example.com"),
            "Decision memo\n\nOpen in sm: https://sm.example.com/docs/widgets/memo.html?version=aaaaaaaaaaaa"
        );
    }
}
