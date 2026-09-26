//! Owner follows and phone notifications (sm#1569).
//!
//! The owner follows an agent or a queue job from the Android app. When the
//! agent runs `sm task-complete`, its session ends, or the job reaches a
//! terminal state, the follow fires once. A background worker then resolves
//! the agent's completion report, pushes one notification to the owner's
//! phones through Firebase Cloud Messaging, and falls back to email when no
//! phone confirms receipt. Spec: `specs/1569_follow_agent_notifications.html`.

use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{Context, Result};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

/// How long delivery waits for a report published after `sm task-complete`.
pub const REPORT_GRACE: Duration = Duration::seconds(120);
/// A push the phone has not acknowledged by then is repeated by email.
pub const ACK_FALLBACK: Duration = Duration::minutes(15);
/// Fired follows stay listed for the app this long.
pub const RECENT_FIRED_WINDOW: Duration = Duration::days(7);
/// Marks the message sm sends a followed agent on the owner's behalf.
pub const FOLLOW_MESSAGE_MARKER: &str = "[sm follow]";
/// Delay after each failed push attempt; the attempt after the last falls
/// back to email.
const PUSH_RETRY_DELAYS_SECONDS: [i64; 5] = [30, 60, 120, 300, 600];
/// A transient email failure is retried this often...
const EMAIL_RETRY_DELAY: Duration = Duration::minutes(5);
/// ...for this long after the email first became due.
const EMAIL_RETRY_WINDOW: Duration = Duration::hours(1);
/// Publishes scanned when looking for a follow's report.
pub const REPORT_SCAN_LIMIT: usize = 50;

pub const TARGET_SESSION: &str = "session";
pub const TARGET_QUEUE_JOB: &str = "queue_job";
pub const REASON_TASK_COMPLETE: &str = "task_complete";
pub const REASON_SESSION_ENDED: &str = "session_ended";
pub const REASON_JOB_FINISHED: &str = "job_finished";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Follow {
    pub id: String,
    #[serde(skip)]
    pub user_id: String,
    pub target_kind: String,
    pub session_id: String,
    pub session_name: String,
    pub job_id: Option<String>,
    pub job_label: Option<String>,
    pub job_state: Option<String>,
    pub job_exit_code: Option<i64>,
    pub job_started_at: Option<String>,
    pub job_finished_at: Option<String>,
    #[serde(skip)]
    pub message_text: Option<String>,
    pub created_at: String,
    pub cancelled_at: Option<String>,
    pub fired_at: Option<String>,
    pub fire_reason: Option<String>,
    pub report_doc_id: Option<String>,
    pub report_title: Option<String>,
    pub report_reader_path: Option<String>,
    pub notify_after: Option<String>,
    pub push_attempts: i64,
    pub last_push_error: Option<String>,
    pub notified_at: Option<String>,
    pub notified_via: Option<String>,
    pub acked_at: Option<String>,
    pub email_sent_at: Option<String>,
}

impl Follow {
    /// Lifecycle state (spec appendix A), derived from the columns.
    pub fn state(&self) -> &'static str {
        if self.cancelled_at.is_some() {
            "cancelled"
        } else if self.fired_at.is_none() {
            "active"
        } else if self.acked_at.is_some() {
            "acked"
        } else if self.notified_at.is_some() {
            "notified"
        } else {
            "fired"
        }
    }

    pub fn is_active(&self) -> bool {
        self.cancelled_at.is_none() && self.fired_at.is_none()
    }

    fn is_job(&self) -> bool {
        self.target_kind == TARGET_QUEUE_JOB
    }
}

/// What a new follow targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowTarget {
    Session {
        session_id: String,
        session_name: String,
    },
    QueueJob {
        job_id: String,
        job_label: String,
        session_id: String,
        session_name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTokenRegistration {
    pub user_id: String,
    pub token: String,
    pub device_id: Option<String>,
    pub device_name: String,
    pub app_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushToken {
    pub user_id: String,
    pub token: String,
    pub device_id: Option<String>,
    pub device_name: String,
}

/// A job's terminal outcome, copied onto the follow when it fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOutcome {
    pub state: String,
    pub exit_code: Option<i64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

pub struct OwnerPushStore {
    db_path: PathBuf,
}

const FOLLOW_COLUMNS: &str = "id, user_id, target_kind, session_id, session_name, job_id, \
     job_label, job_state, job_exit_code, job_started_at, job_finished_at, message_text, \
     created_at, cancelled_at, fired_at, fire_reason, report_doc_id, report_title, \
     report_reader_path, notify_after, push_attempts, last_push_error, notified_at, \
     notified_via, acked_at, email_sent_at";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS push_tokens (
  user_id      TEXT NOT NULL,
  fcm_token    TEXT NOT NULL,
  device_id    TEXT,
  device_name  TEXT NOT NULL,
  app_version  TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL,
  invalid_at   TEXT,
  last_error   TEXT,
  PRIMARY KEY (user_id, fcm_token)
);
CREATE TABLE IF NOT EXISTS owner_follows (
  id                  TEXT PRIMARY KEY,
  user_id             TEXT NOT NULL,
  target_kind         TEXT NOT NULL,
  session_id          TEXT NOT NULL,
  session_name        TEXT NOT NULL,
  job_id              TEXT,
  job_label           TEXT,
  job_state           TEXT,
  job_exit_code       INTEGER,
  job_started_at      TEXT,
  job_finished_at     TEXT,
  message_text        TEXT,
  created_at          TEXT NOT NULL,
  cancelled_at        TEXT,
  fired_at            TEXT,
  fire_reason         TEXT,
  report_doc_id       TEXT,
  report_title        TEXT,
  report_reader_path  TEXT,
  notify_after        TEXT,
  push_attempts       INTEGER NOT NULL DEFAULT 0,
  last_push_error     TEXT,
  notified_at         TEXT,
  notified_via        TEXT,
  acked_at            TEXT,
  email_sent_at       TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS owner_follows_one_active_session
  ON owner_follows(user_id, session_id)
  WHERE target_kind = 'session' AND fired_at IS NULL AND cancelled_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS owner_follows_one_active_job
  ON owner_follows(user_id, job_id)
  WHERE target_kind = 'queue_job' AND fired_at IS NULL AND cancelled_at IS NULL;
CREATE INDEX IF NOT EXISTS owner_follows_pending ON owner_follows(fired_at, notified_at);
"#;

impl OwnerPushStore {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open(&self) -> Result<Connection> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open {}", self.db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(SCHEMA)?;
        Ok(conn)
    }

    pub fn upsert_token(
        &self,
        registration: &PushTokenRegistration,
        now: OffsetDateTime,
    ) -> Result<()> {
        let now = format_ts(now);
        self.open()?.execute(
            "INSERT INTO push_tokens (user_id, fcm_token, device_id, device_name, app_version, \
               created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6) \
             ON CONFLICT(user_id, fcm_token) DO UPDATE SET device_id = excluded.device_id, \
               device_name = excluded.device_name, app_version = excluded.app_version, \
               updated_at = excluded.updated_at, invalid_at = NULL, last_error = NULL",
            params![
                registration.user_id,
                registration.token,
                registration.device_id,
                registration.device_name,
                registration.app_version,
                now
            ],
        )?;
        Ok(())
    }

    pub fn delete_token(&self, user_id: &str, token: &str) -> Result<()> {
        self.open()?.execute(
            "DELETE FROM push_tokens WHERE user_id = ?1 AND fcm_token = ?2",
            params![user_id, token],
        )?;
        Ok(())
    }

    /// Revoking a mobile device removes the push tokens it registered.
    pub fn delete_device_tokens(&self, device_id: &str) -> Result<usize> {
        Ok(self.open()?.execute(
            "DELETE FROM push_tokens WHERE device_id = ?1",
            params![device_id],
        )?)
    }

    pub fn valid_tokens(&self, user_id: &str) -> Result<Vec<PushToken>> {
        let conn = self.open()?;
        let mut statement = conn.prepare(
            "SELECT user_id, fcm_token, device_id, device_name FROM push_tokens \
             WHERE user_id = ?1 AND invalid_at IS NULL ORDER BY updated_at DESC",
        )?;
        let rows = statement
            .query_map(params![user_id], |row| {
                Ok(PushToken {
                    user_id: row.get(0)?,
                    token: row.get(1)?,
                    device_id: row.get(2)?,
                    device_name: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn record_token_error(
        &self,
        token: &PushToken,
        error: &str,
        invalid_at: Option<OffsetDateTime>,
    ) -> Result<()> {
        self.open()?.execute(
            "UPDATE push_tokens SET last_error = ?3, invalid_at = COALESCE(?4, invalid_at) \
             WHERE user_id = ?1 AND fcm_token = ?2",
            params![token.user_id, token.token, error, invalid_at.map(format_ts)],
        )?;
        Ok(())
    }

    /// Creates the follow, or returns the active one for the same owner and
    /// target with `false`.
    pub fn create_follow(
        &self,
        user_id: &str,
        target: &FollowTarget,
        message_text: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<(Follow, bool)> {
        let mut conn = self.open()?;
        let tx = conn.transaction()?;
        if let Some(existing) = active_follow_conn(&tx, user_id, target)? {
            return Ok((existing, false));
        }
        let id = allocate_follow_id(&tx)?;
        let now = format_ts(now);
        let (kind, session_id, session_name, job_id, job_label) = match target {
            FollowTarget::Session {
                session_id,
                session_name,
            } => (TARGET_SESSION, session_id, session_name, None, None),
            FollowTarget::QueueJob {
                job_id,
                job_label,
                session_id,
                session_name,
            } => (
                TARGET_QUEUE_JOB,
                session_id,
                session_name,
                Some(job_id),
                Some(job_label),
            ),
        };
        tx.execute(
            "INSERT INTO owner_follows (id, user_id, target_kind, session_id, session_name, \
               job_id, job_label, message_text, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                user_id,
                kind,
                session_id,
                session_name,
                job_id,
                job_label,
                message_text,
                now
            ],
        )?;
        let follow = get_follow_conn(&tx, &id)?.context("inserted follow disappeared")?;
        tx.commit()?;
        Ok((follow, true))
    }

    /// Removes a follow outright; used when telling the agent failed.
    pub fn delete_follow(&self, id: &str) -> Result<()> {
        self.open()?
            .execute("DELETE FROM owner_follows WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Cancels the active follow of `target_id` (a session id or job id).
    /// A fired follow is already committed and stays as it is.
    pub fn cancel_active(
        &self,
        user_id: &str,
        target_kind: &str,
        target_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let column = if target_kind == TARGET_QUEUE_JOB {
            "job_id"
        } else {
            "session_id"
        };
        let changed = self.open()?.execute(
            &format!(
                "UPDATE owner_follows SET cancelled_at = ?4 WHERE user_id = ?1 \
                 AND target_kind = ?2 AND {column} = ?3 AND fired_at IS NULL \
                 AND cancelled_at IS NULL"
            ),
            params![user_id, target_kind, target_id, format_ts(now)],
        )?;
        Ok(changed > 0)
    }

    pub fn get(&self, id: &str) -> Result<Option<Follow>> {
        get_follow_conn(&self.open()?, id)
    }

    /// Active follows plus follows fired within [`RECENT_FIRED_WINDOW`],
    /// newest first.
    pub fn list_for_owner(&self, user_id: &str, now: OffsetDateTime) -> Result<Vec<Follow>> {
        let cutoff = now - RECENT_FIRED_WINDOW;
        let mut follows = self
            .query(
                "WHERE user_id = ?1 AND cancelled_at IS NULL",
                params![user_id],
            )?
            .into_iter()
            .filter(|follow| match follow.fired_at.as_deref() {
                None => true,
                Some(fired_at) => parse_ts(fired_at).is_some_and(|fired_at| fired_at >= cutoff),
            })
            .collect::<Vec<_>>();
        follows.sort_by(|left, right| {
            let key = |follow: &Follow| parse_ts(&follow.created_at);
            key(right)
                .cmp(&key(left))
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(follows)
    }

    pub fn ack(&self, user_id: &str, id: &str, now: OffsetDateTime) -> Result<bool> {
        let conn = self.open()?;
        let exists = conn
            .query_row(
                "SELECT 1 FROM owner_follows WHERE id = ?1 AND user_id = ?2",
                params![id, user_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            conn.execute(
                "UPDATE owner_follows SET acked_at = ?2 WHERE id = ?1 AND acked_at IS NULL",
                params![id, format_ts(now)],
            )?;
        }
        Ok(exists)
    }

    pub fn active(&self) -> Result<Vec<Follow>> {
        self.query("WHERE fired_at IS NULL AND cancelled_at IS NULL", params![])
    }

    /// Fires an active follow. Returns whether it was still active.
    pub fn fire(
        &self,
        id: &str,
        reason: &str,
        fired_at: OffsetDateTime,
        job: Option<&JobOutcome>,
    ) -> Result<bool> {
        let fired_at = format_ts(fired_at);
        let changed = self.open()?.execute(
            "UPDATE owner_follows SET fired_at = ?2, fire_reason = ?3, notify_after = ?2, \
               job_state = COALESCE(?4, job_state), job_exit_code = COALESCE(?5, job_exit_code), \
               job_started_at = COALESCE(?6, job_started_at), \
               job_finished_at = COALESCE(?7, job_finished_at) \
             WHERE id = ?1 AND fired_at IS NULL AND cancelled_at IS NULL",
            params![
                id,
                fired_at,
                reason,
                job.map(|job| job.state.as_str()),
                job.and_then(|job| job.exit_code),
                job.and_then(|job| job.started_at.as_deref()),
                job.and_then(|job| job.finished_at.as_deref()),
            ],
        )?;
        Ok(changed > 0)
    }

    /// The `sm task-complete` hook: fires every active agent follow of the
    /// session. A local write only; delivery happens on the worker.
    pub fn fire_task_complete(&self, session_id: &str, completed_at: &str) -> Result<usize> {
        let fired_at = parse_ts(completed_at).unwrap_or_else(OffsetDateTime::now_utc);
        let fired_at = format_ts(fired_at);
        Ok(self.open()?.execute(
            "UPDATE owner_follows SET fired_at = ?2, fire_reason = ?3, notify_after = ?2 \
             WHERE target_kind = 'session' AND session_id = ?1 AND fired_at IS NULL \
               AND cancelled_at IS NULL",
            params![session_id, fired_at, REASON_TASK_COMPLETE],
        )?)
    }

    fn pending_delivery(&self, now: OffsetDateTime) -> Result<Vec<Follow>> {
        Ok(self
            .query(
                "WHERE fired_at IS NOT NULL AND notified_at IS NULL AND cancelled_at IS NULL",
                params![],
            )?
            .into_iter()
            .filter(|follow| {
                follow
                    .notify_after
                    .as_deref()
                    .and_then(parse_ts)
                    .is_none_or(|due| due <= now)
            })
            .collect())
    }

    fn ack_fallback_due(&self, now: OffsetDateTime) -> Result<Vec<Follow>> {
        Ok(self
            .query(
                "WHERE notified_via = 'push' AND acked_at IS NULL AND email_sent_at IS NULL",
                params![],
            )?
            .into_iter()
            .filter(|follow| {
                follow
                    .notified_at
                    .as_deref()
                    .and_then(parse_ts)
                    .is_some_and(|notified_at| notified_at + ACK_FALLBACK <= now)
                    // A fallback email that failed transiently waits for its retry time.
                    && follow
                        .notify_after
                        .as_deref()
                        .and_then(parse_ts)
                        .is_none_or(|retry_at| retry_at <= now)
            })
            .collect())
    }

    /// Writes back the delivery columns the worker owns.
    fn save_delivery(&self, follow: &Follow) -> Result<()> {
        self.open()?.execute(
            "UPDATE owner_follows SET session_name = ?2, report_doc_id = ?3, report_title = ?4, \
               report_reader_path = ?5, notify_after = ?6, push_attempts = ?7, \
               last_push_error = ?8, notified_at = ?9, notified_via = ?10, email_sent_at = ?11 \
             WHERE id = ?1",
            params![
                follow.id,
                follow.session_name,
                follow.report_doc_id,
                follow.report_title,
                follow.report_reader_path,
                follow.notify_after,
                follow.push_attempts,
                follow.last_push_error,
                follow.notified_at,
                follow.notified_via,
                follow.email_sent_at,
            ],
        )?;
        Ok(())
    }

    fn query(&self, clause: &str, params: impl rusqlite::Params) -> Result<Vec<Follow>> {
        let conn = self.open()?;
        let mut statement = conn.prepare(&format!(
            "SELECT {FOLLOW_COLUMNS} FROM owner_follows {clause}"
        ))?;
        let rows = statement
            .query_map(params, follow_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn active_follow_conn(
    conn: &Connection,
    user_id: &str,
    target: &FollowTarget,
) -> Result<Option<Follow>> {
    let (clause, target_id) = match target {
        FollowTarget::Session { session_id, .. } => (
            "target_kind = 'session' AND session_id = ?2",
            session_id.as_str(),
        ),
        FollowTarget::QueueJob { job_id, .. } => {
            ("target_kind = 'queue_job' AND job_id = ?2", job_id.as_str())
        }
    };
    Ok(conn
        .query_row(
            &format!(
                "SELECT {FOLLOW_COLUMNS} FROM owner_follows WHERE user_id = ?1 AND {clause} \
                 AND fired_at IS NULL AND cancelled_at IS NULL"
            ),
            params![user_id, target_id],
            follow_from_row,
        )
        .optional()?)
}

fn get_follow_conn(conn: &Connection, id: &str) -> Result<Option<Follow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {FOLLOW_COLUMNS} FROM owner_follows WHERE id = ?1"),
            params![id],
            follow_from_row,
        )
        .optional()?)
}

fn allocate_follow_id(conn: &Connection) -> Result<String> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    for _ in 0..16 {
        let mut bytes = [0u8; 12];
        OsRng.fill_bytes(&mut bytes);
        let suffix: String = bytes
            .iter()
            .map(|byte| ALPHABET[usize::from(byte & 31)] as char)
            .collect();
        let id = format!("fol_{suffix}");
        if get_follow_conn(conn, &id)?.is_none() {
            return Ok(id);
        }
    }
    anyhow::bail!("could not allocate a unique follow id")
}

fn follow_from_row(row: &Row<'_>) -> rusqlite::Result<Follow> {
    Ok(Follow {
        id: row.get(0)?,
        user_id: row.get(1)?,
        target_kind: row.get(2)?,
        session_id: row.get(3)?,
        session_name: row.get(4)?,
        job_id: row.get(5)?,
        job_label: row.get(6)?,
        job_state: row.get(7)?,
        job_exit_code: row.get(8)?,
        job_started_at: row.get(9)?,
        job_finished_at: row.get(10)?,
        message_text: row.get(11)?,
        created_at: row.get(12)?,
        cancelled_at: row.get(13)?,
        fired_at: row.get(14)?,
        fire_reason: row.get(15)?,
        report_doc_id: row.get(16)?,
        report_title: row.get(17)?,
        report_reader_path: row.get(18)?,
        notify_after: row.get(19)?,
        push_attempts: row.get(20)?,
        last_push_error: row.get(21)?,
        notified_at: row.get(22)?,
        notified_via: row.get(23)?,
        acked_at: row.get(24)?,
        email_sent_at: row.get(25)?,
    })
}

/// Seconds-precision UTC RFC3339, so stored times also sort as text.
pub fn format_ts(value: OffsetDateTime) -> String {
    value
        .to_offset(time::UtcOffset::UTC)
        .replace_nanosecond(0)
        .unwrap_or(value)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

pub fn parse_ts(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value.trim(), &Rfc3339).ok()
}

// ---------------------------------------------------------------------------
// Firing and delivery.

/// What the worker needs to know about a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub id: String,
    pub name: String,
    pub stopped: bool,
    pub task_completed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobView {
    pub id: String,
    pub label: String,
    pub state: String,
    pub exit_code: Option<i64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// One publish by the followed session, newest first when listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportView {
    pub doc_id: String,
    pub title: String,
    pub reader_path: String,
    pub published_at: String,
}

/// The rest of the server, as the worker sees it.
pub trait FollowWorld {
    fn sessions(&self) -> Result<Vec<SessionView>>;
    fn job(&self, job_id: &str) -> Result<Option<JobView>>;
    /// Publishes made by the session, newest first.
    fn reports(&self, session_id: &str) -> Result<Vec<ReportView>>;
    fn job_is_terminal(&self, state: &str) -> bool {
        crate::queue::is_terminal_queue_state(state)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushError {
    /// Google no longer knows the token; stop using it.
    InvalidToken(String),
    /// Worth trying again later.
    Retryable(String),
}

impl std::fmt::Display for PushError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidToken(detail) => write!(formatter, "invalid token: {detail}"),
            Self::Retryable(detail) => write!(formatter, "{detail}"),
        }
    }
}

pub trait PushSender: Send + Sync {
    fn send(&self, token: &str, data: &BTreeMap<String, String>) -> Result<(), PushError>;
}

/// Why a follow email was not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailError {
    /// Email is not set up (no bridge, no address): retrying cannot help.
    Unavailable(String),
    /// The send failed and may work later.
    Transient(String),
}

impl std::fmt::Display for MailError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(detail) | Self::Transient(detail) => write!(formatter, "{detail}"),
        }
    }
}

pub trait FollowMailer {
    fn send(&self, follow: &Follow, notification: &Notification) -> Result<(), MailError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub kind: String,
    pub title: String,
    pub body: String,
    pub reader_path: Option<String>,
}

impl Notification {
    /// The FCM data payload (spec appendix F); every value is a string.
    pub fn data(&self, follow: Option<&Follow>) -> BTreeMap<String, String> {
        let mut data = BTreeMap::from([
            ("kind".to_owned(), self.kind.clone()),
            ("title".to_owned(), self.title.clone()),
            ("body".to_owned(), self.body.clone()),
        ]);
        if let Some(follow) = follow {
            data.insert("follow_id".to_owned(), follow.id.clone());
            data.insert("session_id".to_owned(), follow.session_id.clone());
            if let Some(job_id) = &follow.job_id {
                data.insert("job_id".to_owned(), job_id.clone());
            }
        }
        if let Some(reader_path) = &self.reader_path {
            data.insert("reader_path".to_owned(), reader_path.clone());
        }
        data
    }

    pub fn test(hostname: &str) -> Self {
        Self {
            kind: "test".to_owned(),
            title: "sm notifications work".to_owned(),
            body: format!("Sent from {hostname}"),
            reader_path: None,
        }
    }
}

/// Title and body for a fired follow (spec appendix D5).
pub fn notification_for(follow: &Follow) -> Notification {
    let name = follow.session_name.as_str();
    let report_body = follow
        .report_title
        .as_deref()
        .map(|title| format!("Report: {title}"));
    let reason = follow
        .fire_reason
        .as_deref()
        .unwrap_or(REASON_TASK_COMPLETE);
    let (title, body) = match reason {
        REASON_JOB_FINISHED => {
            let label = follow
                .job_label
                .as_deref()
                .filter(|label| !label.trim().is_empty())
                .or(follow.job_id.as_deref())
                .unwrap_or("job");
            let state = follow.job_state.as_deref().unwrap_or("unknown");
            let title = match state {
                "succeeded" => format!("{label} succeeded"),
                "failed" => match follow.job_exit_code {
                    Some(code) => format!("{label} failed (exit {code})"),
                    None => format!("{label} failed"),
                },
                "timed_out" => format!("{label} timed out"),
                "cancelled" => format!("{label} was cancelled"),
                "displaced" => format!("{label} was displaced"),
                "unknown" => format!("{label} ended"),
                other => format!("{label} ended ({})", other.replace('_', " ")),
            };
            let ran = match (
                follow.job_started_at.as_deref().and_then(parse_ts),
                follow.job_finished_at.as_deref().and_then(parse_ts),
            ) {
                (Some(started), Some(finished)) if finished >= started => {
                    Some(format_duration(finished - started))
                }
                _ => None,
            };
            let body = match ran {
                Some(ran) => format!("{name} · ran {ran}"),
                None => name.to_owned(),
            };
            (title, body)
        }
        REASON_SESSION_ENDED => (
            format!("{name} ended without completing"),
            report_body.unwrap_or_else(|| "Session stopped before sm task-complete".to_owned()),
        ),
        _ => (
            format!("{name} finished"),
            report_body.unwrap_or_else(|| "No completion report published".to_owned()),
        ),
    };
    Notification {
        kind: reason.to_owned(),
        title,
        body,
        reader_path: follow.report_reader_path.clone(),
    }
}

/// Largest two non-zero units: `2h 14m`, `41m`, `35s`, `1d 3h`.
pub fn format_duration(duration: Duration) -> String {
    let total = duration.whole_seconds().max(0);
    let units = [
        (total / 86_400, "d"),
        ((total % 86_400) / 3_600, "h"),
        ((total % 3_600) / 60, "m"),
        (total % 60, "s"),
    ];
    let Some(first) = units.iter().position(|(value, _)| *value > 0) else {
        return "0s".to_owned();
    };
    units[first..]
        .iter()
        .take(2)
        .filter(|(value, _)| *value > 0)
        .map(|(value, unit)| format!("{value}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fires follows whose target finished (spec appendix D1). The
/// `sm task-complete` hook is primary for agents; this catches session
/// ends from every stop path and jobs reaching a terminal state.
pub fn sweep(
    store: &OwnerPushStore,
    world: &dyn FollowWorld,
    now: OffsetDateTime,
) -> Result<usize> {
    let active = store.active()?;
    if active.is_empty() {
        return Ok(0);
    }
    let mut sessions: Option<BTreeMap<String, SessionView>> = None;
    let mut fired = 0;
    for follow in active {
        if follow.is_job() {
            let Some(job_id) = follow.job_id.as_deref() else {
                continue;
            };
            let outcome = match world.job(job_id)? {
                Some(job) if world.job_is_terminal(&job.state) => JobOutcome {
                    state: job.state,
                    exit_code: job.exit_code,
                    started_at: job.started_at,
                    finished_at: job.finished_at,
                },
                Some(_) => continue,
                None => JobOutcome {
                    state: "unknown".to_owned(),
                    exit_code: None,
                    started_at: None,
                    finished_at: None,
                },
            };
            fired +=
                usize::from(store.fire(&follow.id, REASON_JOB_FINISHED, now, Some(&outcome))?);
            continue;
        }
        if sessions.is_none() {
            sessions = Some(
                world
                    .sessions()?
                    .into_iter()
                    .map(|session| (session.id.clone(), session))
                    .collect(),
            );
        }
        let session = sessions
            .as_ref()
            .and_then(|sessions| sessions.get(&follow.session_id));
        let created_at = parse_ts(&follow.created_at);
        let completed_after_follow = session
            .and_then(|session| session.task_completed_at.as_deref())
            .and_then(parse_ts)
            .filter(|completed_at| created_at.is_some_and(|created_at| *completed_at > created_at));
        let fired_now = if let Some(completed_at) = completed_after_follow {
            store.fire(&follow.id, REASON_TASK_COMPLETE, completed_at, None)?
        } else if session.is_none_or(|session| session.stopped) {
            store.fire(&follow.id, REASON_SESSION_ENDED, now, None)?
        } else {
            false
        };
        fired += usize::from(fired_now);
    }
    Ok(fired)
}

/// Sends fired follows (spec appendix D3) and the unacknowledged-push email
/// fallback. Returns one line per problem worth logging.
pub fn deliver(
    store: &OwnerPushStore,
    world: &dyn FollowWorld,
    sender: Option<&dyn PushSender>,
    mailer: &dyn FollowMailer,
    now: OffsetDateTime,
) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let pending = store.pending_delivery(now)?;
    let names = if pending.is_empty() {
        BTreeMap::new()
    } else {
        world
            .sessions()?
            .into_iter()
            .map(|session| (session.id, session.name))
            .collect::<BTreeMap<_, _>>()
    };
    for mut follow in pending {
        if let Some(name) = names.get(&follow.session_id) {
            follow.session_name = name.clone();
        }
        if !follow.is_job() {
            let report = find_report(world, &follow)?;
            match report {
                Some(report) => {
                    follow.report_doc_id = Some(report.doc_id);
                    follow.report_title = Some(report.title);
                    follow.report_reader_path = Some(report.reader_path);
                }
                None => {
                    let grace_ends = follow
                        .fired_at
                        .as_deref()
                        .and_then(parse_ts)
                        .map(|fired_at| fired_at + REPORT_GRACE);
                    if let Some(grace_ends) = grace_ends.filter(|grace_ends| now < *grace_ends) {
                        follow.notify_after = Some(format_ts(grace_ends));
                        store.save_delivery(&follow)?;
                        continue;
                    }
                }
            }
        }
        let notification = notification_for(&follow);
        let tokens = match sender {
            Some(_) => store.valid_tokens(&follow.user_id)?,
            None => Vec::new(),
        };
        let mut use_email = sender.is_none() || tokens.is_empty();
        if let (Some(sender), false) = (sender, use_email) {
            let data = notification.data(Some(&follow));
            let mut delivered = false;
            let mut retryable = Vec::new();
            for token in &tokens {
                match sender.send(&token.token, &data) {
                    Ok(()) => delivered = true,
                    Err(PushError::InvalidToken(detail)) => {
                        store.record_token_error(token, &detail, Some(now))?;
                    }
                    Err(PushError::Retryable(detail)) => {
                        store.record_token_error(token, &detail, None)?;
                        retryable.push(format!("{}: {detail}", token.device_name));
                    }
                }
            }
            if delivered {
                follow.notified_at = Some(format_ts(now));
                follow.notified_via = Some("push".to_owned());
                follow.last_push_error = retryable.first().cloned();
            } else if retryable.is_empty() {
                use_email = true;
            } else {
                follow.push_attempts += 1;
                follow.last_push_error = Some(retryable.join("; "));
                match usize::try_from(follow.push_attempts - 1)
                    .ok()
                    .and_then(|index| PUSH_RETRY_DELAYS_SECONDS.get(index))
                {
                    Some(delay) => {
                        follow.notify_after = Some(format_ts(now + Duration::seconds(*delay)));
                    }
                    None => use_email = true,
                }
            }
        }
        if use_email {
            let due_since = follow.fired_at.as_deref().and_then(parse_ts);
            match mailer.send(&follow, &notification) {
                Ok(()) => {}
                Err(MailError::Transient(detail)) if within_email_retry(due_since, now) => {
                    problems.push(format!(
                        "follow {} email failed, retrying: {detail}",
                        follow.id
                    ));
                    follow.last_push_error = Some(format!("email failed: {detail}"));
                    follow.notify_after = Some(format_ts(now + EMAIL_RETRY_DELAY));
                    store.save_delivery(&follow)?;
                    continue;
                }
                Err(error) => {
                    problems.push(format!("follow {} email failed: {error}", follow.id));
                    follow.last_push_error = Some(match (&error, sender) {
                        (MailError::Unavailable(_), None) => "no channel".to_owned(),
                        _ => format!("email failed: {error}"),
                    });
                }
            }
            follow.notified_at = Some(format_ts(now));
            follow.notified_via = Some("email".to_owned());
            follow.email_sent_at = Some(format_ts(now));
        }
        store.save_delivery(&follow)?;
    }
    for mut follow in store.ack_fallback_due(now)? {
        let notification = notification_for(&follow);
        let due_since = follow
            .notified_at
            .as_deref()
            .and_then(parse_ts)
            .map(|notified_at| notified_at + ACK_FALLBACK);
        match mailer.send(&follow, &notification) {
            Ok(()) => {}
            Err(MailError::Transient(detail)) if within_email_retry(due_since, now) => {
                problems.push(format!(
                    "follow {} fallback email failed, retrying: {detail}",
                    follow.id
                ));
                follow.last_push_error = Some(format!("email failed: {detail}"));
                // Once notified, notify_after schedules the fallback retry.
                follow.notify_after = Some(format_ts(now + EMAIL_RETRY_DELAY));
                store.save_delivery(&follow)?;
                continue;
            }
            Err(error) => {
                problems.push(format!(
                    "follow {} fallback email failed: {error}",
                    follow.id
                ));
                follow.last_push_error = Some(format!("email failed: {error}"));
            }
        }
        follow.email_sent_at = Some(format_ts(now));
        store.save_delivery(&follow)?;
    }
    Ok(problems)
}

fn within_email_retry(due_since: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
    due_since.is_some_and(|due_since| now < due_since + EMAIL_RETRY_WINDOW)
}

fn find_report(world: &dyn FollowWorld, follow: &Follow) -> Result<Option<ReportView>> {
    let Some(created_at) = parse_ts(&follow.created_at) else {
        return Ok(None);
    };
    Ok(world
        .reports(&follow.session_id)?
        .into_iter()
        .find(|report| {
            parse_ts(&report.published_at).is_some_and(|published_at| {
                // Stored follow times drop sub-seconds; compare at that precision.
                published_at.replace_nanosecond(0).unwrap_or(published_at) >= created_at
            })
        }))
}

/// Sends a test push to every valid token of `user_id`.
pub fn send_test(
    store: &OwnerPushStore,
    sender: &dyn PushSender,
    user_id: &str,
    hostname: &str,
    now: OffsetDateTime,
) -> Result<(usize, Vec<(String, String)>)> {
    let data = Notification::test(hostname).data(None);
    let mut sent = 0;
    let mut failed = Vec::new();
    for token in store.valid_tokens(user_id)? {
        match sender.send(&token.token, &data) {
            Ok(()) => sent += 1,
            Err(error) => {
                let invalid = matches!(error, PushError::InvalidToken(_)).then_some(now);
                store.record_token_error(&token, &error.to_string(), invalid)?;
                failed.push((token.device_name.clone(), error.to_string()));
            }
        }
    }
    Ok((sent, failed))
}

/// Prefixes the follow marker unless the owner's text already carries it.
pub fn follow_message_text(message: &str) -> String {
    let message = message.trim();
    if message.starts_with(FOLLOW_MESSAGE_MARKER) {
        message.to_owned()
    } else {
        format!("{FOLLOW_MESSAGE_MARKER} {message}")
    }
}

pub fn push_db_path(config: &crate::config::AppConfig) -> PathBuf {
    crate::sessions::expand_home(&config.push.db_path)
}

#[cfg(test)]
mod tests;
