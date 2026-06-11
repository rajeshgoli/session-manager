use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{
    params,
    types::{Value as SqlValue, ValueRef},
    Connection, OpenFlags, OptionalExtension,
};
use serde_json::{Number as JsonNumber, Value as JsonValue};
use time::{
    format_description::well_known::Rfc3339, macros::format_description, Duration, OffsetDateTime,
    PrimitiveDateTime,
};

#[derive(Debug, Clone)]
pub struct RetainedQueueStore {
    db_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct CodexReviewRequestFilters {
    pub notify_session_id: Option<String>,
    pub repo: Option<String>,
    pub pr_number: Option<i64>,
    pub include_inactive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexReviewRequestRegistration {
    pub id: String,
    pub repo: String,
    pub pr_number: i64,
    pub requester_session_id: Option<String>,
    pub notify_session_id: String,
    pub steer: Option<String>,
    pub requested_at: String,
    pub latest_request_comment_id: Option<i64>,
    pub latest_request_comment_url: Option<String>,
    pub latest_request_posted_at: Option<String>,
    pub attempt_count: i64,
    pub next_retry_at: Option<String>,
    pub poll_interval_seconds: i64,
    pub retry_interval_seconds: i64,
    pub pickup_detected_at: Option<String>,
    pub pickup_source: Option<String>,
    pub review_landed_at: Option<String>,
    pub review_source: Option<String>,
    pub review_comment_id: Option<JsonValue>,
    pub review_url: Option<String>,
    pub last_polled_at: Option<String>,
    pub last_error: Option<String>,
    pub state: String,
    pub is_active: bool,
}

#[derive(Debug, Clone, Default)]
pub struct QueueJobFilters {
    pub notify_session_id: Option<String>,
    pub job_type: Option<String>,
    pub state: Option<String>,
    pub include_terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueJobRecord {
    pub id: String,
    pub job_type: String,
    pub label: String,
    pub requester_session_id: Option<String>,
    pub notify_session_id: Option<String>,
    pub cwd: String,
    pub argv: Option<Vec<String>>,
    pub script_path: Option<String>,
    pub timeout_seconds: i64,
    pub state: String,
    pub holding_reason: Option<String>,
    pub queued_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub pid: Option<i64>,
    pub process_group_id: Option<i64>,
    pub exit_code: Option<i64>,
    pub log_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMessage {
    pub id: String,
    pub target_session_id: String,
    pub text: String,
    pub delivery_mode: String,
    pub has_delivery_side_effects: bool,
    pub sender_session_id: Option<String>,
    pub sender_name: Option<String>,
    pub from_sm_send: bool,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopNotifyState {
    pub session_id: String,
    pub sender_session_id: String,
    pub sender_name: String,
    pub delay_seconds: i64,
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

    pub fn list_codex_review_requests_from_path(
        db_path: &Path,
        filters: CodexReviewRequestFilters,
    ) -> Result<Vec<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(Vec::new());
        }
        let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(conn) => conn,
            Err(_) => return Ok(Vec::new()),
        };
        list_codex_review_requests_conn(&conn, filters)
    }

    pub fn list_queue_jobs_from_path(
        db_path: &Path,
        filters: QueueJobFilters,
    ) -> Result<Vec<QueueJobRecord>> {
        if !db_path.exists() {
            return Ok(Vec::new());
        }
        let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(conn) => conn,
            Err(_) => return Ok(Vec::new()),
        };
        list_queue_jobs_conn(&conn, filters)
    }

    pub fn get_queue_job_from_path(db_path: &Path, job_id: &str) -> Result<Option<QueueJobRecord>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(conn) => conn,
            Err(_) => return Ok(None),
        };
        get_queue_job_conn(&conn, job_id)
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
            enqueue_message_with_metadata_conn(
                conn,
                target_session_id,
                text,
                delivery_mode,
                metadata,
            )
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
                    THEN 1 ELSE 0 END AS has_delivery_side_effects,
                    sender_session_id, sender_name, from_sm_send, notify_on_delivery,
                    notify_after_seconds, notify_on_stop, remind_soft_threshold,
                    remind_hard_threshold, remind_cancel_on_reply_session_id,
                    parent_session_id, message_category, response_relay_source
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
                        sender_session_id: row.get(5)?,
                        sender_name: row.get(6)?,
                        from_sm_send: row.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
                        notify_on_delivery: row.get::<_, Option<i64>>(8)?.unwrap_or(0) != 0,
                        notify_after_seconds: row.get::<_, Option<i64>>(9)?.map(i64_to_u64),
                        notify_on_stop: row.get::<_, Option<i64>>(10)?.unwrap_or(0) != 0,
                        remind_soft_threshold: row.get::<_, Option<i64>>(11)?.map(i64_to_u64),
                        remind_hard_threshold: row.get::<_, Option<i64>>(12)?.map(i64_to_u64),
                        remind_cancel_on_reply_session_id: row.get(13)?,
                        parent_session_id: row.get(14)?,
                        message_category: row.get(15)?,
                        response_relay_source: row.get(16)?,
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
                    THEN 1 ELSE 0 END AS has_delivery_side_effects,
                    sender_session_id, sender_name, from_sm_send, notify_on_delivery,
                    notify_after_seconds, notify_on_stop, remind_soft_threshold,
                    remind_hard_threshold, remind_cancel_on_reply_session_id,
                    parent_session_id, message_category, response_relay_source
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
                            sender_session_id: row.get(5)?,
                            sender_name: row.get(6)?,
                            from_sm_send: row.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
                            notify_on_delivery: row.get::<_, Option<i64>>(8)?.unwrap_or(0) != 0,
                            notify_after_seconds: row.get::<_, Option<i64>>(9)?.map(i64_to_u64),
                            notify_on_stop: row.get::<_, Option<i64>>(10)?.unwrap_or(0) != 0,
                            remind_soft_threshold: row.get::<_, Option<i64>>(11)?.map(i64_to_u64),
                            remind_hard_threshold: row.get::<_, Option<i64>>(12)?.map(i64_to_u64),
                            remind_cancel_on_reply_session_id: row.get(13)?,
                            parent_session_id: row.get(14)?,
                            message_category: row.get(15)?,
                            response_relay_source: row.get(16)?,
                        })
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn mark_delivered(&self, message_id: &str) -> Result<()> {
        self.with_connection(|conn| {
            let _ = mark_delivered_conn(conn, message_id)?;
            Ok(())
        })
    }

    pub fn mark_delivered_and_apply_side_effects(&self, message: &PendingMessage) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let result = (|| -> Result<()> {
                if !mark_delivered_conn(conn, &message.id)? {
                    return Ok(());
                }
                if message.notify_on_delivery {
                    if let Some(sender_session_id) = message.sender_session_id.as_deref() {
                        enqueue_message_with_metadata_conn(
                            conn,
                            sender_session_id,
                            &delivery_notification_text(message),
                            "sequential",
                            QueueMessageMetadata::default(),
                        )?;
                    }
                }
                if message.notify_on_stop {
                    if let Some(sender_session_id) = message.sender_session_id.as_deref() {
                        upsert_stop_notify_conn(
                            conn,
                            &message.target_session_id,
                            sender_session_id,
                            message.sender_name.as_deref().unwrap_or(""),
                            0,
                        )?;
                    }
                }
                if let Some(soft_threshold) = message.remind_soft_threshold {
                    let hard_threshold = message
                        .remind_hard_threshold
                        .unwrap_or_else(|| soft_threshold.saturating_add(120));
                    register_remind_conn(
                        conn,
                        &message.target_session_id,
                        soft_threshold,
                        hard_threshold,
                        message.remind_cancel_on_reply_session_id.as_deref(),
                    )?;
                }
                if message.remind_soft_threshold.is_some() {
                    if let Some(parent_session_id) = message.parent_session_id.as_deref() {
                        register_parent_wake_conn(
                            conn,
                            &message.target_session_id,
                            parent_session_id,
                            600,
                        )?;
                    }
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    conn.execute_batch("COMMIT")?;
                    Ok(())
                }
                Err(error) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(error)
                }
            }
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
            register_parent_wake_conn(conn, child_session_id, parent_session_id, period_seconds)
        })
    }

    pub fn register_remind(
        &self,
        target_session_id: &str,
        soft_threshold_seconds: u64,
        hard_threshold_seconds: u64,
        cancel_on_reply_session_id: Option<&str>,
    ) -> Result<String> {
        self.with_connection(|conn| {
            register_remind_conn(
                conn,
                target_session_id,
                soft_threshold_seconds,
                hard_threshold_seconds,
                cancel_on_reply_session_id,
            )
        })
    }

    pub fn cancel_parent_wake(&self, child_session_id: &str) -> Result<()> {
        self.with_connection(|conn| cancel_parent_wake_conn(conn, child_session_id))
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
            upsert_stop_notify_conn(
                conn,
                session_id,
                sender_session_id,
                sender_name,
                delay_seconds,
            )?;
            Ok(())
        })
    }

    pub fn stop_notify_state(&self, session_id: &str) -> Result<Option<StopNotifyState>> {
        self.with_connection(|conn| {
            conn.query_row(
                r#"
                SELECT session_id, sender_session_id, COALESCE(sender_name, ''), delay_seconds
                FROM rust_stop_notify_states
                WHERE session_id = ?1
                "#,
                params![session_id],
                |row| {
                    Ok(StopNotifyState {
                        session_id: row.get(0)?,
                        sender_session_id: row.get(1)?,
                        sender_name: row.get(2)?,
                        delay_seconds: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
        })
    }

    pub fn clear_stop_notify(&self, session_id: &str) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute(
                "DELETE FROM rust_stop_notify_states WHERE session_id = ?1",
                params![session_id],
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
}

fn enqueue_message_with_metadata_conn(
    conn: &Connection,
    target_session_id: &str,
    text: &str,
    delivery_mode: &str,
    metadata: QueueMessageMetadata,
) -> Result<String> {
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
}

fn mark_delivered_conn(conn: &Connection, message_id: &str) -> Result<bool> {
    let changed = conn.execute(
        r#"
        UPDATE message_queue
        SET delivered_at = ?2
        WHERE id = ?1 AND delivered_at IS NULL
        "#,
        params![message_id, now_rfc3339()],
    )?;
    Ok(changed > 0)
}

fn register_remind_conn(
    conn: &Connection,
    target_session_id: &str,
    soft_threshold_seconds: u64,
    hard_threshold_seconds: u64,
    cancel_on_reply_session_id: Option<&str>,
) -> Result<String> {
    let id = generate_record_id("remind");
    let now = now_rfc3339();
    conn.execute(
        "UPDATE remind_registrations SET is_active = 0 WHERE target_session_id = ?1",
        params![target_session_id],
    )?;
    conn.execute(
        r#"
        INSERT OR REPLACE INTO remind_registrations
            (id, target_session_id, soft_threshold_seconds, hard_threshold_seconds,
             registered_at, last_reset_at, cancel_on_reply_session_id, persistent_tracking,
             tracked_status_nudge_fired, soft_fired, is_active)
        VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, 0, 0, 0, 1)
        "#,
        params![
            id,
            target_session_id,
            u64_to_i64(soft_threshold_seconds)?,
            u64_to_i64(hard_threshold_seconds)?,
            now,
            cancel_on_reply_session_id,
        ],
    )?;
    Ok(id)
}

fn register_parent_wake_conn(
    conn: &Connection,
    child_session_id: &str,
    parent_session_id: &str,
    period_seconds: i64,
) -> Result<String> {
    cancel_parent_wake_conn(conn, child_session_id)?;
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
}

fn cancel_parent_wake_conn(conn: &Connection, child_session_id: &str) -> Result<()> {
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

fn upsert_stop_notify_conn(
    conn: &Connection,
    session_id: &str,
    sender_session_id: &str,
    sender_name: &str,
    delay_seconds: i64,
) -> Result<()> {
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
}

fn delivery_notification_text(message: &PendingMessage) -> String {
    let truncated = truncate_chars(&message.text, 100);
    format!(
        "[sm] Message delivered to {}\nOriginal: \"{}\"",
        message.target_session_id, truncated
    )
}

pub fn followup_notification_text(message: &PendingMessage) -> Option<String> {
    let seconds = message.notify_after_seconds?;
    let truncated = truncate_chars(&message.text, 100);
    Some(format!(
        "[sm] Reminder: {seconds}s since your message to {} was delivered\n\
Original: \"{}\"\n\
You can check status with: sm output {}",
        message.target_session_id, truncated, message.target_session_id
    ))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
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

fn list_codex_review_requests_conn(
    conn: &Connection,
    filters: CodexReviewRequestFilters,
) -> Result<Vec<CodexReviewRequestRegistration>> {
    let mut where_clauses = Vec::new();
    let mut values = Vec::<SqlValue>::new();
    if let Some(value) = filters.notify_session_id {
        where_clauses.push("notify_session_id = ?");
        values.push(value.into());
    }
    if let Some(value) = filters.repo {
        where_clauses.push("repo = ?");
        values.push(value.into());
    }
    if let Some(value) = filters.pr_number {
        where_clauses.push("pr_number = ?");
        values.push(value.into());
    }
    if !filters.include_inactive {
        where_clauses.push("is_active = 1");
    }

    let mut query = r#"
        SELECT id, repo, pr_number, requester_session_id, notify_session_id, steer,
               requested_at, latest_request_comment_id, latest_request_comment_url,
               latest_request_posted_at, attempt_count, next_retry_at,
               poll_interval_seconds, retry_interval_seconds, pickup_detected_at,
               pickup_source, review_landed_at, review_source, review_comment_id,
               review_url, last_polled_at, last_error, state, is_active
        FROM codex_review_request_registrations
    "#
    .to_owned();
    if !where_clauses.is_empty() {
        query.push_str(" WHERE ");
        query.push_str(&where_clauses.join(" AND "));
    }
    query.push_str(" ORDER BY requested_at");

    let mut statement = match conn.prepare(&query) {
        Ok(statement) => statement,
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.contains("no such table") =>
        {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error.into()),
    };
    let rows = statement.query_map(rusqlite::params_from_iter(values), |row| {
        Ok(CodexReviewRequestRegistration {
            id: row.get(0)?,
            repo: row.get(1)?,
            pr_number: row.get(2)?,
            requester_session_id: row.get(3)?,
            notify_session_id: row.get(4)?,
            steer: row.get(5)?,
            requested_at: row.get(6)?,
            latest_request_comment_id: row.get(7)?,
            latest_request_comment_url: row.get(8)?,
            latest_request_posted_at: row.get(9)?,
            attempt_count: row.get(10)?,
            next_retry_at: row.get(11)?,
            poll_interval_seconds: row.get(12)?,
            retry_interval_seconds: row.get(13)?,
            pickup_detected_at: row.get(14)?,
            pickup_source: row.get(15)?,
            review_landed_at: row.get(16)?,
            review_source: row.get(17)?,
            review_comment_id: optional_sqlite_json_scalar(row.get_ref(18)?),
            review_url: row.get(19)?,
            last_polled_at: row.get(20)?,
            last_error: row.get(21)?,
            state: row.get(22)?,
            is_active: row.get::<_, Option<i64>>(23)?.unwrap_or(1) != 0,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn list_queue_jobs_conn(
    conn: &Connection,
    filters: QueueJobFilters,
) -> Result<Vec<QueueJobRecord>> {
    let mut where_clauses = Vec::new();
    let mut values = Vec::<SqlValue>::new();
    if let Some(value) = filters.notify_session_id {
        where_clauses.push("notify_session_id = ?");
        values.push(value.into());
    }
    if let Some(value) = filters.job_type {
        where_clauses.push("type = ?");
        values.push(value.into());
    }
    if let Some(value) = filters.state {
        if value == "done" {
            where_clauses
                .push("state IN ('succeeded', 'failed', 'timed_out', 'cancelled', 'displaced')");
        } else {
            where_clauses.push("state = ?");
            values.push(value.into());
        }
    } else if !filters.include_terminal {
        where_clauses.push("state IN ('pending', 'running')");
    }

    let mut query = r#"
        SELECT id, type, label, requester_session_id, notify_session_id, cwd,
               argv_json, script_path, timeout_seconds, state, holding_reason,
               queued_at, started_at, finished_at, pid, process_group_id,
               exit_code, log_path
        FROM queue_jobs
    "#
    .to_owned();
    if !where_clauses.is_empty() {
        query.push_str(" WHERE ");
        query.push_str(&where_clauses.join(" AND "));
    }
    query.push_str(" ORDER BY queued_at");

    let mut statement = match conn.prepare(&query) {
        Ok(statement) => statement,
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.contains("no such table") =>
        {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error.into()),
    };
    let rows = statement.query_map(
        rusqlite::params_from_iter(values),
        queue_job_record_from_row,
    )?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn get_queue_job_conn(conn: &Connection, job_id: &str) -> Result<Option<QueueJobRecord>> {
    let mut statement = match conn.prepare(
        r#"
        SELECT id, type, label, requester_session_id, notify_session_id, cwd,
               argv_json, script_path, timeout_seconds, state, holding_reason,
               queued_at, started_at, finished_at, pid, process_group_id,
               exit_code, log_path
        FROM queue_jobs
        WHERE id = ?1
        LIMIT 1
        "#,
    ) {
        Ok(statement) => statement,
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.contains("no such table") =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    statement
        .query_row(params![job_id], queue_job_record_from_row)
        .optional()
        .map_err(Into::into)
}

fn queue_job_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueueJobRecord> {
    let argv_json: Option<String> = row.get(6)?;
    let argv = match argv_json
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(raw) => Some(serde_json::from_str::<Vec<String>>(raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?),
        None => None,
    };
    Ok(QueueJobRecord {
        id: row.get(0)?,
        job_type: row.get(1)?,
        label: row.get(2)?,
        requester_session_id: row.get(3)?,
        notify_session_id: row
            .get::<_, Option<String>>(4)?
            .filter(|value| !value.is_empty()),
        cwd: row.get(5)?,
        argv,
        script_path: row.get(7)?,
        timeout_seconds: row.get(8)?,
        state: row.get(9)?,
        holding_reason: row.get(10)?,
        queued_at: row.get(11)?,
        started_at: row.get(12)?,
        finished_at: row.get(13)?,
        pid: row.get(14)?,
        process_group_id: row.get(15)?,
        exit_code: row.get(16)?,
        log_path: row.get(17)?,
    })
}

fn optional_sqlite_json_scalar(value: ValueRef<'_>) -> Option<JsonValue> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(value) => Some(JsonValue::Number(value.into())),
        ValueRef::Real(value) => JsonNumber::from_f64(value).map(JsonValue::Number),
        ValueRef::Text(value) => Some(JsonValue::String(
            String::from_utf8_lossy(value).into_owned(),
        )),
        ValueRef::Blob(value) => Some(JsonValue::String(
            String::from_utf8_lossy(value).into_owned(),
        )),
    }
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

fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
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
