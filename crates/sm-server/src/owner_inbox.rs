//! The owner Inbox (sm#1647): what the owner marked Done, when each thread
//! was last read, and notes the owner wrote to an agent before it wrote
//! first. Rows live in the retained queue DB (`sm_send.db_path`) beside
//! owner messages, so a note is recorded and queued in one transaction.
//! Spec: `specs/1647_owner_inbox.html`.

use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior};
use serde::Serialize;
use time::OffsetDateTime;

/// A thread's key: `agent:<session id>` or `doc:<owner doc id>`.
pub fn agent_thread_key(session_id: &str) -> String {
    format!("agent:{session_id}")
}

pub fn doc_thread_key(doc_id: &str) -> String {
    format!("doc:{doc_id}")
}

/// What the owner did to one thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadMarks {
    pub done_at: Option<String>,
    /// How many items the thread held when marked Done. It stays Done
    /// while it holds no more; times alone cannot tell an item that
    /// arrived in the same second as Done.
    pub done_items: Option<i64>,
    pub last_read_at: Option<String>,
}

/// A message the owner started, with no agent message above it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerNote {
    /// The client's `submission_id`.
    pub id: String,
    pub session_id: String,
    pub body: String,
    pub delivered_text: String,
    pub delivered_to_session_id: String,
    pub created_at: String,
}

pub fn init_owner_inbox_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS owner_inbox_threads (
            thread_key TEXT PRIMARY KEY,
            done_at TEXT,
            done_items INTEGER,
            last_read_at TEXT
        );
        CREATE TABLE IF NOT EXISTS owner_message_notes (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            body TEXT NOT NULL,
            delivered_text TEXT NOT NULL,
            delivered_to_session_id TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS owner_message_notes_session
            ON owner_message_notes(session_id, created_at);
        "#,
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct OwnerInboxStore {
    db_path: PathBuf,
}

const NOTE_COLUMNS: &str =
    "id, session_id, body, delivered_text, delivered_to_session_id, created_at";

impl OwnerInboxStore {
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
        init_owner_inbox_schema(&conn)?;
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
                "SELECT 1 FROM sqlite_master WHERE type = 'table' \
                 AND name = 'owner_message_notes'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        Ok(has_tables.then_some(conn))
    }

    /// Every thread the owner has marked Done or read.
    pub fn marks(&self) -> Result<BTreeMap<String, ThreadMarks>> {
        let Some(conn) = self.open_read()? else {
            return Ok(BTreeMap::new());
        };
        let mut statement = conn.prepare(
            "SELECT thread_key, done_at, done_items, last_read_at FROM owner_inbox_threads",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    ThreadMarks {
                        done_at: row.get(1)?,
                        done_items: row.get(2)?,
                        last_read_at: row.get(3)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        Ok(rows)
    }

    /// Done with the `items` the thread holds now.
    pub fn mark_done(&self, thread_key: &str, items: usize) -> Result<()> {
        self.open_write()?.execute(
            "INSERT INTO owner_inbox_threads (thread_key, done_at, done_items) \
             VALUES (?1, ?2, ?3) ON CONFLICT(thread_key) DO UPDATE \
             SET done_at = excluded.done_at, done_items = excluded.done_items",
            params![
                thread_key,
                now_rfc3339(),
                i64::try_from(items).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(())
    }

    pub fn mark_read(&self, thread_key: &str) -> Result<()> {
        self.open_write()?.execute(
            "INSERT INTO owner_inbox_threads (thread_key, last_read_at) VALUES (?1, ?2) \
             ON CONFLICT(thread_key) DO UPDATE SET last_read_at = excluded.last_read_at",
            params![thread_key, now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn note(&self, submission_id: &str) -> Result<Option<OwnerNote>> {
        let Some(conn) = self.open_read()? else {
            return Ok(None);
        };
        get_note_conn(&conn, submission_id)
    }

    /// Every note, oldest first.
    pub fn notes(&self) -> Result<Vec<OwnerNote>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(&format!(
            "SELECT {NOTE_COLUMNS} FROM owner_message_notes ORDER BY created_at, rowid"
        ))?;
        let rows = statement
            .query_map([], note_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Records the note and queues it for `delivered_to_session_id` in one
    /// transaction. A second call with the same id returns the stored note
    /// and `false`, queueing nothing.
    pub fn record_note(&self, note: &OwnerNote) -> Result<(OwnerNote, bool)> {
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = get_note_conn(&tx, &note.id)? {
            return Ok((existing, false));
        }
        tx.execute(
            &format!(
                "INSERT INTO owner_message_notes ({NOTE_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            ),
            params![
                note.id,
                note.session_id,
                note.body,
                note.delivered_text,
                note.delivered_to_session_id,
                now_rfc3339()
            ],
        )?;
        crate::queue::enqueue_message_once_in_conn(
            &tx,
            &format!("owner-note-{}", note.id),
            &note.delivered_to_session_id,
            &note.delivered_text,
        )?;
        let stored = get_note_conn(&tx, &note.id)?.context("note row vanished")?;
        tx.commit()?;
        Ok((stored, true))
    }
}

fn note_from_row(row: &Row<'_>) -> rusqlite::Result<OwnerNote> {
    Ok(OwnerNote {
        id: row.get(0)?,
        session_id: row.get(1)?,
        body: row.get(2)?,
        delivered_text: row.get(3)?,
        delivered_to_session_id: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn get_note_conn(conn: &Connection, id: &str) -> Result<Option<OwnerNote>> {
    Ok(conn
        .query_row(
            &format!("SELECT {NOTE_COLUMNS} FROM owner_message_notes WHERE id = ?1"),
            params![id],
            note_from_row,
        )
        .optional()?)
}

fn now_rfc3339() -> String {
    crate::owner_push::format_ts(OffsetDateTime::now_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (OwnerInboxStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "sm-owner-inbox-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        (OwnerInboxStore::new(dir.join("q.db")), dir)
    }

    #[test]
    fn marks_start_empty_and_keep_both_columns() {
        let (store, dir) = store();
        assert!(store.marks().unwrap().is_empty());
        store.mark_read("agent:a").unwrap();
        store.mark_done("agent:a", 3).unwrap();
        let marks = store.marks().unwrap();
        assert!(marks["agent:a"].done_at.is_some());
        assert_eq!(marks["agent:a"].done_items, Some(3));
        assert!(marks["agent:a"].last_read_at.is_some());
        fs::remove_dir_all(dir).unwrap();
    }
}
