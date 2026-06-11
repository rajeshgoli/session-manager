#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    fs,
    net::SocketAddr,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use axum::{
    body::{to_bytes, Body},
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, DefaultBodyLimit, FromRequestParts, Multipart, Path, Query, Request, State,
    },
    http::{
        header::{
            AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_DISPOSITION, CONTENT_TYPE, COOKIE,
            HOST, LOCATION, UPGRADE,
        },
        HeaderMap, StatusCode, Uri,
    },
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post, put},
    Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use futures_util::{
    stream::{self, StreamExt},
    SinkExt,
};
use hmac::{Hmac, Mac};
#[cfg(unix)]
use nix::{
    errno::Errno,
    fcntl::{fcntl, FcntlArg, OFlag},
    pty::{openpty, Winsize},
    unistd::{dup, read as nix_read, write as nix_write},
};
use p256::{
    ecdsa::{signature::Verifier, Signature, VerifyingKey},
    pkcs8::DecodePublicKey,
};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::app_artifacts::{
    hashed_path, read_metadata, store_artifact, valid_app_name, valid_artifact_hash,
    APP_ARTIFACT_MAX_SIZE_BYTES,
};
use crate::bug_reports::{BugReportStore, CreateBugReport};
use crate::codex_events::{list_codex_events_from_path, CodexEventsResponse};
use crate::codex_requests::{list_codex_pending_requests_from_path, CodexPendingRequestsResponse};
use crate::config::{
    trimmed, AppConfig, MobileTerminalDeviceKeyConfig, MobileTerminalUserConfig, PublicNodeConfig,
};
use crate::email::{
    extract_reply_message_body, extract_routed_session_id, extract_subject_from_raw_email,
    extract_text_from_raw_email, normalize_explicit_session_id, EmailBridge,
    HumanRecipientResponse, SendAgentEmailRequest, DEFAULT_EMAIL_WEBHOOK_PATH,
};
use crate::mobile_analytics::build_mobile_analytics_summary;
use crate::queue::{
    CodexReviewRequestFilters, CodexReviewRequestRegistration, QueueJobFilters, QueueJobRecord,
    RetainedQueueStore,
};
use crate::runtime::TmuxRuntime;
use crate::sessions::{
    expand_home, is_primary_node, AgentStatusRequest, ArmStopNotifyOutcome, ArmStopNotifyRequest,
    ClearSessionRequest, ClientSessionResponse, ContextMonitorOutcome, ContextMonitorRequest,
    CoreClearOutcome, CoreInputBatchResponse, CoreInputBatchResult, CoreRestoreOutcome,
    CoreRetireOutcome, CreateCoreSessionRequest, HandoffOutcome, HandoffRequest,
    MaintainerMutationOutcome, RegistryMutationOutcome, RoleRegistrationRequest,
    SendCoreInputBatchRequest, SendCoreInputRequest, SessionRecord, SessionResponse, SessionStore,
    SessionsEnvelope, SetMaintainerRequest, SubagentStartOutcome, SubagentStartRequest,
    SubagentStopOutcome, SubagentStopRequest, TaskCompleteOutcome, TaskCompleteRequest,
    TurnCompleteOutcome,
};
use crate::tool_usage::{
    list_recent_codex_fork_tool_calls_from_path, list_recent_tool_calls_from_path, ToolCallRow,
};

const SESSION_COOKIE_NAME: &str = "sm_auth";
const SESSION_COOKIE_MAX_AGE_SECONDS: i64 = 60 * 60 * 24 * 14;
const SHADOW_ENVELOPE_MAX_BYTES: usize = 1024 * 1024;
const EM_SPAWN_STOP_NOTIFY_DELAY_SECONDS: i64 = 8;
const REQUEST_STATUS_PROMPT: &str = "[sm] user requests status, please update now using sm status";
const BUG_REPORT_MAX_TEXT_CHARS: usize = 4000;
const BUG_REPORT_MAX_CLIENT_STATE_CHARS: usize = 100_000;
const BUG_REPORT_MAX_SERVER_STATE_CHARS: usize = 200_000;
const MOBILE_TERMINAL_DEFAULT_ROWS: u16 = 24;
const MOBILE_TERMINAL_DEFAULT_COLS: u16 = 80;
const MOBILE_TERMINAL_MIN_ROWS: u16 = 2;
const MOBILE_TERMINAL_MIN_COLS: u16 = 10;
const MOBILE_TERMINAL_MAX_ROWS: u16 = 120;
const MOBILE_TERMINAL_MAX_COLS: u16 = 300;
const MOBILE_TERMINAL_INPUT_MAX_CHARS: usize = 8192;
const MOBILE_TERMINAL_INITIAL_RESIZE_WAIT_SECONDS: f64 = 2.0;
const MOBILE_TERMINAL_MAX_ATTACH_SECONDS: u64 = 3600;

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct MobileTerminalTicket {
    ticket_id: String,
    secret_hash: String,
    user_id: String,
    actor_email: String,
    session_id: String,
    provider: String,
    node: String,
    tmux_session: String,
    tmux_socket_name: Option<String>,
    device_key_id: String,
    created_at_unix: i64,
    expires_at_unix: i64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct MobileTerminalActiveAttach {
    user_id: String,
    session_id: String,
    provider: String,
    device_key_id: String,
    started_at_unix: i64,
    stop: Arc<AtomicBool>,
}

#[derive(Debug, Serialize)]
struct MobileAttachTicketResponse {
    ticket_id: String,
    ticket_secret: String,
    device_key_id: String,
    ws_url: String,
    expires_at: String,
}

#[derive(Debug, Serialize)]
struct MobileTerminalDisableResponse {
    ok: bool,
    disabled: bool,
    active_attaches_terminated: usize,
}

#[derive(Debug, Serialize)]
struct MobileTerminalDeviceSummary {
    user_id: String,
    device_key_id: String,
    enabled: bool,
    revoked: bool,
}

#[derive(Debug, Serialize)]
struct MobileTerminalDeviceListResponse {
    devices: Vec<MobileTerminalDeviceSummary>,
    owner_view: bool,
    runtime_only_revocations: bool,
}

#[derive(Debug, Deserialize)]
struct MobileTerminalRevokeDeviceQuery {
    user_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct MobileTerminalRevokeDeviceResponse {
    ok: bool,
    revoked: bool,
    user_id: String,
    device_key_id: String,
    already_revoked: bool,
    pending_tickets_revoked: usize,
    active_attaches_terminated: usize,
    runtime_only: bool,
}

#[derive(Debug, Deserialize)]
struct MobileTerminalAuthFrame {
    #[serde(rename = "type")]
    frame_type: Option<String>,
    ticket_id: Option<String>,
    ticket_secret: Option<String>,
    device_key_id: Option<String>,
    nonce: Option<String>,
    signature: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    config: AppConfig,
    session_store: SessionStore,
    mobile_terminal_tickets: Arc<Mutex<BTreeMap<String, MobileTerminalTicket>>>,
    mobile_terminal_active_attaches: Arc<Mutex<BTreeMap<String, MobileTerminalActiveAttach>>>,
    mobile_terminal_proof_nonces: Arc<Mutex<BTreeMap<String, i64>>>,
    public_edge_assertion_nonces: Arc<Mutex<BTreeMap<String, i64>>>,
    mobile_terminal_revoked_keys: Arc<Mutex<BTreeSet<(String, String)>>>,
    mobile_terminal_runtime_disabled: Arc<AtomicBool>,
    mobile_terminal_secret: [u8; 32],
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        let state_file = expand_home(&config.paths.state_file);
        let queue_db_path = expand_home(&config.sm_send.db_path);
        let session_store = SessionStore::new_with_queue(state_file, queue_db_path);
        let mut mobile_terminal_secret = [0u8; 32];
        OsRng.fill_bytes(&mut mobile_terminal_secret);
        Self {
            config,
            session_store,
            mobile_terminal_tickets: Arc::new(Mutex::new(BTreeMap::new())),
            mobile_terminal_active_attaches: Arc::new(Mutex::new(BTreeMap::new())),
            mobile_terminal_proof_nonces: Arc::new(Mutex::new(BTreeMap::new())),
            public_edge_assertion_nonces: Arc::new(Mutex::new(BTreeMap::new())),
            mobile_terminal_revoked_keys: Arc::new(Mutex::new(BTreeSet::new())),
            mobile_terminal_runtime_disabled: Arc::new(AtomicBool::new(false)),
            mobile_terminal_secret,
        }
    }
}

pub fn router(state: AppState) -> Router {
    let inbound_email_alias = EmailBridge::load(&state.config)
        .ok()
        .map(|bridge| bridge.webhook_path())
        .unwrap_or_else(|| DEFAULT_EMAIL_WEBHOOK_PATH.to_owned());
    let mut app = Router::new()
        .route("/health", get(health))
        .route("/health/detailed", get(health_detailed))
        .route("/auth/session", get(auth_session))
        .route("/client/bootstrap", get(client_bootstrap))
        .route("/client/analytics/summary", get(client_analytics_summary))
        .route("/client/request-status", post(client_request_status))
        .route("/client/bug-reports", post(submit_client_bug_report))
        .route("/client/terminal", get(mobile_terminal_endpoint))
        .route(
            "/client/mobile-terminal/disable",
            post(disable_mobile_terminal),
        )
        .route(
            "/client/mobile-terminal/devices",
            get(list_mobile_terminal_devices),
        )
        .route(
            "/client/mobile-terminal/devices/{device_key_id}",
            delete(revoke_mobile_terminal_device),
        )
        .route("/deploy/{app_name}", post(deploy_app_artifact))
        .route("/apps/{app_name}/latest.apk", get(get_latest_app_artifact))
        .route("/apps/{app_name}/meta.json", get(get_app_artifact_metadata))
        .route(
            "/apps/{app_name}/{artifact_file}",
            get(get_hashed_app_artifact),
        )
        .route("/apk", get(get_legacy_apk_download))
        .route("/events/state", get(events_state))
        .route("/events", get(events_stream))
        .route("/__shadow/http", post(shadow_http))
        .route("/queue-jobs", get(list_queue_jobs))
        .route("/queue-jobs/{job_id}", get(get_queue_job))
        .route("/codex-review-requests", get(list_codex_review_requests))
        .route(
            "/codex-review-requests/{request_id}",
            get(get_codex_review_request),
        )
        .route("/nodes", get(list_nodes))
        .route("/humans", get(list_humans))
        .route("/humans/{identifier}", get(get_human))
        .route("/humans/{identifier}/email", post(send_human_email))
        .route("/email/send", post(send_registered_email))
        .route(DEFAULT_EMAIL_WEBHOOK_PATH, post(inbound_email_webhook))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/input-batch", post(send_session_input_batch))
        .route("/sessions/spawn", post(spawn_session))
        .route("/sessions/context-monitor", get(get_context_monitor_status))
        .route("/sessions/{session_id}", get(get_session))
        .route(
            "/sessions/{parent_session_id}/children",
            get(list_children_sessions),
        )
        .route(
            "/sessions/{session_id}/attach-descriptor",
            get(get_attach_descriptor),
        )
        .route(
            "/sessions/{session_id}/agent-status",
            post(set_agent_status),
        )
        .route("/sessions/{session_id}/task-complete", post(task_complete))
        .route("/sessions/{session_id}/turn-complete", post(turn_complete))
        .route(
            "/sessions/{session_id}/notify-on-stop",
            post(arm_stop_notify),
        )
        .route(
            "/sessions/{session_id}/context-monitor",
            post(set_context_monitor),
        )
        .route(
            "/sessions/{session_id}/subagents",
            get(list_subagents).post(register_subagent_start),
        )
        .route(
            "/sessions/{session_id}/subagents/{agent_id}/stop",
            post(register_subagent_stop),
        )
        .route("/sessions/{session_id}/input", post(send_session_input))
        .route("/sessions/{session_id}/kill", post(retire_session))
        .route("/sessions/{session_id}/restore", post(restore_session))
        .route("/sessions/{session_id}/clear", post(clear_session))
        .route("/sessions/{session_id}/handoff", post(schedule_handoff))
        .route(
            "/sessions/{session_id}/maintainer",
            put(set_maintainer).delete(clear_maintainer),
        )
        .route(
            "/sessions/{session_id}/registry",
            post(register_agent_role).delete(unregister_agent_role),
        )
        .route("/registry", get(list_agent_registry))
        .route("/registry/{role}", get(lookup_agent_registry))
        .route("/sessions/{session_id}/output", get(session_output))
        .route("/sessions/{session_id}/tool-calls", get(session_tool_calls))
        .route(
            "/sessions/{session_id}/codex-events",
            get(session_codex_events),
        )
        .route(
            "/sessions/{session_id}/codex-pending-requests",
            get(session_codex_pending_requests),
        )
        .route("/client/sessions", get(list_client_sessions))
        .route(
            "/client/sessions/{session_id}/attach-ticket",
            post(create_mobile_attach_ticket),
        )
        .route("/client/sessions/{session_id}", get(get_client_session))
        .fallback(not_found);
    if inbound_email_alias != DEFAULT_EMAIL_WEBHOOK_PATH {
        app = app.route(&inbound_email_alias, post(inbound_email_webhook));
    }
    app.layer(DefaultBodyLimit::max(
        APP_ARTIFACT_MAX_SIZE_BYTES + 1024 * 1024,
    ))
    .with_state(Arc::new(state))
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "healthy" }))
}

async fn health_detailed() -> Json<HealthDetailedResponse> {
    let mut checks = BTreeMap::new();
    checks.insert(
        "rust_server".to_owned(),
        HealthCheck {
            status: "ok",
            message: Some("Rust scaffold running".to_owned()),
        },
    );
    checks.insert(
        "state_ownership".to_owned(),
        HealthCheck {
            status: "ok",
            message: Some("No durable state ownership in this scaffold".to_owned()),
        },
    );

    Json(HealthDetailedResponse {
        status: "healthy",
        checks,
        resources: BTreeMap::new(),
        timestamp: now_rfc3339(),
    })
}

async fn auth_session(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Json<AuthSessionResponse> {
    let auth = &state.config.google_auth;
    if !auth.requested() {
        return Json(AuthSessionResponse::disabled_bypass());
    }
    let peer_addr = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|value| value.0);
    if is_local_bypass_request(request.headers(), peer_addr, &state.config) {
        return Json(AuthSessionResponse::enabled_bypass());
    }
    if !auth.ready() {
        return Json(AuthSessionResponse::misconfigured());
    }

    if let Some(user) = authenticated_user(request.headers(), &state.config) {
        return Json(AuthSessionResponse {
            enabled: true,
            authenticated: true,
            bypass: false,
            email: Some(user.email),
            name: user.name,
            auth_type: Some(user.auth_type),
            error: None,
        });
    }

    Json(AuthSessionResponse {
        enabled: true,
        authenticated: false,
        bypass: false,
        email: None,
        name: None,
        auth_type: None,
        error: None,
    })
}

async fn client_bootstrap(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<ClientBootstrapResponse>, ApiError> {
    ensure_public_edge_assertion_for_request(&state, &request)?;
    Ok(Json(client_bootstrap_response(
        &state.config,
        mobile_terminal_runtime_disabled(&state),
    )))
}

async fn client_analytics_summary(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_public_edge_assertion_for_request(&state, &request)?;
    ensure_session_read_allowed(&state, &request)?;
    Ok(Json(build_mobile_analytics_summary(
        &state.config,
        &state.session_store,
    )?))
}

async fn client_request_status(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<ClientRequestStatusResponse>, ApiError> {
    let request_target = request_target_from_uri(&uri);
    ensure_public_edge_assertion_from_parts(
        &state,
        &headers,
        Some(peer_addr),
        "POST",
        &request_target,
    )?;
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        "/client/request-status",
    )?;
    ensure_core_writes_enabled(&state)?;
    let sessions = state.session_store.list_sessions(false)?;
    let runtime = state
        .config
        .rust_core
        .runtime_enabled
        .then(|| TmuxRuntime::from_app_config(&state.config));
    let mut delivered_count = 0;
    let mut queued_count = 0;
    let mut failed_count = 0;
    let mut targeted_session_ids = Vec::with_capacity(sessions.len());
    for session in sessions {
        targeted_session_ids.push(session.id.clone());
        if session.provider == "codex-app" {
            failed_count += 1;
            continue;
        }
        let payload = SendCoreInputRequest {
            text: REQUEST_STATUS_PROMPT.to_owned(),
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
        let outcome = if let Some(runtime) = runtime.as_ref() {
            if !is_primary_node(&session.node) {
                failed_count += 1;
                continue;
            }
            state
                .session_store
                .send_core_input_with_runtime(&session.id, payload, runtime)?
        } else {
            state.session_store.send_core_input(&session.id, payload)?
        };
        match outcome {
            Some(result) if result.delivered => delivered_count += 1,
            Some(result) if !matches!(result.status.as_str(), "stopped" | "killed") => {
                queued_count += 1
            }
            _ => failed_count += 1,
        }
    }
    Ok(Json(ClientRequestStatusResponse {
        status: "requested",
        prompt: REQUEST_STATUS_PROMPT,
        targeted_count: targeted_session_ids.len(),
        delivered_count,
        queued_count,
        failed_count,
        targeted_session_ids,
    }))
}

async fn submit_client_bug_report(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(payload): Json<ClientBugReportRequest>,
) -> Result<Json<ClientBugReportResponse>, ApiError> {
    let request_target = request_target_from_uri(&uri);
    ensure_public_edge_assertion_from_parts(
        &state,
        &headers,
        Some(peer_addr),
        "POST",
        &request_target,
    )?;
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        "/client/bug-reports",
    )?;
    let actor_email = request_actor_email_from_parts(&state.config, &headers, Some(peer_addr));
    if actor_email.is_none() && state.config.google_auth.requested() {
        return Err(ApiError::Auth {
            status: StatusCode::UNAUTHORIZED,
            detail: "Authentication required",
            login_url: None,
        });
    }
    let report_text = payload.report_text.trim().to_owned();
    if report_text.is_empty() {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "report_text is required".to_owned(),
        });
    }
    if report_text.chars().count() > BUG_REPORT_MAX_TEXT_CHARS {
        return Err(ApiError::Status {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            detail: format!("report_text exceeds {BUG_REPORT_MAX_TEXT_CHARS} characters"),
        });
    }
    let route = payload
        .client_state
        .as_ref()
        .and_then(|value| value.get("route"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let client_state = if payload.include_debug_state {
        validate_json_payload_size(
            "client_state",
            payload.client_state.clone(),
            BUG_REPORT_MAX_CLIENT_STATE_CHARS,
        )?
    } else {
        None
    };
    let server_state = if payload.include_debug_state {
        validate_json_payload_size(
            "server_state",
            Some(bug_report_server_state(
                &state,
                payload.selected_session_id.as_deref(),
            )?),
            BUG_REPORT_MAX_SERVER_STATE_CHARS,
        )?
    } else {
        None
    };
    let bug_report_db_path = expand_home(&state.config.bug_reports.db_path);
    let store = BugReportStore::new(
        bug_report_db_path.clone(),
        state.config.bug_reports.max_reports,
    );
    let created = store.create_report(CreateBugReport {
        report_text: report_text.clone(),
        reported_by: actor_email,
        selected_session_id: payload.selected_session_id.clone(),
        route,
        app_version: payload.app_version,
        artifact_hash: payload.artifact_hash,
        include_debug_state: payload.include_debug_state,
        client_state,
        server_state,
    })?;
    let (maintainer_notified, delivery_result) = notify_maintainer_of_bug_report(
        &state,
        &created.id,
        &report_text,
        &payload.selected_session_id,
        &bug_report_db_path,
    )
    .unwrap_or_else(|error| (false, format!("failed:{}", error_name(&error))));
    store.update_delivery_result(&created.id, &delivery_result)?;
    Ok(Json(ClientBugReportResponse {
        status: "submitted",
        bug_id: created.id,
        maintainer_notified,
    }))
}

async fn list_humans(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let bridge = email_bridge(&state.config)?;
    let humans = bridge
        .list_humans()
        .into_iter()
        .map(HumanRecipientResponse::from)
        .collect::<Vec<_>>();
    Ok(Json(json!({ "humans": humans })))
}

async fn get_human(
    State(state): State<Arc<AppState>>,
    Path(identifier): Path<String>,
    request: Request,
) -> Result<Json<HumanRecipientResponse>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let bridge = email_bridge(&state.config)?;
    let Some(human) = bridge
        .lookup_human(&identifier)
        .map_err(email_config_error)?
    else {
        return Err(ApiError::NotFound("Human recipient not configured"));
    };
    Ok(Json(HumanRecipientResponse::from(human)))
}

async fn send_human_email(
    State(state): State<Arc<AppState>>,
    Path(identifier): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<HumanDeliveryRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/humans/{identifier}/email"),
    )?;
    let bridge = email_bridge_or_503(&state.config)?;
    let Some(human) = bridge
        .lookup_human(&identifier)
        .map_err(email_config_error)?
    else {
        return Err(ApiError::NotFound("Human recipient not configured"));
    };
    if human.channel("email").is_none() {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: format!(
                "Human recipient \"{}\" has no enabled email channel",
                human.name
            ),
        });
    }
    let Some(resolved_email_user) = bridge
        .lookup_human_email_user(&human.name)
        .map_err(email_config_error)?
    else {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: format!(
                "Human recipient \"{}\" has no resolved email address",
                human.name
            ),
        });
    };
    let sender_session_id = required_sender_session_id(payload.requester_session_id.as_deref())?;
    let sender_session = state
        .session_store
        .get_session(&sender_session_id)?
        .ok_or(ApiError::NotFound("Sender session not found"))?;
    let sent = bridge
        .send_agent_email(SendAgentEmailRequest {
            sender_session_id: sender_session.id.clone(),
            sender_name: session_display_name(sender_session.clone()),
            sender_provider: sender_session.provider,
            to_users: vec![resolved_email_user],
            cc_users: Vec::new(),
            subject: payload.subject,
            body_text: payload.text,
            body_html: String::new(),
            body_markdown: payload.body_markdown,
            auto_subject: payload.auto_subject,
        })
        .map_err(email_send_error)?;
    Ok(Json(json!({
        "status": "sent",
        "recipient": human.name,
        "channel": "email",
        "subject": sent.subject,
        "to": [{"username": human.name}],
    })))
}

async fn send_registered_email(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<SendEmailRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), "/email/send")?;
    let bridge = email_bridge_or_503(&state.config)?;
    let sender_session_id = required_sender_session_id(payload.requester_session_id.as_deref())?;
    let sender_session = state
        .session_store
        .get_session(&sender_session_id)?
        .ok_or(ApiError::NotFound("Sender session not found"))?;
    let recipients = unique_identifiers(&payload.recipients);
    let cc = unique_identifiers(&payload.cc);
    if recipients.is_empty() {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "At least one email recipient is required".to_owned(),
        });
    }
    for identifier in recipients.iter().chain(cc.iter()) {
        if let Some(human) = bridge
            .lookup_human(identifier)
            .map_err(email_config_error)?
        {
            return Err(ApiError::Status {
                status: StatusCode::BAD_REQUEST,
                detail: format!(
                    "Human recipient \"{}\" must use explicit human email delivery; generic email routing is not allowed",
                    human.name
                ),
            });
        }
    }
    let to_users = bridge
        .resolve_users(&recipients)
        .map_err(email_lookup_error)?;
    let cc_users = if cc.is_empty() {
        Vec::new()
    } else {
        bridge.resolve_users(&cc).map_err(email_lookup_error)?
    };
    let sent = bridge
        .send_agent_email(SendAgentEmailRequest {
            sender_session_id: sender_session.id.clone(),
            sender_name: session_display_name(sender_session.clone()),
            sender_provider: sender_session.provider,
            to_users,
            cc_users,
            subject: payload.subject,
            body_text: payload.body_text.unwrap_or_default(),
            body_html: payload.body_html.unwrap_or_default(),
            body_markdown: payload.body_markdown,
            auto_subject: payload.auto_subject,
        })
        .map_err(email_send_error)?;
    Ok(Json(serde_json::to_value(sent)?))
}

async fn inbound_email_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<InboundEmailRequest>,
) -> Result<Json<Value>, ApiError> {
    let bridge = email_bridge_or_503(&state.config)?;
    let configured_secret = bridge.worker_secret().ok_or_else(|| ApiError::Status {
        status: StatusCode::SERVICE_UNAVAILABLE,
        detail: "Email worker secret is required".to_owned(),
    })?;
    let provided_secret = headers
        .get(bridge.worker_secret_header())
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or("");
    if provided_secret != configured_secret {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Invalid email worker secret".to_owned(),
        });
    }
    if !bridge.is_authorized_sender(&payload.from_address) {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Inbound sender is not authorized".to_owned(),
        });
    }
    ensure_core_writes_enabled(&state)?;

    let parsed_body = payload.body.unwrap_or_default();
    let raw_email = payload.raw_email.as_deref().unwrap_or("").trim();
    let (mut raw_body, subject) = if raw_email.is_empty() {
        (parsed_body.clone(), None)
    } else {
        (
            extract_text_from_raw_email(raw_email),
            extract_subject_from_raw_email(raw_email),
        )
    };
    if !parsed_body.trim().is_empty()
        && (raw_body.trim().is_empty()
            || (extract_routed_session_id(&raw_body).is_none()
                && extract_routed_session_id(&parsed_body).is_some()))
    {
        raw_body = parsed_body;
    }
    let raw_body = raw_body.trim().to_owned();
    if raw_body.is_empty() {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "body or raw_email is required".to_owned(),
        });
    }

    let trusted_session_id = headers
        .get(bridge.session_id_header())
        .and_then(|value| value.to_str().ok())
        .and_then(normalize_explicit_session_id);
    let session_id = trusted_session_id
        .or_else(|| {
            payload
                .session_id
                .as_deref()
                .and_then(normalize_explicit_session_id)
        })
        .or_else(|| extract_routed_session_id(&raw_body));
    let Some(session_id) = session_id else {
        return Ok(Json(json!({
            "status": "ignored",
            "reason": "missing_routing_footer",
        })));
    };
    let reply_body = extract_reply_message_body(&raw_body);
    if reply_body.trim().is_empty() {
        return Ok(Json(json!({
            "status": "ignored",
            "session_id": session_id,
            "reason": "empty_reply_body",
        })));
    }
    let mut body = format!("{{sm email from {}}}", payload.from_address.trim());
    if let Some(subject) = subject
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        body = format!(
            "{{sm email from {} subj: {subject}}}",
            payload.from_address.trim()
        );
    }
    body.push('\n');
    body.push_str(&reply_body);

    let Some(mut session) = state.session_store.get_session(&session_id)? else {
        return Err(ApiError::NotFound("Session not found"));
    };
    let mut restored = false;
    if matches!(session.status.as_str(), "stopped" | "killed") {
        let outcome = if state.config.rust_core.runtime_enabled {
            let runtime = TmuxRuntime::from_app_config(&state.config);
            state
                .session_store
                .restore_core_session_with_runtime(&session_id, &runtime)?
        } else {
            state.session_store.restore_core_session(&session_id)?
        };
        match outcome {
            Some(CoreRestoreOutcome::Restored(restored_session)) => {
                session = restored_session;
                restored = true;
            }
            Some(CoreRestoreOutcome::NotStopped) => {}
            Some(CoreRestoreOutcome::UnsupportedNode(node)) => {
                return Err(ApiError::Status {
                    status: StatusCode::BAD_REQUEST,
                    detail: format!("Rust runtime does not support remote node {node}"),
                })
            }
            Some(CoreRestoreOutcome::UnsupportedProvider(provider)) => {
                return Err(ApiError::Status {
                    status: StatusCode::BAD_REQUEST,
                    detail: format!("Rust runtime does not support provider {provider}"),
                })
            }
            Some(CoreRestoreOutcome::MissingProviderResumeId(provider)) => {
                return Err(ApiError::Status {
                    status: StatusCode::CONFLICT,
                    detail: format!("Cannot restore {provider} session without provider_resume_id"),
                })
            }
            None => return Err(ApiError::NotFound("Session not found")),
        }
    }
    let input = SendCoreInputRequest {
        text: body,
        delivery_mode: "sequential".to_owned(),
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
    let outcome = if state.config.rust_core.runtime_enabled {
        if !is_primary_node(&session.node) {
            return Err(ApiError::Status {
                status: StatusCode::BAD_REQUEST,
                detail: format!("Rust runtime does not support remote node {}", session.node),
            });
        }
        let runtime = TmuxRuntime::from_app_config(&state.config);
        state
            .session_store
            .send_core_input_with_runtime(&session_id, input, &runtime)?
    } else {
        state.session_store.send_core_input(&session_id, input)?
    };
    let Some(outcome) = outcome else {
        return Err(ApiError::NotFound("Session not found"));
    };
    if !outcome.delivered && matches!(outcome.status.as_str(), "stopped" | "killed") {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: "Failed to deliver inbound email to session".to_owned(),
        });
    }
    Ok(Json(json!({
        "status": "sent",
        "session_id": session_id,
        "restored": restored,
        "delivery_result": if outcome.delivered { "delivered" } else { "queued" },
    })))
}

fn notify_maintainer_of_bug_report(
    state: &AppState,
    bug_id: &str,
    report_text: &str,
    selected_session_id: &Option<String>,
    db_path: &std::path::Path,
) -> Result<(bool, String), ApiError> {
    let Some(maintainer) = state
        .session_store
        .lookup_agent_registration("maintainer")?
    else {
        return Ok((false, "maintainer_not_found".to_owned()));
    };
    let Some(maintainer_session) = state.session_store.get_session(&maintainer.session_id)? else {
        return Ok((false, "maintainer_not_found".to_owned()));
    };
    if state.config.rust_core.runtime_enabled && !is_primary_node(&maintainer_session.node) {
        return Ok((
            false,
            format!("unsupported_remote_node:{}", maintainer_session.node),
        ));
    }
    let selected = selected_session_id.as_deref().unwrap_or("-");
    let message = format!(
        "[app bug] {bug_id}\nreport: {}\nsession: {selected}\ndb: {}",
        bug_report_summary(report_text),
        db_path.display()
    );
    let payload = SendCoreInputRequest {
        text: message,
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
    let outcome = if state.config.rust_core.runtime_enabled {
        let runtime = TmuxRuntime::from_app_config(&state.config);
        state.session_store.send_core_input_with_runtime(
            &maintainer.session_id,
            payload,
            &runtime,
        )?
    } else {
        state
            .session_store
            .send_core_input(&maintainer.session_id, payload)?
    };
    let Some(outcome) = outcome else {
        return Ok((false, "maintainer_not_found".to_owned()));
    };
    if outcome.delivered {
        return Ok((true, "delivered".to_owned()));
    }
    if matches!(outcome.status.as_str(), "stopped" | "killed") {
        return Ok((false, outcome.status));
    }
    Ok((true, "queued".to_owned()))
}

fn bug_report_summary(report_text: &str) -> String {
    let mut summary = report_text.split_whitespace().collect::<Vec<_>>().join(" ");
    if summary.chars().count() > 160 {
        summary = summary.chars().take(157).collect::<String>();
        summary.push_str("...");
    }
    summary
}

fn error_name(error: &ApiError) -> &'static str {
    match error {
        ApiError::Internal(_) => "internal",
        ApiError::NotFound(_) => "not_found",
        ApiError::Status { .. } => "status",
        ApiError::Auth { .. } => "auth",
    }
}

fn email_bridge(config: &AppConfig) -> Result<EmailBridge, ApiError> {
    EmailBridge::load(config).map_err(email_config_error)
}

fn email_bridge_or_503(config: &AppConfig) -> Result<EmailBridge, ApiError> {
    let bridge = EmailBridge::load(config).map_err(email_config_error)?;
    if !bridge.bridge_is_available() {
        return Err(ApiError::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "Email bridge is unavailable".to_owned(),
        });
    }
    Ok(bridge)
}

fn email_config_error(error: anyhow::Error) -> ApiError {
    ApiError::Status {
        status: StatusCode::CONFLICT,
        detail: error.to_string(),
    }
}

fn email_lookup_error(error: anyhow::Error) -> ApiError {
    ApiError::Status {
        status: StatusCode::NOT_FOUND,
        detail: error.to_string(),
    }
}

fn email_send_error(error: anyhow::Error) -> ApiError {
    let detail = error.to_string();
    let status = if detail.contains("Email body is required")
        || detail.contains("Email subject is required")
        || detail.contains("Managed sender session is required")
    {
        StatusCode::BAD_REQUEST
    } else if detail.contains("No registered email user found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_GATEWAY
    };
    ApiError::Status { status, detail }
}

fn required_sender_session_id(value: Option<&str>) -> Result<String, ApiError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "Managed sender session is required for email delivery".to_owned(),
        })
}

async fn deploy_app_artifact(
    State(state): State<Arc<AppState>>,
    Path(app_name): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<AppArtifactDeployResponse>, ApiError> {
    if !valid_app_name(&app_name) {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "Invalid app name".to_owned(),
        });
    }
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/deploy/{app_name}"),
    )?;
    let Some(actor_email) =
        request_actor_email_from_parts(&state.config, &headers, Some(peer_addr))
    else {
        return Err(ApiError::Auth {
            status: StatusCode::UNAUTHORIZED,
            detail: "Authentication required",
            login_url: None,
        });
    };

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut version_code: Option<i64> = None;
    let mut version_name: Option<String> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: format!("Expected multipart form upload: {error}"),
        })?
    {
        let name = field.name().unwrap_or("").to_owned();
        match name.as_str() {
            "file" => {
                let bytes = field.bytes().await.map_err(|error| ApiError::Status {
                    status: StatusCode::BAD_REQUEST,
                    detail: format!("Failed to read multipart file: {error}"),
                })?;
                if bytes.len() > APP_ARTIFACT_MAX_SIZE_BYTES {
                    return Err(ApiError::Status {
                        status: StatusCode::PAYLOAD_TOO_LARGE,
                        detail: "Artifact exceeds 100 MB limit".to_owned(),
                    });
                }
                file_bytes = Some(bytes.to_vec());
            }
            "version_code" => {
                let value = field.text().await.unwrap_or_default();
                let value = value.trim();
                if !value.is_empty() {
                    version_code = Some(value.parse().map_err(|_| ApiError::Status {
                        status: StatusCode::BAD_REQUEST,
                        detail: "version_code must be an integer".to_owned(),
                    })?);
                }
            }
            "version_name" => {
                let value = field.text().await.unwrap_or_default();
                let value = value.trim();
                if !value.is_empty() {
                    version_name = Some(value.to_owned());
                }
            }
            _ => {}
        }
    }
    let Some(file_bytes) = file_bytes else {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "Missing multipart field 'file'".to_owned(),
        });
    };
    if file_bytes.is_empty() {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "Uploaded artifact is empty".to_owned(),
        });
    }

    let root = expand_home(&state.config.app_artifacts.root_dir);
    let stored = store_artifact(
        &root,
        &app_name,
        &file_bytes,
        Some(actor_email),
        version_code,
        version_name,
    )
    .map_err(|error| ApiError::Status {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        detail: format!("Failed to store artifact: {error}"),
    })?;
    Ok(Json(AppArtifactDeployResponse {
        ok: true,
        app: app_name.clone(),
        size_bytes: stored.size_bytes,
        download_url: format!("/apps/{app_name}/latest.apk"),
        artifact_hash: stored.artifact_hash,
    }))
}

async fn get_latest_app_artifact(
    State(state): State<Arc<AppState>>,
    Path(app_name): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    if !valid_app_name(&app_name) {
        return Err(ApiError::NotFound("Artifact not found"));
    }
    let root = expand_home(&state.config.app_artifacts.root_dir);
    let metadata =
        read_metadata(&root, &app_name).map_err(|_| ApiError::NotFound("Artifact not found"))?;
    if !valid_artifact_hash(&metadata.artifact_hash) {
        return Err(ApiError::NotFound("Artifact not found"));
    }
    Ok((
        StatusCode::FOUND,
        [
            (
                LOCATION,
                format!("/apps/{app_name}/{}.apk", metadata.artifact_hash),
            ),
            (CACHE_CONTROL, "no-cache".to_owned()),
        ],
    )
        .into_response())
}

async fn get_hashed_app_artifact(
    State(state): State<Arc<AppState>>,
    Path((app_name, artifact_file)): Path<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let Some(artifact_hash) = artifact_file.strip_suffix(".apk") else {
        return Err(ApiError::NotFound("Artifact not found"));
    };
    if !valid_app_name(&app_name) || !valid_artifact_hash(artifact_hash) {
        return Err(ApiError::NotFound("Artifact not found"));
    }
    let root = expand_home(&state.config.app_artifacts.root_dir);
    let path = hashed_path(&root, &app_name, artifact_hash);
    let bytes = fs::read(&path).map_err(|_| ApiError::NotFound("Artifact not found"))?;
    Ok((
        StatusCode::OK,
        [
            (
                CONTENT_TYPE,
                "application/vnd.android.package-archive".to_owned(),
            ),
            (
                CACHE_CONTROL,
                "public, max-age=31536000, immutable".to_owned(),
            ),
            (
                CONTENT_DISPOSITION,
                format!("attachment; filename=\"{app_name}.apk\""),
            ),
        ],
        Body::from(bytes),
    )
        .into_response())
}

async fn get_app_artifact_metadata(
    State(state): State<Arc<AppState>>,
    Path(app_name): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    if !valid_app_name(&app_name) {
        return Err(ApiError::NotFound("Artifact metadata not found"));
    }
    let root = expand_home(&state.config.app_artifacts.root_dir);
    let metadata = read_metadata(&root, &app_name)
        .map_err(|_| ApiError::NotFound("Artifact metadata not found"))?;
    Ok(Json(serde_json::to_value(metadata)?))
}

async fn get_legacy_apk_download(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    Ok((
        StatusCode::FOUND,
        [(LOCATION, "/apps/session-manager-android/latest.apk")],
    )
        .into_response())
}

async fn events_state(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<EventStateResponse>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    Ok(Json(event_state_payload()))
}

async fn events_stream(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let data = serde_json::to_string(&event_state_payload())?;
    let stream =
        stream::once(
            async move { Ok::<Event, Infallible>(Event::default().event("hello").data(data)) },
        )
        .chain(stream::pending());
    Ok((
        [("x-accel-buffering", "no")],
        Sse::new(stream).keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        ),
    )
        .into_response())
}

async fn list_sessions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListSessionsQuery>,
    request: Request,
) -> Result<Json<SessionsEnvelope<SessionResponse>>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let sessions = state
        .session_store
        .list_sessions(query.include_stopped)?
        .into_iter()
        .map(SessionResponse::from)
        .collect::<Vec<_>>();
    Ok(Json(SessionsEnvelope::from(sessions)))
}

async fn list_children_sessions(
    State(state): State<Arc<AppState>>,
    Path(parent_session_id): Path<String>,
    Query(query): Query<ListChildrenQuery>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let children = state.session_store.list_children(
        &parent_session_id,
        query.recursive,
        query.status.as_deref(),
        query.include_terminated,
    )?;
    Ok(Json(json!({
        "parent_session_id": parent_session_id,
        "children": children,
    })))
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<CreateCoreSessionRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), "/sessions")?;
    ensure_core_writes_enabled(&state)?;
    let log_dir = state.config.rust_core.log_dir.as_deref().map(expand_home);
    let session = if state.config.rust_core.runtime_enabled {
        ensure_core_runtime_provider_supported(&payload)?;
        ensure_core_runtime_request_node_supported(&state, &payload)?;
        let runtime = TmuxRuntime::from_app_config(&state.config);
        state
            .session_store
            .create_core_session_with_runtime(payload, log_dir, &runtime)?
    } else {
        state.session_store.create_core_session(payload, log_dir)?
    };
    Ok(Json(SessionResponse::from(session)))
}

async fn spawn_session(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<SpawnCoreSessionRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), "/sessions/spawn")?;
    ensure_core_writes_enabled(&state)?;
    let Some(parent) = state
        .session_store
        .get_session(&payload.parent_session_id)?
    else {
        return Ok(Json(json!({ "error": "Parent session not found" })));
    };
    if payload.track_seconds.is_some() {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "Rust core spawn does not support track_seconds yet".to_owned(),
        });
    }
    let wait_seconds = payload.wait;
    let provider = payload
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(parent.provider.as_str())
        .to_owned();
    let working_dir = payload
        .working_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(parent.working_dir.as_str())
        .to_owned();
    let node = payload
        .node
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(parent.node.as_str())
        .to_owned();
    let create_payload = CreateCoreSessionRequest {
        id: payload.id,
        name: payload.name,
        working_dir: Some(working_dir),
        provider: Some(provider),
        parent_session_id: Some(parent.id.clone()),
        node: Some(node),
        initial_message: Some(payload.prompt),
        model: payload.model,
        wait: wait_seconds,
    };
    let log_dir = state.config.rust_core.log_dir.as_deref().map(expand_home);
    let child = if state.config.rust_core.runtime_enabled {
        ensure_core_runtime_provider_supported(&create_payload)?;
        ensure_core_runtime_request_node_supported(&state, &create_payload)?;
        let runtime = TmuxRuntime::from_app_config(&state.config);
        state
            .session_store
            .create_core_session_with_runtime(create_payload, log_dir, &runtime)?
    } else {
        state
            .session_store
            .create_core_session(create_payload, log_dir)?
    };
    if parent.is_em && child.provider != "codex-fork" {
        let _ = state.session_store.arm_stop_notify(
            &child.id,
            ArmStopNotifyRequest {
                sender_session_id: parent.id.clone(),
                requester_session_id: parent.id.clone(),
                delay_seconds: EM_SPAWN_STOP_NOTIFY_DELAY_SECONDS,
            },
        )?;
    }
    if let Some(wait_seconds) = wait_seconds {
        spawn_child_wait_monitor(state.clone(), child.clone(), wait_seconds);
    }
    Ok(Json(serde_json::to_value(SpawnSessionResponse::from(
        child,
    ))?))
}

fn spawn_child_wait_monitor(state: Arc<AppState>, child: SessionRecord, wait_seconds: u64) {
    let Some(parent_session_id) = child
        .parent_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
    else {
        return;
    };
    let child_session_id = child.id.clone();

    tokio::spawn(async move {
        let mut last_activity = child.last_activity.clone();
        let mut last_output_size = child_output_size(&child);
        let mut idle_since = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Ok(Some(child)) = state.session_store.get_session(&child_session_id) else {
                break;
            };
            let output_size = child_output_size(&child);
            if child.last_activity != last_activity || output_size != last_output_size {
                last_activity = child.last_activity.clone();
                last_output_size = output_size;
                idle_since = Instant::now();
            }

            let completion_message = if session_status_is_stopped(&child.status)
                || runtime_child_session_exited(&state, &child)
            {
                Some("Session exited".to_owned())
            } else {
                Some(idle_since.elapsed().as_secs())
                    .filter(|idle_seconds| *idle_seconds >= wait_seconds)
                    .map(|idle_seconds| {
                        completion_summary(&state.session_store, &child_session_id)
                            .unwrap_or_else(|| format!("Idle for {idle_seconds}s"))
                    })
            };
            let Some(completion_message) = completion_message else {
                continue;
            };

            let notification = format!(
                "Child {} ({}) completed: {}",
                child_display_name(&child),
                short_session_id(&child_session_id),
                completion_message
            );
            let request = SendCoreInputRequest {
                text: notification,
                delivery_mode: "sequential".to_owned(),
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
            let _ = if state.config.rust_core.runtime_enabled {
                let runtime = TmuxRuntime::from_app_config(&state.config);
                state.session_store.send_core_input_with_runtime(
                    &parent_session_id,
                    request,
                    &runtime,
                )
            } else {
                state
                    .session_store
                    .send_core_input(&parent_session_id, request)
            };
            break;
        }
    });
}

fn session_status_is_stopped(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "stopped" | "killed"
    )
}

fn runtime_child_session_exited(state: &AppState, child: &SessionRecord) -> bool {
    if !state.config.rust_core.runtime_enabled {
        return false;
    }
    let runtime = TmuxRuntime::from_app_config(&state.config)
        .for_socket_name(child.tmux_socket_name.as_deref());
    matches!(runtime.session_exists(&child.tmux_session), Ok(false))
}

fn child_output_size(child: &SessionRecord) -> Option<u64> {
    let log_file = child
        .log_file
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    expand_home(log_file)
        .metadata()
        .ok()
        .map(|metadata| metadata.len())
}

fn completion_summary(session_store: &SessionStore, child_session_id: &str) -> Option<String> {
    let output = session_store.capture_output(child_session_id, 10).ok()??;
    output
        .lines()
        .map(str::trim)
        .find(|line| line.len() > 10)
        .map(|line| {
            if line.chars().count() > 100 {
                format!("{}...", line.chars().take(100).collect::<String>())
            } else {
                line.to_owned()
            }
        })
}

fn child_display_name(child: &SessionRecord) -> String {
    child
        .friendly_name
        .as_deref()
        .or(Some(child.name.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(child.id.as_str())
        .to_owned()
}

fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

async fn list_client_sessions(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_public_edge_assertion_for_request(&state, &request)?;
    ensure_session_read_allowed(&state, &request)?;
    let actor_email = request_actor_email(&state.config, &request);
    let sessions = state
        .session_store
        .list_sessions(false)?
        .into_iter()
        .map(|session| client_session_value(&state, session, actor_email.as_deref(), None))
        .collect::<Vec<_>>();
    Ok(Json(json!({ "sessions": sessions })))
}

async fn list_nodes(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<NodesListResponse>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    Ok(Json(nodes_list_response(&state.config)))
}

async fn list_codex_review_requests(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListCodexReviewRequestsQuery>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let notify_session_id = if let Some(notify_target) = query
        .notify_target
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let Some(session) = resolve_session_or_registry_role(&state, notify_target)? else {
            return Err(ApiError::NotFound("Notify target not found"));
        };
        Some(session.id)
    } else {
        None
    };
    let queue_db_path = expand_home(&state.config.sm_send.db_path);
    let registrations = RetainedQueueStore::list_codex_review_requests_from_path(
        &queue_db_path,
        CodexReviewRequestFilters {
            notify_session_id,
            repo: query
                .repo
                .as_ref()
                .and_then(|value| trimmed(&Some(value.clone()))),
            pr_number: query.pr_number,
            include_inactive: query.include_inactive,
        },
    )?;
    let mut requests = Vec::with_capacity(registrations.len());
    for registration in registrations {
        requests.push(codex_review_request_response(&state, registration)?);
    }
    Ok(Json(json!({ "requests": requests })))
}

async fn get_codex_review_request(
    State(state): State<Arc<AppState>>,
    Path(request_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let queue_db_path = expand_home(&state.config.sm_send.db_path);
    let Some(registration) =
        RetainedQueueStore::get_codex_review_request_from_path(&queue_db_path, &request_id)?
    else {
        return Err(ApiError::NotFound("Codex review request not found"));
    };
    Ok(Json(codex_review_request_response(&state, registration)?))
}

async fn list_queue_jobs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQueueJobsQuery>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let notify_session_id = if let Some(notify_target) = query
        .notify_target
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let Some(session) = resolve_session_or_registry_role(&state, notify_target)? else {
            return Err(ApiError::NotFound("Notify target not found"));
        };
        Some(session.id)
    } else {
        None
    };

    let queue_state_dir = state.config.queue_runner_state_dir();
    let queue_db_path = expand_home(&queue_state_dir.to_string_lossy()).join("queue_runner.db");
    let jobs = RetainedQueueStore::list_queue_jobs_from_path(
        &queue_db_path,
        QueueJobFilters {
            notify_session_id,
            job_type: query
                .job_type
                .as_ref()
                .and_then(|value| trimmed(&Some(value.clone()))),
            state: query
                .state
                .as_ref()
                .and_then(|value| trimmed(&Some(value.clone()))),
            include_terminal: query.include_terminal,
        },
    )?;
    let mut response_jobs = Vec::with_capacity(jobs.len());
    for job in jobs {
        response_jobs.push(queue_job_response(&state, job)?);
    }
    Ok(Json(json!({ "jobs": response_jobs })))
}

async fn get_queue_job(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let queue_state_dir = state.config.queue_runner_state_dir();
    let queue_db_path = expand_home(&queue_state_dir.to_string_lossy()).join("queue_runner.db");
    let Some(job) = RetainedQueueStore::get_queue_job_from_path(&queue_db_path, &job_id)? else {
        return Err(ApiError::NotFound("Queue job not found"));
    };
    Ok(Json(queue_job_response(&state, job)?))
}

fn resolve_session_or_registry_role(
    state: &AppState,
    identifier: &str,
) -> Result<Option<SessionRecord>, ApiError> {
    if let Some(session) = state.session_store.get_session(identifier)? {
        return Ok(Some(session));
    }
    let Some(registration) = state.session_store.lookup_agent_registration(identifier)? else {
        return Ok(None);
    };
    Ok(state.session_store.get_session(&registration.session_id)?)
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    request: Request,
) -> Result<Json<SessionResponse>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let Some(session) = state.session_store.get_session(&session_id)? else {
        return Err(ApiError::NotFound("Session not found"));
    };
    Ok(Json(SessionResponse::from(session)))
}

async fn get_client_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_public_edge_assertion_for_request(&state, &request)?;
    ensure_session_read_allowed(&state, &request)?;
    let actor_email = request_actor_email(&state.config, &request);
    let Some(session) = state.session_store.get_session(&session_id)? else {
        return Err(ApiError::NotFound("Session not found"));
    };
    Ok(Json(client_session_value(
        &state,
        session,
        actor_email.as_deref(),
        None,
    )))
}

async fn create_mobile_attach_ticket(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    request: Request,
) -> Result<Json<MobileAttachTicketResponse>, ApiError> {
    ensure_public_edge_assertion_for_request(&state, &request)?;
    let peer_addr = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|value| value.0);
    let actor_email = request_actor_email_from_parts(&state.config, request.headers(), peer_addr)
        .ok_or_else(|| ApiError::Status {
        status: StatusCode::UNAUTHORIZED,
        detail: "Authentication required".to_owned(),
    })?;
    let Some(session) = resolve_session_or_registry_role(&state, &session_id)? else {
        return Err(ApiError::NotFound("Session not found"));
    };
    let route_prefix = mobile_terminal_request_path_prefix(
        request.uri().path(),
        &format!("/client/sessions/{}/attach-ticket", session.id),
    );
    ensure_mobile_terminal_ticket_runtime_enabled(&state)?;
    let MobileTerminalAuthorization {
        user_id,
        device_key_id,
        ws_url,
        tmux_session,
        tmux_socket_name,
        proof_nonce,
        proof_nonce_expires_at_unix,
    } = authorize_mobile_terminal_ticket_request(
        &state,
        request.headers(),
        route_prefix.as_deref(),
        &session,
        &actor_email,
    )?;

    let now = OffsetDateTime::now_utc();
    record_mobile_terminal_proof_nonce(
        &state,
        &user_id,
        &device_key_id,
        &session.id,
        &proof_nonce,
        proof_nonce_expires_at_unix,
        now.unix_timestamp(),
    )?;
    let ttl_seconds = clamp_u64(state.config.mobile_terminal.ticket_ttl_seconds, 5, 300, 30);
    let expires_at = now + Duration::from_secs(ttl_seconds);
    let ticket_id = format!("att_{}", random_urlsafe_token(18));
    let ticket_secret = random_urlsafe_token(40);
    let secret_hash = mobile_terminal_secret_hash(&state.mobile_terminal_secret, &ticket_secret)?;

    let mut tickets = state
        .mobile_terminal_tickets
        .lock()
        .map_err(|_| ApiError::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: "Mobile terminal ticket store is unavailable".to_owned(),
        })?;
    cleanup_mobile_terminal_tickets(&mut tickets, now.unix_timestamp());
    tickets
        .retain(|_, ticket| !(ticket.user_id == user_id && ticket.device_key_id == device_key_id));
    let active = state
        .mobile_terminal_active_attaches
        .lock()
        .map_err(|_| ApiError::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: "Mobile terminal active attach store is unavailable".to_owned(),
        })?;
    let max_global = clamp_usize(
        state.config.mobile_terminal.max_concurrent_attaches_global,
        1,
        64,
        4,
    );
    let max_user = clamp_usize(
        state
            .config
            .mobile_terminal
            .max_concurrent_attaches_per_user,
        1,
        16,
        1,
    );
    let max_session = clamp_usize(
        state
            .config
            .mobile_terminal
            .max_concurrent_attaches_per_session,
        1,
        16,
        1,
    );
    if tickets.len() + active.len() >= max_global {
        return Err(ApiError::Status {
            status: StatusCode::TOO_MANY_REQUESTS,
            detail: "Too many active mobile attaches".to_owned(),
        });
    }
    if tickets
        .values()
        .filter(|ticket| ticket.user_id == user_id)
        .count()
        + active
            .values()
            .filter(|attach| attach.user_id == user_id)
            .count()
        >= max_user
    {
        return Err(ApiError::Status {
            status: StatusCode::TOO_MANY_REQUESTS,
            detail: "Too many active mobile attaches for user".to_owned(),
        });
    }
    if tickets
        .values()
        .filter(|ticket| ticket.session_id == session.id)
        .count()
        + active
            .values()
            .filter(|attach| attach.session_id == session.id)
            .count()
        >= max_session
    {
        return Err(ApiError::Status {
            status: StatusCode::TOO_MANY_REQUESTS,
            detail: "Session already has an active mobile attach".to_owned(),
        });
    }
    drop(active);

    ensure_mobile_terminal_ticket_runtime_enabled(&state)?;
    tickets.insert(
        ticket_id.clone(),
        MobileTerminalTicket {
            ticket_id: ticket_id.clone(),
            secret_hash,
            user_id,
            actor_email,
            session_id: session.id,
            provider: session.provider,
            node: session.node,
            tmux_session,
            tmux_socket_name,
            device_key_id: device_key_id.clone(),
            created_at_unix: now.unix_timestamp(),
            expires_at_unix: expires_at.unix_timestamp(),
        },
    );

    Ok(Json(MobileAttachTicketResponse {
        ticket_id,
        ticket_secret,
        device_key_id,
        ws_url,
        expires_at: expires_at.format(&Rfc3339).unwrap_or_else(|_| {
            OffsetDateTime::from_unix_timestamp(expires_at.unix_timestamp())
                .unwrap_or(OffsetDateTime::UNIX_EPOCH)
                .to_string()
        }),
    }))
}

async fn disable_mobile_terminal(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<MobileTerminalDisableResponse>, ApiError> {
    ensure_public_edge_assertion_for_request(&state, &request)?;
    let actor_email =
        request_actor_email(&state.config, &request).ok_or_else(|| ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Authentication required".to_owned(),
        })?;
    let Some((_user_id, user_config)) = mobile_terminal_visible_user(&state.config, &actor_email)
    else {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "User is not allowed to disable mobile terminal attach".to_owned(),
        });
    };
    if !user_config.interactive_shell_access || !mobile_terminal_user_can_disable(user_config) {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "User is not allowed to disable mobile terminal attach".to_owned(),
        });
    }

    state
        .mobile_terminal_runtime_disabled
        .store(true, Ordering::SeqCst);
    state
        .mobile_terminal_tickets
        .lock()
        .map_err(|_| ApiError::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: "Mobile terminal ticket store is unavailable".to_owned(),
        })?
        .clear();
    let active_attaches = {
        let mut active =
            state
                .mobile_terminal_active_attaches
                .lock()
                .map_err(|_| ApiError::Status {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    detail: "Mobile terminal active attach store is unavailable".to_owned(),
                })?;
        let active_attaches = active.values().cloned().collect::<Vec<_>>();
        active.clear();
        active_attaches
    };
    for active in &active_attaches {
        active.stop.store(true, Ordering::SeqCst);
    }

    Ok(Json(MobileTerminalDisableResponse {
        ok: true,
        disabled: true,
        active_attaches_terminated: active_attaches.len(),
    }))
}

async fn list_mobile_terminal_devices(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<MobileTerminalDeviceListResponse>, ApiError> {
    ensure_public_edge_assertion_for_request(&state, &request)?;
    let actor_email =
        request_actor_email(&state.config, &request).ok_or_else(|| ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Authentication required".to_owned(),
        })?;
    let Some((actor_user_id, user_config)) =
        mobile_terminal_visible_user(&state.config, &actor_email)
    else {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "User is not allowed to manage mobile terminal devices".to_owned(),
        });
    };
    if !user_config.interactive_shell_access {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "User is not allowed to manage mobile terminal devices".to_owned(),
        });
    }
    let owner_view = mobile_terminal_user_can_disable(user_config);
    let revoked_keys = state
        .mobile_terminal_revoked_keys
        .lock()
        .map_err(|_| ApiError::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: "Mobile terminal revoked key store is unavailable".to_owned(),
        })?
        .clone();
    let devices = state
        .config
        .mobile_terminal
        .allowed_users
        .iter()
        .filter(|(user_id, _)| owner_view || user_id.as_str() == actor_user_id)
        .flat_map(|(user_id, config)| {
            let revoked_keys = revoked_keys.clone();
            config.registered_device_keys.iter().filter_map(move |key| {
                let device_key_id = key.id.trim();
                if device_key_id.is_empty() {
                    return None;
                }
                Some(MobileTerminalDeviceSummary {
                    user_id: user_id.clone(),
                    device_key_id: device_key_id.to_owned(),
                    enabled: key.enabled && !key.public_key.trim().is_empty(),
                    revoked: revoked_keys.contains(&(user_id.clone(), device_key_id.to_owned())),
                })
            })
        })
        .collect();

    Ok(Json(MobileTerminalDeviceListResponse {
        devices,
        owner_view,
        runtime_only_revocations: true,
    }))
}

async fn revoke_mobile_terminal_device(
    State(state): State<Arc<AppState>>,
    Path(device_key_id): Path<String>,
    Query(query): Query<MobileTerminalRevokeDeviceQuery>,
    request: Request,
) -> Result<Json<MobileTerminalRevokeDeviceResponse>, ApiError> {
    ensure_public_edge_assertion_for_request(&state, &request)?;
    let device_key_id = device_key_id.trim().to_owned();
    if device_key_id.is_empty() {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "Mobile terminal device id is required".to_owned(),
        });
    }
    let actor_email =
        request_actor_email(&state.config, &request).ok_or_else(|| ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Authentication required".to_owned(),
        })?;
    let Some((actor_user_id, user_config)) =
        mobile_terminal_visible_user(&state.config, &actor_email)
    else {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "User is not allowed to manage mobile terminal devices".to_owned(),
        });
    };
    if !user_config.interactive_shell_access {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "User is not allowed to manage mobile terminal devices".to_owned(),
        });
    }
    let owner_view = mobile_terminal_user_can_disable(user_config);
    let target_user_id = resolve_mobile_terminal_revoke_target(
        &state.config,
        actor_user_id,
        owner_view,
        &device_key_id,
        query.user_id.as_deref(),
    )?;
    let (already_revoked, pending_tickets_revoked, active_stops) =
        revoke_mobile_terminal_device_in_state(&state, &target_user_id, &device_key_id)?;
    for stop in &active_stops {
        stop.store(true, Ordering::SeqCst);
    }

    Ok(Json(MobileTerminalRevokeDeviceResponse {
        ok: true,
        revoked: true,
        user_id: target_user_id,
        device_key_id,
        already_revoked,
        pending_tickets_revoked,
        active_attaches_terminated: active_stops.len(),
        runtime_only: true,
    }))
}

async fn mobile_terminal_endpoint(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_public_edge_assertion_for_request(&state, &request)?;
    let (mut parts, _body) = request.into_parts();
    let headers = parts.headers.clone();
    let peer_addr = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|value| value.0);
    if let Ok(ws) = WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        return Ok(ws
            .on_upgrade(move |socket| mobile_terminal_websocket(socket, state))
            .into_response());
    }
    mobile_terminal_upgrade_required(&state, &headers, peer_addr)
}

fn mobile_terminal_upgrade_required(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
) -> Result<Response, ApiError> {
    let actor_email = request_actor_email_from_parts(&state.config, headers, peer_addr)
        .ok_or_else(|| ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Authentication required".to_owned(),
        })?;
    let Some((_user_id, user_config)) = mobile_terminal_visible_user(&state.config, &actor_email)
    else {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "User is not allowed to use mobile terminal attach".to_owned(),
        });
    };
    if !user_config.interactive_shell_access {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "User is not allowed to use mobile terminal attach".to_owned(),
        });
    }
    let mut headers = HeaderMap::new();
    headers.insert(UPGRADE, "websocket".parse().unwrap());
    headers.insert(CONNECTION, "Upgrade".parse().unwrap());
    Ok((
        StatusCode::UPGRADE_REQUIRED,
        headers,
        Json(json!({ "detail": "mobile terminal requires a WebSocket upgrade" })),
    )
        .into_response())
}

async fn mobile_terminal_websocket(mut socket: WebSocket, state: Arc<AppState>) {
    if !mobile_terminal_enabled(&state) {
        send_mobile_terminal_error(&mut socket, "mobile terminal attach is disabled").await;
        close_mobile_terminal_socket(&mut socket, 1008, "mobile_terminal_disabled").await;
        return;
    }

    let auth_timeout = Duration::from_secs(clamp_u64(3, 1, 30, 3));
    let auth_frame = match timeout(auth_timeout, socket.recv()).await {
        Ok(Some(Ok(message))) => match parse_mobile_terminal_auth_message(message) {
            Ok(frame) => frame,
            Err(detail) => {
                send_mobile_terminal_error(&mut socket, detail).await;
                close_mobile_terminal_socket(&mut socket, 1008, detail).await;
                return;
            }
        },
        Ok(Some(Err(_))) | Ok(None) => return,
        Err(_) => {
            send_mobile_terminal_error(&mut socket, "terminal auth timed out").await;
            close_mobile_terminal_socket(&mut socket, 1008, "terminal_auth_timed_out").await;
            return;
        }
    };

    match consume_mobile_terminal_ticket(&state, &auth_frame) {
        Ok((ticket, attach_id, stop)) => {
            run_mobile_terminal_bridge(socket, state, ticket, attach_id, stop).await;
        }
        Err(error) => {
            let detail = api_error_detail(&error);
            send_mobile_terminal_error(&mut socket, &detail).await;
            close_mobile_terminal_socket(&mut socket, 1008, &detail).await;
        }
    }
}

fn parse_mobile_terminal_auth_message(
    message: Message,
) -> Result<MobileTerminalAuthFrame, &'static str> {
    let text = match message {
        Message::Text(text) => text.to_string(),
        Message::Binary(bytes) => {
            String::from_utf8(bytes.to_vec()).map_err(|_| "Invalid terminal auth frame")?
        }
        Message::Close(_) => return Err("Invalid terminal auth frame"),
        _ => return Err("First terminal frame must be auth"),
    };
    let frame: MobileTerminalAuthFrame =
        serde_json::from_str(&text).map_err(|_| "Invalid terminal auth frame")?;
    if frame.frame_type.as_deref().map(str::trim) != Some("auth") {
        return Err("First terminal frame must be auth");
    }
    Ok(frame)
}

async fn send_mobile_terminal_error(socket: &mut WebSocket, message: &str) {
    let _ = send_mobile_terminal_json(
        socket,
        json!({
            "type": "error",
            "message": message,
        }),
    )
    .await;
}

async fn send_mobile_terminal_json(socket: &mut WebSocket, payload: Value) -> Result<(), ()> {
    let text = match serde_json::to_string(&payload) {
        Ok(text) => text,
        Err(_) => return Err(()),
    };
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

async fn close_mobile_terminal_socket(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_owned().into(),
        })))
        .await;
}

struct MobileTerminalInitialState {
    rows: u16,
    cols: u16,
    pending_frames: Vec<Value>,
    detached: bool,
}

#[cfg(unix)]
struct MobileTerminalPty {
    master: Arc<OwnedFd>,
    child: Child,
    stop: Arc<AtomicBool>,
}

#[derive(Debug)]
enum MobileTerminalPtyEvent {
    Output(Vec<u8>),
    Closed,
}

async fn run_mobile_terminal_bridge(
    socket: WebSocket,
    state: Arc<AppState>,
    ticket: MobileTerminalTicket,
    attach_id: String,
    stop: Arc<AtomicBool>,
) {
    let _ = run_mobile_terminal_bridge_inner(socket, &state, &ticket, stop).await;
    remove_mobile_terminal_active_attach(&state, &attach_id);
}

async fn run_mobile_terminal_bridge_inner(
    mut socket: WebSocket,
    state: &AppState,
    ticket: &MobileTerminalTicket,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    if !mobile_terminal_active_attach_exists(state, ticket) || !mobile_terminal_enabled(state) {
        let _ = send_mobile_terminal_json(
            &mut socket,
            json!({
                "type": "exit",
                "code": 1008,
                "reason": "mobile_terminal_disabled",
            }),
        )
        .await;
        close_mobile_terminal_socket(&mut socket, 1008, "mobile_terminal_disabled").await;
        return Ok(());
    }

    let initial = wait_for_mobile_terminal_initial_resize(&mut socket, &state.config).await;
    if initial.detached {
        close_mobile_terminal_socket(&mut socket, 1000, "mobile_terminal_detached").await;
        return Ok(());
    }

    preload_mobile_terminal_scrollback(&mut socket, &state.config, ticket).await;

    let pty = match start_mobile_terminal_attach_client(ticket, initial.rows, initial.cols, stop) {
        Ok(pty) => pty,
        Err(error) => {
            send_mobile_terminal_error(&mut socket, "failed to attach tmux session").await;
            close_mobile_terminal_socket(&mut socket, 1011, "mobile_terminal_bridge_failed").await;
            return Err(format!("failed to attach tmux session: {error}"));
        }
    };
    let MobileTerminalPty {
        master,
        mut child,
        stop,
    } = pty;
    let (output_tx, mut output_rx) = mpsc::unbounded_channel();
    let output_master = master.clone();
    let output_stop = stop.clone();
    let output_handle = tokio::task::spawn_blocking(move || {
        mobile_terminal_output_reader(output_master, output_stop, output_tx);
    });

    let _ = send_mobile_terminal_json(
        &mut socket,
        json!({
            "type": "status",
            "state": "attached",
            "session_id": ticket.session_id,
            "rows": initial.rows,
            "cols": initial.cols,
        }),
    )
    .await;

    let mut pending_frames = initial.pending_frames.into_iter().collect::<Vec<_>>();
    pending_frames.reverse();
    let max_attach_seconds = clamp_u64(
        state.config.mobile_terminal.max_attach_seconds,
        30,
        24 * 3600,
        MOBILE_TERMINAL_MAX_ATTACH_SECONDS,
    );
    let max_timer = tokio::time::sleep(Duration::from_secs(max_attach_seconds));
    tokio::pin!(max_timer);

    let (mut sender, mut receiver) = socket.split();
    let mut close_code = 1000u16;
    let mut close_reason = "mobile_terminal_detached".to_owned();

    loop {
        if stop.load(Ordering::SeqCst) || !mobile_terminal_enabled(state) {
            let _ = sender
                .send(Message::Text(
                    json!({
                        "type": "exit",
                        "code": 1008,
                        "reason": "mobile_terminal_disabled",
                    })
                    .to_string()
                    .into(),
                ))
                .await;
            close_code = 1008;
            close_reason = "mobile_terminal_disabled".to_owned();
            break;
        }
        if let Some(frame) = pending_frames.pop() {
            if process_mobile_terminal_client_frame(
                &mut sender,
                &master,
                &frame,
                &mut close_code,
                &mut close_reason,
            )
            .await?
            {
                break;
            }
            continue;
        }

        tokio::select! {
            _ = &mut max_timer => {
                let _ = sender
                    .send(Message::Text(
                        json!({
                            "type": "exit",
                            "code": 124,
                            "reason": "max_attach_seconds",
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                close_code = 1000;
                close_reason = "max_attach_seconds".to_owned();
                break;
            }
            output = output_rx.recv() => {
                match output {
                    Some(MobileTerminalPtyEvent::Output(chunk)) => {
                        let payload = json!({
                            "type": "output",
                            "mode": "stream",
                            "encoding": "base64",
                            "data": STANDARD.encode(chunk),
                        });
                        if sender.send(Message::Text(payload.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Some(MobileTerminalPtyEvent::Closed) | None => {
                        if stop.load(Ordering::SeqCst) || !mobile_terminal_enabled(state) {
                            let _ = sender
                                .send(Message::Text(
                                    json!({
                                        "type": "exit",
                                        "code": 1008,
                                        "reason": "mobile_terminal_disabled",
                                    })
                                    .to_string()
                                    .into(),
                                ))
                                .await;
                            close_code = 1008;
                            close_reason = "mobile_terminal_disabled".to_owned();
                            break;
                        }
                        let _ = sender
                            .send(Message::Text(
                                json!({
                                    "type": "error",
                                    "message": "tmux session is no longer attachable",
                                })
                                .to_string()
                                .into(),
                            ))
                            .await;
                        close_code = 1011;
                        close_reason = "tmux_session_closed".to_owned();
                        break;
                    }
                }
            }
            message = receiver.next() => {
                let Some(message) = message else {
                    break;
                };
                let message = match message {
                    Ok(message) => message,
                    Err(_) => break,
                };
                let Some(frame) = mobile_terminal_client_frame(message)? else {
                    break;
                };
                if process_mobile_terminal_client_frame(
                    &mut sender,
                    &master,
                    &frame,
                    &mut close_code,
                    &mut close_reason,
                )
                .await?
                {
                    break;
                }
            }
        }
    }

    stop.store(true, Ordering::SeqCst);
    let _ = child.kill();
    let _ = child.wait();
    let _ = output_handle.await;
    let _ = sender
        .send(Message::Close(Some(CloseFrame {
            code: close_code,
            reason: close_reason.into(),
        })))
        .await;
    Ok(())
}

async fn wait_for_mobile_terminal_initial_resize(
    socket: &mut WebSocket,
    config: &AppConfig,
) -> MobileTerminalInitialState {
    let mut initial = MobileTerminalInitialState {
        rows: MOBILE_TERMINAL_DEFAULT_ROWS,
        cols: MOBILE_TERMINAL_DEFAULT_COLS,
        pending_frames: Vec::new(),
        detached: false,
    };
    let wait_seconds = finite_f64_or_default(
        config.mobile_terminal.initial_resize_wait_seconds,
        MOBILE_TERMINAL_INITIAL_RESIZE_WAIT_SECONDS,
    )
    .clamp(0.0, 10.0);
    if wait_seconds <= 0.0 {
        return initial;
    }

    let deadline = Instant::now() + Duration::from_secs_f64(wait_seconds);
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return initial;
        };
        let message = match timeout(remaining, socket.recv()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return initial,
        };
        let Some(frame) = (match mobile_terminal_client_frame(message) {
            Ok(frame) => frame,
            Err(_) => continue,
        }) else {
            initial.detached = true;
            return initial;
        };
        match mobile_terminal_frame_type(&frame).as_deref() {
            Some("resize") => {
                if let Some((rows, cols)) = mobile_terminal_resize(&frame) {
                    initial.rows = rows;
                    initial.cols = cols;
                    return initial;
                }
            }
            Some("ping") => {
                let _ =
                    send_mobile_terminal_json(socket, json!({"type": "status", "state": "pong"}))
                        .await;
            }
            Some("detach") => {
                initial.detached = true;
                return initial;
            }
            _ => {
                initial.pending_frames.push(frame);
                return initial;
            }
        }
    }
}

async fn preload_mobile_terminal_scrollback(
    socket: &mut WebSocket,
    config: &AppConfig,
    ticket: &MobileTerminalTicket,
) {
    let lines = config.mobile_terminal.history_preload_lines.min(20_000);
    if lines == 0 {
        return;
    }
    let ticket = ticket.clone();
    let chunk =
        tokio::task::spawn_blocking(move || capture_mobile_terminal_scrollback(&ticket, lines))
            .await
            .ok()
            .flatten();
    let Some(chunk) = chunk else {
        return;
    };
    let _ = send_mobile_terminal_json(
        socket,
        json!({
            "type": "output",
            "mode": "history",
            "encoding": "base64",
            "data": STANDARD.encode(chunk),
        }),
    )
    .await;
}

async fn process_mobile_terminal_client_frame<S>(
    sender: &mut S,
    master: &Arc<OwnedFd>,
    frame: &Value,
    close_code: &mut u16,
    close_reason: &mut String,
) -> Result<bool, String>
where
    S: futures_util::Sink<Message> + Unpin,
{
    match mobile_terminal_frame_type(frame).as_deref() {
        Some("input") => {
            let data = frame.get("data").and_then(Value::as_str).unwrap_or("");
            if data.chars().count() > MOBILE_TERMINAL_INPUT_MAX_CHARS {
                let _ = sender
                    .send(Message::Text(
                        json!({"type": "error", "message": "input frame too large"})
                            .to_string()
                            .into(),
                    ))
                    .await;
                *close_code = 1008;
                *close_reason = "input_frame_too_large".to_owned();
                return Ok(true);
            }
            if !data.is_empty() {
                if let Err(message) =
                    write_mobile_terminal_pty(master.clone(), data.as_bytes().to_vec()).await
                {
                    let _ = sender
                        .send(Message::Text(
                            json!({"type": "error", "message": message})
                                .to_string()
                                .into(),
                        ))
                        .await;
                    *close_code = 1011;
                    *close_reason = "failed_to_deliver_terminal_input".to_owned();
                    return Ok(true);
                }
            }
        }
        Some("key") => {
            let key = frame.get("key").and_then(Value::as_str).unwrap_or("");
            let Some(bytes) = mobile_terminal_key_bytes(key) else {
                let _ = sender
                    .send(Message::Text(
                        json!({"type": "error", "message": format!("unsupported key: {}", key.trim().to_ascii_lowercase())})
                            .to_string()
                            .into(),
                    ))
                    .await;
                return Ok(false);
            };
            if let Err(message) = write_mobile_terminal_pty(master.clone(), bytes).await {
                let _ = sender
                    .send(Message::Text(
                        json!({"type": "error", "message": message})
                            .to_string()
                            .into(),
                    ))
                    .await;
                *close_code = 1011;
                *close_reason = "failed_to_deliver_terminal_key".to_owned();
                return Ok(true);
            }
        }
        Some("resize") => {
            if let Some((rows, cols)) = mobile_terminal_resize(frame) {
                if let Err(message) = resize_mobile_terminal_pty(master.clone(), rows, cols).await {
                    let _ = sender
                        .send(Message::Text(
                            json!({"type": "error", "message": message})
                                .to_string()
                                .into(),
                        ))
                        .await;
                    *close_code = 1011;
                    *close_reason = "failed_to_resize_terminal".to_owned();
                    return Ok(true);
                }
                let _ = sender
                    .send(Message::Text(
                        json!({"type": "status", "state": "resized", "rows": rows, "cols": cols})
                            .to_string()
                            .into(),
                    ))
                    .await;
            } else {
                let _ = sender
                    .send(Message::Text(
                        json!({"type": "error", "message": "ignored invalid resize"})
                            .to_string()
                            .into(),
                    ))
                    .await;
            }
        }
        Some("ping") => {
            let _ = sender
                .send(Message::Text(
                    json!({"type": "status", "state": "pong"})
                        .to_string()
                        .into(),
                ))
                .await;
        }
        Some("detach") => {
            *close_code = 1000;
            *close_reason = "mobile_terminal_detached".to_owned();
            return Ok(true);
        }
        _ => {
            let _ = sender
                .send(Message::Text(
                    json!({"type": "error", "message": "unsupported terminal frame"})
                        .to_string()
                        .into(),
                ))
                .await;
        }
    }
    Ok(false)
}

fn mobile_terminal_client_frame(message: Message) -> Result<Option<Value>, String> {
    let text = match message {
        Message::Text(text) => text.to_string(),
        Message::Binary(bytes) => String::from_utf8(bytes.to_vec())
            .map_err(|_| "unsupported terminal frame".to_owned())?,
        Message::Close(_) => return Ok(None),
        _ => return Err("unsupported terminal frame".to_owned()),
    };
    serde_json::from_str(&text).map_err(|_| "unsupported terminal frame".to_owned())
}

fn mobile_terminal_frame_type(frame: &Value) -> Option<String> {
    frame
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
}

fn mobile_terminal_resize(frame: &Value) -> Option<(u16, u16)> {
    let rows = frame.get("rows").and_then(Value::as_u64)?;
    let cols = frame.get("cols").and_then(Value::as_u64)?;
    if rows < u64::from(MOBILE_TERMINAL_MIN_ROWS)
        || rows > u64::from(MOBILE_TERMINAL_MAX_ROWS)
        || cols < u64::from(MOBILE_TERMINAL_MIN_COLS)
        || cols > u64::from(MOBILE_TERMINAL_MAX_COLS)
    {
        return None;
    }
    Some((rows as u16, cols as u16))
}

fn mobile_terminal_key_bytes(key: &str) -> Option<Vec<u8>> {
    match key.trim().to_ascii_lowercase().as_str() {
        "enter" => Some(b"\r".to_vec()),
        "esc" | "escape" => Some(b"\x1b".to_vec()),
        "tab" => Some(b"\t".to_vec()),
        "backspace" => Some(b"\x7f".to_vec()),
        "ctrl-c" => Some(b"\x03".to_vec()),
        "ctrl-d" => Some(b"\x04".to_vec()),
        "ctrl-z" => Some(b"\x1a".to_vec()),
        "ctrl-b" => Some(b"\x02".to_vec()),
        _ => None,
    }
}

fn mobile_terminal_active_attach_exists(state: &AppState, ticket: &MobileTerminalTicket) -> bool {
    state
        .mobile_terminal_active_attaches
        .lock()
        .map(|active| {
            active.values().any(|attach| {
                attach.user_id == ticket.user_id
                    && attach.session_id == ticket.session_id
                    && attach.device_key_id == ticket.device_key_id
            })
        })
        .unwrap_or(false)
}

#[cfg(unix)]
fn start_mobile_terminal_attach_client(
    ticket: &MobileTerminalTicket,
    rows: u16,
    cols: u16,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<MobileTerminalPty> {
    let size = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(Some(&size), None)?;
    set_mobile_terminal_fd_nonblocking(&pty.master)?;
    let stdout_fd = unsafe { OwnedFd::from_raw_fd(dup(pty.slave.as_raw_fd())?) };
    let stderr_fd = unsafe { OwnedFd::from_raw_fd(dup(pty.slave.as_raw_fd())?) };
    let mut command =
        mobile_terminal_tmux_command(ticket, &["attach-session", "-t", &ticket.tmux_session]);
    command
        .stdin(Stdio::from(fs::File::from(pty.slave)))
        .stdout(Stdio::from(fs::File::from(stdout_fd)))
        .stderr(Stdio::from(fs::File::from(stderr_fd)))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("TERM", "xterm-256color");
    let child = command.spawn()?;
    Ok(MobileTerminalPty {
        master: Arc::new(pty.master),
        child,
        stop,
    })
}

#[cfg(not(unix))]
fn start_mobile_terminal_attach_client(
    _ticket: &MobileTerminalTicket,
    _rows: u16,
    _cols: u16,
    _stop: Arc<AtomicBool>,
) -> anyhow::Result<MobileTerminalPty> {
    anyhow::bail!("mobile terminal PTY bridge is only supported on Unix")
}

#[cfg(unix)]
fn set_mobile_terminal_fd_nonblocking(fd: &OwnedFd) -> anyhow::Result<()> {
    let flags = OFlag::from_bits_truncate(fcntl(fd.as_raw_fd(), FcntlArg::F_GETFL)?);
    fcntl(fd.as_raw_fd(), FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(())
}

#[cfg(unix)]
fn mobile_terminal_output_reader(
    master: Arc<OwnedFd>,
    stop: Arc<AtomicBool>,
    output_tx: mpsc::UnboundedSender<MobileTerminalPtyEvent>,
) {
    let mut buffer = vec![0u8; 8192];
    while !stop.load(Ordering::SeqCst) {
        match nix_read(master.as_raw_fd(), &mut buffer) {
            Ok(0) => {
                let _ = output_tx.send(MobileTerminalPtyEvent::Closed);
                return;
            }
            Ok(n) => {
                if output_tx
                    .send(MobileTerminalPtyEvent::Output(buffer[..n].to_vec()))
                    .is_err()
                {
                    return;
                }
            }
            Err(Errno::EAGAIN) => std::thread::sleep(Duration::from_millis(25)),
            Err(Errno::EINTR) => {}
            Err(_) => {
                let _ = output_tx.send(MobileTerminalPtyEvent::Closed);
                return;
            }
        }
    }
}

#[cfg(not(unix))]
fn mobile_terminal_output_reader(
    _master: Arc<OwnedFd>,
    _stop: Arc<AtomicBool>,
    _output_tx: mpsc::UnboundedSender<MobileTerminalPtyEvent>,
) {
}

async fn write_mobile_terminal_pty(master: Arc<OwnedFd>, data: Vec<u8>) -> Result<(), String> {
    tokio::task::spawn_blocking(move || write_mobile_terminal_pty_blocking(&master, &data))
        .await
        .map_err(|_| "failed to deliver terminal input".to_owned())?
}

#[cfg(unix)]
fn write_mobile_terminal_pty_blocking(master: &OwnedFd, data: &[u8]) -> Result<(), String> {
    let mut offset = 0;
    while offset < data.len() {
        match nix_write(master, &data[offset..]) {
            Ok(0) => return Err("failed to deliver terminal input".to_owned()),
            Ok(n) => offset += n,
            Err(Errno::EAGAIN) => std::thread::sleep(Duration::from_millis(10)),
            Err(Errno::EINTR) => {}
            Err(_) => return Err("failed to deliver terminal input".to_owned()),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn write_mobile_terminal_pty_blocking(_master: &OwnedFd, _data: &[u8]) -> Result<(), String> {
    Err("failed to deliver terminal input".to_owned())
}

async fn resize_mobile_terminal_pty(
    master: Arc<OwnedFd>,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || set_mobile_terminal_pty_size(&master, rows, cols))
        .await
        .map_err(|_| "failed to resize terminal".to_owned())?
}

#[cfg(unix)]
fn set_mobile_terminal_pty_size(master: &OwnedFd, rows: u16, cols: u16) -> Result<(), String> {
    let size = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe { nix::libc::ioctl(master.as_raw_fd(), nix::libc::TIOCSWINSZ, &size) };
    if result == -1 {
        return Err("failed to resize terminal".to_owned());
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mobile_terminal_pty_size(_master: &OwnedFd, _rows: u16, _cols: u16) -> Result<(), String> {
    Err("failed to resize terminal".to_owned())
}

fn capture_mobile_terminal_scrollback(
    ticket: &MobileTerminalTicket,
    lines: usize,
) -> Option<Vec<u8>> {
    let start = format!("-{lines}");
    let output = mobile_terminal_tmux_command(
        ticket,
        &[
            "capture-pane",
            "-e",
            "-p",
            "-S",
            &start,
            "-E",
            "-1",
            "-t",
            &ticket.tmux_session,
        ],
    )
    .output()
    .ok()?;
    if !output.status.success() || output.stdout.iter().all(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    Some(normalize_mobile_terminal_scrollback(&output.stdout))
}

fn normalize_mobile_terminal_scrollback(output: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(output);
    let normalized = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let mut bytes = normalized.into_bytes();
    if !bytes.ends_with(b"\r\n") {
        bytes.extend_from_slice(b"\r\n");
    }
    bytes
}

fn mobile_terminal_tmux_command(ticket: &MobileTerminalTicket, args: &[&str]) -> Command {
    let argv = mobile_terminal_tmux_argv(ticket, args);
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command
}

fn mobile_terminal_tmux_argv(ticket: &MobileTerminalTicket, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["tmux".to_owned()];
    if let Some(socket_name) = ticket.tmux_socket_name.as_deref() {
        argv.extend(["-L".to_owned(), socket_name.to_owned()]);
    }
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    argv
}

fn finite_f64_or_default(value: f64, fallback: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

async fn get_attach_descriptor(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let Some(session) = state.session_store.get_session(&session_id)? else {
        return Err(ApiError::NotFound("Session not found"));
    };
    Ok(Json(attach_descriptor_response(session)))
}

async fn get_context_monitor_status(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    Ok(Json(json!({
        "monitored": state.session_store.list_context_monitors()?,
    })))
}

async fn send_session_input(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<SendCoreInputRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/input"),
    )?;
    ensure_core_writes_enabled(&state)?;
    if payload.text.trim().is_empty() {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "text is required".to_owned(),
        });
    }
    let result = if state.config.rust_core.runtime_enabled {
        ensure_core_runtime_session_node_supported(&state, &session_id)?;
        let runtime = TmuxRuntime::from_app_config(&state.config);
        state
            .session_store
            .send_core_input_with_runtime(&session_id, payload, &runtime)?
    } else {
        state.session_store.send_core_input(&session_id, payload)?
    };
    let Some(result) = result else {
        return Err(ApiError::NotFound("Session not found"));
    };
    Ok(Json(serde_json::to_value(result)?))
}

async fn send_session_input_batch(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<SendCoreInputBatchRequest>,
) -> Result<Json<CoreInputBatchResponse>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        "/sessions/input-batch",
    )?;
    ensure_core_writes_enabled(&state)?;
    if payload.input.text.trim().is_empty() {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "text is required".to_owned(),
        });
    }
    let recipients = unique_identifiers(&payload.recipients);
    if recipients.is_empty() {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "At least one recipient is required".to_owned(),
        });
    }

    let runtime = state
        .config
        .rust_core
        .runtime_enabled
        .then(|| TmuxRuntime::from_app_config(&state.config));
    let mut results = Vec::with_capacity(recipients.len());
    for identifier in recipients {
        results.push(send_session_input_batch_one(
            &state,
            runtime.as_ref(),
            &identifier,
            payload.input.clone(),
        )?);
    }
    let success_count = results
        .iter()
        .filter(|result| matches!(result.status.as_str(), "delivered" | "queued" | "emailed"))
        .count();
    let failure_count = results.len().saturating_sub(success_count);

    Ok(Json(CoreInputBatchResponse {
        ok: failure_count == 0,
        requested_count: results.len(),
        success_count,
        failure_count,
        delivery_mode: payload.input.delivery_mode,
        results,
    }))
}

fn send_session_input_batch_one(
    state: &AppState,
    runtime: Option<&TmuxRuntime>,
    identifier: &str,
    payload: SendCoreInputRequest,
) -> Result<CoreInputBatchResult, ApiError> {
    let Some(session) = state.session_store.get_session(identifier)? else {
        return Ok(failed_batch_result(
            identifier,
            None,
            None,
            None,
            format!("Session '{identifier}' not found"),
        ));
    };
    if runtime.is_some() && !is_primary_node(&session.node) {
        return Ok(failed_batch_result(
            identifier,
            Some(session.id.clone()),
            session_target_name(&session),
            Some(session.provider.clone()),
            format!("Rust runtime does not support remote node {}", session.node),
        ));
    }

    let outcome = if let Some(runtime) = runtime {
        state
            .session_store
            .send_core_input_with_runtime(&session.id, payload, runtime)?
    } else {
        state.session_store.send_core_input(&session.id, payload)?
    };
    let Some(outcome) = outcome else {
        return Ok(failed_batch_result(
            identifier,
            None,
            None,
            None,
            "Session not found".to_owned(),
        ));
    };
    if !outcome.delivered && matches!(outcome.status.as_str(), "stopped" | "killed") {
        return Ok(failed_batch_result(
            identifier,
            Some(outcome.session_id),
            session_target_name(&session),
            Some(session.provider),
            format!("Session {identifier} is stopped"),
        ));
    }
    let status = if outcome.delivered {
        "delivered".to_owned()
    } else {
        "queued".to_owned()
    };
    Ok(CoreInputBatchResult {
        identifier: identifier.to_owned(),
        status: status.clone(),
        delivery_kind: "session".to_owned(),
        session_id: Some(outcome.session_id),
        target_name: session_target_name(&session),
        provider: Some(session.provider),
        bootstrapped: false,
        queue_position: None,
        estimated_delivery: (status == "queued").then(|| "deferred".to_owned()),
        email_username: None,
        email_address: None,
        detail: None,
    })
}

fn session_target_name(session: &SessionRecord) -> Option<String> {
    session
        .friendly_name
        .as_deref()
        .or(Some(session.name.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn failed_batch_result(
    identifier: &str,
    session_id: Option<String>,
    target_name: Option<String>,
    provider: Option<String>,
    detail: String,
) -> CoreInputBatchResult {
    CoreInputBatchResult {
        identifier: identifier.to_owned(),
        status: "failed".to_owned(),
        delivery_kind: "none".to_owned(),
        session_id,
        target_name,
        provider,
        bootstrapped: false,
        queue_position: None,
        estimated_delivery: None,
        email_username: None,
        email_address: None,
        detail: Some(detail),
    }
}

fn unique_identifiers(values: &[String]) -> Vec<String> {
    let mut identifiers = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        for part in value.split(',') {
            let identifier = part.trim();
            if identifier.is_empty() || !seen.insert(identifier.to_owned()) {
                continue;
            }
            identifiers.push(identifier.to_owned());
        }
    }
    identifiers
}

async fn set_agent_status(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<AgentStatusRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/agent-status"),
    )?;
    ensure_core_writes_enabled(&state)?;
    let Some(result) = state.session_store.set_agent_status(&session_id, payload)? else {
        return Err(ApiError::NotFound("Session not found"));
    };
    Ok(Json(serde_json::to_value(result)?))
}

async fn task_complete(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<TaskCompleteRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/task-complete"),
    )?;
    ensure_core_writes_enabled(&state)?;
    let runtime = state
        .config
        .rust_core
        .runtime_enabled
        .then(|| TmuxRuntime::from_app_config(&state.config));
    match state
        .session_store
        .task_complete(&session_id, payload, runtime.as_ref())?
    {
        TaskCompleteOutcome::Completed(result) => Ok(Json(serde_json::to_value(result)?)),
        TaskCompleteOutcome::Error(error) => Ok(Json(json!({ "error": error }))),
    }
}

async fn turn_complete(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<TaskCompleteRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/turn-complete"),
    )?;
    ensure_core_writes_enabled(&state)?;
    match state.session_store.turn_complete(&session_id, payload)? {
        TurnCompleteOutcome::Completed(result) => Ok(Json(serde_json::to_value(result)?)),
        TurnCompleteOutcome::Error(error) => Ok(Json(json!({ "error": error }))),
    }
}

async fn arm_stop_notify(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<ArmStopNotifyRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/notify-on-stop"),
    )?;
    ensure_core_writes_enabled(&state)?;
    match state.session_store.arm_stop_notify(&session_id, payload)? {
        ArmStopNotifyOutcome::Armed(result) | ArmStopNotifyOutcome::Suppressed(result) => {
            Ok(Json(serde_json::to_value(result)?))
        }
        ArmStopNotifyOutcome::NotFound => Err(ApiError::NotFound("Session not found")),
        ArmStopNotifyOutcome::Forbidden(detail) => Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail,
        }),
        ArmStopNotifyOutcome::UnknownSender(sender_session_id) => Err(ApiError::Status {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            detail: format!("sender_session_id {sender_session_id:?} not found"),
        }),
    }
}

async fn retire_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<KillSessionRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/kill"),
    )?;
    ensure_core_writes_enabled(&state)?;
    let requester_session_id = payload
        .requester_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let outcome = if state.config.rust_core.runtime_enabled {
        let runtime = TmuxRuntime::from_app_config(&state.config);
        state.session_store.retire_core_session_with_runtime(
            &session_id,
            requester_session_id,
            &runtime,
        )?
    } else {
        state
            .session_store
            .retire_core_session(&session_id, requester_session_id)?
    };
    match outcome {
        CoreRetireOutcome::Retired(result) => Ok(Json(serde_json::to_value(result)?)),
        CoreRetireOutcome::NotFound => Ok(Json(json!({
            "error": format!("Session {session_id} not found")
        }))),
        CoreRetireOutcome::NotChild => Ok(Json(json!({
            "error": format!("Cannot kill session {session_id} - not your child session")
        }))),
        CoreRetireOutcome::UnsupportedNode(node) => Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: format!("Rust runtime does not support remote node {node}"),
        }),
    }
}

async fn restore_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/restore"),
    )?;
    ensure_core_writes_enabled(&state)?;
    let outcome = if state.config.rust_core.runtime_enabled {
        ensure_core_runtime_session_node_supported(&state, &session_id)?;
        let runtime = TmuxRuntime::from_app_config(&state.config);
        state
            .session_store
            .restore_core_session_with_runtime(&session_id, &runtime)?
    } else {
        state.session_store.restore_core_session(&session_id)?
    };
    let Some(outcome) = outcome else {
        return Err(ApiError::NotFound("Session not found"));
    };
    match outcome {
        CoreRestoreOutcome::Restored(session) => Ok(Json(SessionResponse::from(session))),
        CoreRestoreOutcome::NotStopped => Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: "Session is not stopped".to_owned(),
        }),
        CoreRestoreOutcome::UnsupportedNode(node) => Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: format!("Rust runtime does not support remote node {node}"),
        }),
        CoreRestoreOutcome::UnsupportedProvider(provider) => Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: format!("Rust runtime does not support provider {provider}"),
        }),
        CoreRestoreOutcome::MissingProviderResumeId(provider) => Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: format!("Cannot restore {provider} session without provider_resume_id"),
        }),
    }
}

async fn clear_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<ClearSessionRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/clear"),
    )?;
    ensure_core_writes_enabled(&state)?;
    let result = if state.config.rust_core.runtime_enabled {
        ensure_core_runtime_session_node_supported(&state, &session_id)?;
        let runtime = TmuxRuntime::from_app_config(&state.config);
        state
            .session_store
            .clear_core_session_with_runtime(&session_id, payload, &runtime)?
    } else {
        state
            .session_store
            .clear_core_session(&session_id, payload)?
    };
    match result {
        CoreClearOutcome::Cleared(result) => Ok(Json(serde_json::to_value(result)?)),
        CoreClearOutcome::NotFound => Err(ApiError::NotFound("Session not found")),
        CoreClearOutcome::Unauthorized(detail) => Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail,
        }),
    }
}

async fn set_context_monitor(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<ContextMonitorRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/context-monitor"),
    )?;
    ensure_core_writes_enabled(&state)?;
    match state
        .session_store
        .set_context_monitor(&session_id, payload)?
    {
        ContextMonitorOutcome::Updated(result) => Ok(Json(serde_json::to_value(result)?)),
        ContextMonitorOutcome::NotFound => Err(ApiError::NotFound("Session not found")),
        ContextMonitorOutcome::MissingNotifyTarget => Err(ApiError::Status {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            detail: "notify_session_id required when enabling".to_owned(),
        }),
        ContextMonitorOutcome::NotifyTargetNotFound(notify_session_id) => Err(ApiError::Status {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            detail: format!("notify_session_id {notify_session_id:?} not found"),
        }),
        ContextMonitorOutcome::Unauthorized => Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Cannot configure context monitor - not your session or child session"
                .to_owned(),
        }),
    }
}

async fn register_subagent_start(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<SubagentStartRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/subagents"),
    )?;
    ensure_core_writes_enabled(&state)?;
    match state
        .session_store
        .register_subagent_start(&session_id, payload)?
    {
        SubagentStartOutcome::Registered(result) => Ok(Json(serde_json::to_value(result)?)),
        SubagentStartOutcome::NotFound => Err(ApiError::NotFound("Session not found")),
    }
}

async fn register_subagent_stop(
    State(state): State<Arc<AppState>>,
    Path((session_id, agent_id)): Path<(String, String)>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<SubagentStopRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/subagents/{agent_id}/stop"),
    )?;
    ensure_core_writes_enabled(&state)?;
    match state
        .session_store
        .register_subagent_stop(&session_id, &agent_id, payload)?
    {
        SubagentStopOutcome::Stopped(result) => Ok(Json(serde_json::to_value(result)?)),
        SubagentStopOutcome::SessionNotFound => Err(ApiError::NotFound("Session not found")),
        SubagentStopOutcome::SubagentNotFound(agent_id) => Err(ApiError::Status {
            status: StatusCode::NOT_FOUND,
            detail: format!("Subagent {agent_id} not found"),
        }),
    }
}

async fn list_subagents(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let Some(result) = state.session_store.list_subagents(&session_id)? else {
        return Err(ApiError::NotFound("Session not found"));
    };
    Ok(Json(serde_json::to_value(result)?))
}

async fn schedule_handoff(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<HandoffRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/handoff"),
    )?;
    ensure_core_writes_enabled(&state)?;
    if state.config.rust_core.runtime_enabled {
        ensure_core_runtime_session_node_supported(&state, &session_id)?;
    }
    let result = state.session_store.schedule_handoff(&session_id, payload)?;
    match result {
        HandoffOutcome::Recorded(result) => Ok(Json(serde_json::to_value(result)?)),
        HandoffOutcome::Error(error) => Ok(Json(json!({ "error": error }))),
    }
}

async fn set_maintainer(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<SetMaintainerRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/maintainer"),
    )?;
    ensure_core_writes_enabled(&state)?;
    match state
        .session_store
        .set_maintainer_session(&session_id, payload)?
    {
        MaintainerMutationOutcome::Updated(session) => Ok(Json(SessionResponse::from(session))),
        MaintainerMutationOutcome::NotFound => Err(ApiError::NotFound("Session not found")),
        MaintainerMutationOutcome::BadRequest(detail) => Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail,
        }),
    }
}

async fn clear_maintainer(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<SetMaintainerRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/maintainer"),
    )?;
    ensure_core_writes_enabled(&state)?;
    match state
        .session_store
        .clear_maintainer_session(&session_id, payload)?
    {
        MaintainerMutationOutcome::Updated(session) => Ok(Json(SessionResponse::from(session))),
        MaintainerMutationOutcome::NotFound => Err(ApiError::NotFound("Session not found")),
        MaintainerMutationOutcome::BadRequest(detail) => Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail,
        }),
    }
}

async fn list_agent_registry(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let registrations = state.session_store.list_agent_registrations()?;
    Ok(Json(json!({ "registrations": registrations })))
}

async fn lookup_agent_registry(
    State(state): State<Arc<AppState>>,
    Path(role): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let Some(registration) = state.session_store.lookup_agent_registration(&role)? else {
        return Err(ApiError::Status {
            status: StatusCode::NOT_FOUND,
            detail: "Role not registered".to_owned(),
        });
    };
    Ok(Json(serde_json::to_value(registration)?))
}

async fn register_agent_role(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<RoleRegistrationRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/registry"),
    )?;
    ensure_core_writes_enabled(&state)?;
    registry_mutation_response(
        state
            .session_store
            .register_agent_role(&session_id, payload)?,
    )
}

async fn unregister_agent_role(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<RoleRegistrationRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/registry"),
    )?;
    ensure_core_writes_enabled(&state)?;
    registry_mutation_response(
        state
            .session_store
            .unregister_agent_role(&session_id, payload)?,
    )
}

fn registry_mutation_response(outcome: RegistryMutationOutcome) -> Result<Json<Value>, ApiError> {
    match outcome {
        RegistryMutationOutcome::Registered(registration) => {
            Ok(Json(serde_json::to_value(registration)?))
        }
        RegistryMutationOutcome::NotFound => Err(ApiError::NotFound("Session not found")),
        RegistryMutationOutcome::RoleNotRegistered => Err(ApiError::Status {
            status: StatusCode::NOT_FOUND,
            detail: "Role not registered".to_owned(),
        }),
        RegistryMutationOutcome::RoleNotOwned => Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: "Role is not owned by this session".to_owned(),
        }),
        RegistryMutationOutcome::BadRequest(detail) => Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail,
        }),
        RegistryMutationOutcome::Conflict(detail) => Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail,
        }),
    }
}

async fn session_output(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Query(query): Query<SessionOutputQuery>,
    request: Request,
) -> Result<Json<SessionOutputResponse>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    if state.session_store.get_session(&session_id)?.is_none() {
        return Err(ApiError::NotFound("Session not found"));
    }
    let output = state
        .session_store
        .capture_output(&session_id, query.lines.unwrap_or(50))?;
    Ok(Json(SessionOutputResponse { session_id, output }))
}

async fn session_tool_calls(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Query(query): Query<SessionToolCallsQuery>,
    request: Request,
) -> Result<Json<ToolCallsResponse>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let Some(session) = state.session_store.get_session(&session_id)? else {
        return Err(ApiError::NotFound("Session not found"));
    };
    let Some(limit) = query.validated_limit() else {
        return Err(ApiError::Status {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            detail: "limit must be between 1 and 100".to_owned(),
        });
    };
    if session.provider == "codex-fork" {
        let db_path = expand_home(&state.config.codex_observability.db_path);
        let tool_calls = list_recent_codex_fork_tool_calls_from_path(&db_path, &session_id, limit)?;
        return Ok(Json(ToolCallsResponse {
            session_id,
            tool_calls,
        }));
    }
    let db_path = expand_home(&state.config.tool_logging.db_path);
    let tool_calls = list_recent_tool_calls_from_path(&db_path, &session_id, limit)?;
    Ok(Json(ToolCallsResponse {
        session_id,
        tool_calls,
    }))
}

async fn session_codex_events(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    uri: Uri,
    request: Request,
) -> Result<Json<CodexEventsResponse>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let query = SessionCodexEventsQuery::parse(uri.query().unwrap_or(""))?;
    let Some(session) = state.session_store.get_session(&session_id)? else {
        return Err(ApiError::NotFound("Session not found"));
    };
    if session.provider != "codex-app" {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "codex-events supported only for provider=codex-app".to_owned(),
        });
    }
    if !state.config.codex_rollout.enable_durable_events {
        return Err(ApiError::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "codex durable events disabled by rollout flag".to_owned(),
        });
    }
    let db_path = expand_home(&state.config.codex_events.db_path);
    let response =
        list_codex_events_from_path(&db_path, &session_id, query.since_seq, query.limit)?;
    Ok(Json(response))
}

async fn session_codex_pending_requests(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    uri: Uri,
    request: Request,
) -> Result<Json<CodexPendingRequestsResponse>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let query = SessionCodexPendingRequestsQuery::parse(uri.query().unwrap_or(""))?;
    let Some(session) = state.session_store.get_session(&session_id)? else {
        return Err(ApiError::NotFound("Session not found"));
    };
    if session.provider != "codex-app" {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "codex requests supported only for provider=codex-app".to_owned(),
        });
    }
    if !state.config.codex_rollout.enable_structured_requests {
        return Err(ApiError::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "codex structured requests disabled by rollout flag".to_owned(),
        });
    }
    let db_path = expand_home(&state.config.codex_requests.db_path);
    let response =
        list_codex_pending_requests_from_path(&db_path, &session_id, query.include_orphaned)?;
    Ok(Json(response))
}

async fn shadow_http(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<ShadowHttpResult>, ApiError> {
    let headers = request.headers().clone();
    let peer_addr = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|value| value.0);
    ensure_shadow_allowed(&state.config, &headers, peer_addr)?;
    let body = to_bytes(request.into_body(), SHADOW_ENVELOPE_MAX_BYTES)
        .await
        .map_err(|error| anyhow::anyhow!("failed to read shadow envelope: {error}"))?;
    let envelope: ShadowHttpEnvelope = serde_json::from_slice(&body)?;
    Ok(Json(shadow_compare(&state, envelope)?))
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "detail": "Not Found" })),
    )
}

#[derive(Debug, Deserialize)]
struct ShadowHttpEnvelope {
    request: ShadowRequestEnvelope,
    python_response: ShadowPythonResponse,
}

#[derive(Debug, Deserialize)]
struct ShadowRequestEnvelope {
    method: String,
    path: String,
    #[serde(default)]
    query_string: String,
}

#[derive(Debug, Deserialize)]
struct ShadowPythonResponse {
    status: u16,
    body_sha256: String,
}

#[derive(Serialize)]
struct ShadowHttpResult {
    schema_version: u8,
    method: String,
    path: String,
    support_status: &'static str,
    comparison: &'static str,
    would_write: bool,
    python_status: u16,
    predicted_status: Option<u16>,
    predicted_body_sha256: Option<String>,
    body_sha256_match: Option<bool>,
    detail: Option<String>,
}

struct ShadowPrediction {
    status: u16,
    body_sha256: Option<String>,
    support_status: &'static str,
}

fn shadow_compare(
    state: &AppState,
    envelope: ShadowHttpEnvelope,
) -> anyhow::Result<ShadowHttpResult> {
    let method = envelope.request.method.trim().to_uppercase();
    let path = envelope.request.path.trim().to_owned();
    let python_status = envelope.python_response.status;

    if is_retained_write_surface(&method, &path) {
        return Ok(ShadowHttpResult {
            schema_version: 1,
            method,
            path,
            support_status: "unsupported_retained_write",
            comparison: "not_compared",
            would_write: false,
            python_status,
            predicted_status: None,
            predicted_body_sha256: None,
            body_sha256_match: None,
            detail: Some("Rust shadow mode never performs retained write side effects".to_owned()),
        });
    }

    if is_auth_denial_status(python_status) && is_protected_read_surface(&method, &path) {
        return Ok(ShadowHttpResult {
            schema_version: 1,
            method,
            path,
            support_status: "python_auth_denial",
            comparison: "status_match",
            would_write: false,
            python_status,
            predicted_status: Some(python_status),
            predicted_body_sha256: None,
            body_sha256_match: None,
            detail: Some(
                "Python rejected the protected read before handler execution; shadow mode preserves the denial without reading state"
                    .to_owned(),
            ),
        });
    }

    let Some(prediction) =
        shadow_predict_read(state, &method, &path, &envelope.request.query_string)?
    else {
        return Ok(ShadowHttpResult {
            schema_version: 1,
            method,
            path,
            support_status: "unsupported",
            comparison: "not_compared",
            would_write: false,
            python_status,
            predicted_status: None,
            predicted_body_sha256: None,
            body_sha256_match: None,
            detail: Some("No side-effect-free Rust prediction exists for this surface".to_owned()),
        });
    };

    let status_matches = prediction.status == python_status;
    let body_matches = prediction
        .body_sha256
        .as_ref()
        .map(|value| value == &envelope.python_response.body_sha256);
    let comparison = match (status_matches, body_matches) {
        (true, Some(true)) => "match",
        (true, None) => "status_match",
        (false, _) => "status_mismatch",
        (true, Some(false)) => "body_mismatch",
    };

    Ok(ShadowHttpResult {
        schema_version: 1,
        method,
        path,
        support_status: prediction.support_status,
        comparison,
        would_write: false,
        python_status,
        predicted_status: Some(prediction.status),
        predicted_body_sha256: prediction.body_sha256,
        body_sha256_match: body_matches,
        detail: None,
    })
}

fn shadow_predict_read(
    state: &AppState,
    method: &str,
    path: &str,
    query_string: &str,
) -> anyhow::Result<Option<ShadowPrediction>> {
    if method != "GET" {
        return Ok(None);
    }

    if let Some(request_id) = path
        .strip_prefix("/codex-review-requests/")
        .filter(|value| !value.is_empty() && !value.contains('/'))
    {
        let queue_db_path = expand_home(&state.config.sm_send.db_path);
        return match RetainedQueueStore::get_codex_review_request_from_path(
            &queue_db_path,
            request_id,
        )? {
            Some(_) => Ok(Some(ShadowPrediction {
                status: StatusCode::OK.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            })),
            None => {
                let body =
                    serde_json::to_vec(&json!({ "detail": "Codex review request not found" }))?;
                Ok(Some(ShadowPrediction {
                    status: StatusCode::NOT_FOUND.as_u16(),
                    body_sha256: Some(sha256_hex(&body)),
                    support_status: "implemented_read",
                }))
            }
        };
    }

    if let Some(job_id) = path
        .strip_prefix("/queue-jobs/")
        .filter(|value| !value.is_empty() && !value.contains('/'))
    {
        let queue_state_dir = state.config.queue_runner_state_dir();
        let queue_db_path = expand_home(&queue_state_dir.to_string_lossy()).join("queue_runner.db");
        return match RetainedQueueStore::get_queue_job_from_path(&queue_db_path, job_id)? {
            Some(_) => Ok(Some(ShadowPrediction {
                status: StatusCode::OK.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            })),
            None => {
                let body = serde_json::to_vec(&json!({ "detail": "Queue job not found" }))?;
                Ok(Some(ShadowPrediction {
                    status: StatusCode::NOT_FOUND.as_u16(),
                    body_sha256: Some(sha256_hex(&body)),
                    support_status: "implemented_read",
                }))
            }
        };
    }

    let body = match path {
        "/health" => Some(serde_json::to_vec(&json!({ "status": "healthy" }))?),
        "/health/detailed" => {
            return Ok(Some(ShadowPrediction {
                status: StatusCode::OK.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            }));
        }
        "/auth/session" => {
            return Ok(Some(ShadowPrediction {
                status: StatusCode::OK.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            }));
        }
        "/client/analytics/summary" => {
            return Ok(Some(ShadowPrediction {
                status: StatusCode::OK.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            }));
        }
        "/codex-review-requests" => {
            return Ok(Some(ShadowPrediction {
                status: StatusCode::OK.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            }));
        }
        "/queue-jobs" => {
            return Ok(Some(ShadowPrediction {
                status: StatusCode::OK.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            }));
        }
        "/client/bootstrap" => {
            return Ok(Some(ShadowPrediction {
                status: StatusCode::OK.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            }));
        }
        "/events/state" => Some(serde_json::to_vec(&event_state_payload())?),
        "/nodes" => {
            return Ok(Some(ShadowPrediction {
                status: StatusCode::OK.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            }));
        }
        "/sessions" => {
            return Ok(Some(ShadowPrediction {
                status: StatusCode::OK.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            }));
        }
        "/client/sessions" => {
            return Ok(Some(ShadowPrediction {
                status: StatusCode::OK.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            }));
        }
        _ => return shadow_predict_session_read(state, path, query_string),
    };

    Ok(body.map(|body| ShadowPrediction {
        status: StatusCode::OK.as_u16(),
        body_sha256: Some(sha256_hex(&body)),
        support_status: "implemented_read",
    }))
}

fn shadow_predict_session_read(
    state: &AppState,
    path: &str,
    query_string: &str,
) -> anyhow::Result<Option<ShadowPrediction>> {
    if is_static_sessions_path(path) {
        return Ok(None);
    }

    if path == "/sessions/context-monitor" {
        let body = serde_json::to_vec(&json!({
            "monitored": state.session_store.list_context_monitors()?,
        }))?;
        return Ok(Some(ShadowPrediction {
            status: StatusCode::OK.as_u16(),
            body_sha256: Some(sha256_hex(&body)),
            support_status: "implemented_read",
        }));
    }

    if let Some(parent_session_id) = path
        .strip_prefix("/sessions/")
        .and_then(|value| value.strip_suffix("/children"))
    {
        let children = state.session_store.list_children(
            parent_session_id,
            query_bool(query_string, "recursive"),
            query_value(query_string, "status"),
            query_bool(query_string, "include_terminated"),
        )?;
        let body = serde_json::to_vec(&json!({
            "parent_session_id": parent_session_id,
            "children": children,
        }))?;
        return Ok(Some(ShadowPrediction {
            status: StatusCode::OK.as_u16(),
            body_sha256: Some(sha256_hex(&body)),
            support_status: "implemented_read",
        }));
    }

    if let Some(session_id) = path
        .strip_prefix("/sessions/")
        .and_then(|value| value.strip_suffix("/attach-descriptor"))
    {
        let Some(session) = state.session_store.get_session(session_id)? else {
            let body = serde_json::to_vec(&json!({ "detail": "Session not found" }))?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::NOT_FOUND.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        };
        let body = serde_json::to_vec(&attach_descriptor_response(session))?;
        return Ok(Some(ShadowPrediction {
            status: StatusCode::OK.as_u16(),
            body_sha256: Some(sha256_hex(&body)),
            support_status: "implemented_read",
        }));
    }

    if let Some(session_id) = path
        .strip_prefix("/client/sessions/")
        .filter(|value| !value.contains('/'))
    {
        let Some(_session) = state.session_store.get_session(session_id)? else {
            let body = serde_json::to_vec(&json!({ "detail": "Session not found" }))?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::NOT_FOUND.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        };
        return Ok(Some(ShadowPrediction {
            status: StatusCode::OK.as_u16(),
            body_sha256: None,
            support_status: "implemented_read_status_only",
        }));
    }

    if let Some(session_id) = path
        .strip_prefix("/sessions/")
        .and_then(|value| value.strip_suffix("/output"))
    {
        let Some(_) = state.session_store.get_session(session_id)? else {
            let body = serde_json::to_vec(&json!({ "detail": "Session not found" }))?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::NOT_FOUND.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        };
        let output = state
            .session_store
            .capture_output(session_id, query_usize(query_string, "lines").unwrap_or(50))?;
        let body = serde_json::to_vec(&SessionOutputResponse {
            session_id: session_id.to_owned(),
            output,
        })?;
        return Ok(Some(ShadowPrediction {
            status: StatusCode::OK.as_u16(),
            body_sha256: Some(sha256_hex(&body)),
            support_status: "implemented_read",
        }));
    }

    if let Some(session_id) = path
        .strip_prefix("/sessions/")
        .and_then(|value| value.strip_suffix("/tool-calls"))
        .filter(|value| !value.is_empty() && !value.contains('/'))
    {
        let Some(_) = state.session_store.get_session(session_id)? else {
            let body = serde_json::to_vec(&json!({ "detail": "Session not found" }))?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::NOT_FOUND.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        };
        return Ok(Some(ShadowPrediction {
            status: StatusCode::OK.as_u16(),
            body_sha256: None,
            support_status: "implemented_read_status_only",
        }));
    }

    if let Some(session_id) = path
        .strip_prefix("/sessions/")
        .and_then(|value| value.strip_suffix("/codex-events"))
        .filter(|value| !value.is_empty() && !value.contains('/'))
    {
        if SessionCodexEventsQuery::parse(query_string).is_err() {
            return Ok(Some(ShadowPrediction {
                status: StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            }));
        }
        let Some(session) = state.session_store.get_session(session_id)? else {
            let body = serde_json::to_vec(&json!({ "detail": "Session not found" }))?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::NOT_FOUND.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        };
        if session.provider != "codex-app" {
            let body = serde_json::to_vec(
                &json!({ "detail": "codex-events supported only for provider=codex-app" }),
            )?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::BAD_REQUEST.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        }
        if !state.config.codex_rollout.enable_durable_events {
            let body = serde_json::to_vec(
                &json!({ "detail": "codex durable events disabled by rollout flag" }),
            )?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        }
        return Ok(Some(ShadowPrediction {
            status: StatusCode::OK.as_u16(),
            body_sha256: None,
            support_status: "implemented_read_status_only",
        }));
    }

    if let Some(session_id) = path
        .strip_prefix("/sessions/")
        .and_then(|value| value.strip_suffix("/codex-pending-requests"))
        .filter(|value| !value.is_empty() && !value.contains('/'))
    {
        if SessionCodexPendingRequestsQuery::parse(query_string).is_err() {
            return Ok(Some(ShadowPrediction {
                status: StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
                body_sha256: None,
                support_status: "implemented_read_status_only",
            }));
        }
        let Some(session) = state.session_store.get_session(session_id)? else {
            let body = serde_json::to_vec(&json!({ "detail": "Session not found" }))?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::NOT_FOUND.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        };
        if session.provider != "codex-app" {
            let body = serde_json::to_vec(
                &json!({ "detail": "codex requests supported only for provider=codex-app" }),
            )?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::BAD_REQUEST.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        }
        if !state.config.codex_rollout.enable_structured_requests {
            let body = serde_json::to_vec(
                &json!({ "detail": "codex structured requests disabled by rollout flag" }),
            )?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        }
        return Ok(Some(ShadowPrediction {
            status: StatusCode::OK.as_u16(),
            body_sha256: None,
            support_status: "implemented_read_status_only",
        }));
    }

    if let Some(session_id) = path
        .strip_prefix("/sessions/")
        .and_then(|value| value.strip_suffix("/subagents"))
    {
        let Some(response) = state.session_store.list_subagents(session_id)? else {
            let body = serde_json::to_vec(&json!({ "detail": "Session not found" }))?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::NOT_FOUND.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        };
        let body = serde_json::to_vec(&response)?;
        return Ok(Some(ShadowPrediction {
            status: StatusCode::OK.as_u16(),
            body_sha256: Some(sha256_hex(&body)),
            support_status: "implemented_read",
        }));
    }

    if let Some(session_id) = path
        .strip_prefix("/sessions/")
        .filter(|value| !value.contains('/'))
    {
        let Some(session) = state.session_store.get_session(session_id)? else {
            let body = serde_json::to_vec(&json!({ "detail": "Session not found" }))?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::NOT_FOUND.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        };
        let body = serde_json::to_vec(&SessionResponse::from(session))?;
        return Ok(Some(ShadowPrediction {
            status: StatusCode::OK.as_u16(),
            body_sha256: Some(sha256_hex(&body)),
            support_status: "implemented_read",
        }));
    }

    Ok(None)
}

fn client_bootstrap_response(
    config: &AppConfig,
    mobile_terminal_runtime_disabled: bool,
) -> ClientBootstrapResponse {
    let auth = &config.google_auth;
    let external = &config.external_access;
    let mobile_terminal_ws_url =
        if mobile_terminal_config_enabled(config, mobile_terminal_runtime_disabled) {
            mobile_terminal_ws_url(config, None)
        } else {
            None
        };
    let mobile_terminal_supported = mobile_terminal_ws_url.is_some();

    ClientBootstrapResponse {
        auth: BootstrapAuth {
            mode_name: "browser_session_cookie",
            session_endpoint: "/auth/session",
            login_endpoint: "/auth/google/login",
            logout_endpoint: "/auth/logout",
            device_auth_endpoint: "/auth/device/google",
            device_auth_token_type: "Bearer",
            google_server_client_id: trimmed(&auth.client_id),
        },
        external_access: BootstrapExternalAccess {
            public_http_host: trimmed(&external.public_http_host),
            public_ssh_host: trimmed(&external.public_ssh_host),
            ssh_username: trimmed(&external.ssh_username),
            termux_attach_supported: false,
            mobile_terminal_supported,
            mobile_terminal_ws_url,
        },
        session_open_defaults: SessionOpenDefaults {
            preferred_action: if mobile_terminal_supported {
                "mobile_terminal"
            } else {
                "details"
            },
            termux_package: "com.termux",
        },
    }
}

fn attach_descriptor_payload(session: SessionRecord) -> Value {
    let provider = session.provider.clone();
    let is_stopped = matches!(
        session.status.trim().to_ascii_lowercase().as_str(),
        "stopped" | "killed"
    );
    let has_tmux_session = !session.tmux_session.trim().is_empty();
    let (attach_supported, message) = if is_stopped {
        (false, Some("Session is stopped".to_owned()))
    } else if provider == "codex-app" {
        (
            false,
            Some("Attach not supported for Codex app sessions".to_owned()),
        )
    } else if !has_tmux_session {
        (false, Some("Session has no tmux session".to_owned()))
    } else {
        (true, None)
    };
    json!({
        "session_id": session.id,
        "provider": provider,
        "attach_supported": attach_supported,
        "tmux_session": session.tmux_session,
        "tmux_socket_name": session.tmux_socket_name,
        "runtime_id": null,
        "lifecycle_state": session.status,
        "message": message,
    })
}

fn attach_descriptor_response(session: SessionRecord) -> Value {
    json!({
        "attach": attach_descriptor_payload(session),
    })
}

fn client_session_value(
    state: &AppState,
    session: SessionRecord,
    actor_email: Option<&str>,
    route_prefix: Option<&str>,
) -> Value {
    let mut value = serde_json::to_value(ClientSessionResponse::from(session.clone()))
        .unwrap_or_else(|_| json!({}));
    let attach_descriptor = attach_descriptor_payload(session.clone());
    value["attach_descriptor"] = attach_descriptor.clone();
    value["termux_attach"] = Value::Null;
    let mobile_terminal = mobile_terminal_metadata(
        state,
        &session,
        &attach_descriptor,
        actor_email,
        route_prefix,
        mobile_terminal_runtime_disabled(state),
    );
    value["mobile_terminal"] = mobile_terminal.clone();
    value["primary_action"] = mobile_primary_action(&mobile_terminal, &attach_descriptor);
    value
}

fn mobile_primary_action(mobile_terminal: &Value, attach_descriptor: &Value) -> Value {
    if mobile_terminal
        .get("supported")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        json!({
            "type": "mobile_terminal",
            "label": "Attach",
        })
    } else if attach_descriptor
        .get("attach_supported")
        .and_then(Value::as_bool)
        .unwrap_or(true)
        != true
    {
        json!({
            "type": "details",
            "label": "View details",
            "reason": attach_descriptor
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("attach not supported"),
        })
    } else {
        json!({
            "type": "details",
            "label": "View details",
        })
    }
}

fn mobile_terminal_metadata(
    state: &AppState,
    session: &SessionRecord,
    attach_descriptor: &Value,
    actor_email: Option<&str>,
    route_prefix: Option<&str>,
    runtime_disabled: bool,
) -> Value {
    let config = &state.config;
    if !mobile_terminal_config_enabled(config, runtime_disabled) {
        return json!({
            "supported": false,
            "reason": "mobile terminal attach is disabled",
        });
    }
    let Some(ws_url) = mobile_terminal_ws_url(config, route_prefix) else {
        return json!({
            "supported": false,
            "reason": "mobile terminal public HTTPS host is not configured",
        });
    };
    let Some(actor_email) = actor_email.map(str::trim).filter(|value| !value.is_empty()) else {
        return json!({
            "supported": false,
            "reason": "authenticated mobile terminal user is required",
        });
    };
    let Some((user_id, user_config)) = mobile_terminal_visible_user(config, actor_email) else {
        return json!({
            "supported": false,
            "reason": "mobile terminal user is not configured",
        });
    };
    if !user_config.interactive_shell_access {
        return json!({
            "supported": false,
            "reason": "interactive shell access is not enabled",
        });
    }
    if !mobile_terminal_user_has_available_registered_device(state, user_id, user_config) {
        return json!({
            "supported": false,
            "reason": "registered mobile device key is required",
        });
    }
    if attach_descriptor
        .get("attach_supported")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        != true
    {
        return json!({
            "supported": false,
            "reason": attach_descriptor
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Session is not attachable"),
        });
    }
    let tmux_session = attach_descriptor
        .get("tmux_session")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let tmux_socket_name = attach_descriptor
        .get("tmux_socket_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Err(error) = validate_mobile_terminal_tmux_target(tmux_session, tmux_socket_name) {
        return json!({
            "supported": false,
            "reason": api_error_detail(&error),
        });
    }

    json!({
        "supported": true,
        "transport": "sm-https-tmux",
        "ticket_endpoint": mobile_terminal_attach_ticket_path(config, &session.id, route_prefix),
        "ws_url": ws_url,
        "tmux_session": tmux_session,
        "tmux_socket_name": tmux_socket_name,
        "runtime_mode": "detached_runtime",
        "requires_device_key": true,
    })
}

struct MobileTerminalAuthorization {
    user_id: String,
    device_key_id: String,
    ws_url: String,
    tmux_session: String,
    tmux_socket_name: Option<String>,
    proof_nonce: String,
    proof_nonce_expires_at_unix: i64,
}

fn authorize_mobile_terminal_ticket_request(
    state: &AppState,
    headers: &HeaderMap,
    route_prefix: Option<&str>,
    session: &SessionRecord,
    actor_email: &str,
) -> Result<MobileTerminalAuthorization, ApiError> {
    let config = &state.config;
    if !config.mobile_terminal.enabled {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Mobile terminal attach is disabled".to_owned(),
        });
    }
    if matches!(
        session.status.trim().to_ascii_lowercase().as_str(),
        "stopped" | "killed"
    ) {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: "Session is not running".to_owned(),
        });
    }
    let Some(ws_url) = mobile_terminal_ws_url(config, route_prefix) else {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "mobile terminal public HTTPS host is not configured".to_owned(),
        });
    };
    let Some((user_id, user_config)) = mobile_terminal_visible_user(config, actor_email) else {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "mobile terminal user is not configured".to_owned(),
        });
    };
    if !user_config.interactive_shell_access {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "interactive shell access is not enabled".to_owned(),
        });
    }
    if !mobile_terminal_user_has_registered_device(user_config) {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "registered mobile device key is required".to_owned(),
        });
    }

    let attach = attach_descriptor_payload(session.clone());
    if attach
        .get("attach_supported")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        != true
    {
        let detail = attach
            .get("message")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("Attach not supported");
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: detail.to_owned(),
        });
    }
    let tmux_session = attach
        .get("tmux_session")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let tmux_socket_name = attach
        .get("tmux_socket_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    validate_mobile_terminal_tmux_target(&tmux_session, tmux_socket_name.as_deref())?;

    let device_key_id = header_text(headers, "x-sm-device-key-id");
    let timestamp = header_text(headers, "x-sm-device-timestamp");
    let nonce = header_text(headers, "x-sm-device-nonce");
    let signature = header_text(headers, "x-sm-device-signature");
    if device_key_id.is_none() || timestamp.is_none() || nonce.is_none() || signature.is_none() {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Device key proof is required".to_owned(),
        });
    }
    let device_key_id = device_key_id.unwrap();
    let timestamp = timestamp.unwrap();
    let nonce = nonce.unwrap();
    let signature = signature.unwrap();
    let proof_nonce_expires_at_unix = validate_mobile_terminal_timestamp(
        &timestamp,
        config.mobile_terminal.device_signature_max_skew_seconds,
    )?;
    let Some(device_config) =
        mobile_terminal_device_config(state, user_id, user_config, &device_key_id)?
    else {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Device key is not registered".to_owned(),
        });
    };
    let message = mobile_terminal_ticket_message(
        "POST",
        &mobile_terminal_attach_ticket_path(config, &session.id, route_prefix),
        &session.id,
        actor_email,
        &device_key_id,
        &timestamp,
        &nonce,
    );
    verify_mobile_terminal_p256_signature(&device_config.public_key, &signature, &message)?;

    Ok(MobileTerminalAuthorization {
        user_id: user_id.to_owned(),
        device_key_id,
        ws_url,
        tmux_session,
        tmux_socket_name,
        proof_nonce: nonce,
        proof_nonce_expires_at_unix,
    })
}

fn mobile_terminal_visible_user<'a>(
    config: &'a AppConfig,
    actor_email: &str,
) -> Option<(&'a str, &'a MobileTerminalUserConfig)> {
    let actor = actor_email.trim().to_ascii_lowercase();
    if actor.is_empty() {
        return None;
    }
    config
        .mobile_terminal
        .allowed_users
        .iter()
        .find_map(|(user_id, user_config)| {
            let mut candidates = vec![user_id.trim().to_ascii_lowercase()];
            if let Some(email) = trimmed(&user_config.email) {
                candidates.push(email.to_ascii_lowercase());
            }
            candidates.extend(
                user_config
                    .aliases
                    .iter()
                    .map(|alias| alias.trim().to_ascii_lowercase())
                    .filter(|alias| !alias.is_empty()),
            );
            candidates
                .iter()
                .any(|candidate| !candidate.is_empty() && candidate == &actor)
                .then_some((user_id.as_str(), user_config))
        })
}

fn mobile_terminal_user_has_registered_device(user_config: &MobileTerminalUserConfig) -> bool {
    user_config
        .registered_device_keys
        .iter()
        .any(|key| key.enabled && !key.id.trim().is_empty() && !key.public_key.trim().is_empty())
}

fn mobile_terminal_user_has_available_registered_device(
    state: &AppState,
    user_id: &str,
    user_config: &MobileTerminalUserConfig,
) -> bool {
    let Ok(revoked_keys) = state.mobile_terminal_revoked_keys.lock() else {
        return false;
    };
    user_config.registered_device_keys.iter().any(|key| {
        let device_key_id = key.id.trim();
        key.enabled
            && !device_key_id.is_empty()
            && !key.public_key.trim().is_empty()
            && !revoked_keys.contains(&(user_id.to_owned(), device_key_id.to_owned()))
    })
}

fn mobile_terminal_device_config<'a>(
    state: &AppState,
    user_id: &str,
    user_config: &'a MobileTerminalUserConfig,
    device_key_id: &str,
) -> Result<Option<&'a MobileTerminalDeviceKeyConfig>, ApiError> {
    if mobile_terminal_device_revoked(state, user_id, device_key_id)? {
        return Ok(None);
    }
    Ok(user_config.registered_device_keys.iter().find(|key| {
        key.enabled && key.id.trim() == device_key_id && !key.public_key.trim().is_empty()
    }))
}

fn mobile_terminal_device_revoked(
    state: &AppState,
    user_id: &str,
    device_key_id: &str,
) -> Result<bool, ApiError> {
    Ok(state
        .mobile_terminal_revoked_keys
        .lock()
        .map_err(|_| ApiError::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: "Mobile terminal revoked key store is unavailable".to_owned(),
        })?
        .contains(&(user_id.to_owned(), device_key_id.to_owned())))
}

fn mobile_terminal_user_device_exists(
    config: &AppConfig,
    user_id: &str,
    device_key_id: &str,
) -> bool {
    config
        .mobile_terminal
        .allowed_users
        .get(user_id)
        .map(|user_config| {
            user_config
                .registered_device_keys
                .iter()
                .any(|key| key.id.trim() == device_key_id)
        })
        .unwrap_or(false)
}

fn resolve_mobile_terminal_revoke_target(
    config: &AppConfig,
    actor_user_id: &str,
    owner_view: bool,
    device_key_id: &str,
    requested_user_id: Option<&str>,
) -> Result<String, ApiError> {
    if let Some(requested_user_id) = requested_user_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if requested_user_id != actor_user_id && !owner_view {
            return Err(ApiError::Status {
                status: StatusCode::FORBIDDEN,
                detail: "User is not allowed to revoke this mobile terminal device".to_owned(),
            });
        }
        if mobile_terminal_user_device_exists(config, requested_user_id, device_key_id) {
            return Ok(requested_user_id.to_owned());
        }
        return Err(ApiError::NotFound("Mobile terminal device not found"));
    }

    if mobile_terminal_user_device_exists(config, actor_user_id, device_key_id) {
        return Ok(actor_user_id.to_owned());
    }
    if !owner_view {
        return Err(ApiError::NotFound("Mobile terminal device not found"));
    }

    let matches = config
        .mobile_terminal
        .allowed_users
        .iter()
        .filter(|(_, user_config)| {
            user_config
                .registered_device_keys
                .iter()
                .any(|key| key.id.trim() == device_key_id)
        })
        .map(|(user_id, _)| user_id.clone())
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(ApiError::NotFound("Mobile terminal device not found")),
        [user_id] => Ok(user_id.clone()),
        _ => Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: "Mobile terminal device id matches multiple users; specify user_id".to_owned(),
        }),
    }
}

fn revoke_mobile_terminal_device_in_state(
    state: &AppState,
    user_id: &str,
    device_key_id: &str,
) -> Result<(bool, usize, Vec<Arc<AtomicBool>>), ApiError> {
    let already_revoked = {
        let mut revoked =
            state
                .mobile_terminal_revoked_keys
                .lock()
                .map_err(|_| ApiError::Status {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    detail: "Mobile terminal revoked key store is unavailable".to_owned(),
                })?;
        !revoked.insert((user_id.to_owned(), device_key_id.to_owned()))
    };

    let pending_tickets_revoked = {
        let mut tickets = state
            .mobile_terminal_tickets
            .lock()
            .map_err(|_| ApiError::Status {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                detail: "Mobile terminal ticket store is unavailable".to_owned(),
            })?;
        let before = tickets.len();
        tickets.retain(|_, ticket| {
            !(ticket.user_id == user_id && ticket.device_key_id == device_key_id)
        });
        before - tickets.len()
    };

    let active_stops = {
        let mut active =
            state
                .mobile_terminal_active_attaches
                .lock()
                .map_err(|_| ApiError::Status {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    detail: "Mobile terminal active attach store is unavailable".to_owned(),
                })?;
        let matches = active
            .iter()
            .filter(|(_, attach)| {
                attach.user_id == user_id && attach.device_key_id == device_key_id
            })
            .map(|(attach_id, attach)| (attach_id.clone(), attach.stop.clone()))
            .collect::<Vec<_>>();
        for (attach_id, _) in &matches {
            active.remove(attach_id);
        }
        matches
            .into_iter()
            .map(|(_, stop)| stop)
            .collect::<Vec<_>>()
    };

    Ok((already_revoked, pending_tickets_revoked, active_stops))
}

fn mobile_terminal_user_can_disable(user_config: &MobileTerminalUserConfig) -> bool {
    user_config.mobile_terminal_owner
        || user_config.can_disable_mobile_terminal
        || user_config.owner
}

fn mobile_terminal_runtime_disabled(state: &AppState) -> bool {
    state
        .mobile_terminal_runtime_disabled
        .load(Ordering::SeqCst)
}

fn mobile_terminal_config_enabled(config: &AppConfig, runtime_disabled: bool) -> bool {
    config.mobile_terminal.enabled && !runtime_disabled
}

fn mobile_terminal_enabled(state: &AppState) -> bool {
    mobile_terminal_config_enabled(&state.config, mobile_terminal_runtime_disabled(state))
}

fn ensure_mobile_terminal_ticket_runtime_enabled(state: &AppState) -> Result<(), ApiError> {
    if mobile_terminal_runtime_disabled(state) {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Mobile terminal attach is disabled".to_owned(),
        });
    }
    Ok(())
}

fn mobile_terminal_ws_url(config: &AppConfig, route_prefix: Option<&str>) -> Option<String> {
    if let Some(configured) = trimmed(&config.mobile_terminal.ws_url) {
        if config.mobile_terminal.require_tls
            && !configured.to_ascii_lowercase().starts_with("wss://")
        {
            return None;
        }
        return Some(configured);
    }
    let host = trimmed(&config.external_access.public_http_host)
        .or_else(|| trimmed(&config.google_auth.public_host))?;
    let host_lower = host.to_ascii_lowercase();
    let scheme = if !config.mobile_terminal.require_tls
        && (host_lower.starts_with("localhost")
            || host_lower.starts_with("127.0.0.1")
            || host_lower.starts_with("testserver"))
    {
        "ws"
    } else {
        "wss"
    };
    let prefix = mobile_terminal_public_http_path_prefix(config, route_prefix);
    Some(format!("{scheme}://{host}{prefix}/client/terminal"))
}

fn mobile_terminal_attach_ticket_path(
    config: &AppConfig,
    session_id: &str,
    route_prefix: Option<&str>,
) -> String {
    let prefix = mobile_terminal_public_http_path_prefix(config, route_prefix);
    format!("{prefix}/client/sessions/{session_id}/attach-ticket")
}

fn mobile_terminal_public_http_path_prefix(
    config: &AppConfig,
    route_prefix: Option<&str>,
) -> String {
    if let Some(route_prefix) = route_prefix {
        return normalize_mobile_terminal_path_prefix(route_prefix);
    }
    if let Some(prefix) = trimmed(&config.mobile_terminal.public_path_prefix) {
        return normalize_mobile_terminal_path_prefix(&prefix);
    }
    if let Some(prefix) = trimmed(&config.external_access.public_http_path_prefix) {
        return normalize_mobile_terminal_path_prefix(&prefix);
    }
    if let Some(prefix) = trimmed(&config.google_auth.public_path_prefix) {
        return normalize_mobile_terminal_path_prefix(&prefix);
    }
    String::new()
}

fn normalize_mobile_terminal_path_prefix(value: &str) -> String {
    let mut prefix = value.trim().to_owned();
    if prefix.is_empty() || prefix == "/" {
        return String::new();
    }
    if !prefix.starts_with('/') {
        prefix.insert(0, '/');
    }
    while prefix.ends_with('/') {
        prefix.pop();
    }
    prefix
}

fn mobile_terminal_request_path_prefix(path: &str, route_suffix: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    let suffix = route_suffix.trim_end_matches('/');
    if path == suffix {
        return None;
    }
    path.strip_suffix(suffix)
        .map(normalize_mobile_terminal_path_prefix)
        .filter(|prefix| !prefix.is_empty())
}

fn validate_mobile_terminal_tmux_target(
    tmux_session: &str,
    tmux_socket_name: Option<&str>,
) -> Result<(), ApiError> {
    if tmux_session.is_empty()
        || !tmux_session
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | ':' | '@' | '-'))
    {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Unsafe tmux session target".to_owned(),
        });
    }
    if let Some(socket_name) = tmux_socket_name {
        if !socket_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
        {
            return Err(ApiError::Status {
                status: StatusCode::FORBIDDEN,
                detail: "Unsafe tmux socket target".to_owned(),
            });
        }
    }
    Ok(())
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn validate_mobile_terminal_timestamp(
    timestamp: &str,
    max_skew_seconds: u64,
) -> Result<i64, ApiError> {
    let Ok(timestamp) = timestamp.parse::<f64>() else {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Invalid device timestamp".to_owned(),
        });
    };
    if !timestamp.is_finite() {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Invalid device timestamp".to_owned(),
        });
    }
    let max_skew = clamp_u64(max_skew_seconds, 5, 600, 60) as f64;
    let now = OffsetDateTime::now_utc().unix_timestamp() as f64;
    if (now - timestamp).abs() > max_skew {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Expired device signature".to_owned(),
        });
    }
    Ok((timestamp + max_skew).ceil() as i64)
}

fn ensure_public_edge_assertion_for_request(
    state: &AppState,
    request: &Request,
) -> Result<(), ApiError> {
    let peer_addr = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|value| value.0);
    let path = request_target_from_uri(request.uri());
    ensure_public_edge_assertion_from_parts(
        state,
        request.headers(),
        peer_addr,
        request.method().as_str(),
        &path,
    )
}

fn request_target_from_uri(uri: &Uri) -> String {
    uri.path_and_query()
        .map(|value| value.as_str().to_owned())
        .unwrap_or_else(|| uri.path().to_owned())
}

fn ensure_public_edge_assertion_from_parts(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
    method: &str,
    path: &str,
) -> Result<(), ApiError> {
    let config = &state.config.public_edge;
    if !config.enabled {
        return Ok(());
    }
    if is_local_bypass_request(headers, peer_addr, &state.config) {
        return Ok(());
    }
    let Some(secret) = trimmed(&config.assertion_secret) else {
        return Err(ApiError::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "Public edge assertion is enabled but incomplete".to_owned(),
        });
    };
    let Some(timestamp) = header_text(headers, "x-sm-edge-timestamp") else {
        return Err(public_edge_assertion_required());
    };
    let Some(nonce) = header_text(headers, "x-sm-edge-nonce") else {
        return Err(public_edge_assertion_required());
    };
    let Some(signature) = header_text(headers, "x-sm-edge-signature") else {
        return Err(public_edge_assertion_required());
    };
    let expires_at_unix =
        validate_public_edge_timestamp(&timestamp, config.assertion_max_skew_seconds)?;
    verify_public_edge_assertion(&secret, method, path, &timestamp, &nonce, &signature)?;
    record_public_edge_assertion_nonce(
        state,
        method,
        path,
        &nonce,
        expires_at_unix,
        OffsetDateTime::now_utc().unix_timestamp(),
    )
}

fn public_edge_assertion_required() -> ApiError {
    ApiError::Status {
        status: StatusCode::FORBIDDEN,
        detail: "Public edge assertion is required".to_owned(),
    }
}

fn validate_public_edge_timestamp(timestamp: &str, max_skew_seconds: u64) -> Result<i64, ApiError> {
    let Ok(timestamp) = timestamp.parse::<f64>() else {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Invalid public edge timestamp".to_owned(),
        });
    };
    if !timestamp.is_finite() {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Invalid public edge timestamp".to_owned(),
        });
    }
    let max_skew = clamp_u64(max_skew_seconds, 5, 600, 60) as f64;
    let now = OffsetDateTime::now_utc().unix_timestamp() as f64;
    if (now - timestamp).abs() > max_skew {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Expired public edge assertion".to_owned(),
        });
    }
    Ok((timestamp + max_skew).ceil() as i64)
}

fn public_edge_assertion_message(method: &str, path: &str, timestamp: &str, nonce: &str) -> String {
    [
        "SM-PUBLIC-EDGE-V1",
        &method.to_ascii_uppercase(),
        path,
        timestamp,
        nonce,
    ]
    .join("\n")
}

fn verify_public_edge_assertion(
    secret: &str,
    method: &str,
    path: &str,
    timestamp: &str,
    nonce: &str,
    signature: &str,
) -> Result<(), ApiError> {
    let signature = STANDARD
        .decode(signature.trim())
        .or_else(|_| URL_SAFE_NO_PAD.decode(signature.trim()))
        .map_err(|_| ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Invalid public edge assertion".to_owned(),
        })?;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| ApiError::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "Public edge assertion signer is unavailable".to_owned(),
        })?;
    mac.update(public_edge_assertion_message(method, path, timestamp, nonce).as_bytes());
    let expected = mac.finalize().into_bytes();
    if constant_time_eq(&signature, &expected) {
        Ok(())
    } else {
        Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Invalid public edge assertion".to_owned(),
        })
    }
}

fn record_public_edge_assertion_nonce(
    state: &AppState,
    method: &str,
    path: &str,
    nonce: &str,
    expires_at_unix: i64,
    now_unix: i64,
) -> Result<(), ApiError> {
    let key = public_edge_assertion_nonce_key(method, path, nonce);
    let mut nonces = state
        .public_edge_assertion_nonces
        .lock()
        .map_err(|_| ApiError::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: "Public edge assertion nonce store is unavailable".to_owned(),
        })?;
    nonces.retain(|_, expires_at| *expires_at > now_unix);
    if nonces.contains_key(&key) {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Public edge assertion nonce was already used".to_owned(),
        });
    }
    nonces.insert(key, expires_at_unix.max(now_unix + 1));
    Ok(())
}

fn public_edge_assertion_nonce_key(method: &str, path: &str, nonce: &str) -> String {
    [
        method.to_ascii_uppercase(),
        path.to_owned(),
        nonce.to_owned(),
    ]
    .join("\x1f")
}

fn mobile_terminal_ticket_message(
    method: &str,
    path: &str,
    session_id: &str,
    actor_email: &str,
    device_key_id: &str,
    timestamp: &str,
    nonce: &str,
) -> String {
    [
        "SM-MOBILE-TERMINAL-TICKET-V1",
        &method.to_ascii_uppercase(),
        path,
        session_id,
        &actor_email.to_ascii_lowercase(),
        device_key_id,
        timestamp,
        nonce,
    ]
    .join("\n")
}

fn verify_mobile_terminal_p256_signature(
    public_key_text: &str,
    signature_text: &str,
    message: &str,
) -> Result<(), ApiError> {
    let signature = mobile_terminal_signature_bytes(signature_text)?;
    let public_key = VerifyingKey::from_public_key_pem(public_key_text.trim()).map_err(|_| {
        ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Invalid device public key".to_owned(),
        }
    })?;
    let signature = Signature::from_der(&signature).map_err(|_| ApiError::Status {
        status: StatusCode::UNAUTHORIZED,
        detail: "Invalid device signature".to_owned(),
    })?;
    public_key
        .verify(message.as_bytes(), &signature)
        .map_err(|_| ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Invalid device signature".to_owned(),
        })
}

fn mobile_terminal_signature_bytes(signature_text: &str) -> Result<Vec<u8>, ApiError> {
    let signature_text = signature_text.trim();
    if signature_text.is_empty() {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Missing device signature".to_owned(),
        });
    }
    STANDARD
        .decode(signature_text)
        .or_else(|_| URL_SAFE_NO_PAD.decode(signature_text))
        .map_err(|_| ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Invalid device signature encoding".to_owned(),
        })
}

fn random_urlsafe_token(bytes_len: usize) -> String {
    let mut bytes = vec![0u8; bytes_len];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn mobile_terminal_secret_hash(secret: &[u8], ticket_secret: &str) -> Result<String, ApiError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).map_err(|_| ApiError::Status {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        detail: "Mobile terminal ticket signer is unavailable".to_owned(),
    })?;
    mac.update(ticket_secret.as_bytes());
    Ok(hex_lower(&mac.finalize().into_bytes()))
}

fn cleanup_mobile_terminal_tickets(
    tickets: &mut BTreeMap<String, MobileTerminalTicket>,
    now_unix: i64,
) {
    tickets.retain(|_, ticket| ticket.expires_at_unix > now_unix);
}

fn record_mobile_terminal_proof_nonce(
    state: &AppState,
    user_id: &str,
    device_key_id: &str,
    session_id: &str,
    nonce: &str,
    expires_at_unix: i64,
    now_unix: i64,
) -> Result<(), ApiError> {
    let key = mobile_terminal_proof_nonce_key(user_id, device_key_id, session_id, nonce);
    let mut nonces = state
        .mobile_terminal_proof_nonces
        .lock()
        .map_err(|_| ApiError::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: "Mobile terminal proof nonce store is unavailable".to_owned(),
        })?;
    nonces.retain(|_, expires_at| *expires_at > now_unix);
    if nonces.contains_key(&key) {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Device signature nonce was already used".to_owned(),
        });
    }
    nonces.insert(key, expires_at_unix.max(now_unix + 1));
    Ok(())
}

fn mobile_terminal_proof_nonce_key(
    user_id: &str,
    device_key_id: &str,
    session_id: &str,
    nonce: &str,
) -> String {
    [user_id, device_key_id, session_id, nonce].join("\u{1f}")
}

fn consume_mobile_terminal_ticket(
    state: &AppState,
    frame: &MobileTerminalAuthFrame,
) -> Result<(MobileTerminalTicket, String, Arc<AtomicBool>), ApiError> {
    if !mobile_terminal_enabled(state) {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "mobile terminal attach is disabled".to_owned(),
        });
    }
    let ticket_id = nonempty_frame_field(&frame.ticket_id);
    let ticket_secret = nonempty_frame_field(&frame.ticket_secret);
    let device_key_id = nonempty_frame_field(&frame.device_key_id);
    let nonce = nonempty_frame_field(&frame.nonce);
    let signature = nonempty_frame_field(&frame.signature);
    if ticket_id.is_none()
        || ticket_secret.is_none()
        || device_key_id.is_none()
        || nonce.is_none()
        || signature.is_none()
    {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Invalid terminal auth frame".to_owned(),
        });
    }
    let ticket_id = ticket_id.unwrap();
    let ticket_secret = ticket_secret.unwrap();
    let device_key_id = device_key_id.unwrap();
    let nonce = nonce.unwrap();
    let signature = signature.unwrap();
    let now = OffsetDateTime::now_utc().unix_timestamp();

    let ticket =
        mobile_terminal_ticket_for_consume(state, &ticket_id, &ticket_secret, &device_key_id, now)?;
    let Some((user_id, user_config)) =
        mobile_terminal_visible_user(&state.config, &ticket.actor_email)
    else {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "User is no longer allowed to attach".to_owned(),
        });
    };
    if user_id != ticket.user_id {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "User is no longer allowed to attach".to_owned(),
        });
    }
    let Some(device_config) =
        mobile_terminal_device_config(state, &ticket.user_id, user_config, &device_key_id)?
    else {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Device key is no longer registered".to_owned(),
        });
    };
    let message = mobile_terminal_ws_message(
        &ticket.ticket_id,
        &ticket.session_id,
        &ticket.actor_email,
        &device_key_id,
        &nonce,
    );
    verify_mobile_terminal_p256_signature(&device_config.public_key, &signature, &message)?;

    let Some(session) = state.session_store.get_session(&ticket.session_id)? else {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: "Session is no longer attachable".to_owned(),
        });
    };
    if matches!(
        session.status.trim().to_ascii_lowercase().as_str(),
        "stopped" | "killed"
    ) {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: "Session is no longer attachable".to_owned(),
        });
    }
    let attach = attach_descriptor_payload(session);
    if attach
        .get("attach_supported")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        != true
    {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: attach
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Session is no longer attachable")
                .to_owned(),
        });
    }
    validate_mobile_terminal_tmux_target(&ticket.tmux_session, ticket.tmux_socket_name.as_deref())?;

    if !mobile_terminal_enabled(state) {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "mobile terminal attach is disabled".to_owned(),
        });
    }
    let mut tickets = state
        .mobile_terminal_tickets
        .lock()
        .map_err(|_| ApiError::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: "Mobile terminal ticket store is unavailable".to_owned(),
        })?;
    let Some(current_ticket) = tickets.get(&ticket_id).cloned() else {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Attach ticket is invalid or expired".to_owned(),
        });
    };
    validate_mobile_terminal_ticket_secret(
        state,
        &current_ticket,
        &ticket_id,
        &ticket_secret,
        &device_key_id,
    )?;
    if current_ticket.expires_at_unix <= now {
        tickets.remove(&ticket_id);
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Attach ticket is expired or consumed".to_owned(),
        });
    }
    let mut active =
        state
            .mobile_terminal_active_attaches
            .lock()
            .map_err(|_| ApiError::Status {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                detail: "Mobile terminal active attach store is unavailable".to_owned(),
            })?;
    enforce_mobile_terminal_active_limits(&state.config, active.values(), &current_ticket)?;
    let ticket = tickets.remove(&ticket_id).expect("ticket checked above");
    let attach_id = random_urlsafe_token(16);
    let stop = Arc::new(AtomicBool::new(false));
    active.insert(
        attach_id.clone(),
        MobileTerminalActiveAttach {
            user_id: ticket.user_id.clone(),
            session_id: ticket.session_id.clone(),
            provider: ticket.provider.clone(),
            device_key_id: ticket.device_key_id.clone(),
            started_at_unix: now,
            stop: stop.clone(),
        },
    );
    Ok((ticket, attach_id, stop))
}

fn mobile_terminal_ticket_for_consume(
    state: &AppState,
    ticket_id: &str,
    ticket_secret: &str,
    device_key_id: &str,
    now_unix: i64,
) -> Result<MobileTerminalTicket, ApiError> {
    let mut tickets = state
        .mobile_terminal_tickets
        .lock()
        .map_err(|_| ApiError::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: "Mobile terminal ticket store is unavailable".to_owned(),
        })?;
    let Some(ticket) = tickets.get(ticket_id).cloned() else {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Attach ticket is invalid or expired".to_owned(),
        });
    };
    validate_mobile_terminal_ticket_secret(
        state,
        &ticket,
        ticket_id,
        ticket_secret,
        device_key_id,
    )?;
    if ticket.expires_at_unix <= now_unix {
        tickets.remove(ticket_id);
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Attach ticket is expired or consumed".to_owned(),
        });
    }
    Ok(ticket)
}

fn validate_mobile_terminal_ticket_secret(
    state: &AppState,
    ticket: &MobileTerminalTicket,
    ticket_id: &str,
    ticket_secret: &str,
    device_key_id: &str,
) -> Result<(), ApiError> {
    if ticket.ticket_id != ticket_id {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Attach ticket is invalid or expired".to_owned(),
        });
    }
    if ticket.device_key_id != device_key_id {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Attach ticket device mismatch".to_owned(),
        });
    }
    let expected = mobile_terminal_secret_hash(&state.mobile_terminal_secret, ticket_secret)?;
    if !constant_time_eq(ticket.secret_hash.as_bytes(), expected.as_bytes()) {
        return Err(ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Attach ticket secret mismatch".to_owned(),
        });
    }
    Ok(())
}

fn enforce_mobile_terminal_active_limits<'a>(
    config: &AppConfig,
    active: impl Iterator<Item = &'a MobileTerminalActiveAttach>,
    ticket: &MobileTerminalTicket,
) -> Result<(), ApiError> {
    let active = active.collect::<Vec<_>>();
    let max_global = clamp_usize(
        config.mobile_terminal.max_concurrent_attaches_global,
        1,
        64,
        4,
    );
    let max_user = clamp_usize(
        config.mobile_terminal.max_concurrent_attaches_per_user,
        1,
        16,
        1,
    );
    let max_session = clamp_usize(
        config.mobile_terminal.max_concurrent_attaches_per_session,
        1,
        16,
        1,
    );
    if active.len() >= max_global {
        return Err(ApiError::Status {
            status: StatusCode::TOO_MANY_REQUESTS,
            detail: "Too many active mobile attaches".to_owned(),
        });
    }
    if active
        .iter()
        .filter(|item| item.user_id == ticket.user_id)
        .count()
        >= max_user
    {
        return Err(ApiError::Status {
            status: StatusCode::TOO_MANY_REQUESTS,
            detail: "Too many active mobile attaches for user".to_owned(),
        });
    }
    if active
        .iter()
        .filter(|item| item.session_id == ticket.session_id)
        .count()
        >= max_session
    {
        return Err(ApiError::Status {
            status: StatusCode::TOO_MANY_REQUESTS,
            detail: "Session already has an active mobile attach".to_owned(),
        });
    }
    Ok(())
}

fn remove_mobile_terminal_active_attach(state: &AppState, attach_id: &str) {
    if let Ok(mut active) = state.mobile_terminal_active_attaches.lock() {
        active.remove(attach_id);
    }
}

fn nonempty_frame_field(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn mobile_terminal_ws_message(
    ticket_id: &str,
    session_id: &str,
    actor_email: &str,
    device_key_id: &str,
    nonce: &str,
) -> String {
    [
        "SM-MOBILE-TERMINAL-WS-V1",
        ticket_id,
        session_id,
        &actor_email.to_ascii_lowercase(),
        device_key_id,
        nonce,
    ]
    .join("\n")
}

fn api_error_detail(error: &ApiError) -> String {
    match error {
        ApiError::Status { detail, .. } => detail.clone(),
        ApiError::NotFound(detail) => (*detail).to_owned(),
        ApiError::Auth { detail, .. } => (*detail).to_owned(),
        ApiError::Internal(error) => error.to_string(),
    }
}

fn clamp_u64(value: u64, minimum: u64, maximum: u64, fallback: u64) -> u64 {
    if value == 0 {
        fallback.clamp(minimum, maximum)
    } else {
        value.clamp(minimum, maximum)
    }
}

fn clamp_usize(value: usize, minimum: usize, maximum: usize, fallback: usize) -> usize {
    if value == 0 {
        fallback.clamp(minimum, maximum)
    } else {
        value.clamp(minimum, maximum)
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn is_static_sessions_path(path: &str) -> bool {
    matches!(
        path,
        "/sessions/create" | "/sessions/input-batch" | "/sessions/spawn" | "/sessions/review"
    )
}

fn is_retained_write_surface(method: &str, path: &str) -> bool {
    if method == "POST" && (path == "/client/request-status" || path == "/client/bug-reports") {
        return true;
    }
    if method == "POST" && path.starts_with("/deploy/") {
        return true;
    }
    if method == "POST" && (path == "/sessions" || path == "/sessions/input-batch") {
        return true;
    }
    if path.starts_with("/sessions/") {
        return matches!(method, "POST" | "PUT" | "PATCH" | "DELETE");
    }
    if method == "POST" && path == "/codex-review-requests" {
        return true;
    }
    if path.starts_with("/codex-review-requests/") {
        return matches!(method, "POST" | "DELETE");
    }
    if method == "POST" && path == "/queue-jobs" {
        return true;
    }
    if path.starts_with("/queue-jobs/") {
        return matches!(method, "POST" | "DELETE");
    }
    false
}

fn is_auth_denial_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 503)
}

fn is_protected_read_surface(method: &str, path: &str) -> bool {
    if method != "GET" {
        return false;
    }
    path == "/events"
        || path == "/events/state"
        || path == "/apk"
        || path == "/client/analytics/summary"
        || path == "/codex-review-requests"
        || path.starts_with("/codex-review-requests/")
        || path == "/queue-jobs"
        || path.starts_with("/queue-jobs/")
        || path == "/nodes"
        || path == "/sessions"
        || path == "/client/sessions"
        || path.starts_with("/apps/")
        || path.starts_with("/sessions/")
        || path.starts_with("/client/sessions/")
}

fn ensure_shadow_allowed(
    config: &AppConfig,
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
) -> Result<(), ApiError> {
    let expected = trimmed(&config.rust_shadow.secret);
    let provided = headers
        .get("x-sm-rust-shadow-secret")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(expected) = expected {
        if provided
            .map(|provided| constant_time_eq(expected.as_bytes(), provided.as_bytes()))
            .unwrap_or(false)
        {
            return Ok(());
        }
        return Err(ApiError::Auth {
            status: StatusCode::FORBIDDEN,
            detail: "Rust shadow endpoint requires local peer or shadow secret",
            login_url: None,
        });
    }

    if peer_addr
        .map(|addr| addr.ip().is_loopback())
        .unwrap_or(false)
    {
        return Ok(());
    }

    Err(ApiError::Auth {
        status: StatusCode::FORBIDDEN,
        detail: "Rust shadow endpoint requires local peer or shadow secret",
        login_url: None,
    })
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0u8, |acc, (left, right)| acc | (left ^ right))
        == 0
}

fn query_bool(query_string: &str, key: &str) -> bool {
    query_value(query_string, key)
        .map(|value| matches!(value, "1" | "true" | "True" | "TRUE" | "yes" | "on"))
        .unwrap_or(false)
}

fn query_usize(query_string: &str, key: &str) -> Option<usize> {
    query_value(query_string, key).and_then(|value| value.parse().ok())
}

fn query_value<'a>(query_string: &'a str, key: &str) -> Option<&'a str> {
    query_string.split('&').find_map(|part| {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        if name == key {
            Some(value)
        } else {
            None
        }
    })
}

fn sha256_hex(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug)]
enum ApiError {
    Internal(anyhow::Error),
    NotFound(&'static str),
    Status {
        status: StatusCode,
        detail: String,
    },
    Auth {
        status: StatusCode,
        detail: &'static str,
        login_url: Option<String>,
    },
}

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self::Internal(error.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::Internal(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "detail": error.to_string() })),
            )
                .into_response(),
            Self::NotFound(detail) => {
                (StatusCode::NOT_FOUND, Json(json!({ "detail": detail }))).into_response()
            }
            Self::Status { status, detail } => {
                (status, Json(json!({ "detail": detail }))).into_response()
            }
            Self::Auth {
                status,
                detail,
                login_url,
            } => {
                let mut body = json!({ "detail": detail });
                if let Some(login_url) = login_url {
                    body["login_url"] = Value::String(login_url);
                }
                (status, Json(body)).into_response()
            }
        }
    }
}

fn ensure_core_writes_enabled(state: &AppState) -> Result<(), ApiError> {
    if state.config.rust_core.fixture_writes_enabled || state.config.rust_core.runtime_enabled {
        return Ok(());
    }
    Err(ApiError::Status {
        status: StatusCode::SERVICE_UNAVAILABLE,
        detail: "Rust core writes are disabled".to_owned(),
    })
}

fn ensure_core_runtime_provider_supported(
    payload: &CreateCoreSessionRequest,
) -> Result<(), ApiError> {
    let provider = payload
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("claude");
    if matches!(provider, "claude" | "codex-fork") {
        return Ok(());
    }
    Err(ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: format!("Rust runtime does not support provider {provider}"),
    })
}

fn ensure_core_runtime_request_node_supported(
    state: &AppState,
    payload: &CreateCoreSessionRequest,
) -> Result<(), ApiError> {
    if let Some(node) = payload
        .node
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return ensure_core_runtime_node_supported(node);
    }
    if let Some(parent_session_id) = payload
        .parent_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if let Some(parent) = state.session_store.get_session(parent_session_id)? {
            return ensure_core_runtime_node_supported(&parent.node);
        }
    }
    ensure_core_runtime_node_supported("primary")
}

fn ensure_core_runtime_session_node_supported(
    state: &AppState,
    session_id: &str,
) -> Result<(), ApiError> {
    let Some(session) = state.session_store.get_session(session_id)? else {
        return Ok(());
    };
    ensure_core_runtime_node_supported(&session.node)
}

fn ensure_core_runtime_node_supported(node: &str) -> Result<(), ApiError> {
    if is_primary_node(node) {
        return Ok(());
    }
    Err(ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: format!("Rust runtime does not support remote node {node}"),
    })
}

fn ensure_session_read_allowed(state: &AppState, request: &Request) -> Result<(), ApiError> {
    let peer_addr = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|value| value.0);
    ensure_session_allowed_from_parts(
        &state.config,
        request.headers(),
        peer_addr,
        request.uri().path(),
    )
}

fn ensure_session_allowed_from_parts(
    config: &AppConfig,
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
    path: &str,
) -> Result<(), ApiError> {
    let auth = &config.google_auth;
    if !auth.requested() {
        return Ok(());
    }
    if is_local_bypass_request(headers, peer_addr, config) {
        return Ok(());
    }
    if !auth.ready() {
        return Err(ApiError::Auth {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "Google auth is enabled but incomplete",
            login_url: None,
        });
    }
    if authenticated_user(headers, config).is_some() {
        return Ok(());
    }
    Err(ApiError::Auth {
        status: StatusCode::UNAUTHORIZED,
        detail: "Authentication required",
        login_url: Some(google_login_redirect(path)),
    })
}

#[derive(Debug)]
struct AuthenticatedUser {
    email: String,
    name: Option<String>,
    auth_type: &'static str,
}

#[derive(Debug, Deserialize)]
struct DeviceAccessPayload {
    #[serde(rename = "type")]
    token_type: String,
    email: String,
    #[serde(default)]
    name: Option<String>,
    exp: i64,
}

fn authenticated_user(headers: &HeaderMap, config: &AppConfig) -> Option<AuthenticatedUser> {
    if let Some(user) = device_auth_user(headers, config) {
        return Some(user);
    }
    browser_session_user(headers, config)
}

fn request_actor_email_from_parts(
    config: &AppConfig,
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
) -> Option<String> {
    if let Some(user) = authenticated_user(headers, config) {
        return Some(user.email.trim().to_ascii_lowercase());
    }
    if is_local_bypass_request(headers, peer_addr, config) {
        return Some("local_bypass".to_owned());
    }
    None
}

fn request_actor_email(config: &AppConfig, request: &Request) -> Option<String> {
    let peer_addr = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|value| value.0);
    request_actor_email_from_parts(config, request.headers(), peer_addr)
}

fn validate_json_payload_size(
    field_name: &str,
    payload: Option<Value>,
    max_chars: usize,
) -> Result<Option<Value>, ApiError> {
    let Some(payload) = payload else {
        return Ok(None);
    };
    let raw = serde_json::to_string(&payload).map_err(|_| ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: format!("{field_name} must be JSON serializable"),
    })?;
    if raw.chars().count() > max_chars {
        return Err(ApiError::Status {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            detail: format!("{field_name} exceeds {max_chars} serialized characters"),
        });
    }
    Ok(Some(payload))
}

fn bug_report_server_state(
    state: &AppState,
    selected_session_id: Option<&str>,
) -> Result<Value, ApiError> {
    let sessions = state
        .session_store
        .list_sessions(false)?
        .into_iter()
        .map(SessionResponse::from)
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    let selected_session = if let Some(session_id) = selected_session_id {
        match state.session_store.get_session(session_id)? {
            Some(session) => json!({
                "found": true,
                "session": ClientSessionResponse::from(session),
            }),
            None => json!({
                "id": session_id,
                "found": false,
            }),
        }
    } else {
        Value::Null
    };
    Ok(json!({
        "captured_at": now_rfc3339(),
        "bootstrap": client_bootstrap_response(
            &state.config,
            mobile_terminal_runtime_disabled(state),
        ),
        "health": {
            "status": "healthy",
        },
        "sessions": sessions,
        "selected_session": selected_session,
    }))
}

fn device_auth_user(headers: &HeaderMap, config: &AppConfig) -> Option<AuthenticatedUser> {
    let token = request_bearer_token(headers)?;
    let secret = trimmed(&config.google_auth.session_cookie_secret)?;
    let raw = token.strip_prefix("smat_")?;
    let (payload_b64, signature_b64) = raw.split_once('.')?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(payload_b64.as_bytes());
    let signature = URL_SAFE_NO_PAD.decode(signature_b64).ok()?;
    mac.verify_slice(&signature).ok()?;

    let payload_bytes = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let payload: DeviceAccessPayload = serde_json::from_slice(&payload_bytes).ok()?;
    if payload.token_type != "device_access"
        || payload.exp <= OffsetDateTime::now_utc().unix_timestamp()
    {
        return None;
    }
    let email = payload.email.trim().to_lowercase();
    if email.is_empty() {
        return None;
    }
    Some(AuthenticatedUser {
        email,
        name: payload.name,
        auth_type: "device_bearer",
    })
}

fn request_bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

fn browser_session_user(headers: &HeaderMap, config: &AppConfig) -> Option<AuthenticatedUser> {
    let cookie = cookie_value(headers, SESSION_COOKIE_NAME)?;
    let secret = trimmed(&config.google_auth.session_cookie_secret)?;
    let payload = verify_starlette_session_cookie(&cookie, &secret)?;
    if payload.get("google_authenticated")?.as_bool()? != true {
        return None;
    }
    Some(AuthenticatedUser {
        email: payload
            .get("google_email")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        name: payload
            .get("google_name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        auth_type: "browser_session",
    })
}

fn verify_starlette_session_cookie(cookie: &str, secret: &str) -> Option<Value> {
    let (value, signature_b64) = cookie.rsplit_once('.')?;
    let (payload_b64, timestamp_b64) = value.rsplit_once('.')?;
    let derived_key = itsdangerous_django_concat_key(secret);
    let mut mac = Hmac::<Sha1>::new_from_slice(&derived_key).ok()?;
    mac.update(value.as_bytes());
    let signature = URL_SAFE_NO_PAD.decode(signature_b64).ok()?;
    mac.verify_slice(&signature).ok()?;

    let timestamp = decode_itsdangerous_timestamp(timestamp_b64)?;
    let age = OffsetDateTime::now_utc().unix_timestamp() - timestamp;
    if !(0..=SESSION_COOKIE_MAX_AGE_SECONDS).contains(&age) {
        return None;
    }
    let payload = STANDARD.decode(payload_b64).ok()?;
    serde_json::from_slice(&payload).ok()
}

fn itsdangerous_django_concat_key(secret: &str) -> Vec<u8> {
    let mut hasher = Sha1::new();
    hasher.update(b"itsdangerous.Signersigner");
    hasher.update(secret.as_bytes());
    hasher.finalize().to_vec()
}

fn decode_itsdangerous_timestamp(value: &str) -> Option<i64> {
    let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
    if bytes.len() > 8 {
        return None;
    }
    let mut padded = [0_u8; 8];
    let start = 8 - bytes.len();
    padded[start..].copy_from_slice(&bytes);
    Some(u64::from_be_bytes(padded) as i64)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for header in headers.get_all(COOKIE) {
        let Ok(value) = header.to_str() else {
            continue;
        };
        for part in value.split(';') {
            let Some((cookie_name, cookie_value)) = part.trim().split_once('=') else {
                continue;
            };
            if cookie_name.trim() == name {
                let cookie_value = cookie_value.trim();
                if !cookie_value.is_empty() {
                    return Some(cookie_value.to_owned());
                }
            }
        }
    }
    None
}

fn google_login_redirect(path: &str) -> String {
    format!("/auth/google/login?next={}", percent_encode_path(path))
}

fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::new();
    for byte in path.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn percent_decode_query_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = hex_value(bytes[index + 1])?;
                let low = hex_value(bytes[index + 2])?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            b'%' => return None,
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_local_bypass_request(
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
    config: &AppConfig,
) -> bool {
    let Some(peer_addr) = peer_addr else {
        return false;
    };
    if !peer_addr.ip().is_loopback() {
        return false;
    }
    let Some(hostname) = request_hostname(headers) else {
        return false;
    };
    if let Some(public_host) = trimmed(&config.google_auth.public_host) {
        if hostname.eq_ignore_ascii_case(&public_host) {
            return false;
        }
    }
    matches!(
        hostname.as_str(),
        "127.0.0.1" | "localhost" | "::1" | "testserver"
    )
}

fn request_hostname(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(HOST)?.to_str().ok()?.trim().to_lowercase();
    if host.starts_with('[') {
        return host
            .split(']')
            .next()
            .map(|value| value.trim_start_matches('[').to_owned());
    }
    Some(host.split(':').next().unwrap_or("").to_owned())
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[derive(Serialize)]
struct HealthDetailedResponse {
    status: &'static str,
    checks: BTreeMap<String, HealthCheck>,
    resources: BTreeMap<String, Value>,
    timestamp: String,
}

#[derive(Serialize)]
struct HealthCheck {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListSessionsQuery {
    #[serde(default)]
    include_stopped: bool,
}

#[derive(Debug, Deserialize)]
struct ListChildrenQuery {
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    include_terminated: bool,
}

#[derive(Debug, Deserialize)]
struct ListCodexReviewRequestsQuery {
    #[serde(default)]
    notify_target: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    pr_number: Option<i64>,
    #[serde(default)]
    include_inactive: bool,
}

#[derive(Debug, Deserialize)]
struct ListQueueJobsQuery {
    #[serde(default)]
    notify_target: Option<String>,
    #[serde(default, rename = "type")]
    job_type: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    include_terminal: bool,
}

#[derive(Debug, Deserialize)]
struct SessionOutputQuery {
    lines: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SessionToolCallsQuery {
    #[serde(default = "default_tool_calls_limit")]
    limit: i64,
}

impl SessionToolCallsQuery {
    fn validated_limit(&self) -> Option<usize> {
        if (1..=100).contains(&self.limit) {
            Some(self.limit as usize)
        } else {
            None
        }
    }
}

fn default_tool_calls_limit() -> i64 {
    10
}

#[derive(Debug)]
struct SessionCodexEventsQuery {
    since_seq: Option<i64>,
    limit: usize,
}

impl SessionCodexEventsQuery {
    fn parse(query_string: &str) -> Result<Self, ApiError> {
        let since_seq_value = decoded_last_query_value(query_string, "since_seq")?;
        let limit_value = decoded_last_query_value(query_string, "limit")?;
        let since_seq = match since_seq_value.as_deref() {
            None => None,
            Some(value) => {
                let parsed = value.parse::<i64>().map_err(|_| ApiError::Status {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    detail: "since_seq must be >= 0 and less than 9223372036854775807".to_owned(),
                })?;
                if !(0..i64::MAX).contains(&parsed) {
                    return Err(ApiError::Status {
                        status: StatusCode::UNPROCESSABLE_ENTITY,
                        detail: "since_seq must be >= 0 and less than 9223372036854775807"
                            .to_owned(),
                    });
                }
                Some(parsed)
            }
        };
        let limit = match limit_value.as_deref() {
            None => 200,
            Some(value) => {
                let parsed = value.parse::<i64>().map_err(|_| ApiError::Status {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    detail: "limit must be between 1 and 500".to_owned(),
                })?;
                if !(1..=500).contains(&parsed) {
                    return Err(ApiError::Status {
                        status: StatusCode::UNPROCESSABLE_ENTITY,
                        detail: "limit must be between 1 and 500".to_owned(),
                    });
                }
                parsed as usize
            }
        };
        Ok(Self { since_seq, limit })
    }
}

struct SessionCodexPendingRequestsQuery {
    include_orphaned: bool,
}

impl SessionCodexPendingRequestsQuery {
    fn parse(query_string: &str) -> Result<Self, ApiError> {
        let include_orphaned =
            match decoded_last_query_value(query_string, "include_orphaned")?.as_deref() {
                None => false,
                Some(value) => parse_python_bool_query(value).ok_or_else(|| ApiError::Status {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    detail: "include_orphaned must be a boolean".to_owned(),
                })?,
            };
        Ok(Self { include_orphaned })
    }
}

fn parse_python_bool_query(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" | "t" | "y" => Some(true),
        "false" | "0" | "off" | "no" | "f" | "n" => Some(false),
        _ => None,
    }
}

fn decoded_last_query_value(query_string: &str, key: &str) -> Result<Option<String>, ApiError> {
    let mut value = None;
    for part in query_string.split('&') {
        let (raw_name, raw_value) = part.split_once('=').unwrap_or((part, ""));
        let Some(name) = percent_decode_query_component(raw_name) else {
            return Err(ApiError::Status {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                detail: "invalid query encoding".to_owned(),
            });
        };
        if name == key {
            let Some(decoded_value) = percent_decode_query_component(raw_value) else {
                return Err(ApiError::Status {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    detail: "invalid query encoding".to_owned(),
                });
            };
            value = Some(decoded_value);
        }
    }
    Ok(value)
}

#[derive(Debug, Deserialize)]
struct KillSessionRequest {
    #[serde(default)]
    requester_session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ClientRequestStatusResponse {
    status: &'static str,
    prompt: &'static str,
    targeted_count: usize,
    delivered_count: usize,
    queued_count: usize,
    failed_count: usize,
    targeted_session_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ClientBugReportRequest {
    report_text: String,
    #[serde(default = "default_true")]
    include_debug_state: bool,
    #[serde(default)]
    selected_session_id: Option<String>,
    #[serde(default)]
    client_state: Option<Value>,
    #[serde(default)]
    app_version: Option<String>,
    #[serde(default)]
    artifact_hash: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct ClientBugReportResponse {
    status: &'static str,
    bug_id: String,
    maintainer_notified: bool,
}

#[derive(Debug, Deserialize)]
struct SendEmailRequest {
    #[serde(default)]
    requester_session_id: Option<String>,
    recipients: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body_text: Option<String>,
    #[serde(default)]
    body_html: Option<String>,
    #[serde(default)]
    body_markdown: bool,
    #[serde(default)]
    auto_subject: bool,
}

#[derive(Debug, Deserialize)]
struct HumanDeliveryRequest {
    #[serde(default)]
    requester_session_id: Option<String>,
    text: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body_markdown: bool,
    #[serde(default = "default_true")]
    auto_subject: bool,
}

#[derive(Debug, Deserialize)]
struct InboundEmailRequest {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    raw_email: Option<String>,
    from_address: String,
}

#[derive(Debug, Serialize)]
struct AppArtifactDeployResponse {
    ok: bool,
    app: String,
    size_bytes: u64,
    download_url: String,
    artifact_hash: String,
}

#[derive(Debug, Deserialize)]
struct SpawnCoreSessionRequest {
    #[serde(default)]
    id: Option<String>,
    parent_session_id: String,
    prompt: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    wait: Option<u64>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    track_seconds: Option<u64>,
}

#[derive(Debug, Serialize)]
struct SpawnSessionResponse {
    session_id: String,
    name: String,
    friendly_name: String,
    working_dir: String,
    parent_session_id: Option<String>,
    tmux_session: String,
    node: String,
    provider: String,
    model: Option<String>,
    created_at: String,
}

impl From<SessionRecord> for SpawnSessionResponse {
    fn from(session: SessionRecord) -> Self {
        let friendly_name = session
            .friendly_name
            .as_ref()
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| session.name.clone());
        Self {
            session_id: session.id,
            name: session.name,
            friendly_name,
            working_dir: session.working_dir,
            parent_session_id: session.parent_session_id,
            tmux_session: session.tmux_session,
            node: if session.node.trim().is_empty() {
                "primary".to_owned()
            } else {
                session.node
            },
            provider: if session.provider.trim().is_empty() {
                "claude".to_owned()
            } else {
                session.provider
            },
            model: session.model,
            created_at: session.created_at,
        }
    }
}

#[derive(Serialize)]
struct SessionOutputResponse {
    session_id: String,
    output: Option<String>,
}

#[derive(Serialize)]
struct ToolCallsResponse {
    session_id: String,
    tool_calls: Vec<ToolCallRow>,
}

#[derive(Serialize)]
struct NodesListResponse {
    default: String,
    nodes: Vec<PublicNodeConfig>,
}

fn nodes_list_response(config: &AppConfig) -> NodesListResponse {
    NodesListResponse {
        default: config.nodes.default_node.clone(),
        nodes: config.nodes.redacted_nodes(),
    }
}

fn codex_review_request_response(
    state: &AppState,
    registration: CodexReviewRequestRegistration,
) -> Result<Value, ApiError> {
    let requester_name = match registration.requester_session_id.as_deref() {
        Some(session_id) => state
            .session_store
            .get_session(session_id)?
            .map(session_display_name)
            .or_else(|| Some(session_id.to_owned())),
        None => None,
    };
    let notify_name = state
        .session_store
        .get_session(&registration.notify_session_id)?
        .map(session_display_name)
        .unwrap_or_else(|| registration.notify_session_id.clone());
    Ok(json!({
        "id": registration.id,
        "repo": registration.repo,
        "pr_number": registration.pr_number,
        "requester_session_id": registration.requester_session_id,
        "requester_name": requester_name,
        "notify_session_id": registration.notify_session_id,
        "notify_name": notify_name,
        "steer": registration.steer,
        "requested_at": registration.requested_at,
        "latest_request_comment_id": registration.latest_request_comment_id,
        "latest_request_comment_url": registration.latest_request_comment_url,
        "latest_request_posted_at": registration.latest_request_posted_at,
        "attempt_count": registration.attempt_count,
        "next_retry_at": registration.next_retry_at,
        "poll_interval_seconds": registration.poll_interval_seconds,
        "retry_interval_seconds": registration.retry_interval_seconds,
        "pickup_detected_at": registration.pickup_detected_at,
        "pickup_source": registration.pickup_source,
        "review_landed_at": registration.review_landed_at,
        "review_source": registration.review_source,
        "review_comment_id": registration.review_comment_id,
        "review_url": registration.review_url,
        "last_polled_at": registration.last_polled_at,
        "last_error": registration.last_error,
        "state": registration.state,
        "is_active": registration.is_active,
    }))
}

fn queue_job_response(state: &AppState, job: QueueJobRecord) -> Result<Value, ApiError> {
    let requester_name = match job.requester_session_id.as_deref() {
        Some(session_id) => state
            .session_store
            .get_session(session_id)?
            .map(session_display_name),
        None => None,
    };
    let notify_name = match job.notify_session_id.as_deref() {
        Some(session_id) => state
            .session_store
            .get_session(session_id)?
            .map(session_display_name)
            .or_else(|| Some(session_id.to_owned())),
        None => None,
    };
    Ok(json!({
        "id": job.id,
        "type": job.job_type,
        "label": job.label,
        "requester_session_id": job.requester_session_id,
        "requester_name": requester_name,
        "notify_session_id": job.notify_session_id,
        "notify_name": notify_name,
        "cwd": job.cwd,
        "argv": job.argv,
        "script_path": job.script_path,
        "timeout_seconds": job.timeout_seconds,
        "state": job.state,
        "holding_reason": job.holding_reason,
        "queued_at": job.queued_at,
        "started_at": job.started_at,
        "finished_at": job.finished_at,
        "pid": job.pid,
        "process_group_id": job.process_group_id,
        "exit_code": job.exit_code,
        "log_path": job.log_path,
    }))
}

fn session_display_name(session: SessionRecord) -> String {
    session.cached_display_name().unwrap_or(session.id)
}

#[derive(Serialize)]
struct EventStateResponse {
    tmux_client_event_version: i64,
    last_tmux_client_event: Option<Value>,
}

fn event_state_payload() -> EventStateResponse {
    EventStateResponse {
        tmux_client_event_version: 0,
        last_tmux_client_event: None,
    }
}

#[derive(Serialize)]
struct AuthSessionResponse {
    enabled: bool,
    authenticated: bool,
    bypass: bool,
    email: Option<String>,
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

impl AuthSessionResponse {
    fn disabled_bypass() -> Self {
        Self {
            enabled: false,
            authenticated: true,
            bypass: true,
            email: None,
            name: None,
            auth_type: None,
            error: None,
        }
    }

    fn enabled_bypass() -> Self {
        Self {
            enabled: true,
            authenticated: true,
            bypass: true,
            email: None,
            name: None,
            auth_type: None,
            error: None,
        }
    }

    fn misconfigured() -> Self {
        Self {
            enabled: true,
            authenticated: false,
            bypass: false,
            email: None,
            name: None,
            auth_type: None,
            error: Some("misconfigured"),
        }
    }
}

#[derive(Serialize)]
struct ClientBootstrapResponse {
    auth: BootstrapAuth,
    external_access: BootstrapExternalAccess,
    session_open_defaults: SessionOpenDefaults,
}

#[derive(Serialize)]
struct BootstrapAuth {
    #[serde(rename = "mode")]
    mode_name: &'static str,
    session_endpoint: &'static str,
    login_endpoint: &'static str,
    logout_endpoint: &'static str,
    device_auth_endpoint: &'static str,
    device_auth_token_type: &'static str,
    google_server_client_id: Option<String>,
}

#[derive(Serialize)]
struct BootstrapExternalAccess {
    public_http_host: Option<String>,
    public_ssh_host: Option<String>,
    ssh_username: Option<String>,
    termux_attach_supported: bool,
    mobile_terminal_supported: bool,
    mobile_terminal_ws_url: Option<String>,
}

#[derive(Serialize)]
struct SessionOpenDefaults {
    preferred_action: &'static str,
    termux_package: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Method,
    };
    use p256::{
        ecdsa::{signature::Signer, SigningKey},
        pkcs8::{EncodePublicKey, LineEnding},
    };
    use std::{env, process};
    use tower::ServiceExt;

    fn write_session_state(session_id: &str, status: &str) -> String {
        let dir = env::temp_dir().join(format!(
            "sm-rust-mobile-ticket-{}-{}",
            process::id(),
            random_urlsafe_token(8)
        ));
        fs::create_dir_all(&dir).unwrap();
        let state_file = dir.join("sessions.json");
        fs::write(
            &state_file,
            serde_json::to_string(&json!({
                "sessions": [{
                    "id": session_id,
                    "name": format!("codex-fork-{session_id}"),
                    "working_dir": "/repo",
                    "tmux_session": format!("codex-fork-{session_id}"),
                    "provider": "codex-fork",
                    "status": status,
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        state_file.display().to_string()
    }

    fn mobile_ticket_config(signing_key: &SigningKey) -> AppConfig {
        let public_key = signing_key
            .verifying_key()
            .to_public_key_pem(LineEnding::LF)
            .unwrap();
        let mut config = AppConfig::default();
        config.paths.state_file = write_session_state("fork1001", "running");
        config.mobile_terminal.enabled = true;
        config.mobile_terminal.ws_url = Some("wss://sm.rajeshgo.li/client/terminal".to_owned());
        config.mobile_terminal.allowed_users.insert(
            "local_bypass".to_owned(),
            MobileTerminalUserConfig {
                interactive_shell_access: true,
                registered_device_keys: vec![MobileTerminalDeviceKeyConfig {
                    id: "test-device".to_owned(),
                    public_key,
                    enabled: true,
                }],
                ..MobileTerminalUserConfig::default()
            },
        );
        config
    }

    fn local_request(method: Method, uri: &str, body: Body) -> axum::http::Request<Body> {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(HOST, "testserver")
            .body(body)
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4200))));
        request
    }

    fn public_request(method: Method, uri: &str, body: Body) -> axum::http::Request<Body> {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(HOST, "sm.rajeshgo.li")
            .body(body)
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 10], 4200))));
        request
    }

    fn public_edge_config() -> AppConfig {
        let mut config = AppConfig::default();
        config.public_edge.enabled = true;
        config.public_edge.assertion_secret = Some("edge-secret".to_owned());
        config
    }

    fn sign_public_edge_headers(
        secret: &str,
        method: &str,
        path: &str,
        timestamp: &str,
        nonce: &str,
    ) -> [(String, String); 3] {
        let message = public_edge_assertion_message(method, path, timestamp, nonce);
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(message.as_bytes());
        [
            ("x-sm-edge-timestamp".to_owned(), timestamp.to_owned()),
            ("x-sm-edge-nonce".to_owned(), nonce.to_owned()),
            (
                "x-sm-edge-signature".to_owned(),
                STANDARD.encode(mac.finalize().into_bytes()),
            ),
        ]
    }

    fn add_public_edge_headers(
        request: &mut axum::http::Request<Body>,
        secret: &str,
        method: &str,
        path: &str,
        nonce: &str,
    ) {
        let timestamp = OffsetDateTime::now_utc().unix_timestamp().to_string();
        for (name, value) in sign_public_edge_headers(secret, method, path, &timestamp, nonce) {
            request.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(&value).unwrap(),
            );
        }
    }

    fn sign_ticket_headers(
        signing_key: &SigningKey,
        session_id: &str,
        path: &str,
        timestamp: &str,
        nonce: &str,
    ) -> [(String, String); 4] {
        let message = mobile_terminal_ticket_message(
            "POST",
            path,
            session_id,
            "local_bypass",
            "test-device",
            timestamp,
            nonce,
        );
        let signature: Signature = signing_key.sign(message.as_bytes());
        [
            ("x-sm-device-key-id".to_owned(), "test-device".to_owned()),
            ("x-sm-device-timestamp".to_owned(), timestamp.to_owned()),
            ("x-sm-device-nonce".to_owned(), nonce.to_owned()),
            (
                "x-sm-device-signature".to_owned(),
                STANDARD.encode(signature.to_der().as_bytes()),
            ),
        ]
    }

    fn attach_ticket_request(signing_key: &SigningKey, nonce: &str) -> axum::http::Request<Body> {
        let timestamp = OffsetDateTime::now_utc().unix_timestamp().to_string();
        let mut request = local_request(
            Method::POST,
            "/client/sessions/fork1001/attach-ticket",
            Body::from("{}"),
        );
        for (name, value) in sign_ticket_headers(
            signing_key,
            "fork1001",
            "/client/sessions/fork1001/attach-ticket",
            &timestamp,
            nonce,
        ) {
            request.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(&value).unwrap(),
            );
        }
        request
    }

    async fn response_json(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    async fn mint_mobile_attach_ticket(
        state: &AppState,
        signing_key: &SigningKey,
        nonce: &str,
    ) -> Value {
        let response = router(state.clone())
            .oneshot(attach_ticket_request(signing_key, nonce))
            .await
            .unwrap();
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    fn signed_mobile_terminal_auth_frame(
        signing_key: &SigningKey,
        ticket: &Value,
        nonce: &str,
    ) -> MobileTerminalAuthFrame {
        let ticket_id = ticket["ticket_id"].as_str().unwrap();
        let message =
            mobile_terminal_ws_message(ticket_id, "fork1001", "local_bypass", "test-device", nonce);
        let signature: Signature = signing_key.sign(message.as_bytes());
        MobileTerminalAuthFrame {
            frame_type: Some("auth".to_owned()),
            ticket_id: Some(ticket_id.to_owned()),
            ticket_secret: Some(ticket["ticket_secret"].as_str().unwrap().to_owned()),
            device_key_id: Some("test-device".to_owned()),
            nonce: Some(nonce.to_owned()),
            signature: Some(STANDARD.encode(signature.to_der().as_bytes())),
        }
    }

    fn api_error_status_detail(error: ApiError) -> (StatusCode, String) {
        match error {
            ApiError::Status { status, detail } => (status, detail),
            ApiError::NotFound(detail) => (StatusCode::NOT_FOUND, detail.to_owned()),
            ApiError::Auth { status, detail, .. } => (status, detail.to_owned()),
            ApiError::Internal(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        }
    }

    fn test_mobile_terminal_ticket() -> MobileTerminalTicket {
        MobileTerminalTicket {
            ticket_id: "att_test".to_owned(),
            secret_hash: "secret-hash".to_owned(),
            user_id: "local_bypass".to_owned(),
            actor_email: "local_bypass".to_owned(),
            session_id: "fork1001".to_owned(),
            provider: "codex-fork".to_owned(),
            node: "primary".to_owned(),
            tmux_session: "codex-fork-fork1001".to_owned(),
            tmux_socket_name: Some("sm-test".to_owned()),
            device_key_id: "test-device".to_owned(),
            created_at_unix: 1,
            expires_at_unix: 999_999,
        }
    }

    #[test]
    fn mobile_terminal_key_bytes_use_terminal_control_sequences() {
        assert_eq!(mobile_terminal_key_bytes("enter").unwrap(), b"\r");
        assert_eq!(mobile_terminal_key_bytes("esc").unwrap(), b"\x1b");
        assert_eq!(mobile_terminal_key_bytes("escape").unwrap(), b"\x1b");
        assert_eq!(mobile_terminal_key_bytes("tab").unwrap(), b"\t");
        assert_eq!(mobile_terminal_key_bytes("backspace").unwrap(), b"\x7f");
        assert_eq!(mobile_terminal_key_bytes("ctrl-c").unwrap(), b"\x03");
        assert_eq!(mobile_terminal_key_bytes("ctrl-d").unwrap(), b"\x04");
        assert_eq!(mobile_terminal_key_bytes("ctrl-z").unwrap(), b"\x1a");
        assert_eq!(mobile_terminal_key_bytes("ctrl-b").unwrap(), b"\x02");
        assert!(mobile_terminal_key_bytes("unsupported").is_none());
    }

    #[test]
    fn mobile_terminal_resize_validates_python_bounds() {
        assert_eq!(
            mobile_terminal_resize(&json!({"type": "resize", "rows": 2, "cols": 10})),
            Some((2, 10))
        );
        assert_eq!(
            mobile_terminal_resize(&json!({"type": "resize", "rows": 120, "cols": 300})),
            Some((120, 300))
        );
        assert!(
            mobile_terminal_resize(&json!({"type": "resize", "rows": 1, "cols": 80})).is_none()
        );
        assert!(
            mobile_terminal_resize(&json!({"type": "resize", "rows": 24, "cols": 9})).is_none()
        );
        assert!(
            mobile_terminal_resize(&json!({"type": "resize", "rows": 121, "cols": 80})).is_none()
        );
        assert!(
            mobile_terminal_resize(&json!({"type": "resize", "rows": 24, "cols": 301})).is_none()
        );
    }

    #[test]
    fn mobile_terminal_tmux_argv_includes_socket_and_attach_target() {
        let ticket = test_mobile_terminal_ticket();
        assert_eq!(
            mobile_terminal_tmux_argv(&ticket, &["attach-session", "-t", &ticket.tmux_session]),
            vec![
                "tmux",
                "-L",
                "sm-test",
                "attach-session",
                "-t",
                "codex-fork-fork1001"
            ]
        );
    }

    #[test]
    fn mobile_terminal_scrollback_normalizes_lf_rows_to_crlf() {
        assert_eq!(
            normalize_mobile_terminal_scrollback(b"older line\nold line\n"),
            b"older line\r\nold line\r\n"
        );
        assert_eq!(
            normalize_mobile_terminal_scrollback(b"already\r\nnormalized"),
            b"already\r\nnormalized\r\n"
        );
    }

    #[tokio::test]
    async fn mobile_attach_ticket_requires_registered_device_signature() {
        let signing_key = SigningKey::random(&mut OsRng);
        let state = AppState::new(mobile_ticket_config(&signing_key));
        let app = router(state);

        let missing = app
            .clone()
            .oneshot(local_request(
                Method::POST,
                "/client/sessions/fork1001/attach-ticket",
                Body::from("{}"),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(missing).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["detail"], "Device key proof is required");

        let valid = app
            .oneshot(attach_ticket_request(&signing_key, "nonce-1"))
            .await
            .unwrap();
        let (status, body) = response_json(valid).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["ticket_id"].as_str().unwrap().starts_with("att_"));
        assert!(body["ticket_secret"].as_str().unwrap().len() >= 40);
        assert_eq!(body["device_key_id"], "test-device");
        assert_eq!(body["ws_url"], "wss://sm.rajeshgo.li/client/terminal");
        assert!(!body["ws_url"]
            .as_str()
            .unwrap()
            .contains(body["ticket_secret"].as_str().unwrap()));
    }

    #[tokio::test]
    async fn public_edge_assertion_is_disabled_by_default() {
        let app = router(AppState::new(AppConfig::default()));

        let response = app
            .oneshot(public_request(
                Method::GET,
                "/client/bootstrap",
                Body::empty(),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["auth"]["session_endpoint"], "/auth/session");
    }

    #[tokio::test]
    async fn public_edge_assertion_allows_local_bypass_without_headers() {
        let app = router(AppState::new(public_edge_config()));

        let response = app
            .oneshot(local_request(
                Method::GET,
                "/client/bootstrap",
                Body::empty(),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["auth"]["mode"], "browser_session_cookie");
    }

    #[tokio::test]
    async fn public_edge_assertion_rejects_public_native_routes_without_headers() {
        let signing_key = SigningKey::random(&mut OsRng);
        let mut config = mobile_ticket_config(&signing_key);
        config.public_edge = public_edge_config().public_edge;
        let app = router(AppState::new(config));

        for request in [
            public_request(Method::GET, "/client/bootstrap", Body::empty()),
            public_request(Method::GET, "/client/sessions", Body::empty()),
            public_request(
                Method::POST,
                "/client/sessions/fork1001/attach-ticket",
                Body::from("{}"),
            ),
        ] {
            let response = app.clone().oneshot(request).await.unwrap();
            let (status, body) = response_json(response).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body["detail"], "Public edge assertion is required");
        }
    }

    #[tokio::test]
    async fn public_edge_assertion_accepts_signed_bootstrap_and_rejects_replay() {
        let app = router(AppState::new(public_edge_config()));
        let mut first = public_request(Method::GET, "/client/bootstrap", Body::empty());
        add_public_edge_headers(
            &mut first,
            "edge-secret",
            "GET",
            "/client/bootstrap",
            "edge-nonce-1",
        );
        let mut replay = public_request(Method::GET, "/client/bootstrap", Body::empty());
        add_public_edge_headers(
            &mut replay,
            "edge-secret",
            "GET",
            "/client/bootstrap",
            "edge-nonce-1",
        );

        let first_response = app.clone().oneshot(first).await.unwrap();
        let (first_status, first_body) = response_json(first_response).await;
        assert_eq!(first_status, StatusCode::OK);
        assert_eq!(
            first_body["auth"]["device_auth_endpoint"],
            "/auth/device/google"
        );

        let replay_response = app.oneshot(replay).await.unwrap();
        let (replay_status, replay_body) = response_json(replay_response).await;
        assert_eq!(replay_status, StatusCode::FORBIDDEN);
        assert_eq!(
            replay_body["detail"],
            "Public edge assertion nonce was already used"
        );
    }

    #[tokio::test]
    async fn public_edge_assertion_rejects_invalid_signature() {
        let app = router(AppState::new(public_edge_config()));
        let mut request = public_request(Method::GET, "/client/bootstrap", Body::empty());
        add_public_edge_headers(
            &mut request,
            "wrong-secret",
            "GET",
            "/client/bootstrap",
            "edge-nonce-1",
        );

        let response = app.oneshot(request).await.unwrap();
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["detail"], "Invalid public edge assertion");
    }

    #[tokio::test]
    async fn public_edge_assertion_rejects_expired_timestamp() {
        let app = router(AppState::new(public_edge_config()));
        let mut request = public_request(Method::GET, "/client/bootstrap", Body::empty());
        let expired_timestamp = (OffsetDateTime::now_utc().unix_timestamp() - 3600).to_string();
        for (name, value) in sign_public_edge_headers(
            "edge-secret",
            "GET",
            "/client/bootstrap",
            &expired_timestamp,
            "edge-nonce-1",
        ) {
            request.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(&value).unwrap(),
            );
        }

        let response = app.oneshot(request).await.unwrap();
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["detail"], "Expired public edge assertion");
    }

    #[tokio::test]
    async fn public_edge_assertion_fails_closed_when_enabled_without_secret() {
        let mut config = public_edge_config();
        config.public_edge.assertion_secret = None;
        let app = router(AppState::new(config));

        let response = app
            .oneshot(public_request(
                Method::GET,
                "/client/bootstrap",
                Body::empty(),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body["detail"],
            "Public edge assertion is enabled but incomplete"
        );
    }

    #[tokio::test]
    async fn public_edge_assertion_binds_post_request_status_query_target() {
        let app = router(AppState::new(public_edge_config()));
        let target = "/client/request-status?source=mobile";
        let mut signed_full_target = public_request(Method::POST, target, Body::empty());
        add_public_edge_headers(
            &mut signed_full_target,
            "edge-secret",
            "POST",
            target,
            "edge-nonce-full-target",
        );

        let response = app.clone().oneshot(signed_full_target).await.unwrap();
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["detail"], "Rust core writes are disabled");

        let mut signed_bare_path = public_request(Method::POST, target, Body::empty());
        add_public_edge_headers(
            &mut signed_bare_path,
            "edge-secret",
            "POST",
            "/client/request-status",
            "edge-nonce-bare-path",
        );

        let response = app.oneshot(signed_bare_path).await.unwrap();
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["detail"], "Invalid public edge assertion");
    }

    #[tokio::test]
    async fn public_edge_assertion_binds_post_bug_report_query_target() {
        let app = router(AppState::new(public_edge_config()));
        let target = "/client/bug-reports?source=mobile";
        let mut signed_full_target =
            public_request(Method::POST, target, Body::from(r#"{"report_text":"   "}"#));
        signed_full_target
            .headers_mut()
            .insert(CONTENT_TYPE, "application/json".parse().unwrap());
        add_public_edge_headers(
            &mut signed_full_target,
            "edge-secret",
            "POST",
            target,
            "edge-nonce-bug-full-target",
        );

        let response = app.clone().oneshot(signed_full_target).await.unwrap();
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["detail"], "report_text is required");

        let mut signed_bare_path =
            public_request(Method::POST, target, Body::from(r#"{"report_text":"   "}"#));
        signed_bare_path
            .headers_mut()
            .insert(CONTENT_TYPE, "application/json".parse().unwrap());
        add_public_edge_headers(
            &mut signed_bare_path,
            "edge-secret",
            "POST",
            "/client/bug-reports",
            "edge-nonce-bug-bare-path",
        );

        let response = app.oneshot(signed_bare_path).await.unwrap();
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["detail"], "Invalid public edge assertion");
    }

    #[tokio::test]
    async fn mobile_attach_ticket_retry_replaces_pending_same_user_device_ticket() {
        let signing_key = SigningKey::random(&mut OsRng);
        let state = AppState::new(mobile_ticket_config(&signing_key));
        let app = router(state.clone());

        let first = app
            .clone()
            .oneshot(attach_ticket_request(&signing_key, "nonce-1"))
            .await
            .unwrap();
        let (_, first_body) = response_json(first).await;
        let second = app
            .oneshot(attach_ticket_request(&signing_key, "nonce-2"))
            .await
            .unwrap();
        let (_, second_body) = response_json(second).await;

        assert_ne!(first_body["ticket_id"], second_body["ticket_id"]);
        let tickets = state.mobile_terminal_tickets.lock().unwrap();
        assert_eq!(tickets.len(), 1);
        assert!(tickets.contains_key(second_body["ticket_id"].as_str().unwrap()));
    }

    #[tokio::test]
    async fn mobile_attach_ticket_rejects_active_attach_quota_at_mint_time() {
        let signing_key = SigningKey::random(&mut OsRng);
        let state = AppState::new(mobile_ticket_config(&signing_key));
        state
            .mobile_terminal_active_attaches
            .lock()
            .unwrap()
            .insert(
                "existing".to_owned(),
                MobileTerminalActiveAttach {
                    user_id: "local_bypass".to_owned(),
                    session_id: "fork1001".to_owned(),
                    provider: "codex-fork".to_owned(),
                    device_key_id: "test-device".to_owned(),
                    started_at_unix: OffsetDateTime::now_utc().unix_timestamp(),
                    stop: Arc::new(AtomicBool::new(false)),
                },
            );
        let app = router(state.clone());

        let response = app
            .oneshot(attach_ticket_request(&signing_key, "nonce-1"))
            .await
            .unwrap();
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["detail"], "Too many active mobile attaches for user");
        assert!(state.mobile_terminal_tickets.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mobile_attach_ticket_rejects_reused_device_proof_nonce() {
        let signing_key = SigningKey::random(&mut OsRng);
        let state = AppState::new(mobile_ticket_config(&signing_key));
        let app = router(state.clone());
        let timestamp = OffsetDateTime::now_utc().unix_timestamp().to_string();

        let mut first = local_request(
            Method::POST,
            "/client/sessions/fork1001/attach-ticket",
            Body::from("{}"),
        );
        let mut second = local_request(
            Method::POST,
            "/client/sessions/fork1001/attach-ticket",
            Body::from("{}"),
        );
        for (name, value) in sign_ticket_headers(
            &signing_key,
            "fork1001",
            "/client/sessions/fork1001/attach-ticket",
            &timestamp,
            "nonce-1",
        ) {
            let header_name = axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap();
            let header_value = axum::http::HeaderValue::from_str(&value).unwrap();
            first
                .headers_mut()
                .insert(header_name.clone(), header_value.clone());
            second.headers_mut().insert(header_name, header_value);
        }

        let first_response = app.clone().oneshot(first).await.unwrap();
        let (first_status, _) = response_json(first_response).await;
        assert_eq!(first_status, StatusCode::OK);

        let second_response = app.oneshot(second).await.unwrap();
        let (second_status, second_body) = response_json(second_response).await;
        assert_eq!(second_status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            second_body["detail"],
            "Device signature nonce was already used"
        );
        assert_eq!(state.mobile_terminal_tickets.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn mobile_attach_ticket_rejects_non_finite_device_timestamp() {
        let signing_key = SigningKey::random(&mut OsRng);
        let app = router(AppState::new(mobile_ticket_config(&signing_key)));
        let mut request = local_request(
            Method::POST,
            "/client/sessions/fork1001/attach-ticket",
            Body::from("{}"),
        );
        for (name, value) in sign_ticket_headers(
            &signing_key,
            "fork1001",
            "/client/sessions/fork1001/attach-ticket",
            "NaN",
            "nonce-1",
        ) {
            request.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(&value).unwrap(),
            );
        }

        let response = app.oneshot(request).await.unwrap();
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["detail"], "Invalid device timestamp");
    }

    #[tokio::test]
    async fn mobile_attach_ticket_signature_uses_external_public_path_prefix() {
        let signing_key = SigningKey::random(&mut OsRng);
        let mut config = mobile_ticket_config(&signing_key);
        config.external_access.public_http_path_prefix = Some("/sm".to_owned());
        let app = router(AppState::new(config));
        let timestamp = OffsetDateTime::now_utc().unix_timestamp().to_string();
        let mut request = local_request(
            Method::POST,
            "/client/sessions/fork1001/attach-ticket",
            Body::from("{}"),
        );
        for (name, value) in sign_ticket_headers(
            &signing_key,
            "fork1001",
            "/sm/client/sessions/fork1001/attach-ticket",
            &timestamp,
            "nonce-1",
        ) {
            request.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(&value).unwrap(),
            );
        }

        let response = app.oneshot(request).await.unwrap();
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["ticket_id"].as_str().unwrap().starts_with("att_"));
    }

    #[tokio::test]
    async fn mobile_attach_ticket_signature_uses_google_auth_public_path_prefix() {
        let signing_key = SigningKey::random(&mut OsRng);
        let mut config = mobile_ticket_config(&signing_key);
        config.google_auth.public_path_prefix = Some("/sm".to_owned());
        let app = router(AppState::new(config));
        let timestamp = OffsetDateTime::now_utc().unix_timestamp().to_string();
        let mut request = local_request(
            Method::POST,
            "/client/sessions/fork1001/attach-ticket",
            Body::from("{}"),
        );
        for (name, value) in sign_ticket_headers(
            &signing_key,
            "fork1001",
            "/sm/client/sessions/fork1001/attach-ticket",
            &timestamp,
            "nonce-1",
        ) {
            request.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(&value).unwrap(),
            );
        }

        let response = app.oneshot(request).await.unwrap();
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["ticket_id"].as_str().unwrap().starts_with("att_"));
    }

    #[tokio::test]
    async fn mobile_terminal_routes_advertise_supported_bridge() {
        let signing_key = SigningKey::random(&mut OsRng);
        let app = router(AppState::new(mobile_ticket_config(&signing_key)));

        let response = app
            .oneshot(local_request(
                Method::GET,
                "/client/sessions",
                Body::empty(),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::OK);
        let session = &body["sessions"][0];
        assert_eq!(session["mobile_terminal"]["supported"], true);
        assert_eq!(session["mobile_terminal"]["transport"], "sm-https-tmux");
        assert_eq!(
            session["mobile_terminal"]["ticket_endpoint"],
            "/client/sessions/fork1001/attach-ticket"
        );
        assert_eq!(
            session["mobile_terminal"]["ws_url"],
            "wss://sm.rajeshgo.li/client/terminal"
        );
        assert_eq!(
            session["mobile_terminal"]["tmux_session"],
            "codex-fork-fork1001"
        );
        assert_eq!(session["mobile_terminal"]["requires_device_key"], true);
        assert_eq!(session["termux_attach"], Value::Null);
        assert_eq!(session["primary_action"]["type"], "mobile_terminal");
        assert_eq!(session["primary_action"]["label"], "Attach");
    }

    #[tokio::test]
    async fn mobile_terminal_plain_http_route_reports_upgrade_required() {
        let signing_key = SigningKey::random(&mut OsRng);
        let app = router(AppState::new(mobile_ticket_config(&signing_key)));

        let response = app
            .oneshot(local_request(
                Method::GET,
                "/client/terminal",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(
            response
                .headers()
                .get(UPGRADE)
                .and_then(|value| value.to_str().ok()),
            Some("websocket")
        );
        assert_eq!(
            response
                .headers()
                .get(CONNECTION)
                .and_then(|value| value.to_str().ok()),
            Some("Upgrade")
        );
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::UPGRADE_REQUIRED);
        assert_eq!(
            body,
            json!({ "detail": "mobile terminal requires a WebSocket upgrade" })
        );
    }

    #[tokio::test]
    async fn mobile_terminal_plain_http_route_requires_authentication() {
        let signing_key = SigningKey::random(&mut OsRng);
        let mut config = mobile_ticket_config(&signing_key);
        config.google_auth.enabled = true;
        config.google_auth.public_host = Some("sm.rajeshgo.li".to_owned());
        let app = router(AppState::new(config));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method(Method::GET)
                    .uri("/client/terminal")
                    .header(HOST, "sm.rajeshgo.li")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["detail"], "Authentication required");
    }

    #[tokio::test]
    async fn mobile_terminal_plain_http_route_requires_terminal_user() {
        let signing_key = SigningKey::random(&mut OsRng);
        let mut config = mobile_ticket_config(&signing_key);
        config
            .mobile_terminal
            .allowed_users
            .get_mut("local_bypass")
            .unwrap()
            .interactive_shell_access = false;
        let app = router(AppState::new(config));

        let response = app
            .oneshot(local_request(
                Method::GET,
                "/client/terminal",
                Body::empty(),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body["detail"],
            "User is not allowed to use mobile terminal attach"
        );
    }

    #[tokio::test]
    async fn mobile_terminal_disable_requires_owner_authorization() {
        let signing_key = SigningKey::random(&mut OsRng);
        let state = AppState::new(mobile_ticket_config(&signing_key));
        let app = router(state.clone());

        let response = app
            .oneshot(local_request(
                Method::POST,
                "/client/mobile-terminal/disable",
                Body::empty(),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body["detail"],
            "User is not allowed to disable mobile terminal attach"
        );
        assert!(!state
            .mobile_terminal_runtime_disabled
            .load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn mobile_terminal_disable_owner_terminates_active_attaches() {
        let signing_key = SigningKey::random(&mut OsRng);
        let mut config = mobile_ticket_config(&signing_key);
        config
            .mobile_terminal
            .allowed_users
            .get_mut("local_bypass")
            .unwrap()
            .mobile_terminal_owner = true;
        let state = AppState::new(config);
        let ticket = mint_mobile_attach_ticket(&state, &signing_key, "ticket-nonce-1").await;
        assert!(state
            .mobile_terminal_tickets
            .lock()
            .unwrap()
            .contains_key(ticket["ticket_id"].as_str().unwrap()));
        let stop = Arc::new(AtomicBool::new(false));
        state
            .mobile_terminal_active_attaches
            .lock()
            .unwrap()
            .insert(
                "active-1".to_owned(),
                MobileTerminalActiveAttach {
                    user_id: "local_bypass".to_owned(),
                    session_id: "fork1001".to_owned(),
                    provider: "codex-fork".to_owned(),
                    device_key_id: "test-device".to_owned(),
                    started_at_unix: OffsetDateTime::now_utc().unix_timestamp(),
                    stop: stop.clone(),
                },
            );
        let app = router(state.clone());

        let response = app
            .clone()
            .oneshot(local_request(
                Method::POST,
                "/client/mobile-terminal/disable",
                Body::empty(),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "ok": true,
                "disabled": true,
                "active_attaches_terminated": 1,
            })
        );
        assert!(state
            .mobile_terminal_runtime_disabled
            .load(Ordering::SeqCst));
        assert!(state.mobile_terminal_tickets.lock().unwrap().is_empty());
        assert!(state
            .mobile_terminal_active_attaches
            .lock()
            .unwrap()
            .is_empty());
        assert!(stop.load(Ordering::SeqCst));

        let sessions = app
            .clone()
            .oneshot(local_request(
                Method::GET,
                "/client/sessions",
                Body::empty(),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(sessions).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sessions"][0]["mobile_terminal"]["supported"], false);
        assert_eq!(
            body["sessions"][0]["mobile_terminal"]["reason"],
            "mobile terminal attach is disabled"
        );

        let ticket_after_disable = app
            .oneshot(attach_ticket_request(&signing_key, "ticket-nonce-2"))
            .await
            .unwrap();
        let (status, body) = response_json(ticket_after_disable).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["detail"], "Mobile terminal attach is disabled");
    }

    #[tokio::test]
    async fn mobile_terminal_devices_list_omits_public_key_material() {
        let signing_key = SigningKey::random(&mut OsRng);
        let app = router(AppState::new(mobile_ticket_config(&signing_key)));

        let response = app
            .oneshot(local_request(
                Method::GET,
                "/client/mobile-terminal/devices",
                Body::empty(),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["owner_view"], false);
        assert_eq!(body["runtime_only_revocations"], true);
        assert_eq!(body["devices"].as_array().unwrap().len(), 1);
        let device = &body["devices"][0];
        assert_eq!(device["user_id"], "local_bypass");
        assert_eq!(device["device_key_id"], "test-device");
        assert_eq!(device["enabled"], true);
        assert_eq!(device["revoked"], false);
        assert!(device.get("public_key").is_none());
    }

    #[tokio::test]
    async fn mobile_terminal_revoke_device_clears_tickets_and_stops_active_attach() {
        let signing_key = SigningKey::random(&mut OsRng);
        let state = AppState::new(mobile_ticket_config(&signing_key));
        let ticket = mint_mobile_attach_ticket(&state, &signing_key, "ticket-nonce-1").await;
        assert!(state
            .mobile_terminal_tickets
            .lock()
            .unwrap()
            .contains_key(ticket["ticket_id"].as_str().unwrap()));
        let stop = Arc::new(AtomicBool::new(false));
        state
            .mobile_terminal_active_attaches
            .lock()
            .unwrap()
            .insert(
                "active-1".to_owned(),
                MobileTerminalActiveAttach {
                    user_id: "local_bypass".to_owned(),
                    session_id: "fork1001".to_owned(),
                    provider: "codex-fork".to_owned(),
                    device_key_id: "test-device".to_owned(),
                    started_at_unix: OffsetDateTime::now_utc().unix_timestamp(),
                    stop: stop.clone(),
                },
            );
        let app = router(state.clone());

        let response = app
            .clone()
            .oneshot(local_request(
                Method::DELETE,
                "/client/mobile-terminal/devices/test-device",
                Body::empty(),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "ok": true,
                "revoked": true,
                "user_id": "local_bypass",
                "device_key_id": "test-device",
                "already_revoked": false,
                "pending_tickets_revoked": 1,
                "active_attaches_terminated": 1,
                "runtime_only": true,
            })
        );
        assert!(state
            .mobile_terminal_revoked_keys
            .lock()
            .unwrap()
            .contains(&("local_bypass".to_owned(), "test-device".to_owned())));
        assert!(state.mobile_terminal_tickets.lock().unwrap().is_empty());
        assert!(state
            .mobile_terminal_active_attaches
            .lock()
            .unwrap()
            .is_empty());
        assert!(stop.load(Ordering::SeqCst));

        let list_after_revoke = app
            .clone()
            .oneshot(local_request(
                Method::GET,
                "/client/mobile-terminal/devices",
                Body::empty(),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(list_after_revoke).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["devices"][0]["revoked"], true);

        let sessions_after_revoke = app
            .clone()
            .oneshot(local_request(
                Method::GET,
                "/client/sessions",
                Body::empty(),
            ))
            .await
            .unwrap();
        let (status, body) = response_json(sessions_after_revoke).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sessions"][0]["mobile_terminal"]["supported"], false);
        assert_eq!(
            body["sessions"][0]["mobile_terminal"]["reason"],
            "registered mobile device key is required"
        );

        let ticket_after_revoke = app
            .oneshot(attach_ticket_request(&signing_key, "ticket-nonce-2"))
            .await
            .unwrap();
        let (status, body) = response_json(ticket_after_revoke).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["detail"], "Device key is not registered");
    }

    #[tokio::test]
    async fn mobile_terminal_consume_rechecks_device_revocation_after_mint() {
        let signing_key = SigningKey::random(&mut OsRng);
        let state = AppState::new(mobile_ticket_config(&signing_key));
        let ticket = mint_mobile_attach_ticket(&state, &signing_key, "ticket-nonce-1").await;
        state
            .mobile_terminal_revoked_keys
            .lock()
            .unwrap()
            .insert(("local_bypass".to_owned(), "test-device".to_owned()));
        let frame = signed_mobile_terminal_auth_frame(&signing_key, &ticket, "ws-nonce-1");

        let error = consume_mobile_terminal_ticket(&state, &frame).unwrap_err();
        let (status, detail) = api_error_status_detail(error);

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(detail, "Device key is no longer registered");
        assert!(state
            .mobile_terminal_active_attaches
            .lock()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn mobile_terminal_auth_consumes_ticket_and_tracks_active_attach() {
        let signing_key = SigningKey::random(&mut OsRng);
        let state = AppState::new(mobile_ticket_config(&signing_key));
        let ticket = mint_mobile_attach_ticket(&state, &signing_key, "ticket-nonce-1").await;
        let frame = signed_mobile_terminal_auth_frame(&signing_key, &ticket, "ws-nonce-1");

        let (consumed_ticket, attach_id, stop) =
            consume_mobile_terminal_ticket(&state, &frame).unwrap();

        assert_eq!(consumed_ticket.ticket_id, ticket["ticket_id"]);
        assert_eq!(consumed_ticket.session_id, "fork1001");
        assert_eq!(consumed_ticket.device_key_id, "test-device");
        assert!(!stop.load(Ordering::SeqCst));
        assert!(!state
            .mobile_terminal_tickets
            .lock()
            .unwrap()
            .contains_key(ticket["ticket_id"].as_str().unwrap()));
        {
            let active = state.mobile_terminal_active_attaches.lock().unwrap();
            let attach = active.get(&attach_id).unwrap();
            assert_eq!(attach.user_id, "local_bypass");
            assert_eq!(attach.session_id, "fork1001");
            assert_eq!(attach.provider, "codex-fork");
            assert_eq!(attach.device_key_id, "test-device");
        }

        remove_mobile_terminal_active_attach(&state, &attach_id);
        assert!(state
            .mobile_terminal_active_attaches
            .lock()
            .unwrap()
            .is_empty());

        let replay = consume_mobile_terminal_ticket(&state, &frame).unwrap_err();
        let (status, detail) = api_error_status_detail(replay);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(detail, "Attach ticket is invalid or expired");
    }

    #[tokio::test]
    async fn mobile_terminal_auth_rejects_secret_mismatch_without_consuming_ticket() {
        let signing_key = SigningKey::random(&mut OsRng);
        let state = AppState::new(mobile_ticket_config(&signing_key));
        let ticket = mint_mobile_attach_ticket(&state, &signing_key, "ticket-nonce-1").await;
        let mut frame = signed_mobile_terminal_auth_frame(&signing_key, &ticket, "ws-nonce-1");
        frame.ticket_secret = Some("wrong-secret".to_owned());

        let error = consume_mobile_terminal_ticket(&state, &frame).unwrap_err();
        let (status, detail) = api_error_status_detail(error);

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(detail, "Attach ticket secret mismatch");
        assert!(state
            .mobile_terminal_tickets
            .lock()
            .unwrap()
            .contains_key(ticket["ticket_id"].as_str().unwrap()));
    }

    #[tokio::test]
    async fn mobile_terminal_auth_rejects_device_mismatch_without_consuming_ticket() {
        let signing_key = SigningKey::random(&mut OsRng);
        let state = AppState::new(mobile_ticket_config(&signing_key));
        let ticket = mint_mobile_attach_ticket(&state, &signing_key, "ticket-nonce-1").await;
        let mut frame = signed_mobile_terminal_auth_frame(&signing_key, &ticket, "ws-nonce-1");
        frame.device_key_id = Some("other-device".to_owned());

        let error = consume_mobile_terminal_ticket(&state, &frame).unwrap_err();
        let (status, detail) = api_error_status_detail(error);

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(detail, "Attach ticket device mismatch");
        assert!(state
            .mobile_terminal_tickets
            .lock()
            .unwrap()
            .contains_key(ticket["ticket_id"].as_str().unwrap()));
    }

    #[tokio::test]
    async fn mobile_terminal_auth_rejects_invalid_signature_without_consuming_ticket() {
        let signing_key = SigningKey::random(&mut OsRng);
        let state = AppState::new(mobile_ticket_config(&signing_key));
        let ticket = mint_mobile_attach_ticket(&state, &signing_key, "ticket-nonce-1").await;
        let mut frame = signed_mobile_terminal_auth_frame(&signing_key, &ticket, "ws-nonce-1");
        frame.nonce = Some("tampered-nonce".to_owned());

        let error = consume_mobile_terminal_ticket(&state, &frame).unwrap_err();
        let (status, detail) = api_error_status_detail(error);

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(detail, "Invalid device signature");
        assert!(state
            .mobile_terminal_tickets
            .lock()
            .unwrap()
            .contains_key(ticket["ticket_id"].as_str().unwrap()));
    }

    #[tokio::test]
    async fn mobile_terminal_auth_rechecks_active_limits_without_consuming_ticket() {
        let signing_key = SigningKey::random(&mut OsRng);
        let state = AppState::new(mobile_ticket_config(&signing_key));
        let ticket = mint_mobile_attach_ticket(&state, &signing_key, "ticket-nonce-1").await;
        let frame = signed_mobile_terminal_auth_frame(&signing_key, &ticket, "ws-nonce-1");
        state
            .mobile_terminal_active_attaches
            .lock()
            .unwrap()
            .insert(
                "existing".to_owned(),
                MobileTerminalActiveAttach {
                    user_id: "local_bypass".to_owned(),
                    session_id: "fork1001".to_owned(),
                    provider: "codex-fork".to_owned(),
                    device_key_id: "test-device".to_owned(),
                    started_at_unix: OffsetDateTime::now_utc().unix_timestamp(),
                    stop: Arc::new(AtomicBool::new(false)),
                },
            );

        let error = consume_mobile_terminal_ticket(&state, &frame).unwrap_err();
        let (status, detail) = api_error_status_detail(error);

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(detail, "Too many active mobile attaches for user");
        assert!(state
            .mobile_terminal_tickets
            .lock()
            .unwrap()
            .contains_key(ticket["ticket_id"].as_str().unwrap()));
    }
}
