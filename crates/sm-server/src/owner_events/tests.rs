use super::*;
use std::collections::BTreeSet;

fn temp_db(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sm-owner-events-{name}-{}-{}",
        std::process::id(),
        now_ms()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir.join("messages.db")
}

fn event(at_ms: i64, surface: &str, action: &str) -> OwnerEvent {
    OwnerEvent {
        at_ms,
        surface: surface.into(),
        action: action.into(),
        ..OwnerEvent::default()
    }
}

fn query(limit: usize) -> EventQuery {
    EventQuery {
        limit,
        ..EventQuery::default()
    }
}

#[test]
fn list_orders_by_time_then_id_and_cursor_continues_strictly_after() {
    let store = OwnerEventStore::new(temp_db("cursor"));
    assert_eq!(store.list(&query(10)).unwrap(), (Vec::new(), None));
    for (at, action) in [(30, "c"), (10, "a"), (20, "b1"), (20, "b2"), (40, "d")] {
        store.record(&event(at, "web", action)).unwrap();
    }
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (events, next) = store
            .list(&EventQuery {
                after: cursor.as_deref().and_then(parse_cursor),
                ..query(2)
            })
            .unwrap();
        seen.extend(events.iter().map(|e| e.action.clone()));
        if let (Some(last), Some(next)) = (events.last(), next.as_ref()) {
            assert_eq!(next, &format!("{}:{}", last.at_ms, last.id));
        }
        match next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, ["a", "b1", "b2", "c", "d"]);
    // A full last page still ends with a null cursor.
    let (events, next) = store.list(&query(5)).unwrap();
    assert_eq!((events.len(), next), (5, None));
}

#[test]
fn list_filters_by_window_surface_and_action() {
    let store = OwnerEventStore::new(temp_db("filter"));
    store.record(&event(10, "web", "doc.open")).unwrap();
    store.record(&event(20, "android", "doc.open")).unwrap();
    store.record(&event(30, "android", "inbox.open")).unwrap();
    let rows = |q: EventQuery| -> Vec<(i64, String)> {
        store
            .list(&q)
            .unwrap()
            .0
            .into_iter()
            .map(|e| (e.at_ms, e.surface))
            .collect()
    };
    let window = EventQuery {
        since_ms: Some(20),
        until_ms: Some(30),
        ..query(10)
    };
    assert_eq!(rows(window), [(20, "android".to_owned())]);
    let filtered = EventQuery {
        surface: Some("android".into()),
        action: Some("doc.open".into()),
        ..query(10)
    };
    assert_eq!(rows(filtered), [(20, "android".to_owned())]);
    assert!(store.record(&event(1, "phone", "x")).is_err());
}

#[test]
fn repeats_within_a_minute_coalesce_by_surface_device_action_and_target() {
    let store = OwnerEventStore::new(temp_db("repeat"));
    let at = |at_ms: i64, device: Option<&str>, target: Option<&str>| OwnerEvent {
        at_ms,
        device_id: device.map(str::to_owned),
        target_kind: target.map(|_| "session".to_owned()),
        target_id: target.map(str::to_owned),
        ..event(at_ms, "android", "board.open")
    };
    let t = 1_800_000_000_000;
    let inserted =
        |e: OwnerEvent, coalesce: bool| store.record_unless_repeat(&e, coalesce).unwrap().is_some();
    assert!(inserted(at(t, None, None), true));
    assert!(!inserted(at(t + 5_000, None, None), true));
    assert!(!inserted(at(t + REPEAT_WINDOW_MS - 1, None, None), true));
    // Uncoalesced rows always land.
    assert!(inserted(at(t + 6_000, None, None), false));
    // A different device or target is a different row.
    assert!(inserted(at(t + 7_000, Some("pixel"), None), true));
    assert!(!inserted(at(t + 8_000, Some("pixel"), None), true));
    assert!(inserted(at(t + 9_000, None, Some("s1")), true));
    let web = OwnerEvent {
        surface: "web".into(),
        ..at(t + 10_000, None, None)
    };
    assert!(inserted(web, true));
    // A minute after the last row, the poll shows again.
    assert!(inserted(at(t + 6_000 + REPEAT_WINDOW_MS, None, None), true));
    assert_eq!(store.list(&query(100)).unwrap().0.len(), 6);
}

#[test]
fn rows_still_being_recorded_hold_back_the_settled_time() {
    let store = OwnerEventStore::new(temp_db("settled"));
    let other = OwnerEventStore::new(temp_db("settled-other"));
    let before = now_ms();
    assert!(store.settled_before() >= before);
    let first = store.begin();
    let second = store.begin();
    assert!(second.at_ms >= first.at_ms);
    // Another database's rows do not hold this one back.
    let _elsewhere = other.begin();
    assert_eq!(store.settled_before(), first.at_ms);
    drop(first);
    assert_eq!(store.settled_before(), second.at_ms);
    drop(second);
    assert!(store.settled_before() >= before);
}

#[test]
fn prune_deletes_only_rows_past_180_days() {
    let store = OwnerEventStore::new(temp_db("prune"));
    // A missing database stays missing.
    assert_eq!(store.prune(now_ms()).unwrap(), 0);
    assert!(!store.db_path.exists());
    let now = 1_800_000_000_000;
    store
        .record(&event(now - RETENTION_MS - 1, "web", "old"))
        .unwrap();
    store
        .record(&event(now - RETENTION_MS, "web", "edge"))
        .unwrap();
    store.record(&event(now, "web", "new")).unwrap();
    assert_eq!(store.prune(now).unwrap(), 1);
    let left: Vec<_> = store
        .list(&query(10))
        .unwrap()
        .0
        .into_iter()
        .map(|e| e.action)
        .collect();
    assert_eq!(left, ["edge", "new"]);
}

#[test]
fn terminal_input_coalesces_per_session_and_prompts_do_not() {
    let db = temp_db("terminal");
    for _ in 0..3 {
        record_terminal_input(&db, "s1");
        record_agent_prompt(&db, "s1");
    }
    record_terminal_input(&db, "s2");
    let events = OwnerEventStore::new(db).list(&query(100)).unwrap().0;
    let count = |surface: &str, action: &str, session: &str| {
        events
            .iter()
            .filter(|e| {
                e.surface == surface
                    && e.action == action
                    && e.target_id.as_deref() == Some(session)
            })
            .count()
    };
    assert_eq!(count("terminal", "terminal.input", "s1"), 1);
    assert_eq!(count("terminal", "terminal.input", "s2"), 1);
    assert_eq!(count("agent_prompt", "agent.prompt", "s1"), 3);
    assert_eq!(events.len(), 5);
    assert!(events
        .iter()
        .all(|e| e.target_kind.as_deref() == Some("session")
            && e.status.is_none()
            && e.duration_ms.is_none()
            && e.device_id.is_none()));
}

#[test]
fn nav_header_values() {
    assert_eq!(
        parse_nav(Some("push:n_442")),
        (Some("push".into()), Some("n_442".into()))
    );
    for value in ["launch", "link", "nav"] {
        assert_eq!(parse_nav(Some(value)), (Some(value.into()), None));
    }
    for value in [None, Some("deep"), Some("")] {
        assert_eq!(parse_nav(value), (None, None));
    }
    assert_eq!(clip(&"x".repeat(500)).len(), MAX_HEADER_CHARS);
}

#[test]
fn background_is_the_bg_header_or_a_poll_route_without_client_headers() {
    let none: [String; 0] = [];
    assert!(is_background("/client/board", &none, Some("1"), true));
    assert!(!is_background("/client/board", &none, None, false));
    assert!(!is_background("/client/board", &none, Some("0"), true));
    for route in POLL_ROUTES {
        assert!(is_background(route, &none, None, false), "{route}");
        // A client that sends headers marks its own polls.
        assert!(!is_background(route, &none, None, true), "{route}");
    }
    assert!(is_background(
        "/assets/{*name}",
        &["app.js".into()],
        None,
        true
    ));
    let doc = |rest: &str| ["session-manager".to_owned(), rest.to_owned()];
    let docs = "/docs/{doc_id}/{*rest}";
    assert!(is_background(docs, &doc("docs/img/a.png"), None, false));
    assert!(!is_background(
        docs,
        &doc("docs/working/m.html"),
        None,
        false
    ));
    assert!(!is_background(docs, &doc("README.md"), None, false));
    let head = ["abcd1234".to_owned(), "head".to_owned()];
    assert!(!is_background(docs, &head, None, false));
}

#[test]
fn resolve_names_targets_and_falls_back_to_the_template() {
    let check = |method: &str,
                 route: &str,
                 values: &[&str],
                 expected: (&str, Option<&str>, Option<&str>)| {
        let params: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        let resolved = resolve(method, route, &params);
        assert_eq!(
            (
                resolved.action.as_str(),
                resolved.target_kind,
                resolved.target_id.as_deref()
            ),
            expected,
            "{method} {route}"
        );
    };
    let session = "/client/sessions/{session_id}";
    check(
        "GET",
        session,
        &["s1"],
        ("session.open", Some("session"), Some("s1")),
    );
    let doc = "/docs/{doc_id}";
    check(
        "HEAD",
        doc,
        &["abcd1234"],
        ("doc.open", Some("doc"), Some("abcd1234")),
    );
    let reply = "/inbox/thread/{key}/send";
    check(
        "POST",
        reply,
        &["t_91"],
        ("inbox.reply", Some("thread"), Some("t_91")),
    );
    check(
        "PUT",
        "/client/board/order",
        &[],
        ("board.order", None, None),
    );
    let sub = "/docs/{doc_id}/{*rest}";
    check(
        "GET",
        sub,
        &["abcd1234", "view"],
        ("doc.open", Some("doc"), Some("abcd1234")),
    );
    check(
        "POST",
        sub,
        &["abcd1234", "review"],
        ("doc.review", Some("doc"), Some("abcd1234")),
    );
    check(
        "POST",
        sub,
        &["abcd1234", "agent/retire"],
        ("doc.agent_retire", Some("doc"), Some("abcd1234")),
    );
    check(
        "GET",
        sub,
        &["far", "docs/2102_plan.html"],
        ("doc.open", Some("doc"), Some("far/docs/2102_plan.html")),
    );
    check(
        "GET",
        "/client/github/{owner}/{repo}/{number}",
        &["rajeshgoli", "session-manager", "2173"],
        (
            "ticket.open",
            Some("ticket"),
            Some("rajeshgoli/session-manager#2173"),
        ),
    );
    check(
        "GET",
        "/t/{repo}/{number}",
        &["session-manager", "2173"],
        (
            "ticket.timeline",
            Some("ticket"),
            Some("session-manager#2173"),
        ),
    );
    check(
        "GET",
        "/client/future",
        &[],
        ("GET /client/future", None, None),
    );
}

/// Every `(method, route)` the router registers, read from its source.
fn registered_routes() -> BTreeSet<(String, String)> {
    let source = include_str!("../http.rs")
        .split("pub fn router(state: AppState)")
        .nth(1)
        .unwrap()
        .split(".fallback(not_found)")
        .next()
        .unwrap();
    let route = regex::Regex::new(r#"\.route\(\s*"([^"]+)""#).unwrap();
    let method =
        regex::Regex::new(r"(?:^|[\s(.,]|routing::)(get|post|put|patch|delete)\(").unwrap();
    let starts: Vec<_> = route.captures_iter(source).collect();
    let mut routes = BTreeSet::new();
    for (index, capture) in starts.iter().enumerate() {
        let whole = capture.get(0).unwrap();
        let end = starts
            .get(index + 1)
            .map(|next| next.get(0).unwrap().start())
            .unwrap_or(source.len());
        for found in method.captures_iter(&source[whole.end()..end]) {
            routes.insert((found[1].to_uppercase(), capture[1].to_owned()));
        }
    }
    routes
}

#[test]
fn every_registered_route_maps_to_a_named_action() {
    let routes = registered_routes();
    assert!(routes.len() > 200, "{}", routes.len());
    let mapped: BTreeSet<_> = ACTIONS
        .iter()
        .map(|(method, route, _, _)| (method.to_string(), route.to_string()))
        .collect();
    assert_eq!(mapped.len(), ACTIONS.len(), "duplicate map entries");
    let missing: Vec<_> = routes.difference(&mapped).collect();
    assert!(missing.is_empty(), "routes without an action: {missing:?}");
    // The inbound email webhook registers through a constant.
    let stale: Vec<_> = mapped
        .difference(&routes)
        .filter(|(_, route)| route != crate::email::DEFAULT_EMAIL_WEBHOOK_PATH)
        .collect();
    assert!(stale.is_empty(), "map entries with no route: {stale:?}");
    for (method, route, action, _) in ACTIONS {
        assert!(
            action.contains('.') && !action.contains(' '),
            "{method} {route}: {action}"
        );
    }
}
