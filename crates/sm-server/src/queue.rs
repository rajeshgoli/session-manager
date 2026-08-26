#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration as StdDuration, Instant},
};

use anyhow::{bail, Context, Result};
use rand_core::{OsRng, RngCore};
use rusqlite::{
    params,
    types::{Value as SqlValue, ValueRef},
    Connection, OpenFlags, OptionalExtension,
};
use serde::Serialize;
use serde_json::{Number as JsonNumber, Value as JsonValue};
use time::{
    format_description::well_known::Rfc3339, macros::format_description, Duration, OffsetDateTime,
    PrimitiveDateTime,
};

#[derive(Debug, Clone)]
pub struct RetainedQueueStore {
    db_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledReminder {
    pub id: String,
    pub target_session_id: String,
    pub message: String,
    pub fire_at: String,
    pub recurring_interval_seconds: Option<i64>,
    pub fired: bool,
    pub is_active: bool,
}

impl ScheduledReminder {
    pub fn overdue_seconds(&self, now_utc: OffsetDateTime) -> Option<i64> {
        scheduled_reminder_elapsed_seconds(&self.fire_at, now_utc)
    }

    pub fn recurring_interval_is_valid_at(&self, now_utc: OffsetDateTime) -> bool {
        self.recurring_interval_seconds.is_none_or(|interval| {
            interval > 0
                && local_now_naive(now_utc).is_some_and(|now_local| {
                    now_local.checked_add(Duration::seconds(interval)).is_some()
                })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledReminderDelivery {
    pub reminder: ScheduledReminder,
    pub queue_message_id: String,
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
    pub requested_head_sha: Option<String>,
    pub superseded_by_request_id: Option<String>,
    pub superseded_at: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateCodexReviewRequest {
    pub repo: String,
    pub pr_number: i64,
    pub requester_session_id: Option<String>,
    pub notify_session_id: String,
    pub steer: Option<String>,
    pub requested_head_sha: String,
    pub latest_request_comment_id: Option<i64>,
    pub latest_request_comment_url: Option<String>,
    pub latest_request_posted_at: String,
    pub poll_interval_seconds: i64,
    pub retry_interval_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryCodexReviewRequest {
    pub latest_request_comment_id: Option<i64>,
    pub latest_request_comment_url: Option<String>,
    pub latest_request_posted_at: String,
    pub next_retry_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteCodexReviewRequest {
    pub review_landed_at: String,
    pub review_source: Option<String>,
    pub review_comment_id: Option<JsonValue>,
    pub review_url: Option<String>,
    pub last_polled_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct QueueJobFilters {
    pub notify_session_id: Option<String>,
    pub job_type: Option<String>,
    pub state: Option<String>,
    pub include_terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueueJobRecord {
    pub id: String,
    #[serde(rename = "type")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueAdmissionPolicy {
    pub max_running_jobs: i64,
    pub perf_cooldown_seconds: i64,
    pub tests_max_concurrent: usize,
    pub perf_max_concurrent: usize,
    pub background_max_concurrent: usize,
    pub service_max_concurrent: usize,
}

impl Default for QueueAdmissionPolicy {
    fn default() -> Self {
        Self {
            max_running_jobs: DEFAULT_MAX_RUNNING_QUEUE_JOBS,
            perf_cooldown_seconds: DEFAULT_PERF_COOLDOWN_SECONDS,
            tests_max_concurrent: 2,
            perf_max_concurrent: 1,
            background_max_concurrent: 2,
            service_max_concurrent: 0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueRecoverySummary {
    pub recovered_running: usize,
    pub retried_completion_notifications: usize,
    pub started_pending: usize,
    pub requeued_pending: usize,
    pub held_pending: usize,
    pub polling_running: usize,
    pub finished_succeeded: usize,
    pub finished_failed: usize,
    pub finished_timed_out: usize,
    pub finished_cancelled: usize,
    pub finished_displaced: usize,
}

#[derive(Debug, Clone)]
struct QueueJobRuntimeRecord {
    id: String,
    job_type: String,
    state: String,
    notify_session_id: Option<String>,
    queued_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    holding_reason: Option<String>,
    wrapper_path: Option<String>,
    log_path: Option<String>,
    exit_code_path: Option<String>,
    timeout_seconds: i64,
    pid: Option<i64>,
    process_group_id: Option<i64>,
    exit_code: Option<i64>,
    completion_notified_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveredQueueJobAction {
    Polling,
    Finished(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateQueueJob {
    pub job_type: String,
    pub label: String,
    pub requester_session_id: Option<String>,
    pub notify_session_id: String,
    pub cwd: String,
    pub argv: Option<Vec<String>>,
    pub script: Option<String>,
    pub env: BTreeMap<String, String>,
    pub timeout_seconds: i64,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParentRoutingWakeRow {
    pub id: String,
    pub child_session_id: String,
    pub parent_session_id: String,
    pub period_seconds: i64,
    pub is_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParentRoutingMessageRow {
    pub id: String,
    pub child_session_id: String,
    pub parent_session_id: String,
    pub creates_parent_wake: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ParentRoutingSnapshot {
    pub wake_rows: Vec<ParentRoutingWakeRow>,
    pub message_rows: Vec<ParentRoutingMessageRow>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParentRoutingRetargetResult {
    pub delivered_message_rows: Vec<ParentRoutingMessageRow>,
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

    pub fn ensure_codex_review_requests_schema_from_path(db_path: &Path) -> Result<()> {
        if !db_path.exists() {
            return Ok(());
        }
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_codex_review_requests_schema(&conn)
    }

    pub fn list_active_codex_review_requests_from_path(
        db_path: &Path,
    ) -> Result<Vec<CodexReviewRequestRegistration>> {
        Self::list_codex_review_requests_from_path(
            db_path,
            CodexReviewRequestFilters {
                include_inactive: false,
                ..CodexReviewRequestFilters::default()
            },
        )
    }

    pub fn get_codex_review_request_from_path(
        db_path: &Path,
        request_id: &str,
    ) -> Result<Option<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(conn) => conn,
            Err(_) => return Ok(None),
        };
        get_codex_review_request_conn(&conn, request_id)
    }

    pub fn cancel_codex_review_request_in_path(
        db_path: &Path,
        request_id: &str,
    ) -> Result<Option<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(db_path)?;
        let Some(mut registration) = get_codex_review_request_conn(&conn, request_id)? else {
            return Ok(None);
        };
        if !registration.is_active {
            return Ok(Some(registration));
        }
        registration.is_active = false;
        registration.state = "cancelled".to_owned();
        conn.execute(
            r#"
            UPDATE codex_review_request_registrations
            SET is_active = 0,
                state = ?2,
                last_error = ?3
            WHERE id = ?1 AND is_active = 1
            "#,
            params![
                request_id,
                registration.state.as_str(),
                registration.last_error.as_deref()
            ],
        )?;
        Ok(Some(registration))
    }

    pub fn cancel_codex_review_request_with_error_in_path(
        db_path: &Path,
        request_id: &str,
        last_error: &str,
    ) -> Result<Option<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(db_path)?;
        let Some(mut registration) = get_codex_review_request_conn(&conn, request_id)? else {
            return Ok(None);
        };
        if !registration.is_active {
            return Ok(Some(registration));
        }
        registration.is_active = false;
        registration.state = "cancelled".to_owned();
        registration.last_error = Some(last_error.to_owned());
        conn.execute(
            r#"
            UPDATE codex_review_request_registrations
            SET is_active = 0,
                state = ?2,
                last_error = ?3
            WHERE id = ?1 AND is_active = 1
            "#,
            params![request_id, registration.state.as_str(), last_error],
        )?;
        Ok(Some(registration))
    }

    pub fn create_codex_review_request_in_path(
        db_path: &Path,
        request: CreateCodexReviewRequest,
    ) -> Result<CodexReviewRequestRegistration> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create message queue db directory {}",
                    parent.display()
                )
            })?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("failed to open message queue db {}", db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_codex_review_requests_schema(&conn)?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = create_codex_review_request_conn(&conn, request);
        match result {
            Ok(registration) => {
                conn.execute_batch("COMMIT")?;
                Ok(registration)
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn active_codex_review_request_exists_from_path(
        db_path: &Path,
        repo: &str,
        pr_number: i64,
        notify_session_id: &str,
    ) -> Result<bool> {
        if !db_path.exists() {
            return Ok(false);
        }
        let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(conn) => conn,
            Err(_) => return Ok(false),
        };
        active_codex_review_request_exists_conn(&conn, repo, pr_number, notify_session_id)
    }

    pub fn active_codex_review_requests_for_pr_from_path(
        db_path: &Path,
        repo: &str,
        pr_number: i64,
    ) -> Result<Vec<CodexReviewRequestRegistration>> {
        Self::list_codex_review_requests_from_path(
            db_path,
            CodexReviewRequestFilters {
                repo: Some(repo.to_owned()),
                pr_number: Some(pr_number),
                include_inactive: false,
                ..CodexReviewRequestFilters::default()
            },
        )
    }

    pub fn mark_codex_review_request_pickup_in_path(
        db_path: &Path,
        request_id: &str,
        last_polled_at: &str,
    ) -> Result<Option<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(db_path)?;
        let changed = conn.execute(
            r#"
            UPDATE codex_review_request_registrations
            SET pickup_detected_at = COALESCE(pickup_detected_at, ?2),
                pickup_source = COALESCE(pickup_source, 'reaction'),
                last_polled_at = ?2,
                last_error = NULL
            WHERE id = ?1 AND is_active = 1
            "#,
            params![request_id, last_polled_at],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        get_codex_review_request_conn(&conn, request_id)
    }

    pub fn mark_codex_review_request_poll_error_in_path(
        db_path: &Path,
        request_id: &str,
        last_polled_at: &str,
        last_error: &str,
        next_retry_at: Option<&str>,
    ) -> Result<Option<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(db_path)?;
        let changed = conn.execute(
            r#"
            UPDATE codex_review_request_registrations
            SET last_polled_at = ?2,
                last_error = ?3,
                next_retry_at = COALESCE(?4, next_retry_at)
            WHERE id = ?1 AND is_active = 1
            "#,
            params![request_id, last_polled_at, last_error, next_retry_at],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        get_codex_review_request_conn(&conn, request_id)
    }

    pub fn mark_codex_review_request_polled_in_path(
        db_path: &Path,
        request_id: &str,
        last_polled_at: &str,
    ) -> Result<Option<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(db_path)?;
        let changed = conn.execute(
            r#"
            UPDATE codex_review_request_registrations
            SET last_polled_at = ?2,
                last_error = NULL
            WHERE id = ?1 AND is_active = 1
            "#,
            params![request_id, last_polled_at],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        get_codex_review_request_conn(&conn, request_id)
    }

    pub fn backfill_codex_review_request_head_in_path(
        db_path: &Path,
        request_id: &str,
        requested_head_sha: &str,
        last_polled_at: &str,
    ) -> Result<Option<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(db_path)?;
        let changed = conn.execute(
            r#"
            UPDATE codex_review_request_registrations
            SET requested_head_sha = ?2,
                last_polled_at = ?3,
                last_error = NULL
            WHERE id = ?1
                AND is_active = 1
                AND requested_head_sha IS NULL
            "#,
            params![request_id, requested_head_sha, last_polled_at],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        get_codex_review_request_conn(&conn, request_id)
    }

    pub fn retry_codex_review_request_in_path(
        db_path: &Path,
        request_id: &str,
        retry: RetryCodexReviewRequest,
        last_polled_at: &str,
    ) -> Result<Option<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(db_path)?;
        let changed = conn.execute(
            r#"
            UPDATE codex_review_request_registrations
            SET attempt_count = attempt_count + 1,
                latest_request_comment_id = ?2,
                latest_request_comment_url = ?3,
                latest_request_posted_at = ?4,
                pickup_detected_at = NULL,
                pickup_source = NULL,
                next_retry_at = ?5,
                last_polled_at = ?6,
                last_error = NULL
            WHERE id = ?1 AND is_active = 1
            "#,
            params![
                request_id,
                retry.latest_request_comment_id,
                retry.latest_request_comment_url,
                retry.latest_request_posted_at,
                retry.next_retry_at,
                last_polled_at,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        get_codex_review_request_conn(&conn, request_id)
    }

    pub fn complete_codex_review_request_in_path(
        db_path: &Path,
        request_id: &str,
        completion: CompleteCodexReviewRequest,
    ) -> Result<Option<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(db_path)?;
        let review_comment_id = completion.review_comment_id.map(json_scalar_to_sql_value);
        let changed = conn.execute(
            r#"
            UPDATE codex_review_request_registrations
            SET review_landed_at = ?2,
                review_source = ?3,
                review_comment_id = ?4,
                review_url = ?5,
                state = 'completed',
                is_active = 0,
                last_polled_at = ?6,
                last_error = NULL
            WHERE id = ?1 AND is_active = 1
            "#,
            params![
                request_id,
                completion.review_landed_at,
                completion.review_source,
                review_comment_id,
                completion.review_url,
                completion.last_polled_at,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        get_codex_review_request_conn(&conn, request_id)
    }

    pub fn complete_codex_review_request_and_enqueue_in_path(
        db_path: &Path,
        request_id: &str,
        completion: CompleteCodexReviewRequest,
        wake_text: &str,
    ) -> Result<Option<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_schema(&conn)?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let review_comment_id = completion.review_comment_id.map(json_scalar_to_sql_value);
            let changed = conn.execute(
                r#"
                UPDATE codex_review_request_registrations
                SET review_landed_at = ?2,
                    review_source = ?3,
                    review_comment_id = ?4,
                    review_url = ?5,
                    state = 'completed',
                    is_active = 0,
                    last_polled_at = ?6,
                    last_error = NULL
                WHERE id = ?1 AND is_active = 1
                "#,
                params![
                    request_id,
                    completion.review_landed_at,
                    completion.review_source,
                    review_comment_id,
                    completion.review_url,
                    completion.last_polled_at,
                ],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            let registration = get_codex_review_request_conn(&conn, request_id)?
                .ok_or_else(|| anyhow::anyhow!("completed Codex review request disappeared"))?;
            enqueue_message_with_metadata_conn(
                &conn,
                &registration.notify_session_id,
                wake_text,
                "sequential",
                QueueMessageMetadata::default(),
            )?;
            Ok(Some(registration))
        })();
        match result {
            Ok(registration) => {
                conn.execute_batch("COMMIT")?;
                Ok(registration)
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn supersede_codex_review_request_and_enqueue_in_path(
        db_path: &Path,
        request_id: &str,
        superseded_at: &str,
        reason: &str,
        wake_text: &str,
    ) -> Result<Option<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_schema(&conn)?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let changed = conn.execute(
                r#"
                UPDATE codex_review_request_registrations
                SET state = 'superseded',
                    is_active = 0,
                    superseded_at = ?2,
                    next_retry_at = NULL,
                    last_polled_at = ?2,
                    last_error = ?3
                WHERE id = ?1 AND is_active = 1
                "#,
                params![request_id, superseded_at, reason],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            let registration = get_codex_review_request_conn(&conn, request_id)?
                .ok_or_else(|| anyhow::anyhow!("superseded Codex review request disappeared"))?;
            enqueue_message_with_metadata_conn(
                &conn,
                &registration.notify_session_id,
                wake_text,
                "sequential",
                QueueMessageMetadata::default(),
            )?;
            Ok(Some(registration))
        })();
        match result {
            Ok(registration) => {
                conn.execute_batch("COMMIT")?;
                Ok(registration)
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
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
        match Self::get_queue_job_strict_from_path(db_path, job_id) {
            Ok(job) => Ok(job),
            Err(_) => Ok(None),
        }
    }

    pub fn get_queue_job_strict_from_path(
        db_path: &Path,
        job_id: &str,
    ) -> Result<Option<QueueJobRecord>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("failed to open queue runner db {}", db_path.display()))?;
        get_queue_job_conn(&conn, job_id)
    }

    pub fn create_queue_job_in_state_dir(
        state_dir: &Path,
        request: CreateQueueJob,
    ) -> Result<QueueJobRecord> {
        std::fs::create_dir_all(state_dir).with_context(|| {
            format!(
                "failed to create queue runner state dir {}",
                state_dir.display()
            )
        })?;
        std::fs::create_dir_all(state_dir.join("logs")).with_context(|| {
            format!(
                "failed to create queue runner log dir {}",
                state_dir.join("logs").display()
            )
        })?;
        let db_path = state_dir.join("queue_runner.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open queue runner db {}", db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_queue_jobs_schema(&conn)?;
        create_queue_job_conn(&conn, state_dir, request)
    }

    pub fn start_queue_job_in_state_dir(
        state_dir: &Path,
        message_queue_db_path: &Path,
        job_id: &str,
        cancel_grace_seconds: u64,
    ) -> Result<Option<QueueJobRecord>> {
        Self::start_queue_job_in_state_dir_with_policy(
            state_dir,
            message_queue_db_path,
            job_id,
            cancel_grace_seconds,
            QueueAdmissionPolicy::default(),
        )
    }

    fn start_queue_job_in_state_dir_with_policy(
        state_dir: &Path,
        message_queue_db_path: &Path,
        job_id: &str,
        cancel_grace_seconds: u64,
        admission_policy: QueueAdmissionPolicy,
    ) -> Result<Option<QueueJobRecord>> {
        let db_path = state_dir.join("queue_runner.db");
        let conn = open_queue_jobs_connection(&db_path)?;
        init_queue_jobs_schema(&conn)?;
        let Some(job) = get_queue_job_runtime_conn(&conn, job_id)? else {
            return Ok(None);
        };
        if job.state != "pending" {
            return get_queue_job_conn(&conn, job_id);
        }
        let child = match spawn_queue_job_process(&job) {
            Ok(child) => child,
            Err(error) => {
                finish_queue_job_conn(&conn, &job, "failed", None, Some(message_queue_db_path))?;
                return Err(error);
            }
        };
        let pid = i64::from(child.id());
        conn.execute(
            r#"
            UPDATE queue_jobs
            SET state = 'running',
                holding_reason = NULL,
                started_at = ?2,
                pid = ?3,
                process_group_id = ?3
            WHERE id = ?1 AND state = 'pending'
            "#,
            params![job_id, now_rfc3339(), pid],
        )?;
        let monitor_state_dir = state_dir.to_path_buf();
        let monitor_message_queue_db_path = message_queue_db_path.to_path_buf();
        let monitor_job_id = job_id.to_owned();
        let timeout_seconds = job.timeout_seconds;
        thread::spawn(move || {
            monitor_queue_job_completion(
                monitor_state_dir,
                monitor_message_queue_db_path,
                monitor_job_id,
                child,
                timeout_seconds,
                cancel_grace_seconds,
                admission_policy,
            );
        });
        get_queue_job_conn(&conn, job_id)
    }

    pub fn admit_queue_jobs_in_state_dir(
        state_dir: &Path,
        message_queue_db_path: &Path,
        cancel_grace_seconds: u64,
    ) -> Result<()> {
        Self::admit_queue_jobs_in_state_dir_with_policy(
            state_dir,
            message_queue_db_path,
            cancel_grace_seconds,
            QueueAdmissionPolicy::default(),
            false,
        )
    }

    pub fn admit_queue_jobs_in_state_dir_continuing_after_failed_start(
        state_dir: &Path,
        message_queue_db_path: &Path,
        cancel_grace_seconds: u64,
    ) -> Result<()> {
        Self::admit_queue_jobs_in_state_dir_continuing_after_failed_start_with_policy(
            state_dir,
            message_queue_db_path,
            cancel_grace_seconds,
            QueueAdmissionPolicy::default(),
        )
    }

    pub fn admit_queue_jobs_in_state_dir_continuing_after_failed_start_with_policy(
        state_dir: &Path,
        message_queue_db_path: &Path,
        cancel_grace_seconds: u64,
        admission_policy: QueueAdmissionPolicy,
    ) -> Result<()> {
        Self::admit_queue_jobs_in_state_dir_with_policy(
            state_dir,
            message_queue_db_path,
            cancel_grace_seconds,
            admission_policy,
            true,
        )
    }

    fn admit_queue_jobs_in_state_dir_with_policy(
        state_dir: &Path,
        message_queue_db_path: &Path,
        cancel_grace_seconds: u64,
        admission_policy: QueueAdmissionPolicy,
        continue_after_failed_start: bool,
    ) -> Result<()> {
        let db_path = state_dir.join("queue_runner.db");
        let conn = open_queue_jobs_connection(&db_path)?;
        init_queue_jobs_schema(&conn)?;
        admit_pending_queue_jobs_conn(
            &conn,
            state_dir,
            message_queue_db_path,
            cancel_grace_seconds,
            admission_policy,
            continue_after_failed_start,
        )?;
        Ok(())
    }

    pub fn cancel_queue_job_in_state_dir(
        state_dir: &Path,
        message_queue_db_path: &Path,
        job_id: &str,
        cancel_grace_seconds: u64,
        admission_policy: QueueAdmissionPolicy,
        admit_after_cancel: bool,
    ) -> Result<Option<QueueJobRecord>> {
        let db_path = state_dir.join("queue_runner.db");
        let conn = open_queue_jobs_connection(&db_path)?;
        init_queue_jobs_schema(&conn)?;
        let Some(job) = get_queue_job_runtime_conn(&conn, job_id)? else {
            return Ok(None);
        };
        if is_terminal_queue_state(&job.state) {
            return get_queue_job_conn(&conn, job_id);
        }
        if job.state == "running" {
            mark_queue_job_cancelling_conn(&conn, job_id)?;
            if let Some(pgid) = job.process_group_id.or(job.pid) {
                terminate_process_group_with_grace(pgid, cancel_grace_seconds);
            }
        }
        let exit_code = read_exit_code(job.exit_code_path.as_deref());
        finish_queue_job_conn(
            &conn,
            &job,
            "cancelled",
            exit_code,
            Some(message_queue_db_path),
        )?;
        if admit_after_cancel {
            let _ = admit_pending_queue_jobs_conn(
                &conn,
                state_dir,
                message_queue_db_path,
                cancel_grace_seconds,
                admission_policy,
                true,
            );
        }
        get_queue_job_conn(&conn, job_id)
    }

    pub fn recover_queue_jobs_in_state_dir(
        state_dir: &Path,
        message_queue_db_path: &Path,
        cancel_grace_seconds: u64,
    ) -> Result<QueueRecoverySummary> {
        Self::recover_queue_jobs_in_state_dir_with_policy(
            state_dir,
            message_queue_db_path,
            cancel_grace_seconds,
            QueueAdmissionPolicy::default(),
        )
    }

    pub fn recover_queue_jobs_in_state_dir_with_policy(
        state_dir: &Path,
        message_queue_db_path: &Path,
        cancel_grace_seconds: u64,
        admission_policy: QueueAdmissionPolicy,
    ) -> Result<QueueRecoverySummary> {
        let db_path = state_dir.join("queue_runner.db");
        if !db_path.exists() {
            return Ok(QueueRecoverySummary::default());
        }
        let conn = open_queue_jobs_connection(&db_path)?;
        init_queue_jobs_schema(&conn)?;
        let mut statement = conn.prepare(
            r#"
            SELECT id
            FROM queue_jobs
            WHERE state = 'running'
            ORDER BY queued_at, id
            "#,
        )?;
        let job_ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);

        let mut summary = QueueRecoverySummary::default();
        for job_id in job_ids {
            let Some(job) = get_queue_job_runtime_conn(&conn, &job_id)? else {
                continue;
            };
            if job.state.as_str() == "running" {
                summary.recovered_running += 1;
                match recover_running_queue_job_conn(
                    &conn,
                    state_dir,
                    message_queue_db_path,
                    &job,
                    cancel_grace_seconds,
                    admission_policy,
                )? {
                    RecoveredQueueJobAction::Polling => summary.polling_running += 1,
                    RecoveredQueueJobAction::Finished("succeeded") => {
                        summary.finished_succeeded += 1
                    }
                    RecoveredQueueJobAction::Finished("failed") => summary.finished_failed += 1,
                    RecoveredQueueJobAction::Finished("timed_out") => {
                        summary.finished_timed_out += 1
                    }
                    RecoveredQueueJobAction::Finished("cancelled") => {
                        summary.finished_cancelled += 1
                    }
                    RecoveredQueueJobAction::Finished("displaced") => {
                        summary.finished_displaced += 1
                    }
                    RecoveredQueueJobAction::Finished(_) => {}
                }
            }
        }
        let admission = admit_pending_queue_jobs_conn(
            &conn,
            state_dir,
            message_queue_db_path,
            cancel_grace_seconds,
            admission_policy,
            true,
        )?;
        summary.started_pending += admission.started;
        summary.requeued_pending += admission.requeued;
        summary.held_pending += admission.held;
        summary.finished_failed += admission.failed_start;
        summary.retried_completion_notifications +=
            retry_unnotified_queue_job_completions_conn(&conn, message_queue_db_path)?;
        Ok(summary)
    }

    pub fn retry_unnotified_queue_job_completions_in_state_dir(
        state_dir: &Path,
        message_queue_db_path: &Path,
    ) -> Result<usize> {
        let db_path = state_dir.join("queue_runner.db");
        if !db_path.exists() {
            return Ok(0);
        }
        let conn = open_queue_jobs_connection(&db_path)?;
        init_queue_jobs_schema(&conn)?;
        retry_unnotified_queue_job_completions_conn(&conn, message_queue_db_path)
    }

    pub fn ensure_schema(&self) -> Result<()> {
        self.with_connection(|_| Ok(()))
    }

    pub fn schedule_reminder(
        &self,
        target_session_id: &str,
        delay_seconds: u64,
        message: &str,
        recurring_interval_seconds: Option<u64>,
    ) -> Result<ScheduledReminder> {
        let target_session_id = target_session_id.trim();
        if target_session_id.is_empty() {
            anyhow::bail!("target_session_id must not be empty");
        }
        if delay_seconds == 0 {
            anyhow::bail!("delay_seconds must be greater than zero");
        }
        if recurring_interval_seconds == Some(0) {
            anyhow::bail!("recurring_interval_seconds must be greater than zero");
        }
        let message = if message.trim().is_empty() {
            "Reminder"
        } else {
            message.trim()
        };
        let delay_seconds = u64_to_i64(delay_seconds)?;
        let recurring_interval_seconds = recurring_interval_seconds.map(u64_to_i64).transpose()?;
        let now_utc = OffsetDateTime::now_utc();
        let fire_at_local = local_now_naive(now_utc)
            .context("could not determine the local UTC offset for reminder persistence")?
            .checked_add(Duration::seconds(delay_seconds))
            .context("reminder delay is outside the supported timestamp range")?;
        if let Some(interval) = recurring_interval_seconds {
            fire_at_local
                .checked_add(Duration::seconds(interval))
                .context("recurring reminder interval is outside the supported timestamp range")?;
        }
        let fire_at = python_compatible_reminder_timestamp(fire_at_local)?;
        let id = generate_scheduled_reminder_id();
        self.with_connection(|conn| {
            conn.execute(
                r#"
                INSERT INTO scheduled_reminders
                    (id, target_session_id, message, fire_at, task_type, fired,
                     recurring_interval_seconds, is_active)
                VALUES (?1, ?2, ?3, ?4, 'reminder', 0, ?5, 1)
                "#,
                params![
                    id,
                    target_session_id,
                    message,
                    fire_at,
                    recurring_interval_seconds
                ],
            )?;
            scheduled_reminder_conn(conn, &id)?
                .ok_or_else(|| anyhow::anyhow!("scheduled reminder {id} was not persisted"))
        })
    }

    pub fn cancel_scheduled_reminder(
        &self,
        reminder_id: &str,
    ) -> Result<Option<ScheduledReminder>> {
        self.with_connection(|conn| {
            let Some(reminder) = scheduled_reminder_conn(conn, reminder_id)? else {
                return Ok(None);
            };
            conn.execute(
                "UPDATE scheduled_reminders SET is_active = 0 WHERE id = ?1",
                params![reminder_id],
            )?;
            Ok(Some(reminder))
        })
    }

    pub fn due_scheduled_reminders(
        &self,
        now_utc: OffsetDateTime,
    ) -> Result<Vec<ScheduledReminder>> {
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }
        let conn = Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let mut statement = conn.prepare(
            r#"
            SELECT id, target_session_id, message, fire_at,
                   recurring_interval_seconds, fired, is_active
            FROM scheduled_reminders
            WHERE is_active = 1
              AND (fired = 0 OR recurring_interval_seconds IS NOT NULL)
            ORDER BY fire_at, id
            "#,
        )?;
        let reminders = statement
            .query_map([], scheduled_reminder_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(reminders
            .into_iter()
            .filter(|reminder| scheduled_reminder_is_due(&reminder.fire_at, now_utc))
            .collect())
    }

    pub fn deactivate_scheduled_reminder(&self, reminder_id: &str) -> Result<bool> {
        self.with_connection(|conn| {
            Ok(conn.execute(
                "UPDATE scheduled_reminders SET is_active = 0 WHERE id = ?1 AND is_active = 1",
                params![reminder_id],
            )? > 0)
        })
    }

    pub fn claim_due_scheduled_reminder(
        &self,
        reminder_id: &str,
        now_utc: OffsetDateTime,
    ) -> Result<Option<ScheduledReminderDelivery>> {
        self.with_connection(|conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let result = (|| -> Result<Option<ScheduledReminderDelivery>> {
                let Some(reminder) = scheduled_reminder_conn(conn, reminder_id)? else {
                    return Ok(None);
                };
                if !reminder.is_active
                    || reminder.fired && reminder.recurring_interval_seconds.is_none()
                    || !scheduled_reminder_is_due(&reminder.fire_at, now_utc)
                {
                    return Ok(None);
                }

                let text = scheduled_reminder_message(&reminder);
                let queue_message_id = enqueue_message_with_metadata_conn(
                    conn,
                    &reminder.target_session_id,
                    &text,
                    "urgent",
                    QueueMessageMetadata {
                        message_category: Some("scheduled_reminder".to_owned()),
                        ..QueueMessageMetadata::default()
                    },
                )?;
                let changed = if let Some(interval) = reminder.recurring_interval_seconds {
                    let next_fire_at_local = local_now_naive(now_utc)
                        .context(
                            "could not determine the local UTC offset for reminder persistence",
                        )?
                        .checked_add(Duration::seconds(interval))
                        .context("recurring reminder interval exceeds the timestamp range")?;
                    let next_fire_at = python_compatible_reminder_timestamp(next_fire_at_local)?;
                    conn.execute(
                        r#"
                        UPDATE scheduled_reminders
                        SET fire_at = ?2, fired = 0, is_active = 1
                        WHERE id = ?1 AND is_active = 1
                        "#,
                        params![reminder.id, next_fire_at],
                    )?
                } else {
                    conn.execute(
                        r#"
                        UPDATE scheduled_reminders
                        SET fired = 1, is_active = 0
                        WHERE id = ?1 AND is_active = 1
                        "#,
                        params![reminder.id],
                    )?
                };
                if changed != 1 {
                    anyhow::bail!(
                        "scheduled reminder {} changed while being claimed",
                        reminder.id
                    );
                }
                Ok(Some(ScheduledReminderDelivery {
                    reminder,
                    queue_message_id,
                }))
            })();
            match result {
                Ok(value) => {
                    conn.execute_batch("COMMIT")?;
                    Ok(value)
                }
                Err(error) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(error)
                }
            }
        })
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

    pub fn enqueue_message_once_with_metadata(
        &self,
        id: &str,
        target_session_id: &str,
        text: &str,
        delivery_mode: &str,
        metadata: QueueMessageMetadata,
    ) -> Result<()> {
        self.with_connection(|conn| {
            let message_category = metadata.message_category.clone();
            let inserted = enqueue_message_with_id_and_metadata_conn(
                conn,
                id,
                target_session_id,
                text,
                delivery_mode,
                metadata,
            )?;
            if inserted {
                return Ok(());
            }
            let existing_matches = conn.query_row(
                r#"
                SELECT COUNT(*)
                FROM message_queue
                WHERE id = ?1
                  AND target_session_id = ?2
                  AND text = ?3
                  AND delivery_mode = ?4
                  AND message_category IS ?5
                "#,
                params![id, target_session_id, text, delivery_mode, message_category,],
                |row| row.get::<_, i64>(0),
            )? > 0;
            if !existing_matches {
                anyhow::bail!("queue message id {id} already exists with different content");
            }
            Ok(())
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

    pub fn snapshot_parent_routing(
        &self,
        child_session_id: &str,
        expected_parent_session_id: &str,
    ) -> Result<ParentRoutingSnapshot> {
        self.with_connection(|conn| {
            snapshot_parent_routing_conn(conn, child_session_id, expected_parent_session_id)
        })
    }

    pub fn quiesce_parent_routing(&self, snapshot: &ParentRoutingSnapshot) -> Result<()> {
        self.with_connection(|conn| {
            with_immediate_transaction(conn, |conn| {
                for wake in &snapshot.wake_rows {
                    let current = parent_routing_wake_row_conn(conn, &wake.id)?
                        .with_context(|| format!("parent wake {} disappeared", wake.id))?;
                    if current.child_session_id != wake.child_session_id
                        || current.parent_session_id != wake.parent_session_id
                        || current.period_seconds != wake.period_seconds
                        || (current.is_active != wake.is_active && current.is_active)
                    {
                        anyhow::bail!("parent wake {} changed after planning", wake.id);
                    }
                    conn.execute(
                        "UPDATE parent_wake_registrations SET is_active = 0 WHERE id = ?1",
                        params![wake.id],
                    )?;
                }
                for message in &snapshot.message_rows {
                    let (current_child, current_parent, delivered) =
                        parent_routing_message_state_conn(conn, &message.id)?.with_context(|| {
                            format!("queue message {} disappeared", message.id)
                        })?;
                    if current_child != message.child_session_id
                        || (current_parent.as_deref()
                            != Some(message.parent_session_id.as_str())
                            && current_parent.is_some())
                    {
                        anyhow::bail!("queue message {} changed after planning", message.id);
                    }
                    if delivered {
                        anyhow::bail!("queue message {} delivered before quiesce", message.id);
                    }
                    conn.execute(
                        "UPDATE message_queue SET parent_session_id = NULL WHERE id = ?1 AND delivered_at IS NULL",
                        params![message.id],
                    )?;
                }
                Ok(())
            })
        })
    }

    pub fn retarget_parent_routing(
        &self,
        snapshot: &ParentRoutingSnapshot,
        new_parent_session_id: Option<&str>,
    ) -> Result<ParentRoutingRetargetResult> {
        self.with_connection(|conn| {
            with_immediate_transaction(conn, |conn| {
                let mut result = ParentRoutingRetargetResult::default();
                for wake in &snapshot.wake_rows {
                    let current = parent_routing_wake_row_conn(conn, &wake.id)?
                        .with_context(|| format!("parent wake {} disappeared", wake.id))?;
                    let valid = current.child_session_id == wake.child_session_id
                        && current.period_seconds == wake.period_seconds
                        && ((current.parent_session_id == wake.parent_session_id
                            && current.is_active == wake.is_active)
                            || (current.parent_session_id == wake.parent_session_id
                                && !current.is_active)
                            || new_parent_session_id.is_some_and(|new_parent| {
                                current.parent_session_id == new_parent
                                    && current.is_active == wake.is_active
                            }));
                    if !valid {
                        anyhow::bail!("parent wake {} changed after planning", wake.id);
                    }
                    if let Some(new_parent) = new_parent_session_id {
                        conn.execute(
                            "UPDATE parent_wake_registrations SET parent_session_id = ?2, is_active = ?3 WHERE id = ?1",
                            params![wake.id, new_parent, i64::from(wake.is_active)],
                        )?;
                    } else {
                        conn.execute(
                            "UPDATE parent_wake_registrations SET is_active = 0 WHERE id = ?1",
                            params![wake.id],
                        )?;
                    }
                }
                for message in &snapshot.message_rows {
                    let (current_child, current_parent, delivered) =
                        parent_routing_message_state_conn(conn, &message.id)?.with_context(|| {
                            format!("queue message {} disappeared", message.id)
                        })?;
                    if current_child != message.child_session_id
                        || (current_parent.as_deref()
                            != Some(message.parent_session_id.as_str())
                        && current_parent.is_some()
                        && current_parent.as_deref() != new_parent_session_id)
                    {
                        anyhow::bail!("queue message {} changed after planning", message.id);
                    }
                    if delivered {
                        result.delivered_message_rows.push(message.clone());
                        continue;
                    }
                    conn.execute(
                        "UPDATE message_queue SET parent_session_id = ?2 WHERE id = ?1 AND delivered_at IS NULL",
                        params![message.id, new_parent_session_id],
                    )?;
                }
                Ok(result)
            })
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

    pub fn pending_target_session_ids_by_category(
        &self,
        message_category: &str,
    ) -> Result<Vec<String>> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare(
                r#"
                SELECT target_session_id
                FROM message_queue
                WHERE delivered_at IS NULL
                    AND message_category = ?1
                    AND trim(target_session_id) != ''
                GROUP BY target_session_id
                ORDER BY MIN(queued_at), target_session_id
                "#,
            )?;
            let targets = statement
                .query_map(params![message_category], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(targets)
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

    pub fn pending_messages_for_target_by_category(
        &self,
        target_session_id: &str,
        message_category: &str,
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
                    AND message_category = ?2
                    AND delivered_at IS NULL
                ORDER BY queued_at ASC, id ASC
                LIMIT ?3
                "#,
            )?;
            let rows = statement
                .query_map(
                    params![target_session_id, message_category, limit.max(1) as i64],
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

    pub fn cancel_pending_messages_for_target_category(
        &self,
        target_session_id: &str,
        message_category: &str,
    ) -> Result<usize> {
        self.with_connection(|conn| {
            let changed = conn.execute(
                r#"
                DELETE FROM message_queue
                WHERE target_session_id = ?1
                  AND message_category = ?2
                  AND delivered_at IS NULL
                "#,
                params![target_session_id, message_category],
            )?;
            Ok(changed)
        })
    }

    /// Drops undelivered messages a session raised about *itself*. A context
    /// reset invalidates the alerts that describe the discarded context, and
    /// those are addressed to the monitor rather than the session, so they
    /// cannot be found by target.
    pub fn cancel_pending_messages_from_sender_category(
        &self,
        sender_session_id: &str,
        message_category: &str,
    ) -> Result<usize> {
        self.with_connection(|conn| {
            let changed = conn.execute(
                r#"
                DELETE FROM message_queue
                WHERE sender_session_id = ?1
                  AND message_category = ?2
                  AND delivered_at IS NULL
                "#,
                params![sender_session_id, message_category],
            )?;
            Ok(changed)
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
    let inserted = enqueue_message_with_id_and_metadata_conn(
        conn,
        &id,
        target_session_id,
        text,
        delivery_mode,
        metadata,
    )?;
    if !inserted {
        anyhow::bail!("generated duplicate queue message id {id}");
    }
    Ok(id)
}

fn enqueue_message_with_id_and_metadata_conn(
    conn: &Connection,
    id: &str,
    target_session_id: &str,
    text: &str,
    delivery_mode: &str,
    metadata: QueueMessageMetadata,
) -> Result<bool> {
    let timeout_at = timeout_at_rfc3339(metadata.timeout_seconds)?;
    let response_relay_source = metadata
        .response_relay_source
        .or_else(|| metadata.from_sm_send.then(|| "sm-send".to_owned()));
    let inserted = conn.execute(
        r#"
        INSERT OR IGNORE INTO message_queue
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
    Ok(inserted > 0)
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

fn snapshot_parent_routing_conn(
    conn: &Connection,
    child_session_id: &str,
    expected_parent_session_id: &str,
) -> Result<ParentRoutingSnapshot> {
    let mut wake_statement = conn.prepare(
        r#"
        SELECT id, child_session_id, parent_session_id, period_seconds, is_active
        FROM parent_wake_registrations
        WHERE child_session_id = ?1
          AND parent_session_id = ?2
          AND is_active = 1
        ORDER BY id
        "#,
    )?;
    let wake_rows = wake_statement
        .query_map(
            params![child_session_id, expected_parent_session_id],
            |row| {
                Ok(ParentRoutingWakeRow {
                    id: row.get(0)?,
                    child_session_id: row.get(1)?,
                    parent_session_id: row.get(2)?,
                    period_seconds: row.get(3)?,
                    is_active: row.get::<_, i64>(4)? != 0,
                })
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut message_statement = conn.prepare(
        r#"
        SELECT id, target_session_id, parent_session_id,
               remind_soft_threshold IS NOT NULL
        FROM message_queue
        WHERE target_session_id = ?1
          AND parent_session_id = ?2
          AND delivered_at IS NULL
        ORDER BY id
        "#,
    )?;
    let message_rows = message_statement
        .query_map(
            params![child_session_id, expected_parent_session_id],
            |row| {
                Ok(ParentRoutingMessageRow {
                    id: row.get(0)?,
                    child_session_id: row.get(1)?,
                    parent_session_id: row.get(2)?,
                    creates_parent_wake: row.get::<_, i64>(3)? != 0,
                })
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ParentRoutingSnapshot {
        wake_rows,
        message_rows,
    })
}

fn parent_routing_wake_row_conn(
    conn: &Connection,
    record_id: &str,
) -> Result<Option<ParentRoutingWakeRow>> {
    conn.query_row(
        r#"
        SELECT id, child_session_id, parent_session_id, period_seconds, is_active
        FROM parent_wake_registrations
        WHERE id = ?1
        "#,
        params![record_id],
        |row| {
            Ok(ParentRoutingWakeRow {
                id: row.get(0)?,
                child_session_id: row.get(1)?,
                parent_session_id: row.get(2)?,
                period_seconds: row.get(3)?,
                is_active: row.get::<_, i64>(4)? != 0,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn parent_routing_message_state_conn(
    conn: &Connection,
    record_id: &str,
) -> Result<Option<(String, Option<String>, bool)>> {
    conn.query_row(
        r#"
        SELECT target_session_id, parent_session_id, delivered_at IS NOT NULL
        FROM message_queue
        WHERE id = ?1
        "#,
        params![record_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0)),
    )
    .optional()
    .map_err(Into::into)
}

fn with_immediate_transaction<T>(
    conn: &Connection,
    action: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match action(conn) {
        Ok(value) => {
            conn.execute_batch("COMMIT")?;
            Ok(value)
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
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
        CREATE TABLE IF NOT EXISTS scheduled_reminders (
            id TEXT PRIMARY KEY,
            target_session_id TEXT NOT NULL,
            message TEXT NOT NULL,
            fire_at TIMESTAMP NOT NULL,
            task_type TEXT DEFAULT 'reminder',
            fired INTEGER DEFAULT 0,
            recurring_interval_seconds INTEGER,
            is_active INTEGER DEFAULT 1
        );
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
        "scheduled_reminders",
        "recurring_interval_seconds",
        "INTEGER",
    )?;
    ensure_column(
        conn,
        "scheduled_reminders",
        "is_active",
        "INTEGER DEFAULT 1",
    )?;
    conn.execute(
        r#"
        UPDATE scheduled_reminders
        SET is_active = 1
        WHERE is_active IS NULL
        "#,
        [],
    )?;
    conn.execute(
        r#"
        UPDATE scheduled_reminders
        SET is_active = 0
        WHERE fired = 1
          AND recurring_interval_seconds IS NULL
          AND is_active = 1
        "#,
        [],
    )?;
    conn.execute(
        r#"
        CREATE INDEX IF NOT EXISTS idx_scheduled_reminders_active_fire
        ON scheduled_reminders(is_active, fire_at)
        "#,
        [],
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
    init_codex_review_requests_schema(conn)?;
    Ok(())
}

fn init_queue_jobs_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS queue_jobs (
            id TEXT PRIMARY KEY,
            type TEXT NOT NULL,
            label TEXT NOT NULL,
            requester_session_id TEXT,
            notify_session_id TEXT NOT NULL,
            cwd TEXT NOT NULL,
            argv_json TEXT,
            script_path TEXT,
            env_json TEXT NOT NULL,
            timeout_seconds INTEGER NOT NULL,
            state TEXT NOT NULL,
            holding_reason TEXT,
            queued_at TEXT NOT NULL,
            started_at TEXT,
            finished_at TEXT,
            pid INTEGER,
            process_group_id INTEGER,
            exit_code INTEGER,
            log_path TEXT,
            exit_code_path TEXT,
            wrapper_path TEXT,
            queued_notified_at TEXT,
            started_notified_at TEXT,
            completion_notification_required INTEGER NOT NULL DEFAULT 0,
            completion_notified_at TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_queue_jobs_state_type_queued
            ON queue_jobs(state, type, queued_at);
        CREATE INDEX IF NOT EXISTS idx_queue_jobs_notify_state
            ON queue_jobs(notify_session_id, state);
        CREATE INDEX IF NOT EXISTS idx_queue_jobs_finished
            ON queue_jobs(finished_at);
        CREATE TABLE IF NOT EXISTS queue_resource_samples (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            sampled_at TEXT NOT NULL,
            pending_by_type_json TEXT NOT NULL,
            running_by_type_json TEXT NOT NULL,
            total_running INTEGER NOT NULL,
            memory_json TEXT NOT NULL,
            cpu_json TEXT NOT NULL,
            gpu_json TEXT
        );
        "#,
    )?;
    ensure_column(conn, "queue_jobs", "queued_notified_at", "TEXT")?;
    ensure_column(conn, "queue_jobs", "started_notified_at", "TEXT")?;
    ensure_column(
        conn,
        "queue_jobs",
        "completion_notification_required",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(conn, "queue_jobs", "completion_notified_at", "TEXT")?;
    Ok(())
}

fn init_codex_review_requests_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS codex_review_request_registrations (
            id TEXT PRIMARY KEY,
            repo TEXT NOT NULL,
            pr_number INTEGER NOT NULL,
            requester_session_id TEXT,
            notify_session_id TEXT NOT NULL,
            steer TEXT,
            requested_head_sha TEXT,
            superseded_by_request_id TEXT,
            superseded_at TIMESTAMP,
            requested_at TIMESTAMP NOT NULL,
            latest_request_comment_id INTEGER,
            latest_request_comment_url TEXT,
            latest_request_posted_at TIMESTAMP,
            attempt_count INTEGER NOT NULL,
            next_retry_at TIMESTAMP,
            poll_interval_seconds INTEGER NOT NULL,
            retry_interval_seconds INTEGER NOT NULL,
            pickup_detected_at TIMESTAMP,
            pickup_source TEXT,
            review_landed_at TIMESTAMP,
            review_source TEXT,
            review_comment_id INTEGER,
            review_url TEXT,
            last_polled_at TIMESTAMP,
            last_error TEXT,
            state TEXT NOT NULL,
            is_active INTEGER DEFAULT 1
        );
        "#,
    )?;
    ensure_column(
        conn,
        "codex_review_request_registrations",
        "requested_head_sha",
        "TEXT",
    )?;
    ensure_column(
        conn,
        "codex_review_request_registrations",
        "superseded_by_request_id",
        "TEXT",
    )?;
    ensure_column(
        conn,
        "codex_review_request_registrations",
        "superseded_at",
        "TIMESTAMP",
    )?;
    Ok(())
}

fn create_queue_job_conn(
    conn: &Connection,
    state_dir: &Path,
    request: CreateQueueJob,
) -> Result<QueueJobRecord> {
    let id = generate_queue_job_id();
    let job_dir = state_dir.join(&id);
    std::fs::create_dir_all(&job_dir)
        .with_context(|| format!("failed to create queue job dir {}", job_dir.display()))?;
    let logs_dir = state_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("failed to create queue log dir {}", logs_dir.display()))?;
    let script_path = if let Some(script) = request.script.as_deref() {
        let path = job_dir.join("submitted.zsh");
        std::fs::write(&path, script)
            .with_context(|| format!("failed to write queue job script {}", path.display()))?;
        Some(path.display().to_string())
    } else {
        None
    };
    let exit_code_path = job_dir.join("exit.code");
    let wrapper_path = job_dir.join("run.zsh");
    let log_path = logs_dir.join(format!("{id}.log"));
    write_queue_job_wrapper(
        &wrapper_path,
        &request.cwd,
        request.argv.as_deref(),
        script_path.as_deref(),
        &request.env,
        &exit_code_path,
    )?;
    let queued_at = now_rfc3339();
    let argv_json = request
        .argv
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let env_json = serde_json::to_string(&request.env)?;
    conn.execute(
        r#"
        INSERT INTO queue_jobs
            (id, type, label, requester_session_id, notify_session_id, cwd,
             argv_json, script_path, env_json, timeout_seconds, state,
             holding_reason, queued_at, started_at, finished_at, pid,
             process_group_id, exit_code, log_path, exit_code_path, wrapper_path,
             queued_notified_at, started_notified_at, completion_notified_at)
        VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending',
             NULL, ?11, NULL, NULL, NULL, NULL, NULL, ?12, ?13, ?14,
             NULL, NULL, NULL)
        "#,
        params![
            id,
            request.job_type,
            request.label,
            request.requester_session_id,
            request.notify_session_id,
            request.cwd,
            argv_json,
            script_path,
            env_json,
            request.timeout_seconds,
            queued_at,
            log_path.display().to_string(),
            exit_code_path.display().to_string(),
            wrapper_path.display().to_string(),
        ],
    )?;
    get_queue_job_conn(conn, &id)?.context("created queue job was not persisted")
}

fn write_queue_job_wrapper(
    path: &Path,
    cwd: &str,
    argv: Option<&[String]>,
    script_path: Option<&str>,
    env: &BTreeMap<String, String>,
    exit_code_path: &Path,
) -> Result<()> {
    let mut lines = vec![
        "#!/bin/zsh".to_owned(),
        "set +e".to_owned(),
        format!("cd {} || exit 127", shell_quote(cwd)),
    ];
    for (key, value) in env {
        lines.push(format!(
            "export {}={}",
            shell_quote(key),
            shell_quote(value)
        ));
    }
    if let Some(argv) = argv {
        lines.push(
            argv.iter()
                .map(|part| shell_quote(part))
                .collect::<Vec<_>>()
                .join(" "),
        );
    } else if let Some(script_path) = script_path {
        lines.push(format!("/bin/zsh {}", shell_quote(script_path)));
    }
    lines.extend([
        "code=$?".to_owned(),
        "if [ \"$code\" -eq 127 ]; then".to_owned(),
        "  print -u2 -- \"[sm queue] effective PATH: $PATH\"".to_owned(),
        "fi".to_owned(),
        format!(
            "printf '%s\\n' \"$code\" > {}",
            shell_quote(&exit_code_path.display().to_string())
        ),
        "exit \"$code\"".to_owned(),
    ]);
    std::fs::write(path, format!("{}\n", lines.join("\n")))
        .with_context(|| format!("failed to write queue job wrapper {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn open_queue_jobs_connection(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create queue runner db directory {}",
                parent.display()
            )
        })?;
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open queue runner db {}", db_path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(conn)
}

fn get_queue_job_runtime_conn(
    conn: &Connection,
    job_id: &str,
) -> Result<Option<QueueJobRuntimeRecord>> {
    let mut statement = match conn.prepare(
        r#"
        SELECT id, type, state, notify_session_id, queued_at, started_at, finished_at,
               holding_reason, wrapper_path, log_path, exit_code_path, timeout_seconds,
               pid, process_group_id, exit_code, completion_notified_at
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
        .query_row(params![job_id], |row| {
            Ok(QueueJobRuntimeRecord {
                id: row.get(0)?,
                job_type: row.get(1)?,
                state: row.get(2)?,
                notify_session_id: row.get(3)?,
                queued_at: row.get(4)?,
                started_at: row.get(5)?,
                finished_at: row.get(6)?,
                holding_reason: row.get(7)?,
                wrapper_path: row.get(8)?,
                log_path: row.get(9)?,
                exit_code_path: row.get(10)?,
                timeout_seconds: row.get(11)?,
                pid: row.get(12)?,
                process_group_id: row.get(13)?,
                exit_code: row.get(14)?,
                completion_notified_at: row.get(15)?,
            })
        })
        .optional()
        .map_err(Into::into)
}

fn list_queue_job_runtime_records_conn(conn: &Connection) -> Result<Vec<QueueJobRuntimeRecord>> {
    let mut statement = conn.prepare(
        r#"
        SELECT id, type, state, notify_session_id, queued_at, started_at, finished_at,
               holding_reason, wrapper_path, log_path, exit_code_path, timeout_seconds,
               pid, process_group_id, exit_code, completion_notified_at
        FROM queue_jobs
        ORDER BY queued_at, id
        "#,
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok(QueueJobRuntimeRecord {
                id: row.get(0)?,
                job_type: row.get(1)?,
                state: row.get(2)?,
                notify_session_id: row.get(3)?,
                queued_at: row.get(4)?,
                started_at: row.get(5)?,
                finished_at: row.get(6)?,
                holding_reason: row.get(7)?,
                wrapper_path: row.get(8)?,
                log_path: row.get(9)?,
                exit_code_path: row.get(10)?,
                timeout_seconds: row.get(11)?,
                pid: row.get(12)?,
                process_group_id: row.get(13)?,
                exit_code: row.get(14)?,
                completion_notified_at: row.get(15)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, Default)]
struct QueueAdmissionSummary {
    started: usize,
    requeued: usize,
    held: usize,
    failed_start: usize,
    retry_after_seconds: Option<u64>,
}

fn admit_pending_queue_jobs_conn(
    conn: &Connection,
    state_dir: &Path,
    message_queue_db_path: &Path,
    cancel_grace_seconds: u64,
    admission_policy: QueueAdmissionPolicy,
    continue_after_failed_start: bool,
) -> Result<QueueAdmissionSummary> {
    let _admission_guard = QUEUE_ADMISSION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Validate before clearing any existing holds: after a restart, preserved
    // service processes may outnumber a newly lowered configuration.  In that
    // state, admitting even one finite job would violate the configured global
    // reserve for non-service work.
    let running_jobs = list_queue_job_runtime_records_conn(conn)?;
    ensure_service_capacity_reserve_before_admission(&running_jobs, admission_policy)?;
    let requeued = conn.execute(
        "UPDATE queue_jobs SET holding_reason = NULL WHERE state = 'pending' AND holding_reason IS NOT NULL",
        [],
    )?;
    let mut summary = QueueAdmissionSummary {
        requeued,
        ..QueueAdmissionSummary::default()
    };
    loop {
        let jobs = list_queue_job_runtime_records_conn(conn)?;
        if !jobs.iter().any(|job| job.state == "pending") {
            break;
        }
        if running_queue_job_count(&jobs, Some("perf")) > 0 {
            summary.held += mark_pending_queue_jobs_holding_conn(conn, None, "perf_running")?;
            break;
        }
        let perf_waiting_for_quiet_window = oldest_pending_queue_job(&jobs, "perf").is_some()
            && !perf_blocked_by_tests_after_perf(&jobs);
        if perf_waiting_for_quiet_window {
            if running_queue_job_count(&jobs, Some("tests")) > 0 {
                summary.held += mark_pending_queue_jobs_holding_conn(conn, None, "awaiting_tests")?;
                break;
            }
            if let Some(remaining) = perf_cooldown_remaining_seconds(&jobs, admission_policy) {
                summary.held += mark_pending_queue_jobs_holding_conn(conn, None, "perf_cooldown")?;
                summary.retry_after_seconds = Some(
                    summary
                        .retry_after_seconds
                        .map_or(remaining, |current| current.min(remaining)),
                );
                break;
            }
        }
        if displace_background_for_perf_conn(
            conn,
            &jobs,
            message_queue_db_path,
            cancel_grace_seconds,
            admission_policy,
        )? {
            continue;
        }
        if running_queue_job_count(&jobs, None) as i64 >= admission_policy.max_running_jobs {
            summary.held += mark_pending_queue_jobs_holding_conn(conn, None, "concurrency_cap")?;
            break;
        }
        let Some(candidate_id) =
            next_admissible_queue_job_id_conn(conn, &jobs, admission_policy, &mut summary)?
        else {
            break;
        };
        match RetainedQueueStore::start_queue_job_in_state_dir_with_policy(
            state_dir,
            message_queue_db_path,
            &candidate_id,
            cancel_grace_seconds,
            admission_policy,
        ) {
            Ok(Some(job)) if job.state == "running" => summary.started += 1,
            Ok(_) => {}
            Err(error) => {
                let Some(current) = get_queue_job_conn(conn, &candidate_id)? else {
                    return Err(error);
                };
                if continue_after_failed_start && current.state == "failed" {
                    summary.failed_start += 1;
                } else {
                    return Err(error);
                }
            }
        }
    }
    if let Some(seconds) = summary.retry_after_seconds {
        schedule_queue_admission_retry(
            state_dir.to_path_buf(),
            message_queue_db_path.to_path_buf(),
            cancel_grace_seconds,
            admission_policy,
            seconds,
        );
    }
    Ok(summary)
}

fn ensure_service_capacity_reserve_before_admission(
    jobs: &[QueueJobRuntimeRecord],
    admission_policy: QueueAdmissionPolicy,
) -> Result<()> {
    let global_capacity = usize::try_from(admission_policy.max_running_jobs)
        .context("queue max_running_jobs must be a non-negative integer")?;
    let service_capacity = admission_policy.service_max_concurrent;
    if service_capacity >= global_capacity && service_capacity != 0 {
        bail!(
            "queue service capacity configuration is invalid before admission: \
             service_cap={service_capacity} global_capacity={global_capacity}; \
             service capacity must leave at least one non-service slot"
        );
    }

    let non_service_reserve = global_capacity.saturating_sub(service_capacity);
    let recovered_service_ids = jobs
        .iter()
        .filter(|job| job.state == "running" && job.job_type == "service")
        .map(|job| job.id.as_str())
        .collect::<Vec<_>>();
    let recovered_service_count = recovered_service_ids.len();
    let preserves_service_cap = recovered_service_count <= service_capacity;
    let preserves_global_reserve = recovered_service_count
        .checked_add(non_service_reserve)
        .is_some_and(|required_capacity| required_capacity <= global_capacity);
    if preserves_service_cap && preserves_global_reserve {
        return Ok(());
    }

    bail!(
        "queue admission blocked: recovered live service occupancy violates configured capacity; \
         recovered_live_service_jobs={recovered_service_count} \
         service_cap={service_capacity} global_capacity={global_capacity} \
         non_service_reserve={non_service_reserve} \
         job_ids=[{}]. Recovered jobs were left unchanged and no pending job was admitted; \
         reduce live service occupancy or raise the configuration before retrying recovery.",
        recovered_service_ids.join(", ")
    );
}

fn schedule_queue_admission_retry(
    state_dir: PathBuf,
    message_queue_db_path: PathBuf,
    cancel_grace_seconds: u64,
    admission_policy: QueueAdmissionPolicy,
    delay_seconds: u64,
) {
    thread::spawn(move || {
        thread::sleep(StdDuration::from_secs(delay_seconds.max(1)));
        let _ =
            RetainedQueueStore::admit_queue_jobs_in_state_dir_continuing_after_failed_start_with_policy(
                &state_dir,
                &message_queue_db_path,
                cancel_grace_seconds,
                admission_policy,
            );
    });
}

const DEFAULT_MAX_RUNNING_QUEUE_JOBS: i64 = 2;
const DEFAULT_PERF_COOLDOWN_SECONDS: i64 = 30;
const QUEUE_JOB_TYPE_ORDER: [&str; 4] = ["perf", "tests", "background", "service"];
static QUEUE_ADMISSION_LOCK: Mutex<()> = Mutex::new(());

fn next_admissible_queue_job_id_conn(
    conn: &Connection,
    jobs: &[QueueJobRuntimeRecord],
    admission_policy: QueueAdmissionPolicy,
    summary: &mut QueueAdmissionSummary,
) -> Result<Option<String>> {
    for job_type in QUEUE_JOB_TYPE_ORDER {
        let Some(job) = oldest_pending_queue_job(jobs, job_type) else {
            continue;
        };
        if running_queue_job_count(jobs, Some(job_type))
            >= admission_policy.max_concurrent_jobs(job_type)
        {
            summary.held +=
                mark_pending_queue_jobs_holding_conn(conn, Some(&job.id), "concurrency_cap")?;
            continue;
        }
        if job_type == "perf" && perf_cooldown_active(jobs, admission_policy) {
            summary.held +=
                mark_pending_queue_jobs_holding_conn(conn, Some(&job.id), "perf_cooldown")?;
            if let Some(remaining) = perf_cooldown_remaining_seconds(jobs, admission_policy) {
                summary.retry_after_seconds = Some(
                    summary
                        .retry_after_seconds
                        .map_or(remaining, |current| current.min(remaining)),
                );
            }
            continue;
        }
        if job_type == "perf" && perf_blocked_by_tests_after_perf(jobs) {
            summary.held +=
                mark_pending_queue_jobs_holding_conn(conn, Some(&job.id), "awaiting_tests")?;
            continue;
        }
        return Ok(Some(job.id.clone()));
    }
    Ok(None)
}

fn displace_background_for_perf_conn(
    conn: &Connection,
    jobs: &[QueueJobRuntimeRecord],
    message_queue_db_path: &Path,
    cancel_grace_seconds: u64,
    admission_policy: QueueAdmissionPolicy,
) -> Result<bool> {
    if oldest_pending_queue_job(jobs, "perf").is_none() {
        return Ok(false);
    }
    if running_queue_job_count(jobs, Some("perf")) >= admission_policy.max_concurrent_jobs("perf") {
        return Ok(false);
    }
    if perf_cooldown_active(jobs, admission_policy) || perf_blocked_by_tests_after_perf(jobs) {
        return Ok(false);
    }
    if running_queue_job_count(jobs, Some("tests")) > 0 {
        return Ok(false);
    }
    let Some(background) = jobs
        .iter()
        .filter(|job| job.state == "running" && job.job_type == "background")
        .min_by_key(|job| (job.started_at.as_deref().unwrap_or(&job.queued_at), &job.id))
        .cloned()
    else {
        return Ok(false);
    };
    mark_queue_job_displacing_conn(conn, &background.id)?;
    let mut background = background;
    background.holding_reason = Some("displacing".to_owned());
    if let Some(pgid) = background.process_group_id.or(background.pid) {
        terminate_process_group_with_grace(pgid, cancel_grace_seconds);
    }
    let exit_code = read_exit_code(background.exit_code_path.as_deref());
    finish_queue_job_conn(
        conn,
        &background,
        "displaced",
        exit_code,
        Some(message_queue_db_path),
    )?;
    Ok(true)
}

fn oldest_pending_queue_job<'a>(
    jobs: &'a [QueueJobRuntimeRecord],
    job_type: &str,
) -> Option<&'a QueueJobRuntimeRecord> {
    jobs.iter()
        .filter(|job| job.state == "pending" && job.job_type == job_type)
        .min_by_key(|job| (&job.queued_at, &job.id))
}

fn running_queue_job_count(jobs: &[QueueJobRuntimeRecord], job_type: Option<&str>) -> usize {
    jobs.iter()
        .filter(|job| {
            job.state == "running"
                && match job_type {
                    Some(expected) => job.job_type == expected,
                    None => true,
                }
        })
        .count()
}

impl QueueAdmissionPolicy {
    fn max_concurrent_jobs(self, job_type: &str) -> usize {
        match job_type {
            "tests" => self.tests_max_concurrent,
            "perf" => self.perf_max_concurrent,
            "background" => self.background_max_concurrent,
            "service" => self.service_max_concurrent,
            _ => 1,
        }
    }
}

fn mark_pending_queue_jobs_holding_conn(
    conn: &Connection,
    job_id: Option<&str>,
    reason: &str,
) -> Result<usize> {
    let updated = if let Some(job_id) = job_id {
        conn.execute(
            r#"
            UPDATE queue_jobs
            SET holding_reason = ?2
            WHERE id = ?1 AND state = 'pending' AND COALESCE(holding_reason, '') != ?2
            "#,
            params![job_id, reason],
        )?
    } else {
        conn.execute(
            r#"
            UPDATE queue_jobs
            SET holding_reason = ?1
            WHERE state = 'pending' AND COALESCE(holding_reason, '') != ?1
            "#,
            params![reason],
        )?
    };
    Ok(updated)
}

fn perf_cooldown_active(
    jobs: &[QueueJobRuntimeRecord],
    admission_policy: QueueAdmissionPolicy,
) -> bool {
    perf_cooldown_remaining_seconds(jobs, admission_policy).is_some()
}

fn perf_cooldown_remaining_seconds(
    jobs: &[QueueJobRuntimeRecord],
    admission_policy: QueueAdmissionPolicy,
) -> Option<u64> {
    if admission_policy.perf_cooldown_seconds <= 0 {
        return None;
    }
    let now = OffsetDateTime::now_utc();
    jobs.iter()
        .filter(|job| matches!(job.job_type.as_str(), "perf" | "tests"))
        .filter_map(|job| {
            let elapsed = queue_elapsed_since(job.finished_at.as_deref()?, now)?;
            if elapsed < 0 || elapsed >= admission_policy.perf_cooldown_seconds {
                return None;
            }
            let remaining: u64 = (admission_policy.perf_cooldown_seconds - elapsed)
                .try_into()
                .ok()?;
            Some(remaining.max(1))
        })
        .max()
}

fn perf_blocked_by_tests_after_perf(jobs: &[QueueJobRuntimeRecord]) -> bool {
    let now = OffsetDateTime::now_utc();
    let latest = jobs
        .iter()
        .filter(|job| matches!(job.job_type.as_str(), "perf" | "tests"))
        .filter_map(|job| {
            let finished_at = job.finished_at.as_deref()?;
            let elapsed = queue_elapsed_since(finished_at, now)?;
            if elapsed < 0 {
                return None;
            }
            Some((elapsed, job.job_type.as_str()))
        })
        .min_by_key(|(elapsed, _)| *elapsed);
    matches!(latest, Some((_, "perf")))
        && jobs.iter().any(|job| {
            job.job_type == "tests" && matches!(job.state.as_str(), "pending" | "running")
        })
}

fn spawn_queue_job_process(job: &QueueJobRuntimeRecord) -> Result<Child> {
    let wrapper_path = job
        .wrapper_path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .context("queue job has no wrapper path")?;
    let log_path = job
        .log_path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .context("queue job has no log path")?;
    if let Some(parent) = Path::new(log_path).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create queue log dir {}", parent.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("failed to open queue job log {log_path}"))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("failed to clone queue job log {log_path}"))?;
    let mut command = Command::new("/bin/zsh");
    command
        .arg(wrapper_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    command
        .spawn()
        .with_context(|| format!("failed to start queue job {}", job.id))
}

fn monitor_queue_job_completion(
    state_dir: PathBuf,
    message_queue_db_path: PathBuf,
    job_id: String,
    mut child: Child,
    timeout_seconds: i64,
    cancel_grace_seconds: u64,
    admission_policy: QueueAdmissionPolicy,
) {
    let started = Instant::now();
    let timeout = (timeout_seconds > 0).then(|| StdDuration::from_secs(timeout_seconds as u64));
    let pgid = i64::from(child.id());
    loop {
        let child_status = match child.try_wait() {
            Ok(status) => status,
            Err(_) => {
                terminate_process_group_with_grace(pgid, cancel_grace_seconds);
                let _ = finish_queue_job_in_state_dir_if_running(
                    &state_dir,
                    &message_queue_db_path,
                    &job_id,
                    "failed",
                    None,
                    cancel_grace_seconds,
                    admission_policy,
                );
                return;
            }
        };
        if queue_job_is_cancelled_in_state_dir(&state_dir, &job_id) {
            return;
        }
        if let Some(status) = child_status {
            if !process_group_exists(pgid) {
                let exit_code = status.code().map(i64::from);
                let state = if exit_code == Some(0) {
                    "succeeded"
                } else {
                    "failed"
                };
                let _ = finish_queue_job_in_state_dir_if_running(
                    &state_dir,
                    &message_queue_db_path,
                    &job_id,
                    state,
                    exit_code,
                    cancel_grace_seconds,
                    admission_policy,
                );
                return;
            }
        }
        if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
            terminate_child_process_group_with_grace(&mut child, pgid, cancel_grace_seconds);
            let exit_code = read_queue_job_exit_code_from_state_dir(&state_dir, &job_id);
            let _ = finish_queue_job_in_state_dir_if_running(
                &state_dir,
                &message_queue_db_path,
                &job_id,
                "timed_out",
                exit_code,
                cancel_grace_seconds,
                admission_policy,
            );
            return;
        }
        thread::sleep(StdDuration::from_millis(100));
    }
}

fn recover_running_queue_job_conn(
    conn: &Connection,
    state_dir: &Path,
    message_queue_db_path: &Path,
    job: &QueueJobRuntimeRecord,
    cancel_grace_seconds: u64,
    admission_policy: QueueAdmissionPolicy,
) -> Result<RecoveredQueueJobAction> {
    if let Some(final_state) =
        forced_terminal_state_for_holding_reason(job.holding_reason.as_deref())
    {
        if let Some(pgid) = job.process_group_id.or(job.pid) {
            terminate_process_group_with_grace(pgid, cancel_grace_seconds);
        }
        let exit_code = read_exit_code(job.exit_code_path.as_deref());
        finish_queue_job_conn(
            conn,
            job,
            final_state,
            exit_code,
            Some(message_queue_db_path),
        )?;
        return Ok(RecoveredQueueJobAction::Finished(final_state));
    }
    if queue_job_exit_code_path_exists(job) {
        let exit_code = read_exit_code(job.exit_code_path.as_deref());
        let state = if exit_code == Some(0) {
            "succeeded"
        } else {
            "failed"
        };
        finish_queue_job_conn(conn, job, state, exit_code, Some(message_queue_db_path))?;
        return Ok(RecoveredQueueJobAction::Finished(state));
    }
    if queue_job_timed_out(job) {
        if let Some(pgid) = job.process_group_id.or(job.pid) {
            terminate_process_group_with_grace(pgid, cancel_grace_seconds);
        }
        let exit_code = read_exit_code(job.exit_code_path.as_deref());
        finish_queue_job_conn(
            conn,
            job,
            "timed_out",
            exit_code,
            Some(message_queue_db_path),
        )?;
        return Ok(RecoveredQueueJobAction::Finished("timed_out"));
    }
    let Some(pid) = job.pid else {
        finish_queue_job_conn(conn, job, "failed", None, Some(message_queue_db_path))?;
        return Ok(RecoveredQueueJobAction::Finished("failed"));
    };
    if !process_exists(pid) {
        finish_queue_job_conn(conn, job, "failed", None, Some(message_queue_db_path))?;
        return Ok(RecoveredQueueJobAction::Finished("failed"));
    }

    let state_dir = state_dir.to_path_buf();
    let message_queue_db_path = message_queue_db_path.to_path_buf();
    let job_id = job.id.clone();
    thread::spawn(move || {
        poll_recovered_queue_job(
            state_dir,
            message_queue_db_path,
            job_id,
            pid,
            cancel_grace_seconds,
            admission_policy,
        );
    });
    Ok(RecoveredQueueJobAction::Polling)
}

fn poll_recovered_queue_job(
    state_dir: PathBuf,
    message_queue_db_path: PathBuf,
    job_id: String,
    pid: i64,
    cancel_grace_seconds: u64,
    admission_policy: QueueAdmissionPolicy,
) {
    loop {
        thread::sleep(StdDuration::from_millis(100));
        let db_path = state_dir.join("queue_runner.db");
        let Ok(conn) = open_queue_jobs_connection(&db_path) else {
            return;
        };
        let _ = init_queue_jobs_schema(&conn);
        let Ok(Some(job)) = get_queue_job_runtime_conn(&conn, &job_id) else {
            return;
        };
        if job.state != "running" {
            return;
        }
        if let Some(final_state) =
            forced_terminal_state_for_holding_reason(job.holding_reason.as_deref())
        {
            if let Some(pgid) = job.process_group_id.or(job.pid) {
                terminate_process_group_with_grace(pgid, cancel_grace_seconds);
            }
            let exit_code = read_exit_code(job.exit_code_path.as_deref());
            let _ = finish_queue_job_conn(
                &conn,
                &job,
                final_state,
                exit_code,
                Some(&message_queue_db_path),
            );
            let _ = admit_pending_queue_jobs_conn(
                &conn,
                &state_dir,
                &message_queue_db_path,
                cancel_grace_seconds,
                admission_policy,
                true,
            );
            return;
        }
        if queue_job_exit_code_path_exists(&job) {
            let exit_code = read_exit_code(job.exit_code_path.as_deref());
            let state = if exit_code == Some(0) {
                "succeeded"
            } else {
                "failed"
            };
            let _ =
                finish_queue_job_conn(&conn, &job, state, exit_code, Some(&message_queue_db_path));
            let _ = admit_pending_queue_jobs_conn(
                &conn,
                &state_dir,
                &message_queue_db_path,
                cancel_grace_seconds,
                admission_policy,
                true,
            );
            return;
        }
        if queue_job_timed_out(&job) {
            if let Some(pgid) = job.process_group_id.or(job.pid) {
                terminate_process_group_with_grace(pgid, cancel_grace_seconds);
            }
            let exit_code = read_exit_code(job.exit_code_path.as_deref());
            let _ = finish_queue_job_conn(
                &conn,
                &job,
                "timed_out",
                exit_code,
                Some(&message_queue_db_path),
            );
            let _ = admit_pending_queue_jobs_conn(
                &conn,
                &state_dir,
                &message_queue_db_path,
                cancel_grace_seconds,
                admission_policy,
                true,
            );
            return;
        }
        if !process_exists(pid) {
            let _ =
                finish_queue_job_conn(&conn, &job, "failed", None, Some(&message_queue_db_path));
            let _ = admit_pending_queue_jobs_conn(
                &conn,
                &state_dir,
                &message_queue_db_path,
                cancel_grace_seconds,
                admission_policy,
                true,
            );
            return;
        }
    }
}

fn queue_job_timed_out(job: &QueueJobRuntimeRecord) -> bool {
    queue_job_timed_out_at(job, OffsetDateTime::now_utc())
}

fn queue_job_timed_out_at(job: &QueueJobRuntimeRecord, now_utc: OffsetDateTime) -> bool {
    if job.timeout_seconds <= 0 {
        return false;
    }
    let Some(started_at) = job.started_at.as_deref() else {
        return false;
    };
    let Some(elapsed_seconds) = queue_elapsed_since(started_at, now_utc) else {
        return false;
    };
    elapsed_seconds >= job.timeout_seconds
}

fn queue_elapsed_since(value: &str, now_utc: OffsetDateTime) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(parsed) = OffsetDateTime::parse(value, &Rfc3339) {
        return Some((now_utc - parsed).whole_seconds());
    }
    let parsed = parse_python_naive_datetime(value)?;
    Some((local_now_naive(now_utc)? - parsed).whole_seconds())
}

fn queue_job_is_cancelled_in_state_dir(state_dir: &Path, job_id: &str) -> bool {
    let db_path = state_dir.join("queue_runner.db");
    let Ok(conn) = open_queue_jobs_connection(&db_path) else {
        return false;
    };
    let Ok(Some(job)) = get_queue_job_runtime_conn(&conn, job_id) else {
        return false;
    };
    job.state == "cancelled"
}

fn mark_queue_job_cancelling_conn(conn: &Connection, job_id: &str) -> Result<()> {
    conn.execute(
        r#"
        UPDATE queue_jobs
        SET holding_reason = 'cancelling'
        WHERE id = ?1 AND state = 'running'
        "#,
        params![job_id],
    )?;
    Ok(())
}

fn mark_queue_job_displacing_conn(conn: &Connection, job_id: &str) -> Result<()> {
    conn.execute(
        r#"
        UPDATE queue_jobs
        SET holding_reason = 'displacing'
        WHERE id = ?1 AND state = 'running'
        "#,
        params![job_id],
    )?;
    Ok(())
}

fn forced_terminal_state_for_holding_reason(holding_reason: Option<&str>) -> Option<&'static str> {
    match holding_reason {
        Some("cancelling") => Some("cancelled"),
        Some("displacing") => Some("displaced"),
        _ => None,
    }
}

fn finish_queue_job_in_state_dir_if_running(
    state_dir: &Path,
    message_queue_db_path: &Path,
    job_id: &str,
    state: &str,
    exit_code: Option<i64>,
    cancel_grace_seconds: u64,
    admission_policy: QueueAdmissionPolicy,
) -> Result<()> {
    let db_path = state_dir.join("queue_runner.db");
    let conn = open_queue_jobs_connection(&db_path)?;
    init_queue_jobs_schema(&conn)?;
    let Some(job) = get_queue_job_runtime_conn(&conn, job_id)? else {
        return Ok(());
    };
    if job.state != "running" {
        return Ok(());
    }
    let final_state =
        forced_terminal_state_for_holding_reason(job.holding_reason.as_deref()).unwrap_or(state);
    finish_queue_job_conn(
        &conn,
        &job,
        final_state,
        exit_code,
        Some(message_queue_db_path),
    )?;
    let _ = admit_pending_queue_jobs_conn(
        &conn,
        state_dir,
        message_queue_db_path,
        cancel_grace_seconds,
        admission_policy,
        true,
    );
    Ok(())
}

fn finish_queue_job_conn(
    conn: &Connection,
    job: &QueueJobRuntimeRecord,
    state: &str,
    exit_code: Option<i64>,
    message_queue_db_path: Option<&Path>,
) -> Result<()> {
    let finished_at = now_rfc3339();
    let changed = conn.execute(
        r#"
        UPDATE queue_jobs
        SET state = ?2,
            holding_reason = NULL,
            finished_at = ?3,
            exit_code = ?4,
            completion_notification_required = 1
        WHERE id = ?1 AND state NOT IN ('succeeded', 'failed', 'timed_out', 'cancelled', 'displaced')
        "#,
        params![job.id, state, finished_at, exit_code],
    )?;
    if changed == 0 {
        return Ok(());
    }
    record_queue_job_completion_notification_conn(
        conn,
        job,
        state,
        exit_code,
        &finished_at,
        message_queue_db_path,
    )?;
    Ok(())
}

fn retry_unnotified_queue_job_completions_conn(
    conn: &Connection,
    message_queue_db_path: &Path,
) -> Result<usize> {
    let mut statement = conn.prepare(
        r#"
        SELECT id
        FROM queue_jobs
        WHERE state IN ('succeeded', 'failed', 'timed_out', 'cancelled', 'displaced')
          AND completion_notification_required = 1
          AND completion_notified_at IS NULL
        ORDER BY finished_at, id
        "#,
    )?;
    let job_ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);

    let mut notified = 0;
    for job_id in job_ids {
        let Some(job) = get_queue_job_runtime_conn(conn, &job_id)? else {
            continue;
        };
        let finished_at = job.finished_at.as_deref().unwrap_or(&job.queued_at);
        if record_queue_job_completion_notification_conn(
            conn,
            &job,
            &job.state,
            job.exit_code,
            finished_at,
            Some(message_queue_db_path),
        )? {
            notified += 1;
        }
    }
    Ok(notified)
}

fn record_queue_job_completion_notification_conn(
    conn: &Connection,
    job: &QueueJobRuntimeRecord,
    state: &str,
    exit_code: Option<i64>,
    finished_at: &str,
    message_queue_db_path: Option<&Path>,
) -> Result<bool> {
    let Some(completion_notified_at) = queue_job_completion_notified_at(
        job,
        state,
        exit_code,
        finished_at,
        message_queue_db_path,
    )?
    else {
        return Ok(false);
    };
    let changed = conn.execute(
        r#"
        UPDATE queue_jobs
        SET completion_notified_at = COALESCE(completion_notified_at, ?2)
        WHERE id = ?1
        "#,
        params![job.id, completion_notified_at],
    )?;
    Ok(changed == 1)
}

fn queue_job_completion_notified_at(
    job: &QueueJobRuntimeRecord,
    state: &str,
    exit_code: Option<i64>,
    finished_at: &str,
    message_queue_db_path: Option<&Path>,
) -> Result<Option<String>> {
    if job.completion_notified_at.is_some() {
        return Ok(None);
    }
    let Some(target_session_id) = job
        .notify_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(Some(now_rfc3339()));
    };
    let Some(message_queue_db_path) = message_queue_db_path else {
        return Ok(None);
    };
    let text = queue_job_completion_text(job, state, exit_code, finished_at);
    let queue = RetainedQueueStore::new(message_queue_db_path.to_path_buf());
    queue.enqueue_message_once_with_metadata(
        &format!("queue-completion-{}", job.id),
        target_session_id,
        &text,
        "sequential",
        QueueMessageMetadata {
            message_category: Some("queue-completion".to_owned()),
            ..QueueMessageMetadata::default()
        },
    )?;
    Ok(Some(now_rfc3339()))
}

fn queue_job_completion_text(
    job: &QueueJobRuntimeRecord,
    state: &str,
    exit_code: Option<i64>,
    finished_at: &str,
) -> String {
    let runtime = queue_duration_text(job.started_at.as_deref(), Some(finished_at));
    let queue_end = job.started_at.as_deref().unwrap_or(finished_at);
    let queued = queue_duration_text(Some(&job.queued_at), Some(queue_end));
    let termination_text = match state {
        "timed_out" => " termination=timeout",
        "cancelled" => " termination=cancelled",
        "displaced" => " termination=perf_displacement",
        _ => "",
    };
    let exit_text = exit_code.map_or_else(
        || " exit=unknown (no exit receipt; output is partial/non-evidence)".to_owned(),
        |code| format!(" exit={code}"),
    );
    format!(
        "[sm queue] {} completed: {}{}{} runtime={} queue={}. Log: {}",
        job.id,
        state,
        termination_text,
        exit_text,
        runtime,
        queued,
        job.log_path.as_deref().unwrap_or("-")
    )
}

fn queue_duration_text(start: Option<&str>, end: Option<&str>) -> String {
    let Some(start) = start.and_then(parse_queue_datetime) else {
        return "-".to_owned();
    };
    let Some(end) = end.and_then(parse_queue_datetime) else {
        return "-".to_owned();
    };
    format!("{}s", (end - start).whole_seconds().max(0))
}

fn parse_queue_datetime(value: &str) -> Option<OffsetDateTime> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .or_else(|| parse_python_naive_datetime(value).map(PrimitiveDateTime::assume_utc))
}

fn read_queue_job_exit_code_from_state_dir(state_dir: &Path, job_id: &str) -> Option<i64> {
    let db_path = state_dir.join("queue_runner.db");
    let conn = open_queue_jobs_connection(&db_path).ok()?;
    init_queue_jobs_schema(&conn).ok()?;
    let exit_code_path = get_queue_job_runtime_conn(&conn, job_id)
        .ok()
        .flatten()
        .and_then(|job| job.exit_code_path)?;
    read_exit_code(Some(&exit_code_path))
}

fn queue_job_exit_code_path_exists(job: &QueueJobRuntimeRecord) -> bool {
    job.exit_code_path
        .as_deref()
        .is_some_and(|path| Path::new(path).exists())
}

fn read_exit_code(path: Option<&str>) -> Option<i64> {
    let path = path?;
    fs::read_to_string(path).ok()?.trim().parse::<i64>().ok()
}

fn terminate_process_group(pgid: i64, force: bool) {
    if pgid <= 0 {
        return;
    }
    let signal = if force { "-KILL" } else { "-TERM" };
    let _ = Command::new("/bin/kill")
        .arg(signal)
        .arg(format!("-{pgid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn terminate_process_group_with_grace(pgid: i64, grace_seconds: u64) {
    terminate_process_group(pgid, false);
    let deadline = Instant::now() + StdDuration::from_secs(grace_seconds);
    while process_group_exists(pgid) {
        if Instant::now() >= deadline {
            terminate_process_group(pgid, true);
            break;
        }
        thread::sleep(StdDuration::from_millis(100));
    }
    while process_group_exists(pgid) {
        thread::sleep(StdDuration::from_millis(100));
    }
}

fn terminate_child_process_group_with_grace(child: &mut Child, pgid: i64, grace_seconds: u64) {
    terminate_process_group(pgid, false);
    let deadline = Instant::now() + StdDuration::from_secs(grace_seconds);
    let mut force_sent = false;
    loop {
        let _ = child.try_wait();
        if !process_group_exists(pgid) {
            return;
        }
        if !force_sent && Instant::now() >= deadline {
            terminate_process_group(pgid, true);
            force_sent = true;
        }
        thread::sleep(StdDuration::from_millis(100));
    }
}

fn process_group_exists(pgid: i64) -> bool {
    if pgid <= 0 {
        return false;
    }
    Command::new("/bin/kill")
        .arg("-0")
        .arg(format!("-{pgid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn process_exists(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn is_terminal_queue_state(state: &str) -> bool {
    matches!(
        state,
        "succeeded" | "failed" | "timed_out" | "cancelled" | "displaced"
    )
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
               requested_head_sha, superseded_by_request_id, superseded_at,
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
    let rows = statement.query_map(
        rusqlite::params_from_iter(values),
        codex_review_request_registration_from_row,
    )?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn active_codex_review_request_exists_conn(
    conn: &Connection,
    repo: &str,
    pr_number: i64,
    notify_session_id: &str,
) -> Result<bool> {
    let count = match conn.query_row(
        r#"
        SELECT COUNT(*)
        FROM codex_review_request_registrations
        WHERE repo = ?1
            AND pr_number = ?2
            AND notify_session_id = ?3
            AND is_active = 1
        "#,
        params![repo, pr_number, notify_session_id],
        |row| row.get::<_, i64>(0),
    ) {
        Ok(count) => count,
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.contains("no such table") =>
        {
            0
        }
        Err(error) => return Err(error.into()),
    };
    Ok(count > 0)
}

fn create_codex_review_request_conn(
    conn: &Connection,
    request: CreateCodexReviewRequest,
) -> Result<CodexReviewRequestRegistration> {
    if request.poll_interval_seconds <= 0 {
        anyhow::bail!("poll_interval_seconds must be > 0");
    }
    if request.retry_interval_seconds <= 0 {
        anyhow::bail!("retry_interval_seconds must be > 0");
    }
    let owner = request
        .requester_session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&request.notify_session_id)
        .to_owned();
    let active = list_codex_review_requests_conn(
        conn,
        CodexReviewRequestFilters {
            repo: Some(request.repo.clone()),
            pr_number: Some(request.pr_number),
            include_inactive: false,
            ..CodexReviewRequestFilters::default()
        },
    )?;
    for existing in &active {
        let existing_owner = existing
            .requester_session_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&existing.notify_session_id);
        if existing_owner != owner {
            anyhow::bail!(
                "CONFLICT: active Codex review request {} for {} PR #{} is owned by {}; cancel it or wait for completion",
                existing.id,
                request.repo,
                request.pr_number,
                existing_owner
            );
        }
        if existing.requested_head_sha.as_deref() == Some(request.requested_head_sha.as_str()) {
            return Ok(existing.clone());
        }
    }

    let latest_posted_at = request.latest_request_posted_at;
    let next_retry_at =
        codex_review_next_retry_at(&latest_posted_at, request.retry_interval_seconds)?;
    let registration = CodexReviewRequestRegistration {
        id: generate_codex_review_request_id(),
        repo: request.repo,
        pr_number: request.pr_number,
        requester_session_id: request.requester_session_id,
        notify_session_id: request.notify_session_id,
        steer: request.steer,
        requested_head_sha: Some(request.requested_head_sha),
        superseded_by_request_id: None,
        superseded_at: None,
        requested_at: latest_posted_at.clone(),
        latest_request_comment_id: request.latest_request_comment_id,
        latest_request_comment_url: request.latest_request_comment_url,
        latest_request_posted_at: Some(latest_posted_at),
        attempt_count: 1,
        next_retry_at: Some(next_retry_at),
        poll_interval_seconds: request.poll_interval_seconds,
        retry_interval_seconds: request.retry_interval_seconds,
        pickup_detected_at: None,
        pickup_source: None,
        review_landed_at: None,
        review_source: None,
        review_comment_id: None,
        review_url: None,
        last_polled_at: None,
        last_error: None,
        state: "active".to_owned(),
        is_active: true,
    };
    conn.execute(
        r#"
        UPDATE codex_review_request_registrations
        SET is_active = 0,
            state = 'superseded',
            superseded_by_request_id = ?1,
            superseded_at = ?2,
            last_error = 'Superseded by a newer requested PR head'
        WHERE repo = ?3
            AND pr_number = ?4
            AND is_active = 1
            AND COALESCE(NULLIF(requester_session_id, ''), notify_session_id) = ?5
        "#,
        params![
            registration.id,
            registration.requested_at,
            registration.repo,
            registration.pr_number,
            owner,
        ],
    )?;
    conn.execute(
        r#"
        INSERT INTO codex_review_request_registrations
            (id, repo, pr_number, requester_session_id, notify_session_id, steer,
             requested_head_sha, superseded_by_request_id, superseded_at,
             requested_at, latest_request_comment_id, latest_request_comment_url,
             latest_request_posted_at, attempt_count, next_retry_at,
             poll_interval_seconds, retry_interval_seconds, pickup_detected_at,
             pickup_source, review_landed_at, review_source, review_comment_id,
             review_url, last_polled_at, last_error, state, is_active)
        VALUES
            (?1, ?2, ?3, ?4, ?5, ?6,
             ?7, NULL, NULL,
             ?8, ?9, ?10,
             ?11, ?12, ?13,
             ?14, ?15, NULL,
             NULL, NULL, NULL, NULL,
             NULL, NULL, NULL, ?16, 1)
        "#,
        params![
            registration.id,
            registration.repo,
            registration.pr_number,
            registration.requester_session_id,
            registration.notify_session_id,
            registration.steer,
            registration.requested_head_sha,
            registration.requested_at,
            registration.latest_request_comment_id,
            registration.latest_request_comment_url,
            registration.latest_request_posted_at,
            registration.attempt_count,
            registration.next_retry_at,
            registration.poll_interval_seconds,
            registration.retry_interval_seconds,
            registration.state,
        ],
    )?;
    Ok(registration)
}

fn get_codex_review_request_conn(
    conn: &Connection,
    request_id: &str,
) -> Result<Option<CodexReviewRequestRegistration>> {
    let mut statement = match conn.prepare(
        r#"
        SELECT id, repo, pr_number, requester_session_id, notify_session_id, steer,
               requested_head_sha, superseded_by_request_id, superseded_at,
               requested_at, latest_request_comment_id, latest_request_comment_url,
               latest_request_posted_at, attempt_count, next_retry_at,
               poll_interval_seconds, retry_interval_seconds, pickup_detected_at,
               pickup_source, review_landed_at, review_source, review_comment_id,
               review_url, last_polled_at, last_error, state, is_active
        FROM codex_review_request_registrations
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
        .query_row(
            params![request_id],
            codex_review_request_registration_from_row,
        )
        .optional()
        .map_err(Into::into)
}

fn codex_review_request_registration_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<CodexReviewRequestRegistration> {
    Ok(CodexReviewRequestRegistration {
        id: row.get(0)?,
        repo: row.get(1)?,
        pr_number: row.get(2)?,
        requester_session_id: row.get(3)?,
        notify_session_id: row.get(4)?,
        steer: row.get(5)?,
        requested_head_sha: row.get(6)?,
        superseded_by_request_id: row.get(7)?,
        superseded_at: row.get(8)?,
        requested_at: row.get(9)?,
        latest_request_comment_id: row.get(10)?,
        latest_request_comment_url: row.get(11)?,
        latest_request_posted_at: row.get(12)?,
        attempt_count: row.get(13)?,
        next_retry_at: row.get(14)?,
        poll_interval_seconds: row.get(15)?,
        retry_interval_seconds: row.get(16)?,
        pickup_detected_at: row.get(17)?,
        pickup_source: row.get(18)?,
        review_landed_at: row.get(19)?,
        review_source: row.get(20)?,
        review_comment_id: optional_sqlite_json_scalar(row.get_ref(21)?),
        review_url: row.get(22)?,
        last_polled_at: row.get(23)?,
        last_error: row.get(24)?,
        state: row.get(25)?,
        is_active: row.get::<_, Option<i64>>(26)?.unwrap_or(1) != 0,
    })
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

fn json_scalar_to_sql_value(value: JsonValue) -> SqlValue {
    match value {
        JsonValue::Null => SqlValue::Null,
        JsonValue::Bool(value) => SqlValue::Integer(i64::from(value)),
        JsonValue::Number(value) => {
            if let Some(value) = value.as_i64() {
                SqlValue::Integer(value)
            } else if let Some(value) = value.as_u64().and_then(|value| i64::try_from(value).ok()) {
                SqlValue::Integer(value)
            } else if let Some(value) = value.as_f64() {
                SqlValue::Real(value)
            } else {
                SqlValue::Text(value.to_string())
            }
        }
        JsonValue::String(value) => SqlValue::Text(value),
        JsonValue::Array(_) | JsonValue::Object(_) => SqlValue::Text(value.to_string()),
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
    now_local: Option<PrimitiveDateTime>,
) -> bool {
    let timeout_at = timeout_at.trim();
    if timeout_at.is_empty() {
        return false;
    }
    if let Ok(parsed) = OffsetDateTime::parse(timeout_at, &Rfc3339) {
        return parsed <= now_utc;
    }
    if let Some(parsed) = parse_python_naive_datetime(timeout_at) {
        return now_local.is_some_and(|now_local| parsed <= now_local);
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

fn local_now_naive(now_utc: OffsetDateTime) -> Option<PrimitiveDateTime> {
    #[cfg(unix)]
    {
        let timestamp = nix::libc::time_t::try_from(now_utc.unix_timestamp()).ok()?;
        let mut local = std::mem::MaybeUninit::<nix::libc::tm>::uninit();
        // localtime_r writes only to caller-owned storage and is safe across server threads.
        let result = unsafe { nix::libc::localtime_r(&timestamp, local.as_mut_ptr()) };
        if result.is_null() {
            return None;
        }
        let local = unsafe { local.assume_init() };
        let year = local.tm_year.checked_add(1900)?;
        let ordinal = u16::try_from(local.tm_yday.checked_add(1)?).ok()?;
        let date = time::Date::from_ordinal_date(year, ordinal).ok()?;
        let hour = u8::try_from(local.tm_hour).ok()?;
        let minute = u8::try_from(local.tm_min).ok()?;
        let second = u8::try_from(local.tm_sec.min(59)).ok()?;
        let time = time::Time::from_hms_nano(hour, minute, second, now_utc.nanosecond()).ok()?;
        Some(PrimitiveDateTime::new(date, time))
    }
    #[cfg(not(unix))]
    {
        let offset = time::UtcOffset::local_offset_at(now_utc).ok()?;
        let local = now_utc.to_offset(offset);
        Some(PrimitiveDateTime::new(local.date(), local.time()))
    }
}

fn python_compatible_reminder_timestamp(value: PrimitiveDateTime) -> Result<String> {
    Ok(value.format(format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]"
    ))?)
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

fn generate_queue_job_id() -> String {
    let mut bytes = [0u8; 6];
    OsRng.fill_bytes(&mut bytes);
    let suffix = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("job_{suffix}")
}

fn generate_codex_review_request_id() -> String {
    let mut bytes = [0u8; 6];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn generate_scheduled_reminder_id() -> String {
    let mut bytes = [0u8; 6];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn scheduled_reminder_conn(
    conn: &Connection,
    reminder_id: &str,
) -> Result<Option<ScheduledReminder>> {
    conn.query_row(
        r#"
        SELECT id, target_session_id, message, fire_at,
               recurring_interval_seconds, fired, is_active
        FROM scheduled_reminders
        WHERE id = ?1
        "#,
        params![reminder_id],
        scheduled_reminder_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn scheduled_reminder_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduledReminder> {
    Ok(ScheduledReminder {
        id: row.get(0)?,
        target_session_id: row.get(1)?,
        message: row.get(2)?,
        fire_at: row.get(3)?,
        recurring_interval_seconds: row.get(4)?,
        fired: row.get::<_, Option<i64>>(5)?.unwrap_or(0) != 0,
        is_active: row.get::<_, Option<i64>>(6)?.unwrap_or(1) != 0,
    })
}

fn scheduled_reminder_is_due(fire_at: &str, now_utc: OffsetDateTime) -> bool {
    scheduled_reminder_elapsed_seconds(fire_at, now_utc).is_some_and(|elapsed| elapsed >= 0)
}

fn scheduled_reminder_elapsed_seconds(fire_at: &str, now_utc: OffsetDateTime) -> Option<i64> {
    if let Ok(parsed) = OffsetDateTime::parse(fire_at.trim(), &Rfc3339) {
        return Some((now_utc - parsed).whole_seconds());
    }
    parse_python_naive_datetime(fire_at.trim())
        .and_then(|parsed| local_now_naive(now_utc).map(|now| (now - parsed).whole_seconds()))
}

fn scheduled_reminder_message(reminder: &ScheduledReminder) -> String {
    if reminder.recurring_interval_seconds.is_some() {
        format!(
            "[sm] Recurring reminder: ({})\n{}\n[sm] Cancel: sm remind cancel {}",
            reminder.id, reminder.message, reminder.id
        )
    } else {
        format!(
            "[sm] Scheduled reminder: ({})\n{}",
            reminder.id, reminder.message
        )
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn codex_review_next_retry_at(posted_at: &str, retry_interval_seconds: i64) -> Result<String> {
    let posted_at = parse_queue_datetime(posted_at)
        .or_else(|| parse_python_naive_datetime(posted_at).map(PrimitiveDateTime::assume_utc))
        .unwrap_or_else(OffsetDateTime::now_utc);
    let next_retry_at = posted_at + Duration::seconds(retry_interval_seconds.max(1));
    Ok(next_retry_at.format(&Rfc3339)?)
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
    fn queue_completion_without_exit_receipt_is_explicitly_non_evidence() {
        let job = QueueJobRuntimeRecord {
            id: "job_missing_exit".to_owned(),
            job_type: "tests".to_owned(),
            state: "running".to_owned(),
            notify_session_id: Some("run12345".to_owned()),
            queued_at: "2026-08-16T20:00:00Z".to_owned(),
            started_at: Some("2026-08-16T20:00:01Z".to_owned()),
            finished_at: None,
            holding_reason: None,
            wrapper_path: None,
            log_path: Some("/tmp/job_missing_exit.log".to_owned()),
            exit_code_path: None,
            timeout_seconds: 900,
            pid: None,
            process_group_id: None,
            exit_code: None,
            completion_notified_at: None,
        };

        let failed = queue_job_completion_text(&job, "failed", None, "2026-08-16T20:04:24Z");
        assert!(failed.contains("completed: failed exit=unknown"));
        assert!(failed.contains("output is partial/non-evidence"));

        let displaced = queue_job_completion_text(&job, "displaced", None, "2026-08-16T20:04:24Z");
        assert!(displaced.contains("termination=perf_displacement"));
        assert!(displaced.contains("exit=unknown"));
    }

    #[test]
    fn queue_completion_references_log_without_embedding_output() {
        let log_path = unique_temp_path("completion-log");
        fs::write(&log_path, "long test output that belongs only in the log\n").unwrap();
        let job = QueueJobRuntimeRecord {
            id: "job_completion_log".to_owned(),
            job_type: "tests".to_owned(),
            state: "running".to_owned(),
            notify_session_id: Some("run12345".to_owned()),
            queued_at: "2026-08-16T20:00:00Z".to_owned(),
            started_at: Some("2026-08-16T20:00:01Z".to_owned()),
            finished_at: None,
            holding_reason: None,
            wrapper_path: None,
            log_path: Some(log_path.display().to_string()),
            exit_code_path: None,
            timeout_seconds: 900,
            pid: None,
            process_group_id: None,
            exit_code: None,
            completion_notified_at: None,
        };

        let completion = queue_job_completion_text(&job, "failed", Some(1), "2026-08-16T20:04:24Z");

        assert!(completion.contains(&format!("Log: {}", log_path.display())));
        assert!(!completion.contains("long test output that belongs only in the log"));
        assert!(!completion.contains("log tail:"));
        let _ = fs::remove_file(log_path);
    }

    #[test]
    fn queue_argv_wrapper_reports_effective_path_when_an_executable_is_missing() {
        let job_dir = unique_temp_path("missing-executable");
        fs::create_dir_all(&job_dir).unwrap();
        let wrapper_path = job_dir.join("run.zsh");
        let exit_code_path = job_dir.join("exit.code");
        let log_path = job_dir.join("job.log");
        let path = "/queue-test/bin";

        write_queue_job_wrapper(
            &wrapper_path,
            "/tmp",
            Some(&["sm-command-that-does-not-exist".to_owned()]),
            None,
            &BTreeMap::from([("PATH".to_owned(), path.to_owned())]),
            &exit_code_path,
        )
        .unwrap();
        let job = QueueJobRuntimeRecord {
            id: "job-missing-executable".to_owned(),
            job_type: "tests".to_owned(),
            state: "pending".to_owned(),
            notify_session_id: None,
            queued_at: now_rfc3339(),
            started_at: None,
            finished_at: None,
            holding_reason: None,
            wrapper_path: Some(wrapper_path.display().to_string()),
            log_path: Some(log_path.display().to_string()),
            exit_code_path: Some(exit_code_path.display().to_string()),
            timeout_seconds: 60,
            pid: None,
            process_group_id: None,
            exit_code: None,
            completion_notified_at: None,
        };

        let status = spawn_queue_job_process(&job).unwrap().wait().unwrap();
        let log = fs::read_to_string(&log_path).unwrap();
        assert_eq!(status.code(), Some(127));
        assert!(log.contains("command not found: sm-command-that-does-not-exist"));
        assert!(log.contains("[sm queue] effective PATH: /queue-test/bin"));
        assert_eq!(fs::read_to_string(exit_code_path).unwrap().trim(), "127");

        fs::remove_dir_all(job_dir).unwrap();
    }

    #[test]
    fn queue_script_wrapper_reports_effective_path_when_an_executable_is_missing() {
        let job_dir = unique_temp_path("missing-script-executable");
        fs::create_dir_all(&job_dir).unwrap();
        let script_path = job_dir.join("submitted.zsh");
        fs::write(&script_path, "sm-script-command-that-does-not-exist\n").unwrap();
        let wrapper_path = job_dir.join("run.zsh");
        let exit_code_path = job_dir.join("exit.code");
        let log_path = job_dir.join("job.log");
        let path = "/queue-test/script-bin";

        write_queue_job_wrapper(
            &wrapper_path,
            "/tmp",
            None,
            Some(script_path.to_str().unwrap()),
            &BTreeMap::from([("PATH".to_owned(), path.to_owned())]),
            &exit_code_path,
        )
        .unwrap();
        let wrapper = fs::read_to_string(&wrapper_path).unwrap();
        assert!(wrapper.contains(&format!(
            "/bin/zsh {}",
            shell_quote(script_path.to_str().unwrap())
        )));
        assert!(!wrapper.contains("source \"$1\""));
        let job = QueueJobRuntimeRecord {
            id: "job-missing-script-executable".to_owned(),
            job_type: "tests".to_owned(),
            state: "pending".to_owned(),
            notify_session_id: None,
            queued_at: now_rfc3339(),
            started_at: None,
            finished_at: None,
            holding_reason: None,
            wrapper_path: Some(wrapper_path.display().to_string()),
            log_path: Some(log_path.display().to_string()),
            exit_code_path: Some(exit_code_path.display().to_string()),
            timeout_seconds: 60,
            pid: None,
            process_group_id: None,
            exit_code: None,
            completion_notified_at: None,
        };

        let status = spawn_queue_job_process(&job).unwrap().wait().unwrap();
        let log = fs::read_to_string(&log_path).unwrap();
        assert_eq!(status.code(), Some(127));
        assert!(log.contains("command not found: sm-script-command-that-does-not-exist"));
        assert!(log.contains("[sm queue] effective PATH: /queue-test/script-bin"));
        assert_eq!(fs::read_to_string(exit_code_path).unwrap().trim(), "127");

        fs::remove_dir_all(job_dir).unwrap();
    }

    #[test]
    fn zero_timeout_never_expires() {
        let mut job = QueueJobRuntimeRecord {
            id: "job_unbounded".to_owned(),
            job_type: "background".to_owned(),
            state: "running".to_owned(),
            notify_session_id: None,
            queued_at: "2026-08-16T20:00:00Z".to_owned(),
            started_at: Some("2026-08-16T20:00:01Z".to_owned()),
            finished_at: None,
            holding_reason: None,
            wrapper_path: None,
            log_path: None,
            exit_code_path: None,
            timeout_seconds: 0,
            pid: None,
            process_group_id: None,
            exit_code: None,
            completion_notified_at: None,
        };
        let much_later = OffsetDateTime::parse("2026-08-17T20:00:01Z", &Rfc3339).unwrap();
        assert!(!queue_job_timed_out_at(&job, much_later));

        job.timeout_seconds = 10;
        assert!(queue_job_timed_out_at(&job, much_later));
    }

    #[test]
    fn service_capacity_does_not_block_finite_background_admission() {
        let state_dir = unique_temp_path("service-capacity");
        let create_job = |job_type: &str, label: &str| {
            RetainedQueueStore::create_queue_job_in_state_dir(
                &state_dir,
                CreateQueueJob {
                    job_type: job_type.to_owned(),
                    label: label.to_owned(),
                    requester_session_id: Some("requester".to_owned()),
                    notify_session_id: "notify".to_owned(),
                    cwd: "/tmp".to_owned(),
                    argv: Some(vec!["true".to_owned()]),
                    script: None,
                    env: BTreeMap::new(),
                    timeout_seconds: 60,
                },
            )
            .unwrap()
        };
        let first_service = create_job("service", "first service");
        let second_service = create_job("service", "second service");
        let background = create_job("background", "finite background");
        let third_service = create_job("service", "third service");
        let conn = open_queue_jobs_connection(&state_dir.join("queue_runner.db")).unwrap();
        for job_id in [&first_service.id, &second_service.id] {
            conn.execute(
                "UPDATE queue_jobs SET state = 'running', started_at = ?2 WHERE id = ?1",
                params![job_id, now_rfc3339()],
            )
            .unwrap();
        }
        let policy = QueueAdmissionPolicy {
            max_running_jobs: 4,
            service_max_concurrent: 2,
            ..QueueAdmissionPolicy::default()
        };

        let jobs = list_queue_job_runtime_records_conn(&conn).unwrap();
        let mut summary = QueueAdmissionSummary::default();
        assert_eq!(
            next_admissible_queue_job_id_conn(&conn, &jobs, policy, &mut summary).unwrap(),
            Some(background.id.clone())
        );

        conn.execute(
            "UPDATE queue_jobs SET state = 'running', started_at = ?2 WHERE id = ?1",
            params![background.id, now_rfc3339()],
        )
        .unwrap();
        let jobs = list_queue_job_runtime_records_conn(&conn).unwrap();
        let mut summary = QueueAdmissionSummary::default();
        assert_eq!(
            next_admissible_queue_job_id_conn(&conn, &jobs, policy, &mut summary).unwrap(),
            None
        );
        let holding_reason: Option<String> = conn
            .query_row(
                "SELECT holding_reason FROM queue_jobs WHERE id = ?1",
                params![third_service.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(holding_reason.as_deref(), Some("concurrency_cap"));

        drop(conn);
        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn perf_displacement_never_selects_service_jobs() {
        let state_dir = unique_temp_path("service-perf-displacement");
        let create_job = |job_type: &str, label: &str| {
            RetainedQueueStore::create_queue_job_in_state_dir(
                &state_dir,
                CreateQueueJob {
                    job_type: job_type.to_owned(),
                    label: label.to_owned(),
                    requester_session_id: Some("requester".to_owned()),
                    notify_session_id: "notify".to_owned(),
                    cwd: "/tmp".to_owned(),
                    argv: Some(vec!["true".to_owned()]),
                    script: None,
                    env: BTreeMap::new(),
                    timeout_seconds: 60,
                },
            )
            .unwrap()
        };
        let service = create_job("service", "long-lived service");
        let perf = create_job("perf", "pending perf");
        let conn = open_queue_jobs_connection(&state_dir.join("queue_runner.db")).unwrap();
        conn.execute(
            "UPDATE queue_jobs SET state = 'running', started_at = ?2 WHERE id = ?1",
            params![service.id, now_rfc3339()],
        )
        .unwrap();
        let jobs = list_queue_job_runtime_records_conn(&conn).unwrap();

        assert!(!displace_background_for_perf_conn(
            &conn,
            &jobs,
            &state_dir.join("message_queue.db"),
            0,
            QueueAdmissionPolicy {
                max_running_jobs: 4,
                service_max_concurrent: 2,
                ..QueueAdmissionPolicy::default()
            },
        )
        .unwrap());
        let state: String = conn
            .query_row(
                "SELECT state FROM queue_jobs WHERE id = ?1",
                params![service.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "running");
        let perf_state: String = conn
            .query_row(
                "SELECT state FROM queue_jobs WHERE id = ?1",
                params![perf.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(perf_state, "pending");

        drop(conn);
        fs::remove_dir_all(state_dir).unwrap();
    }

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
    fn parent_routing_coordinator_quiesces_and_retargets_idempotently() {
        let db_path = unique_temp_path("parent-routing");
        let store = RetainedQueueStore::new(db_path.clone());
        store.ensure_schema().unwrap();
        let wake_id = store
            .register_parent_wake("child001", "parent-old", 600)
            .unwrap();
        let message_id = store
            .enqueue_message_with_metadata(
                "child001",
                "parent-derived wait",
                "important",
                QueueMessageMetadata {
                    parent_session_id: Some("parent-old".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();
        store
            .enqueue_message_with_metadata(
                "child001",
                "explicit peer route",
                "important",
                QueueMessageMetadata {
                    parent_session_id: Some("peer-explicit".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();

        let snapshot = store
            .snapshot_parent_routing("child001", "parent-old")
            .unwrap();
        assert_eq!(snapshot.wake_rows.len(), 1);
        assert_eq!(snapshot.wake_rows[0].id, wake_id);
        assert_eq!(snapshot.message_rows.len(), 1);
        assert_eq!(snapshot.message_rows[0].id, message_id);

        store.quiesce_parent_routing(&snapshot).unwrap();
        store.quiesce_parent_routing(&snapshot).unwrap();
        assert_eq!(store.active_parent_wake_parent("child001").unwrap(), None);
        let conn = Connection::open(&db_path).unwrap();
        let quiesced_parent: Option<String> = conn
            .query_row(
                "SELECT parent_session_id FROM message_queue WHERE id = ?1",
                params![message_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(quiesced_parent, None);

        store
            .retarget_parent_routing(&snapshot, Some("parent-new"))
            .unwrap();
        store
            .retarget_parent_routing(&snapshot, Some("parent-new"))
            .unwrap();
        assert_eq!(
            store.active_parent_wake_parent("child001").unwrap(),
            Some("parent-new".to_owned())
        );
        let parents = conn
            .prepare("SELECT text, parent_session_id FROM message_queue ORDER BY text")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            parents,
            vec![
                ("explicit peer route".to_owned(), "peer-explicit".to_owned()),
                ("parent-derived wait".to_owned(), "parent-new".to_owned()),
            ]
        );
    }

    #[test]
    fn parent_routing_coordinator_clears_routes_for_a_new_root_idempotently() {
        let db_path = unique_temp_path("parent-routing-root");
        let store = RetainedQueueStore::new(db_path.clone());
        store.ensure_schema().unwrap();
        store
            .register_parent_wake("child001", "parent-old", 600)
            .unwrap();
        let message_id = store
            .enqueue_message_with_metadata(
                "child001",
                "parent-derived wait",
                "important",
                QueueMessageMetadata {
                    parent_session_id: Some("parent-old".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();
        let snapshot = store
            .snapshot_parent_routing("child001", "parent-old")
            .unwrap();

        store.quiesce_parent_routing(&snapshot).unwrap();
        store.retarget_parent_routing(&snapshot, None).unwrap();
        store.retarget_parent_routing(&snapshot, None).unwrap();

        assert_eq!(store.active_parent_wake_parent("child001").unwrap(), None);
        let conn = Connection::open(&db_path).unwrap();
        let parent: Option<String> = conn
            .query_row(
                "SELECT parent_session_id FROM message_queue WHERE id = ?1",
                params![message_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parent, None);
    }

    #[test]
    fn parent_routing_coordinator_rolls_back_on_snapshot_mismatch() {
        let db_path = unique_temp_path("parent-routing-mismatch");
        let store = RetainedQueueStore::new(db_path.clone());
        store.ensure_schema().unwrap();
        store
            .register_parent_wake("child001", "parent-old", 600)
            .unwrap();
        let first_id = store
            .enqueue_message_with_metadata(
                "child001",
                "first",
                "important",
                QueueMessageMetadata {
                    parent_session_id: Some("parent-old".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();
        let second_id = store
            .enqueue_message_with_metadata(
                "child001",
                "second",
                "important",
                QueueMessageMetadata {
                    parent_session_id: Some("parent-old".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();
        let snapshot = store
            .snapshot_parent_routing("child001", "parent-old")
            .unwrap();
        Connection::open(&db_path)
            .unwrap()
            .execute(
                "UPDATE message_queue SET parent_session_id = 'unexpected' WHERE id = ?1",
                params![second_id],
            )
            .unwrap();

        assert!(store.quiesce_parent_routing(&snapshot).is_err());
        let conn = Connection::open(db_path).unwrap();
        let first_parent: String = conn
            .query_row(
                "SELECT parent_session_id FROM message_queue WHERE id = ?1",
                params![first_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_parent, "parent-old");
        assert_eq!(
            store.active_parent_wake_parent("child001").unwrap(),
            Some("parent-old".to_owned())
        );
    }

    #[test]
    fn parent_routing_retarget_preserves_messages_delivered_while_quiesced() {
        let db_path = unique_temp_path("parent-routing-delivery-race");
        let store = RetainedQueueStore::new(db_path);
        store.ensure_schema().unwrap();
        let message_id = store
            .enqueue_message_with_metadata(
                "child001",
                "delivered during reparent",
                "important",
                QueueMessageMetadata {
                    remind_soft_threshold: Some(600),
                    parent_session_id: Some("parent-old".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();
        let snapshot = store
            .snapshot_parent_routing("child001", "parent-old")
            .unwrap();
        assert!(snapshot.message_rows[0].creates_parent_wake);

        store.quiesce_parent_routing(&snapshot).unwrap();
        let message = store
            .pending_messages_for_target("child001", 10)
            .unwrap()
            .into_iter()
            .find(|message| message.id == message_id)
            .unwrap();
        assert!(message.parent_session_id.is_none());
        store
            .mark_delivered_and_apply_side_effects(&message)
            .unwrap();
        assert!(store
            .active_parent_wake_parent("child001")
            .unwrap()
            .is_none());
        let result = store
            .retarget_parent_routing(&snapshot, Some("parent-new"))
            .unwrap();

        assert_eq!(result.delivered_message_rows, snapshot.message_rows);
        assert!(store
            .pending_messages_for_target("child001", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn explicit_queue_message_id_is_idempotent_and_rejects_conflicts() {
        let db_path = unique_temp_path("queue-once");
        let store = RetainedQueueStore::new(db_path);
        let metadata = QueueMessageMetadata {
            message_category: Some("btw_response".to_owned()),
            ..QueueMessageMetadata::default()
        };
        store
            .enqueue_message_once_with_metadata(
                "btw-response-request-1",
                "requester",
                "summary",
                "sequential",
                metadata.clone(),
            )
            .unwrap();
        store
            .enqueue_message_once_with_metadata(
                "btw-response-request-1",
                "requester",
                "summary",
                "sequential",
                metadata.clone(),
            )
            .unwrap();
        assert!(store
            .enqueue_message_once_with_metadata(
                "btw-response-request-1",
                "requester",
                "different",
                "sequential",
                metadata,
            )
            .is_err());
        assert_eq!(
            store
                .pending_messages_for_target_by_category("requester", "btw_response", 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn pending_category_targets_are_distinct_and_ordered_by_oldest_message() {
        let db_path = unique_temp_path("pending-category-targets");
        let store = RetainedQueueStore::new(db_path);
        for (id, target, category) in [
            ("completion-1", "target-b", "queue-completion"),
            ("completion-2", "target-a", "queue-completion"),
            ("completion-3", "target-b", "queue-completion"),
            ("other-1", "target-c", "other"),
        ] {
            store
                .enqueue_message_once_with_metadata(
                    id,
                    target,
                    id,
                    "sequential",
                    QueueMessageMetadata {
                        message_category: Some(category.to_owned()),
                        ..QueueMessageMetadata::default()
                    },
                )
                .unwrap();
        }

        assert_eq!(
            store
                .pending_target_session_ids_by_category("queue-completion")
                .unwrap(),
            vec!["target-b", "target-a"]
        );
        assert_eq!(
            store
                .pending_target_session_ids_by_category("other")
                .unwrap(),
            vec!["target-c"]
        );
    }

    #[test]
    fn one_shot_reminder_claim_is_atomic_and_exactly_once() {
        let db_path = unique_temp_path("scheduled-reminder-once");
        let store = RetainedQueueStore::new(db_path.clone());
        let reminder = store
            .schedule_reminder("agent-1", 60, "Check the gate", None)
            .unwrap();
        let now = OffsetDateTime::now_utc();
        Connection::open(&db_path)
            .unwrap()
            .execute(
                "UPDATE scheduled_reminders SET fire_at = ?2 WHERE id = ?1",
                params![
                    reminder.id,
                    (now - Duration::seconds(1)).format(&Rfc3339).unwrap()
                ],
            )
            .unwrap();

        assert_eq!(store.due_scheduled_reminders(now).unwrap().len(), 1);
        let delivery = store
            .claim_due_scheduled_reminder(&reminder.id, now)
            .unwrap()
            .unwrap();
        assert_eq!(delivery.reminder.id, reminder.id);
        assert!(store
            .claim_due_scheduled_reminder(&reminder.id, now)
            .unwrap()
            .is_none());

        let pending = store.pending_messages_for_target("agent-1", 10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].delivery_mode, "urgent");
        assert_eq!(
            pending[0].message_category.as_deref(),
            Some("scheduled_reminder")
        );
        assert_eq!(
            pending[0].text,
            format!("[sm] Scheduled reminder: ({})\nCheck the gate", reminder.id)
        );
        let persisted = scheduled_reminder_conn(&Connection::open(&db_path).unwrap(), &reminder.id)
            .unwrap()
            .unwrap();
        assert!(persisted.fired);
        assert!(!persisted.is_active);
    }

    #[test]
    fn new_reminder_timestamps_remain_python_rollback_compatible() {
        let db_path = unique_temp_path("scheduled-reminder-python-time");
        let store = RetainedQueueStore::new(db_path);
        let reminder = store
            .schedule_reminder("agent-python", 60, "Rollback safely", None)
            .unwrap();

        assert!(parse_python_naive_datetime(&reminder.fire_at).is_some());
        assert!(OffsetDateTime::parse(&reminder.fire_at, &Rfc3339).is_err());
        assert!(!reminder.fire_at.ends_with('Z'));
    }

    #[test]
    fn local_naive_conversion_works_in_multiple_threads() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let expected = local_now_naive(now).unwrap();
        let conversions = (0..8)
            .map(|_| std::thread::spawn(move || local_now_naive(now)))
            .collect::<Vec<_>>();

        for conversion in conversions {
            assert_eq!(conversion.join().unwrap(), Some(expected));
        }
    }

    #[test]
    fn recurring_reminder_advances_from_fire_time_and_can_be_cancelled() {
        let db_path = unique_temp_path("scheduled-reminder-recurring");
        let store = RetainedQueueStore::new(db_path.clone());
        let reminder = store
            .schedule_reminder("agent-2", 17, "Inspect queues", Some(17))
            .unwrap();
        let now = OffsetDateTime::now_utc();
        Connection::open(&db_path)
            .unwrap()
            .execute(
                "UPDATE scheduled_reminders SET fire_at = ?2 WHERE id = ?1",
                params![
                    reminder.id,
                    (now - Duration::seconds(1)).format(&Rfc3339).unwrap()
                ],
            )
            .unwrap();

        store
            .claim_due_scheduled_reminder(&reminder.id, now)
            .unwrap()
            .unwrap();
        let persisted = scheduled_reminder_conn(&Connection::open(&db_path).unwrap(), &reminder.id)
            .unwrap()
            .unwrap();
        assert!(persisted.is_active);
        assert!(!persisted.fired);
        assert_eq!(persisted.recurring_interval_seconds, Some(17));
        assert_eq!(
            parse_python_naive_datetime(&persisted.fire_at).unwrap(),
            local_now_naive(now + Duration::seconds(17)).unwrap()
        );
        let pending = store.pending_messages_for_target("agent-2", 10).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0]
            .text
            .ends_with(&format!("[sm] Cancel: sm remind cancel {}", reminder.id)));

        let cancelled = store
            .cancel_scheduled_reminder(&reminder.id)
            .unwrap()
            .unwrap();
        assert!(cancelled.is_active);
        assert!(store
            .due_scheduled_reminders(now + Duration::minutes(1))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn reminder_intervals_outside_timestamp_range_are_rejected_without_writes() {
        let db_path = unique_temp_path("scheduled-reminder-range");
        let store = RetainedQueueStore::new(db_path.clone());
        store.ensure_schema().unwrap();

        let error = store
            .schedule_reminder("agent-range", 1, "Never persist", Some(i64::MAX as u64))
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("outside the supported timestamp range"));

        let conn = Connection::open(db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM scheduled_reminders", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn invalid_persisted_recurring_interval_rolls_back_delivery() {
        let db_path = unique_temp_path("scheduled-reminder-invalid-recurring");
        let store = RetainedQueueStore::new(db_path.clone());
        let reminder = store
            .schedule_reminder("agent-invalid", 60, "Do not deliver", Some(60))
            .unwrap();
        let now = OffsetDateTime::now_utc();
        Connection::open(&db_path)
            .unwrap()
            .execute(
                r#"
                UPDATE scheduled_reminders
                SET fire_at = ?2, recurring_interval_seconds = ?3
                WHERE id = ?1
                "#,
                params![
                    reminder.id,
                    (now - Duration::seconds(1)).format(&Rfc3339).unwrap(),
                    i64::MAX
                ],
            )
            .unwrap();

        assert!(store
            .claim_due_scheduled_reminder(&reminder.id, now)
            .is_err());
        assert!(store
            .pending_messages_for_target("agent-invalid", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn reminder_claim_rolls_back_queue_insert_when_schedule_update_fails() {
        let db_path = unique_temp_path("scheduled-reminder-rollback");
        let store = RetainedQueueStore::new(db_path.clone());
        let reminder = store
            .schedule_reminder("agent-3", 30, "Do not split state", None)
            .unwrap();
        let now = OffsetDateTime::now_utc();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE scheduled_reminders SET fire_at = ?2 WHERE id = ?1",
            params![
                reminder.id,
                (now - Duration::seconds(1)).format(&Rfc3339).unwrap()
            ],
        )
        .unwrap();
        conn.execute_batch(
            r#"
            CREATE TRIGGER reject_scheduled_reminder_update
            BEFORE UPDATE ON scheduled_reminders
            BEGIN
                SELECT RAISE(ABORT, 'forced reminder update failure');
            END;
            "#,
        )
        .unwrap();
        drop(conn);

        assert!(store
            .claim_due_scheduled_reminder(&reminder.id, now)
            .is_err());
        assert!(store
            .pending_messages_for_target("agent-3", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn due_reminders_accept_legacy_python_local_timestamps() {
        let db_path = unique_temp_path("scheduled-reminder-legacy-time");
        let store = RetainedQueueStore::new(db_path.clone());
        store.ensure_schema().unwrap();
        let now = OffsetDateTime::now_utc();
        let local_past = local_now_naive(now).unwrap() - Duration::seconds(1);
        let fire_at = python_naive_timestamp(local_past);
        Connection::open(&db_path)
            .unwrap()
            .execute(
                r#"
                INSERT INTO scheduled_reminders
                    (id, target_session_id, message, fire_at, fired, is_active)
                VALUES ('legacy-reminder', 'agent-4', 'legacy', ?1, 0, 1)
                "#,
                params![fire_at],
            )
            .unwrap();

        let due = store.due_scheduled_reminders(now).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "legacy-reminder");
    }

    #[test]
    fn scheduled_reminder_schema_migrates_before_creating_active_index() {
        let db_path = unique_temp_path("scheduled-reminder-legacy-schema");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE scheduled_reminders (
                id TEXT PRIMARY KEY,
                target_session_id TEXT NOT NULL,
                message TEXT NOT NULL,
                fire_at TIMESTAMP NOT NULL,
                task_type TEXT DEFAULT 'reminder',
                fired INTEGER DEFAULT 0
            );
            INSERT INTO scheduled_reminders
                (id, target_session_id, message, fire_at, fired)
            VALUES
                ('legacy-schema', 'agent-5', 'migrate me', '2099-01-01T00:00:00', 0);
            INSERT INTO scheduled_reminders
                (id, target_session_id, message, fire_at, fired)
            VALUES
                ('legacy-fired', 'agent-5', 'already fired', '2020-01-01T00:00:00', 1);
            "#,
        )
        .unwrap();
        drop(conn);

        let store = RetainedQueueStore::new(db_path.clone());
        store.ensure_schema().unwrap();

        let conn = Connection::open(db_path).unwrap();
        let columns = conn
            .prepare("PRAGMA table_info(scheduled_reminders)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert!(columns.iter().any(|column| column == "is_active"));
        assert!(columns
            .iter()
            .any(|column| column == "recurring_interval_seconds"));
        let is_active: i64 = conn
            .query_row(
                "SELECT is_active FROM scheduled_reminders WHERE id = 'legacy-schema'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(is_active, 1);
        let fired_is_active: i64 = conn
            .query_row(
                "SELECT is_active FROM scheduled_reminders WHERE id = 'legacy-fired'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fired_is_active, 0);
    }

    #[test]
    fn pending_messages_skip_and_delete_expired_timeouts() {
        let db_path = unique_temp_path("queue-expiry");
        let store = RetainedQueueStore::new(db_path.clone());
        store.ensure_schema().unwrap();
        let now_utc = OffsetDateTime::now_utc();
        let now_local = local_now_naive(now_utc).unwrap();
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

    #[test]
    fn queue_job_timeout_treats_python_naive_started_at_as_local_time() {
        let now_utc = OffsetDateTime::now_utc();
        let now_local = local_now_naive(now_utc).unwrap();
        let recent_started_at = python_naive_timestamp(now_local - Duration::seconds(30));
        let old_started_at = python_naive_timestamp(now_local - Duration::seconds(300));
        let mut job = QueueJobRuntimeRecord {
            id: "job-naive-timeout".to_owned(),
            job_type: "tests".to_owned(),
            state: "running".to_owned(),
            notify_session_id: None,
            queued_at: recent_started_at.clone(),
            started_at: Some(recent_started_at),
            finished_at: None,
            holding_reason: None,
            wrapper_path: None,
            log_path: None,
            exit_code_path: None,
            timeout_seconds: 120,
            pid: None,
            process_group_id: None,
            exit_code: None,
            completion_notified_at: None,
        };

        assert!(!queue_job_timed_out_at(&job, now_utc));
        job.started_at = Some(old_started_at);
        assert!(queue_job_timed_out_at(&job, now_utc));
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
