use std::{
    collections::BTreeMap,
    convert::Infallible,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::to_bytes,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{
        header::{AUTHORIZATION, COOKIE, HOST},
        HeaderMap, StatusCode,
    },
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post, put},
    Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use futures_util::stream::{self, StreamExt};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::config::{trimmed, AppConfig, PublicNodeConfig};
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

const SESSION_COOKIE_NAME: &str = "sm_auth";
const SESSION_COOKIE_MAX_AGE_SECONDS: i64 = 60 * 60 * 24 * 14;
const SHADOW_ENVELOPE_MAX_BYTES: usize = 1024 * 1024;
const EM_SPAWN_STOP_NOTIFY_DELAY_SECONDS: i64 = 8;

#[derive(Clone)]
pub struct AppState {
    config: AppConfig,
    session_store: SessionStore,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        let state_file = expand_home(&config.paths.state_file);
        let queue_db_path = expand_home(&config.sm_send.db_path);
        let session_store = SessionStore::new_with_queue(state_file, queue_db_path);
        Self {
            config,
            session_store,
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/health/detailed", get(health_detailed))
        .route("/auth/session", get(auth_session))
        .route("/client/bootstrap", get(client_bootstrap))
        .route("/client/analytics/summary", get(client_analytics_summary))
        .route("/events/state", get(events_state))
        .route("/events", get(events_stream))
        .route("/__shadow/http", post(shadow_http))
        .route("/queue-jobs", get(list_queue_jobs))
        .route("/codex-review-requests", get(list_codex_review_requests))
        .route("/nodes", get(list_nodes))
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
        .route("/client/sessions", get(list_client_sessions))
        .route("/client/sessions/{session_id}", get(get_client_session))
        .fallback(not_found)
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

async fn client_bootstrap(State(state): State<Arc<AppState>>) -> Json<ClientBootstrapResponse> {
    Json(client_bootstrap_response(&state.config))
}

async fn client_analytics_summary(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    Ok(Json(build_mobile_analytics_summary(
        &state.config,
        &state.session_store,
    )?))
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
) -> Result<Json<SessionsEnvelope<ClientSessionResponse>>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let sessions = state
        .session_store
        .list_sessions(false)?
        .into_iter()
        .map(ClientSessionResponse::from)
        .collect::<Vec<_>>();
    Ok(Json(SessionsEnvelope::from(sessions)))
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
) -> Result<Json<ClientSessionResponse>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let Some(session) = state.session_store.get_session(&session_id)? else {
        return Err(ApiError::NotFound("Session not found"));
    };
    Ok(Json(ClientSessionResponse::from(session)))
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
        let Some(session) = state.session_store.get_session(session_id)? else {
            let body = serde_json::to_vec(&json!({ "detail": "Session not found" }))?;
            return Ok(Some(ShadowPrediction {
                status: StatusCode::NOT_FOUND.as_u16(),
                body_sha256: Some(sha256_hex(&body)),
                support_status: "implemented_read",
            }));
        };
        let body = serde_json::to_vec(&ClientSessionResponse::from(session))?;
        return Ok(Some(ShadowPrediction {
            status: StatusCode::OK.as_u16(),
            body_sha256: Some(sha256_hex(&body)),
            support_status: "implemented_read",
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

fn client_bootstrap_response(config: &AppConfig) -> ClientBootstrapResponse {
    let auth = &config.google_auth;
    let external = &config.external_access;

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
            mobile_terminal_supported: false,
            mobile_terminal_ws_url: None,
        },
        session_open_defaults: SessionOpenDefaults {
            preferred_action: "details",
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

fn is_static_sessions_path(path: &str) -> bool {
    matches!(
        path,
        "/sessions/create" | "/sessions/input-batch" | "/sessions/spawn" | "/sessions/review"
    )
}

fn is_retained_write_surface(method: &str, path: &str) -> bool {
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
        || path == "/client/analytics/summary"
        || path == "/codex-review-requests"
        || path == "/queue-jobs"
        || path == "/nodes"
        || path == "/sessions"
        || path == "/client/sessions"
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
struct KillSessionRequest {
    #[serde(default)]
    requester_session_id: Option<String>,
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
