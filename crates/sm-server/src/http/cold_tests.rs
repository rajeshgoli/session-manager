use super::*;

fn cold_fixture() -> AppState {
    let mut config = context_monitor_test_config("claude", "stopped");
    config.usage.enabled = false;
    config.codex_requests.db_path = config.sm_send.db_path.clone();
    let sessions = ["anchor", "cold-x", "cold-y", "successor", "cold-notify", "attachable"]
        .map(|id| json!({
            "id":id,"name":if id=="cold-x" || id=="cold-y" {"same-name"} else {id},
            "provider":if id=="attachable" {"codex-fork"} else {"claude"},
            "working_dir":"/fixture/repo","tmux_session":format!("tmux-{id}"),
            "created_at":"2025-01-01T00:00:00Z","last_activity":"2025-01-01T00:00:00Z",
            "stopped_at":"2025-01-01T00:00:00Z","completed_at":"2025-01-01T00:00:00Z",
            "completion_status":if id=="attachable" {None} else {Some("retired")},
            "status":"stopped", "is_em":id=="anchor" || id=="successor",
            "parent_session_id":if id=="cold-x" || id=="cold-y" {Some("anchor")} else {None},
            "context_monitor_notify":if id=="cold-notify" {Some("anchor")} else {None}
        }));
    fs::write(&config.paths.state_file, json!({"sessions":sessions,"handoff_notes_imported_at":"2025-01-01T00:00:00Z"}).to_string()).unwrap();
    let state = AppState::new(config);
    for id in ["cold-x", "cold-y", "cold-notify"] {
        RetainedQueueStore::new(expand_home(&state.config.sm_send.db_path)).enqueue_message(id, "retained", "important", Some("queue-completion")).unwrap();
    }
    state
}

fn normalize_cold_times(value: &mut Value) {
    match value {
        Value::Object(object) => for (key, value) in object {
            if matches!(key.as_str(), "generated_at" | "last_activity" | "friendly_name_updated_at_ns") {
                *value = json!("<clock>");
            } else { normalize_cold_times(value); }
        },
        Value::Array(array) => array.iter_mut().for_each(normalize_cold_times),
        _ => {},
    }
}

async fn cold_request(state: &AppState, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
    let mut request = local_request(method, path, Body::from(body.to_string()));
    request.headers_mut().insert(CONTENT_TYPE, "application/json".parse().unwrap());
    let (status, mut body) = response_json(router(state.clone()).oneshot(request).await.unwrap()).await;
    normalize_cold_times(&mut body);
    (status, body)
}

fn cold_queue_effects(state: &AppState) -> Vec<(String, String, String, Option<String>, bool)> {
    let db = rusqlite::Connection::open(expand_home(&state.config.sm_send.db_path)).unwrap();
    let mut stmt = db.prepare("SELECT target_session_id,text,delivery_mode,message_category,delivered_at IS NOT NULL FROM message_queue ORDER BY rowid").unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
        .unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

#[tokio::test]
async fn cold_stopped_session_routes_and_work_claims_are_equivalent() {
    let hot = cold_fixture();
    let cold = cold_fixture();
    assert_eq!(cold.session_store.sweep_cold_sessions().unwrap(), 3);
    for path in [
        "/sessions?include_stopped=true", "/history/agents?format=json", "/watch/state",
        "/sessions/anchor/children?include_terminated=true&recursive=true",
        "/nodes/primary/restore-candidates", "/sessions/cold-x", "/client/sessions/cold-y",
        "/worktrees/leftover",
    ] {
        let a = cold_request(&hot, Method::GET, path, Value::Null).await;
        let b = cold_request(&cold, Method::GET, path, Value::Null).await;
        assert_eq!(a.0, StatusCode::OK, "{path}: {:?}", a.1);
        assert_eq!(a, b, "{path}");
    }
    for name in ["cold-x", "cold-y", "same-name", "anchor", "attachable"] {
        assert_eq!(serde_json::to_value(hot.session_store.get_session(name).unwrap()).unwrap(),
            serde_json::to_value(cold.session_store.get_session(name).unwrap()).unwrap(), "{name}");
    }
    let a = claims::session_directory(&hot).unwrap();
    let b = claims::session_directory(&cold).unwrap();
    for id in ["anchor", "cold-x", "cold-y", "cold-notify"] {
        assert_eq!(a.get(id), b.get(id));
    }
    assert_eq!(hot.session_store.test_merged_json(), cold.session_store.test_merged_json());
    assert_eq!(cold_queue_effects(&hot), cold_queue_effects(&cold));
}

#[tokio::test]
async fn cold_restore_retire_kill_and_patch_match_hot_state_and_queue_effects() {
    for (method, path, body) in [
        (Method::PATCH, "/sessions/cold-x", json!({"is_em":false})),
        (Method::POST, "/sessions/cold-x/retire", json!({})),
        (Method::POST, "/sessions/cold-x/kill", json!({})),
        (Method::POST, "/sessions/cold-x/restore", json!({})),
    ] {
        let hot = cold_fixture();
        let cold = cold_fixture();
        cold.session_store.sweep_cold_sessions().unwrap();
        let a = cold_request(&hot, method.clone(), path, body.clone()).await;
        let b = cold_request(&cold, method, path, body).await;
        assert_eq!(a.0, StatusCode::OK, "{path}: {:?}", a.1);
        assert_eq!(a, b, "{path}");
        let mut a = hot.session_store.test_merged_json();
        let mut b = cold.session_store.test_merged_json();
        normalize_cold_times(&mut a);
        normalize_cold_times(&mut b);
        assert_eq!(a, b, "{path}");
        assert_eq!(cold_queue_effects(&hot), cold_queue_effects(&cold), "{path}");
    }
}

#[test]
fn cold_handoff_repoints_children_and_nonchild_notification_targets() {
    let hot = cold_fixture();
    let cold = cold_fixture();
    cold.session_store.sweep_cold_sessions().unwrap();
    for state in [&hot, &cold] {
        state.session_store.transfer_handoff_json("anchor", "successor").unwrap();
        let merged = state.session_store.test_merged_json();
        assert_eq!(merged["sessions"][1]["parent_session_id"], "successor");
        assert_eq!(merged["sessions"][2]["parent_session_id"], "successor");
        assert_eq!(merged["sessions"][4]["context_monitor_notify"], "successor");
    }
    assert_eq!(hot.session_store.test_merged_json(), cold.session_store.test_merged_json());
    assert_eq!(cold_queue_effects(&hot), cold_queue_effects(&cold));
}
