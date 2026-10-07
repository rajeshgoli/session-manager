use super::*;
use crate::opencode::tests::ScratchDir;
use rusqlite::{params, Connection};

struct Fixture {
    _tmp: ScratchDir,
    store: SessionStore,
    root: PathBuf,
    queue: PathBuf,
    tool_db: PathBuf,
    messages: Vec<Value>,
    started: i64,
}

impl Fixture {
    fn new() -> Self {
        let tmp = ScratchDir::new();
        let root = tmp.path().canonicalize().unwrap();
        let queue = root.join("queue.db");
        crate::owner_messages::OwnerMessageStore::new(queue.clone())
            .ensure_schema()
            .unwrap();
        crate::turn_messages::TurnMessageStore::new(queue.clone())
            .ensure_schema()
            .unwrap();
        let path = root.join("sessions.json");
        let started = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
            .unwrap()
            - 2000;
        let record = json!({"id":"agent", "name":"agent", "working_dir":"/repo", "tmux_session":"sm-agent",
            "provider":"opencode", "status":"running", "provider_resume_id":"ses_test",
            "created_at":"2026-10-07T00:00:00Z", "last_activity":"2026-10-07T00:00:01Z",
            "opencode":{"port":18503,"state_dir":root,"version":"1.17.9","model_base_url":"http://127.0.0.1:8000/v1"}});
        fs::write(
            &path,
            serde_json::to_vec(&json!({"sessions":[record]})).unwrap(),
        )
        .unwrap();
        let messages = vec![
            json!({"info":{"id":"msg_user","sessionID":"ses_test","role":"user","time":{"created":started}},
                "parts":[{"id":"prt_prompt","sessionID":"ses_test","messageID":"msg_user","type":"text","text":"brief"}]}),
            json!({"info":{"id":"msg_reply","sessionID":"ses_test","role":"assistant","parentID":"msg_user",
                "time":{"created":started+100,"completed":started+1000},"finish":"stop"},"parts":[
                {"id":"prt_tool","sessionID":"ses_test","messageID":"msg_reply","type":"tool","callID":"call-one",
                    "tool":"bash","state":{"status":"running","input":{"command":"git status"}}},
                {"id":"prt_usage","sessionID":"ses_test","messageID":"msg_reply","type":"step-finish",
                    "tokens":{"input":120,"output":7,"reasoning":3,"cache":{"read":900,"write":20}}},
                {"id":"prt_text","sessionID":"ses_test","messageID":"msg_reply","type":"text","text":"done"}]}),
        ];
        Self {
            store: SessionStore::new_with_queue(path, queue.clone()),
            tool_db: root.join("tools.db"),
            root,
            queue,
            messages,
            started,
            _tmp: tmp,
        }
    }
    fn apply(&self) -> Result<bool> {
        let stopped = self.store.apply_opencode_events(
            "agent",
            OpencodeEventInput::Backfill {
                conversation: "ses_test",
                messages: &self.messages,
                activity: Activity::Idle,
            },
            &OpencodeConfig::default(),
            &BTreeSet::from(["msg_user".into()]),
            &self.tool_db,
        )?;
        self.acknowledge_stop();
        Ok(stopped)
    }
    fn acknowledge_stop(&self) {
        if let Some(signal) = self.store.opencode_pending_stop_signal("agent").unwrap() {
            assert!(self
                .store
                .acknowledge_opencode_stop_signal("agent", &signal)
                .unwrap());
        }
    }
    fn raw(&self) -> Value {
        self.store.load_raw_json_value().unwrap()
    }
    fn save(&self, state: &Value) {
        self.store.write_raw_json_value(state).unwrap();
    }
    fn question(&self, id: &str, ms: i64) {
        let at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
            .unwrap()
            .format(&Rfc3339)
            .unwrap();
        Connection::open(&self.queue).unwrap().execute(
            "INSERT INTO owner_messages (id,human,sender_session_id,sender_session_name,title,body_markdown,blocking,created_at) VALUES (?1,'rajesh','agent','agent','question','body',1,?2)",
            params![id, at]).unwrap();
    }
    fn handled(&self, id: &str) -> bool {
        Connection::open(&self.queue)
            .unwrap()
            .query_row(
                "SELECT handled_at IS NOT NULL FROM owner_messages WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    }
}

#[test]
fn disconnected_turn_commits_activity_history_tools_usage_and_cursor_once() {
    let fixture = Fixture::new();
    assert!(fixture.apply().unwrap());
    let state = fixture.raw();
    let session = &state["sessions"][0];
    assert_eq!(session["status"], "idle");
    assert_eq!(session["turns_completed"], 1);
    assert_eq!(session["last_tool_name"], "bash");
    assert_eq!(session["tokens_used"], 1040);
    assert_eq!(session["provider_event_cursor"], "msg_reply");
    assert_eq!(session["opencode_pending_effects"], json!([]));
    assert_eq!(
        fixture
            .store
            .turn_message_store()
            .unwrap()
            .last_turn("agent")
            .unwrap()
            .unwrap()
            .text,
        "done"
    );
    let usage = fs::read_to_string(fixture.root.join("usage.jsonl")).unwrap();
    assert_eq!(usage.lines().count(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(&usage).unwrap()["message"]["usage"]["output_tokens"],
        10
    );
    let reopened =
        SessionStore::new_with_queue(fixture.store.state_file.clone(), fixture.queue.clone());
    assert!(!reopened
        .apply_opencode_events(
            "agent",
            OpencodeEventInput::Backfill {
                conversation: "ses_test",
                messages: &fixture.messages,
                activity: Activity::Idle
            },
            &OpencodeConfig::default(),
            &BTreeSet::from(["msg_user".into()]),
            &fixture.tool_db
        )
        .unwrap());
    assert_eq!(
        reopened
            .get_session("agent")
            .unwrap()
            .unwrap()
            .turns_completed,
        1
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("usage.jsonl")).unwrap(),
        usage
    );
    let count: i64 = Connection::open(&fixture.tool_db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM tool_usage", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn owner_answer_wakes_after_unlock_even_when_a_later_tool_write_fails() {
    let mut fixture = Fixture::new();
    fixture.question("question", fixture.started - 1);
    let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = wakes.clone();
    let write_lock = fixture.store.write_lock.clone();
    fixture.store = fixture.store.with_owner_answered_wake(Arc::new(move || {
        assert!(
            write_lock.try_lock().is_ok(),
            "owner wake runs outside the registry lock"
        );
        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }));
    fixture.tool_db = fixture.root.join("blocked-tools");
    fs::create_dir(&fixture.tool_db).unwrap();
    assert!(fixture
        .store
        .apply_opencode_events(
            "agent",
            OpencodeEventInput::Backfill {
                conversation: "ses_test",
                messages: &fixture.messages,
                activity: Activity::Idle,
            },
            &OpencodeConfig::default(),
            &BTreeSet::new(),
            &fixture.tool_db
        )
        .is_err());
    assert!(fixture.handled("question"));
    assert_eq!(wakes.load(std::sync::atomic::Ordering::Relaxed), 1);
    fs::remove_dir(&fixture.tool_db).unwrap();
    fixture.store.recover_opencode_effects("agent").unwrap();
    assert_eq!(wakes.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn failed_tool_write_retains_ordered_effects_and_recovery_does_not_count_twice() {
    let mut fixture = Fixture::new();
    fixture.tool_db = fixture.root.join("not-a-database");
    fs::create_dir(&fixture.tool_db).unwrap();
    assert!(fixture.apply().is_err());
    let interrupted = fixture.raw();
    assert_eq!(interrupted["sessions"][0]["turns_completed"], 1);
    assert_eq!(
        interrupted["sessions"][0]["opencode_pending_effects"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(!fixture.root.join("usage.jsonl").exists());
    fs::remove_dir(&fixture.tool_db).unwrap();
    let reopened =
        SessionStore::new_with_queue(fixture.store.state_file.clone(), fixture.queue.clone());
    assert!(reopened.recover_opencode_effects("agent").unwrap());
    let end =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(fixture.started + 1000) * 1_000_000)
            .unwrap();
    fixture
        .store
        .turn_message_store()
        .unwrap()
        .record_finished("agent", end - time::Duration::milliseconds(1))
        .unwrap();
    // Simulate a crash after SQL/file commits but before acknowledging them in
    // the registry. Receipts and usage part IDs prevent duplicate side effects.
    fixture.save(&interrupted);
    assert!(fixture.store.recover_opencode_effects("agent").unwrap());
    fixture.acknowledge_stop();
    assert!(fixture
        .store
        .turn_message_store()
        .unwrap()
        .finished()
        .unwrap()[0]
        .text
        .is_none());
    assert!(!fixture.apply().unwrap());
    assert_eq!(fixture.raw()["sessions"][0]["turns_completed"], 1);
    assert_eq!(
        fs::read_to_string(fixture.root.join("usage.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let count: i64 = Connection::open(&fixture.tool_db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM tool_usage", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn replayed_owner_input_answers_only_questions_existing_when_it_was_typed() {
    let fixture = Fixture::new();
    fixture.question("earlier", fixture.started - 1000);
    fixture.question("later", fixture.started + 1000);
    fixture
        .store
        .apply_opencode_events(
            "agent",
            OpencodeEventInput::Backfill {
                conversation: "ses_test",
                messages: &fixture.messages[..1],
                activity: Activity::Idle,
            },
            &OpencodeConfig::default(),
            &BTreeSet::new(),
            &fixture.tool_db,
        )
        .unwrap();
    assert!(fixture.handled("earlier"));
    assert!(!fixture.handled("later"));
    assert_eq!(fixture.raw()["sessions"][0]["turns_completed"], Value::Null);
    // Even a later inserted row with an old timestamp must not be answered by
    // replay of an already committed owner-input receipt.
    fixture.question("inserted-later", fixture.started - 500);
    let store = crate::owner_messages::OwnerMessageStore::new(fixture.queue.clone());
    let key = Connection::open(&fixture.queue)
        .unwrap()
        .query_row("SELECT key FROM owner_prompt_receipts", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    let at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(fixture.started) * 1_000_000)
        .unwrap()
        .format(&Rfc3339)
        .unwrap();
    assert_eq!(
        store
            .answer_session_with_receipt("agent", "opencode_prompt", &at, &key)
            .unwrap(),
        0
    );
    assert!(!fixture.handled("inserted-later"));
}

#[test]
fn generated_prompt_does_not_answer_owner_and_reconnect_preserves_completion() {
    let fixture = Fixture::new();
    fixture.question("question", fixture.started - 1000);
    fixture.apply().unwrap();
    assert!(!fixture.handled("question"));
    let mut state = fixture.raw();
    state["sessions"][0]["agent_task_completed_at"] = json!("completed");
    fixture.save(&state);
    assert!(!fixture.apply().unwrap());
    assert_eq!(
        fixture.raw()["sessions"][0]["agent_task_completed_at"],
        "completed"
    );
}

#[test]
fn clear_follows_new_conversation_and_stale_frames_never_mutate_activity() {
    let fixture = Fixture::new();
    fixture.apply().unwrap();
    let mut state = fixture.raw();
    state["sessions"][0]["provider_resume_id"] = json!("ses_new");
    state["sessions"][0]["provider_event_cursor"] = Value::Null;
    fixture.save(&state);
    let event = json!({"type":"session.status","properties":{"sessionID":"ses_test","status":{"type":"busy"}}});
    assert!(!fixture
        .store
        .apply_opencode_events(
            "agent",
            OpencodeEventInput::Live(&event),
            &OpencodeConfig::default(),
            &BTreeSet::new(),
            &fixture.tool_db
        )
        .unwrap());
    assert_eq!(fixture.raw()["sessions"][0]["status"], "idle");
    assert_eq!(
        fixture.raw()["sessions"][0]["provider_event_cursor"],
        Value::Null
    );
    let event = json!({"type":"session.status","properties":{"sessionID":"ses_new","status":{"type":"busy"}}});
    fixture
        .store
        .apply_opencode_events(
            "agent",
            OpencodeEventInput::Live(&event),
            &OpencodeConfig::default(),
            &BTreeSet::new(),
            &fixture.tool_db,
        )
        .unwrap();
    assert_eq!(fixture.raw()["sessions"][0]["status"], "running");
}

#[test]
fn pending_old_usage_commits_journal_after_clear_without_overwriting_new_context() {
    let mut fixture = Fixture::new();
    fixture.tool_db = fixture.root.join("not-a-database");
    fs::create_dir(&fixture.tool_db).unwrap();
    assert!(fixture.apply().is_err());
    let mut state = fixture.raw();
    state["sessions"][0]["provider_resume_id"] = json!("ses_new");
    state["sessions"][0]["tokens_used"] = json!(42);
    fixture.save(&state);
    fs::remove_dir(&fixture.tool_db).unwrap();
    assert!(!fixture.store.recover_opencode_effects("agent").unwrap());
    assert_eq!(fixture.raw()["sessions"][0]["tokens_used"], 42);
    assert_eq!(
        fs::read_to_string(fixture.root.join("usage.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn snapshot_from_before_clear_cannot_apply_old_busy_status_to_new_conversation() {
    let fixture = Fixture::new();
    let mut state = fixture.raw();
    state["sessions"][0]["provider_resume_id"] = json!("ses_new");
    state["sessions"][0]["status"] = json!("idle");
    fixture.save(&state);
    assert!(!fixture
        .store
        .apply_opencode_events(
            "agent",
            OpencodeEventInput::Backfill {
                conversation: "ses_test",
                messages: &fixture.messages,
                activity: Activity::Busy,
            },
            &OpencodeConfig::default(),
            &BTreeSet::new(),
            &fixture.tool_db
        )
        .unwrap());
    let after = fixture.raw();
    assert_eq!(after["sessions"][0]["status"], "idle");
    assert!(after["sessions"][0]["opencode_event_projection"].is_null());
    assert_eq!(
        fixture
            .store
            .get_session("agent")
            .unwrap()
            .unwrap()
            .turns_completed,
        0
    );
}

#[test]
fn local_context_window_is_explicit_even_when_model_name_has_claude_suffix() {
    let fixture = Fixture::new();
    let config = OpencodeConfig {
        model_id: "local-model[1m]".into(),
        ..OpencodeConfig::default()
    };
    fixture
        .store
        .apply_opencode_events(
            "agent",
            OpencodeEventInput::Backfill {
                conversation: "ses_test",
                messages: &fixture.messages,
                activity: Activity::Idle,
            },
            &config,
            &BTreeSet::from(["msg_user".into()]),
            &fixture.tool_db,
        )
        .unwrap();
    assert_eq!(
        fixture.raw()["sessions"][0]["context_window_tokens"],
        200_000
    );
}

#[test]
fn title_error_and_compaction_keep_provider_activity_and_clear_context() {
    let fixture = Fixture::new();
    fixture.apply().unwrap();
    for (kind, properties) in [
        (
            "session.updated",
            json!({"info":{"id":"ses_test","title":"native title"}}),
        ),
        (
            "session.error",
            json!({"sessionID":"ses_test","error":{"message":"provider failed"}}),
        ),
        ("session.compacted", json!({"sessionID":"ses_test"})),
    ] {
        fixture
            .store
            .apply_opencode_events(
                "agent",
                OpencodeEventInput::Live(&json!({"type":kind,"properties":properties})),
                &OpencodeConfig::default(),
                &BTreeSet::new(),
                &fixture.tool_db,
            )
            .unwrap();
    }
    let state = fixture.raw();
    assert_eq!(state["sessions"][0]["status"], "idle");
    assert_eq!(state["sessions"][0]["native_title"], "native title");
    assert!(state["sessions"][0]["last_provider_error"]
        .as_str()
        .unwrap()
        .contains("provider failed"));
    assert_eq!(state["sessions"][0]["context_window_tokens"], 200_000);
    assert_eq!(
        state["sessions"][0]["context_total_input_tokens"],
        Value::Null
    );
    assert_eq!(state["sessions"][0]["context_used_percentage"], Value::Null);
    assert_eq!(state["sessions"][0]["turns_completed"], 1);
}

#[test]
fn applied_stop_survives_a_later_tool_failure_restart_and_stale_acknowledgement() {
    let mut fixture = Fixture::new();
    // The first turn completes while disconnected; the following turn starts
    // and its tool log cannot be written. The stop precedes the failing effect.
    fixture.messages[1]["parts"] = json!([fixture.messages[1]["parts"][2].clone()]);
    let next_user = json!({"info":{"id":"msg_next","sessionID":"ses_test","role":"user","time":{"created":fixture.started+1100}},
        "parts":[{"id":"prt_nextprompt","sessionID":"ses_test","messageID":"msg_next","type":"text","text":"next"}]});
    let next_reply = json!({"info":{"id":"msg_nextreply","sessionID":"ses_test","role":"assistant","parentID":"msg_next","time":{"created":fixture.started+1200}},
        "parts":[{"id":"prt_nexttool","sessionID":"ses_test","messageID":"msg_nextreply","type":"tool","callID":"call-next",
            "tool":"bash","state":{"status":"running","input":{"command":"git status"}}}]});
    fixture.messages.extend([next_user, next_reply]);
    fs::create_dir(&fixture.tool_db).unwrap();
    assert!(fixture
        .store
        .apply_opencode_events(
            "agent",
            OpencodeEventInput::Backfill {
                conversation: "ses_test",
                messages: &fixture.messages,
                activity: Activity::Busy
            },
            &OpencodeConfig::default(),
            &BTreeSet::from(["msg_user".into(), "msg_next".into()]),
            &fixture.tool_db
        )
        .is_err());
    let signal = fixture
        .store
        .opencode_pending_stop_signal("agent")
        .unwrap()
        .unwrap();
    assert_eq!(
        fixture.raw()["sessions"][0]["opencode_pending_effects"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(fixture.raw()["sessions"][0]["turns_completed"], 1);
    fs::remove_dir(&fixture.tool_db).unwrap();
    let reopened =
        SessionStore::new_with_queue(fixture.store.state_file.clone(), fixture.queue.clone());
    assert!(reopened.recover_opencode_effects("agent").unwrap());
    assert!(reopened.recover_opencode_effects("agent").unwrap());
    assert_eq!(
        reopened.opencode_pending_stop_signal("agent").unwrap(),
        Some(signal.clone())
    );
    let idle = json!({"type":"session.status","properties":{"sessionID":"ses_test","status":{"type":"idle"}}});
    assert!(reopened
        .apply_opencode_events(
            "agent",
            OpencodeEventInput::Live(&idle),
            &OpencodeConfig::default(),
            &BTreeSet::new(),
            &fixture.tool_db
        )
        .unwrap());
    let newer = reopened
        .opencode_pending_stop_signal("agent")
        .unwrap()
        .unwrap();
    assert_ne!(signal, newer);
    assert!(!reopened
        .acknowledge_opencode_stop_signal("agent", &signal)
        .unwrap());
    assert!(reopened
        .acknowledge_opencode_stop_signal("agent", &newer)
        .unwrap());
    assert!(!reopened.recover_opencode_effects("agent").unwrap());
}

#[test]
fn stop_without_completion_metadata_uses_observation_time_to_fill_finished_row() {
    let mut fixture = Fixture::new();
    fixture.messages[1]["info"]["time"]
        .as_object_mut()
        .unwrap()
        .remove("completed");
    fixture.messages[1]["info"]
        .as_object_mut()
        .unwrap()
        .remove("finish");
    assert!(!fixture
        .store
        .apply_opencode_events(
            "agent",
            OpencodeEventInput::Backfill {
                conversation: "ses_test",
                messages: &fixture.messages,
                activity: Activity::Busy
            },
            &OpencodeConfig::default(),
            &BTreeSet::from(["msg_user".into()]),
            &fixture.tool_db
        )
        .unwrap());
    fixture
        .store
        .turn_message_store()
        .unwrap()
        .record_finished("agent", OffsetDateTime::now_utc())
        .unwrap();
    let idle = json!({"type":"session.status","properties":{"sessionID":"ses_test","status":{"type":"idle"}}});
    assert!(fixture
        .store
        .apply_opencode_events(
            "agent",
            OpencodeEventInput::Live(&idle),
            &OpencodeConfig::default(),
            &BTreeSet::new(),
            &fixture.tool_db
        )
        .unwrap());
    assert_eq!(
        fixture
            .store
            .turn_message_store()
            .unwrap()
            .finished()
            .unwrap()[0]
            .text
            .as_deref(),
        Some("done")
    );
}
