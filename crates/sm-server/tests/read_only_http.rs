use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use serde_json::{json, Value};
use sm_server::{
    config::{AppConfig, ExternalAccessConfig, GoogleAuthConfig},
    http::{router, AppState},
};
use std::net::SocketAddr;
use tower::ServiceExt;

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn get_json_with_host(app: axum::Router, uri: &str, host: &str) -> (StatusCode, Value) {
    get_json_with_host_and_peer(app, uri, host, None).await
}

async fn get_json_with_host_and_peer(
    app: axum::Router,
    uri: &str,
    host: &str,
    peer_addr: Option<SocketAddr>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .uri(uri)
        .header("host", host)
        .body(Body::empty())
        .unwrap();
    if let Some(peer_addr) = peer_addr {
        request.extensions_mut().insert(ConnectInfo(peer_addr));
    }
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn health_matches_python_basic_shape() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = get_json(app, "/health").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "status": "healthy" }));
}

#[tokio::test]
async fn detailed_health_has_required_top_level_fields() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = get_json(app, "/health/detailed").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "healthy");
    assert!(payload["checks"].is_object());
    assert!(payload["resources"].is_object());
    assert!(payload["timestamp"].is_string());
}

#[tokio::test]
async fn auth_session_reports_disabled_bypass_when_google_auth_not_requested() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = get_json(app, "/auth/session").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "enabled": false,
            "authenticated": true,
            "bypass": true,
            "email": null,
            "name": null
        })
    );
}

#[tokio::test]
async fn auth_session_reports_local_bypass_for_localhost_when_auth_is_misconfigured() {
    let app = router(AppState::new(AppConfig {
        google_auth: GoogleAuthConfig {
            enabled: true,
            public_host: Some("sm.example.com".to_owned()),
            ..GoogleAuthConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json_with_host_and_peer(
        app,
        "/auth/session",
        "localhost:8421",
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "enabled": true,
            "authenticated": true,
            "bypass": true,
            "email": null,
            "name": null
        })
    );
}

#[tokio::test]
async fn auth_session_reports_misconfigured_on_public_host_without_ready_google_auth() {
    let app = router(AppState::new(AppConfig {
        google_auth: GoogleAuthConfig {
            enabled: true,
            public_host: Some("sm.example.com".to_owned()),
            ..GoogleAuthConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json_with_host(app, "/auth/session", "sm.example.com").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "enabled": true,
            "authenticated": false,
            "bypass": false,
            "email": null,
            "name": null,
            "error": "misconfigured"
        })
    );
}

#[tokio::test]
async fn auth_session_does_not_bypass_for_spoofed_localhost_host_from_remote_peer() {
    let app = router(AppState::new(AppConfig {
        google_auth: GoogleAuthConfig {
            enabled: true,
            public_host: Some("sm.example.com".to_owned()),
            ..GoogleAuthConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json_with_host_and_peer(
        app,
        "/auth/session",
        "localhost:8421",
        Some(SocketAddr::from(([203, 0, 113, 10], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "enabled": true,
            "authenticated": false,
            "bypass": false,
            "email": null,
            "name": null,
            "error": "misconfigured"
        })
    );
}

#[tokio::test]
async fn bootstrap_preserves_native_schema_without_termux_or_terminal_advertisement() {
    let app = router(AppState::new(AppConfig {
        google_auth: GoogleAuthConfig {
            client_id: Some("web-client-id".to_owned()),
            ..GoogleAuthConfig::default()
        },
        external_access: ExternalAccessConfig {
            public_http_host: Some("sm.example.com".to_owned()),
            public_ssh_host: Some("ssh.sm.example.com".to_owned()),
            ssh_username: Some("rajesh".to_owned()),
            ..ExternalAccessConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/client/bootstrap").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "auth": {
                "mode": "browser_session_cookie",
                "session_endpoint": "/auth/session",
                "login_endpoint": "/auth/google/login",
                "logout_endpoint": "/auth/logout",
                "device_auth_endpoint": "/auth/device/google",
                "device_auth_token_type": "Bearer",
                "google_server_client_id": "web-client-id"
            },
            "external_access": {
                "public_http_host": "sm.example.com",
                "public_ssh_host": "ssh.sm.example.com",
                "ssh_username": "rajesh",
                "termux_attach_supported": false,
                "mobile_terminal_supported": false,
                "mobile_terminal_ws_url": null
            },
            "session_open_defaults": {
                "preferred_action": "details"
            }
        })
    );
    assert!(payload["external_access"]
        .get("ssh_proxy_command")
        .is_none());
}

#[tokio::test]
async fn absent_routes_are_not_implemented() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = get_json(app, "/api/sessions").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Not Found" }));
}
