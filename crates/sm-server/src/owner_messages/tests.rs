use super::*;

fn store() -> (OwnerMessageStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "sm-owner-messages-{}-{}",
        std::process::id(),
        OsRng.next_u64()
    ));
    fs::create_dir_all(&dir).unwrap();
    (OwnerMessageStore::new(dir.join("message_queue.db")), dir)
}

fn new_message(sender: &str, blocking: bool) -> NewOwnerMessage {
    NewOwnerMessage {
        human: "rajesh".into(),
        sender_session_id: sender.into(),
        sender_session_name: format!("{sender}-agent"),
        title: "Keep the old fills table or drop it?".into(),
        body_markdown: "# Keep the old fills table or drop it?\nBody".into(),
        blocking,
    }
}

fn created(outcome: CreateOwnerMessage) -> OwnerMessage {
    match outcome {
        CreateOwnerMessage::Created(message) => *message,
        CreateOwnerMessage::UnreadCapReached => panic!("unexpected unread cap"),
    }
}

fn message(blocking: bool) -> OwnerMessage {
    OwnerMessage {
        id: "msg_3f9a2c1d".into(),
        human: "rajesh".into(),
        sender_session_id: "eng00001".into(),
        sender_session_name: "sm-1679-engineer".into(),
        title: "Keep the old fills table or drop it?".into(),
        body_markdown: String::new(),
        blocking,
        created_at: "2026-09-26T10:00:00Z".into(),
        first_viewed_at: None,
        handled_at: None,
        handled_via: None,
    }
}

#[test]
fn derived_titles() {
    // The spec's table (appendix D2).
    assert_eq!(
        derive_title("# Keep the old fills table?\n\nBody."),
        "Keep the old fills table?"
    );
    assert_eq!(
        derive_title("**Done:** stage 2 is green."),
        "Done: stage 2 is green."
    );
    assert_eq!(derive_title("- one\n- two"), "one");
    // A heading anywhere in the first 10 non-blank lines wins, setext too.
    assert_eq!(
        derive_title("intro line\n\n## Decision ##\nmore"),
        "Decision"
    );
    assert_eq!(
        derive_title("Status report\n=============\nbody"),
        "Status report"
    );
    assert_eq!(derive_title("Status report\n---\nbody"), "Status report");
    let late = format!("{}# Too late", "line\n".repeat(10));
    assert_eq!(derive_title(&late), "line");
    // Leading markers go; a number that isn't a list marker stays.
    assert_eq!(derive_title("> 1. `fills_v2` is live"), "fillsv2 is live");
    assert_eq!(derive_title("3.5% of rows differ"), "3.5% of rows differ");
    assert_eq!(derive_title("  lots   of\tspace  "), "lots of space");
    // `#hashtag` is not a heading.
    assert_eq!(derive_title("#hashtag"), "hashtag");
    // Cut at 120 characters on a character boundary.
    let long = "é".repeat(130);
    assert_eq!(derive_title(&long), format!("{}…", "é".repeat(120)));
    assert_eq!(derive_title(&"é".repeat(120)), "é".repeat(120));
    assert_eq!(derive_title("***"), "Message");
}

#[test]
fn titles_are_validated() {
    assert_eq!(validate_title("  Ship it?  ").unwrap(), "Ship it?");
    assert!(validate_title("   ").is_err());
    assert!(validate_title(&"a".repeat(121)).is_err());
    assert_eq!(validate_title(&"é".repeat(120)).unwrap(), "é".repeat(120));
}

#[test]
fn message_ids_are_msg_and_eight_hex() {
    assert!(is_owner_message_id("msg_3f9a2c1d"));
    for bad in [
        "msg_3F9A2C1D",
        "msg_3f9a2c1",
        "3f9a2c1d",
        "msg_3f9a2c1g",
        "msg_3f9a2c1d0",
    ] {
        assert!(!is_owner_message_id(bad), "{bad}");
    }
}

#[test]
fn derived_states() {
    let plain = message(false);
    assert_eq!(
        derive_message_state(&plain, false, false),
        OwnerMessageState::New
    );
    let mut read = plain.clone();
    read.first_viewed_at = Some("2026-09-26T10:01:00Z".into());
    assert_eq!(
        derive_message_state(&read, false, false),
        OwnerMessageState::Read
    );
    assert_eq!(
        derive_message_state(&read, true, false),
        OwnerMessageState::Replied
    );

    let blocking = message(true);
    assert_eq!(
        derive_message_state(&blocking, false, false),
        OwnerMessageState::NeedsYou
    );
    // A blocking message whose sender ended no longer needs the owner.
    assert_eq!(
        derive_message_state(&blocking, false, true),
        OwnerMessageState::New
    );
    let mut viewed = blocking.clone();
    viewed.first_viewed_at = Some("2026-09-26T10:01:00Z".into());
    assert_eq!(
        derive_message_state(&viewed, false, false),
        OwnerMessageState::NeedsYou
    );
    assert_eq!(
        derive_message_state(&viewed, false, true),
        OwnerMessageState::Read
    );
    let mut handled = viewed.clone();
    handled.handled_at = Some("2026-09-26T10:02:00Z".into());
    assert_eq!(
        derive_message_state(&handled, false, false),
        OwnerMessageState::Handled
    );
    // Replied beats handled.
    assert_eq!(
        derive_message_state(&handled, true, false),
        OwnerMessageState::Replied
    );
}

#[test]
fn answering_session_clears_only_open_blocking_messages_and_records_source() {
    let (store, dir) = store();
    let first = created(store.create(new_message("agent-a", true)).unwrap());
    let second = created(store.create(new_message("agent-a", true)).unwrap());
    let other = created(store.create(new_message("agent-b", true)).unwrap());
    let plain = created(store.create(new_message("agent-a", false)).unwrap());
    assert_eq!(store.answer_session("agent-a", "terminal").unwrap(), 2);
    assert_eq!(store.answer_session("agent-a", "manual").unwrap(), 0);
    for id in [&first.id, &second.id] {
        let message = store.get(id).unwrap().unwrap();
        assert!(message.handled_at.is_some());
        assert_eq!(message.handled_via.as_deref(), Some("terminal"));
    }
    assert!(store.get(&other.id).unwrap().unwrap().handled_at.is_none());
    assert!(store.get(&plain.id).unwrap().unwrap().handled_at.is_none());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn handled_via_column_migrates_existing_message_database() {
    let (store, dir) = store();
    let db_path = dir.join("message_queue.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE owner_messages (
        id TEXT PRIMARY KEY, human TEXT NOT NULL, sender_session_id TEXT NOT NULL,
        sender_session_name TEXT NOT NULL, title TEXT NOT NULL, body_markdown TEXT NOT NULL,
        blocking INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL,
        first_viewed_at TEXT, handled_at TEXT)",
    )
    .unwrap();
    drop(conn);
    store.ensure_schema().unwrap();
    let columns: Vec<String> = Connection::open(&db_path)
        .unwrap()
        .prepare("PRAGMA table_info(owner_messages)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(columns.contains(&"handled_via".to_owned()));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn delivered_text_format() {
    let comments = vec![ReplyComment {
        line: Some(5),
        quote: "Nothing reads `fills` after the cutover.".into(),
        body: "The month-end recon does. Check it first.".into(),
    }];
    // The memo's example, byte for byte.
    assert_eq!(
        render_delivered_text("Rajesh", &message(true), "  Otherwise yes, drop it.\n", &comments),
        "[Input from: Rajesh via sm app] Re: \"Keep the old fills table or drop it?\" (msg_3f9a2c1d)\n\
         Otherwise yes, drop it.\n\
         \n\
         > Nothing reads `fills` after the cutover.\n\
         The month-end recon does. Check it first."
    );
    // No overall text: the first comment follows the header directly;
    // multi-line quotes quote each line; comments are a blank line apart;
    // a comment with no quote is its body alone.
    let comments = vec![
        ReplyComment {
            line: Some(1),
            quote: "first line\nsecond line".into(),
            body: "Why?".into(),
        },
        ReplyComment {
            line: None,
            quote: String::new(),
            body: "General note.".into(),
        },
    ];
    assert_eq!(
        render_delivered_text("Owner", &message(false), "", &comments),
        "[Input from: Owner via sm app] Re: \"Keep the old fills table or drop it?\" (msg_3f9a2c1d)\n\
         > first line\n\
         > second line\n\
         Why?\n\
         \n\
         General note."
    );
    // Only overall text.
    assert_eq!(
        render_delivered_text("Owner", &message(false), "Yes.", &[]),
        "[Input from: Owner via sm app] Re: \"Keep the old fills table or drop it?\" (msg_3f9a2c1d)\nYes."
    );
    // Quotes are cut at 300 characters.
    let long = vec![ReplyComment {
        line: Some(1),
        quote: format!("  {}  ", "é".repeat(301)),
        body: "Too long.".into(),
    }];
    assert!(render_delivered_text("Owner", &message(false), "", &long)
        .ends_with(&format!("> {}…\nToo long.", "é".repeat(300))));
}

#[test]
fn reply_comments_order_by_line_then_age_with_unplaceable_last() {
    let draft = |id: &str, line: Option<i64>, created_at: &str| OwnerMessageDraft {
        id: id.into(),
        message_id: "msg_3f9a2c1d".into(),
        line,
        quote: id.into(),
        body: id.into(),
        created_at: created_at.into(),
        updated_at: created_at.into(),
    };
    let ordered = order_reply_comments(&[
        draft("none", None, "2026-09-26T10:00:00Z"),
        draft("late7", Some(7), "2026-09-26T10:03:00Z"),
        draft("line2", Some(2), "2026-09-26T10:05:00Z"),
        draft("early7", Some(7), "2026-09-26T10:01:00Z"),
    ]);
    let bodies: Vec<_> = ordered.iter().map(|c| c.body.as_str()).collect();
    assert_eq!(bodies, ["line2", "early7", "late7", "none"]);
}

#[test]
fn unread_cap_counts_per_sender_and_human() {
    let (store, dir) = store();
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(created(store.create(new_message("eng00001", false)).unwrap()).id);
    }
    assert_eq!(store.unread_count("eng00001", "rajesh").unwrap(), 5);
    assert_eq!(
        store.create(new_message("eng00001", false)).unwrap(),
        CreateOwnerMessage::UnreadCapReached
    );
    // Another sender is not affected.
    created(store.create(new_message("eng00002", false)).unwrap());
    // Opening any one lets the sender send again.
    store.mark_viewed(&ids[2]).unwrap();
    assert_eq!(store.unread_count("eng00001", "rajesh").unwrap(), 4);
    let sixth = created(store.create(new_message("eng00001", true)).unwrap());
    assert!(is_owner_message_id(&sixth.id));
    assert!(sixth.blocking);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn recording_a_reply_deletes_drafts_marks_viewed_and_queues_once() {
    let (store, dir) = store();
    let message = created(store.create(new_message("eng00001", true)).unwrap());
    let draft = store
        .create_draft(&message.id, Some(2), "Body", "Why?")
        .unwrap();
    let edited = store
        .update_draft(&message.id, &draft.id, "Why not?")
        .unwrap()
        .unwrap();
    assert_eq!(edited.body, "Why not?");
    let kept = store.create_draft(&message.id, None, "", "later").unwrap();
    let reply = RecordReply {
        submission_id: "sub-00000001".into(),
        message_id: message.id.clone(),
        body: "Yes.".into(),
        comments: order_reply_comments(std::slice::from_ref(&edited)),
        delivered_text: "[Input from: Owner via sm app] ...".into(),
        recipient_session_id: "eng00001".into(),
        draft_ids: vec![draft.id.clone()],
    };
    let (stored, inserted) = store.record_reply(&reply).unwrap();
    assert!(inserted);
    assert_eq!(stored.delivered_to_session_id, "eng00001");
    assert_eq!(stored.comments[0].body, "Why not?");
    let (_, inserted) = store.record_reply(&reply).unwrap();
    assert!(!inserted, "a resubmit records nothing new");
    assert_eq!(store.drafts(&message.id).unwrap(), vec![kept]);
    assert!(store
        .get(&message.id)
        .unwrap()
        .unwrap()
        .first_viewed_at
        .is_some());
    assert!(store.has_reply(&message.id).unwrap());
    assert_eq!(store.replies(&message.id).unwrap().len(), 1);
    let conn = Connection::open(dir.join("message_queue.db")).unwrap();
    let queued: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM message_queue
             WHERE id = 'owner-reply-sub-00000001' AND target_session_id = 'eng00001'
               AND delivery_mode = 'sequential' AND sender_session_id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(queued, 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn obligations_list_recent_messages_and_unanswered_blocking_ones() {
    let (store, dir) = store();
    let plain = created(store.create(new_message("eng00001", false)).unwrap());
    let blocking = created(store.create(new_message("eng00001", true)).unwrap());
    let handled = created(store.create(new_message("eng00001", true)).unwrap());
    store.mark_handled(&handled.id).unwrap();
    // Everything is recent against an old cutoff.
    let all = store.for_obligations("2000-01-01T00:00:00Z").unwrap();
    assert_eq!(all.len(), 3);
    // Against a future cutoff only the unanswered blocking message stays.
    let old = store.for_obligations("2999-01-01T00:00:00Z").unwrap();
    let ids: Vec<_> = old.iter().map(|(message, _)| message.id.clone()).collect();
    assert_eq!(ids, vec![blocking.id.clone()]);
    assert!(!ids.contains(&plain.id));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_db_reads_as_empty() {
    let dir = std::env::temp_dir().join(format!("sm-owner-messages-missing-{}", OsRng.next_u64()));
    let store = OwnerMessageStore::new(dir.join("message_queue.db"));
    assert!(store.get("msg_00000000").unwrap().is_none());
    assert_eq!(store.unread_count("x", "rajesh").unwrap(), 0);
    assert!(store
        .for_obligations("2000-01-01T00:00:00Z")
        .unwrap()
        .is_empty());
    assert!(!dir.exists());
}
