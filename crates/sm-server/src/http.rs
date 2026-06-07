use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header::HOST, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Serialize;
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::config::{trimmed, AppConfig};

#[derive(Clone)]
pub struct AppState {
    config: AppConfig,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        Self { config }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/health/detailed", get(health_detailed))
        .route("/auth/session", get(auth_session))
        .route("/client/bootstrap", get(client_bootstrap))
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

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "detail": "Not Found" })),
    )
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
