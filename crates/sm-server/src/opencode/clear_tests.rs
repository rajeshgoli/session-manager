use super::*;

fn create(f: &Fixture) {
    f.create("local1", None).unwrap();
    f.change("local1", "parent_session_id", json!("parent"));
}

fn clear(f: &Fixture, prompt: Option<&str>) -> CoreClearOutcome {
    f.store
        .clear_core_session_with_runtime(
            "local1",
            ClearSessionRequest {
                prompt: prompt.map(str::to_owned),
                requester_session_id: None,
            },
            &f.runtime,
        )
        .unwrap()
}
fn queue(f: &Fixture) -> &RetainedQueueStore {
    f.store.queue_store.as_ref().unwrap()
}
fn requests(f: &Fixture) -> Vec<(String, String, Value)> {
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .request_log()
}
fn send(f: &Fixture, text: &str) -> CoreInputResult {
    f.store
        .send_core_input_with_runtime(
            "local1",
            serde_json::from_value(json!({"text":text,"delivery_mode":"urgent"})).unwrap(),
            &f.runtime,
        )
        .unwrap()
        .unwrap()
}

#[test]
fn opencode_clear_aborts_before_switch_and_keeps_both_usage_mappings_before_prompt() {
    let f = Fixture::new();
    create(&f);
    f.change("local1", "context_used_percentage", json!(90));
    let path = f._scratch.path().join("sessions.usage.db");
    *f.driver.attach_hook.lock().unwrap() = Some(Box::new(move |record| {
        let db = rusqlite::Connection::open(path).unwrap();
        let count: i64 = db.query_row("SELECT COUNT(*) FROM seat_sessions WHERE seat_id = 'local1' AND provider = 'opencode'", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 2);
        assert_eq!(record.provider_resume_id.as_deref(), Some("ses_clear2"));
    }));
    assert!(matches!(
        clear(&f, Some("fresh work")),
        CoreClearOutcome::Cleared(_)
    ));
    let record = f.store.get_session("local1").unwrap().unwrap();
    assert_eq!(record.provider_resume_id.as_deref(), Some("ses_clear2"));
    assert_eq!(record.context_used_percentage, None);
    let log = requests(&f);
    let abort = log
        .iter()
        .position(|(_, p, _)| p.ends_with("/abort"))
        .unwrap();
    let create = log
        .iter()
        .rposition(|(v, p, _)| v == "POST" && p == "/session")
        .unwrap();
    let prompt = log
        .iter()
        .position(|(_, p, _)| p == "/session/ses_clear2/prompt_async")
        .unwrap();
    assert!(abort < create && create < prompt);
    assert_eq!(f.driver.posts(), 1);
    assert_eq!(
        queue(&f)
            .pending_messages_for_target("local1", 100)
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn opencode_clear_refuses_busy_and_leaves_identity_context_and_queue_unchanged() {
    let f = Fixture::new();
    create(&f);
    f.change("local1", "context_used_percentage", json!(81));
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .abort_stays_busy();
    let before = f.store.load_raw_json_value().unwrap();
    assert!(
        matches!(clear(&f, Some("never submit")), CoreClearOutcome::Conflict(ref s) if s == "opencode conversation did not stop; clear not applied")
    );
    assert_eq!(before, f.store.load_raw_json_value().unwrap());
    assert_eq!(f.driver.conversations(), 1);
    assert_eq!(f.driver.posts(), 0);
}

#[test]
fn opencode_clear_resolves_accepted_old_message_without_reposting_to_new_conversation() {
    let f = Fixture::new();
    create(&f);
    let id = queue(&f)
        .enqueue_message("local1", "already accepted", "sequential", None)
        .unwrap();
    let binding = queue(&f)
        .bind_pending_provider_message("local1", &id, "ses_test")
        .unwrap();
    f.driver
        .client(
            f.store
                .get_session("local1")
                .unwrap()
                .unwrap()
                .opencode
                .as_ref()
                .unwrap(),
        )
        .unwrap()
        .attempt_delivery(&binding, "already accepted", Duration::from_secs(1))
        .unwrap();
    assert!(matches!(clear(&f, None), CoreClearOutcome::Cleared(_)));
    assert!(queue(&f).message_delivered(&id).unwrap());
    assert_eq!(f.driver.posts(), 1);
}

#[test]
fn opencode_clear_refuses_unresolved_ids_and_rebinds_only_confirmed_absence() {
    let f = Fixture::new();
    create(&f);
    let id = queue(&f)
        .enqueue_message("local1", "not accepted", "sequential", None)
        .unwrap();
    let old = queue(&f)
        .bind_pending_provider_message("local1", &id, "ses_test")
        .unwrap();
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .lookup_error(Some(503));
    assert!(
        matches!(clear(&f, None), CoreClearOutcome::Conflict(ref s) if s == "opencode server unreachable; pending messages unresolved")
    );
    assert_eq!(f.driver.conversations(), 1);
    assert_eq!(
        queue(&f)
            .pending_provider_message_binding("local1", &id)
            .unwrap(),
        Some(old.clone())
    );
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .lookup_error(None);
    assert!(matches!(clear(&f, None), CoreClearOutcome::Cleared(_)));
    let log = requests(&f);
    let (_, path, body) = log
        .iter()
        .find(|(_, p, _)| p.ends_with("/prompt_async"))
        .unwrap();
    assert_eq!(path, "/session/ses_clear2/prompt_async");
    assert_ne!(body["messageID"].as_str(), Some(old.message_id.as_str()));
    assert!(queue(&f).message_delivered(&id).unwrap());
    assert_eq!(f.driver.posts(), 1);
}

#[test]
fn opencode_clear_recovery_reuses_committed_conversation_and_prompt_after_registry_commit_failure()
{
    let f = Fixture::new();
    create(&f);
    f.driver.fail_attach.store(true, Ordering::Release);
    assert!(matches!(
        clear(&f, Some("recover once")),
        CoreClearOutcome::Conflict(_)
    ));
    assert_eq!(f.driver.conversations(), 2);
    assert_eq!(f.driver.posts(), 0);
    assert!(!send(&f, "queued behind clear").delivered);
    let state = f.store.load_raw_json_value().unwrap();
    let pending = raw_session_object(&state, "local1").unwrap()["opencode_pending_clear"].clone();
    // Simulate the cross-store crash after prompt insertion and before the
    // completion marker is removed. Recovery must reuse this exact queue row.
    queue(&f)
        .enqueue_message_once_with_metadata(
            pending["message_id"].as_str().unwrap(),
            "local1",
            "recover once",
            "sequential",
            QueueMessageMetadata::default(),
        )
        .unwrap();
    f.driver.fail_attach.store(false, Ordering::Release);
    let reopened =
        SessionStore::new_with_queue(f.driver.path.clone(), f._scratch.path().join("queue.db"))
            .with_opencode_launch_driver(f.driver.clone());
    reopened.recover_opencode_clears(&f.runtime).unwrap();
    reopened.recover_opencode_clears(&f.runtime).unwrap();
    assert_eq!(f.driver.conversations(), 2);
    assert_eq!(f.driver.posts(), 2);
    assert_eq!(
        requests(&f)
            .iter()
            .filter(
                |(_, p, b)| p.ends_with("/prompt_async") && b["parts"][0]["text"] == "recover once"
            )
            .count(),
        1
    );
    assert!(
        raw_session_object(&reopened.load_raw_json_value().unwrap(), "local1")
            .unwrap()
            .get("opencode_pending_clear")
            .is_none()
    );
}

#[test]
fn opencode_clear_does_not_abort_idle_and_checks_authorization_and_handoff() {
    let f = Fixture::new();
    create(&f);
    let unauthorized = f
        .store
        .clear_core_session_with_runtime(
            "local1",
            ClearSessionRequest {
                prompt: None,
                requester_session_id: Some("intruder".into()),
            },
            &f.runtime,
        )
        .unwrap();
    assert!(matches!(unauthorized, CoreClearOutcome::Unauthorized(_)));
    let note = crate::handoff::execute::HandoffNote::parse(
        &json!({"kind":"path", "value":"/tmp/handoff.md"}),
    )
    .unwrap();
    f.store.accept_handoff("local1", "local1", &note).unwrap();
    f.store.claim_handoff_start("local1").unwrap().unwrap();
    assert!(matches!(clear(&f, None), CoreClearOutcome::Conflict(_)));
    f.change("local1", "handoff", Value::Null);
    f.driver.server.lock().unwrap().as_ref().unwrap().set_idle();
    assert!(matches!(clear(&f, None), CoreClearOutcome::Cleared(_)));
    assert!(!requests(&f).iter().any(|(_, p, _)| p.ends_with("/abort")));
}

#[test]
fn opencode_stopped_send_is_not_persisted_and_cannot_reappear_after_restore() {
    let f = Fixture::new();
    create(&f);
    f.auto_retire();
    assert!(!send(&f, "reject me").delivered);
    assert!(queue(&f)
        .pending_messages_for_target("local1", 100)
        .unwrap()
        .is_empty());
    assert!(matches!(clear(&f, None), CoreClearOutcome::NotRunning));
    f.store
        .restore_core_session_with_runtime("local1", &f.runtime)
        .unwrap();
    f.store.drain_opencode_outbox("local1", &f.runtime).unwrap();
    assert_eq!(f.driver.posts(), 0);
}

#[test]
fn opencode_clear_new_conversation_events_apply_and_old_events_are_ignored() {
    let f = Fixture::new();
    create(&f);
    assert!(matches!(clear(&f, None), CoreClearOutcome::Cleared(_)));
    let tools = f._scratch.path().join("tools.db");
    for conversation in ["ses_test", "ses_clear2"] {
        f.store.apply_opencode_events("local1", OpencodeEventInput::Live(&json!({"type":"session.status", "properties":{"sessionID":conversation,"status":{"type":"busy"}}})),
            &f.driver.config, &BTreeSet::new(), &tools).unwrap();
        let record = f.store.get_session("local1").unwrap().unwrap();
        assert_eq!(
            record.status,
            if conversation == "ses_test" {
                "idle"
            } else {
                "running"
            }
        );
    }
}

#[test]
fn opencode_clear_retains_switch_until_usage_mapping_can_commit() {
    let f = Fixture::new();
    create(&f);
    f.store
        .seat_session_store
        .append("local1", "opencode", "ses_test", None)
        .unwrap();
    let db = rusqlite::Connection::open(f._scratch.path().join("sessions.usage.db")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_new_mapping BEFORE INSERT ON seat_sessions WHEN NEW.provider_session_id != 'ses_test' BEGIN SELECT RAISE(ABORT, 'injected usage mapping failure'); END;").unwrap();
    assert!(matches!(
        clear(&f, Some("after mapping")),
        CoreClearOutcome::Conflict(_)
    ));
    assert_eq!(f.driver.attachments.load(Ordering::Acquire), 1);
    assert_eq!(f.driver.posts(), 0);
    assert_eq!(
        f.store
            .get_session("local1")
            .unwrap()
            .unwrap()
            .provider_resume_id
            .as_deref(),
        Some("ses_clear2")
    );
    assert!(!send(&f, "later input").delivered);
    db.execute_batch("DROP TRIGGER reject_new_mapping").unwrap();
    f.store.recover_opencode_clears(&f.runtime).unwrap();
    let prompts: Vec<_> = requests(&f)
        .into_iter()
        .filter(|(_, p, _)| p.ends_with("/prompt_async"))
        .collect();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].2["parts"][0]["text"], "after mapping");
    assert_eq!(prompts[1].2["parts"][0]["text"], "later input");
}

#[test]
fn opencode_clear_serializes_retirement_without_blocking_unrelated_registry_work() {
    let f = Fixture::new();
    create(&f);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *f.driver.attach_hook.lock().unwrap() = Some(Box::new(move |_| {
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    }));
    let (store, runtime) = (f.store.clone(), f.runtime.clone());
    let clearing = thread::spawn(move || {
        store
            .clear_core_session_with_runtime(
                "local1",
                ClearSessionRequest {
                    prompt: Some("clear before retirement".into()),
                    requester_session_id: None,
                },
                &runtime,
            )
            .unwrap()
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    f.change(
        "local1",
        "current_task",
        json!("registry remains available"),
    );
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let store = f.store.clone();
    let retiring = thread::spawn(move || {
        let result = store
            .retire_core_session_authorized("local1", RetireAuthority::auto_retire(60), None)
            .unwrap();
        done_tx.send(()).unwrap();
        result
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(200)).is_err());
    release_tx.send(()).unwrap();
    assert!(matches!(
        clearing.join().unwrap(),
        CoreClearOutcome::Cleared(_)
    ));
    assert!(matches!(
        retiring.join().unwrap(),
        CoreRetireOutcome::Retired(_)
    ));
    assert_eq!(f.driver.posts(), 1);
    assert!(!send(&f, "after retirement").delivered);
    assert!(queue(&f)
        .pending_messages_for_target("local1", 100)
        .unwrap()
        .is_empty());
}
