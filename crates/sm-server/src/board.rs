//! sm board (sm#1665, ticket #1681): ticket order read from GitHub's own
//! links, lanes the owner ranks, and each ticket's state.
//!
//! GitHub is the only record of order. sm reads every repo on the board
//! each pass (appendix C), keeps the lanes and their ranks, and recomputes
//! the model (appendix D) after every pass and every lane or link change.
//! The tables live in the claims database. Spec:
//! `docs/working/1665_sm_board.html`, appendices B–F.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use time::OffsetDateTime;

use crate::owner_push::{format_ts, parse_ts};
use crate::work_claims::{canonical_repo, HolderState, SessionDirectory};

pub mod auto_start;
pub mod clock;
pub mod model;
pub mod pushes;
pub mod sync;

use model::{
    Board, Edge, EdgeKind, Holder, Item, Key, Lane, LaneView, Member, ModelInput, PrRef,
    TicketState, WaitingRecord,
};
use sync::{BoardSource, LinkMutation, RefNode, WriteError};

/// Events older than this are deleted at the end of each read pass.
pub const EVENT_RETENTION: time::Duration = time::Duration::days(90);
/// How many events a lane's recent-changes list shows.
pub const CHANGES_SHOWN: usize = 10;
/// Notice kinds the board creates (appendix I).
pub const NOTICE_BOARD_READY: &str = "board_ready";
pub const NOTICE_BOARD_LANE_DONE: &str = "board_lane_done";
pub const NOTICE_BOARD_AUTO_START: &str = "board_auto_start";
/// `board_settings` key of the standing Bugs ticket, `owner/name#N`.
pub const BUGS_GOAL_SETTING: &str = "bugs_goal";

pub fn init_board_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS board_lanes (
            id INTEGER PRIMARY KEY,
            goal_repo TEXT NOT NULL,
            goal_number INTEGER NOT NULL,
            rank INTEGER,
            added_at TEXT NOT NULL,
            added_by TEXT NOT NULL,
            added_by_name TEXT NOT NULL,
            ended_at TEXT,
            end_reason TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS board_lanes_active
            ON board_lanes(goal_repo, goal_number) WHERE ended_at IS NULL;
        CREATE TABLE IF NOT EXISTS board_items (
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            state TEXT NOT NULL,
            state_reason TEXT,
            closed_at TEXT,
            updated_at TEXT,
            first_seen_at TEXT NOT NULL,
            synced_at TEXT NOT NULL,
            PRIMARY KEY (repo, number)
        );
        CREATE TABLE IF NOT EXISTS board_edges (
            waiter_repo TEXT NOT NULL,
            waiter_number INTEGER NOT NULL,
            blocker_repo TEXT NOT NULL,
            blocker_number INTEGER NOT NULL,
            kind TEXT NOT NULL,
            first_seen_at TEXT NOT NULL,
            source TEXT NOT NULL,
            PRIMARY KEY (waiter_repo, waiter_number, blocker_repo, blocker_number, kind)
        );
        CREATE INDEX IF NOT EXISTS board_edges_blocker
            ON board_edges(blocker_repo, blocker_number);
        CREATE TABLE IF NOT EXISTS board_prs (
            repo TEXT NOT NULL,
            issue_number INTEGER NOT NULL,
            pr_repo TEXT NOT NULL,
            pr_number INTEGER NOT NULL,
            pr_state TEXT,
            url TEXT,
            PRIMARY KEY (repo, issue_number, pr_repo, pr_number)
        );
        CREATE TABLE IF NOT EXISTS board_members (
            lane_id INTEGER NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            state TEXT,
            joined_at TEXT NOT NULL,
            PRIMARY KEY (lane_id, repo, number)
        );
        CREATE TABLE IF NOT EXISTS board_ticket_ranks (
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            rank INTEGER NOT NULL,
            lane_id INTEGER NOT NULL,
            PRIMARY KEY (repo, number)
        );
        CREATE TABLE IF NOT EXISTS board_events (
            id INTEGER PRIMARY KEY,
            ts TEXT NOT NULL,
            kind TEXT NOT NULL,
            lane_id INTEGER,
            repo TEXT,
            number INTEGER,
            other_repo TEXT,
            other_number INTEGER,
            actor TEXT,
            actor_name TEXT,
            detail TEXT
        );
        CREATE INDEX IF NOT EXISTS board_events_lane ON board_events(lane_id, id);
        CREATE TABLE IF NOT EXISTS board_repo_sync (
            repo TEXT PRIMARY KEY,
            last_ok_at TEXT,
            last_error TEXT,
            last_error_at TEXT,
            rate_limited_until TEXT
        );
        CREATE TABLE IF NOT EXISTS board_seen (
            user_id TEXT PRIMARY KEY,
            seen_at TEXT NOT NULL,
            -- The newest board event when the owner looked: ids order events
            -- that share a second.
            seen_event_id INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS board_started_early (
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            started_early_at TEXT NOT NULL,
            PRIMARY KEY (repo, number)
        );
        CREATE TABLE IF NOT EXISTS auto_starts (
            repo TEXT NOT NULL, number INTEGER NOT NULL, agent_type TEXT,
            provider TEXT NOT NULL, model TEXT, effort TEXT, brief TEXT,
            state TEXT NOT NULL CHECK (state IN ('waiting','started','failed','cancelled')),
            attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, session_id TEXT,
            authorized_at TEXT NOT NULL, updated_at TEXT NOT NULL,
            PRIMARY KEY (repo, number)
        );
        CREATE TABLE IF NOT EXISTS board_waiting (
            repo TEXT NOT NULL, number INTEGER NOT NULL, text TEXT NOT NULL,
            url TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(repo, number)
        );
        CREATE TABLE IF NOT EXISTS board_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )?;
    if conn
        .prepare("SELECT tier FROM board_items LIMIT 0")
        .is_err()
    {
        conn.execute("ALTER TABLE board_items ADD COLUMN tier TEXT", [])?;
    }
    Ok(())
}

/// Parses a ticket reference: `1654`, `#1654`, `name#1654` (with
/// `default_repo`'s owner) or `owner/name#1654`.
pub fn parse_ticket_ref(text: &str, default_repo: &str) -> Option<Key> {
    let text = text.trim();
    let (repo, number) = match text.rsplit_once('#') {
        Some((repo, number)) => (repo, number),
        None => ("", text),
    };
    let number: i64 = number.parse().ok().filter(|n| *n > 0)?;
    let repo = if repo.is_empty() {
        default_repo.to_owned()
    } else if repo.contains('/') {
        repo.to_owned()
    } else {
        let owner = default_repo.split_once('/')?.0;
        format!("{owner}/{repo}")
    };
    let repo = canonical_repo(&repo);
    let (owner, name) = repo.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/')).then_some((repo, number))
}

/// What the board reads from outside its own tables.
#[derive(Debug, Clone, Default)]
pub struct Outside {
    pub sessions: SessionDirectory,
    pub waiting: Vec<WaitingRecord>,
    /// How the owner is named in the board's texts.
    pub owner_name: String,
    /// `board.repos`, canonical.
    pub config_repos: Vec<String>,
}

/// A refusal the routes return as a status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotFound(String),
    Unprocessable(String),
    /// With the lane id the conflict is about, when there is one.
    Conflict(String, Option<i64>),
    Unreachable(String),
}

impl Refusal {
    pub fn detail(&self) -> &str {
        match self {
            Self::NotFound(detail)
            | Self::Unprocessable(detail)
            | Self::Conflict(detail, _)
            | Self::Unreachable(detail) => detail,
        }
    }
}

/// A link write's result (C5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    Recorded,
    Already,
    Removed,
    Absent,
}

impl LinkOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Already => "already",
            Self::Removed => "removed",
            Self::Absent => "absent",
        }
    }
}

/// `POST /board/links`: `ticket` starts after `target` (`After`), or sits
/// under `target` (`SubIssue`).
#[derive(Debug, Clone)]
pub struct LinkRequest {
    pub ticket: Key,
    pub target: Key,
    pub kind: EdgeKind,
    pub remove: bool,
    /// `sm:<session id>` or `sm:owner`.
    pub actor: String,
    pub actor_name: String,
}

/// The line a link write prints (appendix E).
pub fn link_message(request: &LinkRequest, outcome: LinkOutcome) -> String {
    let base = &request.ticket.0;
    let a = model::short_ref(&request.ticket, base);
    let b = model::short_ref(&request.target, base);
    match (request.kind, outcome) {
        (EdgeKind::After, LinkOutcome::Recorded) => format!("Recorded: {a} starts after {b}."),
        (EdgeKind::After, LinkOutcome::Already) => {
            format!("Already recorded: {a} starts after {b}.")
        }
        (EdgeKind::After, LinkOutcome::Removed) => {
            format!("Removed: {a} no longer starts after {b}.")
        }
        (EdgeKind::After, LinkOutcome::Absent) => {
            format!("Not linked: {a} does not start after {b}.")
        }
        (EdgeKind::SubIssue, LinkOutcome::Recorded) => format!("Recorded: {a} is under {b}."),
        (EdgeKind::SubIssue, LinkOutcome::Already) => {
            format!("Already recorded: {a} is under {b}.")
        }
        (EdgeKind::SubIssue, LinkOutcome::Removed) => {
            format!("Removed: {a} is no longer under {b}.")
        }
        (EdgeKind::SubIssue, LinkOutcome::Absent) => format!("Not linked: {a} is not under {b}."),
    }
}

/// A `board_repo_sync` row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoSync {
    pub repo: String,
    pub last_ok_at: Option<String>,
    pub last_error: Option<String>,
    pub last_error_at: Option<String>,
    pub rate_limited_until: Option<String>,
}

impl RepoSync {
    /// The repo's latest read failed (C4): a good read clears the error.
    pub fn stale(&self) -> bool {
        self.last_error_at.is_some()
    }
}

/// A `board_events` row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Event {
    pub id: i64,
    pub ts: String,
    pub kind: String,
    pub lane_id: Option<i64>,
    pub ticket: Option<Key>,
    pub other: Option<Key>,
    pub actor: Option<String>,
    pub actor_name: Option<String>,
    pub detail: Option<String>,
}

/// A ticket's place in the queue (appendix H): the best-ranked active lane
/// that contains it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketRank {
    pub rank: i64,
    pub lane_id: i64,
    pub goal: Key,
    pub goal_title: String,
}

/// A member whose state changed at a recompute: `from` is `None` for a
/// ticket that joined, `to` is `None` for one that left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub lane_id: i64,
    pub ticket: Key,
    pub from: Option<TicketState>,
    pub to: Option<TicketState>,
}

/// What a recompute found and wrote.
#[derive(Debug, Clone, Default)]
pub struct Recomputed {
    pub board: Board,
    pub input: ModelInput,
    pub transitions: Vec<Transition>,
    /// Lanes this recompute ended because their goal closed, with the
    /// `lane_ended` event id.
    pub ended: Vec<(Lane, i64)>,
    /// Lanes recomputed for the first time.
    pub first_seen: BTreeSet<i64>,
}

#[derive(Debug, Clone)]
pub struct BoardStore {
    db_path: PathBuf,
}

impl BoardStore {
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
        crate::work_claims::init_work_claims_schema(&conn)?;
        Ok(conn)
    }

    /// A read connection; `None` before the database or the tables exist.
    fn open_read(&self) -> Result<Option<Connection>> {
        if !self.db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let has_tables = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'board_seen'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(has_tables.then_some(conn))
    }

    pub fn ensure_schema(&self) -> Result<()> {
        self.open_write().map(|_| ())
    }

    /// Set or clear a durable mark. The route resolves the ticket before setting it.
    pub fn set_waiting(
        &self,
        key: &Key,
        mark: Option<(&RefNode, &str, &str)>,
        now: OffsetDateTime,
    ) -> Result<()> {
        let mut conn = self.open_write()?;
        let tx = conn.transaction()?;
        if let Some((node, text, url)) = mark {
            upsert_ref(&tx, node, &format_ts(now))?;
            tx.execute("INSERT OR REPLACE INTO board_waiting(repo, number, text, url, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![key.0, key.1, text, url, format_ts(now)])?;
        } else {
            tx.execute(
                "DELETE FROM board_waiting WHERE repo = ?1 AND number = ?2",
                params![key.0, key.1],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The standing Bugs ticket, if sm has made one.
    pub fn bugs_goal(&self) -> Result<Option<Key>> {
        match self.open_read()? {
            Some(conn) if table_exists(&conn, "board_settings")? => bugs_goal(&conn),
            _ => Ok(None),
        }
    }

    pub fn set_bugs_goal(&self, goal: &Key) -> Result<()> {
        let conn = self.open_write()?;
        conn.execute(
            "INSERT INTO board_settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![BUGS_GOAL_SETTING, format!("{}#{}", goal.0, goal.1)],
        )?;
        Ok(())
    }

    pub fn record_started_early(&self, key: &Key, now: OffsetDateTime) -> Result<()> {
        let conn = self.open_write()?;
        conn.execute("INSERT OR REPLACE INTO board_started_early(repo, number, started_early_at) VALUES (?1, ?2, ?3)",
            params![key.0, key.1, format_ts(now)])?;
        Ok(())
    }

    pub fn started_early(&self) -> Result<BTreeSet<Key>> {
        let Some(conn) = self.open_read()? else {
            return Ok(BTreeSet::new());
        };
        let mut statement = match conn.prepare("SELECT repo, number FROM board_started_early") {
            Ok(statement) => statement,
            Err(rusqlite::Error::SqliteFailure(_, Some(detail)))
                if detail.contains("no such table") =>
            {
                return Ok(BTreeSet::new());
            }
            Err(error) => return Err(error.into()),
        };
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        Ok(rows)
    }

    /// The model input as the tables hold it now.
    pub fn input(&self, outside: &Outside) -> Result<ModelInput> {
        match self.open_read()? {
            Some(conn) => load_input(&conn, outside),
            None => Ok(ModelInput {
                read_repos: outside.config_repos.iter().cloned().collect(),
                ..ModelInput::default()
            }),
        }
    }

    /// The board computed from the tables, without writing anything.
    pub fn board(&self, outside: &Outside, now: OffsetDateTime) -> Result<(Board, ModelInput)> {
        let input = self.input(outside)?;
        Ok((model::compute(&input, now), input))
    }

    pub fn repo_syncs(&self) -> Result<Vec<RepoSync>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        repo_syncs(&conn)
    }

    pub fn events(&self, limit: usize) -> Result<Vec<Event>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM board_events ORDER BY id DESC LIMIT ?1"
        ))?;
        let rows = statement
            .query_map(params![limit as i64], event_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn lane(&self, id: i64) -> Result<Option<(Lane, Option<String>)>> {
        let Some(conn) = self.open_read()? else {
            return Ok(None);
        };
        Ok(conn
            .query_row(
                "SELECT id, goal_repo, goal_number, IFNULL(rank, 0), added_at, added_by,
                        added_by_name, ended_at
                 FROM board_lanes WHERE id = ?1",
                params![id],
                |row| Ok((lane_from_row(row)?, row.get(7)?)),
            )
            .optional()?)
    }

    pub fn active_lanes(&self) -> Result<Vec<Lane>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        active_lanes(&conn)
    }

    /// When the owner last opened the board, and the newest board event
    /// id at that moment.
    pub fn seen(&self, user_id: &str) -> Result<Option<(String, i64)>> {
        let Some(conn) = self.open_read()? else {
            return Ok(None);
        };
        Ok(conn
            .query_row(
                "SELECT seen_at, seen_event_id FROM board_seen WHERE user_id = ?1",
                params![user_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    pub fn set_seen(&self, user_id: &str, now: OffsetDateTime) -> Result<()> {
        self.open_write()?.execute(
            "INSERT INTO board_seen (user_id, seen_at, seen_event_id)
             VALUES (?1, ?2, (SELECT IFNULL(MAX(id), 0) FROM board_events))
             ON CONFLICT(user_id) DO UPDATE SET seen_at = excluded.seen_at,
                 seen_event_id = excluded.seen_event_id",
            params![user_id, format_ts(now)],
        )?;
        Ok(())
    }

    /// The latest `became_needs_you` event id per ticket.
    pub fn needs_you_since(&self) -> Result<BTreeMap<Key, i64>> {
        let Some(conn) = self.open_read()? else {
            return Ok(BTreeMap::new());
        };
        let mut statement = conn.prepare(
            "SELECT repo, number, MAX(id) FROM board_events
             WHERE kind = 'became_needs_you' GROUP BY repo, number",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    (row.get::<_, String>(0)?, row.get::<_, i64>(1)?),
                    row.get(2)?,
                ))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        Ok(rows)
    }

    /// `board_ticket_ranks`, with each lane's goal.
    pub fn ticket_ranks(&self) -> Result<BTreeMap<Key, TicketRank>> {
        let Some(conn) = self.open_read()? else {
            return Ok(BTreeMap::new());
        };
        let mut statement = conn.prepare(
            "SELECT r.repo, r.number, r.rank, r.lane_id, l.goal_repo, l.goal_number,
                    IFNULL(i.title, '')
             FROM board_ticket_ranks r
             JOIN board_lanes l ON l.id = r.lane_id
             LEFT JOIN board_items i ON i.repo = l.goal_repo AND i.number = l.goal_number",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    (row.get::<_, String>(0)?, row.get::<_, i64>(1)?),
                    TicketRank {
                        rank: row.get(2)?,
                        lane_id: row.get(3)?,
                        goal: (row.get(4)?, row.get(5)?),
                        goal_title: row.get(6)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        Ok(rows)
    }

    /// Whether every pass is paused for the rate limit (C4).
    pub fn rate_limited(&self, now: OffsetDateTime) -> Result<bool> {
        Ok(self.repo_syncs()?.iter().any(|sync| {
            sync.rate_limited_until
                .as_deref()
                .and_then(parse_ts)
                .is_some_and(|until| until > now)
        }))
    }

    /// The repos a pass reads (C1): active lanes' goal repos, the configured
    /// repos, and the repos of open members at the last recompute.
    pub fn read_set(&self, outside: &Outside) -> Result<BTreeSet<String>> {
        let mut repos: BTreeSet<String> = outside.config_repos.iter().cloned().collect();
        let Some(conn) = self.open_read()? else {
            return Ok(repos);
        };
        for lane in active_lanes(&conn)? {
            repos.insert(lane.goal.0);
        }
        let mut statement = conn.prepare(
            "SELECT DISTINCT m.repo FROM board_members m
             JOIN board_lanes l ON l.id = m.lane_id AND l.ended_at IS NULL
             JOIN board_items i ON i.repo = m.repo AND i.number = m.number
             WHERE i.state = 'open'",
        )?;
        for repo in statement.query_map([], |row| row.get::<_, String>(0))? {
            repos.insert(repo?);
        }
        Ok(repos)
    }

    /// Open tickets the tables hold for `repo`.
    pub fn open_numbers(&self, repo: &str) -> Result<Vec<i64>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(
            "SELECT number FROM board_items WHERE repo = ?1 AND state = 'open' ORDER BY number",
        )?;
        let rows = statement
            .query_map(params![repo], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// C3 steps 1–4 for one repo, in one transaction.
    pub fn apply_repo_read(
        &self,
        repo: &str,
        issues: &[sync::IssueNode],
        missing: &BTreeMap<i64, Option<RefNode>>,
        now: OffsetDateTime,
    ) -> Result<()> {
        let repo = canonical_repo(repo);
        let now = format_ts(now);
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for issue in issues {
            for node in issue
                .parent
                .iter()
                .chain(&issue.blocked_by)
                .chain(&issue.sub_issues)
            {
                upsert_ref(&tx, node, &now)?;
            }
        }
        for issue in issues {
            let key = (repo.clone(), issue.number);
            upsert_ref(
                &tx,
                &RefNode {
                    repo: repo.clone(),
                    number: issue.number,
                    title: issue.title.clone(),
                    url: issue.url.clone(),
                    state: "open".to_owned(),
                    state_reason: issue.state_reason.clone(),
                    closed_at: None,
                },
                &now,
            )?;
            if let Some(updated_at) = &issue.updated_at {
                tx.execute(
                    "UPDATE board_items SET updated_at = ?3 WHERE repo = ?1 AND number = ?2",
                    params![repo, issue.number, updated_at],
                )?;
            }
            tx.execute(
                "UPDATE board_items SET tier = ?3 WHERE repo = ?1 AND number = ?2",
                params![
                    repo,
                    issue.number,
                    issue
                        .body
                        .as_deref()
                        .and_then(crate::owner_settings::ticket_tier)
                ],
            )?;
            let outgoing: Vec<(Key, EdgeKind)> = issue
                .blocked_by
                .iter()
                .map(|node| (node.key(), EdgeKind::After))
                .chain(
                    issue
                        .sub_issues
                        .iter()
                        .map(|node| (node.key(), EdgeKind::SubIssue)),
                )
                .collect();
            replace_outgoing(&tx, &key, &outgoing, &now)?;
            replace_parent(&tx, &key, issue.parent.as_ref().map(RefNode::key), &now)?;
            tx.execute(
                "DELETE FROM board_prs WHERE repo = ?1 AND issue_number = ?2",
                params![repo, issue.number],
            )?;
            for pr in &issue.prs {
                tx.execute(
                    "INSERT OR REPLACE INTO board_prs
                     (repo, issue_number, pr_repo, pr_number, pr_state, url)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![repo, issue.number, pr.repo, pr.number, pr.state, pr.url],
                )?;
            }
        }
        for (number, node) in missing {
            match node {
                Some(node) if node.state == "open" => upsert_ref(&tx, node, &now)?,
                Some(node) => upsert_ref(&tx, node, &now)?,
                None => {
                    tx.execute(
                        "UPDATE board_items SET state = 'closed', state_reason = 'missing',
                         closed_at = IFNULL(closed_at, ?3), synced_at = ?3
                         WHERE repo = ?1 AND number = ?2",
                        params![repo, number, now],
                    )?;
                }
            }
            let closed = node.as_ref().is_none_or(|node| node.state != "open");
            if closed {
                tx.execute(
                    "DELETE FROM board_waiting WHERE repo = ?1 AND number = ?2",
                    params![repo, number],
                )?;
                tx.execute(
                    "DELETE FROM board_edges WHERE waiter_repo = ?1 AND waiter_number = ?2",
                    params![repo, number],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO board_repo_sync (repo, last_ok_at) VALUES (?1, ?2)
             ON CONFLICT(repo) DO UPDATE SET last_ok_at = excluded.last_ok_at,
                 last_error = NULL, last_error_at = NULL",
            params![repo, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// C4: a failed read keeps the rows and marks the repo stale.
    pub fn mark_repo_failed(&self, repo: &str, error: &str, now: OffsetDateTime) -> Result<()> {
        let now = format_ts(now);
        let conn = self.open_write()?;
        conn.execute(
            "INSERT INTO board_repo_sync (repo, last_error, last_error_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(repo) DO UPDATE SET last_error = excluded.last_error,
                 last_error_at = excluded.last_error_at",
            params![repo, error, now],
        )?;
        insert_event(
            &conn,
            &now,
            "sync_failed",
            None,
            None,
            None,
            None,
            Some(&format!("{repo}: {error}")),
        )?;
        Ok(())
    }

    /// C4: every repo waits for `until`.
    pub fn set_rate_limited(&self, repos: &BTreeSet<String>, until: &str) -> Result<()> {
        let conn = self.open_write()?;
        for repo in repos {
            conn.execute(
                "INSERT INTO board_repo_sync (repo, rate_limited_until) VALUES (?1, ?2)
                 ON CONFLICT(repo) DO UPDATE SET rate_limited_until = excluded.rate_limited_until",
                params![repo, until],
            )?;
        }
        Ok(())
    }

    pub fn prune_events(&self, now: OffsetDateTime) -> Result<usize> {
        Ok(self.open_write()?.execute(
            "DELETE FROM board_events WHERE ts < ?1",
            params![format_ts(now - EVENT_RETENTION)],
        )?)
    }

    pub fn upsert_nodes(&self, nodes: &[&RefNode], now: OffsetDateTime) -> Result<()> {
        let now = format_ts(now);
        let conn = self.open_write()?;
        for node in nodes {
            upsert_ref(&conn, node, &now)?;
        }
        Ok(())
    }

    /// C5 step 5 (and the already/absent cases): the edge as GitHub now has
    /// it, and the event.
    pub fn record_link(
        &self,
        request: &LinkRequest,
        outcome: LinkOutcome,
        now: OffsetDateTime,
    ) -> Result<()> {
        let now = format_ts(now);
        let (waiter, blocker) = match request.kind {
            EdgeKind::After => (&request.ticket, &request.target),
            EdgeKind::SubIssue => (&request.target, &request.ticket),
        };
        let conn = self.open_write()?;
        match outcome {
            LinkOutcome::Recorded | LinkOutcome::Already => {
                let source = if outcome == LinkOutcome::Recorded {
                    request.actor.as_str()
                } else {
                    "github"
                };
                conn.execute(
                    "INSERT OR IGNORE INTO board_edges
                     (waiter_repo, waiter_number, blocker_repo, blocker_number, kind,
                      first_seen_at, source)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        waiter.0,
                        waiter.1,
                        blocker.0,
                        blocker.1,
                        request.kind.as_str(),
                        now,
                        source
                    ],
                )?;
            }
            LinkOutcome::Removed | LinkOutcome::Absent => {
                conn.execute(
                    "DELETE FROM board_edges WHERE waiter_repo = ?1 AND waiter_number = ?2
                     AND blocker_repo = ?3 AND blocker_number = ?4 AND kind = ?5",
                    params![
                        waiter.0,
                        waiter.1,
                        blocker.0,
                        blocker.1,
                        request.kind.as_str()
                    ],
                )?;
            }
        }
        let kind = match outcome {
            LinkOutcome::Recorded => "link_added",
            LinkOutcome::Removed => "link_removed",
            LinkOutcome::Already | LinkOutcome::Absent => return Ok(()),
        };
        insert_event(
            &conn,
            &now,
            kind,
            None,
            Some(&request.ticket),
            Some(&request.target),
            Some((&request.actor, &request.actor_name)),
            Some(request.kind.as_str()),
        )?;
        Ok(())
    }

    /// D4: a new lane at the bottom. The goal must be in `board_items`
    /// and open (the caller has just read it).
    pub fn add_lane(
        &self,
        goal: &Key,
        added_by: &str,
        added_by_name: &str,
        now: OffsetDateTime,
    ) -> Result<std::result::Result<i64, Refusal>> {
        let now = format_ts(now);
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(i64, i64)> = tx
            .query_row(
                "SELECT id, rank FROM board_lanes
                 WHERE goal_repo = ?1 AND goal_number = ?2 AND ended_at IS NULL",
                params![goal.0, goal.1],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((id, rank)) = existing {
            return Ok(Err(Refusal::Conflict(
                format!("Lane {rank} already has goal {}#{}", goal.0, goal.1),
                Some(id),
            )));
        }
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM board_lanes WHERE ended_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO board_lanes (goal_repo, goal_number, rank, added_at, added_by, added_by_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![goal.0, goal.1, count + 1, now, added_by, added_by_name],
        )?;
        let id = tx.last_insert_rowid();
        insert_event(
            &tx,
            &now,
            "lane_added",
            Some(id),
            Some(goal),
            None,
            Some((added_by, added_by_name)),
            None,
        )?;
        tx.commit()?;
        Ok(Ok(id))
    }

    /// An owner End (`owner`); the pass ends goal-closed lanes itself.
    pub fn end_lane(&self, id: i64, now: OffsetDateTime) -> Result<bool> {
        let now = format_ts(now);
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ended = end_lane_tx(&tx, id, "owner", &now)?.is_some();
        tx.commit()?;
        Ok(ended)
    }

    /// `PUT /client/board/order`: the full new order of active lanes.
    /// `Err` when the set differs from the active set.
    pub fn reorder(
        &self,
        lane_ids: &[i64],
        actor_name: &str,
        now: OffsetDateTime,
    ) -> Result<std::result::Result<(), Refusal>> {
        let now = format_ts(now);
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active = active_lanes(&tx)?;
        let active_ids: BTreeSet<i64> = active.iter().map(|lane| lane.id).collect();
        let given: BTreeSet<i64> = lane_ids.iter().copied().collect();
        if given != active_ids || given.len() != lane_ids.len() {
            return Ok(Err(Refusal::Conflict(
                "the order must list every active lane exactly once".to_owned(),
                None,
            )));
        }
        for (index, id) in lane_ids.iter().enumerate() {
            let rank = index as i64 + 1;
            let old = active.iter().find(|lane| lane.id == *id).map(|l| l.rank);
            if old != Some(rank) {
                tx.execute(
                    "UPDATE board_lanes SET rank = ?2 WHERE id = ?1",
                    params![id, rank],
                )?;
                insert_event(
                    &tx,
                    &now,
                    "lane_moved",
                    Some(*id),
                    None,
                    None,
                    Some(("owner", actor_name)),
                    Some(&rank.to_string()),
                )?;
            }
        }
        rewrite_ticket_ranks(&tx)?;
        tx.commit()?;
        Ok(Ok(()))
    }

    /// C3 step 5: ends lanes whose goal closed, records events, and rewrites
    /// `board_members` and `board_ticket_ranks`.
    pub fn recompute(&self, outside: &Outside, now: OffsetDateTime) -> Result<Recomputed> {
        let ts = format_ts(now);
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let input = load_input(&tx, outside)?;
        let board = model::compute(&input, now);
        let mut result = Recomputed::default();
        for lane in &board.goal_closed {
            if let Some(event) = end_lane_tx(&tx, lane.id, "goal_closed", &ts)? {
                result.ended.push((lane.clone(), event));
            }
        }
        let names = |source: &str| -> (String, String) {
            match source.strip_prefix("sm:") {
                Some("owner") => (source.to_owned(), outside.owner_name.clone()),
                Some(id) => (
                    source.to_owned(),
                    outside
                        .sessions
                        .get(id)
                        .map(|session| session.name.clone())
                        .unwrap_or_else(|| id.to_owned()),
                ),
                None => ("github".to_owned(), "GitHub".to_owned()),
            }
        };
        for view in &board.lanes {
            let lane_id = view.lane.id;
            let previous = input.members.get(&lane_id);
            if previous.is_none() {
                result.first_seen.insert(lane_id);
            }
            let members: BTreeSet<&Key> = view.rows.iter().map(|row| &row.key).collect();
            for row in &view.rows {
                let facts = &board.facts[&row.key];
                let before = previous.and_then(|members| members.get(&row.key));
                let from = before.map(|member| member.state);
                if before.is_none() && previous.is_some() {
                    let source = input
                        .edges
                        .iter()
                        .filter(|edge| edge.blocker == row.key && members.contains(&edge.waiter))
                        .map(|edge| edge.source.as_str())
                        .find(|source| source.starts_with("sm:"))
                        .unwrap_or("github");
                    let (actor, actor_name) = names(source);
                    insert_event(
                        &tx,
                        &ts,
                        "ticket_joined",
                        Some(lane_id),
                        Some(&row.key),
                        None,
                        Some((&actor, &actor_name)),
                        None,
                    )?;
                }
                if from != Some(facts.state) {
                    let kind = match facts.state {
                        TicketState::Ready if from.is_some() => Some("became_ready"),
                        TicketState::NeedsYou => Some("became_needs_you"),
                        TicketState::Done if from.is_some() => Some("ticket_closed"),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        let detail = facts.needs_you.as_ref().map(|needs| needs.text.clone());
                        insert_event(
                            &tx,
                            &ts,
                            kind,
                            Some(lane_id),
                            Some(&row.key),
                            None,
                            None,
                            detail.as_deref(),
                        )?;
                    }
                    result.transitions.push(Transition {
                        lane_id,
                        ticket: row.key.clone(),
                        from,
                        to: Some(facts.state),
                    });
                }
            }
            for (key, member) in previous.into_iter().flatten() {
                if !members.contains(key) {
                    insert_event(
                        &tx,
                        &ts,
                        "ticket_left",
                        Some(lane_id),
                        Some(key),
                        None,
                        None,
                        None,
                    )?;
                    result.transitions.push(Transition {
                        lane_id,
                        ticket: key.clone(),
                        from: Some(member.state),
                        to: None,
                    });
                }
            }
            for cycle in &view.cycles {
                let detail = cycle_detail(cycle);
                let known = tx
                    .query_row(
                        "SELECT 1 FROM board_events
                         WHERE kind = 'cycle_found' AND lane_id = ?1 AND detail = ?2",
                        params![lane_id, detail],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !known {
                    insert_event(
                        &tx,
                        &ts,
                        "cycle_found",
                        Some(lane_id),
                        cycle.first(),
                        cycle.get(1),
                        None,
                        Some(&detail),
                    )?;
                }
            }
            tx.execute(
                "DELETE FROM board_members WHERE lane_id = ?1",
                params![lane_id],
            )?;
            for row in &view.rows {
                tx.execute(
                    "INSERT INTO board_members (lane_id, repo, number, state, joined_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        lane_id,
                        row.key.0,
                        row.key.1,
                        board.facts[&row.key].state.as_str(),
                        row.joined_at
                    ],
                )?;
            }
        }
        tx.execute(
            "DELETE FROM board_members
             WHERE lane_id NOT IN (SELECT id FROM board_lanes WHERE ended_at IS NULL)",
            [],
        )?;
        rewrite_ticket_ranks(&tx)?;
        tx.commit()?;
        result.board = board;
        result.input = input;
        Ok(result)
    }
}

fn cycle_detail(cycle: &[Key]) -> String {
    cycle
        .iter()
        .map(|(repo, number)| format!("{repo}#{number}"))
        .collect::<Vec<_>>()
        .join(" ")
}

const EVENT_COLUMNS: &str =
    "id, ts, kind, lane_id, repo, number, other_repo, other_number, actor, actor_name, detail";

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let key = |repo: Option<String>, number: Option<i64>| repo.zip(number);
    Ok(Event {
        id: row.get(0)?,
        ts: row.get(1)?,
        kind: row.get(2)?,
        lane_id: row.get(3)?,
        ticket: key(row.get(4)?, row.get(5)?),
        other: key(row.get(6)?, row.get(7)?),
        actor: row.get(8)?,
        actor_name: row.get(9)?,
        detail: row.get(10)?,
    })
}

#[allow(clippy::too_many_arguments)]
fn insert_event(
    conn: &Connection,
    ts: &str,
    kind: &str,
    lane_id: Option<i64>,
    ticket: Option<&Key>,
    other: Option<&Key>,
    actor: Option<(&str, &str)>,
    detail: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO board_events
         (ts, kind, lane_id, repo, number, other_repo, other_number, actor, actor_name, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            ts,
            kind,
            lane_id,
            ticket.map(|key| &key.0),
            ticket.map(|key| key.1),
            other.map(|key| &key.0),
            other.map(|key| key.1),
            actor.map(|actor| actor.0),
            actor.map(|actor| actor.1),
            detail
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Records an `agent_started` event (appendix G).
pub fn record_event(
    store: &BoardStore,
    kind: &str,
    lane_id: Option<i64>,
    ticket: Option<&Key>,
    actor: Option<(&str, &str)>,
    detail: Option<&str>,
    now: OffsetDateTime,
) -> Result<i64> {
    let conn = store.open_write()?;
    insert_event(
        &conn,
        &format_ts(now),
        kind,
        lane_id,
        ticket,
        None,
        actor,
        detail,
    )
}

fn upsert_ref(conn: &Connection, node: &RefNode, now: &str) -> Result<()> {
    if node.state == "closed" {
        conn.execute(
            "DELETE FROM board_waiting WHERE repo = ?1 AND number = ?2",
            params![node.repo, node.number],
        )?;
    }
    let closed_at = if node.state == "open" {
        None
    } else {
        node.closed_at.clone()
    };
    conn.execute(
        "INSERT INTO board_items
         (repo, number, title, url, state, state_reason, closed_at, first_seen_at, synced_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT(repo, number) DO UPDATE SET title = excluded.title, url = excluded.url,
             state = excluded.state, state_reason = excluded.state_reason,
             closed_at = excluded.closed_at, synced_at = excluded.synced_at",
        params![
            node.repo,
            node.number,
            node.title,
            node.url,
            node.state,
            node.state_reason,
            closed_at,
            now
        ],
    )?;
    Ok(())
}

type EdgeMeta = BTreeMap<(Key, String), (String, String)>;

fn existing_edges(conn: &Connection, filter: &str, key: &Key) -> Result<EdgeMeta> {
    let mut statement = conn.prepare(&format!(
        "SELECT waiter_repo, waiter_number, blocker_repo, blocker_number, kind, first_seen_at,
                source FROM board_edges WHERE {filter}"
    ))?;
    let rows = statement
        .query_map(params![key.0, key.1], |row| {
            let waiter: Key = (row.get(0)?, row.get(1)?);
            let blocker: Key = (row.get(2)?, row.get(3)?);
            let kind: String = row.get(4)?;
            Ok(((waiter, blocker, kind), (row.get(5)?, row.get(6)?)))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|((waiter, blocker, kind), meta)| {
            // Keyed by the far end: the blocker for outgoing, the waiter for
            // incoming.
            let far = if &waiter == key { blocker } else { waiter };
            ((far, kind), meta)
        })
        .collect())
}

fn insert_edge(
    conn: &Connection,
    waiter: &Key,
    blocker: &Key,
    kind: EdgeKind,
    meta: Option<&(String, String)>,
    now: &str,
) -> Result<()> {
    let (first_seen_at, source) = meta
        .map(|(first, source)| (first.as_str(), source.as_str()))
        .unwrap_or((now, "github"));
    conn.execute(
        "INSERT OR IGNORE INTO board_edges
         (waiter_repo, waiter_number, blocker_repo, blocker_number, kind, first_seen_at, source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            waiter.0,
            waiter.1,
            blocker.0,
            blocker.1,
            kind.as_str(),
            first_seen_at,
            source
        ],
    )?;
    Ok(())
}

/// C3 step 2: X's outgoing edges become exactly `outgoing`, keeping the
/// first-seen time and source of edges that already existed.
fn replace_outgoing(
    conn: &Connection,
    key: &Key,
    outgoing: &[(Key, EdgeKind)],
    now: &str,
) -> Result<()> {
    let existing = existing_edges(conn, "waiter_repo = ?1 AND waiter_number = ?2", key)?;
    conn.execute(
        "DELETE FROM board_edges WHERE waiter_repo = ?1 AND waiter_number = ?2",
        params![key.0, key.1],
    )?;
    for (blocker, kind) in outgoing {
        let meta = existing.get(&(blocker.clone(), kind.as_str().to_owned()));
        insert_edge(conn, key, blocker, *kind, meta, now)?;
    }
    Ok(())
}

/// A child has one parent: the sub-issue edges into X become exactly the
/// one from its `parent`.
fn replace_parent(conn: &Connection, key: &Key, parent: Option<Key>, now: &str) -> Result<()> {
    let existing = existing_edges(
        conn,
        "blocker_repo = ?1 AND blocker_number = ?2 AND kind = 'sub_issue'",
        key,
    )?;
    conn.execute(
        "DELETE FROM board_edges
         WHERE blocker_repo = ?1 AND blocker_number = ?2 AND kind = 'sub_issue'",
        params![key.0, key.1],
    )?;
    if let Some(parent) = parent {
        let meta = existing.get(&(parent.clone(), "sub_issue".to_owned()));
        insert_edge(conn, &parent, key, EdgeKind::SubIssue, meta, now)?;
    }
    Ok(())
}

/// The standing Bugs ticket as `board_settings` records it (spec 1859 A5).
fn bugs_goal(conn: &Connection) -> Result<Option<Key>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM board_settings WHERE key = ?1",
            [BUGS_GOAL_SETTING],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.and_then(|value| parse_ticket_ref(&value, "")))
}

fn lane_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Lane> {
    Ok(Lane {
        id: row.get(0)?,
        goal: (row.get(1)?, row.get(2)?),
        rank: row.get(3)?,
        added_at: row.get(4)?,
        added_by: row.get(5)?,
        added_by_name: row.get(6)?,
    })
}

fn active_lanes(conn: &Connection) -> Result<Vec<Lane>> {
    let mut statement = conn.prepare(
        "SELECT id, goal_repo, goal_number, rank, added_at, added_by, added_by_name
         FROM board_lanes WHERE ended_at IS NULL ORDER BY rank, id",
    )?;
    let rows = statement
        .query_map([], lane_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Ends an active lane and renumbers the rest in their order. Returns the
/// `lane_ended` event id, or `None` when the lane was not active.
fn end_lane_tx(conn: &Connection, id: i64, reason: &str, now: &str) -> Result<Option<i64>> {
    let lane: Option<Key> = conn
        .query_row(
            "SELECT goal_repo, goal_number FROM board_lanes WHERE id = ?1 AND ended_at IS NULL",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some(goal) = lane else {
        return Ok(None);
    };
    conn.execute(
        "UPDATE board_lanes SET rank = NULL, ended_at = ?2, end_reason = ?3 WHERE id = ?1",
        params![id, now, reason],
    )?;
    for (index, lane) in active_lanes(conn)?.iter().enumerate() {
        conn.execute(
            "UPDATE board_lanes SET rank = ?2 WHERE id = ?1",
            params![lane.id, index as i64 + 1],
        )?;
    }
    conn.execute("DELETE FROM board_members WHERE lane_id = ?1", params![id])?;
    let event = insert_event(
        conn,
        now,
        "lane_ended",
        Some(id),
        Some(&goal),
        None,
        None,
        Some(reason),
    )?;
    rewrite_ticket_ranks(conn)?;
    Ok(Some(event))
}

/// `board_ticket_ranks`: every open member of an active lane with the best
/// rank among its lanes.
fn rewrite_ticket_ranks(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM board_ticket_ranks", [])?;
    conn.execute(
        "INSERT INTO board_ticket_ranks (repo, number, rank, lane_id)
         SELECT m.repo, m.number, l.rank, l.id FROM board_members m
         JOIN board_lanes l ON l.id = m.lane_id AND l.ended_at IS NULL
         JOIN board_items i ON i.repo = m.repo AND i.number = m.number AND i.state = 'open'
         WHERE l.rank = (SELECT MIN(l2.rank) FROM board_members m2
                         JOIN board_lanes l2 ON l2.id = m2.lane_id AND l2.ended_at IS NULL
                         WHERE m2.repo = m.repo AND m2.number = m.number)
         GROUP BY m.repo, m.number",
        [],
    )?;
    Ok(())
}

fn repo_syncs(conn: &Connection) -> Result<Vec<RepoSync>> {
    let mut statement = conn.prepare(
        "SELECT repo, last_ok_at, last_error, last_error_at, rate_limited_until
         FROM board_repo_sync ORDER BY repo",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok(RepoSync {
                repo: row.get(0)?,
                last_ok_at: row.get(1)?,
                last_error: row.get(2)?,
                last_error_at: row.get(3)?,
                rate_limited_until: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
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

fn load_input(conn: &Connection, outside: &Outside) -> Result<ModelInput> {
    let mut input = ModelInput::default();
    {
        let mut statement = conn.prepare(
            "SELECT repo, number, title, url, state, state_reason, closed_at FROM board_items",
        )?;
        for item in statement.query_map([], |row| {
            Ok(Item {
                repo: row.get(0)?,
                number: row.get(1)?,
                title: row.get(2)?,
                url: row.get(3)?,
                state: row.get(4)?,
                state_reason: row.get(5)?,
                closed_at: row.get(6)?,
            })
        })? {
            let item = item?;
            input.items.insert(item.key(), item);
        }
    }
    {
        let mut statement = conn.prepare(
            "SELECT waiter_repo, waiter_number, blocker_repo, blocker_number, kind, source
             FROM board_edges ORDER BY waiter_repo, waiter_number, blocker_repo, blocker_number",
        )?;
        for edge in statement.query_map([], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, i64>(1)?),
                (row.get::<_, String>(2)?, row.get::<_, i64>(3)?),
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })? {
            let (waiter, blocker, kind, source) = edge?;
            if let Some(kind) = EdgeKind::parse(&kind) {
                input.edges.push(Edge {
                    waiter,
                    blocker,
                    kind,
                    source,
                });
            }
        }
    }
    {
        let mut statement = conn.prepare(
            "SELECT repo, issue_number, pr_repo, pr_number, IFNULL(pr_state, 'OPEN'),
                    IFNULL(url, '')
             FROM board_prs ORDER BY repo, issue_number, pr_repo, pr_number",
        )?;
        for pr in statement.query_map([], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, i64>(1)?),
                PrRef {
                    repo: row.get(2)?,
                    number: row.get(3)?,
                    state: row.get(4)?,
                    url: row.get(5)?,
                },
            ))
        })? {
            let (key, pr) = pr?;
            input.prs.entry(key).or_default().push(pr);
        }
    }
    input.lanes = active_lanes(conn)?;
    {
        let mut statement = conn.prepare(
            "SELECT lane_id, repo, number, IFNULL(state, 'blocked'), joined_at
             FROM board_members",
        )?;
        for member in statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                (row.get::<_, String>(1)?, row.get::<_, i64>(2)?),
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })? {
            let (lane_id, key, state, joined_at) = member?;
            input.members.entry(lane_id).or_default().insert(
                key,
                Member {
                    state: TicketState::parse(&state).unwrap_or(TicketState::Blocked),
                    joined_at,
                },
            );
        }
    }
    if table_exists(conn, "work_claims")? {
        let mut statement = conn.prepare(
            "SELECT repo, number, session_id, session_name FROM work_claims
             WHERE ended_at IS NULL AND reserved_at IS NULL AND kind = 'ticket'
             ORDER BY claimed_at, id",
        )?;
        for claim in statement.query_map([], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, i64>(1)?),
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })? {
            let (key, session_id, session_name) = claim?;
            let session = outside.sessions.get(&session_id);
            input.holders.entry(key).or_default().push(Holder {
                name: session
                    .map(|session| session.name.clone())
                    .or(session_name)
                    .unwrap_or_else(|| session_id.clone()),
                state: session.map_or(HolderState::Retired, |session| session.state),
                session_id,
            });
        }
    }
    if table_exists(conn, "work_links")? {
        let mut statement =
            conn.prepare("SELECT DISTINCT repo, pr_number, ticket_number FROM work_links")?;
        for link in statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })? {
            let (repo, pr, ticket) = link?;
            input
                .pr_tickets
                .entry((canonical_repo(&repo), pr))
                .or_default()
                .insert(ticket);
        }
    }
    if table_exists(conn, "board_settings")? {
        input.bugs_goal = bugs_goal(conn)?;
    }
    input.waiting = outside.waiting.clone();
    if table_exists(conn, "board_waiting")? {
        let mut statement =
            conn.prepare("SELECT repo, number, text, url, created_at FROM board_waiting")?;
        for entry in statement.query_map([], |row| {
            Ok((
                (row.get(0)?, row.get(1)?),
                WaitingRecord {
                    kind: model::WaitingKind::Elsewhere,
                    session_id: String::new(),
                    pr: None,
                    text: row.get(2)?,
                    url: row.get(3)?,
                    created_at: row.get(4)?,
                },
            ))
        })? {
            let (key, record) = entry?;
            input.elsewhere.insert(key, record);
        }
    }
    let syncs = repo_syncs(conn)?;
    input.stale = syncs
        .iter()
        .filter(|sync| sync.stale())
        .map(|sync| sync.repo.clone())
        .collect();
    input.read_repos = outside.config_repos.iter().cloned().collect();
    for lane in &input.lanes {
        input.read_repos.insert(lane.goal.0.clone());
    }
    // Repos of open members at the last recompute.
    let active: BTreeSet<i64> = input.lanes.iter().map(|lane| lane.id).collect();
    for (lane_id, members) in &input.members {
        if !active.contains(lane_id) {
            continue;
        }
        for key in members.keys() {
            if input.items.get(key).is_some_and(Item::is_open) {
                input.read_repos.insert(key.0.clone());
            }
        }
    }
    Ok(input)
}

/// One read pass (C1–C4): reads every repo on the board until no new repo
/// appears, then recomputes. A pass paused by the rate limit only
/// recomputes.
pub fn run_pass(
    store: &BoardStore,
    source: &dyn BoardSource,
    outside: &Outside,
    now: OffsetDateTime,
) -> Result<Recomputed> {
    store.ensure_schema()?;
    if !store.rate_limited(now)? {
        let mut read: BTreeSet<String> = BTreeSet::new();
        'repos: loop {
            let mut wanted = store.read_set(outside)?;
            // Repos of open members as the tables stand now, so a blocker
            // found in another repo this pass is read this pass.
            let (board, _) = store.board(outside, now)?;
            for view in &board.lanes {
                for row in &view.rows {
                    if board.facts[&row.key].item.is_open() {
                        wanted.insert(row.key.0.clone());
                    }
                }
            }
            let Some(repo) = wanted.into_iter().find(|repo| !read.contains(repo)) else {
                break;
            };
            read.insert(repo.clone());
            match read_one(store, source, &repo, now) {
                Ok(Some(reset_at)) => {
                    // Nothing was applied: stale until a full read lands.
                    store.mark_repo_failed(&repo, "GitHub rate limit", now)?;
                    let mut all = store.read_set(outside)?;
                    all.extend(read.iter().cloned());
                    store.set_rate_limited(&all, &reset_at)?;
                    break 'repos;
                }
                Ok(None) => {}
                Err(error) => store.mark_repo_failed(&repo, &error, now)?,
            }
        }
        store.prune_events(now)?;
    }
    store.recompute(outside, now)
}

/// Reads one repo and applies it. `Ok(Some(reset_at))` when the rate limit
/// ran low and nothing was applied.
fn read_one(
    store: &BoardStore,
    source: &dyn BoardSource,
    repo: &str,
    now: OffsetDateTime,
) -> std::result::Result<Option<String>, String> {
    let read = sync::read_repo(source, repo)?;
    if read.rate_limited {
        return Ok(Some(
            read.rate_reset_at
                .unwrap_or_else(|| format_ts(now + time::Duration::hours(1))),
        ));
    }
    let open: BTreeSet<i64> = read.issues.iter().map(|issue| issue.number).collect();
    let gone: Vec<i64> = store
        .open_numbers(repo)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|number| !open.contains(number))
        .collect();
    let mut missing = BTreeMap::new();
    for chunk in gone.chunks(crate::work_claims::MAX_ALIASES_PER_QUERY) {
        let states = source.items(repo, chunk)?;
        for number in chunk {
            // A number the batch left out failed to fetch: keep its row.
            if let Some(node) = states.get(number) {
                missing.insert(*number, node.clone());
            }
        }
    }
    store
        .apply_repo_read(repo, &read.issues, &missing, now)
        .map_err(|error| format!("{error:#}"))?;
    Ok(None)
}

/// C5: writes one link on GitHub, then records it. `Err` is a refusal.
pub fn write_link(
    store: &BoardStore,
    source: &dyn BoardSource,
    request: &LinkRequest,
    now: OffsetDateTime,
) -> Result<std::result::Result<LinkOutcome, Refusal>> {
    if request.ticket == request.target {
        return Ok(Err(Refusal::Unprocessable(
            "a ticket can't wait on itself".to_owned(),
        )));
    }
    let resolved = match source.resolve(&[request.ticket.clone(), request.target.clone()]) {
        Ok(resolved) => resolved,
        Err(error) => return Ok(Err(Refusal::Unreachable(error))),
    };
    let found = |index: usize, key: &Key| {
        resolved
            .get(index)
            .cloned()
            .flatten()
            .ok_or_else(|| Refusal::NotFound(format!("no such issue: {}#{}", key.0, key.1)))
    };
    let ticket = match found(0, &request.ticket) {
        Ok(ticket) => ticket,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let target = match found(1, &request.target) {
        Ok(target) => target,
        Err(refusal) => return Ok(Err(refusal)),
    };
    store.upsert_nodes(&[&ticket.node, &target.node], now)?;
    let exists = match request.kind {
        EdgeKind::After => ticket.blocked_by.contains(&request.target),
        EdgeKind::SubIssue => ticket.parent.as_ref() == Some(&request.target),
    };
    if !request.remove && request.kind == EdgeKind::SubIssue && !exists {
        if let Some(parent) = &ticket.parent {
            let child_ref = model::short_ref(&request.ticket, &request.ticket.0);
            let parent_arg = if parent.0 == request.ticket.0 {
                parent.1.to_string()
            } else {
                format!("{}#{}", parent.0, parent.1)
            };
            return Ok(Err(Refusal::Unprocessable(format!(
                "{child_ref} already sits under {}#{}; remove that first with sm board under {} {parent_arg} --remove",
                parent.0, parent.1, request.ticket.1
            ))));
        }
    }
    let outcome = match (request.remove, exists) {
        (false, true) => LinkOutcome::Already,
        (true, false) => LinkOutcome::Absent,
        (remove, _) => {
            let mutation = match (request.kind, remove) {
                (EdgeKind::After, false) => LinkMutation::AddBlockedBy {
                    issue_id: ticket.id.clone(),
                    blocking_id: target.id.clone(),
                },
                (EdgeKind::After, true) => LinkMutation::RemoveBlockedBy {
                    issue_id: ticket.id.clone(),
                    blocking_id: target.id.clone(),
                },
                (EdgeKind::SubIssue, false) => LinkMutation::AddSubIssue {
                    parent_id: target.id.clone(),
                    child_id: ticket.id.clone(),
                },
                (EdgeKind::SubIssue, true) => LinkMutation::RemoveSubIssue {
                    parent_id: target.id.clone(),
                    child_id: ticket.id.clone(),
                },
            };
            match source.write_link(&mutation) {
                Ok(()) => {}
                Err(WriteError::Refused(message)) => {
                    return Ok(Err(Refusal::Unprocessable(message)))
                }
                Err(WriteError::Transport(message)) => {
                    return Ok(Err(Refusal::Unreachable(message)))
                }
            }
            if remove {
                LinkOutcome::Removed
            } else {
                LinkOutcome::Recorded
            }
        }
    };
    store.record_link(request, outcome, now)?;
    Ok(Ok(outcome))
}

/// D4's checks on the goal: it must exist on GitHub and be open. Stores
/// the goal's row.
pub fn check_goal(
    store: &BoardStore,
    source: &dyn BoardSource,
    goal: &Key,
    now: OffsetDateTime,
) -> Result<std::result::Result<(), Refusal>> {
    let resolved = match source.resolve(std::slice::from_ref(goal)) {
        Ok(resolved) => resolved,
        Err(error) => return Ok(Err(Refusal::Unreachable(error))),
    };
    let Some(issue) = resolved.into_iter().next().flatten() else {
        return Ok(Err(Refusal::NotFound(format!(
            "no such issue: {}#{}",
            goal.0, goal.1
        ))));
    };
    store.upsert_nodes(&[&issue.node], now)?;
    if issue.node.state != "open" {
        return Ok(Err(Refusal::Unprocessable(format!(
            "#{} is closed",
            goal.1
        ))));
    }
    Ok(Ok(()))
}

// ---------------------------------------------------------------------------
// The board JSON (appendix F).

/// The Board count and the lanes contributing to it (appendix I).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unseen {
    pub count: usize,
    pub lane_ids: BTreeSet<i64>,
}

/// The Needs-you part of the Board count: tickets needs_you in an active
/// lane whose latest `became_needs_you` event is newer than the newest
/// event when the owner looked, each once.
pub fn needs_you_unseen(board: &Board, since: &BTreeMap<Key, i64>, seen_event_id: i64) -> Unseen {
    let mut unseen = Unseen::default();
    let mut counted = BTreeSet::new();
    for view in &board.lanes {
        for row in &view.rows {
            if board.facts[&row.key].state != TicketState::NeedsYou {
                continue;
            }
            let fresh = since
                .get(&row.key)
                .is_some_and(|event_id| *event_id > seen_event_id);
            if fresh {
                unseen.lane_ids.insert(view.lane.id);
                if counted.insert(row.key.clone()) {
                    unseen.count += 1;
                }
            }
        }
    }
    unseen
}

fn key_json(key: &Key) -> Value {
    json!({ "repo": key.0, "number": key.1 })
}

fn done_reason(item: &Item) -> Option<&'static str> {
    if item.is_open() {
        return None;
    }
    let reason = item.state_reason.as_deref().unwrap_or_default();
    Some(if reason.eq_ignore_ascii_case("not_planned") {
        "not_planned"
    } else if reason.eq_ignore_ascii_case("duplicate") {
        "duplicate"
    } else if reason == "missing" {
        "missing"
    } else {
        "completed"
    })
}

fn ticket_json(
    board: &Board,
    key: &Key,
    row: Option<&model::Row>,
    clocks: &BTreeMap<Key, Value>,
) -> Value {
    let facts = &board.facts[key];
    let waits_on: Vec<Value> = facts
        .waits_on
        .iter()
        .map(|blocker| {
            let state = board
                .facts
                .get(blocker)
                .map(|facts| facts.state.as_str())
                .unwrap_or("blocked");
            json!({ "repo": blocker.0, "number": blocker.1, "state": state })
        })
        .collect();
    let mut value = json!({
        "repo": key.0,
        "number": key.1,
        "title": facts.item.title,
        "url": facts.item.url,
        "state": facts.state.as_str(),
        "done_reason": done_reason(&facts.item),
        "needs_you": facts.needs_you,
        "waits_on": waits_on,
        "holder": facts.holder.as_ref().map(|holder| json!({
            "session_id": holder.session_id,
            "name": holder.name,
            "state": holder.state.as_str(),
        })),
        "prs": facts.prs,
        "sub_issues_done": facts.sub_issues_done,
        "sub_issues": { "total": facts.sub_issues.len(), "done": facts.sub_issues_closed },
        "warnings": facts.warnings,
        "closed_at": facts.item.closed_at,
    });
    if let Some(row) = row {
        value["chain"] = json!(row.chain);
        value["on_longest_chain"] = json!(row.on_longest_chain);
        value["also_in"] = json!(row
            .also_in
            .iter()
            .map(|(lane_id, rank)| json!({ "lane_id": lane_id, "rank": rank }))
            .collect::<Vec<_>>());
        value["new"] = json!(row.new);
    }
    if let Some(clock) = clocks.get(key) {
        value["clock"] = clock.clone();
    }
    value
}

/// A change's line (appendix F), `#N` relative to the lane's goal repo.
pub fn change_text(event: &Event, base: &str) -> Option<String> {
    let short = |key: &Option<Key>| {
        key.as_ref()
            .map(|key| model::short_ref(key, base))
            .unwrap_or_default()
    };
    let actor = event.actor_name.clone().unwrap_or_else(|| "GitHub".into());
    let (a, b) = (short(&event.ticket), short(&event.other));
    Some(match event.kind.as_str() {
        "ticket_joined" => format!("{a} joined, added by {actor}"),
        "ticket_left" => format!("{a} left the lane"),
        "link_added" if event.detail.as_deref() == Some("sub_issue") => {
            format!("{a} is now under {b} ({actor})")
        }
        "link_added" => format!("{a} now starts after {b} ({actor})"),
        "link_removed" if event.detail.as_deref() == Some("sub_issue") => {
            format!("{a} is no longer under {b} ({actor})")
        }
        "link_removed" => format!("{a} no longer starts after {b} ({actor})"),
        "became_ready" => format!("{a} ready"),
        "became_needs_you" => format!(
            "{a} waits on you: {}",
            event.detail.as_deref().unwrap_or_default()
        ),
        "ticket_closed" => format!("{a} closed"),
        "lane_moved" => format!(
            "moved to lane {}",
            event.detail.as_deref().unwrap_or_default()
        ),
        "lane_added" => format!("lane added by {actor}"),
        "lane_ended" => "lane ended".to_owned(),
        "review_policy" => format!(
            "review policy {} by {actor}",
            event.detail.as_deref().unwrap_or("changed")
        ),
        "agent_started" => format!(
            "Started {} on {a}",
            event.detail.as_deref().unwrap_or_default()
        ),
        "cycle_found" => {
            let keys: Vec<String> = event
                .detail
                .as_deref()
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|text| {
                    let (repo, number) = text.rsplit_once('#')?;
                    Some(model::short_ref(
                        &(repo.to_owned(), number.parse().ok()?),
                        base,
                    ))
                })
                .collect();
            match keys.split_last() {
                Some((last, rest)) if !rest.is_empty() => {
                    format!("{} and {last} wait on each other", rest.join(", "))
                }
                _ => format!("{a} waits on itself"),
            }
        }
        _ => return None,
    })
}

/// The lane's last changes: its own events, and link events that touch
/// one of its members.
fn lane_changes(view: &LaneView, events: &[Event]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| !event.kind.starts_with("push_") && event.kind != "sync_failed")
        .filter(|event| match event.lane_id {
            Some(lane_id) => lane_id == view.lane.id,
            None => {
                event.kind.starts_with("link_")
                    && [&event.ticket, &event.other]
                        .into_iter()
                        .flatten()
                        .any(|key| view.contains(key))
            }
        })
        .filter_map(|event| {
            Some(json!({
                "ts": event.ts,
                "kind": event.kind,
                "text": change_text(event, &view.lane.goal.0)?,
            }))
        })
        .take(CHANGES_SHOWN)
        .collect()
}

/// Everything the JSON needs beyond the board itself.
pub struct JsonContext<'a> {
    /// Newest first.
    pub events: &'a [Event],
    pub repos: &'a [RepoSync],
    pub unseen: &'a Unseen,
    pub start_defaults: Value,
    /// `?lane=owner/name#N`: only that lane.
    pub lane_filter: Option<&'a Key>,
    pub now: OffsetDateTime,
    /// Each in-progress or needs-you ticket's clock (D7), when asked for.
    pub clocks: &'a BTreeMap<Key, Value>,
}

pub fn board_json(board: &Board, input: &ModelInput, context: &JsonContext<'_>) -> Value {
    let lanes: Vec<Value> = board
        .lanes
        .iter()
        .filter(|view| {
            context
                .lane_filter
                .is_none_or(|goal| &view.lane.goal == goal)
        })
        .map(|view| {
            let goal = board.facts.get(&view.lane.goal);
            let counts = json!({
                "needs_you": view.count(board, TicketState::NeedsYou),
                "close_ready": view.count(board, TicketState::CloseReady),
                "ready": view.count(board, TicketState::Ready),
                "in_progress": view.count(board, TicketState::InProgress),
                "blocked": view.count(board, TicketState::Blocked),
                "done": view.count(board, TicketState::Done),
            });
            json!({
                "id": view.lane.id,
                "rank": view.lane.rank,
                "goal": {
                    "repo": view.lane.goal.0,
                    "number": view.lane.goal.1,
                    "title": goal.map(|facts| facts.item.title.clone()).unwrap_or_default(),
                    "url": goal.map(|facts| facts.item.url.clone()).unwrap_or_default(),
                    "state": goal.map(|facts| facts.state.as_str()),
                    "sub_issues": goal.map(|facts| json!({
                        "total": facts.sub_issues.len(), "done": facts.sub_issues_closed,
                    })),
                },
                "added_at": view.lane.added_at,
                "added_by_name": view.lane.added_by_name,
                "counts": counts,
                "unseen": context.unseen.lane_ids.contains(&view.lane.id),
                "longest_chain": view.longest_chain.iter().map(key_json).collect::<Vec<_>>(),
                "stale": view.stale,
                "cycles": view.cycles.iter()
                    .map(|cycle| cycle.iter().map(key_json).collect::<Vec<_>>())
                    .collect::<Vec<_>>(),
                "tickets": view.rows.iter()
                    .map(|row| ticket_json(board, &row.key, Some(row), context.clocks))
                    .collect::<Vec<_>>(),
                "changes": lane_changes(view, context.events),
            })
        })
        .collect();
    let other: Vec<Value> = board
        .other
        .iter()
        .map(|(repo, keys)| {
            json!({
                "repo": repo,
                "tickets": keys.iter().map(|key| ticket_json(board, key, None, context.clocks)).collect::<Vec<_>>(),
            })
        })
        .collect();
    let syncs: BTreeMap<&str, &RepoSync> = context
        .repos
        .iter()
        .map(|sync| (sync.repo.as_str(), sync))
        .collect();
    let repos: Vec<Value> = input
        .read_repos
        .iter()
        .map(|repo| {
            let sync = syncs.get(repo.as_str());
            let stale = sync.is_some_and(|sync| sync.stale());
            json!({
                "repo": repo,
                "last_ok_at": sync.and_then(|sync| sync.last_ok_at.clone()),
                "stale": stale,
                "error": if stale { sync.and_then(|sync| sync.last_error.clone()) } else { None },
            })
        })
        .collect();
    json!({
        "generated_at": format_ts(context.now),
        "unseen": {
            "count": context.unseen.count,
            "lane_ids": context.unseen.lane_ids.iter().collect::<Vec<_>>(),
        },
        "repos": repos,
        "lanes": lanes,
        "other": other,
        "start_defaults": context.start_defaults,
    })
}

/// A lane object as the board JSON shows it, for the lane routes.
pub fn lane_json(board_json: &Value, lane_id: i64) -> Value {
    board_json["lanes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|lane| lane["id"].as_i64() == Some(lane_id))
        .cloned()
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests;
