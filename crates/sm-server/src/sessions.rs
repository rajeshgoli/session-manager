use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::runtime::{TmuxRuntime, TmuxSessionSpec};

const DEFAULT_SESSION_STATE_FILE: &str = "~/.local/share/claude-sessions/sessions.json";
const LEGACY_TMP_SESSION_STATE_FILE: &str = "/tmp/claude-sessions/sessions.json";
const OUTPUT_TAIL_BYTES_PER_LINE: u64 = 4096;
const MIN_OUTPUT_TAIL_BYTES: u64 = 16 * 1024;
const MAX_OUTPUT_TAIL_BYTES: u64 = 1024 * 1024;
static STATE_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct SessionStore {
    state_file: PathBuf,
    legacy_state_file: Option<PathBuf>,
    write_lock: Arc<Mutex<()>>,
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
        }
    }

    #[cfg(test)]
    fn new_with_legacy_fallback(state_file: PathBuf, legacy_state_file: PathBuf) -> Self {
        Self {
            state_file,
            legacy_state_file: Some(legacy_state_file),
            write_lock: Arc::new(Mutex::new(())),
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
        let record = self.build_core_session_record(
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
        runtime.create_session(&TmuxSessionSpec {
            session_id: record.id.clone(),
            tmux_session: record.tmux_session.clone(),
            working_dir: expand_home(&record.working_dir).display().to_string(),
            log_file,
            initial_message: request.initial_message.clone(),
            model: request.model.clone(),
        })?;
        sessions.push(serde_json::to_value(&record)?);
        if let Err(error) = self.write_raw_json_value(&state) {
            let _ = runtime.kill_session(&record.tmux_session);
            return Err(error);
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
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(None);
        };
        let node = json_text(session.get("node")).unwrap_or_else(default_node);
        ensure_runtime_local_node(&node)?;
        let mut status = json_text(session.get("status")).unwrap_or_else(|| "running".to_owned());
        let tmux_session = json_text(session.get("tmux_session"))
            .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
        let session_socket_name = json_text(session.get("tmux_socket_name"));
        let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
        let mut delivered = false;
        if normalized_status(&status) != "stopped" {
            delivered = session_runtime.send_input(&tmux_session, &request.text)?;
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
        if record.provider != "claude" {
            return Ok(Some(CoreRestoreOutcome::UnsupportedProvider(
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
            initial_message: None,
            model: record.model.clone(),
        };
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
        Ok(Some(CoreRestoreOutcome::Restored(restored)))
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
        let now = now_rfc3339();
        session.insert("status".to_owned(), Value::String("stopped".to_owned()));
        mark_session_killed(session, &now);
        session.insert("stopped_at".to_owned(), Value::String(now.clone()));
        session.insert("last_activity".to_owned(), Value::String(now));
        if let Some(log_file) = json_text(session.get("log_file")) {
            append_log_line(&expand_home(&log_file), "[sm-rust] fixture session retired")?;
        }
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
        if !is_primary_node(&node) {
            return Ok(CoreRetireOutcome::UnsupportedNode(node));
        }
        let tmux_session = json_text(session.get("tmux_session"))
            .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
        let session_socket_name = json_text(session.get("tmux_socket_name"));
        let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
        let _ = session_runtime.kill_session(&tmux_session)?;
        let now = now_rfc3339();
        session.insert("status".to_owned(), Value::String("stopped".to_owned()));
        mark_session_killed(session, &now);
        session.insert("stopped_at".to_owned(), Value::String(now.clone()));
        session.insert("last_activity".to_owned(), Value::String(now));
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
pub struct SendCoreInputRequest {
    pub text: String,
    #[serde(default = "default_delivery_mode")]
    pub delivery_mode: String,
    #[serde(default)]
    pub notify_after_seconds: Option<u64>,
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

    fn cached_display_name(&self) -> Option<String> {
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
    model: Option<String>,
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
            model: session.model,
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
