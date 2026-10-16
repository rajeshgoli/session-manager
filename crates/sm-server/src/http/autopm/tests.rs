use super::*;
use crate::http::tests::{
    browser_host_get, cloudflare_access_config, google_auth_config, handoff_json_request,
    local_request, public_request_with_host, response_json, seed_cloudflare_access_jwks,
    test_browser_access_assertion, write_session_state,
};
use axum::http::{HeaderName, Method};
use tower::ServiceExt;

fn state() -> AppState {
    let mut config = google_auth_config();
    config.cloudflare_access = cloudflare_access_config().cloudflare_access;
    config.paths.state_file = write_session_state("autopm-http", "running");
    let dir = PathBuf::from(&config.paths.state_file);
    config.sm_send.db_path = dir.with_file_name("messages.db").display().to_string();
    config.usage.db_path = dir.with_file_name("usage.db").display().to_string();
    config.utilization.db_path = dir.with_file_name("utilization.db").display().to_string();
    config.rust_core.fixture_writes_enabled = true;
    config.rust_core.runtime_enabled = false;
    let state = AppState::new(config);
    seed_cloudflare_access_jwks(&state);
    state
}

/// A live switch experiment, written directly.
fn seed_live(state: &AppState) {
    let conn = store(state).open().unwrap();
    conn.execute_batch(
        "INSERT INTO experiment_switches VALUES ('autopm.card', 'x-2026w43-card', 1, 0, 1, 'autopm', NULL);
         INSERT INTO autopm_experiments (id, cycle_ticket, title, hypothesis, path, switch_key,
           window_days, state, live_at_ms, measure_at_ms, created_at_ms, updated_at_ms)
         VALUES ('x-2026w43-card', 3001, 'Decision card', 'h', 'switch', 'autopm.card',
           14, 'live', 1, 2, 1, 1);",
    )
    .unwrap();
    state.autopm_switches.reload(&conn).unwrap();
}

fn version(response: &Response) -> u64 {
    response.headers()["x-sm-switches"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

fn json_put(uri: &str, body: Value) -> axum::http::Request<Body> {
    let mut request = local_request(Method::PUT, uri, Body::from(body.to_string()));
    request
        .headers_mut()
        .insert(CONTENT_TYPE, "application/json".parse().unwrap());
    request
}

#[tokio::test]
async fn switch_routes_are_owner_only_and_every_response_names_the_switch_version() {
    let state = state();
    seed_live(&state);
    let app = router(state.clone());
    let response = app
        .clone()
        .oneshot(local_request(Method::GET, "/health", Body::empty()))
        .await
        .unwrap();
    let first = version(&response);
    let (status, body) = response_json(
        app.clone()
            .oneshot(local_request(
                Method::GET,
                "/autopm/switches",
                Body::empty(),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["switches"][0]["key"], "autopm.card");
    assert_eq!(body["enabled"], false, "Auto PM starts off");
    for route in [
        "/autopm/switches",
        "/autopm/experiments",
        "/autopm/budget",
        "/autopm/experiments/x-2026w43-card",
    ] {
        let mut request = local_request(Method::GET, route, Body::empty());
        request
            .headers_mut()
            .insert("x-sm-session-id", "some-agent".parse().unwrap());
        let (status, _) = response_json(app.clone().oneshot(request).await.unwrap()).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{route}: agents wait for sm#2175"
        );
    }
    let response = app
        .clone()
        .oneshot(json_put(
            "/autopm/switches/autopm.card",
            json!({"on": false, "reason": "test"}),
        ))
        .await
        .unwrap();
    assert!(
        version(&response) > first,
        "a switch write raises the version"
    );
    let (status, body) = response_json(response).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["switch"]["on"], false);
    let (_, shown) = response_json(
        app.clone()
            .oneshot(local_request(
                Method::GET,
                "/autopm/experiments/x-2026w43-card",
                Body::empty(),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        shown["experiment"]["state"], "reverted",
        "Rajesh switched it off"
    );
    // Turning a reverted change back on needs Auto PM on.
    let (status, body) = response_json(
        app.clone()
            .oneshot(json_put(
                "/autopm/switches/autopm.card",
                json!({"on": true}),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .starts_with("autopm_disabled: "),
        "{body}"
    );
}

#[tokio::test]
async fn settings_carry_the_switches_and_turning_auto_pm_off_is_the_kill_switch() {
    let state = state();
    seed_live(&state);
    let app = router(state.clone());
    let owner =
        test_browser_access_assertion("sm-browser-aud", "rajeshgoli@gmail.com", 4_102_444_800);
    let put = |body: Value| {
        let request = handoff_json_request(
            public_request_with_host(
                Method::PUT,
                "/client/settings",
                Body::from(body.to_string()),
                "sm.example.com",
            ),
            &[
                ("cf-access-jwt-assertion", owner.as_str()),
                ("origin", "https://sm.example.com"),
            ],
        );
        let app = app.clone();
        async move { response_json(app.oneshot(request).await.unwrap()).await }
    };
    let (status, body) = browser_host_get(&app, "/client/settings", Some(&owner)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["autopm"]["enabled"], false);
    assert_eq!(
        body["autopm"]["dedicated_points"],
        json!({"claude": 5, "codex": 20})
    );
    assert_eq!(
        body["switches"],
        json!({"autopm.card": false}),
        "off while disabled"
    );
    let (status, body) = put(json!({"autopm": {"max_live": 99}})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = put(json!({"autopm": {"skip_window_starting": "soon"}})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = put(json!({"autopm": {"enabled": true}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["switches"], json!({"autopm.card": true}));
    assert!(state.autopm_switches.is_on("autopm.card"));
    let (status, body) = put(json!({"autopm": {"enabled": false}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["switches"], json!({"autopm.card": false}));
    let row = crate::autopm::switches::get(&store(&state).open().unwrap(), "autopm.card")
        .unwrap()
        .unwrap();
    assert!(!row.on && row.held && row.changed_by == "killswitch");
    // Re-enabling turns nothing back on.
    put(json!({"autopm": {"enabled": true}})).await;
    assert!(!state.autopm_switches.is_on("autopm.card"));
}

#[tokio::test]
async fn feedback_is_owner_only_and_a_thumbs_down_switches_the_change_off() {
    let state = state();
    seed_live(&state);
    let app = router(state.clone());
    let owner =
        test_browser_access_assertion("sm-browser-aud", "rajeshgoli@gmail.com", 4_102_444_800);
    let vote = |body: Value| {
        let request = handoff_json_request(
            public_request_with_host(
                Method::POST,
                "/client/autopm/feedback",
                Body::from(body.to_string()),
                "sm.example.com",
            ),
            &[
                ("cf-access-jwt-assertion", owner.as_str()),
                ("origin", "https://sm.example.com"),
            ],
        );
        let app = app.clone();
        async move { response_json(app.oneshot(request).await.unwrap()).await }
    };
    let (status, _) = vote(json!({"experiment_id": "x-2026w43-card", "vote": "sideways"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = vote(json!({"experiment_id": "x-2026w43-card", "vote": "up"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["experiment"]["votes_up"], 1);
    let (status, body) =
        vote(json!({"experiment_id": "x-2026w43-card", "vote": "down", "note": "noisy"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["experiment"]["state"], "reverted");
    let row = crate::autopm::switches::get(&store(&state).open().unwrap(), "autopm.card")
        .unwrap()
        .unwrap();
    assert!(!row.on && row.held && row.changed_by == "feedback");
    let anonymous = public_request_with_host(
        Method::POST,
        "/client/autopm/feedback",
        Body::from(json!({"experiment_id": "x-2026w43-card", "vote": "down"}).to_string()),
        "sm.example.com",
    );
    let status = app.clone().oneshot(anonymous).await.unwrap().status();
    assert!(status.is_client_error(), "{status}");
}

#[test]
fn review_run_tokens_come_from_the_runs_own_output() {
    let codex = [
        json!({"type": "turn.started"}),
        json!({"type": "turn.completed", "usage": {"input_tokens": 1000,
            "cached_input_tokens": 600, "output_tokens": 50}}),
        json!({"type": "turn.completed", "usage": {"input_tokens": 10, "output_tokens": 5}}),
    ]
    .map(|line| line.to_string())
    .join("\n");
    let tokens = codex_run_tokens(&codex);
    assert_eq!(
        (tokens.input, tokens.cache_read, tokens.output),
        (410, 600, 55)
    );
    let claude = json!({"usage": {"input_tokens": 7, "output_tokens": 9,
        "cache_creation_input_tokens": 100, "cache_read_input_tokens": 2000}})
    .to_string();
    let tokens = claude_run_tokens(&claude);
    assert_eq!(
        (
            tokens.input,
            tokens.output,
            tokens.cache_write_5m,
            tokens.cache_read
        ),
        (7, 9, 100, 2000)
    );
}

fn role_state() -> AppState {
    use sha2::{Digest, Sha256};
    let state = state();
    let sessions: Vec<Value> = ["pm", "other", "successor"].into_iter().map(|id| json!({
        "id":id,"name":id,"working_dir":"/repo","tmux_session":id,
        "provider":"claude","status":"running","created_at":"2026-10-15T00:00:00",
        "last_activity":"2026-10-15T00:00:00",
        "session_credential_sha256":format!("{:x}", Sha256::digest(format!("{id}-secret").as_bytes()))
    })).collect();
    std::fs::write(
        &state.config.paths.state_file,
        json!({"sessions":sessions}).to_string(),
    )
    .unwrap();
    state
}

fn role_request(method: Method, path: &str, id: &str, credential: &str, body: Value) -> Request {
    let mut request = local_request(method, path, Body::from(body.to_string()));
    for (key, value) in [
        ("content-type", "application/json"),
        ("x-sm-session", id),
        ("x-sm-session-credential", credential),
    ] {
        request.headers_mut().insert(
            HeaderName::from_bytes(key.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    request
}

#[tokio::test]
async fn autopm_registration_is_reserved_authenticated_single_holder_and_survives_handoff() {
    let state = role_state();
    let app = router(state.clone());
    let register = |id: &str, credential: &str, role: &str| {
        role_request(
            Method::POST,
            &format!("/sessions/{id}/registry"),
            id,
            credential,
            json!({"requester_session_id":id,"role":role}),
        )
    };
    assert_eq!(
        app.clone()
            .oneshot(register("pm", "pm-secret", "autopm"))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    state.session_store.authorize_autopm_seat("pm").unwrap();
    // Normalized role spellings must not bypass the credential check.
    assert_eq!(
        app.clone()
            .oneshot(register("pm", "wrong", "AutoPM!"))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(register("pm", "pm-secret", "autopm"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    state.session_store.authorize_autopm_seat("other").unwrap();
    assert_eq!(
        app.clone()
            .oneshot(register("other", "other-secret", "autopm"))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let mut forged = register("pm", "pm-secret", "autopm");
    forged
        .headers_mut()
        .insert("x-sm-session-id", "other".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(forged).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    state
        .session_store
        .transfer_handoff_json("pm", "successor")
        .unwrap();
    assert!(state
        .session_store
        .is_authorized_autopm_seat("successor")
        .unwrap());
    assert_eq!(
        state
            .session_store
            .lookup_agent_registration("autopm")
            .unwrap()
            .unwrap()
            .session_id,
        "successor"
    );
    for (id, expected) in [("pm", StatusCode::FORBIDDEN), ("successor", StatusCode::OK)] {
        let response = app
            .clone()
            .oneshot(role_request(
                Method::GET,
                "/autopm/switches",
                id,
                &format!("{id}-secret"),
                Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    let unregister = role_request(
        Method::DELETE,
        "/sessions/successor/registry",
        "successor",
        "wrong",
        json!({"requester_session_id":"successor","role":"autopm"}),
    );
    assert_eq!(
        app.oneshot(unregister).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn autopm_routes_use_role_actor_and_preserve_owner_only_feedback() {
    let state = role_state();
    seed_live(&state);
    state.session_store.authorize_autopm_seat("pm").unwrap();
    state
        .session_store
        .register_agent_role(
            "pm",
            RoleRegistrationRequest {
                requester_session_id: "pm".into(),
                role: "autopm".into(),
            },
        )
        .unwrap();
    let app = router(state.clone());
    for route in [
        "/autopm/switches",
        "/autopm/events",
        "/autopm/budget",
        "/autopm/experiments",
        "/autopm/experiments/x-2026w43-card",
    ] {
        for (id, secret, expected) in [
            ("pm", "pm-secret", StatusCode::OK),
            ("pm", "wrong", StatusCode::FORBIDDEN),
            ("other", "other-secret", StatusCode::FORBIDDEN),
        ] {
            let (status, body) = response_json(
                app.clone()
                    .oneshot(role_request(Method::GET, route, id, secret, Value::Null))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(status, expected, "{route}: {body}");
        }
    }
    store(&state)
        .open()
        .unwrap()
        .execute("UPDATE experiment_switches SET held=1", [])
        .unwrap();
    let (status, body) = response_json(
        app.clone()
            .oneshot(role_request(
                Method::PUT,
                "/autopm/switches/autopm.card",
                "pm",
                "pm-secret",
                json!({"on":true}),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["detail"].as_str().unwrap().starts_with("held:"),
        "{body}"
    );
    let (status, body) = response_json(
        app.clone()
            .oneshot(role_request(
                Method::PUT,
                "/autopm/switches/autopm.card",
                "pm",
                "pm-secret",
                json!({"on":false}),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["switch"]["changed_by"], "autopm");
    assert_eq!(
        app.oneshot(role_request(
            Method::POST,
            "/client/autopm/feedback",
            "pm",
            "pm-secret",
            json!({"experiment_id":"x-2026w43-card","vote":"down"})
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::FORBIDDEN
    );
}
