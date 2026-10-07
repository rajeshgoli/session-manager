use super::*;
use axum::http::HeaderValue;
use std::os::unix::fs::PermissionsExt;
use std::time::SystemTime;
use tower::ServiceExt;

struct Fixture {
    directory: PathBuf,
    state: AppState,
}
impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("sm-local-http-{}", random_urlsafe_token(12)));
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = AppConfig::default();
        config.paths.state_file = directory.join("sessions.json").display().to_string();
        config.sm_send.db_path = directory.join("messages.db").display().to_string();
        config.rust_core.fixture_writes_enabled = true;
        config.rust_core.runtime_enabled = false;
        config.usage.enabled = false;
        // A verified gateway works without cookies even when Google auth is on.
        config.google_auth.enabled = true;
        let sessions: Vec<_> = ["agent-a", "agent-b", "retired"]
            .into_iter()
            .map(|id| {
                json!({
                    "id":id,"name":id,"working_dir":directory,"tmux_session":id,"provider":"claude",
                    "status":if id == "retired" { "stopped" } else { "running" },
                    "created_at":"2026-06-01T00:00:00","last_activity":"2026-06-01T00:01:00"
                })
            })
            .collect();
        fs::write(
            &config.paths.state_file,
            serde_json::to_vec(&json!({"sessions":sessions})).unwrap(),
        )
        .unwrap();
        fs::write(directory.join("gateway.key"), [7u8; 32]).unwrap();
        let records: BTreeMap<_,_> = ["agent-a", "agent-b", "retired"].into_iter().enumerate().map(|(n, id)| (id,json!({
            "agent_id":id,"port":18700+n,"active":true,"gateway":{"port":18600+n,"upstream":"127.0.0.1:8420"}
        }))).collect();
        fs::write(
            directory.join("registrations.json"),
            serde_json::to_vec(&records).unwrap(),
        )
        .unwrap();
        for file in ["gateway.key", "registrations.json"] {
            fs::set_permissions(directory.join(file), fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut state = AppState::new(config);
        state.local_agent_verifier =
            crate::local_egress::gateway::StampVerifier::new(directory.clone());
        Self { directory, state }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn signed_request(agent: &str, method: &str, target: &str, body: Value) -> Request {
    let body = serde_json::to_vec(&body).unwrap();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let time = timestamp.to_be_bytes();
    let digest = Sha256::digest(&body);
    let mut mac = Hmac::<Sha256>::new_from_slice(&[7u8; 32]).unwrap();
    for part in [
        b"sm-local-gateway-v1".as_slice(),
        agent.as_bytes(),
        &time,
        method.as_bytes(),
        target.as_bytes(),
        digest.as_slice(),
    ] {
        mac.update(&(part.len() as u64).to_be_bytes());
        mac.update(part);
    }
    let stamp = format!(
        "{timestamp}:{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    );
    let mut request = Request::builder()
        .method(method)
        .uri(target)
        .header("host", "127.0.0.1:8420")
        .header("content-type", "application/json")
        .header(AGENT_HEADER, agent)
        .header(SIGNATURE_HEADER, stamp)
        .header("x-sm-session-id", "forged")
        .header("x-sm-session", "forged")
        .header("x-sm-session-credential", "forged")
        .header("authorization", "Bearer forged")
        .header("cookie", "sm_auth=forged")
        .body(Body::from(body))
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
    ));
    request
}
async fn json_response(response: Response) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}
async fn echo(State(state): State<Arc<AppState>>, request: Request) -> Json<Value> {
    ensure_session_read_allowed(&state, &request).unwrap();
    let caller = crate::local_identity::current();
    let own_credential = state
        .session_store
        .session_credential_matches("agent-a", "not-a-credential")
        .unwrap();
    let other_credential = state
        .session_store
        .session_credential_matches("agent-b", "not-a-credential")
        .unwrap();
    let actor = request_actor_email(&state.config, &request);
    let bypass = is_request_local_bypass(&state, &request);
    let header = header_text(request.headers(), "x-sm-session-id");
    let extension = request
        .extensions()
        .get::<crate::local_egress::gateway::VerifiedLocalAgent>()
        .map(|a| a.agent_id().to_owned());
    let uri = request.uri().to_string();
    let body = to_bytes(request.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    Json(
        json!({"caller":caller.map(|a| a.agent_id().to_owned()),"extension":extension,"actor":actor,"bypass":bypass,
        "own_credential":own_credential,"other_credential":other_credential,"header":header,"uri":uri,"body":serde_json::from_slice::<Value>(&body).ok()}),
    )
}
fn routes() -> Vec<String> {
    let source = include_str!("../../http.rs");
    let source = source
        .split("pub fn router(state: AppState)")
        .nth(1)
        .unwrap()
        .split("async fn mark_in_app")
        .next()
        .unwrap();
    regex::Regex::new(r#"\.route\(\s*"([^"]+)""#)
        .unwrap()
        .captures_iter(source)
        .map(|c| c[1].to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
fn target(route: &str) -> String {
    regex::Regex::new(r"\{[^}]+\}")
        .unwrap()
        .replace_all(route, "agent-a")
        .into_owned()
}
fn echo_router(state: AppState) -> Router {
    let state = Arc::new(state);
    let mut router = Router::new();
    for route in routes() {
        router = router.route(&route, axum::routing::any(echo));
    }
    router
        .fallback(echo)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate,
        ))
        .with_state(state)
}

#[tokio::test]
async fn every_registered_route_is_bound_or_closed_for_a_stamped_caller() {
    let fixture = Fixture::new();
    let app = echo_router(fixture.state.clone());
    let mut accepted = 0;
    for route in routes() {
        for method in ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
            let path = target(&route);
            let body = json!({"requester_session_id":"agent-b","sender_session_id":"agent-b","session_id":"agent-b",
                "notify_target":"agent-b","notify_session_id":"agent-b","recipients":["agent-b"],
                "target_session_id":"agent-b","target_parent_session_id":"agent-b","reviewer":{"session_id":"agent-b"}});
            let response = app
                .clone()
                .oneshot(signed_request("agent-a", method, &path, body.clone()))
                .await
                .unwrap();
            let Some(selected) = policy(method, &route) else {
                assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {route}");
                continue;
            };
            assert_eq!(response.status(), StatusCode::OK, "{method} {route}");
            let result = json_response(response).await;
            assert_eq!(result["caller"], "agent-a", "{method} {route}");
            assert_eq!(result["extension"], "agent-a");
            assert_eq!(result["header"], "agent-a");
            assert_eq!(result["actor"], Value::Null);
            assert_eq!(result["bypass"], false);
            assert_eq!(result["own_credential"], true);
            assert_eq!(result["other_credential"], false);
            for field in selected.fields {
                assert_eq!(result["body"][field], "agent-a", "{method} {route} {field}");
            }
            for field in [
                "notify_target",
                "notify_session_id",
                "recipients",
                "target_session_id",
                "target_parent_session_id",
                "reviewer",
            ] {
                assert_eq!(
                    result["body"][field], body[field],
                    "{method} {route} {field}"
                );
            }
            accepted += 1;
        }
    }
    assert!(accepted > 60);
    assert!(crate::local_identity::current().is_none());
    assert!(!fixture
        .state
        .session_store
        .session_credential_matches("agent-a", "not-a-credential")
        .unwrap());
}

#[tokio::test]
async fn omitted_caller_and_sender_fields_are_supplied_and_self_paths_cannot_be_forged() {
    let fixture = Fixture::new();
    let app = echo_router(fixture.state.clone());
    for (method, path, field) in [
        ("POST", "/claims", "requester_session_id"),
        ("POST", "/sessions/agent-b/input", "sender_session_id"),
        ("POST", "/docs", "session_id"),
    ] {
        let result = app
            .clone()
            .oneshot(signed_request(
                "agent-a",
                method,
                path,
                json!({"notify_target":"agent-b"}),
            ))
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::OK);
        let value = json_response(result).await;
        assert_eq!(value["body"][field], "agent-a");
        assert_eq!(value["body"]["notify_target"], "agent-b");
    }
    for path in [
        "/sessions/agent-b/agent-status",
        "/sessions/agent-b/task-complete",
        "/sessions/%61gent-a/agent-status",
    ] {
        assert_eq!(
            app.clone()
                .oneshot(signed_request("agent-a", "POST", path, json!({})))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let result = app
        .clone()
        .oneshot(signed_request(
            "agent-a",
            "PATCH",
            "/sessions/agent-a",
            json!({"is_em":true}),
        ))
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::FORBIDDEN);
    let result = app
        .clone()
        .oneshot(signed_request(
            "agent-a",
            "POST",
            "/sessions/agent-b/input",
            json!({"parent_session_id":"agent-b","remind_cancel_on_reply_session_id":"agent-b"}),
        ))
        .await
        .unwrap();
    let body = json_response(result).await["body"].clone();
    assert!(body.get("parent_session_id").is_none());
    assert_eq!(body["remind_cancel_on_reply_session_id"], "agent-a");
    let response = app
        .oneshot(signed_request(
            "agent-a",
            "POST",
            "/scheduler/remind?session_id=agent-b&message=hello%20there&delay_seconds=5",
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(
        json_response(response).await["uri"],
        "/scheduler/remind?message=hello%20there&delay_seconds=5&session_id=agent-a"
    );
}

#[tokio::test]
async fn invalid_stamps_never_fall_back_to_owner_and_scope_is_request_local() {
    let fixture = Fixture::new();
    let app = echo_router(fixture.state.clone());
    let mut request = signed_request("agent-a", "GET", "/sessions", json!({}));
    request
        .headers_mut()
        .insert(SIGNATURE_HEADER, HeaderValue::from_static("forged"));
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(signed_request("retired", "GET", "/sessions", json!({})))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(signed_request(
                "agent-a",
                "POST",
                "/unclassified-new-route",
                json!({})
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let (a, b) = tokio::join!(
        app.clone()
            .oneshot(signed_request("agent-a", "GET", "/sessions", json!({}))),
        app.clone()
            .oneshot(signed_request("agent-b", "GET", "/sessions", json!({})))
    );
    assert_eq!(json_response(a.unwrap()).await["caller"], "agent-a");
    assert_eq!(json_response(b.unwrap()).await["caller"], "agent-b");
    let mut legacy = signed_request("agent-a", "GET", "/sessions", json!({}));
    legacy.headers_mut().remove(SIGNATURE_HEADER);
    legacy.headers_mut().remove("authorization");
    legacy.headers_mut().remove("cookie");
    let value = json_response(app.clone().oneshot(legacy).await.unwrap()).await;
    assert_eq!(value["caller"], Value::Null);
    assert_eq!(value["bypass"], true);
    fs::remove_file(fixture.directory.join("gateway.key")).unwrap();
    assert_eq!(
        app.oneshot(signed_request("agent-a", "GET", "/sessions", json!({})))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn real_router_confines_reminders_and_closes_queue_until_wall_binding_exists() {
    let fixture = Fixture::new();
    let app = router(fixture.state.clone());
    let response = app
        .clone()
        .oneshot(signed_request(
            "agent-a",
            "POST",
            "/scheduler/remind?session_id=agent-b&message=test&delay_seconds=600",
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = json_response(response).await;
    assert_eq!(value["session_id"], "agent-a");
    let target = format!(
        "/scheduler/remind/{}",
        value["reminder_id"].as_str().unwrap()
    );
    assert_eq!(
        app.clone()
            .oneshot(signed_request("agent-b", "DELETE", &target, json!({})))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(signed_request("agent-a", "DELETE", &target, json!({})))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let payload = json!({"cwd":fixture.directory,"argv":["/usr/bin/true"],"notify_target":"agent-b","requester_session_id":"agent-b"});
    assert_eq!(
        app.clone()
            .oneshot(signed_request("agent-a", "POST", "/queue-jobs", payload))
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    for route in [
        "/sessions",
        "/sessions/spawn",
        "/client/sessions",
        "/sessions/agent-a/subagents",
        "/hooks/claude",
        "/client/board/start",
    ] {
        assert_eq!(
            app.clone()
                .oneshot(signed_request("agent-a", "POST", route, json!({})))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "{route}"
        );
    }
    assert_eq!(
        app.clone()
            .oneshot(signed_request(
                "agent-a",
                "POST",
                "/sessions/agent-b/agent-status",
                json!({"text":"forged"})
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.oneshot(signed_request(
            "agent-a",
            "POST",
            "/sessions/agent-a/agent-status",
            json!({"text":"working"})
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn real_router_review_cancel_uses_requester_not_notification_recipient() {
    let fixture = Fixture::new();
    let app = router(fixture.state.clone());
    let db = expand_home(&fixture.state.config.sm_send.db_path);
    let review = RetainedQueueStore::create_codex_review_request_in_path(
        &db,
        CreateCodexReviewRequest {
            repo: "owner/repo".into(),
            pr_number: 42,
            requester_session_id: Some("agent-a".into()),
            notify_session_id: "agent-b".into(),
            steer: None,
            requested_head_sha: "a".repeat(40),
            latest_request_comment_id: None,
            latest_request_comment_url: None,
            latest_request_posted_at: now_rfc3339(),
            poll_interval_seconds: 30,
            retry_interval_seconds: 60,
        },
    )
    .unwrap();
    let path = format!("/review-requests/{}", review.id);
    let response = app
        .clone()
        .oneshot(signed_request(
            "agent-b",
            "DELETE",
            &path,
            json!({"requester_session_id":"agent-a"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        RetainedQueueStore::get_codex_review_request_from_path(&db, &review.id)
            .unwrap()
            .unwrap()
            .is_active
    );
    let response = app
        .oneshot(signed_request("agent-a", "DELETE", &path, json!({})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let saved = RetainedQueueStore::get_codex_review_request_from_path(&db, &review.id)
        .unwrap()
        .unwrap();
    assert!(!saved.is_active);
    assert_eq!(saved.notify_session_id, "agent-b");
}

#[tokio::test]
async fn real_router_preserves_parent_authorization_and_hosted_owner_requests() {
    let fixture = Fixture::new();
    let app = router(fixture.state.clone());
    let response = app
        .clone()
        .oneshot(signed_request(
            "agent-a",
            "POST",
            "/sessions/agent-b/retire",
            json!({}),
        ))
        .await
        .unwrap();
    let result = json_response(response).await;
    assert!(result.get("error").is_some(), "{result}");
    assert!(!fixture
        .state
        .session_store
        .get_session("agent-b")
        .unwrap()
        .unwrap()
        .is_stopped());
    let mut unsigned = signed_request(
        "agent-a",
        "POST",
        "/sessions/agent-b/agent-status",
        json!({"text":"hosted status"}),
    );
    unsigned.headers_mut().remove(SIGNATURE_HEADER);
    unsigned.headers_mut().remove("authorization");
    unsigned.headers_mut().remove("cookie");
    assert_eq!(
        app.clone().oneshot(unsigned).await.unwrap().status(),
        StatusCode::OK
    );
    for (method, path) in [
        ("GET", "/client/settings"),
        ("PUT", "/handoff-defaults"),
        ("POST", "/session-credential-rotations"),
        ("POST", "/inbox/agent/agent-b/send"),
    ] {
        assert_eq!(
            app.clone()
                .oneshot(signed_request("agent-a", method, path, json!({})))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "{method} {path}"
        );
    }
    let lan = terminal_lan_router(Arc::new(fixture.state.clone()));
    assert_eq!(
        lan.oneshot(signed_request(
            "agent-a",
            "GET",
            "/client/terminal/probe",
            json!({})
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn queue_identity_survives_storage_and_notify_cannot_cancel() {
    let fixture = Fixture::new();
    let root = fixture.directory.canonicalize().unwrap();
    let checkout = root.join("checkout");
    let agent_state = root.join("wall-a");
    fs::create_dir_all(&checkout).unwrap();
    fs::create_dir_all(&agent_state).unwrap();
    let profile = agent_state.join("wall.sb");
    let shell = agent_state.join("zsh");
    // This test exercises durable submission only; production-profile process
    // confinement is tested with the native launch fixture.
    fs::write(
        &profile,
        "(deny syscall-unix (syscall-number SYS_setsid SYS_setpgid SYS_posix_spawn))",
    )
    .unwrap();
    fs::write(&shell, "fixture shell, never executed").unwrap();
    let queue_dir = fixture.state.config.queue_runner_state_dir();
    fs::create_dir_all(&queue_dir).unwrap();
    let queue_dir = queue_dir.canonicalize().unwrap();
    let spec = crate::queue::local_wall::WallSpec {
        host_authority_sha256: None,
        agent_state,
        checkout: checkout.clone(),
        profile: profile.clone(),
        shell,
        environment: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
        gateway_port: 18600,
        egress_port: 18700,
    };
    let fingerprint =
        crate::queue::local_wall::register(&queue_dir, "agent-a", spec.clone()).unwrap();
    assert_eq!(
        crate::queue::local_wall::register(&queue_dir, "agent-a", spec.clone()).unwrap(),
        fingerprint
    );
    let mut changed = spec;
    changed.egress_port = 18701;
    assert!(crate::queue::local_wall::register(&queue_dir, "agent-a", changed).is_err());
    let app = router(fixture.state.clone());
    for requester in ["", "agent-b"] {
        let response = app.clone().oneshot(signed_request("agent-a", "POST", "/queue-jobs", json!({
            "type":"background", "cwd":checkout, "argv":["/usr/bin/true"],
            "requester_session_id":requester, "notify_target":"agent-b",
            "local_agent_id":"agent-b", "local_submitter":"agent-b",
            "env":{"CLAUDE_SESSION_MANAGER_ID":"agent-b","SM_API_URL":"http://127.0.0.1:8420","DYLD_INSERT_LIBRARIES":"/tmp/evil"}
        }))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_response(response).await;
        assert_eq!(body["requester_session_id"], "agent-a");
        assert_eq!(body["local_agent_id"], "agent-a");
        assert_eq!(body["notify_session_id"], "agent-b");
        let id = body["id"].as_str().unwrap();
        let conn = rusqlite::Connection::open(queue_dir.join("queue_runner.db")).unwrap();
        let (agent, binding, env): (String, String, String) = conn
            .query_row(
                "SELECT local_agent_id, local_binding_json, env_json FROM queue_jobs WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(agent, "agent-a");
        let binding: Value = serde_json::from_str(&binding).unwrap();
        assert_eq!(binding["wall_sha256"], fingerprint);
        let env: Value = serde_json::from_str(&env).unwrap();
        assert_eq!(env["CLAUDE_SESSION_MANAGER_ID"], "agent-a");
        assert_eq!(env["SM_API_URL"], "http://127.0.0.1:18600");
        assert!(env.get("DYLD_INSERT_LIBRARIES").is_none());
        let restored =
            RetainedQueueStore::get_queue_job_from_path(&queue_dir.join("queue_runner.db"), id)
                .unwrap()
                .unwrap();
        assert_eq!(restored.local_agent_id.as_deref(), Some("agent-a"));
        let target = format!("/queue-jobs/{id}/cancel");
        assert_eq!(
            app.clone()
                .oneshot(signed_request("agent-b", "POST", &target, json!({})))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone()
                .oneshot(signed_request("agent-a", "POST", &target, json!({})))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    fs::write(profile, "changed after registration").unwrap();
    assert!(crate::queue::local_wall::validate(&queue_dir, "agent-a").is_err());
}
