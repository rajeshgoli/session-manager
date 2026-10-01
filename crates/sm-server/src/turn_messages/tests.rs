use super::*;
use rand_core::{OsRng, RngCore};
use time::macros::datetime;

fn store() -> TurnMessageStore {
    let dir = std::env::temp_dir().join(format!(
        "sm-turn-messages-{}-{}",
        std::process::id(),
        OsRng.next_u64()
    ));
    fs::create_dir_all(&dir).unwrap();
    TurnMessageStore::new(dir.join("message_queue.db"))
}

const T0: OffsetDateTime = datetime!(2026-09-30 15:00:00 UTC);

fn minutes(n: i64) -> OffsetDateTime {
    T0 + time::Duration::minutes(n)
}

#[test]
fn stop_then_task_complete_then_stop_fills_from_the_second_stop() {
    let store = store();
    store
        .record_turn("s1", "claude", minutes(0), "Working on it")
        .unwrap();
    store.record_finished("s1", minutes(1)).unwrap();
    let row = &store.finished().unwrap()[0];
    assert_eq!(row.text, None);
    store
        .record_turn("s1", "claude", minutes(2), "1855 done and closed")
        .unwrap();
    let row = &store.finished().unwrap()[0];
    assert_eq!(row.text.as_deref(), Some("1855 done and closed"));
    assert_eq!(row.text_at.as_deref(), Some(stamp(minutes(2)).as_str()));
    // A later turn does not overwrite the filled row.
    store
        .record_turn("s1", "claude", minutes(3), "Anything else?")
        .unwrap();
    assert_eq!(
        store.finished().unwrap()[0].text.as_deref(),
        Some("1855 done and closed")
    );
    assert_eq!(
        store.last_turn("s1").unwrap().unwrap().text,
        "Anything else?"
    );
}

#[test]
fn an_older_stop_arriving_late_neither_replaces_the_last_turn_nor_fills_a_newer_finish() {
    let store = store();
    store
        .record_turn("s1", "claude", minutes(5), "Newer turn")
        .unwrap();
    store.record_finished("s1", minutes(6)).unwrap();
    // A Stop emitted at minute 2 arrives now.
    store
        .record_turn("s1", "claude", minutes(2), "Older turn")
        .unwrap();
    assert_eq!(store.last_turn("s1").unwrap().unwrap().text, "Newer turn");
    assert_eq!(store.finished().unwrap()[0].text, None);
    // A Stop emitted after the completion fills it, even if it arrives late.
    store
        .record_turn("s1", "claude", minutes(7), "Closing summary")
        .unwrap();
    assert_eq!(
        store.finished().unwrap()[0].text.as_deref(),
        Some("Closing summary")
    );
}

#[test]
fn task_complete_without_a_later_stop_falls_back_after_ten_minutes() {
    let store = store();
    store
        .record_turn("s1", "claude", minutes(0), "Summary before completing")
        .unwrap();
    store.record_finished("s1", minutes(1)).unwrap();
    assert_eq!(store.sweep(minutes(10)).unwrap(), 0);
    assert_eq!(store.finished().unwrap()[0].text, None);
    assert_eq!(store.sweep(minutes(11)).unwrap(), 1);
    let row = &store.finished().unwrap()[0];
    assert_eq!(row.text.as_deref(), Some("Summary before completing"));
    assert_eq!(row.text_at.as_deref(), Some(stamp(minutes(0)).as_str()));
}

#[test]
fn a_finished_row_with_no_turn_message_at_all_stays_empty() {
    let store = store();
    store.record_finished("s1", minutes(0)).unwrap();
    assert_eq!(store.sweep(minutes(60)).unwrap(), 0);
    assert_eq!(store.finished().unwrap()[0].text, None);
}

#[test]
fn mark_read_sets_read_on_every_unread_row_of_one_session() {
    let store = store();
    store.record_finished("s1", minutes(0)).unwrap();
    store.record_finished("s1", minutes(5)).unwrap();
    store.record_finished("s2", minutes(5)).unwrap();
    assert_eq!(store.mark_read("s1", minutes(6)).unwrap(), 2);
    assert_eq!(store.mark_read("s1", minutes(7)).unwrap(), 0);
    let rows = store.finished().unwrap();
    assert!(rows
        .iter()
        .all(|row| (row.session_id == "s1") == row.read_at.is_some()));
}

#[test]
fn finished_rows_expire_after_ninety_days() {
    let store = store();
    store.record_finished("s1", minutes(0)).unwrap();
    store.sweep(T0 + time::Duration::days(89)).unwrap();
    assert_eq!(store.finished().unwrap().len(), 1);
    store.sweep(T0 + time::Duration::days(91)).unwrap();
    assert!(store.finished().unwrap().is_empty());
}

#[test]
fn text_is_capped_at_a_character_boundary() {
    let long = "é".repeat(MAX_TEXT_CHARS + 5);
    let capped = cap_text(&long);
    assert_eq!(capped.chars().count(), MAX_TEXT_CHARS + 1);
    assert!(capped.ends_with('…'));
    assert_eq!(cap_text("short"), "short");
}

#[test]
fn stamps_compare_as_strings() {
    let a = stamp_rfc3339("2026-09-30T15:00:00.5Z").unwrap();
    let b = stamp_rfc3339("2026-09-30T15:00:00.123Z").unwrap();
    assert!(b < a);
    assert_eq!(a, "2026-09-30T15:00:00.500000Z");
}

#[test]
fn a_missing_database_reads_empty() {
    let store = TurnMessageStore::new(std::env::temp_dir().join("sm-turn-missing/none.db"));
    assert!(store.last_turns().unwrap().is_empty());
    assert!(store.finished().unwrap().is_empty());
    assert_eq!(store.sweep(T0).unwrap(), 0);
}
