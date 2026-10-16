//! Auto PM state (sm#2174; spec `docs/working/2170_auto_pm.html`,
//! Appendices B, C and F.2): live switches, the experiment record and its
//! gates, the health guard, the kill switch, the weekly budget, and the
//! server-run metric.
//!
//! Everything durable lives in the queue database next to the owner
//! activity log, except the guard's per-minute samples, which live in
//! `utilization.db`. Functions here touch only local state; anything that
//! reaches GitHub or an agent is returned as an [`Effect`] for the HTTP layer
//! to carry out, so the rules stay testable without either.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

pub mod ask;
pub mod budget;
pub mod cycle;
pub mod experiments;
pub mod guard;
pub mod metrics;
pub mod recovery;
pub mod switches;

#[cfg(test)]
mod tests;

/// The repo the auto PM works in, and its goal ticket (#2170).
pub const REPO: &str = "rajeshgoli/session-manager";
pub const GOAL: i64 = 2170;
/// The registry role the auto PM seat holds (sm#2175 registers it).
pub const ROLE: &str = "autopm";
pub const DAY_MS: i64 = 86_400_000;
pub const HOUR_MS: i64 = 3_600_000;

pub fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// Unix ms of an RFC 3339 time, as the usage database writes them.
pub fn parse_ms(text: &str) -> Option<i64> {
    crate::work_attribution::nanos(text).map(|nanos| (nanos / 1_000_000) as i64)
}

pub fn format_ms(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|at| {
            at.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

/// Who asks for a change. The role is refused what only the owner may do;
/// `Server` is sm itself (the guard, the measurement, the kill switch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    Owner,
    Autopm,
    Server,
}

/// A refused action: the HTTP status and the failing gate's name (B.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub gate: String,
    pub detail: String,
}

impl Refusal {
    pub fn gate(gate: &str, detail: impl Into<String>) -> Self {
        Self {
            status: 409,
            gate: gate.to_owned(),
            detail: detail.into(),
        }
    }
    pub fn bad(gate: &str, detail: impl Into<String>) -> Self {
        Self {
            status: 400,
            ..Self::gate(gate, detail)
        }
    }
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self {
            status: 404,
            ..Self::gate("not_found", detail)
        }
    }
    pub fn message(&self) -> String {
        format!("{}: {}", self.gate, self.detail)
    }
}

/// A refusal, or a failure of sm itself.
#[derive(Debug)]
pub enum Fail {
    Refused(Refusal),
    Internal(anyhow::Error),
}

impl From<Refusal> for Fail {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<anyhow::Error> for Fail {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl From<rusqlite::Error> for Fail {
    fn from(error: rusqlite::Error) -> Self {
        Self::Internal(error.into())
    }
}

pub type Out<T> = std::result::Result<T, Fail>;

/// What the result of a call must still do outside the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Queue a message to one session.
    Message { session_id: String, text: String },
    /// Message whoever holds the `autopm` role now, if anyone.
    WakeAutopm { text: String },
    /// Place a merge hold (the existing `merge_holds`) on an open PR.
    MergeHold { pr: i64, reason: String },
    /// Close a session-manager issue with a comment.
    CloseIssue { number: i64, comment: String },
    /// Cancel a `waiting` auto-start row on a ticket.
    CancelAutoStart { number: i64, reason: String },
}

/// The `autopm` owner setting (C.1), typed.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub dedicated: BTreeMap<String, f64>,
    pub surplus_cap: BTreeMap<String, f64>,
    pub surplus_margin: f64,
    /// An ISO date; a provider window containing it is skipped.
    pub skip_window_starting: Option<String>,
    pub max_live: i64,
    pub paused_at: Option<String>,
    pub pm_points: BTreeMap<String, f64>,
    pub review_points: BTreeMap<String, f64>,
    pub app_updates_per_week: i64,
    /// The owner's name for messages (`owner_name` in config).
    pub owner: String,
}

pub const PROVIDERS: [&str; 2] = ["claude", "codex"];

impl Settings {
    pub fn with_owner(mut self, owner: &str) -> Self {
        self.owner = owner.to_owned();
        self
    }

    /// From the effective owner settings object.
    pub fn from_owner(settings: &Value) -> Self {
        let autopm = &settings["autopm"];
        let points = |key: &str| {
            PROVIDERS
                .iter()
                .map(|p| ((*p).to_owned(), autopm[key][p].as_f64().unwrap_or(0.0)))
                .collect()
        };
        Self {
            enabled: autopm["enabled"].as_bool().unwrap_or(false),
            dedicated: points("dedicated_points"),
            surplus_cap: points("surplus_cap_points"),
            surplus_margin: autopm["surplus_margin_points"].as_f64().unwrap_or(20.0),
            skip_window_starting: autopm["skip_window_starting"].as_str().map(str::to_owned),
            max_live: autopm["max_live"].as_i64().unwrap_or(3),
            paused_at: autopm["paused_at"].as_str().map(str::to_owned),
            pm_points: points("pm_points"),
            review_points: points("review_points"),
            app_updates_per_week: autopm["app_updates_per_week"].as_i64().unwrap_or(1),
            owner: crate::config::DEFAULT_OWNER_NAME.to_owned(),
        }
    }
}

/// The databases the auto PM reads and writes.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The queue database: experiments, switches, budget, board, claims.
    pub queue_db: PathBuf,
    pub usage_db: PathBuf,
    pub activity_db: PathBuf,
    pub utilization_db: PathBuf,
    pub bugs_db: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AutopmStore {
    pub paths: Paths,
}

pub(crate) fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS experiment_switches (
           key           TEXT PRIMARY KEY,
           experiment_id TEXT NOT NULL,
           on_           INTEGER NOT NULL DEFAULT 0,
           held          INTEGER NOT NULL DEFAULT 0,
           changed_at_ms INTEGER NOT NULL,
           changed_by    TEXT NOT NULL,
           reason        TEXT
         );
         CREATE TABLE IF NOT EXISTS autopm_experiments (
           id                TEXT PRIMARY KEY,
           cycle_ticket      INTEGER NOT NULL,
           title             TEXT NOT NULL,
           hypothesis        TEXT NOT NULL,
           path              TEXT NOT NULL,
           target_repo       TEXT,
           switch_key        TEXT,
           change_class      TEXT,
           plan_path         TEXT,
           plan_pr           INTEGER,
           plan_commit       TEXT,
           plan_reviewed_sha TEXT,
           tickets_json      TEXT NOT NULL DEFAULT '[]',
           metric_json       TEXT,
           guards_json       TEXT,
           proposal_md       TEXT,
           window_days       INTEGER NOT NULL DEFAULT 14,
           baseline_until_ms INTEGER,
           baseline_json     TEXT,
           live_at_ms       INTEGER,
           measure_at_ms     INTEGER,
           extended          INTEGER NOT NULL DEFAULT 0,
           state             TEXT NOT NULL,
           decision_note     TEXT,
           votes_up          INTEGER NOT NULL DEFAULT 0,
           votes_down        INTEGER NOT NULL DEFAULT 0,
           points_est_json   TEXT,
           points_spent_json TEXT,
           revert_commit     TEXT,
           restore_ok_at_ms  INTEGER,
           kept_at_ms        INTEGER,
           created_at_ms     INTEGER NOT NULL,
           updated_at_ms     INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS autopm_measurements (
           experiment_id    TEXT NOT NULL,
           attempt          INTEGER NOT NULL,
           at_ms            INTEGER NOT NULL,
           status           TEXT NOT NULL,
           error            TEXT,
           result_json      TEXT,
           decision         TEXT,
           superseded_at_ms INTEGER,
           PRIMARY KEY (experiment_id, attempt)
         );
         CREATE TABLE IF NOT EXISTS autopm_votes (
           id            INTEGER PRIMARY KEY,
           experiment_id TEXT NOT NULL,
           at_ms         INTEGER NOT NULL,
           vote          TEXT NOT NULL,
           note          TEXT
         );
         CREATE TABLE IF NOT EXISTS autopm_fixes (
           repo            TEXT NOT NULL,
           number          INTEGER NOT NULL,
           kind            TEXT NOT NULL,
           cycle_ticket    INTEGER,
           experiment_id   TEXT,
           created_at_ms   INTEGER NOT NULL,
           cancelled_at_ms INTEGER,
           PRIMARY KEY (repo, number)
         );
         CREATE TABLE IF NOT EXISTS autopm_app_deploys (
           id       INTEGER PRIMARY KEY,
           at_ms    INTEGER NOT NULL,
           repo     TEXT NOT NULL,
           number   INTEGER NOT NULL,
           apk_hash TEXT NOT NULL,
           version_name TEXT
         );
         CREATE TABLE IF NOT EXISTS autopm_reservations (
           repo            TEXT NOT NULL,
           number          INTEGER NOT NULL,
           provider        TEXT NOT NULL,
           points          REAL NOT NULL,
           window_start    TEXT NOT NULL,
           created_at_ms   INTEGER NOT NULL,
           released_at_ms  INTEGER,
           original_points REAL,
           origin_ms       INTEGER,
           suspended_at_ms INTEGER,
           PRIMARY KEY (repo, number, provider, window_start)
         );
         CREATE TABLE IF NOT EXISTS autopm_windows (
           provider     TEXT PRIMARY KEY,
           window_start TEXT NOT NULL,
           start_ms     INTEGER NOT NULL,
           resets_ms    INTEGER NOT NULL,
           skipped      INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS autopm_seats (
           seat_id       TEXT PRIMARY KEY,
           first_seen_ms INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS autopm_review_slots (
           request_id    TEXT PRIMARY KEY,
           provider      TEXT NOT NULL,
           round         INTEGER NOT NULL,
           step_index    INTEGER NOT NULL,
           exempt        INTEGER NOT NULL,
           started_at_ms INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS autopm_review_charges (
           request_id TEXT NOT NULL,
           round      INTEGER NOT NULL,
           step_index INTEGER NOT NULL,
           provider   TEXT NOT NULL,
           points     REAL NOT NULL,
           at_ms      INTEGER NOT NULL,
           PRIMARY KEY (request_id, round, step_index)
         );
         CREATE TABLE IF NOT EXISTS autopm_notices (
           key   TEXT PRIMARY KEY,
           at_ms INTEGER NOT NULL
         );",
    )?;
    Ok(())
}

impl AutopmStore {
    pub fn new(paths: Paths) -> Self {
        Self { paths }
    }

    pub fn open(&self) -> Result<Connection> {
        if let Some(parent) = self.paths.queue_db.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(&self.paths.queue_db)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_schema(&conn)?;
        Ok(conn)
    }

    /// Records a notice key; true the first time only, so a message that
    /// must go once is sent once.
    pub fn first_notice(conn: &Connection, key: &str, at_ms: i64) -> Result<bool> {
        Ok(conn.execute(
            "INSERT OR IGNORE INTO autopm_notices(key, at_ms) VALUES (?1, ?2)",
            params![key, at_ms],
        )? == 1)
    }

    /// Records a seat that holds the `autopm` role (C.3 group 1). Seats are
    /// never removed, so handoff predecessors keep counting.
    pub fn record_seat(&self, seat_id: &str, at_ms: i64) -> Result<()> {
        self.open()?.execute(
            "INSERT OR IGNORE INTO autopm_seats(seat_id, first_seen_ms) VALUES (?1, ?2)",
            params![seat_id, at_ms],
        )?;
        Ok(())
    }

    /// Records a fix, revert or cleanup ticket (B.3 `autopm_fixes`). The
    /// issue route (sm#2175) and sm's own cleanup filing call this.
    pub fn record_fix(
        &self,
        number: i64,
        kind: &str,
        cycle_ticket: Option<i64>,
        experiment_id: Option<&str>,
        at_ms: i64,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(kind, "fix" | "revert" | "cleanup"),
            "kind must be fix, revert or cleanup"
        );
        self.open()?.execute(
            "INSERT OR REPLACE INTO autopm_fixes
               (repo, number, kind, cycle_ticket, experiment_id, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![REPO, number, kind, cycle_ticket, experiment_id, at_ms],
        )?;
        Ok(())
    }
}

/// The start of the Monday-to-Monday week holding `now`, in the owner's
/// timezone.
pub fn week_start_ms(now: i64) -> i64 {
    let at = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let offset = time::UtcOffset::local_offset_at(at).unwrap_or(time::UtcOffset::UTC);
    let local = at.to_offset(offset);
    let monday =
        local.date() - time::Duration::days(i64::from(local.weekday().number_days_from_monday()));
    (monday
        .midnight()
        .assume_offset(offset)
        .unix_timestamp_nanos()
        / 1_000_000) as i64
}

impl AutopmStore {
    /// The app-deploy cap (B.3): a session holding a live claim on a ticket
    /// under #2170 is refused 429 `app_update_cap` once this week already
    /// holds `app_updates_per_week` autopm deploys. The slot is claimed in
    /// the same transaction as the count, so two concurrent deploys cannot
    /// both fit; [`Self::finish_app_deploy`] fills it once the APK is stored
    /// and [`Self::release_app_deploy`] frees it if the deploy fails.
    /// Off while the auto PM is off.
    pub fn claim_app_deploy(
        &self,
        settings: &Settings,
        session_id: &str,
        now: i64,
    ) -> Out<Option<DeploySlot>> {
        if !settings.enabled {
            return Ok(None);
        }
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let under = tickets_under_goal(&tx)?;
        let Some(ticket) = goal_claims(&tx, &under)?
            .into_iter()
            .find(|claim| claim.live && claim.session_id == session_id && claim.number != GOAL)
            .map(|claim| claim.number)
        else {
            return Ok(None);
        };
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM autopm_app_deploys WHERE at_ms >= ?1",
            [week_start_ms(now)],
            |row| row.get(0),
        )?;
        if count >= settings.app_updates_per_week {
            return Err(Refusal {
                status: 429,
                gate: "app_update_cap".into(),
                detail: format!(
                    "{count} autopm app update(s) already this week; batch Android changes into one ticket a week"
                ),
            }
            .into());
        }
        tx.execute(
            "INSERT INTO autopm_app_deploys(at_ms, repo, number, apk_hash) VALUES (?1, ?2, ?3, '')",
            params![now, REPO, ticket],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Some(DeploySlot { id, ticket }))
    }

    pub fn finish_app_deploy(
        &self,
        slot: &DeploySlot,
        apk_hash: &str,
        version_name: &str,
    ) -> Result<()> {
        self.open()?.execute(
            "UPDATE autopm_app_deploys SET apk_hash = ?2, version_name = ?3 WHERE id = ?1",
            params![slot.id, apk_hash, version_name],
        )?;
        Ok(())
    }

    pub fn release_app_deploy(&self, slot: &DeploySlot) -> Result<()> {
        self.open()?
            .execute("DELETE FROM autopm_app_deploys WHERE id = ?1", [slot.id])?;
        Ok(())
    }

    #[cfg(test)]
    pub fn record_app_deploy(
        &self,
        number: i64,
        apk_hash: &str,
        version_name: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.open()?.execute(
            "INSERT INTO autopm_app_deploys(at_ms, repo, number, apk_hash, version_name)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now, REPO, number, apk_hash, version_name],
        )?;
        Ok(())
    }
}

/// A claimed weekly app-deploy slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeploySlot {
    pub id: i64,
    pub ticket: i64,
}

/// `autopm_fixes` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub number: i64,
    pub kind: String,
    pub experiment_id: Option<String>,
    pub cancelled_at_ms: Option<i64>,
}

pub fn fix(conn: &Connection, number: i64) -> Result<Option<Fix>> {
    Ok(conn
        .query_row(
            "SELECT number, kind, experiment_id, cancelled_at_ms FROM autopm_fixes
             WHERE repo = ?1 AND number = ?2",
            params![REPO, number],
            |row| {
                Ok(Fix {
                    number: row.get(0)?,
                    kind: row.get(1)?,
                    experiment_id: row.get(2)?,
                    cancelled_at_ms: row.get(3)?,
                })
            },
        )
        .optional()?)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Every ticket transitively under #2170 on the board (sub-issue edges:
/// the waiter is the parent), the goal itself included.
pub fn tickets_under_goal(conn: &Connection) -> Result<BTreeSet<i64>> {
    let mut tickets = BTreeSet::from([GOAL]);
    if !table_exists(conn, "board_edges")? {
        return Ok(tickets);
    }
    let mut stmt = conn.prepare(
        "WITH RECURSIVE tree(number) AS (
           SELECT ?2
           UNION
           SELECT e.blocker_number FROM board_edges e JOIN tree t
             ON e.waiter_number = t.number
           WHERE e.kind = 'sub_issue' AND e.waiter_repo = ?1 AND e.blocker_repo = ?1
         )
         SELECT number FROM tree",
    )?;
    for number in stmt.query_map(params![REPO, GOAL], |row| row.get::<_, i64>(0))? {
        tickets.insert(number?);
    }
    Ok(tickets)
}

/// Seats that ever held the role (C.3 group 1).
pub fn autopm_seats(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut stmt = conn.prepare("SELECT seat_id FROM autopm_seats")?;
    let seats = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(seats)
}

/// One ticket claim on a ticket under #2170.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub number: i64,
    pub session_id: String,
    pub live: bool,
}

/// Every ticket claim, past and live, on a ticket under #2170 (C.3 group 2).
pub fn goal_claims(conn: &Connection, under: &BTreeSet<i64>) -> Result<Vec<Claim>> {
    if !table_exists(conn, "work_claims")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT number, session_id, ended_at IS NULL FROM work_claims
         WHERE repo = ?1 AND kind = 'ticket' ORDER BY claimed_at, id",
    )?;
    let claims = stmt
        .query_map([REPO], |row| {
            Ok(Claim {
                number: row.get(0)?,
                session_id: row.get(1)?,
                live: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(claims
        .into_iter()
        .filter(|claim| under.contains(&claim.number))
        .collect())
}

/// Sessions holding a live claim on a ticket.
pub fn live_holders(conn: &Connection, number: i64) -> Result<Vec<String>> {
    if !table_exists(conn, "work_claims")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT DISTINCT session_id FROM work_claims
         WHERE repo = ?1 AND number = ?2 AND kind = 'ticket' AND ended_at IS NULL",
    )?;
    let holders = stmt
        .query_map(params![REPO, number], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(holders)
}

/// Whether a board item is open; an unknown ticket counts as open.
pub fn ticket_open(conn: &Connection, number: i64) -> Result<bool> {
    if !table_exists(conn, "board_items")? {
        return Ok(true);
    }
    Ok(conn
        .query_row(
            "SELECT state FROM board_items WHERE repo = ?1 AND number = ?2",
            params![REPO, number],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_none_or(|state| state != "closed"))
}

/// PRs linked to a ticket by `sm pr`, a `Closes #N` reference, or board sync.
pub fn linked_prs(conn: &Connection, number: i64) -> Result<BTreeSet<i64>> {
    let mut prs = BTreeSet::new();
    for (table, sql) in [
        (
            "work_links",
            "SELECT pr_number FROM work_links WHERE repo = ?1 AND ticket_number = ?2",
        ),
        (
            "board_prs",
            "SELECT pr_number FROM board_prs WHERE repo = ?1 AND issue_number = ?2 AND pr_repo = ?1",
        ),
    ] {
        if !table_exists(conn, table)? {
            continue;
        }
        let mut stmt = conn.prepare(sql)?;
        for pr in stmt.query_map(params![REPO, number], |row| row.get::<_, i64>(0))? {
            prs.insert(pr?);
        }
    }
    Ok(prs)
}

/// Tickets a PR is linked to (the reverse of [`linked_prs`]).
pub fn tickets_of_pr(conn: &Connection, repo: &str, pr: i64) -> Result<BTreeSet<i64>> {
    let mut tickets = BTreeSet::new();
    if repo != REPO {
        return Ok(tickets);
    }
    for (table, sql) in [
        (
            "work_links",
            "SELECT ticket_number FROM work_links WHERE repo = ?1 AND pr_number = ?2",
        ),
        (
            "board_prs",
            "SELECT issue_number FROM board_prs WHERE pr_repo = ?1 AND pr_number = ?2 AND repo = ?1",
        ),
    ] {
        if !table_exists(conn, table)? {
            continue;
        }
        let mut stmt = conn.prepare(sql)?;
        for ticket in stmt.query_map(params![repo, pr], |row| row.get::<_, i64>(0))? {
            tickets.insert(ticket?);
        }
    }
    Ok(tickets)
}

/// An autopm PR: linked to a ticket under #2170, or some experiment's plan
/// PR. `revert` is true when it is linked to a `kind=revert` ticket, which
/// every limit exempts (C.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrClass {
    pub autopm: bool,
    pub revert: bool,
}

pub fn classify_pr(conn: &Connection, repo: &str, pr: i64) -> Result<PrClass> {
    let repo = crate::work_claims::canonical_repo(repo);
    if repo != REPO {
        return Ok(PrClass {
            autopm: false,
            revert: false,
        });
    }
    init_schema(conn)?;
    let tickets = tickets_of_pr(conn, &repo, pr)?;
    let under = tickets_under_goal(conn)?;
    let plan_pr: bool = conn
        .query_row(
            "SELECT 1 FROM autopm_experiments WHERE plan_pr = ?1 LIMIT 1",
            [pr],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let mut revert = false;
    for ticket in &tickets {
        if fix(conn, *ticket)?.is_some_and(|fix| fix.kind == "revert") {
            revert = true;
        }
    }
    Ok(PrClass {
        autopm: plan_pr || revert || tickets.iter().any(|t| under.contains(t)),
        revert,
    })
}

/// Opens a database read-only, or `None` when it does not exist.
pub fn open_read_only(path: &Path) -> Result<Option<Connection>> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(Some(conn))
}
