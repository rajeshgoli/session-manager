//! Work claims (sm#1452, ticket #1485): which session is working which
//! ticket or PR, and the ticket-keyed event log.
//!
//! GitHub stays the source of truth for tickets and PRs; `work_items` is a
//! cache of their last known state. A claim is a row in `work_claims` from
//! `sm ticket` / `sm pr` (or `sm spawn --ticket`, or an implicit PR claim)
//! until the item closes on GitHub, the holder is retired, or the claim is
//! released or taken. Holder state (working, idle, stopped, retired) is
//! derived from the session record at read time. See
//! `docs/working/1452_sm_primitives.html`, appendices A–E.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior};
use serde::Serialize;
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::owner_docs::{repo_name, validate_repo_slug};

pub mod worktrees;

/// At most this many aliases per GraphQL call (about 20 KB of output).
pub const MAX_ALIASES_PER_QUERY: usize = 50;
/// A spawn reservation older than this is settled at the next recovery.
pub const RESERVATION_RECOVERY_AGE: Duration = Duration::from_secs(5 * 60);
/// Queue category for every message this module enqueues.
pub const MESSAGE_CATEGORY: &str = "work_claim";

pub fn init_work_claims_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS work_items (
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            kind TEXT NOT NULL,
            title TEXT NOT NULL,
            state TEXT NOT NULL,
            state_reason TEXT,
            url TEXT NOT NULL,
            head_ref TEXT,
            head_sha TEXT,
            closed_at TEXT,
            merged_at TEXT,
            synced_at TEXT,
            merge_check TEXT,
            sync_error TEXT,
            PRIMARY KEY (repo, number)
        );
        CREATE TABLE IF NOT EXISTS work_claims (
            id TEXT PRIMARY KEY,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            kind TEXT NOT NULL,
            session_id TEXT NOT NULL,
            session_name TEXT,
            parent_session_id TEXT,
            source TEXT NOT NULL,
            worktree_path TEXT,
            branch TEXT,
            claimed_at TEXT NOT NULL,
            ended_at TEXT,
            end_reason TEXT,
            ended_by_session_id TEXT,
            nudged_idle_at TEXT,
            managed_worktree INTEGER NOT NULL DEFAULT 0,
            base_sha TEXT,
            reserved_at TEXT,
            check_b_due_at TEXT
        );
        CREATE INDEX IF NOT EXISTS work_claims_item ON work_claims(repo, number);
        CREATE INDEX IF NOT EXISTS work_claims_session ON work_claims(session_id);
        CREATE UNIQUE INDEX IF NOT EXISTS work_claims_one_active
            ON work_claims(repo, number, session_id) WHERE ended_at IS NULL;
        CREATE TABLE IF NOT EXISTS worktree_keeps (
            path TEXT PRIMARY KEY,
            session_id TEXT,
            reason TEXT NOT NULL,
            kept_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS work_links (
            repo TEXT NOT NULL,
            pr_number INTEGER NOT NULL,
            ticket_number INTEGER NOT NULL,
            source TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (repo, pr_number, ticket_number, source)
        );
        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY,
            ts TEXT NOT NULL,
            kind TEXT NOT NULL,
            session_id TEXT,
            repo TEXT,
            ticket INTEGER,
            pr INTEGER,
            payload TEXT
        );
        CREATE INDEX IF NOT EXISTS events_ticket ON events(repo, ticket, id);
        CREATE INDEX IF NOT EXISTS events_pr ON events(repo, pr, id);
        CREATE INDEX IF NOT EXISTS events_session ON events(session_id, id);
        CREATE TABLE IF NOT EXISTS work_claims_meta (
            key TEXT PRIMARY KEY,
            value TEXT
        );
        "#,
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkKind {
    Ticket,
    Pr,
}

impl WorkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ticket => "ticket",
            Self::Pr => "pr",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ticket" => Some(Self::Ticket),
            "pr" => Some(Self::Pr),
            _ => None,
        }
    }

    /// How messages name the kind: `ticket #1452`, `PR #1470`.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Ticket => "ticket",
            Self::Pr => "PR",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimSource {
    Explicit,
    Spawn,
    CodexReview,
    DocPublish,
    Backfill,
}

impl ClaimSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Spawn => "spawn",
            Self::CodexReview => "codex_review",
            Self::DocPublish => "doc_publish",
            Self::Backfill => "backfill",
        }
    }

    /// Implicit claims never block: they record a collision instead.
    pub fn is_implicit(self) -> bool {
        matches!(self, Self::CodexReview | Self::DocPublish)
    }
}

/// Derived from the session record: `status` plus the retirement marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HolderState {
    Working,
    Idle,
    /// Stopped but restorable: the claim is dormant and blocks nobody.
    Stopped,
    Retired,
}

impl HolderState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Stopped => "stopped",
            Self::Retired => "retired",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub parent_session_id: Option<String>,
    pub state: HolderState,
    pub stopped_at: Option<String>,
}

/// The session records the claim rules read, by id.
#[derive(Debug, Clone, Default)]
pub struct SessionDirectory {
    sessions: BTreeMap<String, SessionInfo>,
}

impl SessionDirectory {
    pub fn new(sessions: impl IntoIterator<Item = SessionInfo>) -> Self {
        Self {
            sessions: sessions
                .into_iter()
                .map(|session| (session.id.clone(), session))
                .collect(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&SessionInfo> {
        self.sessions.get(id)
    }

    pub fn insert(&mut self, session: SessionInfo) {
        self.sessions.insert(session.id.clone(), session);
    }

    fn ancestors(&self, id: &str) -> Vec<String> {
        let mut chain = Vec::new();
        let mut current = self
            .sessions
            .get(id)
            .and_then(|session| session.parent_session_id.clone());
        while let Some(parent) = current {
            if chain.len() >= 32 || chain.contains(&parent) || parent == id {
                break;
            }
            current = self
                .sessions
                .get(&parent)
                .and_then(|session| session.parent_session_id.clone());
            chain.push(parent);
        }
        chain
    }

    /// How `other` relates to `claimant` when they are in the same line
    /// (one an ancestor of the other); `None` otherwise, siblings included.
    pub fn relation(&self, claimant: &str, other: &str) -> Option<&'static str> {
        let up = self.ancestors(claimant);
        if let Some(index) = up.iter().position(|id| id == other) {
            return Some(if index == 0 { "parent" } else { "ancestor" });
        }
        let down = self.ancestors(other);
        if let Some(index) = down.iter().position(|id| id == claimant) {
            return Some(if index == 0 { "child" } else { "descendant" });
        }
        None
    }
}

/// An item as GitHub reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhItem {
    pub kind: WorkKind,
    pub title: String,
    /// `open`, `closed` or `merged` (PRs only).
    pub state: String,
    pub state_reason: Option<String>,
    pub url: String,
    pub head_ref: Option<String>,
    pub head_sha: Option<String>,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    /// PRs: every `closingIssuesReferences` node as `(owner/name, number)`.
    /// `None` when the set is incomplete (a page failed): links stay as they
    /// were.
    pub closing_refs: Option<Vec<(String, i64)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemFetch {
    Found(Box<GhItem>),
    NotFound,
}

/// Per number; a number missing from the map failed to fetch.
pub type BatchFetch = BTreeMap<i64, ItemFetch>;

/// Where item state comes from. The live server uses `gh api graphql`; tests
/// substitute a fixture the way `OwnerDocSource` is substituted.
pub trait WorkItemSource: Send + Sync {
    /// One repo, at most `MAX_ALIASES_PER_QUERY` numbers. `Err` is a failure
    /// of the whole batch (transport, invalid JSON).
    fn fetch(&self, repo: &str, numbers: &[i64]) -> Result<BatchFetch, String>;
}

const ITEM_FRAGMENT: &str = "fragment F on IssueOrPullRequest {
  __typename
  ... on Issue { title state stateReason closedAt url }
  ... on PullRequest { title state mergedAt closedAt url headRefName headRefOid
    closingIssuesReferences(first: 20) { totalCount pageInfo { hasNextPage endCursor }
      nodes { number repository { nameWithOwner } } } } }";

fn graphql_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// One aliased query for `numbers` in `repo` (`owner/name`).
pub fn items_query(repo: &str, numbers: &[i64]) -> String {
    let (owner, name) = repo.split_once('/').unwrap_or((repo, ""));
    let aliases = numbers
        .iter()
        .map(|number| format!("  i{number}: issueOrPullRequest(number: {number}) {{ ...F }}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "query {{ repository(owner: {}, name: {}) {{\n{aliases}\n}} }}\n{ITEM_FRAGMENT}",
        graphql_string(owner),
        graphql_string(name)
    )
}

/// The rest of one PR's closing references, after `cursor`.
pub fn closing_refs_page_query(repo: &str, number: i64, cursor: &str) -> String {
    let (owner, name) = repo.split_once('/').unwrap_or((repo, ""));
    format!(
        "query {{ repository(owner: {}, name: {}) {{ pullRequest(number: {number}) {{
  closingIssuesReferences(first: 100, after: {}) {{ pageInfo {{ hasNextPage endCursor }}
    nodes {{ number repository {{ nameWithOwner }} }} }} }} }} }}",
        graphql_string(owner),
        graphql_string(name),
        graphql_string(cursor)
    )
}

/// A parsed batch plus the PRs whose closing references need more pages:
/// `(number, endCursor)`.
pub type ParsedBatch = (BatchFetch, Vec<(i64, String)>);

/// Parses `gh api graphql` stdout for `items_query`. `gh` exits 1 when any
/// alias fails but still prints the data for the rest, so the exit code is
/// ignored: valid JSON is parsed whatever it says.
pub fn parse_items_response(stdout: &[u8], numbers: &[i64]) -> Result<ParsedBatch, String> {
    let payload: Value = serde_json::from_slice(stdout)
        .map_err(|error| format!("GitHub returned invalid JSON: {error}"))?;
    let not_found: BTreeSet<String> = payload["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|error| error["type"].as_str() == Some("NOT_FOUND"))
        .map(|error| {
            error["path"]
                .as_array()
                .and_then(|path| path.last())
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        })
        .collect();
    let repository = &payload["data"]["repository"];
    if repository.is_null() {
        if not_found.contains("repository") {
            return Ok((
                numbers
                    .iter()
                    .map(|number| (*number, ItemFetch::NotFound))
                    .collect(),
                Vec::new(),
            ));
        }
        let detail = payload["errors"][0]["message"]
            .as_str()
            .unwrap_or("no repository in the response");
        return Err(format!("GitHub query failed: {detail}"));
    }
    let mut fetched = BatchFetch::new();
    let mut more = Vec::new();
    for number in numbers {
        let alias = format!("i{number}");
        let node = &repository[alias.as_str()];
        if node.is_null() {
            if not_found.contains(&alias) {
                fetched.insert(*number, ItemFetch::NotFound);
            }
            continue;
        }
        let Some(item) = parse_item(node) else {
            continue;
        };
        if item.kind == WorkKind::Pr {
            let page = &node["closingIssuesReferences"]["pageInfo"];
            if page["hasNextPage"].as_bool() == Some(true) {
                if let Some(cursor) = page["endCursor"].as_str() {
                    more.push((*number, cursor.to_owned()));
                }
            }
        }
        fetched.insert(*number, ItemFetch::Found(Box::new(item)));
    }
    Ok((fetched, more))
}

fn parse_item(node: &Value) -> Option<GhItem> {
    let text = |key: &str| node[key].as_str().map(ToOwned::to_owned);
    let kind = match node["__typename"].as_str()? {
        "Issue" => WorkKind::Ticket,
        "PullRequest" => WorkKind::Pr,
        _ => return None,
    };
    let state = node["state"].as_str()?.to_ascii_lowercase();
    let closing_refs =
        (kind == WorkKind::Pr).then(|| parse_ref_nodes(&node["closingIssuesReferences"]["nodes"]));
    Some(GhItem {
        kind,
        title: text("title").unwrap_or_default(),
        state,
        state_reason: text("stateReason").map(|reason| reason.to_ascii_lowercase()),
        url: text("url").unwrap_or_default(),
        head_ref: text("headRefName"),
        head_sha: text("headRefOid"),
        closed_at: text("closedAt"),
        merged_at: text("mergedAt"),
        closing_refs,
    })
}

fn parse_ref_nodes(nodes: &Value) -> Vec<(String, i64)> {
    nodes
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| {
            Some((
                node["repository"]["nameWithOwner"].as_str()?.to_owned(),
                node["number"].as_i64()?,
            ))
        })
        .collect()
}

/// A PR's closing references as `(owner/name, number)`.
pub type ClosingRefs = Vec<(String, i64)>;

/// One page of `closing_refs_page_query`: its references and the next cursor.
pub fn parse_closing_refs_page(stdout: &[u8]) -> Result<(ClosingRefs, Option<String>), String> {
    let payload: Value = serde_json::from_slice(stdout)
        .map_err(|error| format!("GitHub returned invalid JSON: {error}"))?;
    let refs = &payload["data"]["repository"]["pullRequest"]["closingIssuesReferences"];
    if refs.is_null() {
        return Err("GitHub returned no closing references page".to_owned());
    }
    let next = (refs["pageInfo"]["hasNextPage"].as_bool() == Some(true))
        .then(|| {
            refs["pageInfo"]["endCursor"]
                .as_str()
                .map(ToOwned::to_owned)
        })
        .flatten();
    if refs["pageInfo"]["hasNextPage"].as_bool() == Some(true) && next.is_none() {
        return Err("GitHub returned a page without a cursor".to_owned());
    }
    Ok((parse_ref_nodes(&refs["nodes"]), next))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkItem {
    pub repo: String,
    pub number: i64,
    pub kind: String,
    pub title: String,
    pub state: String,
    pub state_reason: Option<String>,
    pub url: String,
    pub head_ref: Option<String>,
    pub head_sha: Option<String>,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub synced_at: Option<String>,
    pub merge_check: Option<String>,
    pub sync_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkClaim {
    pub id: String,
    pub repo: String,
    pub number: i64,
    pub kind: String,
    pub session_id: String,
    pub session_name: Option<String>,
    pub parent_session_id: Option<String>,
    pub source: String,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub claimed_at: String,
    pub ended_at: Option<String>,
    pub end_reason: Option<String>,
    pub ended_by_session_id: Option<String>,
    pub nudged_idle_at: Option<String>,
    pub managed_worktree: bool,
    pub base_sha: Option<String>,
    pub reserved_at: Option<String>,
    pub check_b_due_at: Option<String>,
}

impl WorkClaim {
    pub fn kind(&self) -> WorkKind {
        WorkKind::parse(&self.kind).unwrap_or(WorkKind::Ticket)
    }
}

/// A claim with its item's cached title and state, as lists show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaimView {
    #[serde(flatten)]
    pub claim: WorkClaim,
    pub title: String,
    pub state: String,
    pub url: String,
    pub history_path: String,
}

/// GitHub slugs are case-insensitive; claims key on the lowercase form so
/// `Acme/Widgets` and `acme/widgets` are one item.
pub fn canonical_repo(repo: &str) -> String {
    repo.trim().to_ascii_lowercase()
}

/// `/t/<repo-name>/<n>`: the item's timeline page.
pub fn history_path(repo: &str, number: i64) -> String {
    format!("/t/{}/{number}", repo_name(repo))
}

#[derive(Debug, Clone)]
pub struct ClaimRequest {
    pub repo: String,
    pub number: i64,
    pub kind: WorkKind,
    pub claimant: SessionInfo,
    pub source: ClaimSource,
    pub take: bool,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    /// `--ticket`: tickets this PR is for (PR claims only).
    pub tickets: Vec<i64>,
    /// `sm spawn --ticket`: insert as a reservation, confirmed later.
    pub reserve: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Holder {
    pub session_id: String,
    pub name: String,
    pub state: String,
    pub claimed_at: String,
    pub worktree_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    Claimed {
        claim: WorkClaim,
        taken: bool,
        notes: Vec<String>,
        /// Implicit claims: one line per unrelated live holder.
        warnings: Vec<String>,
    },
    AlreadyHeld {
        claim: WorkClaim,
        notes: Vec<String>,
    },
    Collision {
        holders: Vec<Holder>,
    },
    /// Wrong kind, closed, merged or not found: 422 with this text.
    Rejected(String),
    /// GitHub could not be reached: 502 with this text; nothing recorded.
    Unreachable(String),
}

/// Result of a write: the outcome plus the sessions that were sent a
/// message, whose queues the caller drains after the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimResult {
    pub outcome: ClaimOutcome,
    pub notified: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WorkClaimStore {
    db_path: PathBuf,
}

impl WorkClaimStore {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open_write(&self) -> Result<Connection> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(&self.db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_work_claims_schema(&conn)?;
        Ok(conn)
    }

    /// A write connection only when the DB already exists; lifecycle hooks
    /// never create it.
    fn open_existing(&self) -> Result<Option<Connection>> {
        if !self.db_path.exists() {
            return Ok(None);
        }
        self.open_write().map(Some)
    }

    /// Read connections never create the DB; a missing one reads as empty.
    fn open_read(&self) -> Result<Option<Connection>> {
        if !self.db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let has_tables = table_exists(&conn, "work_claims_meta")?;
        Ok(has_tables.then_some(conn))
    }

    pub fn ensure_schema(&self) -> Result<()> {
        self.open_write().map(|_| ())
    }

    pub fn item(&self, repo: &str, number: i64) -> Result<Option<WorkItem>> {
        let Some(conn) = self.open_read()? else {
            return Ok(None);
        };
        get_item(&conn, repo, number)
    }

    pub fn claim(&self, id: &str) -> Result<Option<WorkClaim>> {
        let Some(conn) = self.open_read()? else {
            return Ok(None);
        };
        get_claim(&conn, id)
    }

    /// Every claim row for an item, oldest first.
    pub fn claims_for_item(&self, repo: &str, number: i64) -> Result<Vec<WorkClaim>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        query_claims(
            &conn,
            "WHERE repo = ?1 AND number = ?2 ORDER BY claimed_at, id",
            params![repo, number],
        )
    }

    /// Links from a PR to tickets: `(ticket, source)`.
    pub fn links_for_pr(&self, repo: &str, pr: i64) -> Result<Vec<(i64, String)>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(
            "SELECT ticket_number, source FROM work_links
             WHERE repo = ?1 AND pr_number = ?2 ORDER BY ticket_number, source",
        )?;
        let rows = statement
            .query_map(params![repo, pr], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Events, oldest first: `(kind, session_id, ticket, pr, payload)`.
    pub fn events(&self) -> Result<Vec<StoredEvent>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(
            "SELECT id, ts, kind, session_id, repo, ticket, pr, payload FROM events ORDER BY id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(StoredEvent {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    kind: row.get(2)?,
                    session_id: row.get(3)?,
                    repo: row.get(4)?,
                    ticket: row.get(5)?,
                    pr: row.get(6)?,
                    payload: row
                        .get::<_, Option<String>>(7)?
                        .and_then(|text| serde_json::from_str(&text).ok())
                        .unwrap_or(Value::Null),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A session's claims with item title and state; reservations excluded.
    pub fn claims_for_session(
        &self,
        session_id: &str,
        active_only: bool,
    ) -> Result<Vec<ClaimView>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let filter = if active_only {
            "WHERE session_id = ?1 AND ended_at IS NULL AND reserved_at IS NULL"
        } else {
            "WHERE session_id = ?1 AND reserved_at IS NULL"
        };
        let claims = query_claims(
            &conn,
            &format!("{filter} ORDER BY claimed_at, id"),
            params![session_id],
        )?;
        claim_views(&conn, claims)
    }

    /// Every active, confirmed claim: the session feed's source.
    pub fn active_claims(&self) -> Result<Vec<ClaimView>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let claims = query_claims(
            &conn,
            "WHERE ended_at IS NULL AND reserved_at IS NULL ORDER BY claimed_at, id",
            [],
        )?;
        claim_views(&conn, claims)
    }

    /// Stores a claim-time or sync fetch for `repo`. Items found are
    /// upserted (ending claims on a close or merge); a missing one gets
    /// `sync_error = "not found"`; `Err` marks every number. Returns the
    /// sessions that were sent a message.
    pub fn record_fetch(
        &self,
        repo: &str,
        numbers: &[i64],
        fetched: &Result<BatchFetch, String>,
    ) -> Result<()> {
        let repo = &canonical_repo(repo);
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_rfc3339();
        for number in numbers {
            match fetched {
                Err(error) => set_sync_error(&tx, repo, *number, error)?,
                Ok(batch) => match batch.get(number) {
                    Some(ItemFetch::Found(item)) => apply_item(&tx, repo, *number, item, &now)?,
                    Some(ItemFetch::NotFound) => set_sync_error(&tx, repo, *number, "not found")?,
                    None => set_sync_error(&tx, repo, *number, "fetch failed")?,
                },
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// An explicit or spawn claim. `fetched` is the claim-time fetch of the
    /// item and every `--ticket`; it is stored first, then the rules run
    /// in one transaction (appendix C).
    pub fn claim_explicit(
        &self,
        request: &ClaimRequest,
        fetched: Result<BatchFetch, String>,
        sessions: &SessionDirectory,
    ) -> Result<ClaimResult> {
        let request = &ClaimRequest {
            repo: canonical_repo(&request.repo),
            ..request.clone()
        };
        validate_repo_slug(&request.repo)?;
        let mut numbers = vec![request.number];
        numbers.extend(request.tickets.iter().copied());
        let batch = match &fetched {
            Err(error) => {
                return Ok(unreachable(request.number, error));
            }
            Ok(batch) => batch,
        };
        for number in &numbers {
            match batch.get(number) {
                None => {
                    return Ok(unreachable(request.number, "the query returned no result"));
                }
                Some(ItemFetch::NotFound) => {
                    return Ok(ClaimResult {
                        outcome: ClaimOutcome::Rejected(format!(
                            "No ticket or PR #{number} in {}.",
                            request.repo
                        )),
                        notified: Vec::new(),
                    });
                }
                Some(ItemFetch::Found(_)) => {}
            }
        }
        self.record_fetch(&request.repo, &numbers, &fetched)?;
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let item = get_item(&tx, &request.repo, request.number)?
            .context("claimed item vanished after its fetch")?;
        if let Some(detail) = validate_item(&item, request.kind) {
            return Ok(rejected(detail));
        }
        if request.kind == WorkKind::Ticket && !request.tickets.is_empty() {
            return Ok(rejected("--ticket applies to PR claims only.".to_owned()));
        }
        for ticket in &request.tickets {
            let linked = get_item(&tx, &request.repo, *ticket)?
                .context("linked ticket vanished after its fetch")?;
            if let Some(detail) = validate_item(&linked, WorkKind::Ticket) {
                return Ok(rejected(detail));
            }
        }
        let result = apply_claim_rules(&tx, request, sessions)?;
        tx.commit()?;
        Ok(result)
    }

    /// An implicit PR claim for `session` (Codex review request or doc
    /// publish). Never fetches: a PR stored closed or merged is not claimed,
    /// and an unknown PR gets a stub the next sync fills in. Returns `None`
    /// when nothing was claimed, else the warnings to print.
    pub fn claim_implicit(
        &self,
        repo: &str,
        pr: i64,
        session: &SessionInfo,
        source: ClaimSource,
        sessions: &SessionDirectory,
    ) -> Result<Option<Vec<String>>> {
        let repo = &canonical_repo(repo);
        validate_repo_slug(repo)?;
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = implicit_claim_conn(&tx, repo, pr, session, source, sessions, false)?;
        tx.commit()?;
        Ok(result)
    }

    /// `--release`: ends the caller's active claim with `released`.
    pub fn release(
        &self,
        session_id: &str,
        repo: &str,
        number: i64,
        kind: WorkKind,
    ) -> Result<Option<WorkClaim>> {
        let repo = &canonical_repo(repo);
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(claim) = query_claims(
            &tx,
            "WHERE repo = ?1 AND number = ?2 AND session_id = ?3 AND kind = ?4
               AND ended_at IS NULL AND reserved_at IS NULL",
            params![repo, number, session_id, kind.as_str()],
        )?
        .into_iter()
        .next() else {
            return Ok(None);
        };
        end_claim(&tx, &claim, "released", None, &now_rfc3339())?;
        let ended = get_claim(&tx, &claim.id)?;
        tx.commit()?;
        Ok(ended)
    }

    /// Retire or kill: ends every active claim of the session with `retired`.
    pub fn end_claims_for_session(&self, session_id: &str) -> Result<usize> {
        let Some(mut conn) = self.open_existing()? else {
            return Ok(0);
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let claims = query_claims(
            &tx,
            "WHERE session_id = ?1 AND ended_at IS NULL",
            params![session_id],
        )?;
        let now = now_rfc3339();
        for claim in &claims {
            end_claim(&tx, claim, "retired", None, &now)?;
        }
        tx.commit()?;
        Ok(claims.len())
    }

    /// Spawn step 4: the session exists, so the claim becomes real and
    /// supersedes any stopped holders. Returns the sessions sent a message.
    pub fn confirm_reservation(
        &self,
        claim_id: &str,
        sessions: &SessionDirectory,
    ) -> Result<Vec<String>> {
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut notified = Vec::new();
        confirm_reservation_conn(&tx, claim_id, sessions, &mut notified)?;
        tx.commit()?;
        Ok(notified)
    }

    /// Spawn step 3 failed: no session, so no claim.
    pub fn delete_reservation(&self, claim_id: &str) -> Result<()> {
        let conn = self.open_write()?;
        conn.execute(
            "DELETE FROM work_claims WHERE id = ?1 AND reserved_at IS NOT NULL",
            params![claim_id],
        )?;
        Ok(())
    }

    /// Reservations older than `RESERVATION_RECOVERY_AGE`: confirmed when
    /// their session exists, deleted otherwise. Returns (confirmed, deleted).
    pub fn recover_reservations(&self, sessions: &SessionDirectory) -> Result<(usize, usize)> {
        let Some(mut conn) = self.open_existing()? else {
            return Ok((0, 0));
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cutoff = rfc3339_before(RESERVATION_RECOVERY_AGE);
        let stale = query_claims(
            &tx,
            "WHERE reserved_at IS NOT NULL AND reserved_at < ?1",
            params![cutoff],
        )?;
        let (mut confirmed, mut deleted) = (0, 0);
        for claim in stale {
            if sessions.get(&claim.session_id).is_some() {
                confirm_reservation_conn(&tx, &claim.id, sessions, &mut Vec::new())?;
                confirmed += 1;
            } else {
                tx.execute("DELETE FROM work_claims WHERE id = ?1", params![claim.id])?;
                deleted += 1;
            }
        }
        tx.commit()?;
        Ok((confirmed, deleted))
    }

    /// Safety net for a retire whose hook failed: active, confirmed claims
    /// whose session is retired or gone end with `retired`.
    pub fn end_claims_of_retired_sessions(&self, sessions: &SessionDirectory) -> Result<usize> {
        let Some(mut conn) = self.open_existing()? else {
            return Ok(0);
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let claims = query_claims(&tx, "WHERE ended_at IS NULL AND reserved_at IS NULL", [])?;
        let now = now_rfc3339();
        let mut ended = 0;
        for claim in &claims {
            let gone = sessions
                .get(&claim.session_id)
                .is_none_or(|session| session.state == HolderState::Retired);
            if gone {
                end_claim(&tx, claim, "retired", None, &now)?;
                ended += 1;
            }
        }
        tx.commit()?;
        Ok(ended)
    }

    /// The periodic sync's tracked set, per repo (appendix D).
    pub fn tracked_items(&self) -> Result<BTreeMap<String, Vec<i64>>> {
        let Some(conn) = self.open_read()? else {
            return Ok(BTreeMap::new());
        };
        let mut statement = conn.prepare(
            r#"
            SELECT repo, number FROM work_items WHERE synced_at IS NULL
            UNION
            SELECT i.repo, i.number FROM work_items i
             WHERE i.state = 'open' AND (
                EXISTS (SELECT 1 FROM work_claims c
                         WHERE c.repo = i.repo AND c.number = i.number AND c.ended_at IS NULL)
                OR EXISTS (SELECT 1 FROM work_links l JOIN work_claims c
                             ON c.repo = l.repo AND c.ended_at IS NULL
                            AND (c.number = l.ticket_number OR c.number = l.pr_number)
                          WHERE l.repo = i.repo
                            AND (l.pr_number = i.number OR l.ticket_number = i.number)))
            UNION
            SELECT l.repo, l.ticket_number FROM work_links l JOIN work_items p
               ON p.repo = l.repo AND p.number = l.pr_number
             WHERE p.state = 'open' OR p.merge_check = 'pending'
            ORDER BY 1, 2
            "#,
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tracked = BTreeMap::<String, Vec<i64>>::new();
        for (repo, number) in rows {
            let item = get_item(&conn, &repo, number)?;
            // Closed and merged items are never re-polled once fetched.
            if item.as_ref().is_some_and(|item| {
                item.synced_at.is_some() && item.state != "open" && item.merge_check.is_none()
            }) {
                continue;
            }
            tracked.entry(repo).or_default().push(number);
        }
        Ok(tracked)
    }

    /// Backfill, once: a claim per `(repo, PR, session)` from past Codex
    /// review requests and doc publishes, no collision rules, no messages.
    /// Returns false when it already ran.
    pub fn backfill(&self, sessions: &SessionDirectory) -> Result<bool> {
        let Some(mut conn) = self.open_existing()? else {
            return Ok(false);
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if meta_get(&tx, "backfill_done_at")?.is_some() {
            return Ok(false);
        }
        let mut earliest = BTreeMap::<(String, i64, String), String>::new();
        let mut note = |repo: String, pr: i64, session: String, at: String| {
            let entry = earliest
                .entry((canonical_repo(&repo), pr, session))
                .or_insert(at.clone());
            if at < *entry {
                *entry = at;
            }
        };
        if table_exists(&tx, "codex_review_request_registrations")? {
            let mut statement = tx.prepare(
                "SELECT repo, pr_number, requester_session_id, MIN(requested_at)
                   FROM codex_review_request_registrations
                  WHERE requester_session_id IS NOT NULL AND requester_session_id != ''
                  GROUP BY repo, pr_number, requester_session_id",
            )?;
            for row in statement.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })? {
                let (repo, pr, session, at) = row?;
                note(repo, pr, session, at);
            }
        }
        if table_exists(&tx, "owner_doc_publishes")? {
            let mut statement = tx.prepare(
                "SELECT d.repo, d.pr_number, p.session_id, MIN(p.published_at)
                   FROM owner_doc_publishes p JOIN owner_docs d ON d.id = p.doc_id
                  WHERE d.pr_number IS NOT NULL
                  GROUP BY d.repo, d.pr_number, p.session_id",
            )?;
            for row in statement.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })? {
                let (repo, pr, session, at) = row?;
                note(repo, pr, session, at);
            }
        }
        for ((repo, pr, session_id), claimed_at) in earliest {
            let repo = canonical_repo(&repo);
            if validate_repo_slug(&repo).is_err() || has_any_claim(&tx, &repo, pr, &session_id)? {
                continue;
            }
            insert_stub(&tx, &repo, pr, WorkKind::Pr)?;
            let session = sessions.get(&session_id);
            let ended = match session {
                Some(session) if session.state != HolderState::Retired => None,
                Some(session) => Some(session.stopped_at.clone().unwrap_or(claimed_at.clone())),
                None => Some(claimed_at.clone()),
            };
            insert_claim_row(
                &tx,
                &NewClaim {
                    repo: &repo,
                    number: pr,
                    kind: WorkKind::Pr,
                    session_id: &session_id,
                    session_name: session.map(|session| session.name.as_str()),
                    parent_session_id: session.and_then(|s| s.parent_session_id.as_deref()),
                    source: ClaimSource::Backfill,
                    worktree_path: None,
                    branch: None,
                    claimed_at: &claimed_at,
                    reserved: false,
                },
                ended.as_deref().map(|at| (at, "retired")),
            )?;
        }
        // The scan after this only needs rows added from now on.
        meta_set(
            &tx,
            "implicit_watermark_reviews",
            &max_rowid(&tx, "codex_review_request_registrations")?.to_string(),
        )?;
        meta_set(
            &tx,
            "implicit_watermark_publishes",
            &max_rowid(&tx, "owner_doc_publishes")?.to_string(),
        )?;
        meta_set(&tx, "backfill_done_at", &now_rfc3339())?;
        tx.commit()?;
        Ok(true)
    }

    /// Reconciliation for implicit claims whose hook failed: every Codex
    /// review request and doc publish above the watermarks, for a session
    /// with no claim row at all on that PR, runs the implicit claim. A
    /// retired or unknown session gets an ended claim, as backfill does.
    pub fn reconcile_implicit(&self, sessions: &SessionDirectory) -> Result<usize> {
        let Some(mut conn) = self.open_existing()? else {
            return Ok(0);
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut pending = Vec::<(String, i64, String, ClaimSource)>::new();
        let reviews_mark = meta_get(&tx, "implicit_watermark_reviews")?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        let mut reviews_max = reviews_mark;
        if table_exists(&tx, "codex_review_request_registrations")? {
            let mut statement = tx.prepare(
                "SELECT rowid, repo, pr_number, requester_session_id
                   FROM codex_review_request_registrations
                  WHERE rowid > ?1 ORDER BY rowid",
            )?;
            for row in statement.query_map(params![reviews_mark], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })? {
                let (rowid, repo, pr, requester) = row?;
                reviews_max = reviews_max.max(rowid);
                if let Some(requester) = requester.filter(|value| !value.trim().is_empty()) {
                    pending.push((repo, pr, requester, ClaimSource::CodexReview));
                }
            }
        }
        let publishes_mark = meta_get(&tx, "implicit_watermark_publishes")?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        let mut publishes_max = publishes_mark;
        if table_exists(&tx, "owner_doc_publishes")? {
            let mut statement = tx.prepare(
                "SELECT p.id, d.repo, d.pr_number, p.session_id
                   FROM owner_doc_publishes p JOIN owner_docs d ON d.id = p.doc_id
                  WHERE p.id > ?1 ORDER BY p.id",
            )?;
            for row in statement.query_map(params![publishes_mark], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })? {
                let (id, repo, pr, session) = row?;
                publishes_max = publishes_max.max(id);
                if let Some(pr) = pr {
                    pending.push((repo, pr, session, ClaimSource::DocPublish));
                }
            }
        }
        let mut recorded = 0;
        for (repo, pr, session_id, source) in pending {
            let repo = canonical_repo(&repo);
            if validate_repo_slug(&repo).is_err() || has_any_claim(&tx, &repo, pr, &session_id)? {
                continue;
            }
            let session = sessions.get(&session_id).cloned().unwrap_or(SessionInfo {
                id: session_id.clone(),
                name: session_id.clone(),
                parent_session_id: None,
                state: HolderState::Retired,
                stopped_at: None,
            });
            if implicit_claim_conn(&tx, &repo, pr, &session, source, sessions, true)?.is_some() {
                recorded += 1;
            }
        }
        meta_set(&tx, "implicit_watermark_reviews", &reviews_max.to_string())?;
        meta_set(
            &tx,
            "implicit_watermark_publishes",
            &publishes_max.to_string(),
        )?;
        tx.commit()?;
        Ok(recorded)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    pub id: i64,
    pub ts: String,
    pub kind: String,
    pub session_id: Option<String>,
    pub repo: Option<String>,
    pub ticket: Option<i64>,
    pub pr: Option<i64>,
    pub payload: Value,
}

fn unreachable(number: i64, error: &str) -> ClaimResult {
    ClaimResult {
        outcome: ClaimOutcome::Unreachable(format!(
            "Could not reach GitHub to check #{number}: {error}. Nothing was recorded."
        )),
        notified: Vec::new(),
    }
}

fn rejected(detail: String) -> ClaimResult {
    ClaimResult {
        outcome: ClaimOutcome::Rejected(detail),
        notified: Vec::new(),
    }
}

/// The error text for claiming `item` as `kind`, or `None` when it may be
/// claimed: it is that kind and open.
fn validate_item(item: &WorkItem, kind: WorkKind) -> Option<String> {
    let number = item.number;
    match (item.kind.as_str(), kind) {
        ("pr", WorkKind::Ticket) => {
            return Some(format!("#{number} is a pull request; use sm pr {number}."))
        }
        ("ticket", WorkKind::Pr) => {
            return Some(format!("#{number} is a ticket; use sm ticket {number}."))
        }
        _ => {}
    }
    match (kind, item.state.as_str()) {
        (_, "open") => None,
        (WorkKind::Ticket, _) => Some(format!("Ticket #{number} is closed.")),
        (WorkKind::Pr, state) => Some(format!("PR #{number} is {state}.")),
    }
}

enum Classified<'a> {
    Dormant(&'a WorkClaim),
    Retired(&'a WorkClaim),
    SameLine(&'a WorkClaim, &'static str),
    Collision(&'a WorkClaim, Holder),
}

fn classify<'a>(
    others: &'a [WorkClaim],
    claimant: &SessionInfo,
    sessions: &SessionDirectory,
) -> Vec<Classified<'a>> {
    // The claimant may not be in the directory yet (a spawn reservation).
    let mut directory = sessions.clone();
    directory.insert(claimant.clone());
    others
        .iter()
        .map(|claim| {
            let session = directory.get(&claim.session_id).cloned().or_else(|| {
                // A reservation's session is being created: it counts as a
                // live holder whose parent is its spawner.
                claim.reserved_at.as_ref().map(|_| SessionInfo {
                    id: claim.session_id.clone(),
                    name: claim
                        .session_name
                        .clone()
                        .unwrap_or_else(|| claim.session_id.clone()),
                    parent_session_id: claim.parent_session_id.clone(),
                    state: HolderState::Working,
                    stopped_at: None,
                })
            });
            let Some(session) = session else {
                return Classified::Retired(claim);
            };
            match session.state {
                HolderState::Retired => return Classified::Retired(claim),
                HolderState::Stopped => return Classified::Dormant(claim),
                HolderState::Working | HolderState::Idle => {}
            }
            let mut with_holder = directory.clone();
            with_holder.insert(session.clone());
            if let Some(relation) = with_holder.relation(&claimant.id, &session.id) {
                return Classified::SameLine(claim, relation);
            }
            Classified::Collision(
                claim,
                Holder {
                    session_id: session.id.clone(),
                    name: session.name.clone(),
                    state: session.state.as_str().to_owned(),
                    claimed_at: claim.claimed_at.clone(),
                    worktree_path: claim.worktree_path.clone(),
                },
            )
        })
        .collect()
}

fn apply_claim_rules(
    conn: &Connection,
    request: &ClaimRequest,
    sessions: &SessionDirectory,
) -> Result<ClaimResult> {
    let now = now_rfc3339();
    let claimant = &request.claimant;
    let noun = request.kind.noun();
    let number = request.number;
    let active = query_claims(
        conn,
        "WHERE repo = ?1 AND number = ?2 AND ended_at IS NULL ORDER BY claimed_at, id",
        params![request.repo, number],
    )?;
    let (own, others): (Vec<_>, Vec<_>) = active
        .into_iter()
        .partition(|claim| claim.session_id == claimant.id);
    let classified = classify(&others, claimant, sessions);
    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    let mut notified = Vec::new();
    let explicit = !request.source.is_implicit();
    let collisions: Vec<&Holder> = classified
        .iter()
        .filter_map(|entry| match entry {
            Classified::Collision(_, holder) => Some(holder),
            _ => None,
        })
        .collect();

    if let Some(own) = own.into_iter().next() {
        if request.take {
            resolve_others(
                conn,
                &classified,
                claimant,
                request,
                sessions,
                &now,
                &mut notes,
                &mut notified,
            )?;
        }
        if request.kind == WorkKind::Pr && request.source == ClaimSource::Explicit {
            conn.execute(
                "UPDATE work_claims
                    SET worktree_path = COALESCE(worktree_path, ?2),
                        branch = COALESCE(branch, ?3)
                  WHERE id = ?1",
                params![own.id, request.worktree_path, request.branch],
            )?;
            for ticket in link_targets(conn, request, &mut notes)? {
                if insert_link(conn, &request.repo, number, ticket, "claim", &now)? {
                    notes.push(format!("Linked PR #{number} to ticket #{ticket}."));
                }
            }
            notes.extend(closing_ref_notes(conn, &request.repo, number)?);
        }
        let claim = get_claim(conn, &own.id)?.context("held claim vanished")?;
        return Ok(ClaimResult {
            outcome: ClaimOutcome::AlreadyHeld { claim, notes },
            notified,
        });
    }

    if !collisions.is_empty() && explicit && !request.take {
        let holders: Vec<Holder> = collisions.into_iter().cloned().collect();
        let (ticket, pr) = item_keys(conn, &request.repo, request.kind, number)?;
        write_event(
            conn,
            "claim.refused",
            Some(&claimant.id),
            Some(&request.repo),
            ticket,
            pr,
            json!({
                "holder_session_ids": holders.iter().map(|h| h.session_id.clone()).collect::<Vec<_>>(),
            }),
            &now,
        )?;
        return Ok(ClaimResult {
            outcome: ClaimOutcome::Collision { holders },
            notified,
        });
    }

    let taken = explicit && request.take && !collisions.is_empty();
    if explicit {
        resolve_others(
            conn,
            &classified,
            claimant,
            request,
            sessions,
            &now,
            &mut notes,
            &mut notified,
        )?;
    } else {
        for entry in &classified {
            match entry {
                Classified::Dormant(claim) => supersede(
                    conn,
                    claim,
                    claimant,
                    noun,
                    number,
                    &now,
                    &mut notes,
                    &mut notified,
                )?,
                Classified::Retired(claim) => end_claim(conn, claim, "retired", None, &now)?,
                Classified::SameLine(claim, relation) => {
                    notes.push(same_line_note(claim, relation, sessions))
                }
                Classified::Collision(_, holder) => warnings.push(format!(
                    "Warning: {noun} #{number} is also held by {} ({}), {}.",
                    holder.name, holder.session_id, holder.state
                )),
            }
        }
    }
    let claim_id = insert_claim_row(
        conn,
        &NewClaim {
            repo: &request.repo,
            number,
            kind: request.kind,
            session_id: &claimant.id,
            session_name: Some(&claimant.name),
            parent_session_id: claimant.parent_session_id.as_deref(),
            source: request.source,
            worktree_path: request.worktree_path.as_deref(),
            branch: request.branch.as_deref(),
            claimed_at: &now,
            reserved: request.reserve,
        },
        None,
    )?;
    if request.kind == WorkKind::Pr && request.source == ClaimSource::Explicit {
        for ticket in link_targets(conn, request, &mut notes)? {
            insert_link(conn, &request.repo, number, ticket, "claim", &now)?;
        }
    }
    if !request.reserve {
        write_claim_taken(conn, &claim_id, &now)?;
    }
    if !collisions.is_empty() && !explicit {
        let (ticket, pr) = item_keys(conn, &request.repo, request.kind, number)?;
        write_event(
            conn,
            "claim.collision",
            Some(&claimant.id),
            Some(&request.repo),
            ticket,
            pr,
            json!({
                "claim_id": claim_id,
                "holder_session_ids": collisions.iter().map(|h| h.session_id.clone()).collect::<Vec<_>>(),
            }),
            &now,
        )?;
    }
    if request.kind == WorkKind::Pr && request.source == ClaimSource::Explicit {
        notes.extend(closing_ref_notes(conn, &request.repo, number)?);
    }
    let claim = get_claim(conn, &claim_id)?.context("inserted claim vanished")?;
    Ok(ClaimResult {
        outcome: ClaimOutcome::Claimed {
            claim,
            taken,
            notes,
            warnings,
        },
        notified,
    })
}

/// Explicit claim steps 3–4 (and `--take` on an item already held): end
/// dormant, retired and — with `--take` — colliding holders.
#[allow(clippy::too_many_arguments)]
fn resolve_others(
    conn: &Connection,
    classified: &[Classified<'_>],
    claimant: &SessionInfo,
    request: &ClaimRequest,
    sessions: &SessionDirectory,
    now: &str,
    notes: &mut Vec<String>,
    notified: &mut Vec<String>,
) -> Result<()> {
    let noun = request.kind.noun();
    let number = request.number;
    for entry in classified {
        match entry {
            // A spawn reservation supersedes stopped holders only once its
            // session exists (confirm), so a failed spawn tells nobody.
            Classified::Dormant(_) if request.reserve => {}
            Classified::Dormant(claim) => {
                supersede(conn, claim, claimant, noun, number, now, notes, notified)?
            }
            Classified::Retired(claim) => end_claim(conn, claim, "retired", None, now)?,
            Classified::SameLine(claim, relation) => {
                notes.push(same_line_note(claim, relation, sessions))
            }
            Classified::Collision(claim, holder) if request.take => {
                end_claim(conn, claim, "taken", Some(&claimant.id), now)?;
                let text = format!(
                    "[sm claim] {} ({}) claimed {noun} #{number}. Your claim on it ended.",
                    claimant.name, claimant.id
                );
                queue_notice(conn, &claim.session_id, &text, notified)?;
                notes.push(format!(
                    "Took {noun} #{number} from {} ({}).",
                    holder.name, holder.session_id
                ));
            }
            Classified::Collision(..) => {}
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn supersede(
    conn: &Connection,
    claim: &WorkClaim,
    claimant: &SessionInfo,
    noun: &str,
    number: i64,
    now: &str,
    notes: &mut Vec<String>,
    notified: &mut Vec<String>,
) -> Result<()> {
    end_claim(conn, claim, "superseded", Some(&claimant.id), now)?;
    let text = format!(
        "[sm claim] While you were stopped, {} ({}) claimed {noun} #{number}. Your claim on it ended.",
        claimant.name, claimant.id
    );
    queue_notice(conn, &claim.session_id, &text, notified)?;
    notes.push(format!(
        "Previous holder {} ({}) is stopped; its claim ended.",
        claim_holder_name(claim),
        claim.session_id
    ));
    Ok(())
}

fn claim_holder_name(claim: &WorkClaim) -> String {
    claim
        .session_name
        .clone()
        .unwrap_or_else(|| claim.session_id.clone())
}

fn same_line_note(claim: &WorkClaim, relation: &str, sessions: &SessionDirectory) -> String {
    let name = sessions
        .get(&claim.session_id)
        .map(|session| session.name.clone())
        .unwrap_or_else(|| claim_holder_name(claim));
    format!(
        "Also held by your {relation} {name} ({}).",
        claim.session_id
    )
}

fn queue_notice(
    conn: &Connection,
    target: &str,
    text: &str,
    notified: &mut Vec<String>,
) -> Result<()> {
    crate::queue::enqueue_important_in_conn(conn, target, text, MESSAGE_CATEGORY)?;
    if !notified.iter().any(|existing| existing == target) {
        notified.push(target.to_owned());
    }
    Ok(())
}

/// Tickets an explicit PR claim links to: every `--ticket`, else the one
/// ticket the claimant holds in the repo. Several held and none named links
/// nothing and adds a note.
fn link_targets(
    conn: &Connection,
    request: &ClaimRequest,
    notes: &mut Vec<String>,
) -> Result<Vec<i64>> {
    if !request.tickets.is_empty() {
        let mut tickets = request.tickets.clone();
        tickets.sort_unstable();
        tickets.dedup();
        return Ok(tickets);
    }
    let held: Vec<i64> = query_claims(
        conn,
        "WHERE repo = ?1 AND session_id = ?2 AND kind = 'ticket' AND ended_at IS NULL
         ORDER BY claimed_at, id",
        params![request.repo, request.claimant.id],
    )?
    .into_iter()
    .map(|claim| claim.number)
    .collect();
    match held.as_slice() {
        [] => Ok(Vec::new()),
        [one] => Ok(vec![*one]),
        several => {
            let list = several
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            notes.push(format!(
                "You hold tickets {list}; PR #{} is linked to neither (--ticket links one).",
                request.number
            ));
            Ok(Vec::new())
        }
    }
}

/// A warning per claim-linked ticket missing from the PR's closing
/// references (appendix F's `sm pr` note). Only when the PR was fetched.
fn closing_ref_notes(conn: &Connection, repo: &str, pr: i64) -> Result<Vec<String>> {
    if get_item(conn, repo, pr)?.is_none_or(|item| item.synced_at.is_none()) {
        return Ok(Vec::new());
    }
    let mut statement = conn.prepare(
        "SELECT ticket_number FROM work_links l
          WHERE repo = ?1 AND pr_number = ?2 AND source = 'claim'
            AND NOT EXISTS (SELECT 1 FROM work_links c
                             WHERE c.repo = l.repo AND c.pr_number = l.pr_number
                               AND c.ticket_number = l.ticket_number AND c.source = 'closing_ref')
          ORDER BY ticket_number",
    )?;
    let tickets = statement
        .query_map(params![repo, pr], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(tickets
        .into_iter()
        .map(|ticket| {
            format!("Note: PR #{pr}'s closing references don't include ticket #{ticket}.")
        })
        .collect())
}

fn implicit_claim_conn(
    conn: &Connection,
    repo: &str,
    pr: i64,
    session: &SessionInfo,
    source: ClaimSource,
    sessions: &SessionDirectory,
    skip_if_any_row: bool,
) -> Result<Option<Vec<String>>> {
    let held = if skip_if_any_row {
        has_any_claim(conn, repo, pr, &session.id)?
    } else {
        !query_claims(
            conn,
            "WHERE repo = ?1 AND number = ?2 AND session_id = ?3 AND ended_at IS NULL",
            params![repo, pr, session.id],
        )?
        .is_empty()
    };
    if held {
        return Ok(None);
    }
    match get_item(conn, repo, pr)? {
        Some(item) if item.kind != "pr" || item.state != "open" => return Ok(None),
        Some(_) => {}
        None => insert_stub(conn, repo, pr, WorkKind::Pr)?,
    }
    if session.state == HolderState::Retired {
        // Reconciliation for a session already gone: history only.
        let now = now_rfc3339();
        let ended_at = session.stopped_at.clone().unwrap_or(now.clone());
        insert_claim_row(
            conn,
            &NewClaim {
                repo,
                number: pr,
                kind: WorkKind::Pr,
                session_id: &session.id,
                session_name: Some(&session.name),
                parent_session_id: session.parent_session_id.as_deref(),
                source,
                worktree_path: None,
                branch: None,
                claimed_at: &now,
                reserved: false,
            },
            Some((&ended_at, "retired")),
        )?;
        return Ok(Some(Vec::new()));
    }
    let request = ClaimRequest {
        repo: repo.to_owned(),
        number: pr,
        kind: WorkKind::Pr,
        claimant: session.clone(),
        source,
        take: false,
        worktree_path: None,
        branch: None,
        tickets: Vec::new(),
        reserve: false,
    };
    match apply_claim_rules(conn, &request, sessions)?.outcome {
        ClaimOutcome::Claimed { warnings, .. } => Ok(Some(warnings)),
        _ => Ok(None),
    }
}

fn confirm_reservation_conn(
    conn: &Connection,
    claim_id: &str,
    sessions: &SessionDirectory,
    notified: &mut Vec<String>,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE work_claims SET reserved_at = NULL WHERE id = ?1 AND reserved_at IS NOT NULL",
        params![claim_id],
    )?;
    if changed == 0 {
        return Ok(());
    }
    let now = now_rfc3339();
    write_claim_taken(conn, claim_id, &now)?;
    let claim = get_claim(conn, claim_id)?.context("confirmed claim vanished")?;
    let claimant = sessions
        .get(&claim.session_id)
        .cloned()
        .unwrap_or(SessionInfo {
            id: claim.session_id.clone(),
            name: claim_holder_name(&claim),
            parent_session_id: claim.parent_session_id.clone(),
            state: HolderState::Working,
            stopped_at: None,
        });
    let others = query_claims(
        conn,
        "WHERE repo = ?1 AND number = ?2 AND ended_at IS NULL AND session_id != ?3",
        params![claim.repo, claim.number, claim.session_id],
    )?;
    for entry in classify(&others, &claimant, sessions) {
        if let Classified::Dormant(dormant) = entry {
            supersede(
                conn,
                dormant,
                &claimant,
                claim.kind().noun(),
                claim.number,
                &now,
                &mut Vec::new(),
                notified,
            )?;
        }
    }
    Ok(())
}

fn write_claim_taken(conn: &Connection, claim_id: &str, now: &str) -> Result<()> {
    let claim = get_claim(conn, claim_id)?.context("claim vanished")?;
    let (ticket, pr) = item_keys(conn, &claim.repo, claim.kind(), claim.number)?;
    write_event(
        conn,
        "claim.taken",
        Some(&claim.session_id),
        Some(&claim.repo),
        ticket,
        pr,
        json!({
            "claim_id": claim.id,
            "source": claim.source,
            "worktree_path": claim.worktree_path,
            "branch": claim.branch,
        }),
        now,
    )
}

fn end_claim(
    conn: &Connection,
    claim: &WorkClaim,
    reason: &str,
    ended_by: Option<&str>,
    now: &str,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE work_claims SET ended_at = ?2, end_reason = ?3, ended_by_session_id = ?4
          WHERE id = ?1 AND ended_at IS NULL",
        params![claim.id, now, reason, ended_by],
    )?;
    if changed == 0 {
        return Ok(());
    }
    let (ticket, pr) = item_keys(conn, &claim.repo, claim.kind(), claim.number)?;
    write_event(
        conn,
        "claim.released",
        Some(&claim.session_id),
        Some(&claim.repo),
        ticket,
        pr,
        json!({
            "claim_id": claim.id,
            "end_reason": reason,
            "ended_by_session_id": ended_by,
        }),
        now,
    )
}

/// The event's `(ticket, pr)`: a PR sets `ticket` only when it links to
/// exactly one ticket (never guessed).
fn item_keys(
    conn: &Connection,
    repo: &str,
    kind: WorkKind,
    number: i64,
) -> Result<(Option<i64>, Option<i64>)> {
    match kind {
        WorkKind::Ticket => Ok((Some(number), None)),
        WorkKind::Pr => {
            let tickets = linked_tickets(conn, repo, number)?;
            Ok(((tickets.len() == 1).then(|| tickets[0]), Some(number)))
        }
    }
}

fn linked_tickets(conn: &Connection, repo: &str, pr: i64) -> Result<Vec<i64>> {
    let mut statement = conn.prepare(
        "SELECT DISTINCT ticket_number FROM work_links
          WHERE repo = ?1 AND pr_number = ?2 ORDER BY ticket_number",
    )?;
    let rows = statement
        .query_map(params![repo, pr], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
fn write_event(
    conn: &Connection,
    kind: &str,
    session_id: Option<&str>,
    repo: Option<&str>,
    ticket: Option<i64>,
    pr: Option<i64>,
    payload: Value,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO events (ts, kind, session_id, repo, ticket, pr, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![now, kind, session_id, repo, ticket, pr, payload.to_string()],
    )?;
    Ok(())
}

/// Inserts a link row; writes `link.added` when the PR–ticket pair had no
/// row under any source. Returns whether this source's row is new.
fn insert_link(
    conn: &Connection,
    repo: &str,
    pr: i64,
    ticket: i64,
    source: &str,
    now: &str,
) -> Result<bool> {
    let existed = pair_linked(conn, repo, pr, ticket)?;
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO work_links (repo, pr_number, ticket_number, source, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![repo, pr, ticket, source, now],
    )? > 0;
    if inserted && !existed {
        write_event(
            conn,
            "link.added",
            None,
            Some(repo),
            Some(ticket),
            Some(pr),
            json!({"pr_number": pr, "ticket_number": ticket, "source": source}),
            now,
        )?;
    }
    Ok(inserted)
}

fn pair_linked(conn: &Connection, repo: &str, pr: i64, ticket: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM work_links WHERE repo = ?1 AND pr_number = ?2 AND ticket_number = ?3",
            params![repo, pr, ticket],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Replaces a PR's `closing_ref` links with `refs` (same-repo only). Claim
/// links are never touched, so a pair linked both ways stays linked.
fn reconcile_closing_refs(
    conn: &Connection,
    repo: &str,
    pr: i64,
    refs: &[(String, i64)],
    now: &str,
) -> Result<()> {
    let wanted: BTreeSet<i64> = refs
        .iter()
        .filter(|(ref_repo, _)| ref_repo.eq_ignore_ascii_case(repo))
        .map(|(_, number)| *number)
        .collect();
    let mut statement = conn.prepare(
        "SELECT ticket_number FROM work_links
          WHERE repo = ?1 AND pr_number = ?2 AND source = 'closing_ref'",
    )?;
    let current: BTreeSet<i64> = statement
        .query_map(params![repo, pr], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for ticket in current.difference(&wanted) {
        conn.execute(
            "DELETE FROM work_links
              WHERE repo = ?1 AND pr_number = ?2 AND ticket_number = ?3 AND source = 'closing_ref'",
            params![repo, pr, ticket],
        )?;
        if !pair_linked(conn, repo, pr, *ticket)? {
            write_event(
                conn,
                "link.removed",
                None,
                Some(repo),
                Some(*ticket),
                Some(pr),
                json!({"pr_number": pr, "ticket_number": ticket, "source": "closing_ref"}),
                now,
            )?;
        }
    }
    for ticket in wanted.difference(&current) {
        insert_stub(conn, repo, *ticket, WorkKind::Ticket)?;
        insert_link(conn, repo, pr, *ticket, "closing_ref", now)?;
    }
    Ok(())
}

/// Upserts one fetched item. A state change between two fetches writes
/// `github.state_changed` (a first fetch never counts); a close or merge
/// ends the item's active claims either way; a PR's complete closing
/// references replace its `closing_ref` links.
fn apply_item(conn: &Connection, repo: &str, number: i64, item: &GhItem, now: &str) -> Result<()> {
    let previous = get_item(conn, repo, number)?;
    let head_sha = match &previous {
        // The head is frozen at merge.
        Some(previous) if previous.state == "merged" && previous.synced_at.is_some() => {
            previous.head_sha.clone()
        }
        _ => item.head_sha.clone(),
    };
    let transition = previous
        .as_ref()
        .filter(|previous| previous.synced_at.is_some() && previous.state != item.state);
    let merge_check = match (transition, item.kind) {
        (Some(previous), WorkKind::Pr) if previous.state == "open" && item.state == "merged" => {
            Some("pending".to_owned())
        }
        _ => previous
            .as_ref()
            .and_then(|previous| previous.merge_check.clone()),
    };
    conn.execute(
        r#"
        INSERT INTO work_items
            (repo, number, kind, title, state, state_reason, url, head_ref, head_sha,
             closed_at, merged_at, synced_at, merge_check, sync_error)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, NULL)
        ON CONFLICT(repo, number) DO UPDATE SET
            kind = excluded.kind, title = excluded.title, state = excluded.state,
            state_reason = excluded.state_reason, url = excluded.url,
            head_ref = excluded.head_ref, head_sha = excluded.head_sha,
            closed_at = excluded.closed_at, merged_at = excluded.merged_at,
            synced_at = excluded.synced_at, merge_check = excluded.merge_check,
            sync_error = NULL
        "#,
        params![
            repo,
            number,
            item.kind.as_str(),
            item.title,
            item.state,
            item.state_reason,
            item.url,
            item.head_ref,
            head_sha,
            item.closed_at,
            item.merged_at,
            now,
            merge_check,
        ],
    )?;
    if item.kind == WorkKind::Pr {
        if let Some(refs) = &item.closing_refs {
            reconcile_closing_refs(conn, repo, number, refs, now)?;
        }
    }
    if let Some(previous) = transition {
        let (ticket, pr) = item_keys(conn, repo, item.kind, number)?;
        write_event(
            conn,
            "github.state_changed",
            None,
            Some(repo),
            ticket,
            pr,
            json!({"from": previous.state, "to": item.state, "state_reason": item.state_reason}),
            now,
        )?;
    }
    if item.state != "open" {
        let reason = if item.state == "merged" {
            "merged"
        } else {
            "closed"
        };
        let claims = query_claims(
            conn,
            "WHERE repo = ?1 AND number = ?2 AND ended_at IS NULL",
            params![repo, number],
        )?;
        for claim in &claims {
            end_claim(conn, claim, reason, None, now)?;
        }
    }
    Ok(())
}

fn set_sync_error(conn: &Connection, repo: &str, number: i64, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE work_items SET sync_error = ?3 WHERE repo = ?1 AND number = ?2",
        params![repo, number, error],
    )?;
    Ok(())
}

/// A never-fetched row (`synced_at` NULL); the next sync fills it in.
fn insert_stub(conn: &Connection, repo: &str, number: i64, kind: WorkKind) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO work_items (repo, number, kind, title, state, url)
         VALUES (?1, ?2, ?3, '', 'open', '')",
        params![repo, number, kind.as_str()],
    )?;
    Ok(())
}

struct NewClaim<'a> {
    repo: &'a str,
    number: i64,
    kind: WorkKind,
    session_id: &'a str,
    session_name: Option<&'a str>,
    parent_session_id: Option<&'a str>,
    source: ClaimSource,
    worktree_path: Option<&'a str>,
    branch: Option<&'a str>,
    claimed_at: &'a str,
    reserved: bool,
}

/// Inserts a claim row; `ended` inserts it already ended (backfill of a
/// retired session). Events are the caller's.
fn insert_claim_row(
    conn: &Connection,
    claim: &NewClaim<'_>,
    ended: Option<(&str, &str)>,
) -> Result<String> {
    for _ in 0..16 {
        let id = random_claim_id();
        if get_claim(conn, &id)?.is_some() {
            continue;
        }
        conn.execute(
            r#"
            INSERT INTO work_claims
                (id, repo, number, kind, session_id, session_name, parent_session_id, source,
                 worktree_path, branch, claimed_at, ended_at, end_reason, reserved_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
            "#,
            params![
                id,
                claim.repo,
                claim.number,
                claim.kind.as_str(),
                claim.session_id,
                claim.session_name,
                claim.parent_session_id,
                claim.source.as_str(),
                claim.worktree_path,
                claim.branch,
                claim.claimed_at,
                ended.map(|(at, _)| at),
                ended.map(|(_, reason)| reason),
                claim.reserved.then_some(claim.claimed_at),
            ],
        )?;
        return Ok(id);
    }
    bail!("could not allocate a unique claim id")
}

fn has_any_claim(conn: &Connection, repo: &str, number: i64, session_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM work_claims WHERE repo = ?1 AND number = ?2 AND session_id = ?3",
            params![repo, number, session_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

const CLAIM_COLUMNS: &str = "id, repo, number, kind, session_id, session_name, parent_session_id, \
     source, worktree_path, branch, claimed_at, ended_at, end_reason, ended_by_session_id, \
     nudged_idle_at, managed_worktree, base_sha, reserved_at, check_b_due_at";

fn claim_from_row(row: &Row<'_>) -> rusqlite::Result<WorkClaim> {
    Ok(WorkClaim {
        id: row.get(0)?,
        repo: row.get(1)?,
        number: row.get(2)?,
        kind: row.get(3)?,
        session_id: row.get(4)?,
        session_name: row.get(5)?,
        parent_session_id: row.get(6)?,
        source: row.get(7)?,
        worktree_path: row.get(8)?,
        branch: row.get(9)?,
        claimed_at: row.get(10)?,
        ended_at: row.get(11)?,
        end_reason: row.get(12)?,
        ended_by_session_id: row.get(13)?,
        nudged_idle_at: row.get(14)?,
        managed_worktree: row.get::<_, i64>(15)? != 0,
        base_sha: row.get(16)?,
        reserved_at: row.get(17)?,
        check_b_due_at: row.get(18)?,
    })
}

fn query_claims(
    conn: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<WorkClaim>> {
    let mut statement =
        conn.prepare(&format!("SELECT {CLAIM_COLUMNS} FROM work_claims {filter}"))?;
    let rows = statement
        .query_map(params, claim_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn get_claim(conn: &Connection, id: &str) -> Result<Option<WorkClaim>> {
    Ok(query_claims(conn, "WHERE id = ?1", params![id])?
        .into_iter()
        .next())
}

const ITEM_COLUMNS: &str = "repo, number, kind, title, state, state_reason, url, head_ref, \
     head_sha, closed_at, merged_at, synced_at, merge_check, sync_error";

fn item_from_row(row: &Row<'_>) -> rusqlite::Result<WorkItem> {
    Ok(WorkItem {
        repo: row.get(0)?,
        number: row.get(1)?,
        kind: row.get(2)?,
        title: row.get(3)?,
        state: row.get(4)?,
        state_reason: row.get(5)?,
        url: row.get(6)?,
        head_ref: row.get(7)?,
        head_sha: row.get(8)?,
        closed_at: row.get(9)?,
        merged_at: row.get(10)?,
        synced_at: row.get(11)?,
        merge_check: row.get(12)?,
        sync_error: row.get(13)?,
    })
}

fn get_item(conn: &Connection, repo: &str, number: i64) -> Result<Option<WorkItem>> {
    Ok(conn
        .query_row(
            &format!("SELECT {ITEM_COLUMNS} FROM work_items WHERE repo = ?1 AND number = ?2"),
            params![repo, number],
            item_from_row,
        )
        .optional()?)
}

fn claim_views(conn: &Connection, claims: Vec<WorkClaim>) -> Result<Vec<ClaimView>> {
    claims
        .into_iter()
        .map(|claim| {
            let item = get_item(conn, &claim.repo, claim.number)?;
            let history_path = history_path(&claim.repo, claim.number);
            Ok(ClaimView {
                title: item.as_ref().map(|i| i.title.clone()).unwrap_or_default(),
                state: item
                    .as_ref()
                    .map(|i| i.state.clone())
                    .unwrap_or_else(|| "open".to_owned()),
                url: item.map(|i| i.url).unwrap_or_default(),
                history_path,
                claim,
            })
        })
        .collect()
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn max_rowid(conn: &Connection, table: &str) -> Result<i64> {
    if !table_exists(conn, table)? {
        return Ok(0);
    }
    Ok(conn.query_row(
        &format!("SELECT IFNULL(MAX(rowid), 0) FROM {table}"),
        [],
        |row| row.get(0),
    )?)
}

fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM work_claims_meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO work_claims_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn random_claim_id() -> String {
    let mut bytes = [0u8; 4];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn rfc3339_before(age: Duration) -> String {
    (OffsetDateTime::now_utc() - age)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

mod checks;
pub use checks::{IdleSession, MERGE_SETTLE};

#[cfg(test)]
mod tests;
