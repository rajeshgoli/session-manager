use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Debug, Clone)]
pub struct RetainedQueueStore {
    db_path: PathBuf,
}

impl RetainedQueueStore {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn ensure_schema(&self) -> Result<()> {
        self.with_connection(|_| Ok(()))
    }

    pub fn enqueue_message(
        &self,
        target_session_id: &str,
        text: &str,
        delivery_mode: &str,
        message_category: Option<&str>,
    ) -> Result<String> {
        self.with_connection(|conn| {
            let id = generate_record_id("msg");
            conn.execute(
                r#"
                INSERT INTO message_queue
                    (id, target_session_id, text, delivery_mode, from_sm_send, queued_at, message_category)
                VALUES
                    (?1, ?2, ?3, ?4, 0, ?5, ?6)
                "#,
                params![
                    id,
                    target_session_id,
                    text,
                    delivery_mode,
                    now_rfc3339(),
                    message_category
                ],
            )?;
            Ok(id)
        })
    }

    pub fn active_parent_wake_parent(&self, child_session_id: &str) -> Result<Option<String>> {
        self.with_connection(|conn| {
            conn.query_row(
                r#"
                SELECT parent_session_id
                FROM parent_wake_registrations
                WHERE child_session_id = ?1 AND is_active = 1
                LIMIT 1
                "#,
                params![child_session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
        })
    }

    pub fn register_parent_wake(
        &self,
        child_session_id: &str,
        parent_session_id: &str,
        period_seconds: i64,
    ) -> Result<String> {
        self.with_connection(|conn| {
            self.cancel_parent_wake_with_connection(conn, child_session_id)?;
            let id = generate_record_id("wake");
            conn.execute(
                r#"
                INSERT OR REPLACE INTO parent_wake_registrations
                    (id, child_session_id, parent_session_id, period_seconds, registered_at,
                     last_wake_at, last_status_at_prev_wake, escalated, is_active)
                VALUES
                    (?1, ?2, ?3, ?4, ?5, NULL, NULL, 0, 1)
                "#,
                params![
                    id,
                    child_session_id,
                    parent_session_id,
                    period_seconds,
                    now_rfc3339()
                ],
            )?;
            Ok(id)
        })
    }

    pub fn cancel_parent_wake(&self, child_session_id: &str) -> Result<()> {
        self.with_connection(|conn| self.cancel_parent_wake_with_connection(conn, child_session_id))
    }

    pub fn cancel_remind(&self, target_session_id: &str) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute(
                r#"
                UPDATE remind_registrations
                SET is_active = 0
                WHERE target_session_id = ?1
                "#,
                params![target_session_id],
            )?;
            Ok(())
        })
    }

    pub fn upsert_stop_notify(
        &self,
        session_id: &str,
        sender_session_id: &str,
        sender_name: &str,
        delay_seconds: i64,
    ) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute(
                r#"
                INSERT INTO rust_stop_notify_states
                    (session_id, sender_session_id, sender_name, delay_seconds, armed_at)
                VALUES
                    (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(session_id) DO UPDATE SET
                    sender_session_id = excluded.sender_session_id,
                    sender_name = excluded.sender_name,
                    delay_seconds = excluded.delay_seconds,
                    armed_at = excluded.armed_at
                "#,
                params![
                    session_id,
                    sender_session_id,
                    sender_name,
                    delay_seconds,
                    now_rfc3339()
                ],
            )?;
            Ok(())
        })
    }

    fn with_connection<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create queue db directory {}", parent.display())
            })?;
        }
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open queue db {}", self.db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_schema(&conn)?;
        f(&conn)
    }

    fn cancel_parent_wake_with_connection(
        &self,
        conn: &Connection,
        child_session_id: &str,
    ) -> Result<()> {
        conn.execute(
            r#"
            UPDATE parent_wake_registrations
            SET is_active = 0
            WHERE child_session_id = ?1
                "#,
            params![child_session_id],
        )?;
        Ok(())
    }
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS message_queue (
            id TEXT PRIMARY KEY,
            target_session_id TEXT NOT NULL,
            sender_session_id TEXT,
            sender_name TEXT,
            text TEXT NOT NULL,
            delivery_mode TEXT DEFAULT 'sequential',
            from_sm_send INTEGER DEFAULT 0,
            queued_at TIMESTAMP NOT NULL,
            timeout_at TIMESTAMP,
            notify_on_delivery INTEGER DEFAULT 0,
            notify_after_seconds INTEGER,
            notify_on_stop INTEGER DEFAULT 0,
            delivered_at TIMESTAMP,
            remind_soft_threshold INTEGER,
            remind_hard_threshold INTEGER,
            remind_cancel_on_reply_session_id TEXT,
            parent_session_id TEXT,
            message_category TEXT DEFAULT NULL,
            response_relay_source TEXT DEFAULT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_pending
            ON message_queue(target_session_id, delivered_at)
            WHERE delivered_at IS NULL;
        CREATE TABLE IF NOT EXISTS remind_registrations (
            id TEXT PRIMARY KEY,
            target_session_id TEXT NOT NULL UNIQUE,
            soft_threshold_seconds INTEGER NOT NULL,
            hard_threshold_seconds INTEGER NOT NULL,
            registered_at TIMESTAMP NOT NULL,
            last_reset_at TIMESTAMP NOT NULL,
            cancel_on_reply_session_id TEXT,
            soft_fired INTEGER DEFAULT 0,
            is_active INTEGER DEFAULT 1,
            tracked_status_nudge_fired INTEGER DEFAULT 0,
            persistent_tracking INTEGER DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS parent_wake_registrations (
            id TEXT PRIMARY KEY,
            child_session_id TEXT NOT NULL UNIQUE,
            parent_session_id TEXT NOT NULL,
            period_seconds INTEGER NOT NULL,
            registered_at TIMESTAMP NOT NULL,
            last_wake_at TIMESTAMP,
            last_status_at_prev_wake TIMESTAMP,
            escalated INTEGER DEFAULT 0,
            is_active INTEGER DEFAULT 1
        );
        CREATE TABLE IF NOT EXISTS rust_stop_notify_states (
            session_id TEXT PRIMARY KEY,
            sender_session_id TEXT NOT NULL,
            sender_name TEXT,
            delay_seconds INTEGER NOT NULL DEFAULT 0,
            armed_at TIMESTAMP NOT NULL
        );
                "#,
    )?;
    ensure_column(conn, "message_queue", "notify_on_stop", "INTEGER DEFAULT 0")?;
    ensure_column(conn, "message_queue", "from_sm_send", "INTEGER DEFAULT 0")?;
    ensure_column(conn, "message_queue", "remind_soft_threshold", "INTEGER")?;
    ensure_column(conn, "message_queue", "remind_hard_threshold", "INTEGER")?;
    ensure_column(
        conn,
        "message_queue",
        "remind_cancel_on_reply_session_id",
        "TEXT",
    )?;
    ensure_column(conn, "message_queue", "parent_session_id", "TEXT")?;
    ensure_column(
        conn,
        "message_queue",
        "message_category",
        "TEXT DEFAULT NULL",
    )?;
    ensure_column(
        conn,
        "message_queue",
        "response_relay_source",
        "TEXT DEFAULT NULL",
    )?;
    ensure_column(
        conn,
        "remind_registrations",
        "cancel_on_reply_session_id",
        "TEXT",
    )?;
    ensure_column(
        conn,
        "remind_registrations",
        "tracked_status_nudge_fired",
        "INTEGER DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "remind_registrations",
        "persistent_tracking",
        "INTEGER DEFAULT 0",
    )?;
    Ok(())
}

fn ensure_column(conn: &Connection, table: &str, column: &str, column_type: &str) -> Result<()> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !columns.iter().any(|name| name == column) {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {column_type}"),
            [],
        )?;
    }
    Ok(())
}

fn generate_record_id(prefix: &str) -> String {
    let nanos = OffsetDateTime::now_utc().unix_timestamp_nanos();
    format!("{prefix}{:x}{:x}", std::process::id(), nanos as u128)
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use std::{
        env,
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn queue_store_creates_schema_and_writes_retained_rows() {
        let db_path = unique_temp_path("queue");
        let store = RetainedQueueStore::new(db_path.clone());

        store.ensure_schema().unwrap();
        let message_id = store
            .enqueue_message(
                "child001",
                "[sm task-complete] agent child001(worker) completed its task.",
                "important",
                Some("task_complete"),
            )
            .unwrap();
        let wake_id = store
            .register_parent_wake("child001", "em001", 600)
            .unwrap();
        assert!(message_id.starts_with("msg"));
        assert!(wake_id.starts_with("wake"));
        assert_eq!(
            store
                .active_parent_wake_parent("child001")
                .unwrap()
                .as_deref(),
            Some("em001")
        );
        store
            .upsert_stop_notify("child001", "em001", "em", 8)
            .unwrap();
        store
            .upsert_stop_notify("child001", "em002", "other-em", 0)
            .unwrap();
        store.cancel_parent_wake("child001").unwrap();
        store.cancel_remind("child001").unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let row: (String, String, String) = conn
            .query_row(
                "SELECT target_session_id, delivery_mode, message_category FROM message_queue WHERE id = ?1",
                params![message_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "child001".to_owned(),
                "important".to_owned(),
                "task_complete".to_owned()
            )
        );
        let active: i64 = conn
            .query_row(
                "SELECT is_active FROM parent_wake_registrations WHERE child_session_id = 'child001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active, 0);
        let stop_notify: (String, String, i64) = conn
            .query_row(
                "SELECT sender_session_id, sender_name, delay_seconds FROM rust_stop_notify_states WHERE session_id = 'child001'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stop_notify, ("em002".to_owned(), "other-em".to_owned(), 0));
    }

    fn unique_temp_path(label: &str) -> PathBuf {
        let mut path = env::temp_dir();
        path.push(format!(
            "sm-rust-queue-{label}-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        path
    }
}
