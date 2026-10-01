use std::sync::Mutex;

use super::model::*;
use super::sync::*;
use super::*;
use crate::work_claims::SessionInfo;

const REPO: &str = "acme/widgets";
const OTHER: &str = "acme/gadgets";

fn k(number: i64) -> Key {
    (REPO.to_owned(), number)
}

fn ko(number: i64) -> Key {
    (OTHER.to_owned(), number)
}

fn now() -> OffsetDateTime {
    parse_ts("2026-09-29T12:00:00Z").unwrap()
}

// ---------------------------------------------------------------------------
// A fake GitHub.

#[derive(Debug, Clone, Default)]
struct FakeIssue {
    title: String,
    open: bool,
    state_reason: Option<String>,
    blocked_by: Vec<Key>,
    parent: Option<Key>,
    prs: Vec<PrRef>,
}

#[derive(Default)]
struct FakeGitHub {
    issues: Mutex<BTreeMap<Key, FakeIssue>>,
    down: Mutex<BTreeSet<String>>,
    rate_remaining: Mutex<Option<i64>>,
    /// Nodes per connection page; GitHub's is 50.
    connection_page: Mutex<usize>,
    writes: Mutex<Vec<LinkMutation>>,
    connection_calls: Mutex<usize>,
}

fn id_of(key: &Key) -> String {
    format!("{}#{}", key.0, key.1)
}

fn key_of(id: &str) -> Key {
    let (repo, number) = id.rsplit_once('#').unwrap();
    (repo.to_owned(), number.parse().unwrap())
}

impl FakeGitHub {
    fn new() -> Self {
        let github = Self::default();
        *github.connection_page.lock().unwrap() = CONNECTION_PAGE;
        github
    }

    fn open(&self, key: Key) {
        self.issues.lock().unwrap().insert(
            key.clone(),
            FakeIssue {
                title: format!("Ticket {}", key.1),
                open: true,
                ..FakeIssue::default()
            },
        );
    }

    fn with(&self, key: &Key, change: impl FnOnce(&mut FakeIssue)) {
        change(self.issues.lock().unwrap().get_mut(key).unwrap());
    }

    fn after(&self, waiter: &Key, blocker: &Key) {
        self.with(waiter, |issue| issue.blocked_by.push(blocker.clone()));
    }

    fn under(&self, child: &Key, parent: &Key) {
        self.with(child, |issue| issue.parent = Some(parent.clone()));
    }

    fn close(&self, key: &Key, reason: &str) {
        self.with(key, |issue| {
            issue.open = false;
            issue.state_reason = Some(reason.to_owned());
        });
    }

    fn node(&self, key: &Key) -> RefNode {
        let issues = self.issues.lock().unwrap();
        let issue = &issues[key];
        RefNode {
            repo: key.0.clone(),
            number: key.1,
            title: issue.title.clone(),
            url: format!("https://github.com/{}/issues/{}", key.0, key.1),
            state: if issue.open { "open" } else { "closed" }.to_owned(),
            state_reason: issue.state_reason.clone(),
            closed_at: (!issue.open).then(|| "2026-09-29T11:00:00Z".to_owned()),
        }
    }

    fn children(&self, key: &Key) -> Vec<Key> {
        self.issues
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, issue)| issue.parent.as_ref() == Some(key))
            .map(|(child, _)| child.clone())
            .collect()
    }

    fn page(&self, keys: &[Key], offset: usize) -> (Vec<RefNode>, Option<String>) {
        let size = *self.connection_page.lock().unwrap();
        let nodes: Vec<RefNode> = keys
            .iter()
            .skip(offset)
            .take(size)
            .map(|key| self.node(key))
            .collect();
        let next = (offset + size < keys.len()).then(|| (offset + size).to_string());
        (nodes, next)
    }
}

impl BoardSource for FakeGitHub {
    fn issues_page(&self, repo: &str, _cursor: Option<&str>) -> Result<IssuesPage, String> {
        if self.down.lock().unwrap().contains(repo) {
            return Err("gh api graphql failed: timed out".into());
        }
        let open: Vec<(Key, FakeIssue)> = self
            .issues
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, issue)| key.0 == repo && issue.open)
            .map(|(key, issue)| (key.clone(), issue.clone()))
            .collect();
        let nodes = open
            .into_iter()
            .map(|(key, issue)| {
                let (blocked_by, blocked_by_more) = self.page(&issue.blocked_by, 0);
                let (sub_issues, sub_issues_more) = self.page(&self.children(&key), 0);
                IssueNode {
                    number: key.1,
                    title: issue.title.clone(),
                    url: format!("https://github.com/{}/issues/{}", key.0, key.1),
                    updated_at: None,
                    state_reason: issue.state_reason.clone(),
                    parent: issue.parent.as_ref().map(|parent| self.node(parent)),
                    blocked_by,
                    blocked_by_more,
                    sub_issues,
                    sub_issues_more,
                    prs: issue.prs.clone(),
                }
            })
            .collect();
        Ok(IssuesPage {
            rate_remaining: *self.rate_remaining.lock().unwrap(),
            rate_reset_at: Some("2026-09-29T13:00:00Z".into()),
            nodes,
            next_cursor: None,
        })
    }

    fn connection_page(
        &self,
        repo: &str,
        number: i64,
        connection: IssueConnection,
        cursor: &str,
    ) -> Result<(Vec<RefNode>, Option<String>), String> {
        *self.connection_calls.lock().unwrap() += 1;
        let key = (repo.to_owned(), number);
        let keys = match connection {
            IssueConnection::BlockedBy => self.issues.lock().unwrap()[&key].blocked_by.clone(),
            IssueConnection::SubIssues => self.children(&key),
        };
        Ok(self.page(&keys, cursor.parse().unwrap()))
    }

    fn items(&self, repo: &str, numbers: &[i64]) -> Result<BTreeMap<i64, Option<RefNode>>, String> {
        Ok(numbers
            .iter()
            .map(|number| {
                let key = (repo.to_owned(), *number);
                let exists = self.issues.lock().unwrap().contains_key(&key);
                (*number, exists.then(|| self.node(&key)))
            })
            .collect())
    }

    fn resolve(&self, issues: &[Key]) -> Result<Vec<Option<ResolvedIssue>>, String> {
        Ok(issues
            .iter()
            .map(|key| {
                let issue = self.issues.lock().unwrap().get(key).cloned()?;
                Some(ResolvedIssue {
                    id: id_of(key),
                    node: self.node(key),
                    parent: issue.parent.clone(),
                    blocked_by: issue.blocked_by.clone(),
                })
            })
            .collect())
    }

    fn write_link(&self, mutation: &LinkMutation) -> Result<(), WriteError> {
        self.writes.lock().unwrap().push(mutation.clone());
        match mutation {
            LinkMutation::AddBlockedBy {
                issue_id,
                blocking_id,
            } => self.after(&key_of(issue_id), &key_of(blocking_id)),
            LinkMutation::RemoveBlockedBy {
                issue_id,
                blocking_id,
            } => self.with(&key_of(issue_id), |issue| {
                issue.blocked_by.retain(|key| key != &key_of(blocking_id))
            }),
            LinkMutation::AddSubIssue {
                parent_id,
                child_id,
            } => self.under(&key_of(child_id), &key_of(parent_id)),
            LinkMutation::RemoveSubIssue { child_id, .. } => {
                self.with(&key_of(child_id), |issue| issue.parent = None)
            }
        }
        Ok(())
    }
}

fn temp_store() -> (BoardStore, PathBuf) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "sm-board-{}-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    let store = BoardStore::new(dir.join("message_queue.db"));
    store.ensure_schema().unwrap();
    (store, dir)
}

fn outside() -> Outside {
    Outside {
        sessions: SessionDirectory::new([SessionInfo {
            id: "eng1".into(),
            name: "sm-1653-engineer".into(),
            parent_session_id: None,
            state: HolderState::Working,
            stopped_at: None,
        }]),
        waiting: Vec::new(),
        owner_name: "Owner".into(),
        config_repos: vec![REPO.to_owned()],
    }
}

/// The context-handoff lane: 1654, 1656, 1657 after 1653; all four under
/// 1651.
fn handoff(github: &FakeGitHub) {
    for number in [1651, 1653, 1654, 1656, 1657] {
        github.open(k(number));
    }
    for number in [1654, 1656, 1657] {
        github.after(&k(number), &k(1653));
    }
    for number in [1653, 1654, 1656, 1657] {
        github.under(&k(number), &k(1651));
    }
}

fn lane_of(board: &Board, goal: &Key) -> LaneView {
    board
        .lanes
        .iter()
        .find(|lane| &lane.lane.goal == goal)
        .cloned()
        .unwrap()
}

fn state(board: &Board, key: &Key) -> TicketState {
    board.facts[key].state
}

fn add_lane(store: &BoardStore, github: &FakeGitHub, goal: &Key) -> i64 {
    check_goal(store, github, goal, now()).unwrap().unwrap();
    let id = store
        .add_lane(goal, "owner", "Owner", now())
        .unwrap()
        .unwrap();
    run_pass(store, github, &outside(), now()).unwrap();
    id
}

// ---------------------------------------------------------------------------
// C: reading GitHub.

#[test]
fn sync_upserts_open_issues_and_edges() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    handoff(&github);
    github.with(&k(1653), |issue| {
        issue.prs.push(PrRef {
            repo: REPO.into(),
            number: 1700,
            state: "OPEN".into(),
            url: "u".into(),
        })
    });
    run_pass(&store, &github, &outside(), now()).unwrap();
    let input = store.input(&outside()).unwrap();
    assert_eq!(input.items.len(), 5);
    assert!(input.items.values().all(Item::is_open));
    let edges: BTreeSet<(Key, Key, EdgeKind)> = input
        .edges
        .iter()
        .map(|edge| (edge.waiter.clone(), edge.blocker.clone(), edge.kind))
        .collect();
    assert_eq!(edges.len(), 7);
    assert!(edges.contains(&(k(1654), k(1653), EdgeKind::After)));
    assert!(edges.contains(&(k(1651), k(1657), EdgeKind::SubIssue)));
    assert!(input.edges.iter().all(|edge| edge.source == "github"));
    assert_eq!(input.prs[&k(1653)][0].number, 1700);
    let sync = &store.repo_syncs().unwrap()[0];
    assert_eq!(sync.last_ok_at.as_deref(), Some("2026-09-29T12:00:00Z"));
}

#[test]
fn parent_field_becomes_sub_issue_edge() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    // The parent lives in a repo nobody reads.
    github.open(ko(9));
    github.open(k(1));
    github.under(&k(1), &ko(9));
    run_pass(&store, &github, &outside(), now()).unwrap();
    let input = store.input(&outside()).unwrap();
    assert_eq!(
        input.edges,
        vec![Edge {
            waiter: ko(9),
            blocker: k(1),
            kind: EdgeKind::SubIssue,
            source: "github".into()
        }]
    );
    assert!(input.items[&ko(9)].is_open());
}

#[test]
fn sync_marks_missing_issue_closed() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    handoff(&github);
    run_pass(&store, &github, &outside(), now()).unwrap();
    github.close(&k(1653), "NOT_PLANNED");
    github.issues.lock().unwrap().remove(&k(1657));
    run_pass(&store, &github, &outside(), now()).unwrap();
    let input = store.input(&outside()).unwrap();
    let closed = &input.items[&k(1653)];
    assert_eq!(closed.state, "closed");
    assert_eq!(closed.state_reason.as_deref(), Some("NOT_PLANNED"));
    assert_eq!(closed.closed_at.as_deref(), Some("2026-09-29T11:00:00Z"));
    let missing = &input.items[&k(1657)];
    assert_eq!(missing.state, "closed");
    assert_eq!(missing.state_reason.as_deref(), Some("missing"));
    // A closed ticket's own links are dropped.
    assert!(!input.edges.iter().any(|edge| edge.waiter == k(1657)));
}

#[test]
fn sync_pages_connections_over_50() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    *github.connection_page.lock().unwrap() = 2;
    github.open(k(100));
    for number in 1..=5 {
        github.open(k(number));
        github.after(&k(100), &k(number));
        github.under(&k(number), &k(100));
    }
    run_pass(&store, &github, &outside(), now()).unwrap();
    let input = store.input(&outside()).unwrap();
    let count = |kind| {
        input
            .edges
            .iter()
            .filter(|edge| edge.waiter == k(100) && edge.kind == kind)
            .count()
    };
    assert_eq!((count(EdgeKind::After), count(EdgeKind::SubIssue)), (5, 5));
    assert!(*github.connection_calls.lock().unwrap() >= 4);
}

#[test]
fn sync_failure_keeps_rows_and_marks_stale() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    handoff(&github);
    let lane = add_lane(&store, &github, &k(1651));
    github.close(&k(1653), "COMPLETED");
    github.down.lock().unwrap().insert(REPO.into());
    let later = now() + time::Duration::minutes(1);
    let result = run_pass(&store, &github, &outside(), later).unwrap();
    let board = &result.board;
    assert!(
        board.facts[&k(1653)].item.is_open(),
        "rows stay as they were"
    );
    assert!(board.lane(lane).unwrap().stale);
    assert!(board.facts[&k(1653)].warnings.contains(&WARN_STALE));
    assert!(store.repo_syncs().unwrap()[0].stale());
    assert!(store
        .events(50)
        .unwrap()
        .iter()
        .any(|event| event.kind == "sync_failed"));
    assert!(board
        .facts
        .values()
        .all(|facts| facts.state != TicketState::Ready));
    // The next good read clears it.
    github.down.lock().unwrap().clear();
    let result = run_pass(
        &store,
        &github,
        &outside(),
        later + time::Duration::minutes(1),
    )
    .unwrap();
    assert!(!result.board.lane(lane).unwrap().stale);
    assert_eq!(state(&result.board, &k(1654)), TicketState::Ready);
}

#[test]
fn stale_repo_ticket_not_ready() {
    let mut input = handoff_input();
    input.items.insert(k(1653), item(&k(1653), false));
    input.holders.clear();
    input.stale.insert(REPO.into());
    let board = compute(&input, now());
    assert_eq!(state(&board, &k(1654)), TicketState::Blocked);
    assert!(board.facts[&k(1654)].warnings.contains(&WARN_STALE));
}

#[test]
fn sync_skips_while_rate_limited() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    handoff(&github);
    *github.rate_remaining.lock().unwrap() = Some(150);
    run_pass(&store, &github, &outside(), now()).unwrap();
    assert!(store.input(&outside()).unwrap().items.is_empty());
    assert!(store.rate_limited(now()).unwrap());
    *github.rate_remaining.lock().unwrap() = Some(4000);
    run_pass(&store, &github, &outside(), now()).unwrap();
    assert!(
        store.input(&outside()).unwrap().items.is_empty(),
        "skipped until resetAt"
    );
    run_pass(
        &store,
        &github,
        &outside(),
        now() + time::Duration::hours(2),
    )
    .unwrap();
    assert_eq!(store.input(&outside()).unwrap().items.len(), 5);
}

#[test]
fn nested_cross_repo_blocker_read_same_pass() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    github.open(k(1));
    github.open(ko(2));
    github.open(ko(3));
    github.after(&k(1), &ko(2));
    github.after(&ko(2), &ko(3));
    add_lane(&store, &github, &k(1));
    let input = store.input(&outside()).unwrap();
    // gadgets#2 is in the lane, so its repo was read and its own blocker
    // came with it.
    assert!(input
        .edges
        .iter()
        .any(|edge| edge.waiter == ko(2) && edge.blocker == ko(3)));
    let (board, _) = store.board(&outside(), now()).unwrap();
    assert!(lane_of(&board, &k(1)).contains(&ko(3)));
    assert_eq!(state(&board, &ko(3)), TicketState::Ready);
}

// ---------------------------------------------------------------------------
// C5 and E: writing links.

fn link(ticket: Key, target: Key, kind: EdgeKind, remove: bool) -> LinkRequest {
    LinkRequest {
        ticket,
        target,
        kind,
        remove,
        actor: "sm:eng1".into(),
        actor_name: "sm-1653-engineer".into(),
    }
}

#[test]
fn link_add_writes_mutation_and_edge() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    github.open(k(1653));
    github.open(k(1654));
    let request = link(k(1654), k(1653), EdgeKind::After, false);
    let outcome = write_link(&store, &github, &request, now()).unwrap();
    assert_eq!(outcome, Ok(LinkOutcome::Recorded));
    assert_eq!(
        github.writes.lock().unwrap().as_slice(),
        &[LinkMutation::AddBlockedBy {
            issue_id: id_of(&k(1654)),
            blocking_id: id_of(&k(1653))
        }]
    );
    let input = store.input(&outside()).unwrap();
    assert_eq!(input.edges.len(), 1);
    assert_eq!(input.edges[0].source, "sm:eng1");
    let event = &store.events(5).unwrap()[0];
    assert_eq!(event.kind, "link_added");
    assert_eq!(
        change_text(event, REPO).unwrap(),
        "#1654 now starts after #1653 (sm-1653-engineer)"
    );
    assert_eq!(
        link_message(&request, LinkOutcome::Recorded),
        "Recorded: #1654 starts after #1653."
    );
    // The next read keeps sm as the edge's source.
    run_pass(&store, &github, &outside(), now()).unwrap();
    assert_eq!(store.input(&outside()).unwrap().edges[0].source, "sm:eng1");

    let under = link(k(1654), k(1653), EdgeKind::SubIssue, false);
    assert_eq!(
        write_link(&store, &github, &under, now()).unwrap(),
        Ok(LinkOutcome::Recorded)
    );
    assert!(matches!(
        github.writes.lock().unwrap().last(),
        Some(LinkMutation::AddSubIssue { parent_id, child_id })
            if parent_id == &id_of(&k(1653)) && child_id == &id_of(&k(1654))
    ));
    assert_eq!(
        link_message(&under, LinkOutcome::Recorded),
        "Recorded: #1654 is under #1653."
    );
}

#[test]
fn link_add_existing_is_already() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    github.open(k(1653));
    github.open(k(1654));
    github.after(&k(1654), &k(1653));
    let request = link(k(1654), k(1653), EdgeKind::After, false);
    assert_eq!(
        write_link(&store, &github, &request, now()).unwrap(),
        Ok(LinkOutcome::Already)
    );
    assert!(github.writes.lock().unwrap().is_empty());
    assert_eq!(
        link_message(&request, LinkOutcome::Already),
        "Already recorded: #1654 starts after #1653."
    );
    let remove = link(k(1654), k(1653), EdgeKind::After, true);
    assert_eq!(
        write_link(&store, &github, &remove, now()).unwrap(),
        Ok(LinkOutcome::Removed)
    );
    assert!(store.input(&outside()).unwrap().edges.is_empty());
    assert_eq!(
        link_message(&remove, LinkOutcome::Removed),
        "Removed: #1654 no longer starts after #1653."
    );
}

#[test]
fn link_remove_absent_is_absent() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    github.open(k(1653));
    github.open(ko(7));
    let request = link(k(1653), ko(7), EdgeKind::After, true);
    assert_eq!(
        write_link(&store, &github, &request, now()).unwrap(),
        Ok(LinkOutcome::Absent)
    );
    assert!(github.writes.lock().unwrap().is_empty());
    assert_eq!(
        link_message(&request, LinkOutcome::Absent),
        "Not linked: #1653 does not start after gadgets#7."
    );
    let missing = link(k(1653), k(99), EdgeKind::After, false);
    assert_eq!(
        write_link(&store, &github, &missing, now()).unwrap(),
        Err(Refusal::NotFound("no such issue: acme/widgets#99".into()))
    );
}

#[test]
fn under_refuses_to_replace_parent() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    for number in [1, 2, 3] {
        github.open(k(number));
    }
    github.under(&k(3), &k(1));
    let request = link(k(3), k(2), EdgeKind::SubIssue, false);
    assert_eq!(
        write_link(&store, &github, &request, now()).unwrap(),
        Err(Refusal::Unprocessable(
            "#3 already sits under acme/widgets#1; remove that first with sm board under 3 1 --remove"
                .into()
        ))
    );
    assert!(github.writes.lock().unwrap().is_empty());
    assert_eq!(
        write_link(
            &store,
            &github,
            &link(k(3), k(1), EdgeKind::SubIssue, false),
            now()
        )
        .unwrap(),
        Ok(LinkOutcome::Already)
    );
}

#[test]
fn ticket_ref_parsing() {
    assert_eq!(parse_ticket_ref("1654", REPO), Some(k(1654)));
    assert_eq!(parse_ticket_ref("#1654", REPO), Some(k(1654)));
    assert_eq!(parse_ticket_ref("gadgets#7", REPO), Some(ko(7)));
    assert_eq!(parse_ticket_ref("Acme/Gadgets#7", REPO), Some(ko(7)));
    assert_eq!(parse_ticket_ref("acme/gadgets#7", ""), Some(ko(7)));
    assert_eq!(parse_ticket_ref("7", ""), None);
    assert_eq!(parse_ticket_ref("#0", REPO), None);
    assert_eq!(parse_ticket_ref("abc", REPO), None);
    assert_eq!(parse_ticket_ref("a/b/c#1", REPO), None);
}

// ---------------------------------------------------------------------------
// D: the model.

#[test]
fn finished_container_is_close_ready_only_without_active_work() {
    let mut input = ModelInput::default();
    input.read_repos.insert(REPO.to_owned());
    for number in [1, 2] {
        input.items.insert(k(number), item(&k(number), true));
    }
    input.edges.push(edge(&k(1), &k(2), EdgeKind::SubIssue));
    assert_eq!(
        compute(&input, now()).facts[&k(1)].state,
        TicketState::Blocked
    );
    input.items.insert(k(2), item(&k(2), false));
    let board = compute(&input, now());
    assert_eq!(board.facts[&k(1)].state, TicketState::CloseReady);
    assert_eq!(board.facts[&k(1)].sub_issues_closed, 1);
    input
        .holders
        .insert(k(1), vec![holder("agent", HolderState::Working)]);
    assert_eq!(
        compute(&input, now()).facts[&k(1)].state,
        TicketState::InProgress
    );
    input.holders.clear();
    // A finished container that also starts after an open ticket waits for it.
    input.items.insert(k(3), item(&k(3), true));
    input.edges.push(edge(&k(1), &k(3), EdgeKind::After));
    assert_eq!(
        compute(&input, now()).facts[&k(1)].state,
        TicketState::Blocked
    );
    input.items.insert(k(3), item(&k(3), false));
    assert_eq!(
        compute(&input, now()).facts[&k(1)].state,
        TicketState::CloseReady
    );
    input.edges.truncate(1);
    input.edges[0].kind = EdgeKind::After;
    assert_eq!(
        compute(&input, now()).facts[&k(1)].state,
        TicketState::Ready
    );
}

fn item(key: &Key, open: bool) -> Item {
    Item {
        repo: key.0.clone(),
        number: key.1,
        title: format!("Ticket {}", key.1),
        url: String::new(),
        state: if open { "open" } else { "closed" }.into(),
        state_reason: (!open).then(|| "COMPLETED".into()),
        closed_at: (!open).then(|| format!("2026-09-29T10:{:02}:00Z", key.1 % 60)),
    }
}

fn edge(waiter: &Key, blocker: &Key, kind: EdgeKind) -> Edge {
    Edge {
        waiter: waiter.clone(),
        blocker: blocker.clone(),
        kind,
        source: "github".into(),
    }
}

fn lane(id: i64, goal: &Key, rank: i64) -> Lane {
    Lane {
        id,
        goal: goal.clone(),
        rank,
        added_at: "2026-09-27T10:00:00Z".into(),
        added_by: "owner".into(),
        added_by_name: "Owner".into(),
    }
}

fn holder(session: &str, state: HolderState) -> Holder {
    Holder {
        session_id: session.into(),
        name: format!("{session}-agent"),
        state,
    }
}

/// The context-handoff lane as model input, #1653 held by a working agent.
fn handoff_input() -> ModelInput {
    let mut input = ModelInput::default();
    for number in [1651, 1653, 1654, 1656, 1657] {
        input.items.insert(k(number), item(&k(number), true));
    }
    for number in [1654, 1656, 1657] {
        input
            .edges
            .push(edge(&k(number), &k(1653), EdgeKind::After));
    }
    for number in [1653, 1654, 1656, 1657] {
        input
            .edges
            .push(edge(&k(1651), &k(number), EdgeKind::SubIssue));
    }
    input.lanes.push(lane(2, &k(1651), 2));
    input
        .holders
        .insert(k(1653), vec![holder("eng1", HolderState::Working)]);
    input.read_repos.insert(REPO.into());
    input
}

#[test]
fn membership_follows_both_kinds() {
    let mut input = handoff_input();
    input.items.insert(k(1700), item(&k(1700), true));
    input.edges.push(edge(&k(1653), &k(1700), EdgeKind::After));
    let board = compute(&input, now());
    let view = lane_of(&board, &k(1651));
    let members: BTreeSet<i64> = view.rows.iter().map(|row| row.key.1).collect();
    assert_eq!(
        members,
        BTreeSet::from([1651, 1653, 1654, 1656, 1657, 1700])
    );
    assert!(board.other[0].1.is_empty());
}

#[test]
fn membership_stops_at_closed() {
    let mut input = handoff_input();
    input.items.insert(k(1653), item(&k(1653), false));
    input.items.insert(k(1700), item(&k(1700), true));
    input.edges.push(edge(&k(1653), &k(1700), EdgeKind::After));
    let board = compute(&input, now());
    let view = lane_of(&board, &k(1651));
    assert!(view.contains(&k(1653)));
    assert!(!view.contains(&k(1700)), "a closed member is a leaf");
    assert_eq!(board.other[0].1, vec![k(1700)]);
}

#[test]
fn state_rules() {
    struct Case {
        name: &'static str,
        setup: fn(&mut ModelInput),
        expect: TicketState,
    }
    let cases = [
        Case {
            name: "closed is done",
            setup: |input| {
                input.items.insert(k(1654), item(&k(1654), false));
            },
            expect: TicketState::Done,
        },
        Case {
            name: "a waiting record beats a holder",
            setup: |input| {
                input
                    .holders
                    .insert(k(1654), vec![holder("eng2", HolderState::Working)]);
                input.waiting.push(WaitingRecord {
                    kind: WaitingKind::Message,
                    session_id: "eng2".into(),
                    pr: None,
                    text: "ruling?".into(),
                    url: "/messages/msg_00000001".into(),
                    created_at: "2026-09-29T11:00:00Z".into(),
                });
            },
            expect: TicketState::NeedsYou,
        },
        Case {
            name: "a live holder is in progress",
            setup: |input| {
                input
                    .holders
                    .insert(k(1654), vec![holder("eng2", HolderState::Idle)]);
            },
            expect: TicketState::InProgress,
        },
        Case {
            name: "an open PR is in progress",
            setup: |input| {
                input.prs.insert(
                    k(1654),
                    vec![PrRef {
                        repo: REPO.into(),
                        number: 9,
                        state: "OPEN".into(),
                        url: String::new(),
                    }],
                );
            },
            expect: TicketState::InProgress,
        },
        Case {
            name: "all blockers closed is ready",
            setup: |input| {
                input.items.insert(k(1653), item(&k(1653), false));
            },
            expect: TicketState::Ready,
        },
        Case {
            name: "an open blocker is blocked",
            setup: |_| {},
            expect: TicketState::Blocked,
        },
        Case {
            name: "a stale repo is blocked",
            setup: |input| {
                input.items.insert(k(1653), item(&k(1653), false));
                input.stale.insert(REPO.into());
            },
            expect: TicketState::Blocked,
        },
    ];
    for case in cases {
        let mut input = handoff_input();
        (case.setup)(&mut input);
        let board = compute(&input, now());
        assert_eq!(state(&board, &k(1654)), case.expect, "{}", case.name);
    }
}

#[test]
fn retired_holder_is_not_in_progress() {
    let mut input = handoff_input();
    input.items.insert(k(1653), item(&k(1653), false));
    input
        .holders
        .insert(k(1654), vec![holder("gone", HolderState::Retired)]);
    input
        .holders
        .insert(k(1656), vec![holder("sleepy", HolderState::Stopped)]);
    let board = compute(&input, now());
    assert_eq!(state(&board, &k(1654)), TicketState::Ready);
    assert_eq!(board.facts[&k(1654)].holder, None);
    assert_eq!(state(&board, &k(1656)), TicketState::InProgress);
    assert_eq!(board.facts[&k(1656)].warnings, vec![WARN_HOLDER_STOPPED]);
}

#[test]
fn chain_example_1651() {
    let board = compute(&handoff_input(), now());
    let view = lane_of(&board, &k(1651));
    let chain = |number| {
        view.rows
            .iter()
            .find(|row| row.key == k(number))
            .unwrap()
            .chain
    };
    assert_eq!(chain(1651), Some(1));
    assert_eq!(chain(1654), Some(2));
    assert_eq!(chain(1656), Some(2));
    assert_eq!(chain(1657), Some(2));
    assert_eq!(chain(1653), Some(3));
    assert_eq!(view.longest_chain, vec![k(1653), k(1654), k(1651)]);
    assert_eq!(state(&board, &k(1653)), TicketState::InProgress);
    for number in [1651, 1654, 1656, 1657] {
        assert_eq!(state(&board, &k(number)), TicketState::Blocked);
    }

    let mut closed = handoff_input();
    closed.items.insert(k(1653), item(&k(1653), false));
    closed.holders.clear();
    let board = compute(&closed, now());
    for number in [1654, 1656, 1657] {
        assert_eq!(state(&board, &k(number)), TicketState::Ready);
    }
    assert_eq!(state(&board, &k(1651)), TicketState::Blocked);
    assert_eq!(state(&board, &k(1653)), TicketState::Done);
}

#[test]
fn row_order() {
    let mut input = handoff_input();
    input.items.insert(k(1653), item(&k(1653), false));
    input.items.insert(k(1650), item(&k(1650), false));
    input
        .edges
        .push(edge(&k(1651), &k(1650), EdgeKind::SubIssue));
    input.holders.clear();
    input
        .holders
        .insert(k(1657), vec![holder("eng1", HolderState::Working)]);
    // 1656 has one more waiter than 1654.
    input.items.insert(k(1660), item(&k(1660), true));
    input.edges.push(edge(&k(1660), &k(1656), EdgeKind::After));
    input
        .edges
        .push(edge(&k(1651), &k(1660), EdgeKind::SubIssue));
    let board = compute(&input, now());
    let order: Vec<i64> = lane_of(&board, &k(1651))
        .rows
        .iter()
        .map(|row| row.key.1)
        .collect();
    // Ready by chain (1656 is 3), in progress, blocked by chain, then done
    // newest first (1653 closed at :33, 1650 at :30).
    assert_eq!(order, vec![1656, 1654, 1657, 1660, 1651, 1653, 1650]);
}

#[test]
fn cycle_flagged_never_ready() {
    let mut input = handoff_input();
    input.items.insert(k(1653), item(&k(1653), false));
    input.holders.clear();
    input.items.insert(k(1660), item(&k(1660), true));
    input
        .edges
        .push(edge(&k(1651), &k(1660), EdgeKind::SubIssue));
    input.edges.push(edge(&k(1660), &k(1657), EdgeKind::After));
    input.edges.push(edge(&k(1657), &k(1660), EdgeKind::After));
    let board = compute(&input, now());
    let view = lane_of(&board, &k(1651));
    assert_eq!(view.cycles, vec![vec![k(1657), k(1660)]]);
    for number in [1657, 1660] {
        assert_eq!(state(&board, &k(number)), TicketState::Blocked);
        assert!(board.facts[&k(number)].warnings.contains(&WARN_CYCLE));
    }
    assert_eq!(state(&board, &k(1654)), TicketState::Ready);
    let event = Event {
        kind: "cycle_found".into(),
        detail: Some("acme/widgets#1657 acme/widgets#1660".into()),
        ..Event::default()
    };
    assert_eq!(
        change_text(&event, REPO).unwrap(),
        "#1657 and #1660 wait on each other"
    );
}

#[test]
fn not_planned_blocker_counts_closed() {
    let mut input = handoff_input();
    let mut dropped = item(&k(1653), false);
    dropped.state_reason = Some("NOT_PLANNED".into());
    input.items.insert(k(1653), dropped);
    input.holders.clear();
    let board = compute(&input, now());
    assert_eq!(state(&board, &k(1654)), TicketState::Ready);
    let json = board_json(&board, &input, &context(&[], &[], &Unseen::default()));
    let done = json["lanes"][0]["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ticket| ticket["number"] == 1653)
        .unwrap()
        .clone();
    assert_eq!(done["done_reason"], "not_planned");
}

#[test]
fn merged_not_closed_warning() {
    let mut input = ModelInput::default();
    let goal = ko(1775);
    let epic = ko(1774);
    input.items.insert(goal.clone(), item(&goal, true));
    input.items.insert(epic.clone(), item(&epic, true));
    input.edges.push(edge(&goal, &epic, EdgeKind::After));
    input.prs.insert(
        epic.clone(),
        vec![PrRef {
            repo: OTHER.into(),
            number: 1800,
            state: "MERGED".into(),
            url: String::new(),
        }],
    );
    input.lanes.push(lane(1, &goal, 1));
    let board = compute(&input, now());
    assert_eq!(state(&board, &epic), TicketState::Ready);
    assert_eq!(board.facts[&epic].warnings, vec![WARN_MERGED_NOT_CLOSED]);
}

fn review(session: &str, pr: Option<i64>) -> WaitingRecord {
    WaitingRecord {
        kind: WaitingKind::Review,
        session_id: session.into(),
        pr: pr.map(|pr| (OTHER.to_owned(), pr)),
        text: pr
            .map(|pr| format!("PR #{pr} waits for your review"))
            .unwrap_or_else(|| "Memo waits for your review".into()),
        url: "/docs/x".into(),
        created_at: "2026-09-28T22:00:00Z".into(),
    }
}

/// Figure 3: iteration 7 on 28 September, 22:40, in `acme/gadgets`.
fn iteration7() -> ModelInput {
    let mut input = ModelInput::default();
    let open = [
        1813, 1790, 1763, 1805, 1766, 1774, 1775, 1807, 1772, 1815, 1747, 1818,
    ];
    let done = [1787, 1791];
    for number in open {
        input.items.insert(ko(number), item(&ko(number), true));
    }
    for number in done {
        input.items.insert(ko(number), item(&ko(number), false));
    }
    let after = [
        (1790, 1813),
        (1763, 1813),
        (1805, 1807),
        (1805, 1790),
        (1805, 1763),
        (1766, 1763),
        (1774, 1790),
        (1774, 1805),
        (1774, 1763),
        (1774, 1772),
        (1774, 1815),
        (1775, 1774),
        (1790, 1787),
    ];
    for (waiter, blocker) in after {
        input
            .edges
            .push(edge(&ko(waiter), &ko(blocker), EdgeKind::After));
    }
    for number in open.iter().chain(&done).filter(|n| **n != 1775) {
        input
            .edges
            .push(edge(&ko(1775), &ko(*number), EdgeKind::SubIssue));
    }
    input.lanes.push(lane(1, &ko(1775), 1));
    for (number, session, state) in [
        (1813, "e1813", HolderState::Working),
        (1790, "e1790", HolderState::Working),
        (1807, "e1807", HolderState::Working),
        (1772, "e1772", HolderState::Idle),
        (1815, "e1815", HolderState::Working),
    ] {
        input
            .holders
            .insert(ko(number), vec![holder(session, state)]);
    }
    input.waiting.push(review("e1813", Some(1814)));
    input.read_repos.insert(OTHER.into());
    input
}

#[test]
fn iteration7_stage_d_example() {
    let board = compute(&iteration7(), now());
    let view = lane_of(&board, &ko(1775));
    assert_eq!(state(&board, &ko(1813)), TicketState::NeedsYou);
    assert_eq!(state(&board, &ko(1790)), TicketState::InProgress);
    assert!(board.facts[&ko(1790)]
        .warnings
        .contains(&WARN_WORKING_WHILE_BLOCKED));
    for number in [1747, 1818] {
        assert_eq!(state(&board, &ko(number)), TicketState::Ready);
    }
    let chain = |number| {
        view.rows
            .iter()
            .find(|row| row.key == ko(number))
            .unwrap()
            .chain
            .unwrap()
    };
    for (number, expected) in [
        (1775, 1),
        (1774, 2),
        (1766, 2),
        (1805, 3),
        (1763, 4),
        (1790, 4),
        (1807, 4),
        (1813, 5),
    ] {
        assert_eq!(chain(number), expected, "chain of #{number}");
    }
    assert_eq!(
        view.longest_chain,
        vec![ko(1813), ko(1763), ko(1805), ko(1774), ko(1775)]
    );
    assert_eq!(view.count(&board, TicketState::NeedsYou), 1);
    assert_eq!(view.count(&board, TicketState::Ready), 2);
    assert_eq!(view.count(&board, TicketState::InProgress), 4);
    assert_eq!(view.count(&board, TicketState::Blocked), 5);
    assert_eq!(
        board.facts[&ko(1813)].needs_you.as_ref().unwrap().text,
        "PR #1814 waits for your review"
    );
    let first: Vec<i64> = view.rows.iter().take(3).map(|row| row.key.1).collect();
    assert_eq!(first, vec![1813, 1747, 1818]);
}

#[test]
fn two_lanes_min_rank() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    handoff(&github);
    github.open(k(1665));
    github.open(k(1660));
    github.after(&k(1665), &k(1660));
    github.after(&k(1657), &k(1660));
    add_lane(&store, &github, &k(1665));
    add_lane(&store, &github, &k(1651));
    let (board, _) = store.board(&outside(), now()).unwrap();
    let first = lane_of(&board, &k(1665));
    let row = first.rows.iter().find(|row| row.key == k(1660)).unwrap();
    assert_eq!(row.also_in, vec![(2, 2)]);
    let rank_of = |number: i64| -> i64 {
        let conn = store.open_read().unwrap().unwrap();
        conn.query_row(
            "SELECT rank FROM board_ticket_ranks WHERE repo = ?1 AND number = ?2",
            params![REPO, number],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(rank_of(1660), 1);
    assert_eq!(rank_of(1654), 2);
    // Reordering applies to the ranks at once.
    let ids: Vec<i64> = store.active_lanes().unwrap().iter().map(|l| l.id).collect();
    store
        .reorder(&[ids[1], ids[0]], "Owner", now())
        .unwrap()
        .unwrap();
    assert_eq!(rank_of(1660), 1);
    assert_eq!(rank_of(1665), 2);
    assert_eq!(rank_of(1654), 1);
}

#[test]
fn cross_repo_blocker_member() {
    let mut input = ModelInput::default();
    input.items.insert(ko(1830), item(&ko(1830), true));
    input.items.insert(k(1654), item(&k(1654), true));
    input.edges.push(edge(&ko(1830), &k(1654), EdgeKind::After));
    input.lanes.push(lane(1, &ko(1830), 1));
    let board = compute(&input, now());
    let view = lane_of(&board, &ko(1830));
    assert!(view.contains(&k(1654)));
    assert_eq!(state(&board, &k(1654)), TicketState::Ready);
    assert_eq!(state(&board, &ko(1830)), TicketState::Blocked);
    assert_eq!(model::short_ref(&k(1654), OTHER), "widgets#1654");
}

#[test]
fn needs_you_beats_in_progress() {
    let mut input = iteration7();
    input.prs.insert(
        ko(1813),
        vec![PrRef {
            repo: OTHER.into(),
            number: 1814,
            state: "OPEN".into(),
            url: String::new(),
        }],
    );
    let board = compute(&input, now());
    assert_eq!(state(&board, &ko(1813)), TicketState::NeedsYou);
}

#[test]
fn review_request_via_pr_link_is_needs_you() {
    let mut input = iteration7();
    input.waiting = vec![review("someone-else", Some(1814))];
    input
        .pr_tickets
        .insert((OTHER.into(), 1814), BTreeSet::from([1813]));
    let board = compute(&input, now());
    assert_eq!(state(&board, &ko(1813)), TicketState::NeedsYou);
    // Without the link, a publisher that holds nothing makes nobody wait.
    input.pr_tickets.clear();
    let board = compute(&input, now());
    assert_eq!(state(&board, &ko(1813)), TicketState::InProgress);
}

#[test]
fn sub_issues_done_flag() {
    let mut input = handoff_input();
    let board = compute(&input, now());
    assert!(!board.facts[&k(1651)].sub_issues_done);
    for number in [1653, 1654, 1656, 1657] {
        input.items.insert(k(number), item(&k(number), false));
    }
    input.holders.clear();
    let board = compute(&input, now());
    assert!(board.facts[&k(1651)].sub_issues_done);
    assert_eq!(state(&board, &k(1651)), TicketState::CloseReady);
    assert!(!board.facts[&k(1654)].sub_issues_done);
}

// ---------------------------------------------------------------------------
// D4, D5: lanes and the recompute.

#[test]
fn lane_add_bottom_rank() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    handoff(&github);
    github.open(k(1665));
    let first = add_lane(&store, &github, &k(1665));
    let second = add_lane(&store, &github, &k(1651));
    let lanes = store.active_lanes().unwrap();
    assert_eq!(
        lanes.iter().map(|l| (l.id, l.rank)).collect::<Vec<_>>(),
        vec![(first, 1), (second, 2)]
    );
    // The lane's planned members are not new.
    let (board, _) = store.board(&outside(), now()).unwrap();
    assert!(lane_of(&board, &k(1651)).rows.iter().all(|row| !row.new));
    // A ticket linked later is.
    github.open(k(1700));
    github.under(&k(1700), &k(1651));
    let later = now() + time::Duration::minutes(5);
    run_pass(&store, &github, &outside(), later).unwrap();
    let (board, _) = store.board(&outside(), later).unwrap();
    let view = lane_of(&board, &k(1651));
    assert!(view.rows.iter().find(|row| row.key == k(1700)).unwrap().new);
    let joined = store
        .events(20)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "ticket_joined")
        .unwrap();
    assert_eq!(
        change_text(&joined, REPO).unwrap(),
        "#1700 joined, added by GitHub"
    );
    assert_eq!(
        store.add_lane(&k(1651), "owner", "Owner", now()).unwrap(),
        Err(Refusal::Conflict(
            "Lane 2 already has goal acme/widgets#1651".into(),
            Some(second)
        ))
    );
    github.close(&k(1665), "COMPLETED");
    assert_eq!(
        check_goal(&store, &github, &k(1665), now()).unwrap(),
        Err(Refusal::Unprocessable("#1665 is closed".into()))
    );
    assert_eq!(
        check_goal(&store, &github, &k(4242), now()).unwrap(),
        Err(Refusal::NotFound("no such issue: acme/widgets#4242".into()))
    );
}

#[test]
fn goal_closed_ends_lane_and_renumbers() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    handoff(&github);
    github.open(k(1665));
    github.open(k(1662));
    let first = add_lane(&store, &github, &k(1665));
    let second = add_lane(&store, &github, &k(1651));
    let third = add_lane(&store, &github, &k(1662));
    github.close(&k(1665), "COMPLETED");
    let result = run_pass(&store, &github, &outside(), now()).unwrap();
    assert_eq!(result.ended.len(), 1);
    assert_eq!(result.ended[0].0.id, first);
    let lanes = store.active_lanes().unwrap();
    assert_eq!(
        lanes.iter().map(|l| (l.id, l.rank)).collect::<Vec<_>>(),
        vec![(second, 1), (third, 2)]
    );
    let (ended, ended_at) = store.lane(first).unwrap().unwrap();
    assert_eq!(ended.rank, 0);
    assert!(ended_at.is_some());
    // An owner End renumbers too.
    assert!(store.end_lane(second, now()).unwrap());
    assert!(!store.end_lane(second, now()).unwrap());
    assert_eq!(store.active_lanes().unwrap()[0].rank, 1);
}

#[test]
fn order_requires_full_set() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    handoff(&github);
    github.open(k(1665));
    let first = add_lane(&store, &github, &k(1665));
    let second = add_lane(&store, &github, &k(1651));
    assert!(store.reorder(&[first], "Owner", now()).unwrap().is_err());
    assert!(store
        .reorder(&[first, first], "Owner", now())
        .unwrap()
        .is_err());
    store
        .reorder(&[second, first], "Owner", now())
        .unwrap()
        .unwrap();
    assert_eq!(store.active_lanes().unwrap()[0].id, second);
    let moved = store
        .events(10)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "lane_moved")
        .unwrap();
    assert_eq!(change_text(&moved, REPO).unwrap(), "moved to lane 2");
}

#[test]
fn recompute_records_transitions() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    handoff(&github);
    let lane = add_lane(&store, &github, &k(1651));
    github.close(&k(1653), "COMPLETED");
    let result = run_pass(&store, &github, &outside(), now()).unwrap();
    let ready: BTreeSet<i64> = result
        .transitions
        .iter()
        .filter(|t| t.to == Some(TicketState::Ready) && t.lane_id == lane)
        .map(|t| t.ticket.1)
        .collect();
    assert_eq!(ready, BTreeSet::from([1654, 1656, 1657]));
    let kinds: Vec<String> = store
        .events(20)
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert!(kinds.contains(&"ticket_closed".to_owned()));
    assert_eq!(kinds.iter().filter(|k| *k == "became_ready").count(), 3);
    let again = run_pass(&store, &github, &outside(), now()).unwrap();
    assert!(again.transitions.is_empty(), "nothing changed");
}

// ---------------------------------------------------------------------------
// F: JSON and the Board count.

fn context<'a>(events: &'a [Event], repos: &'a [RepoSync], unseen: &'a Unseen) -> JsonContext<'a> {
    JsonContext {
        events,
        repos,
        unseen,
        start_defaults: json!({"provider": "claude"}),
        lane_filter: None,
        now: now(),
        clocks: &NO_CLOCKS,
    }
}

static NO_CLOCKS: BTreeMap<Key, Value> = BTreeMap::new();

#[test]
fn board_json_shape() {
    let input = iteration7();
    let board = compute(&input, now());
    let unseen = Unseen {
        count: 1,
        lane_ids: BTreeSet::from([1]),
    };
    let value = board_json(&board, &input, &context(&[], &[], &unseen));
    assert_eq!(value["unseen"]["count"], 1);
    let lane = &value["lanes"][0];
    assert_eq!(lane["rank"], 1);
    assert_eq!(lane["goal"]["number"], 1775);
    assert_eq!(lane["counts"]["blocked"], 5);
    assert_eq!(lane["unseen"], true);
    assert_eq!(lane["longest_chain"][0]["number"], 1813);
    let first = &lane["tickets"][0];
    assert_eq!(first["state"], "needs_you");
    assert_eq!(first["needs_you"]["kind"], "review");
    assert_eq!(first["holder"]["state"], "working");
    assert_eq!(first["on_longest_chain"], true);
    assert_eq!(first["chain"], 5);
    let rebuild = lane["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ticket| ticket["number"] == 1790)
        .unwrap();
    assert_eq!(rebuild["waits_on"][0]["number"], 1787);
    assert_eq!(rebuild["waits_on"][0]["state"], "done");
    assert_eq!(rebuild["warnings"][0], "working_while_blocked");
    assert_eq!(value["start_defaults"]["provider"], "claude");
}

#[test]
fn board_json_attaches_clocks() {
    let input = iteration7();
    let board = compute(&input, now());
    let unseen = Unseen::default();
    let clocks = BTreeMap::from([(ko(1813), json!({"ball": "you"}))]);
    let value = board_json(
        &board,
        &input,
        &JsonContext {
            clocks: &clocks,
            ..context(&[], &[], &unseen)
        },
    );
    let tickets = value["lanes"][0]["tickets"].as_array().unwrap();
    assert_eq!(tickets[0]["number"], 1813);
    assert_eq!(tickets[0]["clock"]["ball"], "you");
    assert!(tickets[1..]
        .iter()
        .all(|ticket| ticket.get("clock").is_none()));
}

#[test]
fn needs_you_counts_until_seen() {
    let board = compute(&iteration7(), now());
    let since = BTreeMap::from([(ko(1813), 40)]);
    let unseen = needs_you_unseen(&board, &since, 0);
    assert_eq!(unseen.count, 1);
    assert_eq!(unseen.lane_ids, BTreeSet::from([1]));
    // Seen with event 40 the newest: seen, even within the same second.
    assert_eq!(needs_you_unseen(&board, &since, 40).count, 0);
    // A later event in the same second is not.
    assert_eq!(needs_you_unseen(&board, &since, 39).count, 1);
}

#[test]
fn partial_graphql_page_is_rejected() {
    let partial =
        br#"{"data":{"rateLimit":null,"repository":{"issues":{"pageInfo":{"hasNextPage":false},
        "nodes":[{"number":1,"title":"t","url":"u","blockedBy":null,"subIssues":null}]}}},
        "errors":[{"message":"timeout on blockedBy"}]}"#;
    assert_eq!(
        parse_issues_page(partial),
        Err("timeout on blockedBy".to_owned())
    );
}

#[test]
fn lookup_state_reason_keeps_github_casing() {
    use crate::work_claims::{BatchFetch, GhItem, ItemFetch, WorkKind};
    let batch: BatchFetch = BTreeMap::from([(
        5,
        ItemFetch::Found(Box::new(GhItem {
            is_draft: false,
            kind: WorkKind::Ticket,
            title: "t".into(),
            state: "closed".into(),
            state_reason: Some("not_planned".into()),
            url: "u".into(),
            head_ref: None,
            head_sha: None,
            closed_at: None,
            merged_at: None,
            closing_refs: None,
        })),
    )]);
    let node = items_from_batch(REPO, &[5], &batch)[&5].clone().unwrap();
    assert_eq!(node.state_reason.as_deref(), Some("NOT_PLANNED"));
    let mut closed = item(&k(5), false);
    closed.state_reason = Some("not_planned".into());
    assert_eq!(done_reason(&closed), Some("not_planned"));
}

#[test]
fn resolve_rejects_partial_but_reads_missing_issue() {
    let missing = br#"{"data":{"r0":{"issue":null}},
        "errors":[{"type":"NOT_FOUND","message":"Could not resolve"}]}"#;
    assert_eq!(parse_resolve(missing, 1), Ok(vec![None]));
    let partial = br#"{"data":{"r0":{"issue":{"id":"I","number":1,"state":"OPEN",
        "repository":{"nameWithOwner":"acme/widgets"},"blockedBy":null}}},
        "errors":[{"type":"INTERNAL","message":"blockedBy timed out"}]}"#;
    assert_eq!(
        parse_resolve(partial, 1),
        Err("blockedBy timed out".to_owned())
    );
}

#[test]
fn rate_limited_read_leaves_repo_stale() {
    let (store, _dir) = temp_store();
    let github = FakeGitHub::new();
    handoff(&github);
    *github.rate_remaining.lock().unwrap() = Some(150);
    let result = run_pass(&store, &github, &outside(), now()).unwrap();
    assert!(store.repo_syncs().unwrap().iter().any(RepoSync::stale));
    assert!(result
        .board
        .facts
        .values()
        .all(|facts| facts.state != TicketState::Ready));
}

// ---------------------------------------------------------------------------
// I: alerts (ticket #1682).

fn push_store(dir: &std::path::Path) -> crate::owner_push::OwnerPushStore {
    crate::owner_push::OwnerPushStore::new(dir.join("owner_push.db"))
}

/// A read pass, then its alerts as owner notices.
fn pass_alerts(
    store: &BoardStore,
    github: &FakeGitHub,
    push: &crate::owner_push::OwnerPushStore,
) -> Vec<pushes::Alert> {
    let recomputed = run_pass(store, github, &outside(), now()).unwrap();
    pushes::send(store, push, "owner", &recomputed, now()).unwrap()
}

fn board_notices(push: &crate::owner_push::OwnerPushStore) -> Vec<crate::owner_push::Notice> {
    push.list_notices("owner", now()).unwrap()
}

fn claim_ticket(store: &BoardStore, number: i64, session: &str, name: &str) {
    store
        .open_write()
        .unwrap()
        .execute(
            "INSERT INTO work_claims (id, repo, number, kind, session_id, session_name, source,
                                      claimed_at)
             VALUES (?1, ?2, ?3, 'ticket', ?4, ?5, 'explicit', '2026-09-29T10:00:00Z')",
            params![format!("c{number}"), REPO, number, session, name],
        )
        .unwrap();
}

fn set_pr(github: &FakeGitHub, number: i64, state: &str) {
    github.with(&k(number), |issue| {
        issue.prs = vec![PrRef {
            repo: REPO.into(),
            number: 1700,
            state: state.into(),
            url: String::new(),
        }];
    });
}

#[test]
fn ready_notice_once_per_lane_pass() {
    let (store, dir) = temp_store();
    let push = push_store(&dir);
    let github = FakeGitHub::new();
    handoff(&github);
    claim_ticket(&store, 1653, "eng1", "sm-1653-engineer");
    let lane = add_lane(&store, &github, &k(1651));
    github.close(&k(1653), "COMPLETED");
    let alerts = pass_alerts(&store, &github, &push);
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0].title, "Ready in lane 1, Ticket 1651");
    assert_eq!(
        alerts[0].body,
        "#1654, #1656, #1657 can start — #1653 closed"
    );
    let notices = board_notices(&push);
    assert_eq!(notices.len(), 1);
    let notice = &notices[0];
    assert_eq!(notice.kind, NOTICE_BOARD_READY);
    assert_eq!(notice.session_id, "board");
    assert_eq!(notice.session_name, "sm board");
    assert_eq!(notice.reader_path, format!("/board#lane-{lane}"));
    assert!(!notice.blocking);
    let (subject_lane, event_id) = pushes::subject_event(&notice.subject_id).unwrap();
    assert_eq!(subject_lane, lane);
    let event = store
        .events(50)
        .unwrap()
        .into_iter()
        .find(|event| event.id == event_id)
        .unwrap();
    assert_eq!(event.kind, "push_sent");
    // Nothing new: no second notice.
    assert!(pass_alerts(&store, &github, &push).is_empty());
    assert_eq!(board_notices(&push).len(), 1);
}

#[test]
fn ready_notice_lists_four_then_more() {
    let (store, dir) = temp_store();
    let push = push_store(&dir);
    let github = FakeGitHub::new();
    handoff(&github);
    for number in [1658, 1659] {
        github.open(k(number));
        github.after(&k(number), &k(1653));
        github.under(&k(number), &k(1651));
    }
    claim_ticket(&store, 1653, "eng1", "sm-1653-engineer");
    add_lane(&store, &github, &k(1651));
    github.close(&k(1653), "COMPLETED");
    let alerts = pass_alerts(&store, &github, &push);
    assert_eq!(
        alerts[0].body,
        "#1654, #1656, #1657, #1658, +1 more can start — #1653 closed"
    );
}

#[test]
fn ready_notice_after_holder_lets_go() {
    let (store, dir) = temp_store();
    let push = push_store(&dir);
    let github = FakeGitHub::new();
    handoff(&github);
    claim_ticket(&store, 1653, "eng1", "sm-1653-engineer");
    add_lane(&store, &github, &k(1651));
    store
        .open_write()
        .unwrap()
        .execute(
            "UPDATE work_claims SET ended_at = '2026-09-29T11:30:00Z' WHERE id = 'c1653'",
            [],
        )
        .unwrap();
    let alerts = pass_alerts(&store, &github, &push);
    assert_eq!(alerts.len(), 1);
    assert_eq!(
        alerts[0].body,
        "#1653 can start — sm-1653-engineer let go of it"
    );
}

#[test]
fn ready_notice_after_pr_closed_unmerged() {
    let (store, dir) = temp_store();
    let push = push_store(&dir);
    let github = FakeGitHub::new();
    handoff(&github);
    set_pr(&github, 1653, "OPEN");
    add_lane(&store, &github, &k(1651));
    set_pr(&github, 1653, "CLOSED");
    let alerts = pass_alerts(&store, &github, &push);
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0].body, "#1653 can start — PR #1700 closed");
}

#[test]
fn no_ready_notice_for_merged_not_closed() {
    let (store, dir) = temp_store();
    let push = push_store(&dir);
    let github = FakeGitHub::new();
    handoff(&github);
    set_pr(&github, 1653, "OPEN");
    add_lane(&store, &github, &k(1651));
    set_pr(&github, 1653, "MERGED");
    let recomputed = run_pass(&store, &github, &outside(), now()).unwrap();
    assert_eq!(state(&recomputed.board, &k(1653)), TicketState::Ready);
    assert!(pushes::send(&store, &push, "owner", &recomputed, now())
        .unwrap()
        .is_empty());
    assert!(board_notices(&push).is_empty());
}

#[test]
fn ready_notice_after_reopen() {
    let (store, dir) = temp_store();
    let push = push_store(&dir);
    let github = FakeGitHub::new();
    handoff(&github);
    github.close(&k(1653), "COMPLETED");
    github.close(&k(1654), "COMPLETED");
    add_lane(&store, &github, &k(1651));
    github.with(&k(1654), |issue| {
        issue.open = true;
        issue.state_reason = Some("REOPENED".into());
    });
    let alerts = pass_alerts(&store, &github, &push);
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0].body, "#1654 can start — #1654 reopened");
}

#[test]
fn no_notice_for_new_member_or_new_lane() {
    let (store, dir) = temp_store();
    let push = push_store(&dir);
    let github = FakeGitHub::new();
    handoff(&github);
    github.close(&k(1653), "COMPLETED");
    // The lane's first recompute: three tickets are Ready, and no alert.
    check_goal(&store, &github, &k(1651), now())
        .unwrap()
        .unwrap();
    store
        .add_lane(&k(1651), "owner", "Owner", now())
        .unwrap()
        .unwrap();
    assert!(pass_alerts(&store, &github, &push).is_empty());
    // A ticket that joins Ready: no alert either.
    github.open(k(1660));
    github.under(&k(1660), &k(1651));
    let alerts = pass_alerts(&store, &github, &push);
    assert!(alerts.is_empty(), "{alerts:?}");
    assert!(board_notices(&push).is_empty());
}

#[test]
fn no_notice_when_repo_stale() {
    let (store, dir) = temp_store();
    let push = push_store(&dir);
    let github = FakeGitHub::new();
    handoff(&github);
    claim_ticket(&store, 1653, "eng1", "sm-1653-engineer");
    add_lane(&store, &github, &k(1651));
    // The claim ends while the repo's reads fail: nothing is Ready.
    store
        .open_write()
        .unwrap()
        .execute(
            "UPDATE work_claims SET ended_at = '2026-09-29T11:30:00Z'",
            [],
        )
        .unwrap();
    github.down.lock().unwrap().insert(REPO.to_owned());
    assert!(pass_alerts(&store, &github, &push).is_empty());
    // Reads recover: the alert comes then.
    github.down.lock().unwrap().clear();
    assert_eq!(pass_alerts(&store, &github, &push).len(), 1);
}

#[test]
fn lane_done_notice() {
    let (store, dir) = temp_store();
    let push = push_store(&dir);
    let github = FakeGitHub::new();
    handoff(&github);
    // A second lane below: #1671 starts after #1672, under goal #1670.
    for number in [1670, 1671, 1672] {
        github.open(k(number));
    }
    github.after(&k(1671), &k(1672));
    github.under(&k(1671), &k(1670));
    github.under(&k(1672), &k(1670));
    claim_ticket(&store, 1672, "eng1", "sm-1653-engineer");
    let first = add_lane(&store, &github, &k(1651));
    add_lane(&store, &github, &k(1670));
    github.close(&k(1651), "COMPLETED");
    github.close(&k(1672), "COMPLETED");
    let alerts = pass_alerts(&store, &github, &push);
    let titles: Vec<&str> = alerts.iter().map(|alert| alert.title.as_str()).collect();
    // The lane below is now lane 1.
    assert_eq!(
        titles,
        ["Ready in lane 1, Ticket 1670", "Lane done: Ticket 1651"]
    );
    let done = &alerts[1];
    assert_eq!(done.kind, NOTICE_BOARD_LANE_DONE);
    assert_eq!(done.lane_id, first);
    assert_eq!(done.body, "widgets#1651 closed · lanes below move up");
    assert_eq!(done.reader_path, "/board");
    assert_eq!(board_notices(&push).len(), 2);
    // An owner End sends nothing.
    let lanes = store.active_lanes().unwrap();
    store.end_lane(lanes[0].id, now()).unwrap();
    let recomputed = store.recompute(&outside(), now()).unwrap();
    assert!(pushes::send(&store, &push, "owner", &recomputed, now())
        .unwrap()
        .is_empty());
}

#[test]
fn still_wanted_false_once_started() {
    let (store, dir) = temp_store();
    let push = push_store(&dir);
    let github = FakeGitHub::new();
    handoff(&github);
    claim_ticket(&store, 1653, "eng1", "sm-1653-engineer");
    add_lane(&store, &github, &k(1651));
    github.close(&k(1653), "COMPLETED");
    pass_alerts(&store, &github, &push);
    let subject = board_notices(&push)[0].subject_id.clone();
    assert!(store.ready_notice_wanted(&subject).unwrap());
    // Agents take two of the three: one is still Ready.
    claim_ticket(&store, 1654, "eng1", "sm-1653-engineer");
    claim_ticket(&store, 1656, "eng1", "sm-1653-engineer");
    run_pass(&store, &github, &outside(), now()).unwrap();
    assert!(store.ready_notice_wanted(&subject).unwrap());
    claim_ticket(&store, 1657, "eng1", "sm-1653-engineer");
    run_pass(&store, &github, &outside(), now()).unwrap();
    assert!(!store.ready_notice_wanted(&subject).unwrap());
}

#[test]
fn board_notice_opened_by_seen_event() {
    let seen = ("2026-09-29T12:00:00Z".to_owned(), 40);
    // Ordered by event id, even within one second.
    assert!(pushes::notice_opened(
        "board:7:40",
        "2026-09-29T12:00:00Z",
        Some(&seen)
    ));
    assert!(!pushes::notice_opened(
        "board:7:41",
        "2026-09-29T12:00:00Z",
        Some(&seen)
    ));
    assert!(!pushes::notice_opened(
        "board:7:1",
        "2026-09-29T11:00:00Z",
        None
    ));
}
