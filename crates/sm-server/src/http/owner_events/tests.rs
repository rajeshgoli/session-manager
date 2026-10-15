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
}

fn fixture() -> Fixture {
    let mut config = google_auth_config();
    config.cloudflare_access = cloudflare_access_config().cloudflare_access;
    config.paths.state_file = write_session_state("fork1001", "running");
    let db = PathBuf::from(&config.paths.state_file).with_file_name("messages.db");
    config.sm_send.db_path = db.display().to_string();
    let bearer = format!(
        "Bearer {}",
        issue_device_access_token(&config, "rajeshgoli@gmail.com", "Rajesh")
            .unwrap()
            .access_token
    );
    let state = AppState::new(config);
    seed_cloudflare_access_jwks(&state);
    Fixture {
        app: router(state),
        store: OwnerEventStore::new(db),
        bearer,
    }
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
    let anonymous = request(Method::GET, "/autopm/events", "testserver", &[], "");
    assert_eq!(f.send(anonymous).await.0, StatusCode::UNAUTHORIZED);
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
