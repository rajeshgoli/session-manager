#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, macros::format_description, OffsetDateTime};

use crate::queue::{
    followup_notification_text, PendingMessage, QueueMessageMetadata, RetainedQueueStore,
    StopNotifyState,
};
use crate::{
    config::CodexReviewConfig,
    runtime::{TmuxRuntime, TmuxSessionSpec},
};

const DEFAULT_SESSION_STATE_FILE: &str = "~/.local/share/claude-sessions/sessions.json";
const LEGACY_TMP_SESSION_STATE_FILE: &str = "/tmp/claude-sessions/sessions.json";
const OUTPUT_TAIL_BYTES_PER_LINE: u64 = 4096;
const MIN_OUTPUT_TAIL_BYTES: u64 = 16 * 1024;
const MAX_OUTPUT_TAIL_BYTES: u64 = 1024 * 1024;
const CODEX_FORK_THREAD_STARTED_TIMEOUT: Duration = Duration::from_secs(10);
const CODEX_FORK_EVENT_MONITOR_POLL: Duration = Duration::from_millis(250);
const CODEX_FORK_CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
static STATE_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct SessionStore {
    state_file: PathBuf,
    legacy_state_file: Option<PathBuf>,
    write_lock: Arc<Mutex<()>>,
    queue_store: Option<RetainedQueueStore>,
}

impl SessionStore {
    pub fn new(state_file: PathBuf) -> Self {
        let legacy_state_file = if state_file == expand_home(DEFAULT_SESSION_STATE_FILE) {
            Some(PathBuf::from(LEGACY_TMP_SESSION_STATE_FILE))
        } else {
            None
        };
        Self {
            state_file,
            legacy_state_file,
            write_lock: Arc::new(Mutex::new(())),
            queue_store: None,
        }
    }

    pub fn new_with_queue(state_file: PathBuf, queue_db_path: PathBuf) -> Self {
        let mut store = Self::new(state_file);
        store.queue_store = Some(RetainedQueueStore::new(queue_db_path));
        store
    }

    #[cfg(test)]
    fn new_with_legacy_fallback(state_file: PathBuf, legacy_state_file: PathBuf) -> Self {
        Self {
            state_file,
            legacy_state_file: Some(legacy_state_file),
            write_lock: Arc::new(Mutex::new(())),
            queue_store: None,
        }
    }

    pub fn list_sessions(&self, include_stopped: bool) -> Result<Vec<SessionRecord>> {
        let snapshot = self.load_snapshot()?;
        Ok(snapshot
            .into_sessions()
            .into_iter()
            .filter(|session| include_stopped || !session.is_stopped())
            .collect())
    }

    pub fn list_children(
        &self,
        parent_session_id: &str,
        recursive: bool,
        status_filter: Option<&str>,
        include_terminated: bool,
    ) -> Result<Vec<ChildSessionResponse>> {
        let all_sessions = self.load_snapshot()?.into_sessions();
        let mut children = if recursive {
            let mut descendants = Vec::new();
            let mut visited = BTreeSet::new();
            collect_descendants_preorder(
                &all_sessions,
                parent_session_id,
                &mut visited,
                &mut descendants,
            );
            descendants
        } else {
            direct_children(&all_sessions, parent_session_id)
        };

        let status_filter = status_filter
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "all");
        if let Some(status_filter) = status_filter {
            children.retain(|session| match status_filter {
                "running" => normalized_status(&session.status) == "running",
                "completed" => session.completion_status.as_deref() == Some("completed"),
                "error" => session.completion_status.as_deref() == Some("error"),
                _ => true,
            });
        }
        if !include_terminated {
            children.retain(|session| session.completion_status.as_deref() != Some("killed"));
        }

        Ok(children
            .into_iter()
            .map(ChildSessionResponse::from)
            .collect())
    }

    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Ok(None);
        }
        Ok(self
            .load_snapshot()?
            .into_sessions()
            .into_iter()
            .find(|session| {
                session.id == session_id || session.aliases.iter().any(|alias| alias == session_id)
            }))
    }

    pub fn capture_output(&self, session_id: &str, lines: usize) -> Result<Option<String>> {
        let Some(session) = self.get_session(session_id)? else {
            return Ok(None);
        };
        let Some(log_file) = session
            .log_file
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let log_file = expand_home(log_file);
        let output = match read_tail_lines(&log_file, lines) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read session log {}", log_file.display()))
            }
        };
        Ok(Some(output))
    }

    pub fn list_context_monitors(&self) -> Result<Vec<ContextMonitorStatus>> {
        Ok(self
            .load_snapshot()?
            .into_sessions()
            .into_iter()
            .filter(|session| session.context_monitor_enabled)
            .map(|session| {
                let friendly_name = session.cached_display_name();
                ContextMonitorStatus {
                    session_id: session.id,
                    friendly_name,
                    notify_session_id: session.context_monitor_notify,
                }
            })
            .collect())
    }

    pub fn create_core_session(
        &self,
        request: CreateCoreSessionRequest,
        log_dir: Option<PathBuf>,
    ) -> Result<SessionRecord> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let record =
            self.build_core_session_record(sessions, &request, log_dir.as_deref(), false, None)?;
        let log_file = record
            .log_file
            .as_deref()
            .map(expand_home)
            .ok_or_else(|| anyhow::anyhow!("fixture session missing log file"))?;
        append_log_line(&log_file, "[sm-rust] fixture session created")?;
        if let Some(initial_message) = request
            .initial_message
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            append_log_line(&log_file, initial_message)?;
        }
        if let Some(wait) = request.wait {
            append_log_line(
                &log_file,
                &format!("[sm-rust] fixture watch requested: {wait}s"),
            )?;
        }
        sessions.push(serde_json::to_value(&record)?);
        self.write_raw_json_value(&state)?;
        Ok(record)
    }

    pub fn create_core_session_with_runtime(
        &self,
        request: CreateCoreSessionRequest,
        log_dir: Option<PathBuf>,
        runtime: &TmuxRuntime,
    ) -> Result<SessionRecord> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let mut record = self.build_core_session_record(
            sessions,
            &request,
            log_dir.as_deref(),
            true,
            runtime.socket_name(),
        )?;
        ensure_runtime_local_node(&record.node)?;
        let log_file = record
            .log_file
            .as_deref()
            .map(expand_home)
            .ok_or_else(|| anyhow::anyhow!("runtime session missing log file"))?;
        let spec = TmuxSessionSpec {
            session_id: record.id.clone(),
            tmux_session: record.tmux_session.clone(),
            working_dir: expand_home(&record.working_dir).display().to_string(),
            log_file,
            provider: record.provider.clone(),
            initial_message: request.initial_message.clone(),
            model: request.model.clone(),
        };
        let codex_fork_artifacts = runtime.codex_fork_runtime_artifacts(&spec)?;
        runtime.create_session(&spec)?;
        if let Some(artifacts) = &codex_fork_artifacts {
            match wait_for_codex_fork_provider_resume_id(
                &artifacts.event_stream_path,
                CODEX_FORK_THREAD_STARTED_TIMEOUT,
            ) {
                Ok(provider_resume_id) => {
                    record.provider_resume_id = Some(provider_resume_id);
                }
                Err(error) => {
                    let _ = runtime.kill_session(&record.tmux_session);
                    return Err(error).with_context(|| {
                        format!(
                            "codex-fork session {} did not publish a provider resume id",
                            record.id
                        )
                    });
                }
            }
        }
        sessions.push(serde_json::to_value(&record)?);
        if let Err(error) = self.write_raw_json_value(&state) {
            let _ = runtime.kill_session(&record.tmux_session);
            return Err(error);
        }
        if let Some(artifacts) = codex_fork_artifacts {
            self.start_codex_fork_event_monitor(record.id.clone(), artifacts.event_stream_path)?;
        }
        Ok(record)
    }

    pub fn send_core_input(
        &self,
        session_id: &str,
        request: SendCoreInputRequest,
    ) -> Result<Option<CoreInputResult>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(None);
        };
        let status = json_text(session.get("status")).unwrap_or_else(|| "running".to_owned());
        let delivered = normalized_status(&status) != "stopped";
        if delivered {
            let now = now_rfc3339();
            session.insert("last_activity".to_owned(), Value::String(now));
            if let Some(log_file) = json_text(session.get("log_file")) {
                append_log_line(&expand_home(&log_file), &request.text)?;
                if let Some(seconds) = request.notify_after_seconds {
                    append_log_line(
                        &expand_home(&log_file),
                        &format!("[sm-rust] fixture notify requested: {seconds}s"),
                    )?;
                }
            }
        }
        self.write_raw_json_value(&state)?;
        Ok(Some(CoreInputResult {
            ok: true,
            session_id: session_id.to_owned(),
            delivered,
            delivery_mode: request.delivery_mode,
            notify_after_seconds: request.notify_after_seconds,
            status,
        }))
    }

    pub fn send_core_input_with_runtime(
        &self,
        session_id: &str,
        request: SendCoreInputRequest,
        runtime: &TmuxRuntime,
    ) -> Result<Option<CoreInputResult>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;

        let Some(initial_status) = runtime_session_status_raw(&mut state, session_id)? else {
            return Ok(None);
        };
        if normalized_status(&initial_status) == "stopped" {
            return Ok(Some(CoreInputResult {
                ok: true,
                session_id: session_id.to_owned(),
                delivered: false,
                delivery_mode: request.delivery_mode,
                notify_after_seconds: request.notify_after_seconds,
                status: initial_status,
            }));
        }

        let delivery_mode = normalized_delivery_mode(&request.delivery_mode);
        let (queued_text, sender_name) = format_send_input_text_raw(&state, &request);
        if should_persist_runtime_send(&delivery_mode) {
            if let Some(queue) = &self.queue_store {
                let metadata =
                    queue_metadata_for_send_request(&state, session_id, &request, sender_name);
                let pending_message = pending_message_from_metadata(
                    session_id,
                    &queued_text,
                    &delivery_mode,
                    &metadata,
                );
                let message_id = queue.enqueue_message_with_metadata(
                    session_id,
                    &queued_text,
                    &delivery_mode,
                    metadata,
                )?;
                let pending_message = PendingMessage {
                    id: message_id.clone(),
                    ..pending_message
                };
                let drain = if delivery_mode == "urgent" {
                    deliver_urgent_runtime_message_raw(
                        self,
                        &mut state,
                        session_id,
                        runtime,
                        queue,
                        &pending_message,
                    )?
                } else {
                    drain_pending_runtime_messages_raw(
                        self,
                        &mut state,
                        session_id,
                        runtime,
                        queue,
                        if delivery_mode == "important" {
                            Some("important")
                        } else {
                            None
                        },
                        Some(&message_id),
                    )?
                };
                self.write_raw_json_value(&state)?;
                let delivered = drain
                    .delivered_message_ids
                    .iter()
                    .any(|id| id == &message_id)
                    || queue.message_delivered(&message_id)?;
                return Ok(Some(CoreInputResult {
                    ok: true,
                    session_id: session_id.to_owned(),
                    delivered,
                    delivery_mode: request.delivery_mode,
                    notify_after_seconds: request.notify_after_seconds,
                    status: drain.status,
                }));
            }
        }

        let (status, delivered) =
            deliver_runtime_text_to_session_raw(&mut state, session_id, &queued_text, runtime)?;
        self.write_raw_json_value(&state)?;
        Ok(Some(CoreInputResult {
            ok: true,
            session_id: session_id.to_owned(),
            delivered,
            delivery_mode: request.delivery_mode,
            notify_after_seconds: request.notify_after_seconds,
            status,
        }))
    }

    pub fn start_review_with_runtime(
        &self,
        session_id: &str,
        request: StartReviewRequest,
        runtime: &TmuxRuntime,
        timing: &CodexReviewConfig,
    ) -> Result<CoreReviewOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(CoreReviewOutcome::NotFound);
        };

        let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
        if !matches!(provider.as_str(), "codex" | "codex-fork" | "codex-app") {
            return Ok(CoreReviewOutcome::Error(
                "Review requires a Codex session (provider=codex, codex-fork, or codex-app)"
                    .to_owned(),
            ));
        }
        if provider == "codex-app" {
            return Ok(CoreReviewOutcome::Error(
                "Rust core review does not support codex-app review/start yet".to_owned(),
            ));
        }

        let mode = normalized_review_mode(&request.mode);
        if !matches!(
            mode.as_str(),
            "branch" | "uncommitted" | "commit" | "custom"
        ) {
            return Ok(CoreReviewOutcome::Error(format!(
                "Unsupported review mode: {mode}"
            )));
        }
        let status = json_text(session.get("status")).unwrap_or_else(|| "running".to_owned());
        if review_session_is_busy(session, &status) {
            return Ok(CoreReviewOutcome::Error(
                "Session is busy. Wait for current work to complete or use sm clear first."
                    .to_owned(),
            ));
        }

        let node = json_text(session.get("node")).unwrap_or_else(default_node);
        ensure_runtime_local_node(&node)?;
        let tmux_session = json_text(session.get("tmux_session"))
            .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
        let session_socket_name = json_text(session.get("tmux_socket_name"));
        let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
        if !session_runtime.session_exists(&tmux_session)? {
            return Ok(CoreReviewOutcome::Error(
                "Failed to send review sequence to tmux".to_owned(),
            ));
        }
        let working_dir = json_text(session.get("working_dir")).unwrap_or_else(|| ".".to_owned());
        let working_path = expand_home(&working_dir);

        if !git_command_success(&working_path, ["rev-parse", "--git-dir"])? {
            return Ok(CoreReviewOutcome::Error(format!(
                "Working directory is not a git repo: {working_dir}"
            )));
        }

        let branch_position = if mode == "branch" {
            match request
                .base_branch
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(base_branch) => match git_branch_position(&working_path, base_branch)? {
                    Some(position) => Some(position),
                    None => {
                        let branches = git_branch_list(&working_path)?;
                        return Ok(CoreReviewOutcome::Error(format!(
                            "Branch '{base_branch}' not found. Available: {}",
                            branches.join(", ")
                        )));
                    }
                },
                None => None,
            }
        } else {
            None
        };
        if mode == "commit" {
            if let Some(commit_sha) = request
                .commit_sha
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                if !git_commit_exists(&working_path, commit_sha)? {
                    return Ok(CoreReviewOutcome::Error(format!(
                        "Commit '{commit_sha}' not found"
                    )));
                }
            }
        }

        let now = now_rfc3339();
        session.insert(
            "review_config".to_owned(),
            review_config_value(&mode, &request),
        );
        session.insert("last_tool_call".to_owned(), Value::String(now.clone()));
        self.write_raw_json_value(&state)?;

        let delivered = match session_runtime.send_review_sequence(
            &tmux_session,
            &mode,
            request.base_branch.as_deref(),
            request.commit_sha.as_deref(),
            request.custom_prompt.as_deref(),
            branch_position,
            timing,
        ) {
            Ok(delivered) => delivered,
            Err(error) => {
                let mut state = self.load_raw_json_value()?;
                let sessions = ensure_sessions_array_mut(&mut state)?;
                if let Some(session) = session_object_mut(sessions, session_id) {
                    let now = now_rfc3339();
                    mark_review_dispatch_completed(session, &now);
                    self.write_raw_json_value(&state)?;
                }
                return Err(error);
            }
        };
        if !delivered {
            let mut state = self.load_raw_json_value()?;
            let sessions = ensure_sessions_array_mut(&mut state)?;
            if let Some(session) = session_object_mut(sessions, session_id) {
                let now = now_rfc3339();
                mark_review_dispatch_completed(session, &now);
                self.write_raw_json_value(&state)?;
            }
            return Ok(CoreReviewOutcome::Error(
                "Failed to send review sequence to tmux".to_owned(),
            ));
        }

        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(CoreReviewOutcome::NotFound);
        };
        let now = now_rfc3339();
        mark_review_dispatch_completed(session, &now);
        session.insert("status".to_owned(), Value::String("running".to_owned()));
        session.insert("last_activity".to_owned(), Value::String(now));
        self.write_raw_json_value(&state)?;

        Ok(CoreReviewOutcome::Started(CoreReviewResult {
            session_id: session_id.to_owned(),
            review_mode: mode,
            base_branch: request.base_branch,
            commit_sha: request.commit_sha,
            status: "started".to_owned(),
            steer_queued: request
                .steer_text
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty()),
            tmux_session,
            tmux_socket_name: session_socket_name,
            steer_text: request.steer_text,
        }))
    }

    pub fn mark_review_steer_delivered(&self, session_id: &str) -> Result<bool> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(false);
        };
        let Some(review_config) = session
            .get_mut("review_config")
            .and_then(Value::as_object_mut)
        else {
            return Ok(false);
        };
        review_config.insert("steer_delivered".to_owned(), Value::Bool(true));
        self.write_raw_json_value(&state)?;
        Ok(true)
    }

    pub fn drain_runtime_pending_messages_for_session(
        &self,
        session_id: &str,
        runtime: &TmuxRuntime,
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        if let Some(queue) = &self.queue_store {
            drain_pending_runtime_messages_raw(
                self, &mut state, session_id, runtime, queue, None, None,
            )?;
            self.write_raw_json_value(&state)?;
        }
        Ok(())
    }

    pub fn enqueue_stop_notification_for_session(
        &self,
        session_id: &str,
        text: &str,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        if let Some(queue) = &self.queue_store {
            enqueue_stop_notification_raw(self, &mut state, runtime, queue, session_id, text)?;
        } else if raw_session_object(&state, session_id).is_some() {
            push_retained_message_raw(
                &mut state,
                session_id,
                text,
                "important",
                Some("stop_notify"),
            )?;
        }
        self.write_raw_json_value(&state)?;
        Ok(())
    }

    pub fn clear_core_session(
        &self,
        session_id: &str,
        request: ClearSessionRequest,
    ) -> Result<CoreClearOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(CoreClearOutcome::NotFound);
        };
        if let Some(message) =
            clear_authorization_error(session, request.requester_session_id.as_deref())
        {
            return Ok(CoreClearOutcome::Unauthorized(message));
        }
        let now = now_rfc3339();
        reset_session_after_clear(session, &now);
        if let Some(log_file) = json_text(session.get("log_file")) {
            append_log_line(&expand_home(&log_file), "[sm-rust] fixture context cleared")?;
            if let Some(prompt) = request
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                append_log_line(&expand_home(&log_file), prompt)?;
            }
        }
        self.write_raw_json_value(&state)?;
        Ok(CoreClearOutcome::Cleared(CoreClearResult {
            status: "cleared".to_owned(),
            session_id: session_id.to_owned(),
        }))
    }

    pub fn clear_core_session_with_runtime(
        &self,
        session_id: &str,
        request: ClearSessionRequest,
        runtime: &TmuxRuntime,
    ) -> Result<CoreClearOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(CoreClearOutcome::NotFound);
        };
        if let Some(message) =
            clear_authorization_error(session, request.requester_session_id.as_deref())
        {
            return Ok(CoreClearOutcome::Unauthorized(message));
        }
        let node = json_text(session.get("node")).unwrap_or_else(default_node);
        ensure_runtime_local_node(&node)?;
        let tmux_session = json_text(session.get("tmux_session"))
            .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
        let session_socket_name = json_text(session.get("tmux_socket_name"));
        let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
        let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
        let clear_command = if matches!(provider.as_str(), "codex" | "codex-fork") {
            "/new"
        } else {
            "/clear"
        };
        let prompt = request
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let wake_completed =
            json_text(session.get("completion_status")).is_some_and(|value| value == "completed");
        let delivered = session_runtime.clear_session(
            &tmux_session,
            clear_command,
            prompt.as_deref(),
            wake_completed,
        )?;
        if !delivered {
            return Err(anyhow::anyhow!("tmux session is not running"));
        }
        let now = now_rfc3339();
        reset_session_after_clear(session, &now);
        self.write_raw_json_value(&state)?;
        Ok(CoreClearOutcome::Cleared(CoreClearResult {
            status: "cleared".to_owned(),
            session_id: session_id.to_owned(),
        }))
    }

    pub fn restore_core_session(&self, session_id: &str) -> Result<Option<CoreRestoreOutcome>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(None);
        };
        let status = json_text(session.get("status")).unwrap_or_else(|| "running".to_owned());
        if normalized_status(&status) != "stopped" {
            return Ok(Some(CoreRestoreOutcome::NotStopped));
        }
        let now = now_rfc3339();
        session.insert("status".to_owned(), Value::String("running".to_owned()));
        session.insert("stopped_at".to_owned(), Value::Null);
        session.insert("completion_status".to_owned(), Value::Null);
        session.insert("completion_message".to_owned(), Value::Null);
        session.insert("completed_at".to_owned(), Value::Null);
        session.insert("agent_task_completed_at".to_owned(), Value::Null);
        session.insert("last_activity".to_owned(), Value::String(now));
        if let Some(log_file) = json_text(session.get("log_file")) {
            append_log_line(
                &expand_home(&log_file),
                "[sm-rust] fixture session restored",
            )?;
        }
        let restored = serde_json::from_value::<SessionRecord>(Value::Object(session.clone()))?;
        self.write_raw_json_value(&state)?;
        Ok(Some(CoreRestoreOutcome::Restored(restored)))
    }

    pub fn restore_core_session_with_runtime(
        &self,
        session_id: &str,
        runtime: &TmuxRuntime,
    ) -> Result<Option<CoreRestoreOutcome>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(None);
        };
        let status = json_text(session.get("status")).unwrap_or_else(|| "running".to_owned());
        if normalized_status(&status) != "stopped" {
            return Ok(Some(CoreRestoreOutcome::NotStopped));
        }
        let record = serde_json::from_value::<SessionRecord>(Value::Object(session.clone()))?;
        if !is_primary_node(&record.node) {
            return Ok(Some(CoreRestoreOutcome::UnsupportedNode(record.node)));
        }
        if !matches!(record.provider.as_str(), "claude" | "codex-fork") {
            return Ok(Some(CoreRestoreOutcome::UnsupportedProvider(
                record.provider,
            )));
        }
        if record.provider == "codex-fork"
            && record
                .provider_resume_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
        {
            return Ok(Some(CoreRestoreOutcome::MissingProviderResumeId(
                record.provider,
            )));
        }
        let Some(log_file) = record
            .log_file
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(expand_home)
        else {
            return Err(anyhow::anyhow!("session {session_id} missing log_file"));
        };
        let session_runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
        if session_runtime.session_exists(&record.tmux_session)? {
            let _ = session_runtime.kill_session(&record.tmux_session)?;
        }
        let spec = TmuxSessionSpec {
            session_id: record.id.clone(),
            tmux_session: record.tmux_session.clone(),
            working_dir: expand_home(&record.working_dir).display().to_string(),
            log_file,
            provider: record.provider.clone(),
            initial_message: None,
            model: record.model.clone(),
        };
        let codex_fork_artifacts = session_runtime.codex_fork_runtime_artifacts(&spec)?;
        session_runtime.restore_session(
            &spec,
            &record.provider,
            record.provider_resume_id.as_deref(),
        )?;

        let now = now_rfc3339();
        session.insert("status".to_owned(), Value::String("running".to_owned()));
        session.insert("stopped_at".to_owned(), Value::Null);
        session.insert("completion_status".to_owned(), Value::Null);
        session.insert("completion_message".to_owned(), Value::Null);
        session.insert("completed_at".to_owned(), Value::Null);
        session.insert("agent_task_completed_at".to_owned(), Value::Null);
        session.insert("last_activity".to_owned(), Value::String(now));
        if let Some(socket_name) = session_runtime.socket_name() {
            session.insert(
                "tmux_socket_name".to_owned(),
                Value::String(socket_name.to_owned()),
            );
        }
        let restored = serde_json::from_value::<SessionRecord>(Value::Object(session.clone()))?;
        self.write_raw_json_value(&state)?;
        if let Some(artifacts) = codex_fork_artifacts {
            self.start_codex_fork_event_monitor(restored.id.clone(), artifacts.event_stream_path)?;
        }
        Ok(Some(CoreRestoreOutcome::Restored(restored)))
    }

    pub fn revive_stopped_tmux_client_session(
        &self,
        tmux_session: &str,
        runtime: &TmuxRuntime,
    ) -> Result<Option<String>> {
        let tmux_session = tmux_session.trim();
        if tmux_session.is_empty() {
            return Ok(None);
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session_index) = sessions.iter().position(|value| {
            let Some(session) = value.as_object() else {
                return false;
            };
            json_text(session.get("tmux_session")).as_deref() == Some(tmux_session)
                && json_text(session.get("provider")).as_deref() == Some("codex-fork")
                && json_text(session.get("node"))
                    .as_deref()
                    .is_none_or(is_primary_node)
                && json_text(session.get("status"))
                    .as_deref()
                    .is_some_and(|status| normalized_status(status) == "stopped")
        }) else {
            return Ok(None);
        };

        let Some(session) = sessions[session_index].as_object() else {
            return Ok(None);
        };
        let Some(session_id) = json_text(session.get("id")) else {
            return Ok(None);
        };
        let session_runtime =
            runtime.for_socket_name(json_text(session.get("tmux_socket_name")).as_deref());
        if !session_runtime.session_exists(tmux_session)? {
            return Ok(None);
        }

        let now = now_rfc3339();
        let Some(session) = sessions[session_index].as_object_mut() else {
            return Ok(None);
        };
        session.insert("status".to_owned(), Value::String("idle".to_owned()));
        session.insert("stopped_at".to_owned(), Value::Null);
        session.insert("completion_status".to_owned(), Value::Null);
        session.insert("completion_message".to_owned(), Value::Null);
        session.insert("completed_at".to_owned(), Value::Null);
        session.insert("agent_task_completed_at".to_owned(), Value::Null);
        session.insert("error_message".to_owned(), Value::Null);
        session.insert("last_activity".to_owned(), Value::String(now));
        if let Some(socket_name) = session_runtime.socket_name() {
            session.insert(
                "tmux_socket_name".to_owned(),
                Value::String(socket_name.to_owned()),
            );
        }

        let spec = codex_fork_spec_for_session_raw(&session_id, session)?;
        let codex_fork_artifacts = session_runtime.codex_fork_runtime_artifacts(&spec)?;
        self.write_raw_json_value(&state)?;
        if let Some(artifacts) = codex_fork_artifacts {
            self.start_codex_fork_event_monitor_from_current_end(
                session_id.clone(),
                artifacts.event_stream_path,
            )?;
        }
        Ok(Some(session_id))
    }

    pub fn set_context_monitor(
        &self,
        session_id: &str,
        request: ContextMonitorRequest,
    ) -> Result<ContextMonitorOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let requester_session_id = request.requester_session_id.trim();
        if requester_session_id.is_empty() {
            return Ok(ContextMonitorOutcome::Unauthorized);
        }
        let notify_session_id = request
            .notify_session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        if request.enabled && notify_session_id.is_none() {
            return Ok(ContextMonitorOutcome::MissingNotifyTarget);
        }
        if request.enabled {
            let notify_session_id = notify_session_id.as_deref().unwrap_or_default();
            if session_object(sessions, notify_session_id).is_none() {
                return Ok(ContextMonitorOutcome::NotifyTargetNotFound(
                    notify_session_id.to_owned(),
                ));
            }
        }
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(ContextMonitorOutcome::NotFound);
        };
        let is_self = requester_session_id == session_id;
        let is_parent =
            json_text(session.get("parent_session_id")).as_deref() == Some(requester_session_id);
        if !is_self && !is_parent {
            return Ok(ContextMonitorOutcome::Unauthorized);
        }
        session.insert(
            "context_monitor_enabled".to_owned(),
            Value::Bool(request.enabled),
        );
        if request.enabled {
            session.insert(
                "context_monitor_notify".to_owned(),
                Value::String(notify_session_id.unwrap()),
            );
        } else {
            session.insert("context_monitor_notify".to_owned(), Value::Null);
        }
        self.write_raw_json_value(&state)?;
        Ok(ContextMonitorOutcome::Updated(ContextMonitorResult {
            status: "ok".to_owned(),
            enabled: request.enabled,
        }))
    }

    pub fn schedule_handoff(
        &self,
        session_id: &str,
        request: HandoffRequest,
    ) -> Result<HandoffOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        if request.requester_session_id.trim() != session_id {
            return Ok(HandoffOutcome::Error(
                "sm handoff is self-directed only - requester must equal target session".to_owned(),
            ));
        }
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(HandoffOutcome::Error(format!(
                "Session {session_id} not found"
            )));
        };
        let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
        if provider == "codex-app" {
            return Ok(HandoffOutcome::Error(
                "sm handoff is not supported for codex-app sessions".to_owned(),
            ));
        }
        session.insert(
            "pending_handoff_path".to_owned(),
            Value::String(request.file_path.clone()),
        );
        self.write_raw_json_value(&state)?;
        Ok(HandoffOutcome::Recorded(HandoffResult {
            status: "recorded".to_owned(),
        }))
    }

    pub fn list_agent_registrations(&self) -> Result<Vec<AgentRegistrationResponse>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let mut changed = recover_missing_maintainer_registration_raw(&mut state)?;
        changed |= prune_agent_registrations_raw(&mut state)?;
        let registrations = agent_registration_responses_from_state(&state)?;
        if changed {
            self.write_raw_json_value(&state)?;
        }
        Ok(registrations)
    }

    pub fn lookup_agent_registration(
        &self,
        role: &str,
    ) -> Result<Option<AgentRegistrationResponse>> {
        let normalized_role = normalize_role(role);
        if normalized_role.is_empty() {
            return Ok(None);
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let mut changed = recover_missing_maintainer_registration_raw(&mut state)?;
        changed |= prune_agent_registrations_raw(&mut state)?;
        let registration = agent_registration_responses_from_state(&state)?
            .into_iter()
            .find(|registration| registration.role == normalized_role);
        if changed {
            self.write_raw_json_value(&state)?;
        }
        Ok(registration)
    }

    pub fn register_agent_role(
        &self,
        session_id: &str,
        request: RoleRegistrationRequest,
    ) -> Result<RegistryMutationOutcome> {
        if request.requester_session_id.trim() != session_id {
            return Ok(RegistryMutationOutcome::BadRequest(
                "sm register is self-directed only".to_owned(),
            ));
        }
        self.register_agent_role_raw(session_id, &request.role)
    }

    pub fn unregister_agent_role(
        &self,
        session_id: &str,
        request: RoleRegistrationRequest,
    ) -> Result<RegistryMutationOutcome> {
        if request.requester_session_id.trim() != session_id {
            return Ok(RegistryMutationOutcome::BadRequest(
                "sm unregister is self-directed only".to_owned(),
            ));
        }
        self.unregister_agent_role_raw(session_id, &request.role)
    }

    pub fn set_maintainer_session(
        &self,
        session_id: &str,
        request: SetMaintainerRequest,
    ) -> Result<MaintainerMutationOutcome> {
        if request.requester_session_id.trim() != session_id {
            return Ok(MaintainerMutationOutcome::BadRequest(
                "sm maintainer is self-directed only".to_owned(),
            ));
        }
        match self.register_agent_role_raw(session_id, "maintainer")? {
            RegistryMutationOutcome::Registered(_) => {
                let session = self.get_session(session_id)?.ok_or_else(|| {
                    anyhow::anyhow!("session disappeared after maintainer update")
                })?;
                Ok(MaintainerMutationOutcome::Updated(session))
            }
            RegistryMutationOutcome::NotFound => Ok(MaintainerMutationOutcome::NotFound),
            RegistryMutationOutcome::BadRequest(_) | RegistryMutationOutcome::Conflict(_) => Ok(
                MaintainerMutationOutcome::BadRequest("Failed to register maintainer".to_owned()),
            ),
            RegistryMutationOutcome::RoleNotRegistered | RegistryMutationOutcome::RoleNotOwned => {
                Ok(MaintainerMutationOutcome::BadRequest(
                    "Failed to register maintainer".to_owned(),
                ))
            }
        }
    }

    pub fn clear_maintainer_session(
        &self,
        session_id: &str,
        request: SetMaintainerRequest,
    ) -> Result<MaintainerMutationOutcome> {
        if request.requester_session_id.trim() != session_id {
            return Ok(MaintainerMutationOutcome::BadRequest(
                "sm maintainer --clear is self-directed only".to_owned(),
            ));
        }
        match self.unregister_agent_role_raw(session_id, "maintainer")? {
            RegistryMutationOutcome::Registered(_) => {
                let session = self
                    .get_session(session_id)?
                    .ok_or_else(|| anyhow::anyhow!("session disappeared after maintainer clear"))?;
                Ok(MaintainerMutationOutcome::Updated(session))
            }
            RegistryMutationOutcome::NotFound => Ok(MaintainerMutationOutcome::NotFound),
            RegistryMutationOutcome::RoleNotRegistered
            | RegistryMutationOutcome::RoleNotOwned
            | RegistryMutationOutcome::BadRequest(_)
            | RegistryMutationOutcome::Conflict(_) => Ok(MaintainerMutationOutcome::BadRequest(
                "Session is not the active maintainer".to_owned(),
            )),
        }
    }

    fn register_agent_role_raw(
        &self,
        session_id: &str,
        role: &str,
    ) -> Result<RegistryMutationOutcome> {
        let normalized_role = normalize_role(role);
        if normalized_role.is_empty() {
            return Ok(RegistryMutationOutcome::BadRequest(
                "Role cannot be empty".to_owned(),
            ));
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        recover_missing_maintainer_registration_raw(&mut state)?;
        prune_agent_registrations_raw(&mut state)?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        let Some(session) = sessions.iter().find(|session| session.id == session_id) else {
            return Ok(RegistryMutationOutcome::NotFound);
        };
        if session.is_stopped() {
            return Ok(RegistryMutationOutcome::Conflict(
                "Stopped sessions cannot register roles".to_owned(),
            ));
        }

        let existing = find_raw_registration(&state, &normalized_role)?;
        if let Some(existing) = existing
            .as_ref()
            .filter(|entry| entry.session_id != session_id)
        {
            return Ok(RegistryMutationOutcome::Conflict(format!(
                "Role \"{}\" is already registered to {}",
                normalized_role, existing.session_id
            )));
        }

        let created_at = existing
            .and_then(|entry| entry.created_at)
            .unwrap_or_else(now_rfc3339);
        upsert_raw_registration(&mut state, &normalized_role, session_id, &created_at)?;
        sync_maintainer_alias_raw(&mut state)?;
        let response = agent_registration_responses_from_state(&state)?
            .into_iter()
            .find(|registration| registration.role == normalized_role)
            .ok_or_else(|| anyhow::anyhow!("registered role {normalized_role} was not readable"))?;
        self.write_raw_json_value(&state)?;
        Ok(RegistryMutationOutcome::Registered(response))
    }

    fn unregister_agent_role_raw(
        &self,
        session_id: &str,
        role: &str,
    ) -> Result<RegistryMutationOutcome> {
        let normalized_role = normalize_role(role);
        if normalized_role.is_empty() {
            return Ok(RegistryMutationOutcome::BadRequest(
                "Role cannot be empty".to_owned(),
            ));
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        recover_missing_maintainer_registration_raw(&mut state)?;
        prune_agent_registrations_raw(&mut state)?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        if !sessions.iter().any(|session| session.id == session_id) {
            return Ok(RegistryMutationOutcome::NotFound);
        }

        let registrations = agent_registration_responses_from_state(&state)?;
        let Some(response) = registrations
            .into_iter()
            .find(|registration| registration.role == normalized_role)
        else {
            return Ok(RegistryMutationOutcome::RoleNotRegistered);
        };
        if response.session_id != session_id {
            return Ok(RegistryMutationOutcome::RoleNotOwned);
        }

        remove_raw_registration(&mut state, &normalized_role)?;
        forget_role_last_session_raw(&mut state, &normalized_role)?;
        sync_maintainer_alias_raw(&mut state)?;
        self.write_raw_json_value(&state)?;
        Ok(RegistryMutationOutcome::Registered(response))
    }

    pub fn retire_core_session(
        &self,
        session_id: &str,
        requester_session_id: Option<&str>,
    ) -> Result<CoreRetireOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let recipient_name = {
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(CoreRetireOutcome::NotFound);
            };
            if let Some(requester_session_id) = requester_session_id {
                if !requester_session_id.is_empty()
                    && json_text(session.get("parent_session_id")).as_deref()
                        != Some(requester_session_id)
                {
                    return Ok(CoreRetireOutcome::NotChild);
                }
            }
            let recipient_name = raw_session_display_name(session, session_id);
            let now = now_rfc3339();
            session.insert("status".to_owned(), Value::String("stopped".to_owned()));
            mark_session_killed(session, &now);
            session.insert("stopped_at".to_owned(), Value::String(now.clone()));
            session.insert("last_activity".to_owned(), Value::String(now));
            if let Some(log_file) = json_text(session.get("log_file")) {
                append_log_line(&expand_home(&log_file), "[sm-rust] fixture session retired")?;
            }
            recipient_name
        };
        complete_stop_notify_after_stop_raw(self, &mut state, None, session_id, &recipient_name)?;
        self.write_raw_json_value(&state)?;
        Ok(CoreRetireOutcome::Retired(CoreRetireResult {
            ok: true,
            session_id: session_id.to_owned(),
            status: "killed".to_owned(),
        }))
    }

    pub fn retire_core_session_with_runtime(
        &self,
        session_id: &str,
        requester_session_id: Option<&str>,
        runtime: &TmuxRuntime,
    ) -> Result<CoreRetireOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let (node, tmux_session, session_socket_name, recipient_name) = {
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(CoreRetireOutcome::NotFound);
            };
            if let Some(requester_session_id) = requester_session_id {
                if !requester_session_id.is_empty()
                    && json_text(session.get("parent_session_id")).as_deref()
                        != Some(requester_session_id)
                {
                    return Ok(CoreRetireOutcome::NotChild);
                }
            }
            let node = json_text(session.get("node")).unwrap_or_else(default_node);
            let tmux_session = json_text(session.get("tmux_session"))
                .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
            let session_socket_name = json_text(session.get("tmux_socket_name"));
            let recipient_name = raw_session_display_name(session, session_id);
            (node, tmux_session, session_socket_name, recipient_name)
        };
        if !is_primary_node(&node) {
            return Ok(CoreRetireOutcome::UnsupportedNode(node));
        }
        let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
        let _ = session_runtime.kill_session(&tmux_session)?;
        let now = now_rfc3339();
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let session = session_object_mut(sessions, session_id)
            .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared during retire"))?;
        session.insert("status".to_owned(), Value::String("stopped".to_owned()));
        mark_session_killed(session, &now);
        session.insert("stopped_at".to_owned(), Value::String(now.clone()));
        session.insert("last_activity".to_owned(), Value::String(now));
        complete_stop_notify_after_stop_raw(
            self,
            &mut state,
            Some(runtime),
            session_id,
            &recipient_name,
        )?;
        self.write_raw_json_value(&state)?;
        Ok(CoreRetireOutcome::Retired(CoreRetireResult {
            ok: true,
            session_id: session_id.to_owned(),
            status: "killed".to_owned(),
        }))
    }

    pub fn set_agent_status(
        &self,
        session_id: &str,
        request: AgentStatusRequest,
    ) -> Result<Option<AgentStatusResult>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(None);
        };
        let now = now_rfc3339();
        match request.text {
            Some(text) => {
                session.insert("agent_status_text".to_owned(), Value::String(text.clone()));
                session.insert("agent_status_at".to_owned(), Value::String(now.clone()));
                session.insert("last_activity".to_owned(), Value::String(now));
                self.write_raw_json_value(&state)?;
                Ok(Some(AgentStatusResult {
                    status: "updated".to_owned(),
                    session_id: session_id.to_owned(),
                    agent_status_text: Some(text),
                }))
            }
            None => {
                session.insert("agent_status_text".to_owned(), Value::Null);
                session.insert("agent_status_at".to_owned(), Value::Null);
                session.insert("last_activity".to_owned(), Value::String(now));
                self.write_raw_json_value(&state)?;
                Ok(Some(AgentStatusResult {
                    status: "updated".to_owned(),
                    session_id: session_id.to_owned(),
                    agent_status_text: None,
                }))
            }
        }
    }

    pub fn task_complete(
        &self,
        session_id: &str,
        request: TaskCompleteRequest,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<TaskCompleteOutcome> {
        if request.requester_session_id.trim() != session_id {
            return Ok(TaskCompleteOutcome::Error(
                "sm task-complete is self-directed only — requester must equal target session"
                    .to_owned(),
            ));
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        let Some(session) = sessions.iter().find(|session| session.id == session_id) else {
            return Ok(TaskCompleteOutcome::Error(format!(
                "Session {session_id} not found"
            )));
        };

        let queue_parent = match &self.queue_store {
            Some(queue) => queue.active_parent_wake_parent(session_id)?,
            None => None,
        };
        let em_session_id = queue_parent
            .or(active_parent_wake_parent_raw(&state, session_id)?)
            .or_else(|| session.parent_session_id.clone());
        if let Some(queue) = &self.queue_store {
            queue.cancel_remind(session_id)?;
            queue.cancel_parent_wake(session_id)?;
        }
        deactivate_remind_raw(&mut state, session_id)?;
        deactivate_parent_wake_raw(&mut state, session_id)?;

        let completed_at = now_rfc3339();
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let session_object = session_object_mut(sessions, session_id)
            .ok_or_else(|| anyhow::anyhow!("session disappeared during task-complete"))?;
        session_object.insert(
            "agent_task_completed_at".to_owned(),
            Value::String(completed_at.clone()),
        );

        let mut em_notified = false;
        if let Some(em_session_id) = em_session_id {
            let friendly = session
                .cached_display_name()
                .unwrap_or_else(|| non_empty_or(session.name.clone(), &session.id));
            let text =
                format!("[sm task-complete] agent {session_id}({friendly}) completed its task.");
            if let Some(queue) = &self.queue_store {
                let message_id = queue.enqueue_message(
                    &em_session_id,
                    &text,
                    "important",
                    Some("task_complete"),
                )?;
                if let Some(runtime) = runtime {
                    if let Some(parent_session) = raw_session_object(&state, &em_session_id) {
                        let parent_node =
                            json_text(parent_session.get("node")).unwrap_or_else(default_node);
                        if is_primary_node(&parent_node) {
                            let drain = drain_pending_runtime_messages_raw(
                                self,
                                &mut state,
                                &em_session_id,
                                runtime,
                                queue,
                                Some("important"),
                                Some(&message_id),
                            )?;
                            if drain
                                .delivered_message_ids
                                .iter()
                                .any(|delivered_id| delivered_id == &message_id)
                            {
                                clear_agent_task_completed_raw(&mut state, &em_session_id)?;
                            }
                        }
                    }
                }
            }
            push_retained_message_raw(
                &mut state,
                &em_session_id,
                &text,
                "important",
                Some("task_complete"),
            )?;
            em_notified = true;
        }

        self.write_raw_json_value(&state)?;
        Ok(TaskCompleteOutcome::Completed(TaskCompleteResult {
            status: "completed".to_owned(),
            session_id: session_id.to_owned(),
            em_notified,
            agent_task_completed_at: completed_at,
        }))
    }

    pub fn turn_complete(
        &self,
        session_id: &str,
        request: TaskCompleteRequest,
    ) -> Result<TurnCompleteOutcome> {
        if request.requester_session_id.trim() != session_id {
            return Ok(TurnCompleteOutcome::Error(
                "sm turn-complete is self-directed only — requester must equal target session"
                    .to_owned(),
            ));
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        if !sessions.iter().any(|session| session.id == session_id) {
            return Ok(TurnCompleteOutcome::Error(format!(
                "Session {session_id} not found"
            )));
        }

        if let Some(queue) = &self.queue_store {
            queue.cancel_remind(session_id)?;
        }
        deactivate_remind_raw(&mut state, session_id)?;
        self.write_raw_json_value(&state)?;
        Ok(TurnCompleteOutcome::Completed(TurnCompleteResult {
            status: "turn_completed".to_owned(),
            session_id: session_id.to_owned(),
        }))
    }

    pub fn arm_stop_notify(
        &self,
        session_id: &str,
        request: ArmStopNotifyRequest,
    ) -> Result<ArmStopNotifyOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        let Some(target) = sessions.iter().find(|session| session.id == session_id) else {
            return Ok(ArmStopNotifyOutcome::NotFound);
        };

        let requester = sessions
            .iter()
            .find(|session| session.id == request.requester_session_id);
        if !requester.is_some_and(|session| session.is_em) {
            return Ok(ArmStopNotifyOutcome::Forbidden(
                "Only EM sessions (is_em=True) may arm stop notifications".to_owned(),
            ));
        }

        if target.parent_session_id.as_deref() != Some(request.requester_session_id.as_str()) {
            return Ok(ArmStopNotifyOutcome::Forbidden(
                "Cannot arm stop notify — not the parent of target session".to_owned(),
            ));
        }

        let Some(sender) = sessions
            .iter()
            .find(|session| session.id == request.sender_session_id)
        else {
            return Ok(ArmStopNotifyOutcome::UnknownSender(
                request.sender_session_id,
            ));
        };

        if target.provider == "codex-fork" {
            return Ok(ArmStopNotifyOutcome::Suppressed(ArmStopNotifyResult {
                status: "suppressed".to_owned(),
                session_id: session_id.to_owned(),
                sender_session_id: request.sender_session_id,
                reason: Some("notify_on_stop disabled for codex-fork sessions".to_owned()),
            }));
        }

        let sender_name = sender
            .cached_display_name()
            .unwrap_or_else(|| non_empty_or(sender.name.clone(), &sender.id));
        if let Some(queue) = &self.queue_store {
            queue.upsert_stop_notify(
                session_id,
                &request.sender_session_id,
                &sender_name,
                request.delay_seconds.max(0),
            )?;
        }
        upsert_stop_notify_raw(
            &mut state,
            session_id,
            &request.sender_session_id,
            &sender_name,
            request.delay_seconds.max(0),
        )?;
        self.write_raw_json_value(&state)?;
        Ok(ArmStopNotifyOutcome::Armed(ArmStopNotifyResult {
            status: "ok".to_owned(),
            session_id: session_id.to_owned(),
            sender_session_id: request.sender_session_id,
            reason: None,
        }))
    }

    pub fn register_subagent_start(
        &self,
        session_id: &str,
        request: SubagentStartRequest,
    ) -> Result<SubagentStartOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let now = now_python_naive_iso();
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(SubagentStartOutcome::NotFound);
        };
        let subagent = json!({
            "agent_id": request.agent_id,
            "agent_type": request.agent_type,
            "parent_session_id": session_id,
            "transcript_path": request.transcript_path,
            "started_at": now,
            "stopped_at": null,
            "status": "running",
            "summary": null
        });
        let response = subagent_response_from_value(&subagent)?;
        ensure_subagents_array_mut(session).push(subagent);
        self.write_raw_json_value(&state)?;
        Ok(SubagentStartOutcome::Registered(response))
    }

    pub fn register_subagent_stop(
        &self,
        session_id: &str,
        agent_id: &str,
        request: SubagentStopRequest,
    ) -> Result<SubagentStopOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let now = now_python_naive_iso();
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(SubagentStopOutcome::SessionNotFound);
        };
        let subagents = ensure_subagents_array_mut(session);
        let Some(subagent) = subagents
            .iter_mut()
            .find(|subagent| subagent.get("agent_id").and_then(Value::as_str) == Some(agent_id))
        else {
            return Ok(SubagentStopOutcome::SubagentNotFound(agent_id.to_owned()));
        };
        if let Some(subagent) = subagent.as_object_mut() {
            subagent.insert("stopped_at".to_owned(), Value::String(now));
            subagent.insert("status".to_owned(), Value::String("completed".to_owned()));
            if let Some(transcript_path) = request.transcript_path {
                subagent.insert("transcript_path".to_owned(), Value::String(transcript_path));
            }
            if let Some(summary) = request.summary {
                subagent.insert("summary".to_owned(), Value::String(summary));
            }
        }
        let summary = subagent
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.write_raw_json_value(&state)?;
        Ok(SubagentStopOutcome::Stopped(SubagentStopResult {
            session_id: session_id.to_owned(),
            agent_id: agent_id.to_owned(),
            status: "stopped".to_owned(),
            summary,
        }))
    }

    pub fn list_subagents(&self, session_id: &str) -> Result<Option<SubagentListResponse>> {
        let state = self.load_raw_json_value()?;
        let Some(session) = raw_session_object(&state, session_id) else {
            return Ok(None);
        };
        let subagents = session
            .get("subagents")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(subagent_response_from_value)
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Some(SubagentListResponse {
            session_id: session_id.to_owned(),
            subagents,
        }))
    }

    fn load_snapshot(&self) -> Result<StateSnapshot> {
        let state_file = self.readable_state_file();
        if !state_file.exists() {
            return Ok(StateSnapshot::default());
        }
        match read_snapshot(&state_file) {
            Ok(snapshot) => Ok(snapshot),
            Err(primary_error) => {
                if state_file == self.state_file {
                    if let Some(legacy_state_file) = &self.legacy_state_file {
                        if legacy_state_file.exists() {
                            return read_snapshot(legacy_state_file).with_context(|| {
                                format!(
                                    "failed to read fallback session state {} after primary failed: {primary_error:#}",
                                    legacy_state_file.display()
                                )
                            });
                        }
                    }
                }
                Err(primary_error)
            }
        }
    }

    fn readable_state_file(&self) -> PathBuf {
        if !self.state_file.exists() {
            if let Some(legacy_state_file) = &self.legacy_state_file {
                if legacy_state_file.exists() {
                    return legacy_state_file.clone();
                }
            }
        }
        self.state_file.clone()
    }

    fn load_raw_json_value(&self) -> Result<Value> {
        let state_file = self.readable_state_file();
        if !state_file.exists() {
            return Ok(json!({ "sessions": [] }));
        }
        let content = fs::read_to_string(&state_file)
            .with_context(|| format!("failed to read session state {}", state_file.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse session state {}", state_file.display()))
    }

    fn write_raw_json_value(&self, value: &Value) -> Result<()> {
        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create state directory {}", parent.display())
            })?;
        }
        let tmp = self.state_file.with_extension(format!(
            "json.tmp.{}.{}",
            std::process::id(),
            STATE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&tmp, serde_json::to_vec_pretty(value)?)
            .with_context(|| format!("failed to write temp state {}", tmp.display()))?;
        fs::rename(&tmp, &self.state_file).with_context(|| {
            format!(
                "failed to atomically replace session state {}",
                self.state_file.display()
            )
        })?;
        Ok(())
    }

    fn write_guard(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.write_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("session state write lock poisoned"))
    }

    fn start_codex_fork_event_monitor(
        &self,
        session_id: String,
        event_stream_path: PathBuf,
    ) -> Result<()> {
        self.start_codex_fork_event_monitor_at_offset(session_id, event_stream_path, 0)
    }

    fn start_codex_fork_event_monitor_from_current_end(
        &self,
        session_id: String,
        event_stream_path: PathBuf,
    ) -> Result<()> {
        let initial_offset = fs::metadata(&event_stream_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        self.start_codex_fork_event_monitor_at_offset(session_id, event_stream_path, initial_offset)
    }

    fn start_codex_fork_event_monitor_at_offset(
        &self,
        session_id: String,
        event_stream_path: PathBuf,
        initial_offset: u64,
    ) -> Result<()> {
        let store = self.clone();
        let thread_session_id = format!(
            "{}-{}",
            sanitize_path_component(&session_id),
            stable_session_id_hash(&session_id)
        );
        thread::Builder::new()
            .name(format!("sm-codex-fork-events-{thread_session_id}"))
            .spawn(move || {
                store.monitor_codex_fork_event_stream(session_id, event_stream_path, initial_offset)
            })
            .with_context(|| "failed to start codex-fork event monitor")?;
        Ok(())
    }

    fn monitor_codex_fork_event_stream(
        &self,
        session_id: String,
        event_stream_path: PathBuf,
        initial_offset: u64,
    ) {
        let mut offset = initial_offset;
        let mut buffer = String::new();
        loop {
            match self.codex_fork_monitor_should_continue(&session_id) {
                Ok(true) => {}
                Ok(false) | Err(_) => return,
            }

            if let Ok(chunk) = read_file_from_offset(&event_stream_path, &mut offset) {
                for line in split_complete_event_lines(&mut buffer, &chunk) {
                    let _ = self.apply_codex_fork_event_line(&session_id, &line);
                }
            }
            thread::sleep(CODEX_FORK_EVENT_MONITOR_POLL);
        }
    }

    fn codex_fork_monitor_should_continue(&self, session_id: &str) -> Result<bool> {
        let state = self.load_raw_json_value()?;
        let Some(sessions) = state.get("sessions").and_then(Value::as_array) else {
            return Ok(false);
        };
        let Some(session) = sessions.iter().find(|session| {
            session.get("id").and_then(Value::as_str) == Some(session_id)
                || session
                    .get("aliases")
                    .and_then(Value::as_array)
                    .is_some_and(|aliases| {
                        aliases
                            .iter()
                            .any(|alias| alias.as_str() == Some(session_id))
                    })
        }) else {
            return Ok(false);
        };
        let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
        if provider != "codex-fork" {
            return Ok(false);
        }
        let status = json_text(session.get("status")).unwrap_or_else(|| "running".to_owned());
        Ok(normalized_status(&status) != "stopped")
    }

    fn apply_codex_fork_event_line(&self, session_id: &str, line: &str) -> Result<()> {
        let raw = line.trim();
        if raw.is_empty() {
            return Ok(());
        }
        let Ok(event) = serde_json::from_str::<Value>(raw) else {
            return Ok(());
        };
        let Some(event) = event.as_object() else {
            return Ok(());
        };

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(());
        };
        let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
        if provider != "codex-fork" {
            return Ok(());
        }
        let status = json_text(session.get("status")).unwrap_or_else(|| "running".to_owned());
        if normalized_status(&status) == "stopped" {
            return Ok(());
        }

        let mut changed = false;
        if let Some(provider_resume_id) = codex_fork_provider_resume_id(event) {
            if json_text(session.get("provider_resume_id")).as_deref()
                != Some(provider_resume_id.as_str())
            {
                session.insert(
                    "provider_resume_id".to_owned(),
                    Value::String(provider_resume_id),
                );
                changed = true;
            }
        }

        if let Some(next_status) = codex_fork_status_for_event(event) {
            if status != next_status {
                session.insert("status".to_owned(), Value::String(next_status.to_owned()));
            }
            let now = now_rfc3339();
            session.insert("last_activity".to_owned(), Value::String(now.clone()));
            if next_status == "stopped" {
                session.insert("stopped_at".to_owned(), Value::String(now));
            } else {
                session.insert("stopped_at".to_owned(), Value::Null);
            }
            changed = true;
        }

        if changed {
            self.write_raw_json_value(&state)?;
        }
        Ok(())
    }

    fn build_core_session_record(
        &self,
        sessions: &[Value],
        request: &CreateCoreSessionRequest,
        log_dir: Option<&Path>,
        runtime_backed: bool,
        tmux_socket_name: Option<&str>,
    ) -> Result<SessionRecord> {
        let session_id = request
            .id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(generate_session_id);
        if sessions
            .iter()
            .any(|value| value.get("id").and_then(Value::as_str) == Some(session_id.as_str()))
        {
            anyhow::bail!("session already exists: {session_id}");
        }
        let parent_session = request.parent_session_id.as_deref().and_then(|parent_id| {
            sessions
                .iter()
                .find(|value| value.get("id").and_then(Value::as_str) == Some(parent_id))
        });
        let parent_working_dir =
            parent_session.and_then(|value| json_text(value.get("working_dir")));
        let parent_node = parent_session.and_then(|value| json_text(value.get("node")));
        let provider = request
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("claude")
            .to_owned();
        let name = request
            .name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("{provider}-{session_id}"));
        let working_dir = request
            .working_dir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(parent_working_dir.as_deref())
            .unwrap_or(".")
            .to_owned();
        let node = request
            .node
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(parent_node.as_deref())
            .unwrap_or("primary")
            .to_owned();
        let now = now_rfc3339();
        let log_file = core_log_file_path(&self.state_file, log_dir, &session_id);
        Ok(SessionRecord {
            id: session_id.clone(),
            name,
            working_dir,
            tmux_session: if runtime_backed {
                core_tmux_session_name(&provider, &session_id)
            } else {
                format!("sm-rust-{session_id}")
            },
            tmux_socket_name: tmux_socket_name.map(ToOwned::to_owned),
            node,
            provider,
            model: optional_trimmed(request.model.as_deref()),
            log_file: Some(log_file.display().to_string()),
            provider_resume_id: None,
            transcript_path: None,
            codex_thread_id: None,
            forked_from_session_id: None,
            forked_from_provider_resume_id: None,
            forked_provider_resume_id: None,
            forked_at: None,
            forked_by_session_id: None,
            friendly_name: request.name.clone(),
            friendly_name_is_explicit: true,
            friendly_name_updated_at_ns: None,
            native_title: None,
            native_title_updated_at_ns: None,
            native_title_source_mtime_ns: None,
            telegram_chat_id: None,
            telegram_thread_id: None,
            telegram_topic_id: None,
            telegram_root_msg_id: None,
            current_task: None,
            git_remote_url: None,
            review_config: None,
            parent_session_id: request.parent_session_id.clone(),
            last_handoff_path: None,
            agent_status_text: None,
            agent_status_at: None,
            agent_task_completed_at: None,
            completion_status: None,
            completion_message: None,
            completed_at: None,
            stopped_at: None,
            is_em: false,
            role: None,
            status: "running".to_owned(),
            spawned_at: Some(now.clone()),
            created_at: now.clone(),
            last_activity: now,
            last_tool_call: None,
            last_tool_name: None,
            tokens_used: 0,
            context_monitor_enabled: false,
            context_monitor_notify: None,
            aliases: Vec::new(),
            pending_adoption_proposals: Vec::new(),
        })
    }
}

fn wait_for_codex_fork_provider_resume_id(
    event_stream_path: &Path,
    timeout: Duration,
) -> Result<String> {
    let started = Instant::now();
    loop {
        if let Ok(content) = fs::read_to_string(event_stream_path) {
            for line in content.lines() {
                let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                let Some(event) = event.as_object() else {
                    continue;
                };
                if let Some(provider_resume_id) = codex_fork_provider_resume_id(event) {
                    return Ok(provider_resume_id);
                }
            }
        }
        if started.elapsed() >= timeout {
            anyhow::bail!(
                "timed out waiting for codex-fork thread_started event in {}",
                event_stream_path.display()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn read_file_from_offset(path: &Path, offset: &mut u64) -> Result<String> {
    let mut file = match fs::OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to open {}", path.display()))
        }
    };
    let len = file.metadata()?.len();
    if *offset > len {
        *offset = 0;
    }
    file.seek(SeekFrom::Start(*offset))?;
    let mut chunk = String::new();
    file.read_to_string(&mut chunk)?;
    *offset = file.stream_position()?;
    Ok(chunk)
}

fn split_complete_event_lines(buffer: &mut String, chunk: &str) -> Vec<String> {
    if chunk.is_empty() {
        return Vec::new();
    }
    buffer.push_str(chunk);
    let mut lines = Vec::new();
    while let Some(index) = buffer.find('\n') {
        let line = buffer[..index].to_owned();
        buffer.drain(..=index);
        lines.push(line);
    }
    lines
}

fn codex_fork_provider_resume_id(event: &Map<String, Value>) -> Option<String> {
    extract_codex_fork_thread_started(event).or_else(|| {
        event
            .get("session_id")
            .and_then(non_unknown_json_text)
            .or_else(|| {
                codex_fork_payload(event)
                    .and_then(|payload| payload.get("session_id"))
                    .and_then(non_unknown_json_text)
            })
    })
}

fn extract_codex_fork_thread_started(event: &Map<String, Value>) -> Option<String> {
    let raw_event_type = codex_fork_event_type(event)?;
    let normalized_event_type = normalize_codex_fork_event_type(&raw_event_type.replace('/', "_"));
    if raw_event_type != "thread/started"
        && raw_event_type != "thread_started"
        && normalized_event_type != "thread_started"
    {
        return None;
    }
    let payload = codex_fork_payload(event)?;
    let thread_payload = payload
        .get("thread")
        .and_then(Value::as_object)
        .unwrap_or(payload);
    thread_payload
        .get("id")
        .and_then(non_unknown_json_text)
        .or_else(|| {
            thread_payload
                .get("thread_id")
                .and_then(non_unknown_json_text)
        })
        .or_else(|| payload.get("thread_id").and_then(non_unknown_json_text))
        .or_else(|| payload.get("session_id").and_then(non_unknown_json_text))
}

fn codex_fork_status_for_event(event: &Map<String, Value>) -> Option<&'static str> {
    let event_type = normalize_codex_fork_event_type(codex_fork_event_type(event)?.as_str());
    match event_type.as_str() {
        "turn_started" => Some("running"),
        "turn_complete" => Some("idle"),
        "turn_aborted" => {
            let reason = codex_fork_payload(event)
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_ascii_lowercase);
            if reason.as_deref() == Some("interrupted") {
                Some("running")
            } else {
                Some("idle")
            }
        }
        "approval_request"
        | "user_input_request"
        | "approval_resolved"
        | "user_input_resolved"
        | "turn_delta"
        | "turn_diff"
        | "item_started"
        | "item_completed"
        | "agent_message"
        | "exec_command_end" => Some("running"),
        "error" if codex_fork_error_will_retry(event) => Some("running"),
        "error" | "shutdown" => Some("stopped"),
        "shutdown_complete" | "stream_error" | "thread_started" | "thread_name_updated" => None,
        other if other.ends_with("_begin") || other.ends_with("_delta") => Some("running"),
        _ => None,
    }
}

fn codex_fork_error_will_retry(event: &Map<String, Value>) -> bool {
    event
        .get("willRetry")
        .or_else(|| event.get("will_retry"))
        .and_then(Value::as_bool)
        .or_else(|| {
            codex_fork_payload(event)
                .and_then(|payload| {
                    payload
                        .get("willRetry")
                        .or_else(|| payload.get("will_retry"))
                })
                .and_then(Value::as_bool)
        })
        .unwrap_or(false)
}

fn codex_fork_event_type(event: &Map<String, Value>) -> Option<String> {
    event
        .get("event_type")
        .or_else(|| event.get("type"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn codex_fork_payload(event: &Map<String, Value>) -> Option<&Map<String, Value>> {
    event.get("payload").and_then(Value::as_object)
}

fn non_unknown_json_text(value: &Value) -> Option<String> {
    let text = value.as_str()?.trim();
    if text.is_empty()
        || matches!(
            text.to_ascii_lowercase().as_str(),
            "unknown" | "none" | "null"
        )
    {
        return None;
    }
    Some(text.to_owned())
}

fn normalize_codex_fork_event_type(event_type: &str) -> String {
    let mut snake = String::new();
    let mut previous_is_separator = true;
    for ch in event_type.trim().chars() {
        if ch == '/' || ch == '-' || ch == ' ' {
            if !previous_is_separator {
                snake.push('_');
                previous_is_separator = true;
            }
            continue;
        }
        if ch.is_ascii_uppercase() {
            if !previous_is_separator && !snake.ends_with('_') {
                snake.push('_');
            }
            snake.push(ch.to_ascii_lowercase());
            previous_is_separator = false;
        } else {
            snake.push(ch);
            previous_is_separator = ch == '_';
        }
    }
    let normalized = snake.trim_matches('_');
    match normalized {
        "task_started" => "turn_started".to_owned(),
        "task_complete" | "turn_completed" => "turn_complete".to_owned(),
        "exec_approval_request" | "patch_approval_request" | "request_approval" => {
            "approval_request".to_owned()
        }
        "request_user_input" => "user_input_request".to_owned(),
        "approval_decision" | "approval_submitted" => "approval_resolved".to_owned(),
        "user_input_submitted" | "user_input_response" => "user_input_resolved".to_owned(),
        "runtime_error" | "fatal_error" => "error".to_owned(),
        _ => normalized.to_owned(),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateCoreSessionRequest {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default, alias = "prompt")]
    pub initial_message: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub wait: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartReviewRequest {
    #[serde(default = "default_review_mode")]
    pub mode: String,
    #[serde(default)]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub custom_prompt: Option<String>,
    #[serde(default, alias = "steer")]
    pub steer_text: Option<String>,
    #[serde(default)]
    pub wait: Option<u64>,
    #[serde(default)]
    pub watcher_session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpawnReviewRequest {
    pub parent_session_id: String,
    #[serde(default = "default_review_mode")]
    pub mode: String,
    #[serde(default)]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub custom_prompt: Option<String>,
    #[serde(default, alias = "steer")]
    pub steer_text: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub wait: Option<u64>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
}

fn default_review_mode() -> String {
    "branch".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendCoreInputRequest {
    pub text: String,
    #[serde(default = "default_delivery_mode")]
    pub delivery_mode: String,
    #[serde(default)]
    pub sender_session_id: Option<String>,
    #[serde(default)]
    pub from_sm_send: bool,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub notify_on_delivery: bool,
    #[serde(default)]
    pub notify_after_seconds: Option<u64>,
    #[serde(default)]
    pub notify_on_stop: bool,
    #[serde(default)]
    pub remind_soft_threshold: Option<u64>,
    #[serde(default)]
    pub remind_hard_threshold: Option<u64>,
    #[serde(default)]
    pub remind_cancel_on_reply_session_id: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendCoreInputBatchRequest {
    #[serde(flatten)]
    pub input: SendCoreInputRequest,
    #[serde(default)]
    pub recipients: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentStatusRequest {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClearSessionRequest {
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub requester_session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContextMonitorRequest {
    pub enabled: bool,
    pub requester_session_id: String,
    #[serde(default)]
    pub notify_session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HandoffRequest {
    pub requester_session_id: String,
    pub file_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetMaintainerRequest {
    pub requester_session_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoleRegistrationRequest {
    pub requester_session_id: String,
    pub role: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskCompleteRequest {
    pub requester_session_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArmStopNotifyRequest {
    pub sender_session_id: String,
    pub requester_session_id: String,
    #[serde(default)]
    pub delay_seconds: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubagentStartRequest {
    pub agent_id: String,
    pub agent_type: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubagentStopRequest {
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRegistrationResponse {
    pub role: String,
    pub session_id: String,
    pub friendly_name: Option<String>,
    pub provider: Option<String>,
    pub status: String,
    pub activity_state: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreInputResult {
    pub ok: bool,
    pub session_id: String,
    pub delivered: bool,
    pub delivery_mode: String,
    pub notify_after_seconds: Option<u64>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreInputBatchResult {
    pub identifier: String,
    pub status: String,
    pub delivery_kind: String,
    pub session_id: Option<String>,
    pub target_name: Option<String>,
    pub provider: Option<String>,
    pub bootstrapped: bool,
    pub queue_position: Option<u64>,
    pub estimated_delivery: Option<String>,
    pub email_username: Option<String>,
    pub email_address: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreInputBatchResponse {
    pub ok: bool,
    pub requested_count: usize,
    pub success_count: usize,
    pub failure_count: usize,
    pub delivery_mode: String,
    pub results: Vec<CoreInputBatchResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreReviewResult {
    pub session_id: String,
    pub review_mode: String,
    pub base_branch: Option<String>,
    pub commit_sha: Option<String>,
    pub status: String,
    pub steer_queued: bool,
    #[serde(skip)]
    pub tmux_session: String,
    #[serde(skip)]
    pub tmux_socket_name: Option<String>,
    #[serde(skip)]
    pub steer_text: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CoreReviewOutcome {
    Started(CoreReviewResult),
    NotFound,
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentResponse {
    pub agent_id: String,
    pub agent_type: String,
    pub parent_session_id: String,
    pub started_at: String,
    pub stopped_at: Option<String>,
    pub status: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentStopResult {
    pub session_id: String,
    pub agent_id: String,
    pub status: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentListResponse {
    pub session_id: String,
    pub subagents: Vec<SubagentResponse>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreRetireResult {
    pub ok: bool,
    pub session_id: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub enum CoreRetireOutcome {
    Retired(CoreRetireResult),
    NotFound,
    NotChild,
    UnsupportedNode(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreClearResult {
    pub status: String,
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub enum CoreClearOutcome {
    Cleared(CoreClearResult),
    NotFound,
    Unauthorized(String),
}

#[derive(Debug, Clone)]
pub enum CoreRestoreOutcome {
    Restored(SessionRecord),
    NotStopped,
    UnsupportedNode(String),
    UnsupportedProvider(String),
    MissingProviderResumeId(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentStatusResult {
    pub status: String,
    pub session_id: String,
    pub agent_status_text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextMonitorStatus {
    pub session_id: String,
    pub friendly_name: Option<String>,
    pub notify_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextMonitorResult {
    pub status: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub enum ContextMonitorOutcome {
    Updated(ContextMonitorResult),
    NotFound,
    MissingNotifyTarget,
    NotifyTargetNotFound(String),
    Unauthorized,
}

#[derive(Debug, Clone, Serialize)]
pub struct HandoffResult {
    pub status: String,
}

#[derive(Debug, Clone)]
pub enum HandoffOutcome {
    Recorded(HandoffResult),
    Error(String),
}

#[derive(Debug, Clone)]
pub enum RegistryMutationOutcome {
    Registered(AgentRegistrationResponse),
    NotFound,
    RoleNotRegistered,
    RoleNotOwned,
    BadRequest(String),
    Conflict(String),
}

#[derive(Debug, Clone)]
pub enum MaintainerMutationOutcome {
    Updated(SessionRecord),
    NotFound,
    BadRequest(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCompleteResult {
    pub status: String,
    pub session_id: String,
    pub em_notified: bool,
    pub agent_task_completed_at: String,
}

#[derive(Debug, Clone)]
pub enum TaskCompleteOutcome {
    Completed(TaskCompleteResult),
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnCompleteResult {
    pub status: String,
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub enum TurnCompleteOutcome {
    Completed(TurnCompleteResult),
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct ArmStopNotifyResult {
    pub status: String,
    pub session_id: String,
    pub sender_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ArmStopNotifyOutcome {
    Armed(ArmStopNotifyResult),
    Suppressed(ArmStopNotifyResult),
    NotFound,
    Forbidden(String),
    UnknownSender(String),
}

pub enum SubagentStartOutcome {
    Registered(SubagentResponse),
    NotFound,
}

pub enum SubagentStopOutcome {
    Stopped(SubagentStopResult),
    SessionNotFound,
    SubagentNotFound(String),
}

fn read_snapshot(path: &Path) -> Result<StateSnapshot> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read session state {}", path.display()))?;
    let raw: RawStateSnapshot = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse session state {}", path.display()))?;
    StateSnapshot::try_from(raw)
        .with_context(|| format!("failed to parse session records {}", path.display()))
}

fn snapshot_from_raw_value(value: &Value) -> Result<StateSnapshot> {
    let raw = serde_json::from_value::<RawStateSnapshot>(value.clone())
        .context("failed to parse raw session state")?;
    StateSnapshot::try_from(raw).context("failed to parse raw session records")
}

fn ensure_object_mut(value: &mut Value) -> Result<&mut Map<String, Value>> {
    if !value.is_object() {
        *value = json!({});
    }
    Ok(value.as_object_mut().expect("object value set above"))
}

fn ensure_sessions_array_mut(value: &mut Value) -> Result<&mut Vec<Value>> {
    let object = ensure_object_mut(value)?;
    let sessions = object
        .entry("sessions".to_owned())
        .or_insert_with(|| json!([]));
    if !sessions.is_array() {
        anyhow::bail!("session state field 'sessions' is not an array");
    }
    Ok(sessions.as_array_mut().expect("array checked above"))
}

fn ensure_subagents_array_mut(session: &mut Map<String, Value>) -> &mut Vec<Value> {
    let subagents = session
        .entry("subagents".to_owned())
        .or_insert_with(|| json!([]));
    if !subagents.is_array() {
        *subagents = json!([]);
    }
    subagents.as_array_mut().expect("array value set above")
}

fn ensure_agent_registrations_array_mut(value: &mut Value) -> Result<&mut Vec<Value>> {
    let object = ensure_object_mut(value)?;
    let registrations = object
        .entry("agent_registrations".to_owned())
        .or_insert_with(|| json!([]));
    if !registrations.is_array() {
        anyhow::bail!("session state field 'agent_registrations' is not an array");
    }
    Ok(registrations.as_array_mut().expect("array checked above"))
}

fn ensure_array_field_mut<'a>(value: &'a mut Value, field: &str) -> Result<&'a mut Vec<Value>> {
    let object = ensure_object_mut(value)?;
    let entries = object.entry(field.to_owned()).or_insert_with(|| json!([]));
    if !entries.is_array() {
        anyhow::bail!("session state field '{field}' is not an array");
    }
    Ok(entries.as_array_mut().expect("array checked above"))
}

fn active_parent_wake_parent_raw(state: &Value, child_session_id: &str) -> Result<Option<String>> {
    Ok(state
        .get("retained_parent_wake_registrations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry.get("child_session_id").and_then(Value::as_str) == Some(child_session_id)
        })
        .find(|entry| {
            entry
                .get("is_active")
                .and_then(Value::as_bool)
                .unwrap_or(true)
        })
        .and_then(|entry| json_text(entry.get("parent_session_id"))))
}

fn clear_agent_task_completed_raw(state: &mut Value, session_id: &str) -> Result<()> {
    let sessions = ensure_sessions_array_mut(state)?;
    if let Some(session) = session_object_mut(sessions, session_id) {
        session.insert("agent_task_completed_at".to_owned(), Value::Null);
    }
    Ok(())
}

fn deactivate_parent_wake_raw(state: &mut Value, child_session_id: &str) -> Result<()> {
    let registrations = ensure_array_field_mut(state, "retained_parent_wake_registrations")?;
    for entry in registrations.iter_mut().filter(|entry| {
        entry.get("child_session_id").and_then(Value::as_str) == Some(child_session_id)
    }) {
        if let Some(object) = entry.as_object_mut() {
            object.insert("is_active".to_owned(), Value::Bool(false));
            object.insert("cancelled_at".to_owned(), Value::String(now_rfc3339()));
        }
    }
    Ok(())
}

fn deactivate_remind_raw(state: &mut Value, session_id: &str) -> Result<()> {
    let registrations = ensure_array_field_mut(state, "retained_remind_registrations")?;
    for entry in registrations.iter_mut().filter(|entry| {
        entry.get("session_id").and_then(Value::as_str) == Some(session_id)
            || entry.get("target_session_id").and_then(Value::as_str) == Some(session_id)
    }) {
        if let Some(object) = entry.as_object_mut() {
            object.insert("is_active".to_owned(), Value::Bool(false));
            object.insert("cancelled_at".to_owned(), Value::String(now_rfc3339()));
        }
    }
    Ok(())
}

fn stop_notify_state_raw(state: &Value, session_id: &str) -> Option<StopNotifyState> {
    state
        .get("retained_stop_notify_states")
        .and_then(Value::as_array)?
        .iter()
        .find(|entry| entry.get("session_id").and_then(Value::as_str) == Some(session_id))
        .map(|entry| StopNotifyState {
            session_id: session_id.to_owned(),
            sender_session_id: json_text(entry.get("sender_session_id")).unwrap_or_default(),
            sender_name: json_text(entry.get("sender_name")).unwrap_or_default(),
            delay_seconds: entry
                .get("delay_seconds")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        })
        .filter(|entry| !entry.sender_session_id.is_empty())
}

fn clear_stop_notify_raw(state: &mut Value, session_id: &str) -> Result<()> {
    let entries = ensure_array_field_mut(state, "retained_stop_notify_states")?;
    entries.retain(|entry| entry.get("session_id").and_then(Value::as_str) != Some(session_id));
    Ok(())
}

fn push_retained_message_raw(
    state: &mut Value,
    target_session_id: &str,
    text: &str,
    delivery_mode: &str,
    message_category: Option<&str>,
) -> Result<()> {
    let messages = ensure_array_field_mut(state, "retained_pending_messages")?;
    messages.push(json!({
        "target_session_id": target_session_id,
        "text": text,
        "delivery_mode": delivery_mode,
        "message_category": message_category,
        "created_at": now_rfc3339(),
    }));
    Ok(())
}

fn upsert_remind_raw(
    state: &mut Value,
    target_session_id: &str,
    soft_threshold_seconds: u64,
    hard_threshold_seconds: u64,
    cancel_on_reply_session_id: Option<&str>,
) -> Result<()> {
    let registrations = ensure_array_field_mut(state, "retained_remind_registrations")?;
    let record = json!({
        "id": format!("rust-remind-{target_session_id}"),
        "session_id": target_session_id,
        "target_session_id": target_session_id,
        "soft_threshold_seconds": soft_threshold_seconds,
        "hard_threshold_seconds": hard_threshold_seconds,
        "cancel_on_reply_session_id": cancel_on_reply_session_id,
        "registered_at": now_rfc3339(),
        "last_reset_at": now_rfc3339(),
        "tracked_status_nudge_fired": false,
        "soft_fired": false,
        "persistent_tracking": false,
        "is_active": true,
    });
    if let Some(existing) = registrations.iter_mut().find(|entry| {
        entry.get("session_id").and_then(Value::as_str) == Some(target_session_id)
            || entry.get("target_session_id").and_then(Value::as_str) == Some(target_session_id)
    }) {
        *existing = record;
    } else {
        registrations.push(record);
    }
    Ok(())
}

fn upsert_parent_wake_raw(
    state: &mut Value,
    child_session_id: &str,
    parent_session_id: &str,
    period_seconds: i64,
) -> Result<()> {
    let registrations = ensure_array_field_mut(state, "retained_parent_wake_registrations")?;
    let record = json!({
        "id": format!("rust-wake-{child_session_id}"),
        "child_session_id": child_session_id,
        "parent_session_id": parent_session_id,
        "period_seconds": period_seconds,
        "registered_at": now_rfc3339(),
        "last_wake_at": null,
        "last_status_at_prev_wake": null,
        "escalated": false,
        "is_active": true,
    });
    if let Some(existing) = registrations.iter_mut().find(|entry| {
        entry.get("child_session_id").and_then(Value::as_str) == Some(child_session_id)
    }) {
        *existing = record;
    } else {
        registrations.push(record);
    }
    Ok(())
}

#[derive(Debug)]
struct QueueDrainResult {
    status: String,
    delivered_message_ids: Vec<String>,
}

fn should_persist_runtime_send(delivery_mode: &str) -> bool {
    matches!(
        normalized_delivery_mode(delivery_mode).as_str(),
        "sequential" | "important" | "urgent"
    )
}

fn normalized_delivery_mode(delivery_mode: &str) -> String {
    delivery_mode.trim().to_ascii_lowercase()
}

fn format_send_input_text_raw(
    state: &Value,
    request: &SendCoreInputRequest,
) -> (String, Option<String>) {
    let Some(sender_session_id) = optional_trimmed(request.sender_session_id.as_deref()) else {
        return (request.text.clone(), None);
    };
    let Some(sessions) = state.get("sessions").and_then(Value::as_array) else {
        return (request.text.clone(), None);
    };
    let Some(sender) = session_object(sessions, &sender_session_id) else {
        return (request.text.clone(), None);
    };
    let sender_name = raw_session_display_name(sender, &sender_session_id);
    (
        format!(
            "[Input from: {sender_name} ({}) via sm send]\n{}",
            short_session_id(&sender_session_id),
            request.text
        ),
        Some(sender_name),
    )
}

fn normalized_review_mode(mode: &str) -> String {
    let mode = mode.trim();
    if mode.is_empty() {
        "branch".to_owned()
    } else {
        mode.to_owned()
    }
}

fn review_config_value(mode: &str, request: &StartReviewRequest) -> Value {
    json!({
        "mode": mode,
        "base_branch": trimmed_value(request.base_branch.as_deref()),
        "commit_sha": trimmed_value(request.commit_sha.as_deref()),
        "custom_prompt": trimmed_value(request.custom_prompt.as_deref()),
        "steer_text": trimmed_value(request.steer_text.as_deref()),
        "steer_delivered": false,
        "dispatch_in_progress": true,
        "dispatch_completed_at": null,
        "pr_number": null,
        "pr_repo": null,
        "pr_comment_id": null
    })
}

fn review_session_is_busy(session: &Map<String, Value>, status: &str) -> bool {
    if review_dispatch_in_progress(session) {
        return true;
    }
    if normalized_status(status) != "running" {
        return false;
    }
    let Some(last_tool_call) = json_text(session.get("last_tool_call"))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    !review_dispatch_completed_after(session, &last_tool_call)
}

fn review_dispatch_in_progress(session: &Map<String, Value>) -> bool {
    session
        .get("review_config")
        .and_then(Value::as_object)
        .and_then(|config| config.get("dispatch_in_progress"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn review_dispatch_completed_after(session: &Map<String, Value>, last_tool_call: &str) -> bool {
    session
        .get("review_config")
        .and_then(Value::as_object)
        .and_then(|config| config.get("dispatch_completed_at"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some_and(|completed_at| completed_at >= last_tool_call)
}

fn mark_review_dispatch_completed(session: &mut Map<String, Value>, completed_at: &str) {
    if let Some(config) = session
        .get_mut("review_config")
        .and_then(Value::as_object_mut)
    {
        config.insert("dispatch_in_progress".to_owned(), Value::Bool(false));
        config.insert(
            "dispatch_completed_at".to_owned(),
            Value::String(completed_at.to_owned()),
        );
    }
}

fn trimmed_value(value: Option<&str>) -> Value {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| Value::String(value.to_owned()))
        .unwrap_or(Value::Null)
}

fn git_command_success<const N: usize>(working_path: &Path, args: [&str; N]) -> Result<bool> {
    let output = Command::new("git")
        .args(args)
        .current_dir(working_path)
        .output()
        .with_context(|| format!("failed to run git in {}", working_path.display()))?;
    Ok(output.status.success())
}

fn git_commit_exists(working_path: &Path, commit_sha: &str) -> Result<bool> {
    let commit_ref = format!("{commit_sha}^{{commit}}");
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(commit_ref)
        .current_dir(working_path)
        .output()
        .with_context(|| format!("failed to verify git commit in {}", working_path.display()))?;
    Ok(output.status.success())
}

fn git_branch_position(working_path: &Path, branch: &str) -> Result<Option<usize>> {
    Ok(git_branch_list(working_path)?
        .iter()
        .position(|candidate| candidate == branch))
}

fn git_branch_list(working_path: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["branch", "--list"])
        .current_dir(working_path)
        .output()
        .with_context(|| format!("failed to list git branches in {}", working_path.display()))?;
    if !output.status.success() {
        anyhow::bail!("Failed to list git branches");
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let branch = line.trim().trim_start_matches("* ").trim();
            (!branch.is_empty()).then(|| branch.to_owned())
        })
        .collect())
}

fn pending_message_from_metadata(
    target_session_id: &str,
    text: &str,
    delivery_mode: &str,
    metadata: &QueueMessageMetadata,
) -> PendingMessage {
    PendingMessage {
        id: String::new(),
        target_session_id: target_session_id.to_owned(),
        text: text.to_owned(),
        delivery_mode: delivery_mode.to_owned(),
        has_delivery_side_effects: metadata.has_delivery_side_effects(),
        sender_session_id: metadata.sender_session_id.clone(),
        sender_name: metadata.sender_name.clone(),
        from_sm_send: metadata.from_sm_send,
        notify_on_delivery: metadata.notify_on_delivery,
        notify_after_seconds: metadata.notify_after_seconds,
        notify_on_stop: metadata.notify_on_stop,
        remind_soft_threshold: metadata.remind_soft_threshold,
        remind_hard_threshold: metadata.remind_hard_threshold,
        remind_cancel_on_reply_session_id: metadata.remind_cancel_on_reply_session_id.clone(),
        parent_session_id: metadata.parent_session_id.clone(),
        message_category: metadata.message_category.clone(),
        response_relay_source: metadata
            .response_relay_source
            .clone()
            .or_else(|| metadata.from_sm_send.then(|| "sm-send".to_owned())),
    }
}

fn queue_metadata_for_send_request(
    state: &Value,
    target_session_id: &str,
    request: &SendCoreInputRequest,
    sender_name: Option<String>,
) -> QueueMessageMetadata {
    let sender_session_id = optional_trimmed(request.sender_session_id.as_deref())
        .filter(|sender_id| raw_session_object(state, sender_id).is_some());
    let has_sender = sender_session_id.is_some();
    let notify_on_stop = request.notify_on_stop
        && sender_session_id.as_deref().is_some_and(|sender_id| {
            sender_id != target_session_id
                && raw_session_is_em(state, sender_id)
                && raw_session_provider(state, target_session_id).as_deref() != Some("codex-fork")
        });
    QueueMessageMetadata {
        sender_session_id,
        sender_name,
        from_sm_send: request.from_sm_send,
        timeout_seconds: request.timeout_seconds,
        notify_on_delivery: request.notify_on_delivery && has_sender,
        notify_after_seconds: has_sender.then_some(request.notify_after_seconds).flatten(),
        notify_on_stop,
        remind_soft_threshold: request.remind_soft_threshold,
        remind_hard_threshold: request.remind_hard_threshold,
        remind_cancel_on_reply_session_id: request.remind_cancel_on_reply_session_id.clone(),
        parent_session_id: request.parent_session_id.clone(),
        message_category: None,
        response_relay_source: None,
    }
}

fn raw_session_display_name(session: &Map<String, Value>, fallback_id: &str) -> String {
    json_text(session.get("friendly_name"))
        .or_else(|| json_text(session.get("name")))
        .unwrap_or_else(|| fallback_id.to_owned())
}

fn raw_session_is_em(state: &Value, session_id: &str) -> bool {
    raw_session_object(state, session_id)
        .and_then(|session| session.get("is_em"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn raw_session_provider(state: &Value, session_id: &str) -> Option<String> {
    raw_session_object(state, session_id).and_then(|session| json_text(session.get("provider")))
}

fn raw_session_object<'a>(state: &'a Value, session_id: &str) -> Option<&'a Map<String, Value>> {
    let sessions = state.get("sessions").and_then(Value::as_array)?;
    session_object(sessions, session_id)
}

fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
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

fn runtime_session_status_raw(state: &mut Value, session_id: &str) -> Result<Option<String>> {
    let sessions = ensure_sessions_array_mut(state)?;
    let Some(session) = session_object_mut(sessions, session_id) else {
        return Ok(None);
    };
    let node = json_text(session.get("node")).unwrap_or_else(default_node);
    ensure_runtime_local_node(&node)?;
    let _tmux_session = json_text(session.get("tmux_session"))
        .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
    Ok(Some(
        json_text(session.get("status")).unwrap_or_else(|| "running".to_owned()),
    ))
}

fn deliver_runtime_text_to_session_raw(
    state: &mut Value,
    session_id: &str,
    text: &str,
    runtime: &TmuxRuntime,
) -> Result<(String, bool)> {
    let sessions = ensure_sessions_array_mut(state)?;
    let session = session_object_mut(sessions, session_id)
        .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared during delivery"))?;
    let node = json_text(session.get("node")).unwrap_or_else(default_node);
    ensure_runtime_local_node(&node)?;
    let mut status = json_text(session.get("status")).unwrap_or_else(|| "running".to_owned());
    let tmux_session = json_text(session.get("tmux_session"))
        .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
    let session_socket_name = json_text(session.get("tmux_socket_name"));
    let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
    let mut delivered = false;
    if normalized_status(&status) != "stopped" {
        match deliver_codex_fork_control_text_to_session_raw(session_id, session, text, runtime)? {
            Some(true) => {
                delivered = true;
            }
            _ => {
                delivered = session_runtime.send_input(&tmux_session, text)?;
            }
        }
        let now = now_rfc3339();
        if delivered {
            session.insert("last_activity".to_owned(), Value::String(now));
        } else {
            status = "stopped".to_owned();
            session.insert("status".to_owned(), Value::String(status.clone()));
            session.insert("stopped_at".to_owned(), Value::String(now.clone()));
            session.insert("last_activity".to_owned(), Value::String(now));
        }
    }
    Ok((status, delivered))
}

fn deliver_urgent_runtime_text_to_session_raw(
    state: &mut Value,
    session_id: &str,
    text: &str,
    runtime: &TmuxRuntime,
) -> Result<(String, bool)> {
    let sessions = ensure_sessions_array_mut(state)?;
    let session = session_object_mut(sessions, session_id)
        .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared during delivery"))?;
    let node = json_text(session.get("node")).unwrap_or_else(default_node);
    ensure_runtime_local_node(&node)?;
    let mut status = json_text(session.get("status")).unwrap_or_else(|| "running".to_owned());
    let tmux_session = json_text(session.get("tmux_session"))
        .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
    let provider = json_text(session.get("provider")).unwrap_or_else(|| "claude".to_owned());
    let session_socket_name = json_text(session.get("tmux_socket_name"));
    let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
    let mut delivered = false;
    if normalized_status(&status) != "stopped" {
        match deliver_codex_fork_control_text_to_session_raw(session_id, session, text, runtime)? {
            Some(true) => {
                delivered = true;
            }
            _ => {
                delivered = session_runtime.send_urgent_input(
                    &tmux_session,
                    text,
                    provider.eq_ignore_ascii_case("claude"),
                )?;
            }
        }
        let now = now_rfc3339();
        if delivered {
            session.insert("last_activity".to_owned(), Value::String(now));
        } else {
            status = "stopped".to_owned();
            session.insert("status".to_owned(), Value::String(status.clone()));
            session.insert("stopped_at".to_owned(), Value::String(now.clone()));
            session.insert("last_activity".to_owned(), Value::String(now));
        }
    }
    Ok((status, delivered))
}

fn deliver_codex_fork_control_text_to_session_raw(
    session_id: &str,
    session: &mut Map<String, Value>,
    text: &str,
    runtime: &TmuxRuntime,
) -> Result<Option<bool>> {
    let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
    if !provider.eq_ignore_ascii_case("codex-fork") {
        return Ok(None);
    }

    let result = codex_fork_control_socket_path_for_session_raw(session_id, session, runtime)
        .and_then(|control_socket_path| codex_fork_submit_message(&control_socket_path, text));
    match result {
        Ok(()) => {
            clear_codex_fork_control_degraded_raw(session);
            Ok(Some(true))
        }
        Err(error) => {
            mark_codex_fork_control_degraded_raw(session, &error.to_string());
            Ok(Some(false))
        }
    }
}

fn codex_fork_control_socket_path_for_session_raw(
    session_id: &str,
    session: &Map<String, Value>,
    runtime: &TmuxRuntime,
) -> Result<PathBuf> {
    let spec = codex_fork_spec_for_session_raw(session_id, session)?;
    let artifacts = runtime
        .codex_fork_runtime_artifacts(&spec)?
        .ok_or_else(|| anyhow::anyhow!("session {session_id} is not a codex-fork session"))?;
    Ok(artifacts.control_socket_path)
}

fn codex_fork_spec_for_session_raw(
    session_id: &str,
    session: &Map<String, Value>,
) -> Result<TmuxSessionSpec> {
    let tmux_session = json_text(session.get("tmux_session"))
        .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
    let working_dir = json_text(session.get("working_dir"))
        .ok_or_else(|| anyhow::anyhow!("session {session_id} missing working_dir"))?;
    let log_file = json_text(session.get("log_file"))
        .ok_or_else(|| anyhow::anyhow!("session {session_id} missing log_file"))?;
    Ok(TmuxSessionSpec {
        session_id: session_id.to_owned(),
        tmux_session,
        working_dir: expand_home(&working_dir).display().to_string(),
        log_file: expand_home(&log_file),
        provider: "codex-fork".to_owned(),
        initial_message: None,
        model: json_text(session.get("model")),
    })
}

fn codex_fork_submit_message(control_socket_path: &Path, text: &str) -> Result<()> {
    if !control_socket_path.exists() {
        return Err(anyhow::anyhow!(
            "control socket not found: {}",
            control_socket_path.display()
        ));
    }

    let mut epoch = codex_fork_refresh_control_epoch(control_socket_path)?;
    let mut response =
        codex_fork_send_control_command(control_socket_path, "submit_message", &epoch, text)?;
    if !codex_fork_response_ok(&response)
        && codex_fork_error_code(&response).as_deref() == Some("stale_epoch")
    {
        epoch = codex_fork_refresh_control_epoch(control_socket_path)?;
        response =
            codex_fork_send_control_command(control_socket_path, "submit_message", &epoch, text)?;
    }
    ensure_codex_fork_response_ok(&response, "control command failed")
}

fn codex_fork_refresh_control_epoch(control_socket_path: &Path) -> Result<String> {
    let request = json!({
        "request_id": codex_fork_control_request_id(),
        "command": "get_epoch",
    });
    let response = codex_fork_control_roundtrip(control_socket_path, &request)
        .with_context(|| "failed to read control epoch")?;
    ensure_codex_fork_response_ok(&response, "failed to fetch epoch")?;
    codex_fork_response_epoch(&response)
        .ok_or_else(|| anyhow::anyhow!("control epoch missing from response"))
}

fn codex_fork_send_control_command(
    control_socket_path: &Path,
    command: &str,
    expected_epoch: &str,
    message: &str,
) -> Result<Value> {
    let request = json!({
        "request_id": codex_fork_control_request_id(),
        "expected_epoch": expected_epoch,
        "command": command,
        "message": message,
    });
    codex_fork_control_roundtrip(control_socket_path, &request)
        .with_context(|| "control command failed")
}

#[cfg(unix)]
fn codex_fork_control_roundtrip(control_socket_path: &Path, request: &Value) -> Result<Value> {
    let mut stream = UnixStream::connect(control_socket_path).with_context(|| {
        format!(
            "failed to connect control socket {}",
            control_socket_path.display()
        )
    })?;
    stream
        .set_read_timeout(Some(CODEX_FORK_CONTROL_TIMEOUT))
        .with_context(|| "failed to set control socket read timeout")?;
    stream
        .set_write_timeout(Some(CODEX_FORK_CONTROL_TIMEOUT))
        .with_context(|| "failed to set control socket write timeout")?;
    let mut raw_request = serde_json::to_string(request)?;
    raw_request.push('\n');
    stream
        .write_all(raw_request.as_bytes())
        .with_context(|| "failed to write control socket request")?;
    stream
        .flush()
        .with_context(|| "failed to flush control socket request")?;

    let mut reader = BufReader::new(stream);
    let mut raw_response = String::new();
    reader
        .read_line(&mut raw_response)
        .with_context(|| "failed to read control socket response")?;
    if raw_response.is_empty() {
        return Err(anyhow::anyhow!("control socket closed without response"));
    }
    serde_json::from_str(&raw_response).with_context(|| "control socket returned invalid JSON")
}

#[cfg(not(unix))]
fn codex_fork_control_roundtrip(_control_socket_path: &Path, _request: &Value) -> Result<Value> {
    Err(anyhow::anyhow!(
        "codex-fork control sockets are only supported on Unix"
    ))
}

fn ensure_codex_fork_response_ok(response: &Value, default_message: &str) -> Result<()> {
    if codex_fork_response_ok(response) {
        return Ok(());
    }
    let code = codex_fork_error_code(response).unwrap_or_else(|| "unknown_error".to_owned());
    let message = codex_fork_error_message(response).unwrap_or_else(|| default_message.to_owned());
    Err(anyhow::anyhow!("{code}: {message}"))
}

fn codex_fork_response_ok(response: &Value) -> bool {
    response.get("ok").and_then(Value::as_bool).unwrap_or(false)
}

fn codex_fork_response_epoch(response: &Value) -> Option<String> {
    response
        .get("result")
        .and_then(Value::as_object)
        .and_then(|result| json_text(result.get("epoch")))
        .or_else(|| json_text(response.get("epoch")))
}

fn codex_fork_error_code(response: &Value) -> Option<String> {
    response
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| json_text(error.get("code")))
}

fn codex_fork_error_message(response: &Value) -> Option<String> {
    response
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| json_text(error.get("message")))
}

fn codex_fork_control_request_id() -> String {
    let counter = STATE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("rust-{}-{counter}", std::process::id())
}

fn mark_codex_fork_control_degraded_raw(session: &mut Map<String, Value>, reason: &str) {
    let reason = reason.trim();
    let reason = if reason.is_empty() {
        "unknown_control_error"
    } else {
        reason
    };
    session.insert(
        "error_message".to_owned(),
        Value::String(format!("codex_fork_control_degraded: {reason}")),
    );
}

fn clear_codex_fork_control_degraded_raw(session: &mut Map<String, Value>) {
    let is_degraded_error = json_text(session.get("error_message"))
        .as_deref()
        .is_some_and(|message| message.starts_with("codex_fork_control_degraded:"));
    if is_degraded_error {
        session.insert("error_message".to_owned(), Value::Null);
    }
}

fn drain_pending_runtime_messages_raw(
    store: &SessionStore,
    state: &mut Value,
    session_id: &str,
    runtime: &TmuxRuntime,
    queue: &RetainedQueueStore,
    delivery_mode_filter: Option<&str>,
    stop_after_message_id: Option<&str>,
) -> Result<QueueDrainResult> {
    let mut status =
        runtime_session_status_raw(state, session_id)?.unwrap_or_else(|| "stopped".to_owned());
    let mut delivered_message_ids = Vec::new();
    loop {
        let messages = match delivery_mode_filter {
            Some(delivery_mode) => {
                queue.pending_messages_for_target_by_mode(session_id, delivery_mode, 10)?
            }
            None => queue.pending_messages_for_target(session_id, 10)?,
        };
        if messages.is_empty() {
            break;
        }

        let mut should_continue = true;
        for message in messages {
            let (next_status, delivered) =
                if normalized_delivery_mode(&message.delivery_mode) == "urgent" {
                    deliver_urgent_runtime_text_to_session_raw(
                        state,
                        session_id,
                        &message.text,
                        runtime,
                    )?
                } else {
                    deliver_runtime_text_to_session_raw(state, session_id, &message.text, runtime)?
                };
            status = next_status;
            if !delivered {
                should_continue = false;
                break;
            }
            complete_runtime_message_delivery_raw(store, state, runtime, queue, &message)?;
            let delivered_target =
                stop_after_message_id.is_some_and(|target_id| target_id == message.id);
            delivered_message_ids.push(message.id);
            if delivered_target {
                should_continue = false;
                break;
            }
        }

        if !should_continue {
            break;
        }
    }
    Ok(QueueDrainResult {
        status,
        delivered_message_ids,
    })
}

fn complete_runtime_message_delivery_raw(
    store: &SessionStore,
    state: &mut Value,
    runtime: &TmuxRuntime,
    queue: &RetainedQueueStore,
    message: &PendingMessage,
) -> Result<()> {
    let sanitized_message;
    let message = if message
        .sender_session_id
        .as_deref()
        .is_some_and(|sender_id| raw_session_object(state, sender_id).is_none())
    {
        sanitized_message = PendingMessage {
            sender_session_id: None,
            sender_name: None,
            notify_on_delivery: false,
            notify_after_seconds: None,
            notify_on_stop: false,
            ..message.clone()
        };
        &sanitized_message
    } else {
        message
    };

    queue.mark_delivered_and_apply_side_effects(message)?;

    if message.notify_on_delivery {
        if let Some(sender_session_id) = message.sender_session_id.as_deref() {
            push_retained_message_raw(
                state,
                sender_session_id,
                &runtime_delivery_notification_text(message),
                "sequential",
                None,
            )?;
        }
    }

    if message.notify_on_stop {
        if let Some(sender_session_id) = message.sender_session_id.as_deref() {
            upsert_stop_notify_raw(
                state,
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
        upsert_remind_raw(
            state,
            &message.target_session_id,
            soft_threshold,
            hard_threshold,
            message.remind_cancel_on_reply_session_id.as_deref(),
        )?;
        if let Some(parent_session_id) = message.parent_session_id.as_deref() {
            upsert_parent_wake_raw(state, &message.target_session_id, parent_session_id, 600)?;
        }
    }

    if message.notify_on_delivery {
        if let Some(sender_session_id) = message.sender_session_id.as_deref() {
            drain_pending_runtime_messages_raw(
                store,
                state,
                sender_session_id,
                runtime,
                queue,
                Some("sequential"),
                None,
            )?;
        }
    }

    schedule_runtime_followup_notification(
        store.clone(),
        runtime.clone(),
        queue.clone(),
        message.clone(),
    );
    Ok(())
}

fn runtime_delivery_notification_text(message: &PendingMessage) -> String {
    let truncated = truncate_chars(&message.text, 100);
    format!(
        "[sm] Message delivered to {}\nOriginal: \"{}\"",
        message.target_session_id, truncated
    )
}

fn schedule_runtime_followup_notification(
    store: SessionStore,
    runtime: TmuxRuntime,
    queue: RetainedQueueStore,
    message: PendingMessage,
) {
    let Some(sender_session_id) = message.sender_session_id.clone() else {
        return;
    };
    let Some(seconds) = message.notify_after_seconds else {
        return;
    };
    if seconds == 0 {
        return;
    }
    let Some(text) = followup_notification_text(&message) else {
        return;
    };
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            if queue
                .enqueue_message(&sender_session_id, &text, "sequential", None)
                .is_ok()
            {
                let _ =
                    store.drain_runtime_pending_messages_for_session(&sender_session_id, &runtime);
            }
        });
    }
}

fn complete_stop_notify_after_stop_raw(
    store: &SessionStore,
    state: &mut Value,
    runtime: Option<&TmuxRuntime>,
    session_id: &str,
    recipient_name: &str,
) -> Result<()> {
    let queue = store.queue_store.as_ref();
    let stop_notify = match queue {
        Some(queue) => queue.stop_notify_state(session_id)?,
        None => stop_notify_state_raw(state, session_id),
    };
    let Some(stop_notify) = stop_notify else {
        return Ok(());
    };

    if let Some(queue) = queue {
        queue.clear_stop_notify(session_id)?;
    }
    clear_stop_notify_raw(state, session_id)?;

    if raw_session_object(state, &stop_notify.sender_session_id).is_none() {
        return Ok(());
    }

    let text = runtime_stop_notification_text(recipient_name, session_id);
    if stop_notify.delay_seconds > 0 {
        schedule_stop_notification(
            store.clone(),
            runtime.cloned(),
            stop_notify.sender_session_id,
            text,
            stop_notify.delay_seconds as u64,
        );
        return Ok(());
    }

    if let Some(queue) = queue {
        enqueue_stop_notification_raw(
            store,
            state,
            runtime,
            queue,
            &stop_notify.sender_session_id,
            &text,
        )?;
    } else {
        push_retained_message_raw(
            state,
            &stop_notify.sender_session_id,
            &text,
            "important",
            Some("stop_notify"),
        )?;
    }
    Ok(())
}

fn enqueue_stop_notification_raw(
    store: &SessionStore,
    state: &mut Value,
    runtime: Option<&TmuxRuntime>,
    queue: &RetainedQueueStore,
    sender_session_id: &str,
    text: &str,
) -> Result<()> {
    if raw_session_object(state, sender_session_id).is_none() {
        return Ok(());
    }
    queue.enqueue_message(sender_session_id, text, "important", Some("stop_notify"))?;
    push_retained_message_raw(
        state,
        sender_session_id,
        text,
        "important",
        Some("stop_notify"),
    )?;
    if let Some(runtime) = runtime {
        drain_pending_runtime_messages_raw(
            store,
            state,
            sender_session_id,
            runtime,
            queue,
            Some("important"),
            None,
        )?;
    }
    Ok(())
}

fn schedule_stop_notification(
    store: SessionStore,
    runtime: Option<TmuxRuntime>,
    sender_session_id: String,
    text: String,
    delay_seconds: u64,
) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_secs(delay_seconds)).await;
            let _ = store.enqueue_stop_notification_for_session(
                &sender_session_id,
                &text,
                runtime.as_ref(),
            );
        });
    }
}

fn runtime_stop_notification_text(recipient_name: &str, recipient_session_id: &str) -> String {
    format!(
        "[sm] {} ({}) completed (Stop hook fired)",
        recipient_name,
        short_session_id(recipient_session_id)
    )
}

fn deliver_urgent_runtime_message_raw(
    store: &SessionStore,
    state: &mut Value,
    session_id: &str,
    runtime: &TmuxRuntime,
    queue: &RetainedQueueStore,
    message: &PendingMessage,
) -> Result<QueueDrainResult> {
    let (status, delivered) =
        deliver_urgent_runtime_text_to_session_raw(state, session_id, &message.text, runtime)?;
    let mut delivered_message_ids = Vec::new();
    if delivered {
        complete_runtime_message_delivery_raw(store, state, runtime, queue, message)?;
        delivered_message_ids.push(message.id.clone());
    }
    Ok(QueueDrainResult {
        status,
        delivered_message_ids,
    })
}

fn upsert_stop_notify_raw(
    state: &mut Value,
    session_id: &str,
    sender_session_id: &str,
    sender_name: &str,
    delay_seconds: i64,
) -> Result<()> {
    let entries = ensure_array_field_mut(state, "retained_stop_notify_states")?;
    let record = json!({
        "session_id": session_id,
        "sender_session_id": sender_session_id,
        "sender_name": sender_name,
        "delay_seconds": delay_seconds,
        "armed_at": now_rfc3339(),
    });
    if let Some(existing) = entries
        .iter_mut()
        .find(|entry| entry.get("session_id").and_then(Value::as_str) == Some(session_id))
    {
        *existing = record;
    } else {
        entries.push(record);
    }
    Ok(())
}

fn raw_registration_record(value: &Value) -> Option<AgentRegistrationRecord> {
    let role = value
        .get("role")
        .and_then(Value::as_str)
        .map(normalize_role)
        .filter(|role| !role.is_empty())?;
    let session_id = value
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())?
        .to_owned();
    let created_at = value
        .get("created_at")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|created_at| !created_at.is_empty())
        .map(ToOwned::to_owned);
    Some(AgentRegistrationRecord {
        role,
        session_id,
        created_at,
    })
}

fn find_raw_registration(state: &Value, role: &str) -> Result<Option<AgentRegistrationRecord>> {
    let normalized_role = normalize_role(role);
    let registrations = state
        .get("agent_registrations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(registrations
        .iter()
        .filter_map(raw_registration_record)
        .find(|registration| registration.role == normalized_role))
}

fn upsert_raw_registration(
    state: &mut Value,
    role: &str,
    session_id: &str,
    created_at: &str,
) -> Result<()> {
    let normalized_role = normalize_role(role);
    let registrations = ensure_agent_registrations_array_mut(state)?;
    if let Some(existing) = registrations.iter_mut().find(|entry| {
        entry
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|value| normalize_role(value) == normalized_role)
    }) {
        *existing = json!({
            "role": normalized_role,
            "session_id": session_id,
            "created_at": created_at,
        });
        return Ok(());
    }
    registrations.push(json!({
        "role": normalized_role,
        "session_id": session_id,
        "created_at": created_at,
    }));
    Ok(())
}

fn remove_raw_registration(state: &mut Value, role: &str) -> Result<()> {
    let normalized_role = normalize_role(role);
    let registrations = ensure_agent_registrations_array_mut(state)?;
    registrations.retain(|entry| {
        !entry
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|value| normalize_role(value) == normalized_role)
    });
    Ok(())
}

fn remember_role_last_session_raw(state: &mut Value, role: &str, session_id: &str) -> Result<()> {
    if role.is_empty() || session_id.trim().is_empty() {
        return Ok(());
    }
    let object = ensure_object_mut(state)?;
    let last = object
        .entry("agent_role_last_session_ids".to_owned())
        .or_insert_with(|| json!({}));
    if !last.is_object() {
        *last = json!({});
    }
    last.as_object_mut()
        .expect("object value set above")
        .insert(role.to_owned(), Value::String(session_id.to_owned()));
    Ok(())
}

fn forget_role_last_session_raw(state: &mut Value, role: &str) -> Result<()> {
    let normalized_role = normalize_role(role);
    if normalized_role.is_empty() {
        return Ok(());
    }
    if let Some(last) = ensure_object_mut(state)?
        .get_mut("agent_role_last_session_ids")
        .and_then(Value::as_object_mut)
    {
        last.remove(&normalized_role);
    }
    Ok(())
}

fn sync_maintainer_alias_raw(state: &mut Value) -> Result<()> {
    let maintainer = find_raw_registration(state, "maintainer")?;
    let object = ensure_object_mut(state)?;
    object.insert(
        "maintainer_session_id".to_owned(),
        maintainer
            .map(|registration| Value::String(registration.session_id))
            .unwrap_or(Value::Null),
    );
    Ok(())
}

fn recover_missing_maintainer_registration_raw(state: &mut Value) -> Result<bool> {
    if find_raw_registration(state, "maintainer")?.is_some() {
        return Ok(false);
    }
    let sessions = snapshot_from_raw_value(state)?.into_sessions();
    let mut candidates = Vec::<(String, bool)>::new();
    let legacy_maintainer_session_id = state
        .get("maintainer_session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    if let Some(session_id) = legacy_maintainer_session_id.as_deref() {
        candidates.push((session_id.to_owned(), true));
    }
    if let Some(session_id) = state
        .get("agent_role_last_session_ids")
        .and_then(Value::as_object)
        .and_then(|last| last.get("maintainer"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        candidates.push((session_id.to_owned(), false));
    }

    for (session_id, from_legacy_field) in candidates {
        let Some(session) = sessions.iter().find(|session| session.id == session_id) else {
            continue;
        };
        if !session.is_restorable_for_registry() {
            continue;
        }
        if !from_legacy_field && !session_has_maintainer_identity(session) {
            continue;
        }
        upsert_raw_registration(state, "maintainer", &session_id, &now_rfc3339())?;
        remember_role_last_session_raw(state, "maintainer", &session_id)?;
        sync_maintainer_alias_raw(state)?;
        return Ok(true);
    }
    if let Some(session_id) = legacy_maintainer_session_id {
        remember_role_last_session_raw(state, "maintainer", &session_id)?;
        sync_maintainer_alias_raw(state)?;
        return Ok(true);
    }
    Ok(false)
}

fn session_has_maintainer_identity(session: &SessionRecord) -> bool {
    [session.friendly_name.as_deref(), session.role.as_deref()]
        .into_iter()
        .flatten()
        .any(|value| value.trim().eq_ignore_ascii_case("maintainer"))
}

fn prune_agent_registrations_raw(state: &mut Value) -> Result<bool> {
    let restorable_session_ids = snapshot_from_raw_value(state)?
        .into_sessions()
        .into_iter()
        .filter(SessionRecord::is_restorable_for_registry)
        .map(|session| session.id)
        .collect::<BTreeSet<_>>();
    let mut removed = Vec::<AgentRegistrationRecord>::new();
    {
        let registrations = ensure_agent_registrations_array_mut(state)?;
        registrations.retain(|entry| {
            let Some(registration) = raw_registration_record(entry) else {
                return false;
            };
            if restorable_session_ids.contains(&registration.session_id) {
                return true;
            }
            removed.push(registration);
            false
        });
    }
    for registration in &removed {
        remember_role_last_session_raw(state, &registration.role, &registration.session_id)?;
    }
    if !removed.is_empty() {
        sync_maintainer_alias_raw(state)?;
    }
    Ok(!removed.is_empty())
}

fn agent_registration_responses_from_state(
    state: &Value,
) -> Result<Vec<AgentRegistrationResponse>> {
    let sessions = snapshot_from_raw_value(state)?.into_sessions();
    let sessions_by_id = sessions
        .into_iter()
        .map(|session| (session.id.clone(), session))
        .collect::<BTreeMap<_, _>>();
    let mut responses = state
        .get("agent_registrations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(raw_registration_record)
        .filter_map(|registration| {
            let session = sessions_by_id.get(&registration.session_id)?;
            Some(agent_registration_response(
                &registration.role,
                session,
                registration.created_at.as_deref(),
            ))
        })
        .collect::<Vec<_>>();
    responses.sort_by(|left, right| left.role.cmp(&right.role));
    Ok(responses)
}

fn agent_registration_response(
    role: &str,
    session: &SessionRecord,
    created_at: Option<&str>,
) -> AgentRegistrationResponse {
    let status = normalized_status(&session.status);
    AgentRegistrationResponse {
        role: normalize_role(role),
        session_id: session.id.clone(),
        friendly_name: session.cached_display_name(),
        provider: Some(non_empty_or(session.provider.clone(), "claude")),
        status: status.to_owned(),
        activity_state: fallback_activity_state(status),
        created_at: created_at
            .map(ToOwned::to_owned)
            .unwrap_or_else(now_rfc3339),
    }
}

fn session_object_mut<'a>(
    sessions: &'a mut [Value],
    session_id: &str,
) -> Option<&'a mut Map<String, Value>> {
    sessions.iter_mut().find_map(|value| {
        if value.get("id").and_then(Value::as_str) == Some(session_id) {
            value.as_object_mut()
        } else {
            None
        }
    })
}

fn session_object<'a>(sessions: &'a [Value], session_id: &str) -> Option<&'a Map<String, Value>> {
    sessions.iter().find_map(|value| {
        if value.get("id").and_then(Value::as_str) == Some(session_id) {
            value.as_object()
        } else {
            None
        }
    })
}

fn direct_children(sessions: &[SessionRecord], parent_session_id: &str) -> Vec<SessionRecord> {
    sessions
        .iter()
        .filter(|session| session.parent_session_id.as_deref() == Some(parent_session_id))
        .cloned()
        .collect()
}

fn collect_descendants_preorder(
    sessions: &[SessionRecord],
    parent_session_id: &str,
    visited: &mut BTreeSet<String>,
    descendants: &mut Vec<SessionRecord>,
) {
    for child in direct_children(sessions, parent_session_id) {
        if !visited.insert(child.id.clone()) {
            continue;
        }
        let child_id = child.id.clone();
        descendants.push(child);
        collect_descendants_preorder(sessions, &child_id, visited, descendants);
    }
}

fn reset_session_after_clear(session: &mut Map<String, Value>, now: &str) {
    session.insert("agent_status_text".to_owned(), Value::Null);
    session.insert("agent_status_at".to_owned(), Value::Null);
    session.insert("agent_task_completed_at".to_owned(), Value::Null);
    session.insert("completion_status".to_owned(), Value::Null);
    session.insert("completion_message".to_owned(), Value::Null);
    session.insert("completed_at".to_owned(), Value::Null);
    session.insert("last_activity".to_owned(), Value::String(now.to_owned()));
    mark_review_dispatch_completed(session, now);
}

fn mark_session_killed(session: &mut Map<String, Value>, now: &str) {
    session.insert(
        "completion_status".to_owned(),
        Value::String("killed".to_owned()),
    );
    session.insert(
        "completion_message".to_owned(),
        Value::String("Terminated via sm kill".to_owned()),
    );
    session.insert("completed_at".to_owned(), Value::String(now.to_owned()));
}

fn clear_authorization_error(
    session: &Map<String, Value>,
    requester_session_id: Option<&str>,
) -> Option<String> {
    let parent_id = json_text(session.get("parent_session_id"));
    let requester_session_id = requester_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(requester_session_id) = requester_session_id {
        if parent_id.as_deref() != Some(requester_session_id) {
            return Some(format!(
                "Not authorized. You can only clear your child sessions. Target session parent: {}",
                parent_id.as_deref().unwrap_or("none")
            ));
        }
    } else if parent_id.is_none() {
        return Some("Can only clear child sessions. Target session has no parent.".to_owned());
    }
    None
}

fn json_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn subagent_response_from_value(value: &Value) -> Result<SubagentResponse> {
    Ok(SubagentResponse {
        agent_id: json_text(value.get("agent_id")).unwrap_or_default(),
        agent_type: json_text(value.get("agent_type")).unwrap_or_else(|| "unknown".to_owned()),
        parent_session_id: json_text(value.get("parent_session_id")).unwrap_or_default(),
        started_at: json_text(value.get("started_at")).unwrap_or_default(),
        stopped_at: json_text(value.get("stopped_at")),
        status: json_text(value.get("status")).unwrap_or_else(|| "running".to_owned()),
        summary: json_text(value.get("summary")),
    })
}

fn append_log_line(path: &Path, line: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open session log {}", path.display()))?;
    writeln!(file, "{line}")
        .with_context(|| format!("failed to append session log {}", path.display()))?;
    Ok(())
}

fn core_log_file_path(state_file: &Path, log_dir: Option<&Path>, session_id: &str) -> PathBuf {
    let safe_id = sanitize_path_component(session_id);
    let id_hash = stable_session_id_hash(session_id);
    let dir = log_dir
        .map(Path::to_path_buf)
        .or_else(|| state_file.parent().map(|parent| parent.join("logs")))
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join(format!("{safe_id}-{id_hash}.log"))
}

fn core_tmux_session_name(provider: &str, session_id: &str) -> String {
    let safe_provider = sanitize_path_component(provider);
    let safe_id = sanitize_path_component(session_id);
    let id_hash = stable_session_id_hash(session_id);
    format!("sm-rust-{safe_provider}-{safe_id}-{id_hash}")
}

fn sanitize_path_component(value: &str) -> String {
    let mut safe = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .collect::<String>();
    if safe.is_empty() {
        safe = "session".to_owned();
    }
    safe
}

fn stable_session_id_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut hash = String::with_capacity(12);
    for byte in &digest[..6] {
        hash.push(hex_char(byte >> 4));
        hash.push(hex_char(byte & 0x0f));
    }
    hash
}

fn hex_char(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'a' + (value - 10)),
        _ => unreachable!("hex nibble out of range"),
    }
}

fn generate_session_id() -> String {
    let nanos = OffsetDateTime::now_utc().unix_timestamp_nanos();
    format!("rs{:x}", nanos as u128)
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn now_python_naive_iso() -> String {
    let now_utc = OffsetDateTime::now_utc();
    let local = OffsetDateTime::now_local().unwrap_or(now_utc);
    local
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]"
        ))
        .unwrap_or_else(|_| "1970-01-01T00:00:00.000000".to_owned())
}

pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path));
    }
    let Some(rest) = path.strip_prefix("~/") else {
        return PathBuf::from(path);
    };
    match env::var_os("HOME") {
        Some(home) => Path::new(&home).join(rest),
        None => PathBuf::from(path),
    }
}

#[derive(Debug, Default, Deserialize)]
struct StateSnapshot {
    #[serde(default)]
    sessions: Vec<SessionRecord>,
    #[serde(default)]
    maintainer_session_id: Option<String>,
    #[serde(default)]
    agent_registrations: Vec<AgentRegistrationRecord>,
    #[serde(default)]
    adoption_proposals: Vec<AdoptionProposalRecord>,
}

#[derive(Debug, Default, Deserialize)]
struct RawStateSnapshot {
    #[serde(default)]
    sessions: Vec<Value>,
    #[serde(default)]
    maintainer_session_id: Option<String>,
    #[serde(default)]
    agent_registrations: Vec<AgentRegistrationRecord>,
    #[serde(default)]
    adoption_proposals: Vec<AdoptionProposalRecord>,
}

impl TryFrom<RawStateSnapshot> for StateSnapshot {
    type Error = serde_json::Error;

    fn try_from(raw: RawStateSnapshot) -> std::result::Result<Self, Self::Error> {
        let mut sessions = Vec::new();
        for raw_session in raw.sessions {
            if is_legacy_codex_app_record(&raw_session) {
                continue;
            }
            sessions.push(serde_json::from_value(raw_session)?);
        }
        Ok(Self {
            sessions,
            maintainer_session_id: raw.maintainer_session_id,
            agent_registrations: raw.agent_registrations,
            adoption_proposals: raw.adoption_proposals,
        })
    }
}

impl StateSnapshot {
    fn into_sessions(mut self) -> Vec<SessionRecord> {
        let alias_map = self.alias_map();
        for session in &mut self.sessions {
            session.aliases = alias_map
                .get(&session.id)
                .map(|aliases| aliases.iter().cloned().collect())
                .unwrap_or_default();
        }

        let proposer_names = self
            .sessions
            .iter()
            .map(|session| {
                (
                    session.id.clone(),
                    session
                        .cached_display_name()
                        .unwrap_or_else(|| non_empty_or(session.name.clone(), &session.id)),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut proposal_map = self.pending_proposal_map(&proposer_names);
        for session in &mut self.sessions {
            session.pending_adoption_proposals =
                proposal_map.remove(&session.id).unwrap_or_default();
        }

        self.sessions
    }

    fn alias_map(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut aliases = BTreeMap::<String, BTreeSet<String>>::new();
        if let Some(session_id) = self
            .maintainer_session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|session_id| self.session_is_restorable_for_registry(session_id))
        {
            aliases
                .entry(session_id.to_owned())
                .or_default()
                .insert("maintainer".to_owned());
        }
        for registration in &self.agent_registrations {
            let role = normalize_role(&registration.role);
            let session_id = registration.session_id.trim();
            if role.is_empty() || session_id.is_empty() {
                continue;
            }
            if !self.session_is_restorable_for_registry(session_id) {
                continue;
            }
            aliases
                .entry(session_id.to_owned())
                .or_default()
                .insert(role);
        }
        aliases
    }

    fn session_is_restorable_for_registry(&self, session_id: &str) -> bool {
        self.sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(SessionRecord::is_restorable_for_registry)
    }

    fn pending_proposal_map(
        &self,
        proposer_names: &BTreeMap<String, String>,
    ) -> BTreeMap<String, Vec<AdoptionProposalResponse>> {
        let mut proposal_map = BTreeMap::<String, Vec<AdoptionProposalResponse>>::new();
        for proposal in &self.adoption_proposals {
            if proposal.status != "pending" {
                continue;
            }
            proposal_map
                .entry(proposal.target_session_id.clone())
                .or_default()
                .push(AdoptionProposalResponse {
                    id: proposal.id.clone(),
                    proposer_session_id: proposal.proposer_session_id.clone(),
                    proposer_name: proposer_names.get(&proposal.proposer_session_id).cloned(),
                    target_session_id: proposal.target_session_id.clone(),
                    created_at: proposal.created_at.clone(),
                    status: proposal.status.clone(),
                    decided_at: proposal.decided_at.clone(),
                });
        }
        for proposals in proposal_map.values_mut() {
            proposals.sort_by(|left, right| {
                (&left.created_at, &left.id).cmp(&(&right.created_at, &right.id))
            });
        }
        proposal_map
    }
}

fn is_legacy_codex_app_record(value: &Value) -> bool {
    let Some(record) = value.as_object() else {
        return false;
    };
    let provider = record.get("provider").and_then(Value::as_str);
    if provider != Some("codex") {
        return false;
    }
    let has_codex_thread_id = record
        .get("codex_thread_id")
        .is_some_and(|value| !value.is_null());
    let has_tmux_session = record
        .get("tmux_session")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    let has_log_file = record
        .get("log_file")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    has_codex_thread_id || (!has_tmux_session && !has_log_file)
}

#[derive(Debug, Clone, Deserialize)]
struct AgentRegistrationRecord {
    role: String,
    session_id: String,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AdoptionProposalRecord {
    id: String,
    proposer_session_id: String,
    target_session_id: String,
    created_at: String,
    status: String,
    #[serde(default)]
    decided_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionRecord {
    pub id: String,
    pub name: String,
    pub working_dir: String,
    pub tmux_session: String,
    #[serde(default)]
    pub tmux_socket_name: Option<String>,
    #[serde(default = "default_node")]
    pub node: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub log_file: Option<String>,
    #[serde(default)]
    pub provider_resume_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub codex_thread_id: Option<String>,
    #[serde(default)]
    pub forked_from_session_id: Option<String>,
    #[serde(default)]
    pub forked_from_provider_resume_id: Option<String>,
    #[serde(default)]
    pub forked_provider_resume_id: Option<String>,
    #[serde(default)]
    pub forked_at: Option<String>,
    #[serde(default)]
    pub forked_by_session_id: Option<String>,
    #[serde(default)]
    pub friendly_name: Option<String>,
    #[serde(default)]
    pub friendly_name_is_explicit: bool,
    #[serde(default)]
    pub friendly_name_updated_at_ns: Option<i64>,
    #[serde(default)]
    pub native_title: Option<String>,
    #[serde(default)]
    pub native_title_updated_at_ns: Option<i64>,
    #[serde(default)]
    pub native_title_source_mtime_ns: Option<i64>,
    #[serde(default)]
    pub telegram_chat_id: Option<i64>,
    #[serde(default)]
    pub telegram_thread_id: Option<i64>,
    #[serde(default)]
    pub telegram_topic_id: Option<i64>,
    #[serde(default)]
    pub telegram_root_msg_id: Option<i64>,
    #[serde(default)]
    pub current_task: Option<String>,
    #[serde(default)]
    pub git_remote_url: Option<String>,
    #[serde(default)]
    pub review_config: Option<Value>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub last_handoff_path: Option<String>,
    #[serde(default)]
    pub agent_status_text: Option<String>,
    #[serde(default)]
    pub agent_status_at: Option<String>,
    #[serde(default)]
    pub agent_task_completed_at: Option<String>,
    #[serde(default)]
    pub completion_status: Option<String>,
    #[serde(default)]
    pub completion_message: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub stopped_at: Option<String>,
    #[serde(default)]
    pub is_em: bool,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub spawned_at: Option<String>,
    pub created_at: String,
    pub last_activity: String,
    #[serde(default)]
    pub last_tool_call: Option<String>,
    #[serde(default)]
    pub last_tool_name: Option<String>,
    #[serde(default)]
    pub tokens_used: i64,
    #[serde(default)]
    pub context_monitor_enabled: bool,
    #[serde(default)]
    pub context_monitor_notify: Option<String>,
    #[serde(skip)]
    pub aliases: Vec<String>,
    #[serde(skip)]
    pub pending_adoption_proposals: Vec<AdoptionProposalResponse>,
}

impl SessionRecord {
    fn is_stopped(&self) -> bool {
        normalized_status(&self.status) == "stopped"
    }

    fn is_restorable_for_registry(&self) -> bool {
        if !self.is_stopped() {
            return true;
        }
        let has_provider_resume_id = has_text(self.provider_resume_id.as_deref());
        match self.provider.as_str() {
            "claude" => has_provider_resume_id || has_text(self.transcript_path.as_deref()),
            "codex-app" => has_provider_resume_id || has_text(self.codex_thread_id.as_deref()),
            "codex" | "codex-fork" => has_provider_resume_id,
            _ => has_provider_resume_id,
        }
    }

    pub(crate) fn cached_display_name(&self) -> Option<String> {
        if let Some(alias) = self.aliases.first() {
            return Some(alias.clone());
        }
        let native_title = self.native_title.as_deref().filter(|value| {
            matches!(
                self.provider.as_str(),
                "claude" | "codex" | "codex-app" | "codex-fork"
            ) && !value.trim().is_empty()
        });
        let friendly_name = self
            .friendly_name
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let friendly_name_updated_at_ns = self.friendly_name_updated_at_ns.unwrap_or(0);
        let native_title_updated_at_ns = self
            .native_title_updated_at_ns
            .or(self.native_title_source_mtime_ns)
            .unwrap_or(0);

        if let (Some(friendly_name), Some(native_title)) = (friendly_name, native_title) {
            if friendly_name_updated_at_ns >= native_title_updated_at_ns {
                return Some(friendly_name.to_owned());
            }
            return Some(native_title.to_owned());
        }
        if self.friendly_name_is_explicit {
            if let Some(friendly_name) = friendly_name {
                return Some(friendly_name.to_owned());
            }
        }
        if let Some(native_title) = native_title {
            return Some(native_title.to_owned());
        }
        if let Some(friendly_name) = friendly_name {
            return Some(friendly_name.to_owned());
        }
        Some(non_empty_or(self.name.clone(), &self.id))
    }

    fn resolved_telegram_thread_id(&self) -> Option<i64> {
        self.telegram_thread_id
            .or(self.telegram_topic_id)
            .or(self.telegram_root_msg_id)
    }
}

#[derive(Debug, Serialize)]
pub struct SessionsEnvelope<T> {
    pub sessions: Vec<T>,
}

impl From<Vec<SessionResponse>> for SessionsEnvelope<SessionResponse> {
    fn from(sessions: Vec<SessionResponse>) -> Self {
        Self { sessions }
    }
}

impl From<Vec<ClientSessionResponse>> for SessionsEnvelope<ClientSessionResponse> {
    fn from(sessions: Vec<ClientSessionResponse>) -> Self {
        Self { sessions }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AdoptionProposalResponse {
    id: String,
    proposer_session_id: String,
    proposer_name: Option<String>,
    target_session_id: String,
    created_at: String,
    status: String,
    decided_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionResponse {
    id: String,
    name: String,
    working_dir: String,
    status: String,
    created_at: String,
    last_activity: String,
    completed_at: Option<String>,
    stopped_at: Option<String>,
    tmux_session: String,
    tmux_socket_name: Option<String>,
    node: String,
    provider: Option<String>,
    provider_resume_id: Option<String>,
    forked_from_session_id: Option<String>,
    forked_from_provider_resume_id: Option<String>,
    forked_provider_resume_id: Option<String>,
    forked_at: Option<String>,
    forked_by_session_id: Option<String>,
    friendly_name: Option<String>,
    telegram_chat_id: Option<i64>,
    telegram_thread_id: Option<i64>,
    current_task: Option<String>,
    git_remote_url: Option<String>,
    parent_session_id: Option<String>,
    last_handoff_path: Option<String>,
    agent_status_text: Option<String>,
    agent_status_at: Option<String>,
    agent_task_completed_at: Option<String>,
    is_em: bool,
    role: Option<String>,
    activity_state: String,
    last_tool_call: Option<String>,
    last_tool_name: Option<String>,
    last_action_summary: Option<String>,
    last_action_at: Option<String>,
    tokens_used: i64,
    context_monitor_enabled: bool,
    pending_adoption_proposals: Vec<AdoptionProposalResponse>,
    aliases: Vec<String>,
    is_maintainer: bool,
}

impl From<SessionRecord> for SessionResponse {
    fn from(session: SessionRecord) -> Self {
        let status = normalized_status(&session.status);
        let friendly_name = session.cached_display_name();
        let is_maintainer = session.aliases.iter().any(|alias| alias == "maintainer");
        let telegram_thread_id = session.resolved_telegram_thread_id();
        Self {
            id: session.id,
            name: session.name,
            working_dir: session.working_dir,
            status: status.to_owned(),
            created_at: session.created_at,
            last_activity: session.last_activity,
            completed_at: session.completed_at,
            stopped_at: session.stopped_at,
            tmux_session: session.tmux_session,
            tmux_socket_name: session.tmux_socket_name,
            node: non_empty_or(session.node, "primary"),
            provider: Some(non_empty_or(session.provider, "claude")),
            provider_resume_id: session.provider_resume_id,
            forked_from_session_id: session.forked_from_session_id,
            forked_from_provider_resume_id: session.forked_from_provider_resume_id,
            forked_provider_resume_id: session.forked_provider_resume_id,
            forked_at: session.forked_at,
            forked_by_session_id: session.forked_by_session_id,
            friendly_name,
            telegram_chat_id: session.telegram_chat_id,
            telegram_thread_id,
            current_task: session.current_task,
            git_remote_url: session.git_remote_url,
            parent_session_id: session.parent_session_id,
            last_handoff_path: session.last_handoff_path,
            agent_status_text: session.agent_status_text,
            agent_status_at: session.agent_status_at,
            agent_task_completed_at: session.agent_task_completed_at,
            is_em: session.is_em,
            role: session.role,
            activity_state: fallback_activity_state(status),
            last_tool_call: session.last_tool_call,
            last_tool_name: session.last_tool_name,
            last_action_summary: None,
            last_action_at: None,
            tokens_used: session.tokens_used,
            context_monitor_enabled: session.context_monitor_enabled,
            pending_adoption_proposals: session.pending_adoption_proposals,
            aliases: session.aliases,
            is_maintainer,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChildSessionResponse {
    id: String,
    name: String,
    friendly_name: Option<String>,
    status: String,
    activity_state: String,
    completion_status: Option<String>,
    completion_message: Option<String>,
    last_activity: String,
    spawned_at: Option<String>,
    completed_at: Option<String>,
    tmux_session: String,
    tmux_socket_name: Option<String>,
    agent_status_text: Option<String>,
    agent_status_at: Option<String>,
    provider: String,
    activity_projection: Option<Value>,
}

impl From<SessionRecord> for ChildSessionResponse {
    fn from(session: SessionRecord) -> Self {
        let status = normalized_status(&session.status).to_owned();
        let friendly_name = session.cached_display_name();
        let spawned_at = session
            .spawned_at
            .clone()
            .or(Some(session.created_at.clone()));
        Self {
            id: session.id,
            name: session.name,
            friendly_name,
            status: status.clone(),
            activity_state: fallback_activity_state(&status),
            completion_status: session.completion_status,
            completion_message: session.completion_message,
            last_activity: session.last_activity,
            spawned_at,
            completed_at: session.completed_at,
            tmux_session: session.tmux_session,
            tmux_socket_name: session.tmux_socket_name,
            agent_status_text: session.agent_status_text,
            agent_status_at: session.agent_status_at,
            provider: non_empty_or(session.provider, "claude"),
            activity_projection: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ClientSessionResponse {
    #[serde(flatten)]
    session: SessionResponse,
    attach_descriptor: AttachDescriptor,
    termux_attach: Option<Value>,
    mobile_terminal: Value,
    primary_action: PrimaryAction,
}

impl From<SessionRecord> for ClientSessionResponse {
    fn from(session: SessionRecord) -> Self {
        let response = SessionResponse::from(session);
        let attach_descriptor = AttachDescriptor {
            attach_supported: false,
            message: Some(
                "attach tickets are not implemented in the Rust read-only scaffold".to_owned(),
            ),
            tmux_session: Some(response.tmux_session.clone()),
            runtime_mode: Some("read_only".to_owned()),
        };
        Self {
            session: response,
            attach_descriptor,
            termux_attach: None,
            mobile_terminal: json!({
                "supported": false,
                "reason": "mobile terminal is not implemented in the Rust read-only scaffold"
            }),
            primary_action: PrimaryAction {
                action_type: "details",
                label: "View details",
                reason: None,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AttachDescriptor {
    attach_supported: bool,
    message: Option<String>,
    tmux_session: Option<String>,
    runtime_mode: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PrimaryAction {
    #[serde(rename = "type")]
    action_type: &'static str,
    label: &'static str,
    reason: Option<String>,
}

fn normalized_status(status: &str) -> &str {
    match status {
        "starting" => "running",
        "waiting_input" | "waiting_permission" | "error" => "idle",
        "running" | "idle" | "stopped" => status,
        _ => status,
    }
}

fn fallback_activity_state(status: &str) -> String {
    match status {
        "stopped" => "stopped".to_owned(),
        "running" => "working".to_owned(),
        _ => "idle".to_owned(),
    }
}

fn non_empty_or(value: String, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

fn optional_trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn has_text(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

fn tail_lines(content: &str, lines: usize) -> String {
    if lines == 0 {
        return String::new();
    }
    let all_lines = content.lines().collect::<Vec<_>>();
    if all_lines.is_empty() {
        return String::new();
    }
    let start = all_lines.len().saturating_sub(lines);
    let mut output = all_lines[start..].join("\n");
    if content.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn read_tail_lines(path: &Path, lines: usize) -> io::Result<String> {
    if lines == 0 {
        return Ok(String::new());
    }

    let mut file = fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len == 0 {
        return Ok(String::new());
    }

    let read_len = file_len.min(output_tail_byte_limit(lines));
    file.seek(SeekFrom::End(-(read_len as i64)))?;
    let mut bytes = Vec::with_capacity(read_len as usize);
    file.take(read_len).read_to_end(&mut bytes)?;
    Ok(tail_lines(&String::from_utf8_lossy(&bytes), lines))
}

fn output_tail_byte_limit(lines: usize) -> u64 {
    let requested = (lines as u64).saturating_mul(OUTPUT_TAIL_BYTES_PER_LINE);
    requested.clamp(MIN_OUTPUT_TAIL_BYTES, MAX_OUTPUT_TAIL_BYTES)
}

fn default_node() -> String {
    "primary".to_owned()
}

pub fn is_primary_node(node: &str) -> bool {
    let node = node.trim();
    node.is_empty() || node == "primary"
}

fn ensure_runtime_local_node(node: &str) -> Result<()> {
    if is_primary_node(node) {
        return Ok(());
    }
    anyhow::bail!("Rust runtime does not support remote node {node}");
}

fn default_provider() -> String {
    "claude".to_owned()
}

fn default_delivery_mode() -> String {
    "sequential".to_owned()
}

fn normalize_role(role: &str) -> String {
    let mut normalized = String::new();
    let mut last_was_dash = false;
    for ch in role.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch);
            last_was_dash = false;
        } else if !last_was_dash && !normalized.is_empty() {
            normalized.push('-');
            last_was_dash = true;
        }
    }
    while normalized.ends_with('-') {
        normalized.pop();
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_handles_bare_home_and_home_relative_paths() {
        let Some(home) = env::var_os("HOME") else {
            return;
        };
        let home = PathBuf::from(home);
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/work"), home.join("work"));
        assert_eq!(expand_home("/tmp/work"), PathBuf::from("/tmp/work"));
    }

    fn session_record(status: &str) -> SessionRecord {
        SessionRecord {
            id: "abc12345".to_owned(),
            name: "claude-abc12345".to_owned(),
            working_dir: "/repo".to_owned(),
            tmux_session: "claude-abc12345".to_owned(),
            tmux_socket_name: None,
            node: "primary".to_owned(),
            provider: "claude".to_owned(),
            model: None,
            log_file: Some("/tmp/abc12345.log".to_owned()),
            provider_resume_id: None,
            transcript_path: None,
            codex_thread_id: None,
            forked_from_session_id: None,
            forked_from_provider_resume_id: None,
            forked_provider_resume_id: None,
            forked_at: None,
            forked_by_session_id: None,
            friendly_name: Some("Example".to_owned()),
            friendly_name_is_explicit: false,
            friendly_name_updated_at_ns: None,
            native_title: None,
            native_title_updated_at_ns: None,
            native_title_source_mtime_ns: None,
            telegram_chat_id: None,
            telegram_thread_id: None,
            telegram_topic_id: None,
            telegram_root_msg_id: None,
            current_task: None,
            git_remote_url: None,
            review_config: None,
            parent_session_id: None,
            last_handoff_path: None,
            agent_status_text: None,
            agent_status_at: None,
            agent_task_completed_at: None,
            completion_status: None,
            completion_message: None,
            completed_at: None,
            stopped_at: None,
            is_em: false,
            role: None,
            status: status.to_owned(),
            spawned_at: Some("2026-06-01T00:00:00".to_owned()),
            created_at: "2026-06-01T00:00:00".to_owned(),
            last_activity: "2026-06-01T00:01:00".to_owned(),
            last_tool_call: None,
            last_tool_name: None,
            tokens_used: 0,
            context_monitor_enabled: false,
            context_monitor_notify: None,
            aliases: Vec::new(),
            pending_adoption_proposals: Vec::new(),
        }
    }

    #[test]
    fn session_projection_maps_legacy_status_and_activity() {
        let response = SessionResponse::from(session_record("waiting_permission"));

        assert_eq!(response.status, "idle");
        assert_eq!(response.activity_state, "idle");
    }

    #[test]
    fn client_projection_disables_unported_attach_surfaces() {
        let response = ClientSessionResponse::from(session_record("running"));

        assert!(!response.attach_descriptor.attach_supported);
        assert!(response.termux_attach.is_none());
        assert_eq!(response.mobile_terminal["supported"], false);
        assert_eq!(response.primary_action.action_type, "details");
    }

    #[test]
    fn cached_display_name_prefers_newer_native_title() {
        let mut session = session_record("running");
        session.friendly_name = Some("stale-friendly-name".to_owned());
        session.friendly_name_updated_at_ns = Some(10);
        session.native_title = Some("cached-native-title".to_owned());
        session.native_title_updated_at_ns = Some(20);
        let response = SessionResponse::from(session);

        assert_eq!(
            response.friendly_name.as_deref(),
            Some("cached-native-title")
        );
    }

    #[test]
    fn cached_display_name_keeps_newer_explicit_friendly_name() {
        let mut session = session_record("running");
        session.friendly_name = Some("explicit-name".to_owned());
        session.friendly_name_is_explicit = true;
        session.friendly_name_updated_at_ns = Some(30);
        session.native_title = Some("older-native-title".to_owned());
        session.native_title_updated_at_ns = Some(20);
        let response = SessionResponse::from(session);

        assert_eq!(response.friendly_name.as_deref(), Some("explicit-name"));
    }

    #[test]
    fn cached_display_name_falls_back_to_session_name_or_id() {
        let mut session = session_record("running");
        session.friendly_name = None;
        let response = SessionResponse::from(session.clone());

        assert_eq!(response.friendly_name.as_deref(), Some("claude-abc12345"));

        session.name = String::new();
        let response = SessionResponse::from(session);

        assert_eq!(response.friendly_name.as_deref(), Some("abc12345"));
    }

    #[test]
    fn default_state_loader_reads_legacy_fallback_when_primary_missing() {
        let state_file = unique_temp_path("primary");
        let legacy_state_file = unique_temp_path("legacy");
        fs::write(
            &legacy_state_file,
            json!({
                "sessions": [
                    {
                        "id": "legacy1",
                        "name": "claude-legacy1",
                        "working_dir": "/repo",
                        "tmux_session": "claude-legacy1",
                        "log_file": "/tmp/legacy1.log",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00",
                        "last_activity": "2026-06-01T00:01:00"
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file, legacy_state_file);

        let sessions = store.list_sessions(false).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "legacy1");
    }

    #[test]
    fn default_state_loader_reads_legacy_fallback_when_primary_is_invalid() {
        let state_file = unique_temp_path("primary");
        let legacy_state_file = unique_temp_path("legacy");
        fs::write(&state_file, "{not json").unwrap();
        fs::write(
            &legacy_state_file,
            json!({
                "sessions": [
                    {
                        "id": "legacy2",
                        "name": "claude-legacy2",
                        "working_dir": "/repo",
                        "tmux_session": "claude-legacy2",
                        "log_file": "/tmp/legacy2.log",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00",
                        "last_activity": "2026-06-01T00:01:00"
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file, legacy_state_file);

        let sessions = store.list_sessions(false).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "legacy2");
    }

    #[test]
    fn snapshot_skips_legacy_codex_app_records_before_deserializing_sessions() {
        let raw = RawStateSnapshot {
            sessions: vec![
                json!({
                    "id": "legacyapp",
                    "name": "legacy app",
                    "provider": "codex",
                    "codex_thread_id": "thread-1",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }),
                json!({
                    "id": "tmux1",
                    "name": "claude-tmux1",
                    "working_dir": "/repo",
                    "tmux_session": "claude-tmux1",
                    "log_file": "/tmp/tmux1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }),
            ],
            ..RawStateSnapshot::default()
        };

        let snapshot = StateSnapshot::try_from(raw).unwrap();
        let sessions = snapshot.into_sessions();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "tmux1");
    }

    #[test]
    fn codex_fork_retry_error_events_are_not_terminal() {
        let retry = json!({
            "event_type": "error",
            "payload": {
                "willRetry": true,
                "error": {
                    "message": "Reconnecting... 1/5"
                }
            }
        });
        let retry_event = retry.as_object().unwrap();
        assert_eq!(codex_fork_status_for_event(retry_event), Some("running"));

        let terminal = json!({
            "event_type": "error",
            "payload": {
                "willRetry": false,
                "error": {
                    "message": "Selected model is at capacity."
                }
            }
        });
        let terminal_event = terminal.as_object().unwrap();
        assert_eq!(codex_fork_status_for_event(terminal_event), Some("stopped"));
    }

    #[test]
    fn session_projection_uses_legacy_telegram_thread_fields() {
        let mut session = session_record("running");
        session.telegram_topic_id = Some(123);
        let response = SessionResponse::from(session);

        assert_eq!(response.telegram_thread_id, Some(123));

        let mut session = session_record("running");
        session.telegram_root_msg_id = Some(456);
        let response = SessionResponse::from(session);

        assert_eq!(response.telegram_thread_id, Some(456));
    }

    #[test]
    fn snapshot_projects_aliases_and_pending_adoption_proposals() {
        let snapshot = StateSnapshot {
            sessions: vec![
                SessionRecord {
                    id: "em123456".to_owned(),
                    friendly_name: Some("em-ops".to_owned()),
                    is_em: true,
                    ..session_record("running")
                },
                SessionRecord {
                    id: "child001".to_owned(),
                    friendly_name: None,
                    ..session_record("running")
                },
            ],
            maintainer_session_id: Some("em123456".to_owned()),
            agent_registrations: vec![AgentRegistrationRecord {
                role: "Reviewer".to_owned(),
                session_id: "child001".to_owned(),
                created_at: None,
            }],
            adoption_proposals: vec![AdoptionProposalRecord {
                id: "proposal1".to_owned(),
                proposer_session_id: "em123456".to_owned(),
                target_session_id: "child001".to_owned(),
                created_at: "2026-06-01T00:03:00".to_owned(),
                status: "pending".to_owned(),
                decided_at: None,
            }],
        };

        let sessions = snapshot.into_sessions();
        let maintainer = sessions
            .iter()
            .find(|session| session.id == "em123456")
            .unwrap();
        let child = sessions
            .iter()
            .find(|session| session.id == "child001")
            .unwrap();

        assert_eq!(maintainer.aliases, vec!["maintainer"]);
        assert_eq!(
            maintainer.cached_display_name().as_deref(),
            Some("maintainer")
        );
        assert_eq!(child.aliases, vec!["reviewer"]);
        assert_eq!(child.pending_adoption_proposals.len(), 1);
        assert_eq!(
            child.pending_adoption_proposals[0].proposer_name.as_deref(),
            Some("maintainer")
        );
    }

    #[test]
    fn snapshot_prunes_stale_aliases_but_keeps_restorable_stopped_aliases() {
        let snapshot = StateSnapshot {
            sessions: vec![
                SessionRecord {
                    id: "dead001".to_owned(),
                    provider_resume_id: None,
                    transcript_path: None,
                    ..session_record("stopped")
                },
                SessionRecord {
                    id: "restore1".to_owned(),
                    provider_resume_id: Some("resume-id".to_owned()),
                    ..session_record("stopped")
                },
            ],
            maintainer_session_id: Some("dead001".to_owned()),
            agent_registrations: vec![
                AgentRegistrationRecord {
                    role: "Stale Role".to_owned(),
                    session_id: "dead001".to_owned(),
                    created_at: None,
                },
                AgentRegistrationRecord {
                    role: "Restorable Role".to_owned(),
                    session_id: "restore1".to_owned(),
                    created_at: None,
                },
            ],
            adoption_proposals: Vec::new(),
        };

        let sessions = snapshot.into_sessions();
        let stale = sessions
            .iter()
            .find(|session| session.id == "dead001")
            .unwrap();
        let restorable = sessions
            .iter()
            .find(|session| session.id == "restore1")
            .unwrap();

        assert!(stale.aliases.is_empty());
        assert_eq!(restorable.aliases, vec!["restorable-role"]);
    }

    fn unique_temp_path(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!(
            "sm-rust-session-store-{label}-{}-{nanos}-{}.json",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }
}
