use std::{collections::BTreeMap, convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

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
    routing::{get, post},
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

use crate::config::{trimmed, AppConfig};
use crate::sessions::{
    expand_home, ClientSessionResponse, SessionResponse, SessionStore, SessionsEnvelope,
};

const SESSION_COOKIE_NAME: &str = "sm_auth";
const SESSION_COOKIE_MAX_AGE_SECONDS: i64 = 60 * 60 * 24 * 14;
const SHADOW_ENVELOPE_MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    config: AppConfig,
    session_store: SessionStore,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        let session_store = SessionStore::new(expand_home(&config.paths.state_file));
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
        .route("/events/state", get(events_state))
        .route("/events", get(events_stream))
        .route("/__shadow/http", post(shadow_http))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{session_id}", get(get_session))
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
    let auth = &state.config.google_auth;
    let external = &state.config.external_access;

    Json(ClientBootstrapResponse {
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
        },
    })
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
        "/auth/session" => Some(serde_json::to_vec(&shadow_auth_session_response(
            &state.config,
        ))?),
        "/client/bootstrap" => Some(serde_json::to_vec(&shadow_client_bootstrap_response(
            &state.config,
        ))?),
        "/events/state" => Some(serde_json::to_vec(&event_state_payload())?),
        "/sessions" => {
            let include_stopped = query_bool(query_string, "include_stopped");
            let sessions = state
                .session_store
                .list_sessions(include_stopped)?
                .into_iter()
                .map(SessionResponse::from)
                .collect::<Vec<_>>();
            Some(serde_json::to_vec(&SessionsEnvelope::from(sessions))?)
        }
        "/client/sessions" => {
            let sessions = state
                .session_store
                .list_sessions(false)?
                .into_iter()
                .map(ClientSessionResponse::from)
                .collect::<Vec<_>>();
            Some(serde_json::to_vec(&SessionsEnvelope::from(sessions))?)
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

fn shadow_auth_session_response(config: &AppConfig) -> AuthSessionResponse {
    let auth = &config.google_auth;
    if !auth.requested() {
        return AuthSessionResponse::disabled_bypass();
    }
    if !auth.ready() {
        return AuthSessionResponse::misconfigured();
    }
    AuthSessionResponse {
        enabled: true,
        authenticated: false,
        bypass: false,
        email: None,
        name: None,
        auth_type: None,
        error: None,
    }
}

fn shadow_client_bootstrap_response(config: &AppConfig) -> ClientBootstrapResponse {
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
        },
    }
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

fn ensure_shadow_allowed(
    config: &AppConfig,
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
) -> Result<(), ApiError> {
    if peer_addr
        .map(|addr| addr.ip().is_loopback())
        .unwrap_or(false)
    {
        return Ok(());
    }

    let expected = trimmed(&config.rust_shadow.secret);
    let provided = headers
        .get("x-sm-rust-shadow-secret")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let (Some(expected), Some(provided)) = (expected, provided) {
        if constant_time_eq(expected.as_bytes(), provided.as_bytes()) {
            return Ok(());
        }
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

fn ensure_session_read_allowed(state: &AppState, request: &Request) -> Result<(), ApiError> {
    let auth = &state.config.google_auth;
    if !auth.requested() {
        return Ok(());
    }
    let peer_addr = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|value| value.0);
    if is_local_bypass_request(request.headers(), peer_addr, &state.config) {
        return Ok(());
    }
    if !auth.ready() {
        return Err(ApiError::Auth {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "Google auth is enabled but incomplete",
            login_url: None,
        });
    }
    if authenticated_user(request.headers(), &state.config).is_some() {
        return Ok(());
    }
    Err(ApiError::Auth {
        status: StatusCode::UNAUTHORIZED,
        detail: "Authentication required",
        login_url: Some(google_login_redirect(request.uri().path())),
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
struct SessionOutputQuery {
    lines: Option<usize>,
}

#[derive(Serialize)]
struct SessionOutputResponse {
    session_id: String,
    output: Option<String>,
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
}
