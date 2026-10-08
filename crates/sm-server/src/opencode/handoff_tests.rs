use super::*;

fn accept(f: &Fixture) {
    let note = crate::handoff::execute::HandoffNote::parse(
        &json!({"kind":"path","value":"/tmp/local-handoff.md"}),
    )
    .unwrap();
    assert!(matches!(
        f.store.accept_handoff("local1", "local1", &note).unwrap(),
        HandoffAcceptOutcome::Accepted { .. }
    ));
}
fn queue(f: &Fixture) -> &RetainedQueueStore {
    f.store.queue_store.as_ref().unwrap()
}
fn note_requests(f: &Fixture) -> usize {
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .request_log()
        .iter()
        .filter(|(_, p, _)| p.contains("/message/"))
        .count()
}

#[test]
fn opencode_handoff_unreachable_rows_defer_at_five_seconds_and_fail_after_ten_minutes_without_moves(
) {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    f.change("local1", "status", json!("idle"));
    let id = queue(&f)
        .enqueue_message("local1", "uncertain", "sequential", None)
        .unwrap();
    let binding = queue(&f)
        .bind_pending_provider_message("local1", &id, "ses_test")
        .unwrap();
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .lookup_error(Some(503));
    accept(&f);
    assert!(!f
        .store
        .opencode_handoff_ready("local1", &f.runtime)
        .unwrap());
    let first = f.store.load_raw_json_value().unwrap()["sessions"][0]
        ["opencode_handoff_delivery_blocked_at"]
        .clone();
    assert!(first.is_string());
    let requests = note_requests(&f);
    assert!(!f
        .store
        .opencode_handoff_ready("local1", &f.runtime)
        .unwrap());
    assert_eq!(note_requests(&f), requests);
    assert!(queue(&f).hand_off_rows("local1", "succ").is_err());
    assert!(queue(&f).hand_off_messages("local1", "succ").is_err());
    assert_eq!(
        queue(&f)
            .pending_provider_message_binding("local1", &id)
            .unwrap(),
        Some(binding)
    );
    let reopened =
        SessionStore::new_with_queue(f.driver.path.clone(), f._scratch.path().join("queue.db"))
            .with_opencode_launch_driver(f.driver.clone());
    assert!(!reopened
        .opencode_handoff_ready("local1", &f.runtime)
        .unwrap());
    assert_eq!(
        reopened.load_raw_json_value().unwrap()["sessions"][0]
            ["opencode_handoff_delivery_blocked_at"],
        first
    );
    f.change(
        "local1",
        "opencode_handoff_delivery_blocked_at",
        json!((OffsetDateTime::now_utc() - time::Duration::minutes(11))
            .format(&Rfc3339)
            .unwrap()),
    );
    f.change(
        "local1",
        "opencode_handoff_delivery_last_attempt_at",
        Value::Null,
    );
    assert!(!reopened
        .opencode_handoff_ready("local1", &f.runtime)
        .unwrap());
    let session = reopened.get_session("local1").unwrap().unwrap();
    assert!(!session.is_stopped());
    assert_eq!(session.handoff.unwrap()["state"], "failed");
    assert_eq!(
        session.agent_status_text.as_deref(),
        Some("handoff blocked: unresolved deliveries to an unreachable conversation")
    );
    assert_eq!(
        queue(&f)
            .pending_messages_for_target("local1", 100)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(f.driver.starts.load(Ordering::Acquire), 1);
    assert_eq!(f.driver.stops.load(Ordering::Acquire), 0);
    accept(&f);
    assert!(
        raw_session_object(&f.store.load_raw_json_value().unwrap(), "local1")
            .unwrap()
            .get("opencode_handoff_delivery_blocked_at")
            .is_none()
    );
}

#[test]
fn opencode_handoff_confirms_accepted_old_turn_and_moves_only_authoritative_absence() {
    let f = Fixture::with_ports(2);
    let pred = f.create("local1", None).unwrap();
    let accepted = queue(&f)
        .enqueue_message("local1", "accepted once", "sequential", None)
        .unwrap();
    let binding = queue(&f)
        .bind_pending_provider_message("local1", &accepted, "ses_test")
        .unwrap();
    f.driver
        .client(pred.opencode.as_ref().unwrap())
        .unwrap()
        .attempt_delivery(&binding, "accepted once", Duration::from_secs(1))
        .unwrap();
    let absent = queue(&f)
        .enqueue_message("local1", "not accepted", "sequential", None)
        .unwrap();
    queue(&f)
        .bind_pending_provider_message("local1", &absent, "ses_test")
        .unwrap();
    f.change("local1", "status", json!("idle"));
    accept(&f);
    assert!(f
        .store
        .opencode_handoff_ready("local1", &f.runtime)
        .unwrap());
    assert!(queue(&f).message_delivered(&accepted).unwrap());
    assert!(queue(&f)
        .pending_provider_message_binding("local1", &absent)
        .unwrap()
        .is_none());
    f.store.claim_handoff_start("local1").unwrap().unwrap();
    assert!(f
        .create("ordinary", None)
        .unwrap_err()
        .to_string()
        .contains("no local seat free"));
    let (store, runtime) = (f.store.clone(), f.runtime.clone());
    *f.driver.start_hook.lock().unwrap() = Some(Box::new(move |_| {
        let sent = store
            .send_core_input_with_runtime(
                "local1",
                serde_json::from_value(
                    json!({"text":"during successor start", "delivery_mode":"urgent"}),
                )
                .unwrap(),
                &runtime,
            )
            .unwrap()
            .unwrap();
        assert!(!sent.delivered);
    }));
    let successor = f
        .store
        .create_opencode_handoff_successor(
            Fixture::request("succ", None),
            None,
            &f.runtime,
            "local1",
        )
        .unwrap();
    assert!(f.store.opencode_http_ready("succ").unwrap());
    assert_eq!(f.driver.posts(), 1);
    f.store
        .with_opencode_handoff_transfer("local1", &f.runtime, || {
            f.store.transfer_handoff_json("local1", "succ")?;
            queue(&f).hand_off_rows("local1", "succ")
        })
        .unwrap();
    queue(&f).hand_off_messages("local1", "succ").unwrap();
    f.store.drain_opencode_outbox("succ", &f.runtime).unwrap();
    assert_eq!(f.driver.posts(), 3);
    assert!(queue(&f).message_delivered(&absent).unwrap());
    let old = f.driver.other_servers.lock().unwrap();
    assert_eq!(old.get(&pred.opencode.unwrap().port).unwrap().posts(), 1);
    drop(old);
    f.store
        .retire_core_session_with_runtime_authorized(
            "local1",
            RetireAuthority::handoff("succ"),
            None,
            &f.runtime,
        )
        .unwrap();
    assert!(f.driver.present(&successor, &f.runtime).unwrap());
    assert_eq!(
        f.store
            .get_session("local1")
            .unwrap()
            .unwrap()
            .completion_status
            .as_deref(),
        Some("retired")
    );
}

#[test]
fn opencode_handoff_restore_resolves_retained_ids_before_retrying_transfer() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    f.change("local1", "status", json!("idle"));
    let id = queue(&f)
        .enqueue_message("local1", "retained", "sequential", None)
        .unwrap();
    queue(&f)
        .bind_pending_provider_message("local1", &id, "ses_test")
        .unwrap();
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .lookup_error(Some(503));
    accept(&f);
    assert!(!f
        .store
        .opencode_handoff_ready("local1", &f.runtime)
        .unwrap());
    f.change("local1", "status", json!("stopped"));
    f.store
        .restore_core_session_with_runtime("local1", &f.runtime)
        .unwrap();
    f.change(
        "local1",
        "opencode_handoff_delivery_last_attempt_at",
        Value::Null,
    );
    assert!(f
        .store
        .opencode_handoff_ready("local1", &f.runtime)
        .unwrap());
    assert!(queue(&f)
        .pending_provider_message_binding("local1", &id)
        .unwrap()
        .is_none());
    assert_eq!(f.driver.posts(), 0);
    assert!(
        raw_session_object(&f.store.load_raw_json_value().unwrap(), "local1")
            .unwrap()
            .get("opencode_handoff_delivery_blocked_at")
            .is_none()
    );
}
