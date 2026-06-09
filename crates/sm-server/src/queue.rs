use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use time::{
    format_description::well_known::Rfc3339, macros::format_description, Duration, OffsetDateTime,
    PrimitiveDateTime,
};

#[derive(Debug, Clone)]
pub struct RetainedQueueStore {
    db_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMessage {
    pub id: String,
    pub target_session_id: String,
    pub text: String,
    pub delivery_mode: String,
    pub has_delivery_side_effects: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueMessageMetadata {
    pub sender_session_id: Option<String>,
    pub sender_name: Option<String>,
    pub from_sm_send: bool,
    pub timeout_seconds: Option<u64>,
    pub notify_on_delivery: bool,
    pub notify_after_seconds: Option<u64>,
    pub notify_on_stop: bool,
    pub remind_soft_threshold: Option<u64>,
    pub remind_hard_threshold: Option<u64>,
    pub remind_cancel_on_reply_session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub message_category: Option<String>,
    pub response_relay_source: Option<String>,
}

impl QueueMessageMetadata {
    pub fn has_delivery_side_effects(&self) -> bool {
        self.notify_on_delivery
            || self.notify_after_seconds.is_some()
            || self.notify_on_stop
            || self.remind_soft_threshold.is_some()
            || self.remind_hard_threshold.is_some()
            || self
                .remind_cancel_on_reply_session_id
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .parent_session_id
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
    }
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
        self.enqueue_message_with_metadata(
            target_session_id,
            text,
            delivery_mode,
            QueueMessageMetadata {
                message_category: message_category.map(ToOwned::to_owned),
                ..QueueMessageMetadata::default()
            },
        )
    }

    pub fn enqueue_message_with_metadata(
        &self,
        target_session_id: &str,
        text: &str,
        delivery_mode: &str,
        metadata: QueueMessageMetadata,
    ) -> Result<String> {
        self.with_connection(|conn| {
            let id = generate_record_id("msg");
            let timeout_at = timeout_at_rfc3339(metadata.timeout_seconds)?;
            let response_relay_source = metadata
                .response_relay_source
                .or_else(|| metadata.from_sm_send.then(|| "sm-send".to_owned()));
            conn.execute(
                r#"
                INSERT INTO message_queue
                    (id, target_session_id, sender_session_id, sender_name, text,
                     delivery_mode, from_sm_send, queued_at, timeout_at, notify_on_delivery,
                     notify_after_seconds, notify_on_stop, remind_soft_threshold,
                     remind_hard_threshold, remind_cancel_on_reply_session_id, parent_session_id,
                     message_category, response_relay_source)
                VALUES
                    (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
                "#,
                params![
                    id,
                    target_session_id,
                    metadata.sender_session_id,
                    metadata.sender_name,
                    text,
                    delivery_mode,
                    metadata.from_sm_send,
                    now_rfc3339(),
                    timeout_at,
                    metadata.notify_on_delivery,
                    metadata.notify_after_seconds.map(u64_to_i64).transpose()?,
                    metadata.notify_on_stop,
                    metadata.remind_soft_threshold.map(u64_to_i64).transpose()?,
                    metadata.remind_hard_threshold.map(u64_to_i64).transpose()?,
                    metadata.remind_cancel_on_reply_session_id,
                    metadata.parent_session_id,
                    metadata.message_category,
                    response_relay_source,
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

    pub fn pending_messages_for_target(
        &self,
        target_session_id: &str,
        limit: usize,
    ) -> Result<Vec<PendingMessage>> {
        self.with_connection(|conn| {
            expire_pending_messages_for_target(conn, target_session_id)?;
            let mut statement = conn.prepare(
                r#"
                SELECT id, target_session_id, text, delivery_mode,
                    CASE WHEN
                        notify_on_delivery != 0
                        OR notify_after_seconds IS NOT NULL
                        OR notify_on_stop != 0
                        OR remind_soft_threshold IS NOT NULL
                        OR remind_hard_threshold IS NOT NULL
                        OR (
                            remind_cancel_on_reply_session_id IS NOT NULL
                            AND trim(remind_cancel_on_reply_session_id) != ''
                        )
                        OR (
                            parent_session_id IS NOT NULL
                            AND trim(parent_session_id) != ''
                        )
                    THEN 1 ELSE 0 END AS has_delivery_side_effects
                FROM message_queue
                WHERE target_session_id = ?1 AND delivered_at IS NULL
                ORDER BY queued_at ASC, id ASC
                LIMIT ?2
                "#,
            )?;
            let rows = statement
                .query_map(params![target_session_id, limit.max(1) as i64], |row| {
                    Ok(PendingMessage {
                        id: row.get(0)?,
                        target_session_id: row.get(1)?,
                        text: row.get(2)?,
                        delivery_mode: row.get(3)?,
                        has_delivery_side_effects: row.get::<_, i64>(4)? != 0,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn pending_messages_for_target_by_mode(
        &self,
        target_session_id: &str,
        delivery_mode: &str,
        limit: usize,
    ) -> Result<Vec<PendingMessage>> {
        self.with_connection(|conn| {
            expire_pending_messages_for_target(conn, target_session_id)?;
            let mut statement = conn.prepare(
                r#"
                SELECT id, target_session_id, text, delivery_mode,
                    CASE WHEN
                        notify_on_delivery != 0
                        OR notify_after_seconds IS NOT NULL
                        OR notify_on_stop != 0
                        OR remind_soft_threshold IS NOT NULL
                        OR remind_hard_threshold IS NOT NULL
                        OR (
                            remind_cancel_on_reply_session_id IS NOT NULL
                            AND trim(remind_cancel_on_reply_session_id) != ''
                        )
                        OR (
                            parent_session_id IS NOT NULL
                            AND trim(parent_session_id) != ''
                        )
                    THEN 1 ELSE 0 END AS has_delivery_side_effects
                FROM message_queue
                WHERE target_session_id = ?1
                    AND delivery_mode = ?2
                    AND delivered_at IS NULL
                ORDER BY queued_at ASC, id ASC
                LIMIT ?3
                "#,
            )?;
            let rows = statement
                .query_map(
                    params![target_session_id, delivery_mode, limit.max(1) as i64],
                    |row| {
                        Ok(PendingMessage {
                            id: row.get(0)?,
                            target_session_id: row.get(1)?,
                            text: row.get(2)?,
                            delivery_mode: row.get(3)?,
                            has_delivery_side_effects: row.get::<_, i64>(4)? != 0,
                        })
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn mark_delivered(&self, message_id: &str) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute(
                r#"
                UPDATE message_queue
                SET delivered_at = ?2
                WHERE id = ?1 AND delivered_at IS NULL
                "#,
                params![message_id, now_rfc3339()],
            )?;
            Ok(())
        })
    }

    pub fn message_delivered(&self, message_id: &str) -> Result<bool> {
        self.with_connection(|conn| {
            conn.query_row(
                r#"
                SELECT delivered_at IS NOT NULL
                FROM message_queue
                WHERE id = ?1
                "#,
                params![message_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|value| value.unwrap_or(0) != 0)
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

fn expire_pending_messages_for_target(conn: &Connection, target_session_id: &str) -> Result<()> {
    let mut statement = conn.prepare(
        r#"
        SELECT id, timeout_at
        FROM message_queue
        WHERE target_session_id = ?1
            AND delivered_at IS NULL
            AND timeout_at IS NOT NULL
        "#,
    )?;
    let rows = statement
        .query_map(params![target_session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let now_utc = OffsetDateTime::now_utc();
    let now_local = local_now_naive(now_utc);
    for (id, timeout_at) in rows {
        if timeout_is_expired(&timeout_at, now_utc, now_local) {
            conn.execute(
                "DELETE FROM message_queue WHERE id = ?1 AND delivered_at IS NULL",
                params![id],
            )?;
        }
    }
    Ok(())
}

fn timeout_is_expired(
    timeout_at: &str,
    now_utc: OffsetDateTime,
    now_local: PrimitiveDateTime,
) -> bool {
    let timeout_at = timeout_at.trim();
    if timeout_at.is_empty() {
        return false;
    }
    if let Ok(parsed) = OffsetDateTime::parse(timeout_at, &Rfc3339) {
        return parsed <= now_utc;
    }
    if let Some(parsed) = parse_python_naive_datetime(timeout_at) {
        return parsed <= now_local;
    }
    false
}

fn parse_python_naive_datetime(value: &str) -> Option<PrimitiveDateTime> {
    PrimitiveDateTime::parse(
        value,
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]"),
    )
    .or_else(|_| {
        PrimitiveDateTime::parse(
            value,
            format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]"),
        )
    })
    .ok()
}

fn local_now_naive(now_utc: OffsetDateTime) -> PrimitiveDateTime {
    let local = OffsetDateTime::now_local().unwrap_or(now_utc);
    PrimitiveDateTime::new(local.date(), local.time())
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

fn timeout_at_rfc3339(timeout_seconds: Option<u64>) -> Result<Option<String>> {
    let Some(timeout_seconds) = timeout_seconds.filter(|seconds| *seconds > 0) else {
        return Ok(None);
    };
    let timeout_at = OffsetDateTime::now_utc() + Duration::seconds(u64_to_i64(timeout_seconds)?);
    Ok(Some(timeout_at.format(&Rfc3339)?))
}

fn u64_to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).context("queue metadata seconds value is too large")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use std::{
        env,
        sync::atomic::{AtomicU64, Ordering},
    };
    use time::Duration;

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
        let pending = store.pending_messages_for_target("child001", 10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, message_id);
        store.mark_delivered(&message_id).unwrap();
        assert!(store.message_delivered(&message_id).unwrap());
        assert!(store
            .pending_messages_for_target("child001", 10)
            .unwrap()
            .is_empty());
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

    #[test]
    fn pending_messages_skip_and_delete_expired_timeouts() {
        let db_path = unique_temp_path("queue-expiry");
        let store = RetainedQueueStore::new(db_path.clone());
        store.ensure_schema().unwrap();
        let now_utc = OffsetDateTime::now_utc();
        let now_local = local_now_naive(now_utc);
        let expired_naive = python_naive_timestamp(now_local - Duration::seconds(5));
        let future_naive = python_naive_timestamp(now_local + Duration::minutes(5));
        let expired_rfc3339 = (now_utc - Duration::seconds(5)).format(&Rfc3339).unwrap();
        let queued_at = now_rfc3339();
        let conn = Connection::open(&db_path).unwrap();
        for (id, text, timeout_at) in [
            (
                "expired-naive",
                "expired naive",
                Some(expired_naive.as_str()),
            ),
            (
                "expired-rfc3339",
                "expired rfc3339",
                Some(expired_rfc3339.as_str()),
            ),
            ("future-naive", "future naive", Some(future_naive.as_str())),
            ("no-timeout", "no timeout", None),
        ] {
            conn.execute(
                r#"
                INSERT INTO message_queue
                    (id, target_session_id, text, delivery_mode, from_sm_send, queued_at, timeout_at)
                VALUES
                    (?1, 'child001', ?2, 'sequential', 1, ?3, ?4)
                "#,
                params![id, text, queued_at, timeout_at],
            )
            .unwrap();
        }

        let pending = store.pending_messages_for_target("child001", 10).unwrap();
        let pending_texts = pending
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(pending_texts, vec!["future naive", "no timeout"]);
        let expired_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_queue WHERE id IN ('expired-naive', 'expired-rfc3339')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(expired_count, 0);
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

    fn python_naive_timestamp(value: PrimitiveDateTime) -> String {
        value
            .format(format_description!(
                "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]"
            ))
            .unwrap()
    }
}
