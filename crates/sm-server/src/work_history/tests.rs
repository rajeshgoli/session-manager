use super::*;
use crate::owner_docs::init_owner_docs_schema;
use crate::work_claims::{init_work_claims_schema, SessionInfo};
use rusqlite::params;
use serde_json::json;
use std::path::PathBuf;

const REPO: &str = "acme/widgets";

fn now() -> OffsetDateTime {
    parse_time("2026-09-24T12:00:00Z").unwrap()
}

struct Db {
    path: PathBuf,
    conn: Connection,
}

fn db() -> Db {
    let dir = std::env::temp_dir().join(format!(
        "sm-work-history-{}-{}",
        std::process::id(),
        rand_core::RngCore::next_u64(&mut rand_core::OsRng)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("message_queue.db");
    let conn = Connection::open(&path).unwrap();
    init_work_claims_schema(&conn).unwrap();
    init_owner_docs_schema(&conn).unwrap();
    conn.execute_batch(
        "CREATE TABLE codex_review_request_registrations (
            id TEXT PRIMARY KEY, repo TEXT NOT NULL, pr_number INTEGER NOT NULL,
            requester_session_id TEXT, requested_at TEXT NOT NULL,
            review_landed_at TEXT, review_url TEXT)",
    )
    .unwrap();
    Db { path, conn }
}

impl Db {
    fn item(&self, number: i64, kind: &str, state: &str, synced_at: Option<&str>) {
        let url = if kind == "pr" {
            format!("https://github.com/{REPO}/pull/{number}")
        } else {
            format!("https://github.com/{REPO}/issues/{number}")
        };
        self.conn
            .execute(
                "INSERT INTO work_items (repo, number, kind, title, state, url, synced_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    REPO,
                    number,
                    kind,
                    format!("Item {number}"),
                    state,
                    url,
                    synced_at
                ],
            )
            .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn claim(
        &self,
        id: &str,
        number: i64,
        kind: &str,
        session: &str,
        claimed_at: &str,
        ended: Option<(&str, &str)>,
        worktree: Option<&str>,
    ) {
        self.conn
            .execute(
                "INSERT INTO work_claims (id, repo, number, kind, session_id, session_name,
                    source, worktree_path, claimed_at, ended_at, end_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'explicit', ?7, ?8, ?9, ?10)",
                params![
                    id,
                    REPO,
                    number,
                    kind,
                    session,
                    format!("{session}-snapshot"),
                    worktree,
                    claimed_at,
                    ended.map(|e| e.0),
                    ended.map(|e| e.1)
                ],
            )
            .unwrap();
    }

    fn link(&self, pr: i64, ticket: i64) {
        self.conn
            .execute(
                "INSERT INTO work_links (repo, pr_number, ticket_number, source, created_at)
                 VALUES (?1, ?2, ?3, 'closing_ref', '2026-09-24T00:00:00Z')",
                params![REPO, pr, ticket],
            )
            .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn event(
        &self,
        ts: &str,
        kind: &str,
        session: Option<&str>,
        ticket: Option<i64>,
        pr: Option<i64>,
        payload: Value,
    ) {
        self.conn
            .execute(
                "INSERT INTO events (ts, kind, session_id, repo, ticket, pr, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![ts, kind, session, REPO, ticket, pr, payload.to_string()],
            )
            .unwrap();
    }

    fn review(&self, id: &str, pr: i64, requester: &str, at: &str, landed: Option<&str>) {
        self.conn
            .execute(
                "INSERT INTO codex_review_request_registrations
                    (id, repo, pr_number, requester_session_id, requested_at, review_landed_at,
                     review_url)
                 VALUES (?1, 'Acme/Widgets', ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    pr,
                    requester,
                    at,
                    landed,
                    landed.map(|_| format!("https://github.com/{REPO}/pull/{pr}#r1"))
                ],
            )
            .unwrap();
    }

    fn doc(&self, id: &str, pr: Option<i64>, author: &str, published_at: &str) {
        self.conn
            .execute(
                "INSERT INTO owner_docs (id, repo, path, pr_number, author_session_id,
                    author_session_name, title, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                params![
                    id,
                    REPO,
                    format!("docs/{id}.md"),
                    pr,
                    author,
                    format!("{author}-snapshot"),
                    format!("Doc {id}"),
                    published_at
                ],
            )
            .unwrap();
        self.conn
            .execute(
                "INSERT INTO owner_doc_publishes (doc_id, commit_sha, blob_sha, session_id,
                    review_requested, published_at)
                 VALUES (?1, ?2, 'b', ?3, 1, ?4)",
                params![id, "c".repeat(40), author, published_at],
            )
            .unwrap();
    }

    fn load(&self) -> HistoryData {
        HistoryData::load(&self.path).unwrap()
    }
}

fn session(id: &str, parent: Option<&str>, state: HolderState) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        name: format!("{id}-live"),
        parent_session_id: parent.map(Into::into),
        state,
        stopped_at: None,
    }
}

/// lead → (eng1, eng2); other unrelated; asleep stopped. `gone` is purged.
fn sessions() -> SessionDirectory {
    SessionDirectory::new([
        session("lead", None, HolderState::Idle),
        session("eng1", Some("lead"), HolderState::Working),
        session("eng2", Some("lead"), HolderState::Working),
        session("other", None, HolderState::Working),
        session("asleep", None, HolderState::Stopped),
    ])
}

fn query() -> HistoryQuery {
    HistoryQuery {
        limit: DEFAULT_LIMIT,
        ..HistoryQuery::default()
    }
}

fn row(data: &HistoryData, number: i64) -> HistoryRow {
    data.list(&sessions(), &query(), now())
        .rows
        .into_iter()
        .find(|row| row.number == number)
        .unwrap_or_else(|| panic!("no row for #{number}"))
}

fn numbers(page: &HistoryPage) -> Vec<i64> {
    page.rows.iter().map(|row| row.number).collect()
}

const SYNCED: Option<&str> = Some("2026-09-24T11:30:00Z");

#[test]
fn rows_are_tickets_and_unlinked_prs_with_agents_prs_and_docs() {
    let db = db();
    db.item(1, "ticket", "open", SYNCED);
    db.item(2, "ticket", "open", SYNCED);
    db.item(9, "pr", "open", SYNCED);
    db.item(10, "pr", "merged", SYNCED);
    db.item(11, "pr", "open", SYNCED);
    // PR 9 closes both tickets; PR 11 links to nothing.
    db.link(9, 1);
    db.link(9, 2);
    db.claim(
        "c1",
        1,
        "ticket",
        "eng1",
        "2026-09-24T01:00:00Z",
        None,
        None,
    );
    db.claim(
        "c2",
        9,
        "pr",
        "gone",
        "2026-09-24T00:30:00Z",
        Some(("2026-09-24T03:00:00Z", "retired")),
        None,
    );
    db.claim("c3", 9, "pr", "eng1", "2026-09-24T02:00:00Z", None, None);
    db.review(
        "r1",
        9,
        "eng1",
        "2026-09-24T02:10:00Z",
        Some("2026-09-24T02:20:00Z"),
    );
    db.review("r2", 9, "eng1", "2026-09-24T02:30:00Z", None);
    db.doc("d1", Some(9), "eng1", "2026-09-24T02:40:00Z");
    let data = db.load();
    let page = data.list(&sessions(), &query(), now());
    // PR 9 appears under both tickets, not on its own; 10 and 11 are unlinked.
    let mut all = numbers(&page);
    all.sort_unstable();
    assert_eq!(all, vec![1, 2, 10, 11]);
    assert!(!page.multi_repo);

    let one = row(&data, 1);
    assert_eq!(one.history_path, "/t/widgets/1");
    assert_eq!(one.title, "Item 1");
    // Earliest first; the purged session is retired under its snapshot name.
    assert_eq!(
        one.agents
            .iter()
            .map(|a| (a.name.as_str(), a.state.as_str(), a.end_reason.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            ("gone-snapshot", "retired", Some("retired")),
            ("eng1-live", "working", None)
        ]
    );
    assert_eq!(one.agents[1].claimed_at, "2026-09-24T01:00:00Z");
    assert_eq!(
        one.prs,
        vec![RowPr {
            number: 9,
            title: "Item 9".into(),
            state: "open".into(),
            url: format!("https://github.com/{REPO}/pull/9"),
            codex_requested: 2,
            codex_landed: 1,
        }]
    );
    assert_eq!(one.docs.len(), 1);
    assert_eq!(one.docs[0].name, "widgets/docs/d1.md");
    assert_eq!(one.docs[0].state, "review_requested");
    assert_eq!(
        one.docs[0].reader_path,
        "/docs/widgets/docs/d1.md?version=cccccccccccc"
    );
    assert_eq!(one.last_activity.as_deref(), Some("2026-09-24T03:00:00Z"));
    assert!(one.flags.is_empty(), "{:?}", one.flags);
    // Ticket 2 shares PR 9's facts but has no claim of its own.
    assert_eq!(row(&data, 2).agents.len(), 2);
    assert_eq!(row(&data, 11).prs[0].number, 11);
}

#[test]
fn commit_only_docs_join_the_ticket_their_publisher_held_at_the_time() {
    let db = db();
    db.item(1, "ticket", "open", SYNCED);
    db.claim(
        "c1",
        1,
        "ticket",
        "eng1",
        "2026-09-24T01:00:00Z",
        Some(("2026-09-24T05:00:00Z", "released")),
        None,
    );
    db.doc("inside", None, "eng1", "2026-09-24T02:00:00Z");
    db.doc("after", None, "eng1", "2026-09-24T06:00:00Z");
    db.doc("stranger", None, "eng2", "2026-09-24T02:00:00Z");
    let data = db.load();
    let docs: Vec<String> = row(&data, 1).docs.into_iter().map(|d| d.id).collect();
    assert_eq!(docs, vec!["inside"]);
}

#[test]
fn two_agents_flags_unrelated_live_holders_only() {
    let db = db();
    for n in 1..=4 {
        db.item(n, "ticket", "open", SYNCED);
    }
    // Siblings collide.
    db.claim(
        "a1",
        1,
        "ticket",
        "eng1",
        "2026-09-24T01:00:00Z",
        None,
        None,
    );
    db.claim(
        "a2",
        1,
        "ticket",
        "eng2",
        "2026-09-24T01:00:00Z",
        None,
        None,
    );
    // Parent and child share.
    db.claim(
        "b1",
        2,
        "ticket",
        "lead",
        "2026-09-24T01:00:00Z",
        None,
        None,
    );
    db.claim(
        "b2",
        2,
        "ticket",
        "eng1",
        "2026-09-24T01:00:00Z",
        None,
        None,
    );
    // A stopped holder is dormant.
    db.claim(
        "c1",
        3,
        "ticket",
        "asleep",
        "2026-09-24T01:00:00Z",
        None,
        None,
    );
    db.claim(
        "c2",
        3,
        "ticket",
        "other",
        "2026-09-24T01:00:00Z",
        None,
        None,
    );
    // An ended claim never counts.
    db.claim(
        "d1",
        4,
        "ticket",
        "eng1",
        "2026-09-24T01:00:00Z",
        Some(("2026-09-24T02:00:00Z", "released")),
        None,
    );
    db.claim(
        "d2",
        4,
        "ticket",
        "other",
        "2026-09-24T03:00:00Z",
        None,
        None,
    );
    let data = db.load();
    assert!(row(&data, 1).has_flag(Flag::TwoAgents));
    assert_eq!(edge_flags(&row(&data, 2)), Vec::<&str>::new());
    assert!(!row(&data, 3).has_flag(Flag::TwoAgents));
    assert!(!row(&data, 4).has_flag(Flag::TwoAgents));
}

fn edge_flags(row: &HistoryRow) -> Vec<&'static str> {
    row.flags.clone()
}

#[test]
fn two_agents_also_fires_for_one_linked_pr() {
    let db = db();
    db.item(1, "ticket", "open", SYNCED);
    db.item(9, "pr", "open", SYNCED);
    db.link(9, 1);
    db.claim("a", 9, "pr", "eng1", "2026-09-24T01:00:00Z", None, None);
    db.claim("b", 9, "pr", "other", "2026-09-24T01:00:00Z", None, None);
    assert!(row(&db.load(), 1).has_flag(Flag::TwoAgents));
}

#[test]
fn open_after_merge_and_no_live_holder() {
    let db = db();
    db.item(1, "ticket", "open", SYNCED);
    db.item(2, "ticket", "closed", SYNCED);
    db.item(3, "ticket", "open", SYNCED);
    db.item(4, "ticket", "open", SYNCED);
    db.item(9, "pr", "merged", SYNCED);
    db.item(10, "pr", "merged", SYNCED);
    db.link(9, 1);
    db.link(10, 2);
    // Retired, then taken and retired: nobody holds 3.
    db.claim(
        "a",
        3,
        "ticket",
        "gone",
        "2026-09-24T01:00:00Z",
        Some(("2026-09-24T02:00:00Z", "taken")),
        None,
    );
    db.claim(
        "b",
        3,
        "ticket",
        "gone2",
        "2026-09-24T02:00:00Z",
        Some(("2026-09-24T04:00:00Z", "retired")),
        None,
    );
    // A live holder remains on 4.
    db.claim(
        "c",
        4,
        "ticket",
        "gone",
        "2026-09-24T01:00:00Z",
        Some(("2026-09-24T02:00:00Z", "retired")),
        None,
    );
    db.claim("d", 4, "ticket", "eng1", "2026-09-24T03:00:00Z", None, None);
    let data = db.load();
    assert_eq!(row(&data, 1).flags, vec!["open_after_merge"]);
    // Closed: done, no flag.
    assert!(row(&data, 2).flags.is_empty());
    assert_eq!(row(&data, 3).flags, vec!["no_live_holder"]);
    assert!(row(&data, 4).flags.is_empty());
}

#[test]
fn worktree_left_uses_the_latest_event_per_path_and_ignores_absent() {
    let db = db();
    for n in 1..=3 {
        db.item(n, "ticket", "open", SYNCED);
    }
    db.claim(
        "a",
        1,
        "ticket",
        "eng1",
        "2026-09-24T01:00:00Z",
        None,
        Some("/wt/one"),
    );
    db.claim(
        "b",
        2,
        "ticket",
        "eng2",
        "2026-09-24T01:00:00Z",
        None,
        Some("/wt/two"),
    );
    db.claim(
        "c",
        3,
        "ticket",
        "other",
        "2026-09-24T01:00:00Z",
        None,
        Some("/wt/three"),
    );
    db.event(
        "2026-09-24T02:00:00Z",
        "worktree.left",
        Some("eng1"),
        None,
        None,
        json!({"path": "/wt/one", "reason": "kept: server running", "retryable": true}),
    );
    db.event(
        "2026-09-24T02:00:00Z",
        "worktree.left",
        Some("eng2"),
        None,
        None,
        json!({"path": "/wt/two", "reason": "process 4121 (node) runs in it", "retryable": true}),
    );
    db.event(
        "2026-09-24T03:00:00Z",
        "worktree.removed",
        Some("eng2"),
        None,
        None,
        json!({"path": "/wt/two", "reason": "PR #9 merged"}),
    );
    db.event(
        "2026-09-24T02:00:00Z",
        "worktree.left",
        Some("other"),
        None,
        None,
        json!({"path": "/wt/three", "reason": "absent"}),
    );
    let data = db.load();
    assert_eq!(row(&data, 1).flags, vec!["worktree_left"]);
    assert!(row(&data, 2).flags.is_empty());
    assert!(row(&data, 3).flags.is_empty());
}

#[test]
fn stale_means_failed_never_fetched_or_old_while_held() {
    let db = db();
    let old = Some("2026-09-24T10:00:00Z");
    db.item(1, "ticket", "open", old); // held and 2h old
    db.item(2, "ticket", "open", old); // not held: sm stopped tracking it
    db.item(3, "ticket", "closed", old); // closed items are never re-polled
    db.item(4, "ticket", "open", None); // never fetched
    db.item(5, "ticket", "open", SYNCED);
    db.conn
        .execute(
            "UPDATE work_items SET sync_error = 'not found' WHERE number = 5",
            [],
        )
        .unwrap();
    db.claim("a", 1, "ticket", "eng1", "2026-09-24T01:00:00Z", None, None);
    db.claim(
        "b",
        3,
        "ticket",
        "eng1",
        "2026-09-24T01:00:00Z",
        Some(("2026-09-24T02:00:00Z", "closed")),
        None,
    );
    let data = db.load();
    assert!(row(&data, 1).has_flag(Flag::Stale));
    assert!(!row(&data, 2).has_flag(Flag::Stale));
    assert!(!row(&data, 3).has_flag(Flag::Stale));
    assert!(row(&data, 4).has_flag(Flag::Stale));
    assert!(row(&data, 5).has_flag(Flag::Stale));
    assert_eq!(row(&data, 5).sync_error.as_deref(), Some("not found"));
}

#[test]
fn filters_by_agent_repo_and_open() {
    let db = db();
    db.item(1, "ticket", "open", SYNCED);
    db.item(2, "ticket", "closed", SYNCED);
    db.item(3, "ticket", "open", SYNCED);
    db.item(4, "ticket", "open", SYNCED);
    db.item(9, "pr", "open", SYNCED);
    db.item(10, "pr", "open", SYNCED);
    db.link(9, 3);
    db.link(10, 4);
    db.claim("a", 1, "ticket", "eng1", "2026-09-24T01:00:00Z", None, None);
    db.claim(
        "b",
        2,
        "ticket",
        "gone",
        "2026-09-24T01:00:00Z",
        Some(("2026-09-24T02:00:00Z", "closed")),
        None,
    );
    db.review("r", 9, "eng1", "2026-09-24T03:00:00Z", None);
    db.doc("d", Some(10), "eng1", "2026-09-24T04:00:00Z");
    let data = db.load();
    let run = |q: HistoryQuery| {
        let mut n = numbers(&data.list(&sessions(), &q, now()));
        n.sort_unstable();
        n
    };
    let by = |ids: &[&str]| HistoryQuery {
        agent_ids: Some(ids.iter().map(|s| (*s).to_owned()).collect()),
        ..query()
    };
    // A claim, a Codex review request, and a doc each count.
    assert_eq!(run(by(&["eng1"])), vec![1, 3, 4]);
    assert_eq!(run(by(&[])), Vec::<i64>::new());
    // A purged agent by its snapshot name.
    assert_eq!(
        data.sessions_named("GONE-snapshot"),
        BTreeSet::from(["gone".to_owned()])
    );
    assert!(data.knows_session("gone"));
    assert!(!data.knows_session("nobody"));
    assert_eq!(run(by(&["gone"])), vec![2]);
    let repo = |r: &str| HistoryQuery {
        repo: Some(r.into()),
        ..query()
    };
    assert_eq!(run(repo("Widgets")), vec![1, 2, 3, 4]);
    assert_eq!(run(repo("acme/widgets")), vec![1, 2, 3, 4]);
    assert_eq!(run(repo("other")), Vec::<i64>::new());
    let open = HistoryQuery {
        open_only: true,
        ..query()
    };
    assert_eq!(run(open), vec![1, 3, 4]);
}

#[test]
fn pages_newest_first_with_an_opaque_cursor() {
    let db = db();
    for (n, at) in [
        (1, "2026-09-24T01:00:00Z"),
        (2, "2026-09-24T03:00:00Z"),
        (3, "2026-09-24T02:00:00Z"),
        (4, "2026-09-24T02:00:00Z"),
    ] {
        db.item(n, "ticket", "open", SYNCED);
        db.claim(&format!("c{n}"), n, "ticket", "eng1", at, None, None);
    }
    // Never touched: no activity, sorts last.
    db.item(5, "ticket", "open", SYNCED);
    let data = db.load();
    let first = data.list(
        &sessions(),
        &HistoryQuery {
            limit: 2,
            ..query()
        },
        now(),
    );
    // Ties on time break on number, descending.
    assert_eq!(numbers(&first), vec![2, 4]);
    let cursor = first.next_before.clone().unwrap();
    let second = data.list(
        &sessions(),
        &HistoryQuery {
            limit: 2,
            before: Cursor::decode(&cursor),
            ..query()
        },
        now(),
    );
    assert_eq!(numbers(&second), vec![3, 1]);
    let third = data.list(
        &sessions(),
        &HistoryQuery {
            limit: 2,
            before: Cursor::decode(second.next_before.as_deref().unwrap()),
            ..query()
        },
        now(),
    );
    assert_eq!(numbers(&third), vec![5]);
    assert_eq!(third.rows[0].last_activity, None);
    assert_eq!(third.next_before, None);
    assert_eq!(Cursor::decode("not a cursor"), None);
    // Limits clamp to 1–200.
    let clamped = data.list(
        &sessions(),
        &HistoryQuery {
            limit: 0,
            ..query()
        },
        now(),
    );
    assert_eq!(clamped.rows.len(), 1);
}

#[test]
fn timeline_merges_events_reviews_docs_and_owner_reviews_in_order() {
    let db = db();
    db.item(1, "ticket", "open", SYNCED);
    db.item(9, "pr", "merged", SYNCED);
    db.link(9, 1);
    db.claim(
        "c1",
        1,
        "ticket",
        "eng1",
        "2026-09-24T01:00:00Z",
        None,
        None,
    );
    db.event(
        "2026-09-24T01:00:00Z",
        "claim.taken",
        Some("eng1"),
        Some(1),
        None,
        json!({"claim_id": "c1", "source": "explicit"}),
    );
    db.event(
        "2026-09-24T02:00:00Z",
        "claim.taken",
        Some("eng1"),
        Some(1),
        Some(9),
        json!({"claim_id": "c2", "source": "explicit"}),
    );
    db.event(
        "2026-09-24T02:00:01Z",
        "link.added",
        None,
        Some(1),
        Some(9),
        json!({"pr_number": 9, "ticket_number": 1, "source": "closing_ref"}),
    );
    db.review(
        "r1",
        9,
        "eng1",
        "2026-09-24T03:00:00Z",
        Some("2026-09-24T03:10:00Z"),
    );
    db.doc("d1", Some(9), "eng1", "2026-09-24T04:00:00Z");
    db.conn
        .execute(
            "INSERT INTO owner_doc_reviews (id, status, doc_id, commit_sha, blob_sha, verdict,
                line_comment_count, file_comment_count, github_review_url, submitted_at)
             VALUES ('rv', 'posted', 'd1', 'c', 'b', 'approve', 0, 0,
                'https://github.com/acme/widgets/pull/9#pullrequestreview-1',
                '2026-09-24T05:00:00Z')",
            [],
        )
        .unwrap();
    db.event(
        "2026-09-24T06:00:00Z",
        "github.state_changed",
        None,
        Some(1),
        Some(9),
        json!({"from": "open", "to": "merged"}),
    );
    db.event(
        "2026-09-24T06:01:00Z",
        "claim.nudge",
        Some("eng1"),
        Some(1),
        None,
        json!({"check": "A", "message_id": "m1",
               "text": "[sm claim] PR #9 merged 1m ago. Linked ticket #1 \"Item 1\" is open."}),
    );
    db.event(
        "2026-09-24T07:00:00Z",
        "claim.released",
        Some("eng1"),
        Some(1),
        None,
        json!({"claim_id": "c1", "end_reason": "retired"}),
    );
    // Another ticket's event stays off this page.
    db.event(
        "2026-09-24T07:30:00Z",
        "claim.taken",
        Some("eng2"),
        Some(2),
        None,
        json!({}),
    );
    let data = db.load();
    let timeline = data.timeline(&sessions(), REPO, 1, now()).unwrap();
    assert_eq!(timeline.item.number, 1);
    assert_eq!(timeline.item.flags, vec!["open_after_merge"]);
    let lines: Vec<(&str, Option<&str>, &str)> = timeline
        .events
        .iter()
        .map(|e| (e.kind.as_str(), e.name.as_deref(), e.text.as_str()))
        .collect();
    assert_eq!(
        lines,
        vec![
            ("claim.taken", Some("eng1-live"), "claimed the ticket"),
            ("claim.taken", Some("eng1-live"), "claimed PR #9"),
            (
                "link.added",
                None,
                "PR #9 linked (the PR says it closes the ticket)"
            ),
            (
                "codex_review.requested",
                Some("eng1-live"),
                "requested Codex review on #9"
            ),
            (
                "codex_review.landed",
                Some("eng1-live"),
                "Codex review landed on #9"
            ),
            (
                "doc.published",
                Some("eng1-live"),
                "published Doc d1 (review requested)"
            ),
            ("doc.reviewed", None, "owner reviewed Doc d1: approved"),
            ("github.state_changed", None, "PR #9 merged"),
            (
                "claim.nudge",
                Some("eng1-live"),
                "was told: [sm claim] PR #9 merged 1m ago. Linked ticket #1 \"Item 1\" is open."
            ),
            (
                "claim.released",
                Some("eng1-live"),
                "retired; claim on the ticket ended"
            ),
        ]
    );
    assert_eq!(
        timeline.events[5].link.as_deref(),
        Some("/docs/widgets/docs/d1.md?version=cccccccccccc")
    );
    assert_eq!(
        timeline.events[1].link.as_deref(),
        Some("https://github.com/acme/widgets/pull/9")
    );
    assert_eq!(
        timeline.item.last_activity.as_deref(),
        Some("2026-09-24T07:00:00Z")
    );

    // The PR's own page: its events, and it names its ticket.
    let pr = data.timeline(&sessions(), REPO, 9, now()).unwrap();
    assert_eq!(pr.item.linked_tickets, vec![1]);
    assert!(pr
        .events
        .iter()
        .any(|e| e.kind == "github.state_changed" && e.text == "The PR merged"));
    assert!(pr.events.iter().all(|e| e.text != "claimed the ticket"));
    assert!(data.timeline(&sessions(), REPO, 99, now()).is_none());
    assert_eq!(data.repos_for("WIDGETS", 1), vec![REPO.to_owned()]);
}

#[test]
fn claims_without_events_still_show_on_the_timeline() {
    let db = db();
    db.item(1, "ticket", "closed", SYNCED);
    // Backfilled: the claim row exists, no claim.taken or claim.released.
    db.claim(
        "b1",
        1,
        "ticket",
        "gone",
        "2026-09-24T01:00:00Z",
        Some(("2026-09-24T02:00:00Z", "closed")),
        None,
    );
    db.conn
        .execute(
            "UPDATE work_claims SET source = 'backfill' WHERE id = 'b1'",
            [],
        )
        .unwrap();
    // Logged: its events are used and not doubled.
    db.claim(
        "e1",
        1,
        "ticket",
        "eng1",
        "2026-09-24T03:00:00Z",
        None,
        None,
    );
    db.event(
        "2026-09-24T03:00:00Z",
        "claim.taken",
        Some("eng1"),
        Some(1),
        None,
        json!({"claim_id": "e1", "source": "explicit"}),
    );
    let timeline = db.load().timeline(&sessions(), REPO, 1, now()).unwrap();
    let lines: Vec<(&str, Option<&str>, &str)> = timeline
        .events
        .iter()
        .map(|e| (e.at.as_str(), e.name.as_deref(), e.text.as_str()))
        .collect();
    assert_eq!(
        lines,
        vec![
            (
                "2026-09-24T01:00:00Z",
                Some("gone-snapshot"),
                "claimed the ticket (from history)"
            ),
            (
                "2026-09-24T02:00:00Z",
                Some("gone-snapshot"),
                "claim on the ticket ended: closed"
            ),
            (
                "2026-09-24T03:00:00Z",
                Some("eng1-live"),
                "claimed the ticket"
            ),
        ]
    );
}

#[test]
fn missing_db_reads_empty() {
    let data = HistoryData::load(Path::new("/nonexistent/sm/message_queue.db")).unwrap();
    assert!(data.list(&sessions(), &query(), now()).rows.is_empty());
}
