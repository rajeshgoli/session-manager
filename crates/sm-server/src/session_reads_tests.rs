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
