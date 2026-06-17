#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration as StdDuration, Instant},
};

use anyhow::{Context, Result};
use rand_core::{OsRng, RngCore};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateCodexReviewRequest {
    pub repo: String,
    pub pr_number: i64,
    pub requester_session_id: Option<String>,
    pub notify_session_id: String,
    pub steer: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueAdmissionPolicy {
    pub max_running_jobs: i64,
    pub perf_cooldown_seconds: i64,
}

impl Default for QueueAdmissionPolicy {
    fn default() -> Self {
        Self {
            max_running_jobs: DEFAULT_MAX_RUNNING_QUEUE_JOBS,
            perf_cooldown_seconds: DEFAULT_PERF_COOLDOWN_SECONDS,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueRecoverySummary {
    pub recovered_running: usize,
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
        create_codex_review_request_conn(&conn, request)
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

    pub fn mark_codex_review_request_pickup_in_path(
        db_path: &Path,
        request_id: &str,
        last_polled_at: &str,
    ) -> Result<Option<CodexReviewRequestRegistration>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(db_path)?;
        conn.execute(
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
        conn.execute(
            r#"
            UPDATE codex_review_request_registrations
            SET last_polled_at = ?2,
                last_error = ?3,
                next_retry_at = COALESCE(?4, next_retry_at)
            WHERE id = ?1 AND is_active = 1
            "#,
            params![request_id, last_polled_at, last_error, next_retry_at],
        )?;
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
        conn.execute(
            r#"
            UPDATE codex_review_request_registrations
            SET last_polled_at = ?2,
                last_error = NULL
            WHERE id = ?1 AND is_active = 1
            "#,
            params![request_id, last_polled_at],
        )?;
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
        conn.execute(
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
        conn.execute(
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
        get_codex_review_request_conn(&conn, request_id)
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
        let timeout_seconds = job.timeout_seconds.max(1) as u64;
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
        Ok(summary)
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
               pid, process_group_id, completion_notified_at
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
                completion_notified_at: row.get(14)?,
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
               pid, process_group_id, completion_notified_at
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
                completion_notified_at: row.get(14)?,
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
        if running_queue_job_count(&jobs, None) as i64 >= admission_policy.max_running_jobs {
            if displace_background_for_perf_conn(
                conn,
                &jobs,
                message_queue_db_path,
                cancel_grace_seconds,
                admission_policy,
            )? {
                continue;
            }
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
const QUEUE_JOB_TYPE_ORDER: [&str; 3] = ["perf", "tests", "background"];
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
        if running_queue_job_count(jobs, Some(job_type)) >= max_concurrent_queue_jobs(job_type) {
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
    if running_queue_job_count(jobs, Some("perf")) >= max_concurrent_queue_jobs("perf") {
        return Ok(false);
    }
    if perf_cooldown_active(jobs, admission_policy) || perf_blocked_by_tests_after_perf(jobs) {
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

fn max_concurrent_queue_jobs(job_type: &str) -> usize {
    match job_type {
        "perf" => 1,
        "tests" | "background" => 2,
        _ => 1,
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
    timeout_seconds: u64,
    cancel_grace_seconds: u64,
    admission_policy: QueueAdmissionPolicy,
) {
    let started = Instant::now();
    let timeout = StdDuration::from_secs(timeout_seconds.max(1));
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
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
            Ok(None) if queue_job_is_cancelled_in_state_dir(&state_dir, &job_id) => {
                let pgid = i64::from(child.id());
                terminate_process_group(pgid, false);
                let grace_deadline = Instant::now() + StdDuration::from_secs(cancel_grace_seconds);
                loop {
                    match child.try_wait() {
                        Ok(Some(_status)) => break,
                        Ok(None) if Instant::now() >= grace_deadline => {
                            terminate_process_group(pgid, true);
                            let _ = child.wait();
                            break;
                        }
                        Ok(None) => thread::sleep(StdDuration::from_millis(100)),
                        Err(_) => break,
                    }
                }
                return;
            }
            Ok(None) if started.elapsed() >= timeout => {
                let pgid = i64::from(child.id());
                terminate_process_group(pgid, false);
                let grace_deadline = Instant::now() + StdDuration::from_secs(cancel_grace_seconds);
                loop {
                    match child.try_wait() {
                        Ok(Some(_status)) => break,
                        Ok(None) if Instant::now() >= grace_deadline => {
                            terminate_process_group(pgid, true);
                            let _ = child.wait();
                            break;
                        }
                        Ok(None) => thread::sleep(StdDuration::from_millis(100)),
                        Err(_) => break,
                    }
                }
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
            Ok(None) => thread::sleep(StdDuration::from_millis(100)),
            Err(_) => {
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
        }
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
    let Some(started_at) = job.started_at.as_deref() else {
        return false;
    };
    let Some(elapsed_seconds) = queue_elapsed_since(started_at, now_utc) else {
        return false;
    };
    elapsed_seconds >= job.timeout_seconds.max(1)
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
    Some((local_now_naive(now_utc) - parsed).whole_seconds())
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
            exit_code = ?4
        WHERE id = ?1 AND state NOT IN ('succeeded', 'failed', 'timed_out', 'cancelled', 'displaced')
        "#,
        params![job.id, state, finished_at, exit_code],
    )?;
    if changed == 0 {
        return Ok(());
    }
    if let Some(completion_notified_at) =
        queue_job_completion_notified_at(job, state, exit_code, &finished_at, message_queue_db_path)
    {
        conn.execute(
            r#"
            UPDATE queue_jobs
            SET completion_notified_at = COALESCE(completion_notified_at, ?2)
            WHERE id = ?1
            "#,
            params![job.id, completion_notified_at],
        )?;
    }
    Ok(())
}

fn queue_job_completion_notified_at(
    job: &QueueJobRuntimeRecord,
    state: &str,
    exit_code: Option<i64>,
    finished_at: &str,
    message_queue_db_path: Option<&Path>,
) -> Option<String> {
    if job.completion_notified_at.is_some() {
        return None;
    }
    let Some(target_session_id) = job
        .notify_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Some(now_rfc3339());
    };
    let Some(message_queue_db_path) = message_queue_db_path else {
        return None;
    };
    let text = queue_job_completion_text(job, state, exit_code, finished_at);
    let queue = RetainedQueueStore::new(message_queue_db_path.to_path_buf());
    match queue.enqueue_message(target_session_id, &text, "sequential", None) {
        Ok(_) => Some(now_rfc3339()),
        Err(_) => None,
    }
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
    let exit_text = exit_code
        .map(|code| format!(" exit={code}"))
        .unwrap_or_default();
    let mut text = format!(
        "[sm queue] {} completed: {}{} runtime={} queue={}. Log: {}",
        job.id,
        state,
        exit_text,
        runtime,
        queued,
        job.log_path.as_deref().unwrap_or("-")
    );
    let stderr_tail = tail_queue_job_log(job.log_path.as_deref(), 8192);
    if !stderr_tail.is_empty() {
        text.push_str("\nlog tail:\n");
        text.push_str(&stderr_tail);
    }
    text
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

fn tail_queue_job_log(path: Option<&str>, max_bytes: usize) -> String {
    let Some(path) = path else {
        return String::new();
    };
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let Ok(metadata) = file.metadata() else {
        return String::new();
    };
    let start = metadata.len().saturating_sub(max_bytes as u64);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).trim().to_owned()
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
            return;
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
    if active_codex_review_request_exists_conn(
        conn,
        &request.repo,
        request.pr_number,
        &request.notify_session_id,
    )? {
        anyhow::bail!(
            "Active Codex review request already exists for {} PR #{}",
            request.repo,
            request.pr_number
        );
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
        INSERT INTO codex_review_request_registrations
            (id, repo, pr_number, requester_session_id, notify_session_id, steer,
             requested_at, latest_request_comment_id, latest_request_comment_url,
             latest_request_posted_at, attempt_count, next_retry_at,
             poll_interval_seconds, retry_interval_seconds, pickup_detected_at,
             pickup_source, review_landed_at, review_source, review_comment_id,
             review_url, last_polled_at, last_error, state, is_active)
        VALUES
            (?1, ?2, ?3, ?4, ?5, ?6,
             ?7, ?8, ?9,
             ?10, ?11, ?12,
             ?13, ?14, NULL,
             NULL, NULL, NULL, NULL,
             NULL, NULL, NULL, ?15, 1)
        "#,
        params![
            registration.id,
            registration.repo,
            registration.pr_number,
            registration.requester_session_id,
            registration.notify_session_id,
            registration.steer,
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

    #[test]
    fn queue_job_timeout_treats_python_naive_started_at_as_local_time() {
        let now_utc = OffsetDateTime::now_utc();
        let now_local = local_now_naive(now_utc);
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
