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
    /// Follow results the thread held when last read; more than this is
    /// new. A count, for the same reason as `done_items`.
    pub read_follows: Option<i64>,
    pub archived_items: Option<i64>,
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
    pub thread_key: Option<String>,
}

pub fn init_owner_inbox_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS owner_inbox_threads (
            thread_key TEXT PRIMARY KEY,
            done_at TEXT,
            done_items INTEGER,
            last_read_at TEXT,
            read_follows INTEGER
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
    if conn
        .prepare("SELECT archived_items FROM owner_inbox_threads LIMIT 0")
        .is_err()
    {
        conn.execute(
            "ALTER TABLE owner_inbox_threads ADD COLUMN archived_items INTEGER",
            [],
        )?;
    }
    if conn
        .prepare("SELECT thread_key FROM owner_message_notes LIMIT 0")
        .is_err()
    {
        conn.execute(
            "ALTER TABLE owner_message_notes ADD COLUMN thread_key TEXT",
            [],
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct OwnerInboxStore {
    db_path: PathBuf,
}

const NOTE_COLUMNS: &str =
    "id, session_id, body, delivered_text, delivered_to_session_id, created_at, thread_key";

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
        let conn = self.open_write()?;
        let mut statement = conn.prepare(
            "SELECT thread_key, done_at, done_items, last_read_at, read_follows, archived_items \
                 FROM owner_inbox_threads",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    ThreadMarks {
                        done_at: row.get(1)?,
                        done_items: row.get(2)?,
                        last_read_at: row.get(3)?,
                        read_follows: row.get(4)?,
                        archived_items: row.get(5)?,
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

    pub fn mark_archive(&self, thread_key: &str, items: usize) -> Result<()> {
        let conn = self.open_write()?;
        conn.execute(
            "INSERT INTO owner_inbox_threads (thread_key, done_at, done_items, archived_items)
             VALUES (?1, ?2, ?3, ?3) ON CONFLICT(thread_key) DO UPDATE SET
             done_at = excluded.done_at, done_items = excluded.done_items,
             archived_items = excluded.archived_items",
            params![
                thread_key,
                now_rfc3339(),
                i64::try_from(items).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(())
    }

    pub fn unarchive(&self, thread_key: &str) -> Result<()> {
        self.open_write()?.execute(
            "UPDATE owner_inbox_threads SET archived_items = NULL, done_at = NULL,
             done_items = NULL WHERE thread_key = ?1",
            params![thread_key],
        )?;
        Ok(())
    }

    /// Move an old Done mark without overwriting an action on the new key.
    pub fn migrate_done(&self, thread_key: &str, items: usize) -> Result<()> {
        self.open_write()?.execute(
            "INSERT INTO owner_inbox_threads (thread_key, done_at, done_items)
             VALUES (?1, ?2, ?3) ON CONFLICT(thread_key) DO UPDATE SET
             done_at = COALESCE(owner_inbox_threads.done_at, excluded.done_at),
             done_items = COALESCE(owner_inbox_threads.done_items, excluded.done_items)",
            params![
                thread_key,
                now_rfc3339(),
                i64::try_from(items).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(())
    }

    /// Read, having seen `follows` follow results.
    pub fn mark_read(&self, thread_key: &str, follows: usize) -> Result<()> {
        self.open_write()?.execute(
            "INSERT INTO owner_inbox_threads (thread_key, last_read_at, read_follows) \
             VALUES (?1, ?2, ?3) ON CONFLICT(thread_key) DO UPDATE \
             SET last_read_at = excluded.last_read_at, read_follows = excluded.read_follows",
            params![
                thread_key,
                now_rfc3339(),
                i64::try_from(follows).unwrap_or(i64::MAX)
            ],
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
        self.record_note_inner(note, true)
    }

    /// The first Ask question is already part of a newly spawned reader's
    /// initial prompt, so record it in the thread without a second delivery.
    pub fn record_note_in_initial_prompt(&self, note: &OwnerNote) -> Result<(OwnerNote, bool)> {
        self.record_note_inner(note, false)
    }

    fn record_note_inner(&self, note: &OwnerNote, enqueue: bool) -> Result<(OwnerNote, bool)> {
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = get_note_conn(&tx, &note.id)? {
            return Ok((existing, false));
        }
        tx.execute(
            &format!(
                "INSERT INTO owner_message_notes ({NOTE_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ),
            params![
                note.id,
                note.session_id,
                note.body,
                note.delivered_text,
                note.delivered_to_session_id,
                now_rfc3339(),
                note.thread_key,
            ],
        )?;
        if enqueue {
            crate::queue::enqueue_message_once_in_conn(
                &tx,
                &format!("owner-note-{}", note.id),
                &note.delivered_to_session_id,
                &note.delivered_text,
            )?;
        }
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
        thread_key: row.get(6)?,
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
        store.mark_read("agent:a", 2).unwrap();
        store.mark_done("agent:a", 3).unwrap();
        let marks = store.marks().unwrap();
        assert!(marks["agent:a"].done_at.is_some());
        assert_eq!(marks["agent:a"].done_items, Some(3));
        assert!(marks["agent:a"].last_read_at.is_some());
        assert_eq!(marks["agent:a"].read_follows, Some(2));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migration_and_archive_keep_item_watermarks() {
        let (store, dir) = store();
        store.mark_done("agent:old", 2).unwrap();
        store.mark_read("ticket:owner/repo#7", 1).unwrap();
        store.migrate_done("ticket:owner/repo#7", 2).unwrap();
        store.migrate_done("ticket:owner/repo#7", 4).unwrap();
        assert_eq!(
            store.marks().unwrap()["ticket:owner/repo#7"].done_items,
            Some(2)
        );
        assert_eq!(
            store.marks().unwrap()["ticket:owner/repo#7"].read_follows,
            Some(1)
        );
        store.mark_archive("ticket:owner/repo#7", 3).unwrap();
        let marks = store.marks().unwrap();
        assert_eq!(marks["ticket:owner/repo#7"].done_items, Some(3));
        assert_eq!(marks["ticket:owner/repo#7"].archived_items, Some(3));
        store.unarchive("ticket:owner/repo#7").unwrap();
        assert_eq!(
            store.marks().unwrap()["ticket:owner/repo#7"].archived_items,
            None
        );
        assert_eq!(
            store.marks().unwrap()["ticket:owner/repo#7"].done_items,
            None
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_schema_gains_archive_and_note_thread_columns() {
        let (store, dir) = store();
        let conn = Connection::open(&store.db_path).unwrap();
        conn.execute_batch("CREATE TABLE owner_inbox_threads (thread_key TEXT PRIMARY KEY, done_at TEXT, done_items INTEGER, last_read_at TEXT, read_follows INTEGER); CREATE TABLE owner_message_notes (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, body TEXT NOT NULL, delivered_text TEXT NOT NULL, delivered_to_session_id TEXT NOT NULL, created_at TEXT NOT NULL);").unwrap();
        conn.execute(
            "INSERT INTO owner_inbox_threads (thread_key, done_items) VALUES ('agent:old', 2)",
            [],
        )
        .unwrap();
        drop(conn);
        assert_eq!(store.marks().unwrap()["agent:old"].done_items, Some(2));
        assert!(store.notes().unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
}
