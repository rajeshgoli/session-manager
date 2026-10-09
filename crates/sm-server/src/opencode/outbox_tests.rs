use super::*;

impl Fixture {
    fn queue(&self) -> &RetainedQueueStore {
        self.store.queue_store.as_ref().unwrap()
    }
    fn send(&self, text: &str, mode: &str) -> CoreInputResult {
        self.store
            .send_core_input_with_runtime(
                "local1",
                serde_json::from_value(json!({
                    "text": text, "delivery_mode": mode,
                }))
                .unwrap(),
                &self.runtime,
            )
            .unwrap()
            .unwrap()
    }
    fn requests(&self) -> Vec<(String, String, Value)> {
        self.driver
            .server
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .request_log()
    }
}

#[test]
fn opencode_outbox_delivery_effects_wait_for_confirmation_and_commit_once() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    let id = f
        .queue()
        .enqueue_message_with_metadata(
            "local1",
            "with effects",
            "sequential",
            QueueMessageMetadata {
                sender_session_id: Some("local1".into()),
                notify_on_delivery: true,
                notify_on_stop: true,
                remind_soft_threshold: Some(60),
                remind_hard_threshold: Some(120),
                parent_session_id: Some("local1".into()),
                ..QueueMessageMetadata::default()
            },
        )
        .unwrap();
    let db = rusqlite::Connection::open(f._scratch.path().join("queue.db")).unwrap();
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .lose_post_reply();
    f.store.drain_opencode_outbox("local1", &f.runtime).unwrap();
    for table in [
        "remind_registrations",
        "parent_wake_registrations",
        "rust_stop_notify_states",
    ] {
        let count: i64 = db
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table} applied before confirmation");
    }
    for _ in 0..2 {
        f.store.drain_opencode_outbox("local1", &f.runtime).unwrap();
    }
    assert!(f.queue().message_delivered(&id).unwrap());
    for table in [
        "remind_registrations",
        "parent_wake_registrations",
        "rust_stop_notify_states",
    ] {
        let count: i64 = db
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1, "{table} did not commit once");
    }
    let rows: i64 = db
        .query_row("SELECT COUNT(*) FROM message_queue", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 2, "delivery acknowledgement duplicated");
    assert_eq!(
        f.driver.posts(),
        2,
        "original prompt or acknowledgement duplicated"
    );
}

#[test]
fn opencode_outbox_category_retry_runs_immediately_without_skipping_earlier_sends() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    let ordinary = f
        .queue()
        .enqueue_message("local1", "ordinary", "sequential", None)
        .unwrap();
    let wake = f
        .queue()
        .enqueue_message("local1", "wake", "sequential", Some("queue-completion"))
        .unwrap();
    assert_eq!(
        f.store
            .drain_runtime_pending_message_targets_by_category("queue-completion")
            .unwrap(),
        1
    );
    assert!(
        f.queue().message_delivered(&ordinary).unwrap()
            && f.queue().message_delivered(&wake).unwrap()
    );
    assert_eq!(f.driver.posts(), 2);
}

#[test]
fn opencode_outbox_lost_reply_reopens_and_confirms_without_duplicate_append() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .lose_post_reply();
    assert!(!f.send("hello", "urgent").delivered);
    let row = f.queue().pending_messages_for_target("local1", 10).unwrap()[0].clone();
    let binding = f
        .queue()
        .pending_provider_message_binding("local1", &row.id)
        .unwrap()
        .unwrap();
    let reopened =
        SessionStore::new_with_queue(f.driver.path.clone(), f._scratch.path().join("queue.db"))
            .with_delivery_runtime(Some(f.runtime.clone()))
            .with_opencode_launch_driver(f.driver.clone());
    reopened.drain_runtime_background_retry_messages().unwrap();
    assert!(f.queue().message_delivered(&row.id).unwrap());
    assert_eq!(f.driver.posts(), 1);
    assert!(f
        .requests()
        .iter()
        .any(|(method, path, _)| method == "GET" && path.ends_with(&binding.message_id)));
    assert!(!f
        .requests()
        .iter()
        .any(|(_, path, _)| path.ends_with("/abort")));
}

#[test]
fn opencode_outbox_busy_modes_and_categories_share_one_order_without_abort() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    f.change("local1", "status", json!("running"));
    let texts = ["first", "urgent", "reminder", "important", "reparent"];
    let mut ids = Vec::new();
    for (text, mode, category) in [
        (texts[0], "sequential", None),
        (texts[1], "urgent", None),
        (texts[2], "sequential", Some("scheduled_reminder")),
        (texts[3], "important", Some("queue-completion")),
        (texts[4], "important", Some("reparent")),
    ] {
        ids.push(
            f.queue()
                .enqueue_message("local1", text, mode, category)
                .unwrap(),
        );
    }
    f.store
        .drain_runtime_pending_messages_for_session_category("local1", &f.runtime, Some("reparent"))
        .unwrap();
    assert!(ids
        .iter()
        .all(|id| f.queue().message_delivered(id).unwrap()));
    let prompts: Vec<_> = f
        .requests()
        .into_iter()
        .filter(|(verb, path, _)| verb == "POST" && path.ends_with("/prompt_async"))
        .map(|(_, _, body)| body["parts"][0]["text"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(prompts, texts);
    assert!(!f
        .requests()
        .iter()
        .any(|(_, path, _)| path.ends_with("/abort")));
    assert!(f.send("direct mode", "immediate").delivered);
}

#[test]
fn opencode_outbox_unknown_lookup_holds_every_later_row_and_keeps_runtime_live() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    let first = f
        .queue()
        .enqueue_message("local1", "first", "sequential", None)
        .unwrap();
    let second = f
        .queue()
        .enqueue_message("local1", "second", "urgent", Some("queue-completion"))
        .unwrap();
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .lookup_error(Some(503));
    f.store.drain_runtime_background_retry_messages().unwrap();
    assert_eq!(f.driver.posts(), 0);
    assert!(f
        .queue()
        .pending_provider_message_binding("local1", &first)
        .unwrap()
        .is_some());
    assert!(f
        .queue()
        .pending_provider_message_binding("local1", &second)
        .unwrap()
        .is_none());
    assert!(!f.store.get_session("local1").unwrap().unwrap().is_stopped());
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .lookup_error(None);
    f.store.drain_runtime_background_retry_messages().unwrap();
    assert!(
        f.queue().message_delivered(&first).unwrap()
            && f.queue().message_delivered(&second).unwrap()
    );
    assert_eq!(f.driver.posts(), 2);
}

#[test]
fn opencode_outbox_rename_failure_blocks_prompt_and_retries_patch_in_order() {
    let f = Fixture::new();
    let record = f.create("local1", None).unwrap();
    assert!(f
        .store
        .queue_provider_native_rename(&record, "renamed")
        .unwrap());
    let row = f
        .queue()
        .enqueue_message("local1", "after rename", "urgent", None)
        .unwrap();
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .rename_error(true);
    f.store.drain_opencode_outbox("local1", &f.runtime).unwrap();
    assert_eq!(f.driver.posts(), 0);
    assert!(!f.queue().message_delivered(&row).unwrap());
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .rename_error(false);
    f.store.drain_opencode_outbox("local1", &f.runtime).unwrap();
    let writes: Vec<_> = f
        .requests()
        .into_iter()
        .filter(|(verb, path, _)| verb == "PATCH" || path.ends_with("/prompt_async"))
        .map(|(verb, _, body)| (verb, body))
        .collect();
    assert_eq!(
        writes
            .iter()
            .map(|(verb, _)| verb.as_str())
            .collect::<Vec<_>>(),
        ["PATCH", "PATCH", "POST"]
    );
    assert_eq!(writes[1].1["title"], "renamed");
    assert!(f.queue().message_delivered(&row).unwrap());
}

#[test]
fn opencode_outbox_confirmation_survives_queue_commit_failure_and_reopen() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    let id = f
        .queue()
        .enqueue_message("local1", "crash boundary", "sequential", None)
        .unwrap();
    let db = rusqlite::Connection::open(f._scratch.path().join("queue.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_completion BEFORE UPDATE OF delivered_at ON message_queue
        WHEN NEW.delivered_at IS NOT NULL BEGIN SELECT RAISE(ABORT, 'injected commit failure'); END;").unwrap();
    assert!(f.store.drain_opencode_outbox("local1", &f.runtime).is_err());
    assert_eq!(f.driver.posts(), 1);
    assert!(!f.queue().message_delivered(&id).unwrap());
    db.execute_batch("DROP TRIGGER fail_completion;").unwrap();
    let reopened =
        SessionStore::new_with_queue(f.driver.path.clone(), f._scratch.path().join("queue.db"))
            .with_opencode_launch_driver(f.driver.clone());
    reopened
        .drain_opencode_outbox("local1", &f.runtime)
        .unwrap();
    assert!(f.queue().message_delivered(&id).unwrap());
    assert_eq!(f.driver.posts(), 1);
}

#[test]
fn opencode_outbox_binding_resolution_confirms_or_clears_without_posting() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    let accepted = f
        .queue()
        .enqueue_message("local1", "accepted", "sequential", None)
        .unwrap();
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .lose_post_reply();
    f.store.drain_opencode_outbox("local1", &f.runtime).unwrap();
    let missing = f
        .queue()
        .enqueue_message("local1", "missing", "sequential", None)
        .unwrap();
    f.queue()
        .bind_pending_provider_message("local1", &missing, "ses_test")
        .unwrap();
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .lookup_error(Some(503));
    assert!(f
        .store
        .resolve_opencode_pending_bindings("local1", &f.runtime)
        .is_err());
    assert!(f
        .queue()
        .pending_provider_message_binding("local1", &missing)
        .unwrap()
        .is_some());
    assert!(!f.queue().message_delivered(&accepted).unwrap());
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .lookup_error(None);
    f.store
        .resolve_opencode_pending_bindings("local1", &f.runtime)
        .unwrap();
    assert!(f.queue().message_delivered(&accepted).unwrap());
    assert!(f
        .queue()
        .pending_provider_message_binding("local1", &missing)
        .unwrap()
        .is_none());
    assert_eq!(f.driver.posts(), 1);
}

#[test]
fn opencode_outbox_handoff_reservation_holds_new_rows_for_successor() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    let note = crate::handoff::execute::HandoffNote::parse(
        &json!({"kind":"path", "value":"/tmp/handoff.md"}),
    )
    .unwrap();
    f.store.accept_handoff("local1", "local1", &note).unwrap();
    f.store.claim_handoff_start("local1").unwrap().unwrap();
    assert!(!f.send("for successor", "urgent").delivered);
    f.store.drain_runtime_background_retry_messages().unwrap();
    assert_eq!(f.driver.posts(), 0);
    assert_eq!(
        f.queue()
            .pending_messages_for_target("local1", 10)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn opencode_outbox_parallel_attempts_submit_one_turn() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    let id = f
        .queue()
        .enqueue_message("local1", "one turn", "sequential", None)
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (store, runtime, barrier) = (f.store.clone(), f.runtime.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                store.drain_opencode_outbox("local1", &runtime).unwrap();
            })
        })
        .collect();
    for worker in threads {
        worker.join().unwrap();
    }
    assert!(f.queue().message_delivered(&id).unwrap());
    assert_eq!(f.driver.posts(), 1);
}

#[test]
fn opencode_outbox_http_wait_does_not_hold_registry_and_handoff_waits_for_submission() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    f.driver
        .server
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .before_lookup(Box::new(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
    let (store, runtime) = (f.store.clone(), f.runtime.clone());
    let sending = thread::spawn(move || {
        store
            .send_core_input_with_runtime(
                "local1",
                serde_json::from_value(json!({"text":"in flight", "delivery_mode":"urgent"}))
                    .unwrap(),
                &runtime,
            )
            .unwrap()
            .unwrap()
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    f.change("local1", "current_task", json!("registry remains writable"));
    let (handoff_tx, handoff_rx) = std::sync::mpsc::channel();
    let store = f.store.clone();
    let handoff = thread::spawn(move || {
        let note = crate::handoff::execute::HandoffNote::parse(
            &json!({"kind":"path", "value":"/tmp/handoff.md"}),
        )
        .unwrap();
        store.accept_handoff("local1", "local1", &note).unwrap();
        handoff_tx.send(()).unwrap();
    });
    assert!(handoff_rx.recv_timeout(Duration::from_millis(200)).is_err());
    release_tx.send(()).unwrap();
    assert!(sending.join().unwrap().delivered);
    handoff.join().unwrap();
    assert_eq!(f.driver.posts(), 1);
}

#[test]
fn model_drain_holds_all_delivery_modes_and_reload_releases_in_order() {
    let f = Fixture::new();
    f.create("local1", None).unwrap();
    let host = crate::local_model::register_outbox_fixture(
        f.queue().db_path().to_path_buf(),
        f.driver.path.clone(),
        f.store.clone(),
    );
    let db = rusqlite::Connection::open(f.queue().db_path()).unwrap();
    for state in ["draining", "yielded", "loading"] {
        db.execute("UPDATE local_model SET state=?1", [state])
            .unwrap();
        for mode in ["urgent", "sequential", "important", "steer"] {
            assert!(!f.send(&format!("{state}-{mode}"), mode).delivered);
        }
    }
    assert_eq!(
        f.queue()
            .pending_messages_for_target("local1", 100)
            .unwrap()
            .len(),
        12
    );
    assert!(!f
        .requests()
        .iter()
        .any(|(_, path, _)| path.ends_with("/prompt_async")));
    db.execute("UPDATE local_model SET state='ready'", [])
        .unwrap();
    assert!(!host.delivery_held().unwrap());
    f.store.drain_opencode_outbox("local1", &f.runtime).unwrap();
    assert!(f
        .queue()
        .pending_messages_for_target("local1", 100)
        .unwrap()
        .is_empty());
    let texts = f
        .requests()
        .iter()
        .filter(|(_, path, _)| path.ends_with("/prompt_async"))
        .map(|(_, _, body)| body["parts"][0]["text"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(texts.len(), 12);
    assert!(texts[0].contains("draining-urgent"));
    assert!(texts[11].contains("loading-steer"));
}
