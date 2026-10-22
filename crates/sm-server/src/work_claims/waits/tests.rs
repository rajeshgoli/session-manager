use super::*;

const REPO: &str = "acme/widgets";

fn new_store() -> (WorkClaimStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "sm-agent-waits-{}-{}",
        std::process::id(),
        OsRng.next_u64()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db = dir.join("message_queue.db");
    let store = WorkClaimStore::new(db.clone());
    store.ensure_schema().unwrap();
    (store, db)
}

fn session(id: &str, name: &str, state: HolderState) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        name: name.into(),
        parent_session_id: None,
        state,
        stopped_at: None,
    }
}

/// `waiter` (far-2139) waits; `target` (far-2171) delivers; `holder` holds
/// ticket #7; `gone` is retired.
fn directory() -> SessionDirectory {
    SessionDirectory::new([
        session("7e317fcd", "far-2139", HolderState::Idle),
        session("4c1d2e3f", "far-2171", HolderState::Working),
        session("holder01", "holder", HolderState::Working),
        session("gone0001", "gone", HolderState::Retired),
    ])
}

const WAITER: &str = "7e317fcd";
const TARGET: &str = "4c1d2e3f";

fn at(minutes: i64) -> OffsetDateTime {
    OffsetDateTime::parse("2026-10-21T22:00:00Z", &Rfc3339).unwrap()
        + time::Duration::minutes(minutes)
}

fn seat(id: &str) -> WaitTarget {
    WaitTarget::Seat(id.into())
}

fn ticket(number: i64) -> WaitTarget {
    WaitTarget::Ticket {
        repo: REPO.into(),
        number,
    }
}

fn queued(db: &PathBuf, target: &str) -> Vec<String> {
    let conn = Connection::open(db).unwrap();
    if !table_exists(&conn, "message_queue").unwrap() {
        return Vec::new();
    }
    let mut statement = conn
        .prepare("SELECT text FROM message_queue WHERE target_session_id = ?1 ORDER BY queued_at")
        .unwrap();
    statement
        .query_map(params![target], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

fn end_reasons(store: &WorkClaimStore) -> Vec<String> {
    store
        .events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "wait.ended")
        .map(|event| event.payload["reason"].as_str().unwrap().to_owned())
        .collect()
}

fn found(state: &str) -> ItemFetch {
    ItemFetch::Found(Box::new(GhItem {
        is_draft: false,
        kind: WorkKind::Ticket,
        title: "Final weights".into(),
        state: state.into(),
        state_reason: None,
        url: "https://github.com/acme/widgets/issues/7".into(),
        head_ref: None,
        head_sha: None,
        closed_at: None,
        merged_at: None,
        closing_refs: None,
    }))
}

fn answered(numbers: &[i64]) -> BTreeSet<(String, i64)> {
    numbers.iter().map(|n| (REPO.to_owned(), *n)).collect()
}

fn hold_ticket(store: &WorkClaimStore, holder: &str, dir: &SessionDirectory) {
    let request = ClaimRequest {
        repo: REPO.into(),
        number: 7,
        kind: WorkKind::Ticket,
        claimant: dir.get(holder).unwrap().clone(),
        source: ClaimSource::Explicit,
        take: false,
        worktree_path: None,
        branch: None,
        tickets: Vec::new(),
        reserve: false,
    };
    let fetched: Result<BatchFetch, String> = Ok([(7, found("open"))].into_iter().collect());
    store.claim_explicit(&request, fetched, dir).unwrap();
}

#[test]
fn validation_bounds_deliverable_and_eta() {
    assert_eq!(validate_deliverable("  hashes ").unwrap(), "hashes");
    for bad in ["", "   ", "two\nlines", &"x".repeat(201)] {
        assert_eq!(
            validate_deliverable(bad).unwrap_err(),
            "for must be one line of 1 to 200 characters"
        );
    }
    assert!(validate_deliverable(&"x".repeat(200)).is_ok());
    let now = at(0);
    assert_eq!(
        validate_until("2026-10-22T05:30:00Z", now).unwrap(),
        at(450)
    );
    assert!(validate_until("2026-10-24T22:00:00Z", now).is_ok());
    for bad in [
        "2026-10-21T22:00:00Z",
        "2026-10-24T22:00:01Z",
        "6h",
        "tomorrow",
    ] {
        assert_eq!(
            validate_until(bad, now).unwrap_err(),
            "until must be in the future and within 72h"
        );
    }
    assert_eq!(eta_text(at(450)), "Thu 22 Oct 05:30 UTC");
}

#[test]
fn reopening_a_wait_updates_it_and_keeps_its_age() {
    let (store, _db) = new_store();
    let (first, created) = store
        .open_wait(WAITER, &seat(TARGET), "hashes", at(450), at(0))
        .unwrap();
    assert!(created);
    assert!(first.id.starts_with("wait_") && first.id.len() == 17);
    let (second, created) = store
        .open_wait(WAITER, &seat(TARGET), "end.pt hashes", at(480), at(10))
        .unwrap();
    assert!(!created);
    assert_eq!(second.id, first.id);
    assert_eq!(second.created_at, first.created_at);
    assert_eq!(second.deliverable, "end.pt hashes");
    assert_eq!(second.until_at, checks::format_time(at(480)));
    store
        .open_wait(WAITER, &ticket(7), "weights", at(450), at(10))
        .unwrap();
    assert_eq!(store.open_waits().unwrap().len(), 2);
    let kinds: Vec<String> = store
        .events()
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .filter(|kind| kind.starts_with("wait."))
        .collect();
    assert_eq!(kinds, ["wait.opened", "wait.updated", "wait.opened"]);
}

#[test]
fn a_message_from_the_target_or_ticket_holder_delivers() {
    let (store, db) = new_store();
    let dir = directory();
    hold_ticket(&store, "holder01", &dir);
    store
        .open_wait(WAITER, &seat(TARGET), "hashes", at(450), at(0))
        .unwrap();
    store
        .open_wait(WAITER, &ticket(7), "weights", at(450), at(0))
        .unwrap();
    // Another seat, or the waiter itself, delivers nothing.
    assert_eq!(store.deliver_waits("holder01", TARGET, at(1)).unwrap(), 0);
    assert_eq!(store.deliver_waits("gone0001", WAITER, at(1)).unwrap(), 0);
    assert_eq!(store.deliver_waits(WAITER, WAITER, at(1)).unwrap(), 0);
    // The awaited seat ends only its own wait.
    assert_eq!(store.deliver_waits(TARGET, WAITER, at(2)).unwrap(), 1);
    // A holder of the awaited ticket ends the ticket wait.
    assert_eq!(store.deliver_waits("holder01", WAITER, at(3)).unwrap(), 1);
    assert!(store.open_waits().unwrap().is_empty());
    assert_eq!(end_reasons(&store), ["delivered", "delivered"]);
    // The delivered message is the wake: sm adds none.
    assert!(queued(&db, WAITER).is_empty());
}

#[test]
fn sync_endings_message_the_waiter_once() {
    let (store, db) = new_store();
    let mut dir = directory();
    store
        .open_wait(WAITER, &seat(TARGET), "hashes", at(450), at(0))
        .unwrap();
    store
        .open_wait(WAITER, &ticket(7), "weights", at(450), at(1))
        .unwrap();
    store
        .open_wait(WAITER, &ticket(8), "fixture", at(450), at(2))
        .unwrap();
    store
        .open_wait("holder01", &seat(TARGET), "review", at(60), at(3))
        .unwrap();
    // The awaited tickets joined the tracked set.
    assert_eq!(store.tracked_items().unwrap()[REPO], vec![7, 8]);
    // Nothing is due yet.
    assert!(store
        .run_wait_endings(&answered(&[]), &dir, at(30))
        .unwrap()
        .is_empty());
    // #7 closed, #8 vanished; the holder's ETA passed.
    let fetched: Result<BatchFetch, String> = Ok([(7, found("closed")), (8, ItemFetch::NotFound)]
        .into_iter()
        .collect());
    store.record_fetch(REPO, &[7, 8], &fetched).unwrap();
    let mut notified = store
        .run_wait_endings(&answered(&[7, 8]), &dir, at(60))
        .unwrap();
    notified.sort();
    assert_eq!(notified, ["7e317fcd", "holder01"]);
    assert_eq!(
        queued(&db, WAITER),
        [
            "[sm await] #7 \"Final weights\" closed. Your wait on it for \"weights\" ended.",
            "[sm await] #8 was not found on GitHub. Your wait on it for \"fixture\" ended.",
        ]
    );
    assert_eq!(
        queued(&db, "holder01"),
        ["[sm await] Your wait on far-2171 for \"review\" passed its ETA, Wed 21 Oct 23:00 UTC, and ended."]
    );
    // The target retires: its remaining waiter is told who.
    dir.insert(session(TARGET, "far-2171", HolderState::Retired));
    assert_eq!(
        store
            .run_wait_endings(&answered(&[]), &dir, at(61))
            .unwrap(),
        [WAITER]
    );
    assert_eq!(
        queued(&db, WAITER)[2],
        "[sm await] far-2171 (4c1d2e3f) retired without messaging you. Your wait on it for \"hashes\" ended."
    );
    assert_eq!(
        end_reasons(&store),
        [
            "ticket_closed",
            "ticket_closed",
            "expired",
            "target_retired"
        ]
    );
    assert!(store.open_waits().unwrap().is_empty());
}

#[test]
fn precedence_waiter_retired_then_target_retired_then_expired() {
    let (store, db) = new_store();
    let mut dir = directory();
    store
        .open_wait(WAITER, &seat(TARGET), "hashes", at(30), at(0))
        .unwrap();
    store
        .open_wait("holder01", &seat(TARGET), "review", at(30), at(1))
        .unwrap();
    // Both waits are past their ETA and the target retired; the waiter of
    // the first retired too.
    dir.insert(session(TARGET, "far-2171", HolderState::Retired));
    dir.insert(session(WAITER, "far-2139", HolderState::Retired));
    assert_eq!(
        store
            .run_wait_endings(&answered(&[]), &dir, at(60))
            .unwrap(),
        ["holder01"]
    );
    assert!(queued(&db, WAITER).is_empty());
    assert_eq!(queued(&db, "holder01").len(), 1);
    assert!(queued(&db, "holder01")[0].contains("retired without messaging you"));
    assert_eq!(end_reasons(&store), ["waiter_retired", "target_retired"]);
}

#[test]
fn cancel_and_retire_end_waits_silently() {
    let (store, db) = new_store();
    store
        .open_wait(WAITER, &seat(TARGET), "hashes", at(450), at(0))
        .unwrap();
    store
        .open_wait(WAITER, &ticket(7), "weights", at(450), at(0))
        .unwrap();
    store
        .open_wait("holder01", &seat(TARGET), "review", at(450), at(0))
        .unwrap();
    assert_eq!(
        store
            .cancel_waits(WAITER, Some(&seat("other")), at(1))
            .unwrap(),
        0
    );
    assert_eq!(
        store.cancel_waits(WAITER, Some(&ticket(7)), at(1)).unwrap(),
        1
    );
    assert_eq!(store.cancel_waits(WAITER, None, at(2)).unwrap(), 1);
    store.end_claims_for_session("holder01").unwrap();
    assert!(store.open_waits().unwrap().is_empty());
    assert_eq!(
        end_reasons(&store),
        ["cancelled", "cancelled", "waiter_retired"]
    );
    assert!(queued(&db, WAITER).is_empty());
}

#[test]
fn task_complete_lists_live_waiters_on_the_seat_and_its_tickets() {
    let (store, db) = new_store();
    let dir = directory();
    hold_ticket(&store, "holder01", &dir);
    assert!(store
        .notify_awaited_at_task_complete("holder01", &dir)
        .unwrap()
        .is_empty());
    store
        .open_wait(WAITER, &ticket(7), "weights", at(450), at(0))
        .unwrap();
    store
        .open_wait(TARGET, &seat("holder01"), "review", at(60), at(1))
        .unwrap();
    store
        .open_wait("gone0001", &seat("holder01"), "ignored", at(60), at(2))
        .unwrap();
    assert_eq!(
        store
            .notify_awaited_at_task_complete("holder01", &dir)
            .unwrap(),
        ["holder01"]
    );
    assert_eq!(
        queued(&db, "holder01"),
        ["[sm await] At task-complete, far-2139 (7e317fcd) waits on you for \"weights\", ETA Thu 22 Oct 05:30 UTC; far-2171 (4c1d2e3f) waits on you for \"review\", ETA Wed 21 Oct 23:00 UTC."]
    );
    assert_eq!(store.open_waits().unwrap().len(), 3);
}

#[test]
fn handoff_moves_open_waits_to_the_successor() {
    let (store, _db) = new_store();
    store
        .open_wait(WAITER, &seat(TARGET), "hashes", at(450), at(0))
        .unwrap();
    store
        .open_wait("holder01", &seat(WAITER), "review", at(450), at(1))
        .unwrap();
    store.hand_off(WAITER, "succ0001", "far-2139").unwrap();
    let moved: Vec<(String, Option<String>)> = store
        .open_waits()
        .unwrap()
        .into_iter()
        .map(|wait| (wait.waiter_session_id, wait.target_session_id))
        .collect();
    assert_eq!(
        moved,
        [
            ("succ0001".to_owned(), Some(TARGET.to_owned())),
            ("holder01".to_owned(), Some("succ0001".to_owned())),
        ]
    );
}

#[test]
fn restore_after_retire_reopens_waits_for_the_successor() {
    let (store, _db) = new_store();
    store
        .open_wait(WAITER, &seat(TARGET), "hashes", at(450), at(0))
        .unwrap();
    // An older ended wait stays ended.
    store
        .open_wait(WAITER, &seat("holder01"), "old", at(450), at(1))
        .unwrap();
    store
        .cancel_waits(WAITER, Some(&seat("holder01")), at(2))
        .unwrap();
    let before_retire = checks::format_time(OffsetDateTime::now_utc() - time::Duration::seconds(5));
    store.end_claims_for_session(WAITER).unwrap();
    assert!(store.open_waits().unwrap().is_empty());
    store.reopen_retired_claims(WAITER, &before_retire).unwrap();
    store.hand_off(WAITER, "succ0001", "far-2139").unwrap();
    let open = store.open_waits().unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(
        (
            open[0].waiter_session_id.as_str(),
            open[0].deliverable.as_str()
        ),
        ("succ0001", "hashes")
    );
}

#[test]
fn a_wait_on_a_ticket_already_cached_closed_ends_at_the_next_pass() {
    let (store, db) = new_store();
    let fetched: Result<BatchFetch, String> = Ok([(7, found("closed"))].into_iter().collect());
    store.record_fetch(REPO, &[7], &fetched).unwrap();
    store
        .open_wait(WAITER, &ticket(7), "weights", at(450), at(0))
        .unwrap();
    // Closed items are never re-polled, so this pass has no answer for #7.
    assert!(store.tracked_items().unwrap().is_empty());
    assert_eq!(
        store
            .run_wait_endings(&answered(&[]), &directory(), at(5))
            .unwrap(),
        [WAITER]
    );
    assert_eq!(
        queued(&db, WAITER),
        ["[sm await] #7 \"Final weights\" closed. Your wait on it for \"weights\" ended."]
    );
}
