use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode},
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use futures_util::StreamExt as _;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use sm_server::config::RustShadowConfig;
use sm_server::{
    config::{AppConfig, ExternalAccessConfig, GoogleAuthConfig, PathsConfig},
    http::{router, AppState},
};
use std::{
    fs,
    net::SocketAddr,
    path::PathBuf,
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
    post_json_with_headers_and_peer(
        app,
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
    let mut builder = Request::builder()
        .method("POST")
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

    let (status, payload) = get_json(app, "/sessions").await;

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
}

fn config_with_state_file(state_file: &PathBuf) -> AppConfig {
    AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        ..AppConfig::default()
    }
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
