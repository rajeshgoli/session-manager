use super::*;
use crate::http::tests::{
    cloudflare_access_config, google_auth_config, local_request, public_request_with_host,
    seed_cloudflare_access_jwks, test_browser_access_assertion, test_mobile_access_assertion,
    write_session_state,
};
use axum::http::Method;
use tower::ServiceExt;

struct Fixture {
    app: Router,
    store: OwnerEventStore,
    bearer: String,
    guest_bearer: String,
}

/// An owner and a guest, each with an app login and an enrolled phone.
fn add_users(config: &mut AppConfig) {
    config
        .google_auth
        .allowlist_emails
        .push("guest@example.com".into());
    for (user_id, email, owner, device) in [
        ("rajesh", "rajeshgoli@gmail.com", true, "pixel-9"),
        ("guest", "guest@example.com", false, "guest-phone"),
    ] {
        config.mobile_terminal.allowed_users.insert(
            user_id.into(),
            MobileTerminalUserConfig {
                email: Some(email.into()),
                interactive_shell_access: true,
                mobile_terminal_owner: owner,
                registered_device_keys: vec![crate::config::MobileTerminalDeviceKeyConfig {
                    id: device.into(),
                    public_key: "fixture".into(),
                    enabled: true,
                }],
                ..Default::default()
            },
        );
    }
}

fn fixture() -> Fixture {
    let (state, db, bearer, guest_bearer) = owner_state();
    Fixture {
        app: router(state),
        store: OwnerEventStore::new(db),
        bearer,
        guest_bearer,
    }
}

/// The server state behind [`fixture`], its event database and both bearers.
fn owner_state() -> (AppState, PathBuf, String, String) {
    let mut config = google_auth_config();
    config.cloudflare_access = cloudflare_access_config().cloudflare_access;
    config.paths.state_file = write_session_state("fork1001", "running");
    let db = PathBuf::from(&config.paths.state_file).with_file_name("messages.db");
    config.sm_send.db_path = db.display().to_string();
    add_users(&mut config);
    let token = |email: &str| {
        format!(
            "Bearer {}",
            issue_device_access_token(&config, email, "Someone")
                .unwrap()
                .access_token
        )
    };
    let (bearer, guest_bearer) = (token("rajeshgoli@gmail.com"), token("guest@example.com"));
    let state = AppState::new(config);
    seed_cloudflare_access_jwks(&state);
    (state, db, bearer, guest_bearer)
}

fn request(
    method: Method,
    uri: &str,
    host: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> axum::http::Request<Body> {
    let mut request = public_request_with_host(method, uri, Body::from(body.to_owned()), host);
    for (name, value) in headers {
        request.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    request
}

impl Fixture {
    async fn send(&self, request: axum::http::Request<Body>) -> (StatusCode, Value) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    fn rows(&self) -> Vec<OwnerEvent> {
        self.store
            .list(&EventQuery {
                limit: log::MAX_LIMIT,
                ..EventQuery::default()
            })
            .unwrap()
            .0
    }

    /// Rows are written off the request path; wait for `count`, then a beat
    /// longer so a row that should not exist has had time to appear.
    async fn settled_rows(&self, count: usize) -> Vec<OwnerEvent> {
        for _ in 0..100 {
            if self.rows().len() >= count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.rows()
    }
}

#[tokio::test]
async fn records_owner_requests_by_surface_and_never_agents_or_strangers() {
    let f = fixture();
    let bearer = f.bearer.as_str();
    let owner =
        test_browser_access_assertion("sm-browser-aud", "rajeshgoli@gmail.com", 4_102_444_800);
    let stranger =
        test_browser_access_assertion("sm-browser-aud", "stranger@example.com", 4_102_444_800);
    let phone = test_mobile_access_assertion("pixel-9");
    let guest_phone = test_mobile_access_assertion("guest-phone");
    let guest = f.guest_bearer.as_str();
    // Not recorded: agents, loopback without owner auth, strangers,
    // background polls and static files.
    for skipped in [
        request(
            Method::GET,
            "/client/board",
            "testserver",
            &[("authorization", bearer), ("x-sm-session-id", "abc12345")],
            "",
        ),
        request(
            Method::GET,
            "/client/board",
            "testserver",
            &[("authorization", bearer), ("x-sm-session", "abc12345")],
            "",
        ),
        local_request(Method::GET, "/client/board", Body::empty()),
        request(
            Method::GET,
            "/client/board",
            "testserver",
            &[("authorization", guest)],
            "",
        ),
        request(
            Method::GET,
            "/client/board",
            "sm-app.example.com",
            &[("cf-access-jwt-assertion", &guest_phone)],
            "",
        ),
        request(
            Method::GET,
            "/client/board",
            "sm.example.com",
            &[("cf-access-jwt-assertion", &stranger)],
            "",
        ),
        request(
            Method::GET,
            "/client/host-status",
            "testserver",
            &[("authorization", bearer)],
            "",
        ),
        request(
            Method::GET,
            "/client/board",
            "testserver",
            &[("authorization", bearer), ("x-sm-bg", "1")],
            "",
        ),
        request(
            Method::GET,
            "/assets/app.js",
            "sm.example.com",
            &[("cf-access-jwt-assertion", &owner)],
            "",
        ),
    ] {
        f.send(skipped).await;
    }
    // Recorded, one per auth path.
    f.send(request(
        Method::GET,
        "/client/sessions/fork1001?secret=q1",
        "testserver",
        &[
            ("authorization", bearer),
            ("x-sm-screen", "session/detail"),
            ("x-sm-nav", "push:n_442"),
        ],
        "",
    ))
    .await;
    f.send(request(
        Method::GET,
        "/client/board",
        "sm-app.example.com",
        &[("cf-access-jwt-assertion", &phone), ("x-sm-nav", "launch")],
        "",
    ))
    .await;
    f.send(request(
        Method::GET,
        "/inbox?format=json",
        "sm.example.com",
        &[("cf-access-jwt-assertion", &owner)],
        "",
    ))
    .await;
    f.send(request(
        Method::POST,
        "/client/board/seen",
        "testserver",
        &[
            ("authorization", bearer),
            ("content-type", "application/json"),
        ],
        r#"{"note":"secret-body"}"#,
    ))
    .await;
    // A poll route with a client header is the client's own call.
    f.send(request(
        Method::GET,
        "/client/host-status",
        "testserver",
        &[("authorization", bearer), ("x-sm-screen", "board")],
        "",
    ))
    .await;

    let rows = f.settled_rows(5).await;
    let summary: Vec<_> = rows
        .iter()
        .map(|e| {
            (
                e.surface.as_str(),
                e.action.as_str(),
                e.target_id.as_deref(),
                e.device_id.as_deref(),
                e.screen.as_deref(),
                e.nav.as_deref(),
                e.notice_id.as_deref(),
            )
        })
        .collect();
    let expected = [
        (
            "android",
            "session.open",
            Some("fork1001"),
            None,
            Some("session/detail"),
            Some("push"),
            Some("n_442"),
        ),
        (
            "android",
            "board.open",
            None,
            Some("pixel-9"),
            None,
            Some("launch"),
            None,
        ),
        ("web", "inbox.open", None, None, None, None, None),
        ("android", "board.seen", None, None, None, None, None),
        (
            "android",
            "host.status",
            None,
            None,
            Some("board"),
            None,
            None,
        ),
    ];
    for row in expected {
        assert!(summary.contains(&row), "{row:?} not in {summary:?}");
    }
    assert_eq!(rows.len(), 5, "{summary:?}");
    assert!(rows
        .iter()
        .all(|e| e.status.is_some() && e.duration_ms.is_some()));
    let stored = serde_json::to_string(&rows).unwrap();
    assert!(!stored.contains("q1") && !stored.contains("secret-body") && !stored.contains('?'));
}

#[tokio::test]
async fn polls_without_screen_or_nav_coalesce_to_one_row_a_minute() {
    let f = fixture();
    let bearer = f.bearer.as_str();
    let board = |headers: &[(&str, &str)]| {
        let mut all = vec![("authorization", bearer)];
        all.extend_from_slice(headers);
        request(Method::GET, "/client/board", "testserver", &all, "")
    };
    for _ in 0..3 {
        f.send(board(&[])).await;
    }
    assert_eq!(f.settled_rows(1).await.len(), 1);
    // Rows carrying the client's screen or nav are never coalesced.
    for headers in [
        [("x-sm-screen", "board")],
        [("x-sm-screen", "board")],
        [("x-sm-nav", "nav")],
    ] {
        f.send(board(&headers)).await;
    }
    assert_eq!(f.settled_rows(4).await.len(), 4);
    f.send(board(&[])).await;
    // Another target is another row.
    let session = request(
        Method::GET,
        "/client/sessions/fork1001",
        "testserver",
        &[("authorization", bearer)],
        "",
    );
    f.send(session).await;
    let rows = f.settled_rows(5).await;
    assert_eq!(rows.len(), 5, "{rows:?}");
    assert_eq!(rows.iter().filter(|e| e.action == "board.open").count(), 4);
}

#[tokio::test]
async fn events_route_pages_with_the_cursor_and_is_owner_only() {
    let f = fixture();
    // Recent enough that the startup prune leaves them alone.
    let base = log::now_ms() - 1_000;
    for (at, action) in [(10, "a"), (20, "b"), (30, "c")] {
        f.store
            .record(&OwnerEvent {
                at_ms: base + at,
                surface: "web".into(),
                action: action.into(),
                ..OwnerEvent::default()
            })
            .unwrap();
    }
    let get = |uri: &str| local_request(Method::GET, uri, Body::empty());
    let (status, body) = f.send(get("/autopm/events?limit=2")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["events"].as_array().unwrap().len(), 2);
    assert_eq!(body["events"][1]["action"], "b");
    let cursor = body["next_cursor"].as_str().unwrap().to_owned();
    assert!(cursor.starts_with(&format!("{}:", base + 20)));
    let (_, body) = f
        .send(get(&format!("/autopm/events?limit=2&cursor={cursor}")))
        .await;
    assert_eq!(body["events"][0]["action"], "c");
    assert_eq!(body["next_cursor"], Value::Null);
    let (_, body) = f
        .send(get(&format!(
            "/autopm/events?since_ms={}&until_ms={}&surface=web&action=b",
            base + 15,
            base + 30
        )))
        .await;
    assert_eq!(body["events"].as_array().map(Vec::len), Some(1), "{body}");
    for uri in [
        "/autopm/events?limit=0",
        "/autopm/events?limit=10001",
        "/autopm/events?cursor=nope",
        "/autopm/events?surface=phone",
    ] {
        assert_eq!(f.send(get(uri)).await.0, StatusCode::BAD_REQUEST, "{uri}");
    }
    for header in ["x-sm-session", "x-sm-session-id"] {
        let mut request = get("/autopm/events");
        request
            .headers_mut()
            .insert(header, "abc12345".parse().unwrap());
        assert_eq!(f.send(request).await.0, StatusCode::FORBIDDEN, "{header}");
    }
    let guest = request(
        Method::GET,
        "/autopm/events",
        "testserver",
        &[("authorization", &f.guest_bearer)],
        "",
    );
    assert_eq!(f.send(guest).await.0, StatusCode::FORBIDDEN);
    let anonymous = request(Method::GET, "/autopm/events", "testserver", &[], "");
    assert_eq!(f.send(anonymous).await.0, StatusCode::UNAUTHORIZED);
    // A row still being written holds the feed back to just before it.
    let pending = f.store.begin();
    f.store
        .record(&OwnerEvent {
            at_ms: pending.at_ms + 1,
            surface: "web".into(),
            action: "late".into(),
            ..OwnerEvent::default()
        })
        .unwrap();
    let late = "/autopm/events?action=late";
    assert_eq!(f.send(get(late)).await.1["events"], json!([]));
    drop(pending);
    assert_eq!(f.send(get(late)).await.1["events"][0]["action"], "late");
    let owner = request(
        Method::GET,
        "/autopm/events",
        "testserver",
        &[("authorization", &f.bearer)],
        "",
    );
    assert_eq!(f.send(owner).await.0, StatusCode::OK);
}

#[tokio::test]
async fn owner_typed_claude_prompts_are_recorded_without_their_text() {
    let f = fixture();
    for prompt in ["ship it", "[Input from: agent] relayed", "  "] {
        let body = json!({
            "session_manager_id": "fork1001",
            "hook_event_name": "UserPromptSubmit",
            "prompt": prompt,
        });
        let mut hook = local_request(Method::POST, "/hooks/claude", Body::from(body.to_string()));
        hook.headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        let (status, body) = f.send(hook).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let rows = f.settled_rows(1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (
            rows[0].surface.as_str(),
            rows[0].action.as_str(),
            rows[0].target_id.as_deref()
        ),
        ("agent_prompt", "agent.prompt", Some("fork1001"))
    );
    assert!(!serde_json::to_string(&rows).unwrap().contains("ship"));
}

#[test]
fn only_owner_users_count_as_the_owner() {
    let mut config = google_auth_config();
    // With no users configured, the allowlist is the owner.
    assert!(is_owner_email(&config, "rajeshgoli@gmail.com"));
    add_users(&mut config);
    assert!(is_owner_email(&config, "rajeshgoli@gmail.com"));
    assert!(!is_owner_email(&config, "guest@example.com"));
    assert!(!is_owner_email(&config, "stranger@example.com"));
    assert!(is_owner_user_id(&config, "rajesh"));
    assert!(!is_owner_user_id(&config, "guest"));
    assert!(!is_owner_user_id(&config, ""));
}

#[tokio::test]
async fn events_route_needs_the_owner_without_google_auth() {
    let mut config = cloudflare_access_config();
    config.google_auth.allowlist_emails = vec!["rajeshgoli@gmail.com".into()];
    add_users(&mut config);
    config.paths.state_file = write_session_state("fork1001", "running");
    config.sm_send.db_path = PathBuf::from(&config.paths.state_file)
        .with_file_name("messages.db")
        .display()
        .to_string();
    assert!(!config.google_auth.requested());
    let state = AppState::new(config);
    seed_cloudflare_access_jwks(&state);
    let app = router(state);
    for (email, expected) in [
        ("guest@example.com", StatusCode::FORBIDDEN),
        ("rajeshgoli@gmail.com", StatusCode::OK),
    ] {
        let assertion = test_browser_access_assertion("sm-browser-aud", email, 4_102_444_800);
        let request = request(
            Method::GET,
            "/autopm/events",
            "sm.example.com",
            &[("cf-access-jwt-assertion", &assertion)],
            "",
        );
        let status = app.clone().oneshot(request).await.unwrap().status();
        assert_eq!(status, expected, "{email}");
    }
}

#[tokio::test]
async fn owner_app_names_the_phone_the_web_app_or_neither() {
    use crate::owner_messages::OwnerApp;
    let (state, _, bearer, _) = owner_state();
    let state = Arc::new(state);
    let browser =
        test_browser_access_assertion("sm-browser-aud", "rajeshgoli@gmail.com", 4_102_444_800);
    let phone = test_mobile_access_assertion("pixel-9");
    let peer: SocketAddr = "203.0.113.9:443".parse().unwrap();
    for (host, headers, expected) in [
        (
            "testserver",
            vec![("authorization", bearer.as_str())],
            OwnerApp::Phone,
        ),
        (
            "sm-app.example.com",
            vec![("cf-access-jwt-assertion", phone.as_str())],
            OwnerApp::Phone,
        ),
        (
            "sm.example.com",
            vec![("cf-access-jwt-assertion", browser.as_str())],
            OwnerApp::Web,
        ),
        ("sm.example.com", vec![], OwnerApp::Unknown),
    ] {
        let request = request(
            Method::POST,
            "/inbox/agent/fork1001/send",
            host,
            &headers,
            "",
        );
        assert_eq!(
            owner_app(&state, request.headers(), peer).await,
            expected,
            "{host} {headers:?}"
        );
    }
}
