//! Each agent's last turn message, and the Finished rows `sm task-complete`
//! leaves for the owner (sm#1789). The last turn message is the text an
//! agent wrote at the end of its latest turn: Claude's Stop hook summary, or
//! codex-fork's final agent message. A Finished row is filled with the first
//! turn message written at or after the completion, else, after
//! [`FALLBACK_AFTER`], with the latest one before it. Rows live in the
//! retained queue DB (`sm_send.db_path`) beside owner messages and Inbox
//! marks. Spec: `docs/working/1782_fit_and_finish.html`, appendix D.

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
        "#,
    )?;
    Ok(())
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

    /// Records a session's latest turn message, and fills every Finished
    /// row of that session still without text whose completion is at or
    /// before it. Blank text records nothing.
    pub fn record_turn(
        &self,
        session_id: &str,
        provider: &str,
        at: OffsetDateTime,
        text: &str,
    ) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let text = cap_text(text);
        let at = stamp(at);
        let mut conn = self.open_write()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO turn_messages (session_id, provider, at, text) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(session_id) DO UPDATE SET provider = excluded.provider, \
             at = excluded.at, text = excluded.text",
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
