use super::*;

fn retire(f: &Fixture) -> Result<CoreRetireOutcome> {
    f.store.retire_core_session_with_runtime_authorized(
        "local1",
        RetireAuthority::operator("test"),
        None,
        &f.runtime,
    )
}

#[test]
fn opencode_retire_requires_teardown_proof_and_recovery_retains_capacity_even_if_marked_stopped() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    f.driver.fail_stop.store(true, Ordering::Release);
    assert!(retire(&f)
        .unwrap_err()
        .to_string()
        .contains("opencode server did not exit"));
    let record = f.store.get_session("local1").unwrap().unwrap();
    assert!(!record.is_stopped());
    assert_ne!(record.completion_status.as_deref(), Some("retired"));
    let sent = f
        .store
        .send_core_input_with_runtime(
            "local1",
            serde_json::from_value(json!({"text":"reject during teardown"})).unwrap(),
            &f.runtime,
        )
        .unwrap()
        .unwrap();
    assert!(!sent.delivered);
    assert!(f
        .store
        .queue_store
        .as_ref()
        .unwrap()
        .pending_messages_for_target("local1", 100)
        .unwrap()
        .is_empty());
    f.change("local1", "status", json!("stopped"));
    assert!(f
        .create("other", None)
        .unwrap_err()
        .to_string()
        .contains("no local seat free"));
    assert!(f
        .store
        .restore_core_session_with_runtime("local1", &f.runtime)
        .unwrap_err()
        .to_string()
        .contains("awaiting verified teardown"));
    let reopened =
        SessionStore::new_with_queue(f.driver.path.clone(), f._scratch.path().join("queue.db"))
            .with_opencode_launch_driver(f.driver.clone());
    reopened.recover_opencode_retirements(&f.runtime).unwrap();
    assert_ne!(
        reopened
            .get_session("local1")
            .unwrap()
            .unwrap()
            .completion_status
            .as_deref(),
        Some("retired")
    );
    f.driver.fail_stop.store(false, Ordering::Release);
    reopened.recover_opencode_retirements(&f.runtime).unwrap();
    let record = reopened.get_session("local1").unwrap().unwrap();
    assert!(record.is_stopped());
    assert_eq!(record.completion_status.as_deref(), Some("retired"));
    assert_eq!(record.terminal_provenance.unwrap().source, "test");
    assert!(
        raw_session_object(&reopened.load_raw_json_value().unwrap(), "local1")
            .unwrap()
            .get("opencode_pending_retire")
            .is_none()
    );
    f.create("other", None).unwrap();
}

#[test]
fn opencode_retire_preserves_state_and_conversation_and_is_idempotent() {
    let f = Fixture::new();
    let record = f.create("local1", None).unwrap();
    let path = PathBuf::from(record.opencode.unwrap().state_dir);
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("retained-state"), "keep").unwrap();
    assert!(matches!(retire(&f).unwrap(), CoreRetireOutcome::Retired(_)));
    let before = f.store.get_session("local1").unwrap().unwrap();
    let stopped = f.driver.stops.load(Ordering::Acquire);
    assert!(matches!(retire(&f).unwrap(), CoreRetireOutcome::Retired(_)));
    assert_eq!(f.driver.stops.load(Ordering::Acquire), stopped);
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(f.store.get_session("local1").unwrap().unwrap()).unwrap()
    );
    assert_eq!(
        fs::read_to_string(path.join("retained-state")).unwrap(),
        "keep"
    );
    f.store
        .restore_core_session_with_runtime("local1", &f.runtime)
        .unwrap();
    assert_eq!(
        f.store
            .get_session("local1")
            .unwrap()
            .unwrap()
            .provider_resume_id,
        before.provider_resume_id
    );
    assert_eq!(f.driver.posts(), 0);
}

#[test]
fn opencode_retire_checks_authorization_and_idle_before_teardown() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    assert!(matches!(
        f.store
            .retire_core_session_with_runtime_authorized(
                "local1",
                RetireAuthority::authenticated_parent("missing"),
                Some("forged"),
                &f.runtime
            )
            .unwrap(),
        CoreRetireOutcome::Forbidden
    ));
    assert!(matches!(
        f.store
            .retire_core_session_with_runtime_authorized_if_finished_idle(
                "local1",
                RetireAuthority::operator("test"),
                None,
                &f.runtime,
                true,
                &|_| false
            )
            .unwrap(),
        CoreRetireOutcome::PreconditionFailed
    ));
    assert_eq!(f.driver.stops.load(Ordering::Acquire), 0);
    assert!(
        raw_session_object(&f.store.load_raw_json_value().unwrap(), "local1")
            .unwrap()
            .get("opencode_pending_retire")
            .is_none()
    );
}

#[test]
fn opencode_retire_host_wait_keeps_registry_available_and_publishes_intent_first() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let path = f.driver.path.clone();
    *f.driver.stop_hook.lock().unwrap() = Some(Box::new(move |_| {
        let raw: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(!raw_session_object(&raw, "local1").unwrap()["retirement_intent"].is_null());
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    }));
    let (store, runtime) = (f.store.clone(), f.runtime.clone());
    let retiring = thread::spawn(move || {
        store
            .retire_core_session_with_runtime_authorized(
                "local1",
                RetireAuthority::operator("test"),
                None,
                &runtime,
            )
            .unwrap()
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    f.change(
        "local1",
        "current_task",
        json!("registry writable while stopping"),
    );
    release_tx.send(()).unwrap();
    assert!(matches!(
        retiring.join().unwrap(),
        CoreRetireOutcome::Retired(_)
    ));
    assert_eq!(
        f.store
            .get_session("local1")
            .unwrap()
            .unwrap()
            .current_task
            .as_deref(),
        Some("registry writable while stopping")
    );
}
