//! Golden read results captured before the shared-view implementation.
use super::*;

fn unique_temp_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{label}-{}-{}.json",
        std::process::id(),
        STATE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

#[test]
fn session_reads_golden_equivalence() {
    let path = unique_temp_path("session-reads-golden");
    let sessions = (0..200).map(|i| json!({
        "id": format!("read{i:04}"), "name": format!("name{}", i / 2),
        "friendly_name": format!("display{}", i / 2), "native_title": format!("old{i}"),
        "working_dir": "/repo", "tmux_session": format!("sm-{i}"),
        "created_at": "2026-10-01T00:00:00Z", "last_activity": "2026-10-01T00:00:00Z",
        "status": if i > 0 && i % 2 == 0 { "stopped" } else { "running" },
        "completion_status": if i > 0 && i % 4 == 0 { Some("retired") } else { None },
        "parent_session_id": if i > 0 { Some(format!("read{:04}", (i - 1) / 3)) } else { None },
        "predecessor_session_id": if i == 5 { Some("read0004") } else { None },
        "successor_session_id": if i == 4 { Some("read0005") } else { None },
        "handoff_policy_override": { "enabled": i % 3 == 0 },
        "handoff": if i == 4 { Some(json!({"state":"done", "trigger":"context", "asked_at":"2026-10-01T00:00:00Z", "successor_session_id":"read0005"})) } else { None }
    })).collect::<Vec<_>>();
    fs::write(&path, json!({"sessions": sessions,
        "maintainer_session_id": "read0001",
        "agent_registrations": [{"role":"scout", "session_id":"read0003", "registered_at":"2026-10-01T00:00:00Z"}],
        "adoption_proposals": [{"id":"proposal", "target_session_id":"read0007", "proposer_session_id":"read0003", "status":"pending", "created_at":"2026-10-01T00:00:00Z"}]
    }).to_string()).unwrap();
    let store = SessionStore::new(path.clone());
    let mut results = BTreeMap::new();
    let mut capture = |key: String, value: Value| {
        let bytes = serde_json::to_vec(&value).unwrap();
        results.insert(key, format!("{:x}", Sha256::digest(bytes)));
    };
    for i in 0..200 {
        for key in [
            format!("read{i:04}"),
            format!("name{}", i / 2),
            format!("display{}", i / 2),
            format!("old{i}"),
        ] {
            capture(
                format!("get/{key}"),
                serde_json::to_value(store.get_session(&key).unwrap()).unwrap(),
            );
        }
        capture(
            format!("root/{i}"),
            json!(format!(
                "{:?}",
                store
                    .resolve_hierarchy_root(&format!("read{i:04}"))
                    .unwrap()
            )),
        );
    }
    for key in ["maintainer", "scout", "missing", "", " read0001 "] {
        capture(
            format!("get/{key}"),
            serde_json::to_value(store.get_session(key).unwrap()).unwrap(),
        );
    }
    for stopped in [true, false] {
        capture(
            format!("list/{stopped}"),
            serde_json::to_value(store.list_sessions(stopped).unwrap()).unwrap(),
        );
        for recursive in [true, false] {
            for status in [None, Some("running"), Some("completed"), Some("error")] {
                capture(
                    format!("children/{stopped}/{recursive}/{status:?}"),
                    serde_json::to_value(
                        store
                            .list_child_records("read0000", recursive, status, stopped)
                            .unwrap(),
                    )
                    .unwrap(),
                );
            }
        }
    }
    let actual = serde_json::to_string_pretty(&results).unwrap();
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/session_reads_golden.json");
    if std::env::var_os("SM_CAPTURE_READ_GOLDEN").is_some() {
        fs::write(&golden, &actual).unwrap();
    }
    assert_eq!(actual, fs::read_to_string(golden).unwrap());
    fs::remove_file(path).unwrap();
}

#[test]
fn stores_share_paths_cache_and_serialized_updates() {
    let dir = unique_temp_path("shared-store").with_extension("dir");
    fs::create_dir_all(&dir).unwrap();
    let link = dir.with_extension("link");
    std::os::unix::fs::symlink(&dir, &link).unwrap();
    // Construct before the file exists, through both ancestor spellings.
    let first = SessionStore::new(dir.join("sessions.json"));
    let second = SessionStore::new(link.join("./sessions.json"));
    assert!(Arc::ptr_eq(&first.shared, &second.shared));
    first
        .write_raw_json_value(&json!({"sessions":[], "count":0}))
        .unwrap();
    assert!(Arc::ptr_eq(
        &first.load_parsed_state().unwrap(),
        &second.load_parsed_state().unwrap()
    ));
    let guard = first.write_guard().unwrap();
    assert!(second.shared.write_lock.try_lock().is_err());
    drop(guard);
    let workers = [first.clone(), second.clone()].map(|store| {
        thread::spawn(move || {
            for _ in 0..10 {
                let _guard = store.write_guard().unwrap();
                let mut raw = store.load_raw_json_value().unwrap();
                raw["count"] = json!(raw["count"].as_u64().unwrap() + 1);
                store.write_raw_json_value(&raw).unwrap();
            }
        })
    });
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(first.load_parsed_state().unwrap().raw["count"], 20);
    assert_eq!(first.shared.parses.load(Ordering::Relaxed), 0);
    // A symlink directly to the state file also shares it and writes its target.
    let file_link = dir.join("file-link.json");
    std::os::unix::fs::symlink(&first.state_file, &file_link).unwrap();
    let third = SessionStore::new(file_link.clone());
    assert!(Arc::ptr_eq(&first.shared, &third.shared));
    third
        .write_raw_json_value(&json!({"sessions":[], "count":21}))
        .unwrap();
    assert!(fs::symlink_metadata(file_link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(second.load_parsed_state().unwrap().raw["count"], 21);
    fs::remove_file(link).unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn writes_install_normalized_state_without_parsing() {
    let path = unique_temp_path("normalized-install");
    let store = SessionStore::new(path.clone());
    let raw = json!({"sessions":[{"id":"local", "provider":"opencode", "status":"running", "reasoning_effort":"high", "last_activity":"2026-10-01T00:00:00Z"}]});
    store.write_raw_json_value(&raw).unwrap();
    let installed = store.load_parsed_state().unwrap();
    let disk = ParsedState::new(serde_json::from_slice(&fs::read(&path).unwrap()).unwrap());
    assert_eq!(installed.raw, disk.raw);
    assert_eq!(installed.raw["sessions"][0]["status"], "stopped");
    assert_eq!(store.shared.parses.load(Ordering::Relaxed), 0);
    fs::remove_file(path).unwrap();
}

#[test]
fn write_install_rejects_interleaved_external_writers() {
    for rename in [true, false] {
        let path = unique_temp_path("interleaved-write");
        let store = SessionStore::new(path.clone());
        let ours = json!({"sessions":[], "writer":"ours"});
        let theirs = json!({"sessions":[], "writer":"them"});
        store
            .write_raw_json_value_after_rename(&ours, || {
                let bytes = serde_json::to_vec_pretty(&theirs).unwrap();
                if rename {
                    let replacement = path.with_extension("replacement");
                    fs::write(&replacement, bytes).unwrap();
                    fs::rename(replacement, &path).unwrap();
                } else {
                    // Same-length and same-inode rewrite, with a later mtime.
                    fs::write(&path, bytes).unwrap();
                    fs::File::options()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_modified(std::time::SystemTime::now() + Duration::from_secs(1))
                        .unwrap();
                }
            })
            .unwrap();
        assert_eq!(store.load_parsed_state().unwrap().raw, theirs);
        assert_eq!(store.shared.parses.load(Ordering::Relaxed), 1);
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn borrowed_registration_checks_match_repairs_and_skip_the_write_lock() {
    let path = unique_temp_path("registration-checks");
    let store = SessionStore::new(path.clone());
    for legacy in [Value::Null, json!(""), json!("live"), json!("dead")] {
        for registrations in [
            json!([]),
            json!([{"role":"maintainer","session_id":"live","created_at":"2026-10-01T00:00:00Z"}]),
            json!([{"role":"scout","session_id":"dead"}]),
        ] {
            let raw = json!({"sessions":[
                {"id":"live", "name":"maintainer", "role":"maintainer", "status":"running", "working_dir":"/repo", "tmux_session":"live", "created_at":"2026-10-01T00:00:00Z", "last_activity":"2026-10-01T00:00:00Z"},
                {"id":"dead", "name":"dead", "status":"stopped", "working_dir":"/repo", "tmux_session":"dead", "created_at":"2026-10-01T00:00:00Z", "last_activity":"2026-10-01T00:00:00Z"}],
                "maintainer_session_id":legacy, "agent_registrations":registrations});
            let mut repaired = raw.clone();
            let mut changed = recover_missing_maintainer_registration_raw(&mut repaired).unwrap();
            changed |= prune_agent_registrations_raw(&mut repaired).unwrap();
            let view = ParsedState::new(raw.clone()).view().unwrap();
            assert_eq!(
                agent_registrations_need_repair(&raw, &view.sessions).unwrap(),
                changed
            );
            store.write_raw_json_value(&raw).unwrap();
            let attempts = store.write_lock_attempts.load(Ordering::Relaxed);
            let actual = store.list_agent_registrations().unwrap();
            let expected = agent_registration_responses_from_state(&repaired).unwrap();
            // Repairs synthesize current timestamps; compare the stable response fields.
            let stable = |rows: Vec<AgentRegistrationResponse>| {
                rows.into_iter()
                    .map(|row| (row.role, row.session_id, row.status))
                    .collect::<Vec<_>>()
            };
            assert_eq!(stable(actual), stable(expected));
            assert_eq!(
                store.write_lock_attempts.load(Ordering::Relaxed) - attempts,
                u64::from(changed)
            );
        }
    }
    fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn watch_handoff_inheritance_stays_fresh_without_session_writes() {
    use crate::handoff::policy::HandoffOverride;
    use crate::work_claims::{HandoffScope, WorkClaimStore};
    use axum::{
        body::{to_bytes, Body},
        extract::ConnectInfo,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    let dir = unique_temp_path("handoff-freshness").with_extension("dir");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sessions.json");
    let queue = dir.join("messages.db");
    fs::write(
        &path,
        json!({"sessions":[{
            "id":"fresh001", "name":"fresh", "status":"stopped", "completion_status":"retired",
            "working_dir":"/repo", "tmux_session":"fresh001", "provider":"claude",
            "created_at":"2026-10-01T00:00:00Z", "last_activity":"2026-10-01T00:00:00Z"
        }]})
        .to_string(),
    )
    .unwrap();
    let claims = WorkClaimStore::new(queue.clone());
    claims.ensure_schema().unwrap();
    rusqlite::Connection::open(&queue).unwrap().execute(
        "INSERT INTO work_claims (id,repo,number,kind,session_id,source,claimed_at) VALUES ('fresh','acme/widgets',42,'ticket','fresh001','explicit','2026-10-01T00:00:00Z')", []
    ).unwrap();
    let mut config = crate::config::AppConfig::default();
    config.paths.state_file = path.to_string_lossy().into_owned();
    config.sm_send.db_path = queue.to_string_lossy().into_owned();
    let app = crate::http::router(crate::http::AppState::new(config));
    let store = SessionStore::new_with_queue(path.clone(), queue);
    async fn read(app: axum::Router) -> Value {
        let mut request = Request::builder()
            .uri("/watch/state?stopped=1")
            .header("host", "testserver")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                4200,
            ))));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
    }
    let first = read(app.clone()).await;
    let parsed = store.load_parsed_state().unwrap();
    let before = fs::read(&path).unwrap();
    let override_ = HandoffOverride {
        enabled: Some(true),
        threshold_percent: Some(47.0),
        set_at: Some("2026-10-01T00:00:00Z".to_owned()),
        ..HandoffOverride::default()
    };
    claims
        .set_handoff_override(HandoffScope::Ticket, "acme/widgets", 42, Some(&override_))
        .unwrap();
    let second = read(app).await;
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(Arc::ptr_eq(&parsed, &store.load_parsed_state().unwrap()));
    let handoff = |body: &Value| {
        body["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "fresh001")
            .unwrap()["handoff"]
            .clone()
    };
    assert_ne!(handoff(&first), handoff(&second));
    assert_eq!(handoff(&second)["source"], "ticket");
    assert_eq!(handoff(&second)["threshold_percent"], 47.0);
    // The original snapshot path still provides a reference for the same claim sequence.
    let expected = store
        .load_snapshot()
        .unwrap()
        .into_sessions()
        .into_iter()
        .find(|r| r.id == "fresh001")
        .unwrap()
        .handoff_view
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&handoff(&second)).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    let reads = store.sessions_read_set().unwrap();
    assert!(reads.iter().all(|record| record.handoff_view.is_none()));
    assert_eq!(reads.record(0).handoff_view, Some(expected));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rejected_hooks_do_not_attempt_the_write_lock() {
    let path = unique_temp_path("rejected-hooks");
    let store = SessionStore::new(path.clone());
    store
        .write_raw_json_value(&json!({"sessions":[
            {"id":"stopped", "provider":"codex-fork", "status":"stopped"},
            {"id":"live", "provider":"claude", "status":"running",
             "activity_hook_emitted_at":"2026-10-01T12:00:00Z",
             "context_sampled_at":"2026-10-01T12:00:00Z"}
        ]}))
        .unwrap();
    let before = fs::read(&path).unwrap();
    for id in ["missing", "stopped"] {
        assert!(!store
            .apply_claude_pre_tool_use_hook(id, Some("Read"))
            .unwrap());
        assert!(!store
            .apply_claude_user_prompt_submit_hook(id, None)
            .unwrap());
        assert!(!store
            .apply_claude_stop_hook(id, None, None, None, None, None, None)
            .unwrap());
        store
            .apply_codex_fork_event_line_with_artifact(
                id,
                r#"{"event_type":"turn/completed"}"#,
                None,
            )
            .unwrap();
    }
    assert!(!store
        .apply_claude_user_prompt_submit_hook("live", Some("2026-10-01T11:00:00Z"))
        .unwrap());
    assert!(matches!(
        store
            .apply_context_usage_event(
                &ContextUsageEvent {
                    session_id: "missing".to_owned(),
                    ..ContextUsageEvent::default()
                },
                None
            )
            .unwrap(),
        ContextUsageOutcome::UnknownSession
    ));
    assert!(matches!(
        store
            .apply_context_usage_event(
                &ContextUsageEvent {
                    session_id: "live".to_owned(),
                    emitted_at: Some("2026-10-01T12:00:00Z".to_owned()),
                    used_percentage: Some(20.0),
                    ..ContextUsageEvent::default()
                },
                None
            )
            .unwrap(),
        ContextUsageOutcome::StaleSample
    ));
    let runtime = TmuxRuntime::from_config(&crate::config::RustCoreConfig::default());
    assert!(store
        .revive_stopped_tmux_client_session("absent-tmux", &runtime)
        .unwrap()
        .is_none());
    assert_eq!(store.write_lock_attempts.load(Ordering::Relaxed), 0);
    assert_eq!(fs::read(&path).unwrap(), before);
    fs::remove_file(path).unwrap();
}
