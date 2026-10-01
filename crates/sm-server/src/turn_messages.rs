//! Each agent's last turn message, and the Finished rows `sm task-complete`
//! leaves for the owner (sm#1789). The last turn message is the text an
//! agent wrote at the end of its latest turn: Claude's Stop hook summary, or
//! codex-fork's final agent message. A Finished row is filled with the first
//! turn message written at or after the completion, else, after
//! [`FALLBACK_AFTER`], with the latest one before it. Rows live in the
//! retained queue DB (`sm_send.db_path`) beside owner messages and Inbox
//! marks. Spec: `docs/working/1782_fit_and_finish.html`, appendix D.
//!
//! An agent's reply to the owner (sm#1844) is the turn message of the first
//! turn that started after the owner's latest reply or note to it from the
//! Inbox, the agent band or a message page. It is kept in `thread_replies`,
//! one row per owner input, so the Inbox thread shows the conversation.

use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime};

/// Longest stored text, in characters, before it is cut and given "…".
pub const MAX_TEXT_CHARS: usize = 20_000;
/// How long a Finished row waits for a later turn message before it takes
/// the latest earlier one.
pub const FALLBACK_AFTER: time::Duration = time::Duration::minutes(10);
/// Finished rows are deleted this long after their completion.
pub const FINISHED_RETENTION: time::Duration = time::Duration::days(90);

/// Fixed-width UTC times, so stored times compare as strings.
const STAMP: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z");

pub fn stamp(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(STAMP)
        .unwrap_or_else(|_| "1970-01-01T00:00:00.000000Z".to_owned())
}

/// An RFC 3339 time in the stored form; `None` when it does not parse.
pub fn stamp_rfc3339(value: &str) -> Option<String> {
    OffsetDateTime::parse(value.trim(), &time::format_description::well_known::Rfc3339)
        .ok()
        .map(stamp)
}

/// `text` cut to [`MAX_TEXT_CHARS`] at a character boundary, with "…".
pub fn cap_text(text: &str) -> String {
    match text.char_indices().nth(MAX_TEXT_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnMessage {
    pub at: String,
    pub text: String,
}

/// An agent's answer to one owner input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadReply {
    pub session_id: String,
    /// When the owner's reply or note was sent.
    pub input_at: String,
    pub text: String,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedRow {
    pub session_id: String,
    pub completed_at: String,
    pub text: Option<String>,
    pub text_at: Option<String>,
    pub read_at: Option<String>,
}

pub fn init_turn_messages_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS turn_messages (
            session_id TEXT PRIMARY KEY,
            provider TEXT NOT NULL,
            at TEXT NOT NULL,
            text TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS finished (
            session_id TEXT NOT NULL,
            completed_at TEXT NOT NULL,
            text TEXT,
            text_at TEXT,
            read_at TEXT,
            PRIMARY KEY (session_id, completed_at)
        );
        CREATE TABLE IF NOT EXISTS thread_replies (
            session_id TEXT NOT NULL,
            input_at TEXT NOT NULL,
            text TEXT NOT NULL,
            at TEXT NOT NULL,
            turn_started TEXT NOT NULL,
            PRIMARY KEY (session_id, input_at)
        );
        CREATE TABLE IF NOT EXISTS turn_message_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        "#,
    )?;
    // Owner inputs from before replies were recorded have no answer to find.
    conn.execute(
        "INSERT OR IGNORE INTO turn_message_meta (key, value) VALUES ('replies_since', ?1)",
        params![stamp(OffsetDateTime::now_utc())],
    )?;
    Ok(())
}

/// The newest owner reply or note delivered to `session_id` at or before
/// `cutoff` and after `since`, from the owner message tables when present.
fn latest_owner_input(
    conn: &Connection,
    session_id: &str,
    since: OffsetDateTime,
    cutoff: OffsetDateTime,
) -> Result<Option<OffsetDateTime>> {
    let mut latest = None;
    for table in ["owner_message_replies", "owner_message_notes"] {
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !exists {
            continue;
        }
        let mut statement = conn.prepare(&format!(
            "SELECT created_at FROM {table} WHERE delivered_to_session_id = ?1 \
             ORDER BY created_at DESC LIMIT 20"
        ))?;
        let times = statement
            .query_map(params![session_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        latest = times
            .iter()
            .filter_map(|value| parse_time(value))
            // Sends are stored to the second, so compare at that precision.
            .filter(|at| *at >= since.replace_nanosecond(0).unwrap_or(since) && *at <= cutoff)
            .chain(latest)
            .max();
    }
    Ok(latest)
}

fn parse_time(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value.trim(), &time::format_description::well_known::Rfc3339).ok()
}

#[derive(Debug, Clone)]
pub struct TurnMessageStore {
    db_path: PathBuf,
}

impl TurnMessageStore {
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
        init_turn_messages_schema(&conn)?;
        Ok(conn)
    }

    /// Creates the tables, and so fixes the time from which owner sends get
    /// their answer recorded. The server calls it at startup.
    pub fn ensure_schema(&self) -> Result<()> {
        self.open_write().map(drop)
    }

    /// Read connections never create the DB; a missing one reads as empty.
    fn open_read(&self) -> Result<Option<Connection>> {
        if !self.db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let has_tables: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'finished'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        Ok(has_tables.then_some(conn))
    }

    /// Records a session's latest turn message, unless one written later is
    /// already stored (hooks can arrive out of order), and fills every
    /// Finished row of that session still without text whose completion is
    /// at or before it. Blank text records nothing.
    ///
    /// `turn_started` is when the turn began, when the provider reports it;
    /// the latest owner input sent at or before it is answered by this turn
    /// unless an earlier turn already answered it. Without it, `at` is used.
    pub fn record_turn(
        &self,
        session_id: &str,
        provider: &str,
        at: OffsetDateTime,
        turn_started: Option<OffsetDateTime>,
        text: &str,
    ) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let text = cap_text(text);
        let turn_at = at;
        let at = stamp(at);
        let mut conn = self.open_write()?;
        let tx = conn.transaction()?;
        let since = tx
            .query_row(
                "SELECT value FROM turn_message_meta WHERE key = 'replies_since'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|value| parse_time(&value))
            .unwrap_or(turn_at);
        let started = turn_started.unwrap_or(turn_at);
        if let Some(input) = latest_owner_input(&tx, session_id, since, started)? {
            // Hooks can arrive out of order: the turn that started first
            // after the send is its answer, whichever Stop lands first.
            tx.execute(
                "INSERT INTO thread_replies (session_id, input_at, text, at, turn_started) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(session_id, input_at) DO UPDATE SET text = excluded.text, \
                 at = excluded.at, turn_started = excluded.turn_started \
                 WHERE excluded.turn_started < thread_replies.turn_started",
                params![session_id, stamp(input), text, at, stamp(started)],
            )?;
        }
        tx.execute(
            "INSERT INTO turn_messages (session_id, provider, at, text) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(session_id) DO UPDATE SET provider = excluded.provider, \
             at = excluded.at, text = excluded.text WHERE excluded.at >= turn_messages.at",
            params![session_id, provider, at, text],
        )?;
        tx.execute(
            "UPDATE finished SET text = ?2, text_at = ?3 \
             WHERE session_id = ?1 AND text IS NULL AND completed_at <= ?3",
            params![session_id, text, at],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A Finished row for one `sm task-complete`, still without text.
    pub fn record_finished(&self, session_id: &str, completed_at: OffsetDateTime) -> Result<()> {
        self.open_write()?.execute(
            "INSERT OR IGNORE INTO finished (session_id, completed_at) VALUES (?1, ?2)",
            params![session_id, stamp(completed_at)],
        )?;
        Ok(())
    }

    /// Fills Finished rows that waited [`FALLBACK_AFTER`] with no later turn
    /// message from the latest one before their completion, and deletes rows
    /// past [`FINISHED_RETENTION`]. Returns the rows it changed.
    pub fn sweep(&self, now: OffsetDateTime) -> Result<usize> {
        if self.open_read()?.is_none() {
            return Ok(0);
        }
        let conn = self.open_write()?;
        let filled = conn.execute(
            "UPDATE finished SET \
                 text = (SELECT t.text FROM turn_messages t WHERE t.session_id = finished.session_id), \
                 text_at = (SELECT t.at FROM turn_messages t WHERE t.session_id = finished.session_id) \
             WHERE text IS NULL AND completed_at <= ?1 AND EXISTS \
                 (SELECT 1 FROM turn_messages t WHERE t.session_id = finished.session_id \
                  AND t.at < finished.completed_at)",
            params![stamp(now - FALLBACK_AFTER)],
        )?;
        let deleted = conn.execute(
            "DELETE FROM finished WHERE completed_at < ?1",
            params![stamp(now - FINISHED_RETENTION)],
        )?;
        Ok(filled + deleted)
    }

    /// Sets `read_at` on every unread Finished row of a session.
    pub fn mark_read(&self, session_id: &str, now: OffsetDateTime) -> Result<usize> {
        if self.open_read()?.is_none() {
            return Ok(0);
        }
        Ok(self.open_write()?.execute(
            "UPDATE finished SET read_at = ?2 WHERE session_id = ?1 AND read_at IS NULL",
            params![session_id, stamp(now)],
        )?)
    }

    pub fn last_turn(&self, session_id: &str) -> Result<Option<TurnMessage>> {
        let Some(conn) = self.open_read()? else {
            return Ok(None);
        };
        Ok(conn
            .query_row(
                "SELECT at, text FROM turn_messages WHERE session_id = ?1",
                params![session_id],
                |row| {
                    Ok(TurnMessage {
                        at: row.get(0)?,
                        text: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// Every session's last turn message.
    pub fn last_turns(&self) -> Result<BTreeMap<String, TurnMessage>> {
        let Some(conn) = self.open_read()? else {
            return Ok(BTreeMap::new());
        };
        let mut statement = conn.prepare("SELECT session_id, at, text FROM turn_messages")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    TurnMessage {
                        at: row.get(1)?,
                        text: row.get(2)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Every Finished row, oldest completion first.
    pub fn finished(&self) -> Result<Vec<FinishedRow>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(
            "SELECT session_id, completed_at, text, text_at, read_at FROM finished \
             ORDER BY completed_at, session_id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(FinishedRow {
                    session_id: row.get(0)?,
                    completed_at: row.get(1)?,
                    text: row.get(2)?,
                    text_at: row.get(3)?,
                    read_at: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Every agent reply to an owner input, oldest first.
    pub fn thread_replies(&self) -> Result<Vec<ThreadReply>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let has_table: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'thread_replies'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !has_table {
            return Ok(Vec::new());
        }
        let mut statement = conn.prepare(
            "SELECT session_id, input_at, text, at FROM thread_replies ORDER BY at, session_id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(ThreadReply {
                    session_id: row.get(0)?,
                    input_at: row.get(1)?,
                    text: row.get(2)?,
                    at: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Each session's newest Finished row.
    pub fn newest_finished(&self) -> Result<BTreeMap<String, FinishedRow>> {
        Ok(self
            .finished()?
            .into_iter()
            .map(|row| (row.session_id.clone(), row))
            .collect())
    }
}

#[cfg(test)]
mod tests;
