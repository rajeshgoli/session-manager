//! Owner activity log (sm#2173; spec `docs/working/2170_auto_pm.html`,
//! Appendix A). One row per owner action: surface, action, target, screen
//! and timing. Never a body, query string, message or prompt text.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Rows older than this are pruned (A.6).
pub const RETENTION_MS: i64 = 180 * 24 * 60 * 60 * 1000;
pub const MAX_LIMIT: usize = 10_000;
pub const DEFAULT_LIMIT: usize = 1_000;
/// Terminal input coalesces to one row per session per this window (A.2).
const TERMINAL_COALESCE: Duration = Duration::from_secs(60);
const MAX_HEADER_CHARS: usize = 128;
/// A request repeating a row this recent writes nothing, so a page polling
/// without `x-sm-bg` shows as one row a minute.
pub const REPEAT_WINDOW_MS: i64 = 60_000;

pub const SURFACES: [&str; 4] = ["android", "web", "agent_prompt", "terminal"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OwnerEvent {
    pub id: i64,
    pub at_ms: i64,
    pub surface: String,
    pub device_id: Option<String>,
    pub action: String,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub screen: Option<String>,
    pub nav: Option<String>,
    pub notice_id: Option<String>,
    pub status: Option<i64>,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub surface: Option<String>,
    pub action: Option<String>,
    pub limit: usize,
    /// Continue strictly after this `(at_ms, id)`.
    pub after: Option<(i64, i64)>,
}

pub struct OwnerEventStore {
    db_path: PathBuf,
}

/// Rows being recorded, per database: token to `at_ms`. A row's time is
/// taken when its request arrives but written after the response, so a
/// reader must not page past a time that may still gain rows.
type InFlight = HashMap<PathBuf, BTreeMap<u64, i64>>;

fn in_flight() -> &'static Mutex<(u64, InFlight)> {
    static IN_FLIGHT: OnceLock<Mutex<(u64, InFlight)>> = OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

/// A row about to be recorded, holding back [`OwnerEventStore::settled_before`]
/// until it is dropped.
pub struct Pending {
    db_path: PathBuf,
    token: u64,
    pub at_ms: i64,
}

impl Drop for Pending {
    fn drop(&mut self) {
        let mut guard = in_flight().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rows) = guard.1.get_mut(&self.db_path) {
            rows.remove(&self.token);
            if rows.is_empty() {
                guard.1.remove(&self.db_path);
            }
        }
    }
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS owner_events (
           id          INTEGER PRIMARY KEY,
           at_ms       INTEGER NOT NULL,
           surface     TEXT NOT NULL,
           device_id   TEXT,
           action      TEXT NOT NULL,
           target_kind TEXT,
           target_id   TEXT,
           screen      TEXT,
           nav         TEXT,
           notice_id   TEXT,
           status      INTEGER,
           duration_ms INTEGER
         );
         CREATE INDEX IF NOT EXISTS owner_events_at ON owner_events(at_ms);
         CREATE INDEX IF NOT EXISTS owner_events_action ON owner_events(action, at_ms);",
    )?;
    Ok(())
}

impl OwnerEventStore {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    /// Takes the time of a row to be recorded. Under one lock with
    /// [`Self::settled_before`], so every later row is at or after what a
    /// reader has been told is settled.
    pub fn begin(&self) -> Pending {
        let mut guard = in_flight().lock().unwrap_or_else(|e| e.into_inner());
        guard.0 += 1;
        let token = guard.0;
        let at_ms = now_ms();
        guard
            .1
            .entry(self.db_path.clone())
            .or_default()
            .insert(token, at_ms);
        Pending {
            db_path: self.db_path.clone(),
            token,
            at_ms,
        }
    }

    /// Rows before this time are final: the earliest row still being
    /// recorded, or now.
    pub fn settled_before(&self) -> i64 {
        let guard = in_flight().lock().unwrap_or_else(|e| e.into_inner());
        guard
            .1
            .get(&self.db_path)
            .and_then(|rows| rows.values().min().copied())
            .unwrap_or_else(now_ms)
    }

    fn open_write(&self) -> Result<Connection> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(&self.db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_schema(&conn)?;
        Ok(conn)
    }

    /// Read connections never create the DB; a missing one reads as empty.
    fn open_read(&self) -> Result<Option<Connection>> {
        if !self.db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let has_table = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'owner_events'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        Ok(has_table.then_some(conn))
    }

    /// Stores `event`; its `id` is ignored and assigned here.
    pub fn record(&self, event: &OwnerEvent) -> Result<i64> {
        Ok(self
            .record_unless_repeat(event, false)?
            .expect("an uncoalesced record always inserts"))
    }

    /// Stores `event`, or with `coalesce` skips it when a row with the same
    /// surface, device, action and target was recorded in the
    /// [`REPEAT_WINDOW_MS`] before it. Returns the new id, if any.
    pub fn record_unless_repeat(&self, event: &OwnerEvent, coalesce: bool) -> Result<Option<i64>> {
        if !SURFACES.contains(&event.surface.as_str()) {
            bail!("unknown surface {}", event.surface);
        }
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if coalesce {
            let repeat = tx
                .query_row(
                    "SELECT 1 FROM owner_events
                     WHERE action = ?1 AND at_ms > ?2 AND surface = ?3 AND device_id IS ?4
                       AND target_kind IS ?5 AND target_id IS ?6
                     LIMIT 1",
                    params![
                        event.action,
                        event.at_ms - REPEAT_WINDOW_MS,
                        event.surface,
                        event.device_id,
                        event.target_kind,
                        event.target_id,
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            if repeat.is_some() {
                return Ok(None);
            }
        }
        tx.execute(
            "INSERT INTO owner_events (at_ms, surface, device_id, action, target_kind,
               target_id, screen, nav, notice_id, status, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                event.at_ms,
                event.surface,
                event.device_id,
                event.action,
                event.target_kind,
                event.target_id,
                event.screen,
                event.nav,
                event.notice_id,
                event.status,
                event.duration_ms,
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Some(id))
    }

    /// Deletes rows older than [`RETENTION_MS`] before `now_ms`.
    pub fn prune(&self, now_ms: i64) -> Result<usize> {
        if self.open_read()?.is_none() {
            return Ok(0);
        }
        let conn = self.open_write()?;
        Ok(conn.execute(
            "DELETE FROM owner_events WHERE at_ms < ?1",
            [now_ms - RETENTION_MS],
        )?)
    }

    /// Rows in `(at_ms, id)` order, and the cursor of the last row when more
    /// remain.
    pub fn list(&self, query: &EventQuery) -> Result<(Vec<OwnerEvent>, Option<String>)> {
        let Some(conn) = self.open_read()? else {
            return Ok((Vec::new(), None));
        };
        let limit = query.limit.clamp(1, MAX_LIMIT);
        let (after_at, after_id) = query.after.unwrap_or((i64::MIN, i64::MIN));
        let mut statement = conn.prepare(
            "SELECT id, at_ms, surface, device_id, action, target_kind, target_id,
                    screen, nav, notice_id, status, duration_ms
             FROM owner_events
             WHERE at_ms >= ?1 AND at_ms < ?2
               AND (?3 IS NULL OR surface = ?3)
               AND (?4 IS NULL OR action = ?4)
               AND (at_ms > ?5 OR (at_ms = ?5 AND id > ?6))
             ORDER BY at_ms, id
             LIMIT ?7",
        )?;
        let mut events = statement
            .query_map(
                params![
                    query.since_ms.unwrap_or(i64::MIN),
                    query.until_ms.unwrap_or(i64::MAX),
                    query.surface,
                    query.action,
                    after_at,
                    after_id,
                    (limit + 1) as i64,
                ],
                |row| {
                    Ok(OwnerEvent {
                        id: row.get(0)?,
                        at_ms: row.get(1)?,
                        surface: row.get(2)?,
                        device_id: row.get(3)?,
                        action: row.get(4)?,
                        target_kind: row.get(5)?,
                        target_id: row.get(6)?,
                        screen: row.get(7)?,
                        nav: row.get(8)?,
                        notice_id: row.get(9)?,
                        status: row.get(10)?,
                        duration_ms: row.get(11)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = events.len() > limit;
        events.truncate(limit);
        let next = more
            .then(|| events.last().map(|e| format!("{}:{}", e.at_ms, e.id)))
            .flatten();
        Ok((events, next))
    }
}

pub fn parse_cursor(value: &str) -> Option<(i64, i64)> {
    let (at, id) = value.split_once(':')?;
    Some((at.parse().ok()?, id.parse().ok()?))
}

pub fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

/// An owner-typed agent prompt (A.2). Never stores the prompt.
pub fn record_agent_prompt(db_path: &Path, session_id: &str) {
    record_session_input(db_path, "agent_prompt", session_id);
}

/// Submitted app-terminal input, coalesced to one row per session per 60 s.
pub fn record_terminal_input(db_path: &Path, session_id: &str) {
    static RECENT: OnceLock<Mutex<HashMap<(PathBuf, String), Instant>>> = OnceLock::new();
    let now = Instant::now();
    {
        let Ok(mut recent) = RECENT.get_or_init(Default::default).lock() else {
            return;
        };
        recent.retain(|_, at| now.duration_since(*at) < TERMINAL_COALESCE);
        let key = (db_path.to_path_buf(), session_id.to_owned());
        if recent.contains_key(&key) {
            return;
        }
        recent.insert(key, now);
    }
    record_session_input(db_path, "terminal", session_id);
}

fn record_session_input(db_path: &Path, surface: &str, session_id: &str) {
    let store = OwnerEventStore::new(db_path.to_path_buf());
    let pending = store.begin();
    let event = OwnerEvent {
        at_ms: pending.at_ms,
        surface: surface.to_owned(),
        action: if surface == "terminal" {
            "terminal.input"
        } else {
            "agent.prompt"
        }
        .to_owned(),
        target_kind: Some("session".to_owned()),
        target_id: Some(session_id.to_owned()),
        ..OwnerEvent::default()
    };
    if let Err(error) = store.record(&event) {
        eprintln!("owner event {surface} for {session_id} not recorded: {error:#}");
    }
    drop(pending);
}

/// `x-sm-nav` (A.5): `push:<notice_id>`, `launch`, `link` or `nav`; anything
/// else stores no nav.
pub fn parse_nav(value: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(value) = value.map(str::trim) else {
        return (None, None);
    };
    if let Some(notice) = value.strip_prefix("push:") {
        let notice = notice.trim();
        return (
            Some("push".to_owned()),
            (!notice.is_empty()).then(|| clip(notice)),
        );
    }
    match value {
        "push" | "launch" | "link" | "nav" => (Some(value.to_owned()), None),
        _ => (None, None),
    }
}

/// Header values are client-supplied; keep them short.
pub fn clip(value: &str) -> String {
    value.trim().chars().take(MAX_HEADER_CHARS).collect()
}

/// Routes the clients poll; without any client header these are background
/// (A.4).
const POLL_ROUTES: [&str; 6] = [
    "/watch/state",
    "/client/host-status",
    "/client/board/badge",
    "/client/utilization/series",
    "/client/usage/meters",
    "/client/queue/stats",
];

/// Static files a page pulls in as it loads: the web app's modules and a
/// doc's images beside it. The owner did not ask for them one by one.
fn is_subresource(route: &str, params: &[String]) -> bool {
    match (route, params) {
        ("/assets/{*name}", _) => true,
        ("/docs/{doc_id}/{*rest}", [first, rest]) => {
            !crate::owner_docs::is_owner_doc_id(first) && !is_doc_page(rest)
        }
        _ => false,
    }
}

fn is_doc_page(rest: &str) -> bool {
    let name = rest.rsplit('/').next().unwrap_or(rest);
    match name.rsplit_once('.') {
        Some((_, extension)) => {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "html" | "htm" | "md"
            )
        }
        None => true,
    }
}

/// A.4: `x-sm-bg: 1`, or no client header and a polled route.
pub fn is_background(
    route: &str,
    params: &[String],
    bg: Option<&str>,
    has_client_header: bool,
) -> bool {
    if bg.map(str::trim) == Some("1") {
        return true;
    }
    (!has_client_header && POLL_ROUTES.contains(&route)) || is_subresource(route, params)
}

/// One owner action, resolved from method and route template (A.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAction {
    pub action: String,
    pub target_kind: Option<&'static str>,
    pub target_id: Option<String>,
}

/// `(method, route template, action, target kind)` for every route the
/// router registers; path parameters fill the target id. A route missing here records its template.
#[rustfmt::skip]
pub const ACTIONS: &[(&str, &str, &str, Option<&str>)] = &[
    ("GET", "/backup/status", "backup.summary", None),
    ("POST", "/backup/requests", "backup.request", None),
    ("GET", "/backup/requests/{id}", "backup.status", Some("backup_request")),
    ("GET", "/backup/requests/{id}/parts/{n}", "backup.part", Some("backup_request")),
    ("POST", "/backup/requests/{id}/ack", "backup.ack", Some("backup_request")),
    ("POST", "/backup/probe", "backup.probe", None),
    ("POST", "/backup/renew", "backup.renew", None),
    ("GET", "/backup/cli", "backup.cli", None),
    ("GET", "/backup/cli/binary", "backup.cli_binary", None),
    ("POST", "/backup/allow-deletes", "backup.allow_deletes", None),
    ("POST", "/artifacts/{name}/owner-action", "artifact.owner_action", Some("artifact")),
    ("POST", "/autopm/ask", "autopm.ask", None),
    ("POST", "/autopm/issues", "autopm.issue_create", None),
    ("PUT", "/autopm/auto-start", "autopm.auto_start", None),
    ("GET", "/health", "server.health", None),
    ("GET", "/health/detailed", "server.health_detailed", None),
    ("GET", "/auth/session", "auth.session", None),
    ("POST", "/auth/device/google", "auth.device_login", None),
    ("POST", "/hooks/claude", "hook.claude", None),
    ("POST", "/hooks/tool-use", "hook.tool_use", None),
    ("POST", "/hooks/tmux-client", "hook.tmux_client", None),
    ("POST", "/hooks/context-usage", "hook.context_usage", None),
    ("GET", "/client/bootstrap", "app.bootstrap", None),
    ("GET", "/client/session-models", "session.models", None),
    ("GET", "/client/host-status", "host.status", None),
    ("GET", "/client/model", "model.status", None),
    ("POST", "/client/model/load", "model.load", None),
    ("POST", "/client/model/unload", "model.unload", None),
    ("GET", "/client/queue", "queue.open", None),
    ("GET", "/client/queue/stats", "queue.stats", None),
    ("GET", "/client/queue/jobs/{job_id}/start-check", "queue.start_check", Some("queue_job")),
    ("POST", "/client/queue/jobs/{job_id}/start", "queue.start", Some("queue_job")),
    ("GET", "/client/queue/jobs/{job_id}/usage", "queue.job_usage", Some("queue_job")),
    ("GET", "/client/utilization/series", "utilization.series", None),
    ("GET", "/client/analytics/spend", "analytics.spend", None),
    ("GET", "/client/usage/meters", "usage.meters", None),
    ("GET", "/client/analytics/time", "analytics.time", None),
    ("PUT", "/client/push-token", "push.register", None),
    ("DELETE", "/client/push-token", "push.unregister", None),
    ("GET", "/client/push/status", "push.status", None),
    ("POST", "/client/push/test", "push.test", None),
    ("GET", "/client/follows", "follow.list", None),
    ("POST", "/client/follows/{follow_id}/ack", "follow.ack", Some("follow")),
    ("GET", "/client/notices", "notice.list", None),
    ("POST", "/client/notices/{notice_id}/ack", "notice.ack", Some("notice")),
    ("POST", "/sessions/{session_id}/follow", "session.follow", Some("session")),
    ("DELETE", "/sessions/{session_id}/follow", "session.unfollow", Some("session")),
    ("POST", "/queue-jobs/{job_id}/follow", "queue.follow", Some("queue_job")),
    ("DELETE", "/queue-jobs/{job_id}/follow", "queue.unfollow", Some("queue_job")),
    ("POST", "/client/bug-reports", "bug.report", None),
    ("GET", "/client/bug-reports/options", "bug.options", None),
    ("GET", "/bug-reports/{bug_id}", "bug.open", Some("bug")),
    ("GET", "/bug-reports/{bug_id}/screenshot.png", "bug.screenshot", Some("bug")),
    ("GET", "/bugs/{bug_id}", "bug.open", Some("bug")),
    ("GET", "/bugs/{bug_id}/screenshot.png", "bug.screenshot", Some("bug")),
    ("GET", "/client/terminal", "terminal.attach", None),
    ("GET", "/client/terminal/probe", "terminal.probe", None),
    ("POST", "/client/mobile-terminal/disable", "terminal.disable", None),
    ("GET", "/admin/studio-ssh", "admin.studio_ssh", None),
    ("POST", "/admin/studio-ssh", "admin.studio_ssh_set", Some("setting")),
    ("GET", "/client/mobile-terminal/devices", "terminal.devices", None),
    ("DELETE", "/client/mobile-terminal/devices/{device_key_id}", "terminal.device_remove", Some("device")),
    ("POST", "/deploy/{app_name}", "app.deploy", Some("app")),
    ("GET", "/apps/{app_name}/latest.apk", "app.download", Some("app")),
    ("GET", "/apps/{app_name}/meta.json", "app.meta", Some("app")),
    ("GET", "/apps/{app_name}/{artifact_file}", "app.artifact", Some("app")),
    ("GET", "/apk", "app.download", None),
    ("GET", "/events/state", "events.state", None),
    ("GET", "/events", "events.stream", None),
    ("POST", "/__shadow/http", "shadow.http", None),
    ("GET", "/session-obligations", "session.obligations", None),
    ("GET", "/queue-jobs", "queue.jobs", None),
    ("POST", "/queue-jobs", "queue.submit", None),
    ("GET", "/queue-jobs/{job_id}", "queue.job", Some("queue_job")),
    ("DELETE", "/queue-jobs/{job_id}", "queue.delete", Some("queue_job")),
    ("GET", "/queue-jobs/{job_id}/log", "queue.log", Some("queue_job")),
    ("POST", "/queue-jobs/{job_id}/cancel", "queue.cancel", Some("queue_job")),
    ("GET", "/review-requests", "review.list", None),
    ("POST", "/review-requests", "review.request", None),
    ("GET", "/review-policies", "review.policies", None),
    ("PUT", "/review-policies", "review.policy_set", Some("setting")),
    ("GET", "/review-requests/{request_id}", "review.open", Some("review")),
    ("DELETE", "/review-requests/{request_id}", "review.cancel", Some("review")),
    ("POST", "/review-requests/{request_id}/submit", "review.submit", Some("review")),
    ("GET", "/client/reviews/status", "review.status", None),
    ("POST", "/client/reviews/github-codex/check", "review.codex_check", None),
    ("POST", "/client/review-requests/{request_id}/retry", "review.retry", Some("review")),
    ("POST", "/client/review-requests/{request_id}/owner", "review.owner", Some("review")),
    ("POST", "/client/review-requests/{request_id}/dismiss", "review.dismiss", Some("review")),
    ("GET", "/btw-requests/{request_id}", "btw.open", Some("btw")),
    ("GET", "/docs", "doc.list", None),
    ("POST", "/docs", "doc.publish", None),
    ("GET", "/merge-holds", "merge_hold.list", None),
    ("POST", "/merge-holds", "merge_hold.place", None),
    ("POST", "/merge-holds/release", "merge_hold.release", None),
    ("GET", "/artifacts", "artifact.list", None),
    ("POST", "/artifacts", "artifact.create", None),
    ("GET", "/artifacts/{name}", "artifact.open", Some("artifact")),
    ("POST", "/artifacts/{name}/{action}", "artifact.action", Some("artifact")),
    ("GET", "/claims", "claim.list", None),
    ("POST", "/claims", "claim.take", None),
    ("GET", "/board", "board.read", None),
    ("PUT", "/board/not-before", "board.not_before", None),
    ("PUT", "/board/waiting", "board.waiting", None),
    ("GET", "/notes", "note.list", None),
    ("POST", "/notes", "note.create", None),
    ("GET", "/notes/search", "note.search", None),
    ("POST", "/notes/preview", "note.preview", None),
    ("POST", "/notes/import", "note.import", None),
    ("GET", "/notes/{id}", "note.open", Some("note")),
    ("PUT", "/notes/{id}", "note.save", Some("note")),
    ("DELETE", "/notes/{id}", "note.delete", Some("note")),
    ("GET", "/notes/{id}/revisions", "note.revisions", Some("note")),
    ("GET", "/notes/{id}/revisions/{version}", "note.revision", Some("note")),
    ("POST", "/notes/{id}/restore", "note.restore", Some("note")),
    ("POST", "/github/issues", "ticket.create", None),
    ("POST", "/board/links", "board.link", None),
    ("POST", "/board/lanes", "board.lane_add", None),
    ("GET", "/client/board", "board.open", None),
    ("POST", "/client/board/launch-preview", "board.launch_preview", None),
    ("PUT", "/client/board/launch-selection", "board.launch_selection", None),
    ("GET", "/client/board/launch-default", "board.launch_default", None),
    ("PUT", "/client/board/launch-default", "board.launch_default_set", Some("setting")),
    ("POST", "/client/board/start", "board.start", None),
    ("PUT", "/client/board/auto-start", "board.auto_start", None),
    ("DELETE", "/client/board/auto-start", "board.auto_start_off", None),
    ("PUT", "/client/board/auto-start/lane", "board.auto_start_lane", Some("lane")),
    ("POST", "/client/board/close", "board.close", None),
    ("GET", "/client/github/{owner}/{repo}/{number}", "ticket.open", Some("ticket")),
    ("GET", "/client/board/start-options", "board.start_options", None),
    ("GET", "/client/settings", "settings.open", None),
    ("PUT", "/client/settings", "settings.change", Some("setting")),
    ("GET", "/client/devices", "device.list", None),
    ("POST", "/client/devices/enroll", "device.enroll", None),
    ("DELETE", "/client/devices/{device_key_id}", "device.remove", Some("device")),
    ("PUT", "/client/board/order", "board.order", None),
    ("POST", "/client/board/lanes", "board.lane_add", None),
    ("DELETE", "/client/board/lanes/{lane_id}", "board.lane_remove", Some("lane")),
    ("GET", "/client/board/badge", "board.badge", None),
    ("POST", "/client/board/seen", "board.seen", None),
    ("POST", "/client/board/refresh", "board.refresh", None),
    ("POST", "/claims/release", "claim.release", None),
    ("POST", "/claims/worktree", "claim.worktree", None),
    ("POST", "/worktrees/keep", "worktree.keep", None),
    ("POST", "/worktrees/delete", "worktree.delete", None),
    ("GET", "/worktrees/leftover", "worktree.leftover", None),
    ("GET", "/history", "history.open", None),
    ("GET", "/history/agents", "history.agents", None),
    ("GET", "/history/tickets", "history.tickets", None),
    ("GET", "/guestbook", "guestbook.open", None),
    ("GET", "/t/{repo}/{number}", "ticket.timeline", Some("ticket")),
    ("GET", "/", "web.home", None),
    ("GET", "/watch", "watch.open", None),
    ("GET", "/watch/state", "watch.state", None),
    ("GET", "/queue", "web.queue", None),
    ("GET", "/analytics", "web.analytics", None),
    ("GET", "/analytics/{view}", "web.analytics", Some("view")),
    ("GET", "/settings", "web.settings", None),
    ("GET", "/terminal/{session_id}", "web.terminal", Some("session")),
    ("GET", "/assets/{*name}", "web.asset", None),
    ("GET", "/docs/{doc_id}", "doc.open", Some("doc")),
    ("GET", "/docs/{doc_id}/{*rest}", "doc.open", Some("doc")),
    ("POST", "/docs/{doc_id}/{*rest}", "doc.action", Some("doc")),
    ("PATCH", "/docs/{doc_id}/{*rest}", "doc.draft_edit", Some("doc")),
    ("DELETE", "/docs/{doc_id}/{*rest}", "doc.draft_delete", Some("doc")),
    ("POST", "/scheduler/remind", "reminder.set", None),
    ("DELETE", "/scheduler/remind/{reminder_id}", "reminder.cancel", Some("reminder")),
    ("GET", "/nodes", "node.list", None),
    ("POST", "/nodes/{node_id}/ping", "node.ping", Some("node")),
    ("GET", "/nodes/{node_id}/restore-candidates", "node.restore_candidates", Some("node")),
    ("POST", "/nodes/{node_id}/restore-candidates/{session_id}/restore", "node.restore", Some("session")),
    ("GET", "/humans", "human.list", None),
    ("GET", "/humans/{identifier}", "human.open", Some("human")),
    ("POST", "/humans/{identifier}/email", "human.email", Some("human")),
    ("POST", "/humans/{identifier}/messages", "human.message", Some("human")),
    ("GET", "/inbox", "inbox.open", None),
    ("POST", "/inbox/done", "inbox.done", None),
    ("POST", "/inbox/archive", "inbox.archive", None),
    ("POST", "/inbox/unarchive", "inbox.unarchive", None),
    ("GET", "/inbox/thread/{key}", "inbox.thread", Some("thread")),
    ("POST", "/inbox/thread/{key}/send", "inbox.reply", Some("thread")),
    ("GET", "/inbox/agent/{session_id}", "inbox.agent", Some("session")),
    ("POST", "/inbox/agent/{session_id}/send", "inbox.reply", Some("session")),
    ("GET", "/messages/{message_id}", "message.open", Some("message")),
    ("GET", "/messages/{message_id}/drafts", "message.drafts", Some("message")),
    ("POST", "/messages/{message_id}/drafts", "message.draft_save", Some("message")),
    ("PATCH", "/messages/{message_id}/drafts/{draft_id}", "message.draft_edit", Some("message")),
    ("DELETE", "/messages/{message_id}/drafts/{draft_id}", "message.draft_delete", Some("message")),
    ("POST", "/messages/{message_id}/reply", "message.reply", Some("message")),
    ("POST", "/messages/{message_id}/handled", "message.handled", Some("message")),
    ("POST", "/email/send", "email.send", None),
    ("POST", "/api/email-inbound", "email.inbound", None),
    ("GET", "/usage/accounts", "usage.accounts", None),
    ("GET", "/sessions", "session.list", None),
    ("POST", "/sessions", "session.create", None),
    ("GET", "/reparent-requests", "reparent.list", None),
    ("GET", "/session-credential-rotations", "session.credential_rotations", None),
    ("GET", "/reparent-requests/{request_id}", "reparent.open", Some("reparent")),
    ("POST", "/reparent-requests/{request_id}/approve", "reparent.approve", Some("reparent")),
    ("POST", "/reparent-requests/{request_id}/reject", "reparent.reject", Some("reparent")),
    ("POST", "/reparent-requests/{request_id}/human-approve", "reparent.owner_approve", Some("reparent")),
    ("POST", "/reparent-requests/{request_id}/human-reject", "reparent.owner_reject", Some("reparent")),
    ("POST", "/reparent-requests/{request_id}/repair", "reparent.repair", Some("reparent")),
    ("POST", "/sessions/input-batch", "session.input_batch", None),
    ("POST", "/sessions/spawn", "session.spawn", None),
    ("GET", "/sessions/context-monitor", "session.context_monitors", None),
    ("GET", "/sessions/{session_id}", "session.read", Some("session")),
    ("PATCH", "/sessions/{session_id}", "session.update", Some("session")),
    ("GET", "/sessions/{session_id}/context", "session.context", Some("session")),
    ("POST", "/sessions/{session_id}/reparent-requests", "session.reparent", Some("session")),
    ("POST", "/sessions/{session_id}/reparent-tree-requests", "session.reparent_tree", Some("session")),
    ("POST", "/sessions/{session_id}/credential-rotation", "session.recredential", Some("session")),
    ("GET", "/sessions/{session_id}/usage", "session.usage", Some("session")),
    ("GET", "/sessions/{parent_session_id}/children", "session.children", Some("session")),
    ("GET", "/sessions/{session_id}/root", "session.root", Some("session")),
    ("GET", "/sessions/{session_id}/attach-descriptor", "session.attach_descriptor", Some("session")),
    ("POST", "/sessions/{session_id}/agent-status", "session.status", Some("session")),
    ("POST", "/sessions/{session_id}/task-complete", "session.task_complete", Some("session")),
    ("POST", "/sessions/{session_id}/turn-complete", "session.turn_complete", Some("session")),
    ("POST", "/sessions/{session_id}/needs-you/answered", "session.answered", Some("session")),
    ("GET", "/sessions/{session_id}/last-turn", "session.last_turn", Some("session")),
    ("PUT", "/sessions/{session_id}/note", "session.note", Some("session")),
    ("POST", "/sessions/{session_id}/notify-on-stop", "session.notify_on_stop", Some("session")),
    ("POST", "/sessions/{session_id}/context-monitor", "session.context_monitor", Some("session")),
    ("GET", "/sessions/{session_id}/subagents", "session.subagents", Some("session")),
    ("POST", "/sessions/{session_id}/subagents", "session.subagent_start", Some("session")),
    ("POST", "/sessions/{session_id}/subagents/{agent_id}/stop", "session.subagent_stop", Some("session")),
    ("POST", "/sessions/{session_id}/input", "session.input", Some("session")),
    ("POST", "/sessions/{session_id}/what", "session.what", Some("session")),
    ("POST", "/sessions/{session_id}/retire", "session.retire", Some("session")),
    ("POST", "/sessions/{session_id}/kill", "session.kill", Some("session")),
    ("POST", "/sessions/{session_id}/restore", "session.restore", Some("session")),
    ("GET", "/host-restarts/latest", "restart.latest", None),
    ("POST", "/host-restarts/{restart_id}/restore", "restart.restore", Some("restart")),
    ("POST", "/host-restarts/{restart_id}/members/{session_id}/leave", "restart.leave", Some("session")),
    ("POST", "/sessions/{session_id}/clear", "session.clear", Some("session")),
    ("POST", "/sessions/{session_id}/handoff", "session.handoff", Some("session")),
    ("GET", "/sessions/{session_id}/handoff-policy", "handoff.policy", Some("session")),
    ("PUT", "/sessions/{session_id}/handoff-policy", "handoff.policy_set", Some("session")),
    ("GET", "/handoff-defaults", "handoff.defaults", None),
    ("PUT", "/handoff-defaults", "handoff.defaults_set", Some("setting")),
    ("GET", "/handoff-policy/ticket/{owner}/{repo}/{number}", "handoff.ticket_policy", Some("ticket")),
    ("PUT", "/handoff-policy/ticket/{owner}/{repo}/{number}", "handoff.ticket_policy_set", Some("ticket")),
    ("PUT", "/sessions/{session_id}/maintainer", "session.maintainer", Some("session")),
    ("DELETE", "/sessions/{session_id}/maintainer", "session.maintainer_clear", Some("session")),
    ("POST", "/sessions/{session_id}/registry", "session.register", Some("session")),
    ("DELETE", "/sessions/{session_id}/registry", "session.unregister", Some("session")),
    ("GET", "/registry", "registry.list", None),
    ("GET", "/registry/{role}", "registry.lookup", Some("role")),
    ("GET", "/sessions/{session_id}/output", "session.output", Some("session")),
    ("GET", "/sessions/{session_id}/tool-calls", "session.tool_calls", Some("session")),
    ("GET", "/sessions/{session_id}/activity-actions", "session.activity", Some("session")),
    ("GET", "/sessions/{session_id}/codex-events", "session.codex_events", Some("session")),
    ("GET", "/sessions/{session_id}/codex-pending-requests", "session.codex_requests", Some("session")),
    ("GET", "/client/sessions", "session.list", None),
    ("POST", "/client/sessions", "session.create", None),
    ("POST", "/client/sessions/{session_id}/attach-ticket", "terminal.ticket", Some("session")),
    ("POST", "/client/sessions/{session_id}/browser-attach-ticket", "terminal.ticket", Some("session")),
    ("GET", "/client/sessions/{session_id}", "session.open", Some("session")),
    ("GET", "/autopm/events", "autopm.events", None),
    ("GET", "/autopm/switches", "autopm.switches", None),
    ("PUT", "/autopm/switches/{key}", "autopm.switch", Some("setting")),
    ("GET", "/autopm/experiments", "autopm.experiments", None),
    ("POST", "/autopm/experiments", "autopm.experiment_create", None),
    ("GET", "/autopm/experiments/{id}", "autopm.experiment", Some("experiment")),
    ("PATCH", "/autopm/experiments/{id}", "autopm.experiment_set", Some("experiment")),
    ("POST", "/autopm/experiments/{id}/live", "autopm.experiment_live", Some("experiment")),
    ("GET", "/autopm/budget", "autopm.budget", None),
    ("GET", "/client/autopm", "autopm.view", None),
    ("POST", "/client/autopm/feedback", "autopm.feedback", None),
];

/// The action for a request: the mapped name, else its template (A.3).
pub fn resolve(method: &str, route: &str, params: &[String]) -> ResolvedAction {
    let method = if method == "HEAD" { "GET" } else { method };
    let entry = ACTIONS
        .iter()
        .find(|(m, r, _, _)| *m == method && *r == route);
    let Some((_, _, action, kind)) = entry else {
        return ResolvedAction {
            action: format!("{method} {route}"),
            target_kind: None,
            target_id: None,
        };
    };
    let mut action = (*action).to_owned();
    let target_id = match route {
        "/client/github/{owner}/{repo}/{number}"
        | "/handoff-policy/ticket/{owner}/{repo}/{number}" => {
            (params.len() == 3).then(|| format!("{}/{}#{}", params[0], params[1], params[2]))
        }
        "/t/{repo}/{number}" => (params.len() == 2).then(|| format!("{}#{}", params[0], params[1])),
        "/nodes/{node_id}/restore-candidates/{session_id}/restore"
        | "/host-restarts/{restart_id}/members/{session_id}/leave" => params.get(1).cloned(),
        "/docs/{doc_id}/{*rest}" if params.len() == 2 => {
            let (first, rest) = (&params[0], &params[1]);
            if crate::owner_docs::is_owner_doc_id(first) {
                // `/docs/{id}/view` opens; the other subroutes name themselves.
                const SUBROUTES: [&str; 14] = [
                    "raw",
                    "head",
                    "drafts",
                    "reopen-target",
                    "ask-target",
                    "ask-items",
                    "retract",
                    "review",
                    "dismiss-review",
                    "assign",
                    "merge-hold",
                    "agent/retire",
                    "ask",
                    "view",
                ];
                if method == "GET" && rest == "view" {
                    action = "doc.open".to_owned();
                } else if matches!(method, "GET" | "POST") && SUBROUTES.contains(&rest.as_str()) {
                    action = format!("doc.{}", rest.replace(['-', '/'], "_"));
                }
                Some(first.clone())
            } else {
                // The readable reader: `/docs/<repo-name>/<path>`.
                Some(format!("{first}/{rest}"))
            }
        }
        _ => params.first().cloned(),
    };
    ResolvedAction {
        action,
        target_kind: *kind,
        target_id: target_id.filter(|id| !id.is_empty()),
    }
}

#[cfg(test)]
mod tests;
