use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode},
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use futures_util::{future::join, StreamExt as _};
use hmac::{Hmac, Mac};
use rusqlite::Connection;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use sm_server::config::RustShadowConfig;
use sm_server::queue::RetainedQueueStore;
use sm_server::{
    config::{
        AppConfig, ExternalAccessConfig, GoogleAuthConfig, PathsConfig, RustCoreConfig,
        SmSendConfig,
    },
    http::{router, AppState},
};
use std::{
    fs,
    net::SocketAddr,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tower::ServiceExt;

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let (status, _headers, body) = get_response(app, uri).await;
    (status, serde_json::from_slice(&body).unwrap())
}

async fn get_response(app: axum::Router, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, body.to_vec())
}

async fn post_json(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, Value) {
    json_request_with_headers_and_peer(
        app,
        "POST",
        uri,
        payload,
        &[],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await
}

async fn post_json_with_headers_and_peer(
    app: axum::Router,
    uri: &str,
    payload: Value,
    headers: &[(&str, &str)],
    peer_addr: Option<SocketAddr>,
) -> (StatusCode, Value) {
    json_request_with_headers_and_peer(app, "POST", uri, payload, headers, peer_addr).await
}

async fn put_json(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, Value) {
    json_request_with_headers_and_peer(
        app,
        "PUT",
        uri,
        payload,
        &[],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await
}

async fn delete_json(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, Value) {
    json_request_with_headers_and_peer(
        app,
        "DELETE",
        uri,
        payload,
        &[],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await
}

async fn json_request_with_headers_and_peer(
    app: axum::Router,
    method: &str,
    uri: &str,
    payload: Value,
    headers: &[(&str, &str)],
    peer_addr: Option<SocketAddr>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();
    if let Some(peer_addr) = peer_addr {
        request.extensions_mut().insert(ConnectInfo(peer_addr));
    }
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn get_json_with_host(app: axum::Router, uri: &str, host: &str) -> (StatusCode, Value) {
    get_json_with_host_and_peer(app, uri, host, None).await
}

async fn get_json_with_host_and_headers(
    app: axum::Router,
    uri: &str,
    host: &str,
    headers: &[(&str, String)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().uri(uri).header("host", host);
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }
    let response = app
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
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
async fn shadow_http_reports_match_for_stable_read_only_route() {
    let app = router(AppState::new(AppConfig::default()));
    let python_body = serde_json::to_vec(&json!({ "status": "healthy" })).unwrap();

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/health",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "match");
    assert_eq!(payload["would_write"], false);
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["body_sha256_match"], true);
}

#[tokio::test]
async fn shadow_http_reports_body_mismatch_for_stable_read_only_route() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/health",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"different")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "body_mismatch");
    assert_eq!(payload["body_sha256_match"], false);
}

#[tokio::test]
async fn shadow_http_classifies_core_writes_without_side_effects() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "POST",
                "path": "/sessions",
                "query_string": "",
                "headers": {},
                "body_sha256": sha256_hex(b"{\"working_dir\":\"~\"}")
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"{\"id\":\"python-owned\"}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "unsupported_retained_write");
    assert_eq!(payload["comparison"], "not_compared");
    assert_eq!(payload["would_write"], false);
    assert!(payload["detail"]
        .as_str()
        .unwrap()
        .contains("never performs retained write side effects"));
}

#[tokio::test]
async fn shadow_http_rejects_remote_without_shadow_secret() {
    let app = router(AppState::new(AppConfig::default()));
    let python_body = serde_json::to_vec(&json!({ "status": "healthy" })).unwrap();

    let (status, payload) = post_json_with_headers_and_peer(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/health",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
        &[],
        Some(SocketAddr::from(([203, 0, 113, 10], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "Rust shadow endpoint requires local peer or shadow secret"
    );
}

#[tokio::test]
async fn shadow_http_allows_remote_with_configured_shadow_secret() {
    let app = router(AppState::new(AppConfig {
        rust_shadow: RustShadowConfig {
            secret: Some("shared-shadow-secret".to_owned()),
        },
        ..AppConfig::default()
    }));
    let python_body = serde_json::to_vec(&json!({ "status": "healthy" })).unwrap();

    let (status, payload) = post_json_with_headers_and_peer(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/health",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
        &[("x-sm-rust-shadow-secret", "shared-shadow-secret")],
        Some(SocketAddr::from(([203, 0, 113, 10], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["comparison"], "match");
}

#[tokio::test]
async fn shadow_http_requires_configured_secret_even_from_loopback() {
    let app = router(AppState::new(AppConfig {
        rust_shadow: RustShadowConfig {
            secret: Some("shared-shadow-secret".to_owned()),
        },
        ..AppConfig::default()
    }));
    let python_body = serde_json::to_vec(&json!({ "status": "healthy" })).unwrap();

    let (status, payload) = post_json_with_headers_and_peer(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/health",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
        &[],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "Rust shadow endpoint requires local peer or shadow secret"
    );
}

#[tokio::test]
async fn shadow_http_preserves_python_auth_denial_for_protected_reads() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 401,
                "body_sha256": sha256_hex(b"{\"detail\":\"Authentication required\"}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "python_auth_denial");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 401);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
}

#[tokio::test]
async fn shadow_http_treats_auth_session_as_status_only() {
    let app = router(AppState::new(AppConfig {
        google_auth: GoogleAuthConfig {
            enabled: true,
            public_host: Some("sm.example.com".to_owned()),
            client_id: Some("web-client".to_owned()),
            client_secret: Some("web-secret".to_owned()),
            redirect_uri: Some("https://sm.example.com/auth/google/callback".to_owned()),
            allowlist_emails: vec!["user@example.com".to_owned()],
            session_cookie_secret: Some("cookie-secret".to_owned()),
            ..GoogleAuthConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/auth/session",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"{\"authenticated\":true}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
}

#[tokio::test]
async fn shadow_http_does_not_treat_static_sessions_route_as_session_id() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/context-monitor",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"{\"enabled\":true}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "body_mismatch");
    assert_eq!(payload["predicted_status"], 200);
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

#[tokio::test]
async fn events_state_returns_fallback_snapshot() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = get_json(app, "/events/state").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "tmux_client_event_version": 0,
            "last_tmux_client_event": null
        })
    );
}

#[tokio::test]
async fn events_stream_emits_hello_frame_with_sse_headers() {
    let app = router(AppState::new(AppConfig::default()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let mut body_stream = response.into_body().into_data_stream();
    let first_chunk = body_stream.next().await.unwrap().unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-cache")
    );
    assert_eq!(
        headers
            .get("x-accel-buffering")
            .and_then(|value| value.to_str().ok()),
        Some("no")
    );
    assert_eq!(
        String::from_utf8(first_chunk.to_vec()).unwrap(),
        "event: hello\ndata: {\"tmux_client_event_version\":0,\"last_tmux_client_event\":null}\n\n"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), body_stream.next())
            .await
            .is_err(),
        "SSE stream closed before the keepalive interval"
    );
}

#[tokio::test]
async fn events_state_rejects_public_host_when_google_auth_enabled() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));

    let (status, payload) = get_json_with_host(app, "/events/state", "sm.example.com").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        payload,
        json!({
            "detail": "Authentication required",
            "login_url": "/auth/google/login?next=%2Fevents%2Fstate"
        })
    );
}

#[tokio::test]
async fn sessions_lists_running_sessions_and_filters_stopped_by_default() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(payload["sessions"][0]["id"], "run12345");
    assert_eq!(payload["sessions"][0]["friendly_name"], "Runner Native");
    assert_eq!(payload["sessions"][0]["activity_state"], "working");
    assert_eq!(payload["sessions"][0]["provider"], "claude");
    assert_eq!(payload["sessions"][1]["id"], "oldstate");
    assert_eq!(payload["sessions"][1]["friendly_name"], "claude-oldstate");
    assert_eq!(payload["sessions"][1]["status"], "idle");
    assert_eq!(payload["sessions"][1]["activity_state"], "idle");
    assert!(payload["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session["id"] != "stop1234"));
}

#[tokio::test]
async fn sessions_can_include_stopped_sessions() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions?include_stopped=true").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["sessions"].as_array().unwrap().len(), 3);
    assert_eq!(payload["sessions"][2]["id"], "stop1234");
    assert_eq!(payload["sessions"][2]["activity_state"], "stopped");
}

#[tokio::test]
async fn client_sessions_adds_read_only_mobile_metadata_without_termux() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/client/sessions").await;

    assert_eq!(status, StatusCode::OK);
    let first = &payload["sessions"][0];
    assert_eq!(first["id"], "run12345");
    assert_eq!(first["attach_descriptor"]["attach_supported"], false);
    assert_eq!(
        first["attach_descriptor"]["message"],
        "attach tickets are not implemented in the Rust read-only scaffold"
    );
    assert_eq!(first["termux_attach"], Value::Null);
    assert_eq!(first["mobile_terminal"]["supported"], false);
    assert_eq!(first["primary_action"]["type"], "details");
    assert!(payload["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session["id"] != "stop1234"));
}

#[tokio::test]
async fn session_detail_returns_one_projected_session() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions/run12345").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "run12345");
    assert_eq!(payload["friendly_name"], "Runner Native");
    assert_eq!(payload["activity_state"], "working");
    assert_eq!(payload["provider"], "claude");
}

#[tokio::test]
async fn session_detail_returns_404_for_unknown_session() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions/missing-session").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Session not found" }));
}

#[tokio::test]
async fn client_session_detail_returns_mobile_metadata_for_one_session() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/client/sessions/run12345").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "run12345");
    assert_eq!(payload["attach_descriptor"]["attach_supported"], false);
    assert_eq!(payload["termux_attach"], Value::Null);
    assert_eq!(payload["mobile_terminal"]["supported"], false);
    assert_eq!(payload["primary_action"]["type"], "details");
}

#[tokio::test]
async fn session_detail_prunes_stale_role_aliases() {
    let state_file = write_registry_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app.clone(), "/sessions/stale-role").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Session not found" }));

    let (status, payload) = get_json(app.clone(), "/client/sessions/stale-role").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Session not found" }));

    let (status, payload) = get_json(app, "/sessions/reviewer").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "child001");
}

#[tokio::test]
async fn session_output_tails_fixture_log_file() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions/run12345/output?lines=2").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "run12345");
    assert_eq!(
        payload["output"],
        "fixture log line 2\nfixture log line 3\n"
    );
}

#[tokio::test]
async fn fixture_core_writes_are_disabled_by_default() {
    let state_file = write_session_fixture();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let app = router(AppState::new(config_with_state_file_and_queue(&state_file)));

    let (status, payload) = post_json(
        app,
        "/sessions",
        json!({
            "id": "rustcore",
            "name": "rust-core",
            "working_dir": "/repo",
            "provider": "claude"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        payload,
        json!({ "detail": "Rust core writes are disabled" })
    );
    assert!(
        !queue_db_path.exists(),
        "disabled Rust core writes must not create the retained queue DB"
    );
}

#[test]
fn runtime_core_inherits_existing_tmux_socket_config() {
    let config_path = unique_temp_path();
    let missing_env_path = unique_temp_path();

    fs::write(
        &config_path,
        r#"
tmux:
  socket_name: "session-manager"
timeouts:
  tmux:
    send_keys_settle_seconds: 0.25
    send_keys_settle_max_seconds: 1.25
    send_keys_settle_per_ki_chars: 0.07
    send_keys_settle_per_extra_line: 0.02
    send_keys_max_chunk_chars: 2048
rust_core:
  runtime_enabled: true
"#,
    )
    .unwrap();
    let config =
        AppConfig::load_from_path_with_local_env(&config_path, Some(&missing_env_path)).unwrap();
    assert_eq!(
        config.rust_core.tmux_socket_name.as_deref(),
        Some("session-manager")
    );
    assert_eq!(config.rust_core.send_keys_settle_ms, Some(250.0));
    assert_eq!(config.rust_core.send_keys_settle_max_ms, Some(1250.0));
    assert_eq!(config.rust_core.send_keys_settle_per_ki_ms, Some(70.0));
    assert_eq!(
        config.rust_core.send_keys_settle_per_extra_line_ms,
        Some(20.0)
    );
    assert_eq!(config.rust_core.send_keys_max_chunk_chars, Some(2048));

    fs::write(
        &config_path,
        r#"
tmux:
  socket_name: "session-manager"
rust_core:
  runtime_enabled: true
  tmux_socket_name: "rust-core-only"
"#,
    )
    .unwrap();
    let config =
        AppConfig::load_from_path_with_local_env(&config_path, Some(&missing_env_path)).unwrap();
    assert_eq!(
        config.rust_core.tmux_socket_name.as_deref(),
        Some("rust-core-only")
    );
}

#[tokio::test]
async fn fixture_core_lifecycle_creates_sends_outputs_and_retires() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "rustcore",
            "name": "rust-core",
            "working_dir": "/repo",
            "provider": "claude",
            "initial_message": "initial fixture prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "rustcore");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["friendly_name"], "rust-core");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "rustchild",
            "name": "rust-child",
            "parent_session_id": "rustcore",
            "provider": "claude",
            "initial_message": "child fixture prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "rustchild");
    assert_eq!(payload["parent_session_id"], "rustcore");
    assert_eq!(payload["working_dir"], "/repo");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/rustcore/input",
        json!({
            "text": "hello from rust fixture",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    assert_eq!(payload["delivery_mode"], "sequential");
    assert_eq!(payload["notify_after_seconds"], Value::Null);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/rustcore/input",
        json!({
            "text": "urgent fixture note",
            "delivery_mode": "urgent",
            "notify_after_seconds": 7
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    assert_eq!(payload["delivery_mode"], "urgent");
    assert_eq!(payload["notify_after_seconds"], 7);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/rustcore/agent-status",
        json!({
            "text": "writing Rust status"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "updated");
    assert_eq!(payload["agent_status_text"], "writing Rust status");

    let (status, payload) = get_json(app.clone(), "/sessions/rustcore/output?lines=4").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["output"]
        .as_str()
        .unwrap()
        .contains("hello from rust fixture"));

    let (status, payload) = get_json(app.clone(), "/sessions/rustcore").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["agent_status_text"], "writing Rust status");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/rustchild/kill",
        json!({ "requester_session_id": "otherparent" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload["error"],
        "Cannot kill session rustchild - not your child session"
    );

    let (status, payload) = get_json(app.clone(), "/sessions/rustchild").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "running");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/rustchild/kill",
        json!({ "requester_session_id": "rustcore" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert_eq!(payload["session_id"], "rustchild");

    let (status, payload) = get_json(app.clone(), "/sessions/rustchild").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "stopped");

    let (status, payload) = post_json(app.clone(), "/sessions/missingcore/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["error"], "Session missingcore not found");

    let (status, payload) = post_json(app.clone(), "/sessions/rustcore/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");

    let (status, payload) = get_json(app, "/sessions?include_stopped=true").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["sessions"][0]["id"], "rustcore");
    assert_eq!(payload["sessions"][0]["status"], "stopped");
}

#[tokio::test]
async fn fixture_core_session_graph_endpoints_round_trip_state() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "graphparent",
            "name": "graph-parent",
            "working_dir": "/repo",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "graphchild",
            "name": "graph-child",
            "parent_session_id": "graphparent",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "graphgrandchild",
            "name": "graph-grandchild",
            "parent_session_id": "graphchild",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "graphgreatgrandchild",
            "name": "graph-great-grandchild",
            "parent_session_id": "graphgrandchild",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, payload) = get_json(app.clone(), "/sessions/graphparent/children").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["parent_session_id"], "graphparent");
    assert_eq!(payload["children"].as_array().unwrap().len(), 1);
    assert_eq!(payload["children"][0]["id"], "graphchild");
    assert_eq!(payload["children"][0]["friendly_name"], "graph-child");

    let (status, payload) =
        get_json(app.clone(), "/sessions/graphparent/children?recursive=true").await;
    assert_eq!(status, StatusCode::OK);
    let child_ids = payload["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|child| child["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        child_ids,
        vec!["graphchild", "graphgrandchild", "graphgreatgrandchild"]
    );

    let (status, payload) = get_json(app.clone(), "/sessions/graphchild/attach-descriptor").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["attach"]["attach_supported"], true);
    assert_eq!(payload["attach"]["tmux_session"], "sm-rust-graphchild");

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "graphcodexapp",
            "name": "graph-codex-app",
            "provider": "codex-app"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, payload) =
        get_json(app.clone(), "/sessions/graphcodexapp/attach-descriptor").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["attach"]["attach_supported"], false);
    assert_eq!(
        payload["attach"]["message"],
        "Attach not supported for Codex app sessions"
    );

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/graphchild/context-monitor",
        json!({
            "enabled": true,
            "requester_session_id": "graphparent",
            "notify_session_id": "graphparent"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "status": "ok", "enabled": true }));

    let (status, payload) = get_json(app.clone(), "/sessions/context-monitor").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["monitored"][0]["session_id"], "graphchild");
    assert_eq!(payload["monitored"][0]["notify_session_id"], "graphparent");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/graphchild/agent-status",
        json!({ "text": "old task state" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["agent_status_text"], "old task state");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/graphparent/clear",
        json!({ "prompt": "root reset denied" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "Can only clear child sessions. Target session has no parent."
    );

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/graphchild/clear",
        json!({
            "prompt": "sibling reset denied",
            "requester_session_id": "graphgrandchild"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "Not authorized. You can only clear your child sessions. Target session parent: graphparent"
    );

    let mut state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let child_entry = state["sessions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|session| session["id"] == "graphchild")
        .unwrap();
    child_entry["completion_status"] = json!("completed");
    child_entry["completion_message"] = json!("stale completed message");
    child_entry["completed_at"] = json!("2026-06-01T00:02:00Z");
    child_entry["agent_task_completed_at"] = json!("2026-06-01T00:03:00Z");
    fs::write(&state_file, state.to_string()).unwrap();

    let (status, payload) = get_json(
        app.clone(),
        "/sessions/graphparent/children?status=completed",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["children"].as_array().unwrap().len(), 1);
    assert_eq!(payload["children"][0]["completion_status"], "completed");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/graphchild/clear",
        json!({ "prompt": "new task after clear" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({ "status": "cleared", "session_id": "graphchild" })
    );

    let (status, payload) = get_json(app.clone(), "/sessions/graphchild").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["agent_status_text"], Value::Null);

    let (status, payload) = get_json(app.clone(), "/sessions/graphparent/children").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["children"][0]["completion_status"], Value::Null);
    assert_eq!(payload["children"][0]["completion_message"], Value::Null);

    let (status, payload) = get_json(
        app.clone(),
        "/sessions/graphparent/children?status=completed",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["children"].as_array().unwrap().len(), 0);

    let (status, payload) = get_json(app.clone(), "/sessions/graphchild/output?lines=5").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["output"]
        .as_str()
        .unwrap()
        .contains("new task after clear"));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/graphchild/handoff",
        json!({
            "requester_session_id": "graphchild",
            "file_path": "/tmp/handoff.md"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "status": "recorded" }));

    let (status, payload) = get_json(app.clone(), "/sessions/graphchild").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["last_handoff_path"], Value::Null);
    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let graph_child = raw_state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "graphchild")
        .unwrap();
    assert_eq!(graph_child["pending_handoff_path"], "/tmp/handoff.md");

    let (status, payload) = post_json(app.clone(), "/sessions/graphchild/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");

    let (status, payload) = get_json(app.clone(), "/sessions/graphparent/children").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["children"]
        .as_array()
        .unwrap()
        .iter()
        .all(|child| child["id"] != "graphchild"));

    let (status, payload) = get_json(
        app.clone(),
        "/sessions/graphparent/children?include_terminated=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let graphchild = payload["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|child| child["id"] == "graphchild")
        .unwrap();
    assert_eq!(graphchild["completion_status"], "killed");
    assert_eq!(graphchild["completion_message"], "Terminated via sm kill");

    let (status, payload) = get_json(app.clone(), "/sessions/graphchild/attach-descriptor").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["attach"]["attach_supported"], false);
    assert_eq!(payload["attach"]["message"], "Session is stopped");

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let child = state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "graphchild")
        .unwrap()
        .clone();
    let mut state = state;
    let sessions = state["sessions"].as_array_mut().unwrap();
    let child_entry = sessions
        .iter_mut()
        .find(|session| session["id"] == "graphchild")
        .unwrap();
    child_entry["completion_status"] = json!("killed");
    child_entry["completion_message"] = json!("stale killed message");
    child_entry["completed_at"] = json!("2026-06-01T00:02:00Z");
    child_entry["agent_task_completed_at"] = json!("2026-06-01T00:03:00Z");
    assert_ne!(child, *child_entry);
    fs::write(&state_file, state.to_string()).unwrap();

    let (status, payload) = post_json(app.clone(), "/sessions/graphchild/restore", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "graphchild");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["stopped_at"], Value::Null);
    assert_eq!(payload["completion_status"], Value::Null);
    assert_eq!(payload["completion_message"], Value::Null);
    assert_eq!(payload["completed_at"], Value::Null);
    assert_eq!(payload["agent_task_completed_at"], Value::Null);

    let (status, payload) = post_json(app, "/sessions/graphchild/restore", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(payload, json!({ "detail": "Session is not stopped" }));
}

#[tokio::test]
async fn fixture_completion_endpoints_preserve_python_compatible_state() {
    let state_file = write_completion_fixture();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let mut config = config_with_state_file_and_queue(&state_file);
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/task-complete",
        json!({ "requester_session_id": "other001" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["error"].as_str().unwrap().contains("self-directed"));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/task-complete",
        json!({ "requester_session_id": "child001" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "completed");
    assert_eq!(payload["session_id"], "child001");
    assert_eq!(payload["em_notified"], true);
    assert!(payload["agent_task_completed_at"].is_string());

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let child = state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "child001")
        .unwrap();
    assert!(child["agent_task_completed_at"].is_string());
    assert_eq!(
        state["retained_remind_registrations"][0]["is_active"],
        false
    );
    assert_eq!(
        state["retained_parent_wake_registrations"][0]["is_active"],
        false
    );
    assert_eq!(
        state["retained_pending_messages"][0]["text"],
        "[sm task-complete] agent child001(worker-1) completed its task."
    );
    assert_eq!(
        state["retained_pending_messages"][0]["target_session_id"],
        "em001"
    );
    assert_eq!(
        state["retained_pending_messages"][0]["delivery_mode"],
        "important"
    );
    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let queued_message: (String, String, String) = queue_conn
        .query_row(
            "SELECT target_session_id, text, delivery_mode FROM message_queue WHERE message_category = 'task_complete'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        queued_message,
        (
            "em001".to_owned(),
            "[sm task-complete] agent child001(worker-1) completed its task.".to_owned(),
            "important".to_owned()
        )
    );

    let state_file = write_completion_fixture();
    let mut config = config_with_state_file_and_queue(&state_file);
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));
    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/turn-complete",
        json!({ "requester_session_id": "child001" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({ "status": "turn_completed", "session_id": "child001" })
    );

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let child = state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "child001")
        .unwrap();
    assert_eq!(child["agent_task_completed_at"], Value::Null);
    assert_eq!(
        state["retained_remind_registrations"][0]["is_active"],
        false
    );
    assert_eq!(
        state["retained_parent_wake_registrations"][0]["is_active"],
        true
    );
}

#[tokio::test]
async fn fixture_notify_on_stop_preserves_authorization_and_state_contract() {
    let state_file = write_completion_fixture();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let mut config = config_with_state_file_and_queue(&state_file);
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/missing/notify-on-stop",
        json!({ "sender_session_id": "em001", "requester_session_id": "em001" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Session not found");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/notify-on-stop",
        json!({ "sender_session_id": "child001", "requester_session_id": "child001" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "Only EM sessions (is_em=True) may arm stop notifications"
    );

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/notify-on-stop",
        json!({ "sender_session_id": "em002", "requester_session_id": "em002" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "Cannot arm stop notify — not the parent of target session"
    );

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/notify-on-stop",
        json!({ "sender_session_id": "missing", "requester_session_id": "em001" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(payload["detail"], "sender_session_id \"missing\" not found");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/fork001/notify-on-stop",
        json!({ "sender_session_id": "em001", "requester_session_id": "em001" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "suppressed");
    assert_eq!(
        payload["reason"],
        "notify_on_stop disabled for codex-fork sessions"
    );

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/notify-on-stop",
        json!({
            "sender_session_id": "em001",
            "requester_session_id": "em001",
            "delay_seconds": 8
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({ "status": "ok", "session_id": "child001", "sender_session_id": "em001" })
    );

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(
        state["retained_stop_notify_states"][0]["session_id"],
        "child001"
    );
    assert_eq!(
        state["retained_stop_notify_states"][0]["sender_session_id"],
        "em001"
    );
    assert_eq!(state["retained_stop_notify_states"][0]["sender_name"], "em");
    assert_eq!(state["retained_stop_notify_states"][0]["delay_seconds"], 8);
    assert_eq!(
        state["retained_stop_notify_states"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let stop_notify: (String, String, i64) = queue_conn
        .query_row(
            "SELECT sender_session_id, sender_name, delay_seconds FROM rust_stop_notify_states WHERE session_id = 'child001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(stop_notify, ("em001".to_owned(), "em".to_owned(), 8));
}

#[tokio::test]
async fn fixture_registry_and_maintainer_endpoints_round_trip_state() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "registryowner",
            "name": "registry-owner",
            "working_dir": "/repo",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "otherowner",
            "name": "other-owner",
            "working_dir": "/repo",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, payload) = put_json(
        app.clone(),
        "/sessions/registryowner/maintainer",
        json!({ "requester_session_id": "otherowner" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(payload["detail"], "sm maintainer is self-directed only");

    let (status, payload) = put_json(
        app.clone(),
        "/sessions/registryowner/maintainer",
        json!({ "requester_session_id": "registryowner" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "registryowner");
    assert_eq!(payload["aliases"], json!(["maintainer"]));
    assert_eq!(payload["is_maintainer"], true);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/registryowner/registry",
        json!({
            "requester_session_id": "registryowner",
            "role": "Review Owner"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["role"], "review-owner");
    assert_eq!(payload["session_id"], "registryowner");
    assert_eq!(payload["friendly_name"], "maintainer");
    assert_eq!(payload["provider"], "claude");
    assert_eq!(payload["activity_state"], "working");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/otherowner/registry",
        json!({
            "requester_session_id": "otherowner",
            "role": "review-owner"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        payload["detail"],
        "Role \"review-owner\" is already registered to registryowner"
    );

    let (status, payload) = get_json(app.clone(), "/registry/review-owner").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["role"], "review-owner");
    assert_eq!(payload["session_id"], "registryowner");

    let (status, payload) = get_json(app.clone(), "/registry").await;
    assert_eq!(status, StatusCode::OK);
    let roles = payload["registrations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["role"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(roles, vec!["maintainer", "review-owner"]);

    let (status, payload) = get_json(app.clone(), "/sessions/registryowner").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["aliases"], json!(["maintainer", "review-owner"]));

    let (status, payload) = delete_json(
        app.clone(),
        "/sessions/otherowner/registry",
        json!({
            "requester_session_id": "otherowner",
            "role": "review-owner"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(payload["detail"], "Role is not owned by this session");

    let (status, payload) = delete_json(
        app.clone(),
        "/sessions/registryowner/registry",
        json!({
            "requester_session_id": "registryowner",
            "role": "review-owner"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["role"], "review-owner");
    assert_eq!(payload["session_id"], "registryowner");

    let (status, payload) = get_json(app.clone(), "/registry/review-owner").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Role not registered" }));

    let (status, payload) = delete_json(
        app.clone(),
        "/sessions/registryowner/maintainer",
        json!({ "requester_session_id": "registryowner" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "registryowner");
    assert_eq!(payload["aliases"], json!([]));
    assert_eq!(payload["is_maintainer"], false);
}

#[tokio::test]
async fn fixture_registry_prunes_stale_roles_and_updates_maintainer_alias() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "liveagent",
                    "name": "claude-liveagent",
                    "working_dir": "/repo",
                    "tmux_session": "claude-liveagent",
                    "log_file": "/tmp/liveagent.log",
                    "status": "running",
                    "provider": "claude",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                },
                {
                    "id": "restorable",
                    "name": "claude-restorable",
                    "working_dir": "/repo",
                    "tmux_session": "claude-restorable",
                    "log_file": "/tmp/restorable.log",
                    "status": "stopped",
                    "provider": "claude",
                    "provider_resume_id": "resume-restorable",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "stopped_at": "2026-06-01T00:02:00"
                },
                {
                    "id": "staleagent",
                    "name": "claude-staleagent",
                    "working_dir": "/repo",
                    "tmux_session": "claude-staleagent",
                    "log_file": "/tmp/staleagent.log",
                    "status": "stopped",
                    "provider": "claude",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "stopped_at": "2026-06-01T00:02:00"
                }
            ],
            "maintainer_session_id": "staleagent",
            "agent_registrations": [
                {
                    "role": "Live Role",
                    "session_id": "liveagent",
                    "created_at": "2026-06-01T00:03:00"
                },
                {
                    "role": "Restorable Role",
                    "session_id": "restorable",
                    "created_at": "2026-06-01T00:03:01"
                },
                {
                    "role": "Stale Role",
                    "session_id": "staleagent",
                    "created_at": "2026-06-01T00:03:02"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app.clone(), "/registry").await;
    assert_eq!(status, StatusCode::OK);
    let roles = payload["registrations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["role"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(roles, vec!["live-role", "restorable-role"]);

    let (status, payload) = get_json(app.clone(), "/registry/stale-role").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Role not registered" }));

    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(raw_state["maintainer_session_id"], Value::Null);
    assert_eq!(
        raw_state["agent_role_last_session_ids"]["stale-role"],
        "staleagent"
    );
    assert!(raw_state["agent_registrations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["role"] != "stale-role"));
}

#[tokio::test]
async fn fixture_registry_clears_stale_legacy_maintainer_without_registration() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "staleonly",
                    "name": "claude-staleonly",
                    "working_dir": "/repo",
                    "tmux_session": "claude-staleonly",
                    "log_file": "/tmp/staleonly.log",
                    "status": "stopped",
                    "provider": "claude",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "stopped_at": "2026-06-01T00:02:00"
                }
            ],
            "maintainer_session_id": "staleonly",
            "agent_registrations": []
        })
        .to_string(),
    )
    .unwrap();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/registry").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "registrations": [] }));

    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(raw_state["maintainer_session_id"], Value::Null);
    assert_eq!(
        raw_state["agent_role_last_session_ids"]["maintainer"],
        "staleonly"
    );
}

#[tokio::test]
async fn fixture_registry_clear_removes_recovered_maintainer_history() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "maintainer1",
                    "name": "claude-maintainer1",
                    "working_dir": "/repo",
                    "tmux_session": "claude-maintainer1",
                    "log_file": "/tmp/maintainer1.log",
                    "status": "running",
                    "provider": "claude",
                    "friendly_name": "maintainer",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ],
            "maintainer_session_id": null,
            "agent_role_last_session_ids": {
                "maintainer": "maintainer1"
            },
            "agent_registrations": []
        })
        .to_string(),
    )
    .unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app.clone(), "/registry").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["registrations"][0]["role"], "maintainer");
    assert_eq!(payload["registrations"][0]["session_id"], "maintainer1");

    let (status, payload) = delete_json(
        app.clone(),
        "/sessions/maintainer1/maintainer",
        json!({ "requester_session_id": "maintainer1" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["aliases"], json!([]));
    assert_eq!(payload["is_maintainer"], false);

    let (status, payload) = get_json(app.clone(), "/registry").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "registrations": [] }));
    let (status, payload) = get_json(app, "/registry/maintainer").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Role not registered" }));

    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(raw_state["maintainer_session_id"], Value::Null);
    assert!(raw_state["agent_role_last_session_ids"]
        .as_object()
        .is_some_and(|last| !last.contains_key("maintainer")));
}

#[tokio::test]
async fn shadow_attach_descriptor_reuses_real_attach_support_rules() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));
    let expected_body = serde_json::to_vec(&json!({
        "attach": {
            "session_id": "stop1234",
            "provider": "claude",
            "attach_supported": false,
            "tmux_session": "claude-stop1234",
            "tmux_socket_name": null,
            "runtime_id": null,
            "lifecycle_state": "stopped",
            "message": "Session is stopped"
        }
    }))
    .unwrap();

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/stop1234/attach-descriptor",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(&expected_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "match");
    assert_eq!(payload["body_sha256_match"], true);
}

#[tokio::test]
async fn fixture_core_spawn_endpoint_inherits_parent_fields() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let parent_dir = unique_temp_path();
    fs::create_dir_all(&parent_dir).unwrap();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "parentfixture",
                    "name": "claude-parentfixture",
                    "working_dir": parent_dir.display().to_string(),
                    "tmux_session": "claude-parentfixture",
                    "node": "primary",
                    "provider": "claude",
                    "log_file": "/tmp/parentfixture.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ],
            "agent_registrations": [
                {
                    "role": "parent-alias",
                    "session_id": "parentfixture"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "trackedchildfixture",
            "parent_session_id": "parentfixture",
            "prompt": "tracked child fixture prompt",
            "name": "tracked-child-fixture",
            "track_seconds": 300
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload,
        json!({ "detail": "Rust core spawn does not support track_seconds yet" })
    );
    let (status, _payload) = get_json(app.clone(), "/sessions/trackedchildfixture").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "childfixture",
            "parent_session_id": "parent-alias",
            "prompt": "child fixture prompt",
            "name": "child-fixture"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "childfixture");
    assert_eq!(payload["friendly_name"], "child-fixture");
    assert_eq!(payload["parent_session_id"], "parentfixture");
    assert_eq!(payload["working_dir"], parent_dir.display().to_string());
    assert_eq!(payload["node"], "primary");
    assert_eq!(payload["provider"], "claude");

    let (status, payload) = get_json(app, "/sessions/childfixture").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["parent_session_id"], "parentfixture");
    assert_eq!(payload["working_dir"], parent_dir.display().to_string());
}

#[tokio::test]
async fn fixture_core_spawn_auto_arms_em_stop_notify_for_retained_children() {
    let state_file = write_completion_fixture();
    let log_dir = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let mut config = config_with_state_file_and_queue(&state_file);
    config.rust_core.fixture_writes_enabled = true;
    config.rust_core.log_dir = Some(log_dir.display().to_string());
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "spawnchild",
            "parent_session_id": "em001",
            "prompt": "spawn child prompt",
            "name": "spawn-child",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "spawnchild");

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let stop_notify = state["retained_stop_notify_states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["session_id"] == "spawnchild")
        .unwrap();
    assert_eq!(stop_notify["sender_session_id"], "em001");
    assert_eq!(stop_notify["sender_name"], "em");
    assert_eq!(stop_notify["delay_seconds"], 8);
    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let db_stop_notify: (String, String, i64) = queue_conn
        .query_row(
            "SELECT sender_session_id, sender_name, delay_seconds FROM rust_stop_notify_states WHERE session_id = 'spawnchild'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(db_stop_notify, ("em001".to_owned(), "em".to_owned(), 8));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "spawnfork",
            "parent_session_id": "em001",
            "prompt": "spawn codex fork prompt",
            "name": "spawn-fork",
            "provider": "codex-fork"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "spawnfork");

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert!(state["retained_stop_notify_states"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["session_id"] != "spawnfork"));
}

#[tokio::test]
async fn fixture_core_writes_preserve_concurrent_session_updates() {
    let state_file = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(unique_temp_path().display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let first = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "rustcore1",
            "name": "rust-core-1",
            "working_dir": "/repo",
            "provider": "claude"
        }),
    );
    let second = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "rustcore2",
            "name": "rust-core-2",
            "working_dir": "/repo",
            "provider": "claude"
        }),
    );

    let ((status1, _payload1), (status2, _payload2)) = join(first, second).await;
    assert_eq!(status1, StatusCode::OK);
    assert_eq!(status2, StatusCode::OK);

    let (status, payload) = get_json(app, "/sessions").await;
    assert_eq!(status, StatusCode::OK);
    let mut ids = payload["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(ids, vec!["rustcore1", "rustcore2"]);
}

#[tokio::test]
async fn fixture_core_logs_do_not_collide_for_sanitized_session_ids() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, _first) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "ab",
            "name": "rust-core-ab",
            "initial_message": "first prompt only"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _second) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "a/b",
            "name": "rust-core-slash",
            "initial_message": "second prompt only"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let log_files = state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["log_file"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(log_files.len(), 2);
    assert_ne!(log_files[0], log_files[1]);

    let (status, payload) = get_json(app, "/sessions/ab/output?lines=10").await;
    assert_eq!(status, StatusCode::OK);
    let output = payload["output"].as_str().unwrap();
    assert!(output.contains("first prompt only"));
    assert!(!output.contains("second prompt only"));
}

#[tokio::test]
async fn runtime_core_lifecycle_uses_tmux_backend_when_enabled() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db_path_for_state_file(&state_file)
                .display()
                .to_string(),
        },
        rust_core: RustCoreConfig {
            runtime_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            tmux_socket_name: Some(tmux_socket.clone()),
            runtime_command: Some(
                r#"/bin/sh -lc 'while IFS= read -r line; do printf "argv:%s\nids:%s:%s:%s\nruntime:%s\n" "$*" "$SESSION_MANAGER_ID" "$CLAUDE_SESSION_MANAGER_ID" "$ENABLE_TOOL_SEARCH" "$line"; done' runtime-sh"#
                    .to_owned(),
            ),
            runtime_prompt_mode: Some("stdin".to_owned()),
            runtime_start_settle_ms: Some(100),
            send_keys_settle_ms: Some(10.0),
            send_keys_settle_max_ms: Some(50.0),
            send_keys_max_chunk_chars: Some(128),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimecore",
            "name": "runtime-core",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "initial runtime prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "runtimecore");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["tmux_socket_name"], tmux_socket);
    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();

    let payload =
        wait_for_output_contains(app.clone(), "runtimecore", "runtime:initial runtime prompt")
            .await;
    assert_eq!(payload["session_id"], "runtimecore");
    wait_for_output_contains(
        app.clone(),
        "runtimecore",
        "ids:runtimecore:runtimecore:false",
    )
    .await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimecore/input",
        json!({
            "text": "second runtime message",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);

    wait_for_output_contains(app.clone(), "runtimecore", "runtime:second runtime message").await;
    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let delivered_at: Option<String> = queue_conn
        .query_row(
            "SELECT delivered_at FROM message_queue WHERE target_session_id = 'runtimecore' AND text = 'second runtime message'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(delivered_at.is_some());

    let long_runtime_message = format!("{}chunked-runtime-tail", "x".repeat(500));
    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimecore/input",
        json!({
            "text": long_runtime_message,
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(app.clone(), "runtimecore", "chunked-runtime-tail").await;

    assert!(tmux_enter_copy_mode(&tmux_socket, &tmux_session));
    assert_eq!(tmux_pane_in_mode(&tmux_socket, &tmux_session), Some(1));
    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimecore/input",
        json!({
            "text": "copy mode runtime message",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(
        app.clone(),
        "runtimecore",
        "runtime:copy mode runtime message",
    )
    .await;

    let (status, payload) = post_json(app.clone(), "/sessions/runtimecore/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");

    let (status, payload) = get_json(app.clone(), "/sessions/runtimecore").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "stopped");
    assert!(!tmux_session_exists(&tmux_socket, &tmux_session));

    let (status, payload) =
        post_json(app.clone(), "/sessions/runtimecore/restore", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "runtimecore");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["completion_status"], Value::Null);
    assert!(tmux_session_exists(&tmux_socket, &tmux_session));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimecore/input",
        json!({
            "text": "restored runtime message",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(app, "runtimecore", "runtime:restored runtime message").await;
}

#[tokio::test]
async fn runtime_core_delivers_sm_send_metadata_rows() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-side-effects-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimesmsend",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "sm send initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimesender",
            "name": "runtime-sender",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "sender initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_output_contains(app.clone(), "runtimesmsend", "runtime:sm send initial").await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimesmsend/input",
        json!({
            "text": "ordinary sm send metadata delivered",
            "sender_session_id": "runtimesender",
            "delivery_mode": "sequential",
            "from_sm_send": true,
            "timeout_seconds": 60,
            "notify_on_stop": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    assert_eq!(payload["status"], "running");
    wait_for_output_contains(
        app.clone(),
        "runtimesmsend",
        "runtime:ordinary sm send metadata delivered",
    )
    .await;

    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let pending: (
        String,
        Option<String>,
        Option<String>,
        i64,
        Option<String>,
        i64,
        Option<i64>,
        i64,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = queue_conn
        .query_row(
            r#"
            SELECT text, sender_session_id, sender_name, from_sm_send, timeout_at,
                   notify_on_delivery, notify_after_seconds, notify_on_stop,
                   remind_soft_threshold, remind_hard_threshold,
                   remind_cancel_on_reply_session_id, parent_session_id,
                   response_relay_source, delivered_at
            FROM message_queue
            WHERE target_session_id = 'runtimesmsend'
            "#,
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                ))
            },
        )
        .unwrap();
    let timeout_at = pending.4.clone();
    assert!(timeout_at.is_some());
    assert_eq!(
        pending.0,
        "[Input from: runtime-sender (runtimes) via sm send]\nordinary sm send metadata delivered"
    );
    assert_eq!(pending.1.as_deref(), Some("runtimesender"));
    assert_eq!(pending.2.as_deref(), Some("runtime-sender"));
    assert_eq!(pending.3, 1);
    assert_eq!(pending.4, timeout_at);
    assert_eq!(pending.5, 0);
    assert_eq!(pending.6, None);
    assert_eq!(pending.7, 0);
    assert_eq!(pending.8, None);
    assert_eq!(pending.9, None);
    assert_eq!(pending.10, None);
    assert_eq!(pending.11, None);
    assert_eq!(pending.12.as_deref(), Some("sm-send"));
    assert!(pending.13.is_some());
}

#[tokio::test]
async fn runtime_core_materializes_send_delivery_side_effects() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-side-effects-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimeem",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "em side effect initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimechild",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "parent_session_id": "runtimeem",
            "initial_message": "child side effect initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut raw_state: Value =
        serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let em = raw_state["sessions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|session| session["id"] == "runtimeem")
        .unwrap();
    em["is_em"] = Value::Bool(true);
    em["friendly_name"] = Value::String("runtime-em".to_owned());
    fs::write(
        &state_file,
        serde_json::to_string_pretty(&raw_state).unwrap(),
    )
    .unwrap();
    wait_for_output_contains(
        app.clone(),
        "runtimechild",
        "runtime:child side effect initial",
    )
    .await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimechild/input",
        json!({
            "text": "side effect delivery should be materialized",
            "delivery_mode": "sequential",
            "sender_session_id": "runtimeem",
            "from_sm_send": true,
            "notify_on_delivery": true,
            "notify_after_seconds": 1,
            "notify_on_stop": true,
            "remind_soft_threshold": 30,
            "remind_hard_threshold": 45,
            "remind_cancel_on_reply_session_id": "runtimeem",
            "parent_session_id": "runtimeem"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(
        app.clone(),
        "runtimechild",
        "runtime:side effect delivery should be materialized",
    )
    .await;

    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let original: (
        Option<String>,
        i64,
        Option<i64>,
        i64,
        Option<i64>,
        Option<String>,
    ) = queue_conn
        .query_row(
            r#"
                SELECT delivered_at, notify_on_delivery, notify_after_seconds,
                       notify_on_stop, remind_soft_threshold, parent_session_id
                FROM message_queue
                WHERE target_session_id = 'runtimechild'
                "#,
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert!(original.0.is_some());
    assert_eq!(original.1, 1);
    assert_eq!(original.2, Some(1));
    assert_eq!(original.3, 1);
    assert_eq!(original.4, Some(30));
    assert_eq!(original.5.as_deref(), Some("runtimeem"));

    let delivery_notification_count: i64 = queue_conn
        .query_row(
            "SELECT COUNT(*) FROM message_queue WHERE target_session_id = 'runtimeem' AND text LIKE '[sm] Message delivered%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(delivery_notification_count, 1);

    tokio::time::sleep(Duration::from_millis(1200)).await;
    let followup_count: i64 = queue_conn
        .query_row(
            "SELECT COUNT(*) FROM message_queue WHERE target_session_id = 'runtimeem' AND text LIKE '[sm] Reminder: 1s since%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(followup_count, 1);

    let remind: (i64, i64, Option<String>, i64) = queue_conn
        .query_row(
            "SELECT soft_threshold_seconds, hard_threshold_seconds, cancel_on_reply_session_id, is_active FROM remind_registrations WHERE target_session_id = 'runtimechild'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(remind, (30, 45, Some("runtimeem".to_owned()), 1));
    let parent_wake: (String, i64, i64) = queue_conn
        .query_row(
            "SELECT parent_session_id, period_seconds, is_active FROM parent_wake_registrations WHERE child_session_id = 'runtimechild'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(parent_wake, ("runtimeem".to_owned(), 600, 1));
    let stop_notify: (String, String, i64) = queue_conn
        .query_row(
            "SELECT sender_session_id, sender_name, delay_seconds FROM rust_stop_notify_states WHERE session_id = 'runtimechild'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        stop_notify,
        ("runtimeem".to_owned(), "runtime-em".to_owned(), 0)
    );

    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(
        raw_state["retained_stop_notify_states"][0]["sender_session_id"],
        "runtimeem"
    );
    assert_eq!(
        raw_state["retained_remind_registrations"][0]["target_session_id"],
        "runtimechild"
    );
    assert_eq!(
        raw_state["retained_parent_wake_registrations"][0]["parent_session_id"],
        "runtimeem"
    );
}

#[tokio::test]
async fn runtime_core_priority_sends_bypass_sequential_queue_backlog() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-priority-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimepriority",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "priority initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_output_contains(app.clone(), "runtimepriority", "runtime:priority initial").await;

    let queue = RetainedQueueStore::new(queue_db_path.clone());
    for index in 0..12 {
        queue
            .enqueue_message(
                "runtimepriority",
                &format!("stale sequential backlog {index}"),
                "sequential",
                None,
            )
            .unwrap();
    }

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimepriority/input",
        json!({
            "text": "sequential after large backlog",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(
        app.clone(),
        "runtimepriority",
        "runtime:sequential after large backlog",
    )
    .await;

    for index in 0..12 {
        queue
            .enqueue_message(
                "runtimepriority",
                &format!("second stale sequential backlog {index}"),
                "sequential",
                None,
            )
            .unwrap();
    }

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimepriority/input",
        json!({
            "text": "important priority message",
            "delivery_mode": "important"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(
        app.clone(),
        "runtimepriority",
        "runtime:important priority message",
    )
    .await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimepriority/input",
        json!({
            "text": "urgent priority message",
            "delivery_mode": "urgent"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(app.clone(), "runtimepriority", "urgent priority message").await;

    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let delivered_priority_count: i64 = queue_conn
        .query_row(
            r#"
            SELECT COUNT(*)
            FROM message_queue
            WHERE target_session_id = 'runtimepriority'
                AND text IN (
                    'sequential after large backlog',
                    'important priority message',
                    'urgent priority message'
                )
                AND delivered_at IS NOT NULL
            "#,
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(delivered_priority_count, 3);
    let delivered_backlog_count: i64 = queue_conn
        .query_row(
            r#"
            SELECT COUNT(*)
            FROM message_queue
            WHERE target_session_id = 'runtimepriority'
                AND text LIKE 'second stale sequential backlog%'
                AND delivered_at IS NOT NULL
            "#,
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(delivered_backlog_count, 0);
}

#[tokio::test]
async fn runtime_core_replays_retained_urgent_rows_with_interrupt_semantics() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-retained-urgent-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimeurgentreplay",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "urgent replay initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_output_contains(
        app.clone(),
        "runtimeurgentreplay",
        "runtime:urgent replay initial",
    )
    .await;

    let queue = RetainedQueueStore::new(queue_db_path.clone());
    let urgent_id = queue
        .enqueue_message(
            "runtimeurgentreplay",
            "retained urgent replay",
            "urgent",
            None,
        )
        .unwrap();

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimeurgentreplay/input",
        json!({
            "text": "sequential trigger after retained urgent",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    let payload = wait_for_output_contains(
        app.clone(),
        "runtimeurgentreplay",
        "sequential trigger after retained urgent",
    )
    .await;
    let output = payload["output"].as_str().unwrap_or_default();
    assert!(
        output.contains("runtime:\u{2}\u{1b}retained urgent replay"),
        "retained urgent row was not replayed through the interrupt path: {output:?}",
    );
    assert!(queue.message_delivered(&urgent_id).unwrap());
}

#[tokio::test]
async fn runtime_core_handoff_records_without_interrupting_active_turn() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-handoff-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket);
    let app = runtime_app_with_command(
        &state_file,
        &log_dir,
        _tmux_guard.0.as_str(),
        r#"/bin/sh -lc 'printf ">\n"; while IFS= read -r line; do printf "runtime:%s\n>\n" "$line"; done' runtime-sh"#,
    );

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimehandoff",
            "name": "runtime-handoff",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "initial handoff prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_output_contains(
        app.clone(),
        "runtimehandoff",
        "runtime:initial handoff prompt",
    )
    .await;

    let handoff_path = unique_temp_path();
    fs::write(&handoff_path, "handoff body").unwrap();
    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimehandoff/handoff",
        json!({
            "requester_session_id": "runtimehandoff",
            "file_path": handoff_path.display().to_string()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "recorded");

    let (status, output) = get_json(app.clone(), "/sessions/runtimehandoff/output?lines=20").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!output["output"]
        .as_str()
        .unwrap()
        .contains("continue from where you left off"));

    let (status, payload) = get_json(app, "/sessions/runtimehandoff").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["last_handoff_path"], Value::Null);
    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let runtime_handoff = raw_state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "runtimehandoff")
        .unwrap();
    assert_eq!(
        runtime_handoff["pending_handoff_path"],
        handoff_path.display().to_string()
    );
}

#[tokio::test]
async fn runtime_core_spawn_endpoint_uses_tmux_and_parent_fields() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let parent_dir = unique_temp_path();
    fs::create_dir_all(&parent_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-spawn-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, parent_payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimeparent",
            "name": "runtime-parent",
            "working_dir": parent_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "parent runtime prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parent_tmux_session = parent_payload["tmux_session"].as_str().unwrap().to_owned();
    wait_for_output_contains(
        app.clone(),
        "runtimeparent",
        "runtime:parent runtime prompt",
    )
    .await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "runtimechild",
            "parent_session_id": "runtimeparent",
            "prompt": "spawn endpoint runtime prompt",
            "name": "runtime-child",
            "model": "opus",
            "wait": 5
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "runtimechild");
    assert_eq!(payload["friendly_name"], "runtime-child");
    assert_eq!(payload["parent_session_id"], "runtimeparent");
    assert_eq!(payload["working_dir"], parent_dir.display().to_string());
    assert_eq!(payload["node"], "primary");
    assert_eq!(payload["provider"], "claude");
    assert_eq!(payload["model"], "opus");
    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();
    assert!(tmux_session.starts_with("sm-rust-claude-runtimechild-"));

    wait_for_output_contains(
        app.clone(),
        "runtimechild",
        "runtime:spawn endpoint runtime prompt",
    )
    .await;
    wait_for_output_contains(app.clone(), "runtimechild", "argv:--model opus").await;

    let (status, payload) = get_json(app.clone(), "/sessions/runtimechild").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["model"], "opus");

    let (status, payload) = post_json(app.clone(), "/sessions/runtimechild/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert!(!tmux_session_exists(&tmux_socket, &tmux_session));

    wait_for_output_contains(
        app.clone(),
        "runtimeparent",
        "Child runtime-child (runtimec) completed: Session exited",
    )
    .await;

    let (status, payload) = post_json(app.clone(), "/sessions/runtimeparent/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert!(!tmux_session_exists(&tmux_socket, &parent_tmux_session));
}

#[tokio::test]
async fn runtime_core_spawn_wait_detects_naturally_exited_tmux_child() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let parent_dir = unique_temp_path();
    fs::create_dir_all(&parent_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-spawn-exit-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app_with_command(
        &state_file,
        &log_dir,
        &tmux_socket,
        r#"/bin/sh -lc 'while IFS= read -r line; do printf "runtime:%s\n" "$line"; case "$line" in *natural-child-prompt*) exit 0;; esac; done' runtime-sh"#,
    );

    let (status, parent_payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "naturalparent",
            "name": "natural-parent",
            "working_dir": parent_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "parent prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parent_tmux_session = parent_payload["tmux_session"].as_str().unwrap().to_owned();
    wait_for_output_contains(app.clone(), "naturalparent", "runtime:parent prompt").await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "naturalchild",
            "parent_session_id": "naturalparent",
            "prompt": "natural-child-prompt",
            "name": "natural-child",
            "wait": 10
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let child_tmux_session = payload["tmux_session"].as_str().unwrap();

    wait_for_output_contains(
        app.clone(),
        "naturalparent",
        "Child natural-child (naturalc) completed: Session exited",
    )
    .await;
    assert!(!tmux_session_exists(&tmux_socket, child_tmux_session));

    let (status, payload) = post_json(app.clone(), "/sessions/naturalparent/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert!(!tmux_session_exists(&tmux_socket, &parent_tmux_session));
}

#[tokio::test]
async fn runtime_core_spawn_wait_uses_runtime_output_as_activity() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let parent_dir = unique_temp_path();
    fs::create_dir_all(&parent_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-spawn-active-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app_with_command(
        &state_file,
        &log_dir,
        &tmux_socket,
        r#"/bin/sh -lc 'while IFS= read -r line; do case "$line" in *active-child-prompt*) for i in 1 2 3 4 5 6 7 8; do printf "runtime:heartbeat-%s\n" "$i"; sleep 0.2; done; exit 0;; *) printf "runtime:%s\n" "$line";; esac; done' runtime-sh"#,
    );

    let (status, parent_payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "activeparent",
            "name": "active-parent",
            "working_dir": parent_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "parent prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parent_tmux_session = parent_payload["tmux_session"].as_str().unwrap().to_owned();
    wait_for_output_contains(app.clone(), "activeparent", "runtime:parent prompt").await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "activechild",
            "parent_session_id": "activeparent",
            "prompt": "active-child-prompt",
            "name": "active-child",
            "wait": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let child_tmux_session = payload["tmux_session"].as_str().unwrap();

    let parent_output = wait_for_output_contains(
        app.clone(),
        "activeparent",
        "Child active-child (activech) completed: Session exited",
    )
    .await;
    assert!(!parent_output["output"]
        .as_str()
        .unwrap_or_default()
        .contains("Idle for"));
    assert!(!tmux_session_exists(&tmux_socket, child_tmux_session));

    let (status, payload) = post_json(app.clone(), "/sessions/activeparent/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert!(!tmux_session_exists(&tmux_socket, &parent_tmux_session));
}

#[tokio::test]
async fn runtime_core_send_and_retire_use_persisted_tmux_socket() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let socket_a = format!(
        "sm-rust-test-a-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let socket_b = format!("{socket_a}-b");
    let _guard_a = TestTmuxSocket(socket_a.clone());
    let _guard_b = TestTmuxSocket(socket_b.clone());
    let app_a = runtime_app(&state_file, &log_dir, &socket_a);
    let app_b = runtime_app(&state_file, &log_dir, &socket_b);

    let (status, payload) = post_json(
        app_a.clone(),
        "/sessions",
        json!({
            "id": "runtimepersisted",
            "name": "runtime-persisted",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "persisted socket initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["tmux_socket_name"], socket_a);
    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();
    wait_for_output_contains(
        app_a.clone(),
        "runtimepersisted",
        "runtime:persisted socket initial",
    )
    .await;

    let (status, payload) = post_json(
        app_b.clone(),
        "/sessions/runtimepersisted/input",
        json!({ "text": "sent through changed config socket" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(
        app_b.clone(),
        "runtimepersisted",
        "runtime:sent through changed config socket",
    )
    .await;

    let (status, payload) = post_json(app_b, "/sessions/runtimepersisted/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert!(!tmux_session_exists(&socket_a, &tmux_session));
}

#[tokio::test]
async fn runtime_core_expands_bare_home_working_dir_for_tmux() {
    if !tmux_available() {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let tmux_socket = format!(
        "sm-rust-test-home-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimehome",
            "name": "runtime-home",
            "working_dir": "~",
            "provider": "claude",
            "initial_message": "home runtime prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["working_dir"], "~");
    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();
    let home_path = PathBuf::from(home);
    assert_eq!(
        tmux_pane_current_path(&tmux_socket, &tmux_session).as_deref(),
        Some(home_path.as_path())
    );

    let (status, payload) = post_json(app.clone(), "/sessions/runtimehome/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");

    let (status, payload) = post_json(app, "/sessions/runtimehome/restore", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "runtimehome");
    assert_eq!(payload["status"], "running");
    assert_eq!(
        tmux_pane_current_path(&tmux_socket, &tmux_session).as_deref(),
        Some(home_path.as_path())
    );
}

#[tokio::test]
async fn runtime_core_marks_missing_tmux_stopped_on_send_and_retire() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-missing-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimemissingsender",
            "name": "runtime-missing-sender",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "missing sender initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimemissingsend",
            "name": "runtime-missing-send",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "missing send initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();
    assert!(tmux_kill_session(&tmux_socket, &tmux_session));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimemissingsend/input",
        json!({
            "text": "after external kill",
            "delivery_mode": "sequential",
            "sender_session_id": "runtimemissingsender",
            "from_sm_send": true,
            "notify_on_delivery": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], false);
    assert_eq!(payload["status"], "stopped");
    let queue_conn = Connection::open(queue_db_path_for_state_file(&state_file)).unwrap();
    let pending: (String, Option<String>, i64) = queue_conn
        .query_row(
            r#"
            SELECT text, delivered_at, notify_on_delivery
            FROM message_queue
            WHERE target_session_id = 'runtimemissingsend'
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        pending,
        (
            "[Input from: runtime-missing-sender (runtimem) via sm send]\nafter external kill"
                .to_owned(),
            None,
            1
        )
    );
    let sender_notification_count: i64 = queue_conn
        .query_row(
            "SELECT COUNT(*) FROM message_queue WHERE target_session_id = 'runtimemissingsender' AND text LIKE '[sm] Message delivered%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sender_notification_count, 0);

    let (status, payload) = get_json(app.clone(), "/sessions/runtimemissingsend").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "stopped");
    assert!(payload["stopped_at"].as_str().is_some());

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimemissingretire",
            "name": "runtime-missing-retire",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "missing retire initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();
    assert!(tmux_kill_session(&tmux_socket, &tmux_session));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimemissingretire/kill",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");

    let (status, payload) = get_json(app, "/sessions/runtimemissingretire").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "stopped");
    assert!(payload["stopped_at"].as_str().is_some());
}

#[tokio::test]
async fn runtime_core_does_not_queue_sends_to_already_stopped_sessions() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "runtimestopped",
                    "name": "runtime-stopped",
                    "working_dir": "/repo",
                    "tmux_session": "claude-runtimestopped",
                    "node": "primary",
                    "provider": "claude",
                    "log_file": "/tmp/runtimestopped.log",
                    "status": "stopped",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "stopped_at": "2026-06-01T00:02:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let log_dir = unique_temp_path();
    let app = runtime_app(&state_file, &log_dir, "sm-rust-test-stopped-send");

    let (status, payload) = post_json(
        app,
        "/sessions/runtimestopped/input",
        json!({
            "text": "do not retain for stopped session",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], false);
    assert_eq!(payload["status"], "stopped");
    assert!(!queue_db_path_for_state_file(&state_file).exists());
}

#[tokio::test]
async fn runtime_core_rejects_unsupported_provider() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-provider-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimecodex",
            "working_dir": working_dir.display().to_string(),
            "provider": "codex",
            "initial_message": "codex should not launch claude"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload,
        json!({ "detail": "Rust runtime does not support provider codex" })
    );
    let (status, _) = get_json(app, "/sessions/runtimecodex").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn runtime_core_rejects_remote_node_create_before_local_tmux_launch() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let app = runtime_app(&state_file, &log_dir, "sm-rust-test-remote-create");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimeremote",
            "working_dir": ".",
            "provider": "claude",
            "node": "macbook",
            "initial_message": "remote node should not launch locally"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload,
        json!({ "detail": "Rust runtime does not support remote node macbook" })
    );
    let (status, _) = get_json(app, "/sessions/runtimeremote").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn runtime_core_rejects_child_create_that_inherits_remote_parent_node() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "remoteparent",
                    "name": "claude-remoteparent",
                    "working_dir": "/remote/repo",
                    "tmux_session": "claude-remoteparent",
                    "node": "macbook",
                    "provider": "claude",
                    "log_file": "/tmp/remoteparent.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let log_dir = unique_temp_path();
    let app = runtime_app(&state_file, &log_dir, "sm-rust-test-remote-child");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "remotechild",
            "parent_session_id": "remoteparent",
            "provider": "claude",
            "initial_message": "child should inherit remote node and reject"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload,
        json!({ "detail": "Rust runtime does not support remote node macbook" })
    );
    let (status, _) = get_json(app, "/sessions/remotechild").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn runtime_core_rejects_remote_node_send_and_retire_without_mutating_state() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "remoteruntime",
                    "name": "claude-remoteruntime",
                    "working_dir": "/repo",
                    "tmux_session": "claude-remoteruntime",
                    "node": "macbook",
                    "provider": "claude",
                    "log_file": "/tmp/remoteruntime.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let log_dir = unique_temp_path();
    let app = runtime_app(&state_file, &log_dir, "sm-rust-test-remote-existing");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/remoteruntime/input",
        json!({ "text": "do not send to local tmux" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload,
        json!({ "detail": "Rust runtime does not support remote node macbook" })
    );

    let (status, payload) = post_json(app.clone(), "/sessions/remoteruntime/kill", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload,
        json!({ "detail": "Rust runtime does not support remote node macbook" })
    );

    let (status, payload) = get_json(app, "/sessions/remoteruntime").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["node"], "macbook");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["stopped_at"], Value::Null);
}

#[tokio::test]
async fn runtime_core_fails_create_when_stdin_prompt_cannot_be_delivered() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-exit-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db_path_for_state_file(&state_file)
                .display()
                .to_string(),
        },
        rust_core: RustCoreConfig {
            runtime_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            tmux_socket_name: Some(tmux_socket.clone()),
            runtime_command: Some("/bin/sh -lc 'exit 0'".to_owned()),
            runtime_prompt_mode: Some("stdin".to_owned()),
            runtime_start_settle_ms: Some(100),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimeexited",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "cannot be delivered"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        payload,
        json!({ "detail": "tmux session exited before initial prompt could be delivered" })
    );
    let (status, _) = get_json(app, "/sessions/runtimeexited").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn session_output_tails_large_log_file_from_end() {
    let state_file = unique_temp_path();
    let log_file = unique_temp_path();
    fs::write(
        &log_file,
        format!(
            "{}\nlast retained line\nfinal retained line\n",
            "x".repeat(2 * 1024 * 1024)
        ),
    )
    .unwrap();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "largeout",
                    "name": "claude-largeout",
                    "working_dir": "/repo",
                    "tmux_session": "claude-largeout",
                    "node": "primary",
                    "provider": "claude",
                    "log_file": log_file.display().to_string(),
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions/largeout/output?lines=2").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload["output"],
        "last retained line\nfinal retained line\n"
    );
}

#[tokio::test]
async fn sessions_missing_state_file_returns_empty_list() {
    let state_file = unique_temp_path();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "sessions": [] }));
}

#[tokio::test]
async fn sessions_reject_public_host_when_google_auth_enabled() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));

    let (status, payload) = get_json_with_host(app, "/sessions", "sm.example.com").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        payload,
        json!({
            "detail": "Authentication required",
            "login_url": "/auth/google/login?next=%2Fsessions"
        })
    );
}

#[tokio::test]
async fn sessions_allow_public_device_bearer_when_google_auth_enabled() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));
    let token = device_access_token("session-cookie-secret", "rajesh@example.com", "Rajesh");

    let (status, payload) = get_json_with_host_and_headers(
        app,
        "/sessions",
        "sm.example.com",
        &[("authorization", format!("Bearer {token}"))],
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["sessions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn client_sessions_allow_public_browser_cookie_when_google_auth_enabled() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));
    let cookie = starlette_session_cookie(
        "session-cookie-secret",
        json!({
            "google_authenticated": true,
            "google_email": "rajesh@example.com",
            "google_name": "Rajesh"
        }),
    );

    let (status, payload) = get_json_with_host_and_headers(
        app,
        "/client/sessions",
        "sm.example.com",
        &[("cookie", format!("sm_auth={cookie}"))],
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["sessions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn auth_session_reports_device_bearer_when_google_auth_enabled() {
    let token = device_access_token("session-cookie-secret", "rajesh@example.com", "Rajesh");
    let app = router(AppState::new(config_with_state_file_and_auth(
        &unique_temp_path(),
    )));

    let (status, payload) = get_json_with_host_and_headers(
        app,
        "/auth/session",
        "sm.example.com",
        &[("authorization", format!("Bearer {token}"))],
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "enabled": true,
            "authenticated": true,
            "bypass": false,
            "email": "rajesh@example.com",
            "name": "Rajesh",
            "auth_type": "device_bearer"
        })
    );
}

#[tokio::test]
async fn client_sessions_allow_local_bypass_when_google_auth_enabled() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));

    let (status, payload) = get_json_with_host_and_peer(
        app,
        "/client/sessions",
        "localhost:8421",
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["sessions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn sessions_project_top_level_registry_and_adoption_state() {
    let state_file = write_registry_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app.clone(), "/sessions").await;

    assert_eq!(status, StatusCode::OK);
    let sessions = payload["sessions"].as_array().unwrap();
    let maintainer = sessions
        .iter()
        .find(|session| session["id"] == "em123456")
        .unwrap();
    let child = sessions
        .iter()
        .find(|session| session["id"] == "child001")
        .unwrap();
    assert_eq!(maintainer["aliases"], json!(["maintainer"]));
    assert_eq!(maintainer["is_maintainer"], true);
    assert_eq!(maintainer["friendly_name"], "maintainer");
    assert_eq!(child["aliases"], json!(["reviewer"]));
    assert_eq!(
        child["pending_adoption_proposals"],
        json!([
            {
                "id": "proposal1",
                "proposer_session_id": "em123456",
                "proposer_name": "maintainer",
                "target_session_id": "child001",
                "created_at": "2026-06-01T00:03:00",
                "status": "pending",
                "decided_at": null
            }
        ])
    );

    let (status, payload) = get_json(app, "/registry").await;
    assert_eq!(status, StatusCode::OK);
    let roles = payload["registrations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["role"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(roles, vec!["maintainer", "reviewer"]);
    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert!(raw_state["agent_registrations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["role"] == "maintainer" && entry["session_id"] == "em123456"));
}

fn config_with_state_file(state_file: &PathBuf) -> AppConfig {
    AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        ..AppConfig::default()
    }
}

fn config_with_state_file_and_queue(state_file: &PathBuf) -> AppConfig {
    AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db_path_for_state_file(state_file)
                .display()
                .to_string(),
        },
        ..AppConfig::default()
    }
}

fn queue_db_path_for_state_file(state_file: &PathBuf) -> PathBuf {
    state_file.with_extension("message_queue.db")
}

fn config_with_state_file_and_auth(state_file: &PathBuf) -> AppConfig {
    AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        google_auth: GoogleAuthConfig {
            enabled: true,
            public_host: Some("sm.example.com".to_owned()),
            client_id: Some("web-client-id".to_owned()),
            android_client_id: Some("android-client-id".to_owned()),
            client_secret: Some("web-client-secret".to_owned()),
            redirect_uri: Some("https://sm.example.com/auth/google/callback".to_owned()),
            allowlist_emails: vec!["rajesh@example.com".to_owned()],
            session_cookie_secret: Some("session-cookie-secret".to_owned()),
        },
        ..AppConfig::default()
    }
}

fn write_session_fixture() -> PathBuf {
    let path = unique_temp_path();
    let log_file = unique_temp_path();
    fs::write(
        &log_file,
        "fixture log line 1\nfixture log line 2\nfixture log line 3\n",
    )
    .unwrap();
    fs::write(
        &path,
        json!({
            "sessions": [
                {
                    "id": "run12345",
                    "name": "claude-run12345",
                    "working_dir": "/repo",
                    "tmux_session": "claude-run12345",
                    "tmux_socket_name": null,
                    "node": "primary",
                    "provider": "claude",
                    "log_file": log_file.display().to_string(),
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "friendly_name": "Runner",
                    "friendly_name_is_explicit": false,
                    "friendly_name_updated_at_ns": 10,
                    "native_title": "Runner Native",
                    "native_title_updated_at_ns": 20,
                    "current_task": "Working",
                    "tokens_used": 42,
                    "context_monitor_enabled": true
                },
                {
                    "id": "oldstate",
                    "name": "claude-oldstate",
                    "working_dir": "/repo",
                    "tmux_session": "claude-oldstate",
                    "log_file": "/tmp/oldstate.log",
                    "status": "waiting_permission",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                },
                {
                    "id": "stop1234",
                    "name": "claude-stop1234",
                    "working_dir": "/repo",
                    "tmux_session": "claude-stop1234",
                    "log_file": "/tmp/stop1234.log",
                    "status": "stopped",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "stopped_at": "2026-06-01T00:02:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    path
}

fn write_completion_fixture() -> PathBuf {
    let path = unique_temp_path();
    fs::write(
        &path,
        json!({
            "sessions": [
                {
                    "id": "em001",
                    "name": "claude-em001",
                    "working_dir": "/repo",
                    "tmux_session": "claude-em001",
                    "log_file": "/tmp/em001.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "friendly_name": "em",
                    "is_em": true
                },
                {
                    "id": "em002",
                    "name": "claude-em002",
                    "working_dir": "/repo",
                    "tmux_session": "claude-em002",
                    "log_file": "/tmp/em002.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "friendly_name": "other-em",
                    "is_em": true
                },
                {
                    "id": "child001",
                    "name": "claude-child001",
                    "working_dir": "/repo",
                    "tmux_session": "claude-child001",
                    "log_file": "/tmp/child001.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "friendly_name": "worker-1",
                    "parent_session_id": "em001",
                    "agent_task_completed_at": null
                },
                {
                    "id": "fork001",
                    "name": "codex-fork-fork001",
                    "working_dir": "/repo",
                    "tmux_session": "codex-fork-fork001",
                    "log_file": "/tmp/fork001.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "provider": "codex-fork",
                    "parent_session_id": "em001"
                }
            ],
            "retained_remind_registrations": [
                {
                    "session_id": "child001",
                    "is_active": true
                }
            ],
            "retained_parent_wake_registrations": [
                {
                    "child_session_id": "child001",
                    "parent_session_id": "em001",
                    "period_seconds": 600,
                    "is_active": true
                }
            ],
            "retained_pending_messages": [],
            "retained_stop_notify_states": []
        })
        .to_string(),
    )
    .unwrap();
    path
}

fn write_registry_fixture() -> PathBuf {
    let path = unique_temp_path();
    fs::write(
        &path,
        json!({
            "sessions": [
                {
                    "id": "em123456",
                    "name": "claude-em123456",
                    "working_dir": "/repo",
                    "tmux_session": "claude-em123456",
                    "log_file": "/tmp/em123456.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "friendly_name": "em-ops",
                    "is_em": true
                },
                {
                    "id": "child001",
                    "name": "claude-child001",
                    "working_dir": "/repo",
                    "tmux_session": "claude-child001",
                    "log_file": "/tmp/child001.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                },
                {
                    "id": "deadrole",
                    "name": "claude-deadrole",
                    "working_dir": "/repo",
                    "tmux_session": "claude-deadrole",
                    "log_file": "/tmp/deadrole.log",
                    "status": "stopped",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "stopped_at": "2026-06-01T00:02:00"
                }
            ],
            "maintainer_session_id": "em123456",
            "agent_registrations": [
                {
                    "role": "Reviewer",
                    "session_id": "child001",
                    "created_at": "2026-06-01T00:02:00"
                },
                {
                    "role": "Stale Role",
                    "session_id": "deadrole",
                    "created_at": "2026-06-01T00:02:30"
                }
            ],
            "adoption_proposals": [
                {
                    "id": "proposal1",
                    "proposer_session_id": "em123456",
                    "target_session_id": "child001",
                    "created_at": "2026-06-01T00:03:00",
                    "status": "pending",
                    "decided_at": null
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    path
}

fn unique_temp_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "sm-rust-read-only-sessions-{}-{nanos}-{}.json",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn runtime_app(state_file: &PathBuf, log_dir: &PathBuf, tmux_socket: &str) -> axum::Router {
    runtime_app_with_command(
        state_file,
        log_dir,
        tmux_socket,
        r#"/bin/sh -lc 'while IFS= read -r line; do printf "argv:%s\nids:%s:%s:%s\nruntime:%s\n" "$*" "$SESSION_MANAGER_ID" "$CLAUDE_SESSION_MANAGER_ID" "$ENABLE_TOOL_SEARCH" "$line"; done' runtime-sh"#,
    )
}

fn runtime_app_with_command(
    state_file: &PathBuf,
    log_dir: &PathBuf,
    tmux_socket: &str,
    runtime_command: &str,
) -> axum::Router {
    router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db_path_for_state_file(state_file)
                .display()
                .to_string(),
        },
        rust_core: RustCoreConfig {
            runtime_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            tmux_socket_name: Some(tmux_socket.to_owned()),
            runtime_command: Some(runtime_command.to_owned()),
            runtime_prompt_mode: Some("stdin".to_owned()),
            runtime_start_settle_ms: Some(100),
            send_keys_settle_ms: Some(10.0),
            send_keys_settle_max_ms: Some(50.0),
            send_keys_max_chunk_chars: Some(128),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }))
}

async fn wait_for_output_contains(app: axum::Router, session_id: &str, needle: &str) -> Value {
    for _ in 0..30 {
        let (status, payload) = get_json(
            app.clone(),
            &format!("/sessions/{session_id}/output?lines=20"),
        )
        .await;
        if status == StatusCode::OK
            && payload["output"]
                .as_str()
                .unwrap_or_default()
                .contains(needle)
        {
            return payload;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for output containing {needle:?}");
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn tmux_session_exists(socket: &str, session: &str) -> bool {
    Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .arg("has-session")
        .arg("-t")
        .arg(session)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn tmux_kill_session(socket: &str, session: &str) -> bool {
    Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .arg("kill-session")
        .arg("-t")
        .arg(session)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn tmux_enter_copy_mode(socket: &str, session: &str) -> bool {
    Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .arg("copy-mode")
        .arg("-t")
        .arg(session)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn tmux_pane_in_mode(socket: &str, session: &str) -> Option<i32> {
    let output = Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .arg("display-message")
        .arg("-p")
        .arg("-t")
        .arg(session)
        .arg("#{pane_in_mode}")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "0" => Some(0),
        "1" => Some(1),
        _ => None,
    }
}

fn tmux_pane_current_path(socket: &str, session: &str) -> Option<PathBuf> {
    let output = Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .arg("display-message")
        .arg("-p")
        .arg("-t")
        .arg(session)
        .arg("#{pane_current_path}")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

struct TestTmuxSocket(String);

impl Drop for TestTmuxSocket {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-L")
            .arg(&self.0)
            .arg("kill-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn device_access_token(secret: &str, email: &str, name: &str) -> String {
    let exp = unix_timestamp() + 3600;
    let payload = json!({
        "v": 1,
        "type": "device_access",
        "email": email,
        "name": name,
        "iat": unix_timestamp(),
        "exp": exp
    });
    let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
    let signature = hmac_sha256_urlsafe(secret.as_bytes(), payload_b64.as_bytes());
    format!("smat_{payload_b64}.{signature}")
}

fn starlette_session_cookie(secret: &str, payload: Value) -> String {
    let payload_b64 = STANDARD.encode(serde_json::to_vec(&payload).unwrap());
    let timestamp_b64 = URL_SAFE_NO_PAD.encode(int_to_bytes(unix_timestamp() as u64));
    let value = format!("{payload_b64}.{timestamp_b64}");
    let derived_key = itsdangerous_django_concat_key(secret);
    let signature = hmac_sha1_urlsafe(&derived_key, value.as_bytes());
    format!("{value}.{signature}")
}

fn hmac_sha256_urlsafe(key: &[u8], value: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(value);
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn sha256_hex(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac_sha1_urlsafe(key: &[u8], value: &[u8]) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).unwrap();
    mac.update(value);
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn itsdangerous_django_concat_key(secret: &str) -> Vec<u8> {
    let mut hasher = Sha1::new();
    hasher.update(b"itsdangerous.Signersigner");
    hasher.update(secret.as_bytes());
    hasher.finalize().to_vec()
}

fn int_to_bytes(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let first_nonzero = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len() - 1);
    bytes[first_nonzero..].to_vec()
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
