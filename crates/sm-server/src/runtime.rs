#[cfg(unix)]
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::{
    collections::HashMap,
    env, fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{mpsc, Arc, Condvar, Mutex, OnceLock, Weak},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{AppConfig, CodexReviewConfig, RustCoreConfig};

const DEFAULT_SEND_KEYS_SETTLE_MS: f64 = 300.0;
const DEFAULT_SEND_KEYS_SETTLE_MAX_MS: f64 = 900.0;
const DEFAULT_SEND_KEYS_SETTLE_PER_KI_MS: f64 = 60.0;
const DEFAULT_SEND_KEYS_SETTLE_PER_EXTRA_LINE_MS: f64 = 15.0;
const DEFAULT_SEND_KEYS_MAX_CHUNK_CHARS: usize = 4096;
const CODEX_MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_INITIAL_BRIEF_READY_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_INITIAL_BRIEF_ACK_TIMEOUT: Duration = Duration::from_secs(30);
static SESSION_INPUT_LOCKS: OnceLock<Mutex<HashMap<String, Weak<SessionInputLock>>>> =
    OnceLock::new();

#[derive(Debug)]
struct SessionInputLock {
    held: Mutex<bool>,
    available: Condvar,
}

#[derive(Debug)]
pub struct SessionInputGuard {
    lock: Arc<SessionInputLock>,
}

impl Drop for SessionInputGuard {
    fn drop(&mut self) {
        if let Ok(mut held) = self.lock.held.lock() {
            *held = false;
            self.lock.available.notify_one();
        }
    }
}

#[derive(Debug, Clone)]
pub struct TmuxRuntime {
    socket_name: Option<String>,
    tmux_binary: String,
    custom_runtime_command: bool,
    claude_command: String,
    claude_args: Vec<String>,
    codex_command: String,
    codex_args: Vec<String>,
    codex_default_model: Option<String>,
    codex_fork_command: String,
    codex_fork_args: Vec<String>,
    codex_fork_default_model: Option<String>,
    codex_fork_event_schema_version: u32,
    codex_fork_control_tmux_fallback_enabled: bool,
    tmux_native_scrollback: bool,
    tmux_history_limit: Option<u64>,
    prompt_mode: String,
    start_settle_ms: u64,
    initial_brief_ready_timeout: Duration,
    initial_brief_ack_timeout: Duration,
    send_keys_settle_ms: f64,
    send_keys_settle_max_ms: f64,
    send_keys_settle_per_ki_ms: f64,
    send_keys_settle_per_extra_line_ms: f64,
    send_keys_max_chunk_chars: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConditionalClearOutcome {
    Cleared,
    IdlePromptNotReady,
    PostClearPreconditionFailed,
    PreconditionFailed,
    SessionMissing,
}

#[derive(Clone)]
pub struct TmuxSessionSpec {
    pub session_id: String,
    pub session_credential: Option<String>,
    pub tmux_session: String,
    pub working_dir: String,
    pub log_file: PathBuf,
    pub provider: String,
    pub initial_message: Option<String>,
    /// Bypass argv prompt mode to deliver an already-verified spawn brief via
    /// tmux stdin, avoiding both command-line size limits and path races.
    pub force_initial_prompt_stdin: bool,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CodexForkRuntimeArtifacts {
    pub event_stream_path: PathBuf,
    pub control_socket_path: PathBuf,
}

#[derive(Debug, Clone)]
struct ClaudeInitialBriefReadyPane {
    pane: String,
    composer_y: usize,
}

#[derive(Debug)]
pub enum CodexModelValidationError {
    Unsupported {
        requested: String,
        supported: Vec<String>,
    },
    DiscoveryUnavailable(String),
}

/// A spawn brief reached the runtime but could not be proved accepted by its
/// provider. The immutable artifact remains recorded for recovery.
#[derive(Debug)]
pub enum InitialBriefDeliveryError {
    ProviderAcknowledgementUnavailable { provider: String },
    ProviderReadinessTimedOut { provider: String },
    ProviderAcceptanceTimedOut { provider: String },
    SessionExited { provider: String },
}

impl std::fmt::Display for InitialBriefDeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProviderAcknowledgementUnavailable { provider } => write!(
                formatter,
                "initial spawn brief delivery is unverified for {provider}: this provider has no equivalent provider-originated acknowledgement"
            ),
            Self::ProviderReadinessTimedOut { provider } => write!(
                formatter,
                "timed out waiting for {provider} to become ready for the initial spawn brief"
            ),
            Self::ProviderAcceptanceTimedOut { provider } => write!(
                formatter,
                "timed out waiting for {provider} to acknowledge the initial spawn brief; it was not resent to avoid duplicate work"
            ),
            Self::SessionExited { provider } => write!(
                formatter,
                "{provider} exited before the initial spawn brief could be delivered"
            ),
        }
    }
}

impl std::error::Error for InitialBriefDeliveryError {}

impl std::fmt::Display for CodexModelValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported {
                requested,
                supported,
            } => write!(
                formatter,
                "Unsupported Codex model: {requested}. Supported models: {}",
                supported.join(", ")
            ),
            Self::DiscoveryUnavailable(detail) => write!(
                formatter,
                "Could not validate the requested Codex model because model discovery failed: {detail}"
            ),
        }
    }
}

impl std::error::Error for CodexModelValidationError {}

impl TmuxRuntime {
    pub fn from_config(config: &RustCoreConfig) -> Self {
        Self {
            socket_name: config
                .tmux_socket_name
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
            tmux_binary: "tmux".to_owned(),
            custom_runtime_command: config
                .runtime_command
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_some(),
            claude_command: config
                .runtime_command
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("claude")
                .to_owned(),
            claude_args: Vec::new(),
            codex_command: "codex".to_owned(),
            codex_args: Vec::new(),
            codex_default_model: None,
            codex_fork_command: "codex".to_owned(),
            codex_fork_args: vec![
                "-c".to_owned(),
                "check_for_update_on_startup=false".to_owned(),
            ],
            codex_fork_default_model: None,
            codex_fork_event_schema_version: 2,
            codex_fork_control_tmux_fallback_enabled: true,
            tmux_native_scrollback: config.tmux_native_scrollback.unwrap_or(false),
            tmux_history_limit: config.tmux_history_limit.filter(|value| *value > 0),
            prompt_mode: config
                .runtime_prompt_mode
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("argv")
                .to_owned(),
            start_settle_ms: config.runtime_start_settle_ms.unwrap_or(300),
            initial_brief_ready_timeout: config
                .runtime_initial_brief_ready_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_INITIAL_BRIEF_READY_TIMEOUT),
            initial_brief_ack_timeout: config
                .runtime_initial_brief_ack_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_INITIAL_BRIEF_ACK_TIMEOUT),
            send_keys_settle_ms: finite_nonnegative_or_default(
                config.send_keys_settle_ms,
                DEFAULT_SEND_KEYS_SETTLE_MS,
            ),
            send_keys_settle_max_ms: finite_nonnegative_or_default(
                config.send_keys_settle_max_ms,
                DEFAULT_SEND_KEYS_SETTLE_MAX_MS,
            ),
            send_keys_settle_per_ki_ms: finite_nonnegative_or_default(
                config.send_keys_settle_per_ki_ms,
                DEFAULT_SEND_KEYS_SETTLE_PER_KI_MS,
            ),
            send_keys_settle_per_extra_line_ms: finite_nonnegative_or_default(
                config.send_keys_settle_per_extra_line_ms,
                DEFAULT_SEND_KEYS_SETTLE_PER_EXTRA_LINE_MS,
            ),
            send_keys_max_chunk_chars: config
                .send_keys_max_chunk_chars
                .unwrap_or(DEFAULT_SEND_KEYS_MAX_CHUNK_CHARS)
                .max(1),
        }
    }

    pub fn from_app_config(config: &AppConfig) -> Self {
        let mut runtime = Self::from_config(&config.rust_core);
        if config.rust_core.tmux_native_scrollback.is_none() {
            runtime.tmux_native_scrollback = config.tmux.native_scrollback;
        }
        if config.rust_core.tmux_history_limit.is_none() {
            runtime.tmux_history_limit = config.tmux.history_limit.filter(|value| *value > 0);
        }
        if config
            .rust_core
            .runtime_command
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
        {
            runtime.claude_command = config.claude.command.clone();
            runtime.claude_args = config.claude.args.clone();
        }
        runtime.codex_command = config.codex.command.clone();
        runtime.codex_args = config.codex.args.clone();
        runtime.codex_default_model = config.codex.default_model.clone();
        runtime.codex_fork_command = config.codex_fork.command.clone();
        runtime.codex_fork_args = config.codex_fork.args.clone();
        runtime.codex_fork_default_model = config.codex_fork.default_model.clone();
        runtime.codex_fork_event_schema_version = config.codex_fork.event_schema_version;
        runtime.codex_fork_control_tmux_fallback_enabled =
            config.codex_fork.control_tmux_fallback_enabled;
        runtime
    }

    pub fn socket_name(&self) -> Option<&str> {
        self.socket_name.as_deref()
    }

    pub fn codex_fork_control_tmux_fallback_enabled(&self) -> bool {
        self.codex_fork_control_tmux_fallback_enabled
    }

    pub fn validate_codex_fork_model(&self, requested: &str, working_dir: &Path) -> Result<()> {
        self.validate_codex_fork_model_with_timeout(
            requested,
            working_dir,
            CODEX_MODEL_DISCOVERY_TIMEOUT,
        )
    }

    fn validate_codex_fork_model_with_timeout(
        &self,
        requested: &str,
        working_dir: &Path,
        timeout: Duration,
    ) -> Result<()> {
        let executable = resolve_launch_command(&self.codex_fork_command, working_dir)
            .map_err(|error| CodexModelValidationError::DiscoveryUnavailable(error.to_string()))?;
        let mut command = Command::new(executable);
        command
            .args(&self.codex_fork_args)
            .args(["debug", "models"])
            .current_dir(working_dir);
        let output = command_output_with_timeout(command, timeout)
            .map_err(|error| CodexModelValidationError::DiscoveryUnavailable(error.to_string()))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = truncate_for_error(detail.trim(), 500);
            let detail = if detail.is_empty() {
                format!("model catalog command exited with {}", output.status)
            } else {
                format!(
                    "model catalog command exited with {}: {detail}",
                    output.status
                )
            };
            return Err(CodexModelValidationError::DiscoveryUnavailable(detail).into());
        }

        let catalog: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            CodexModelValidationError::DiscoveryUnavailable(format!(
                "model catalog returned invalid JSON: {error}"
            ))
        })?;
        let supported = catalog
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CodexModelValidationError::DiscoveryUnavailable(
                    "model catalog response is missing the models array".to_owned(),
                )
            })?
            .iter()
            .filter(|model| model.get("visibility").and_then(Value::as_str) == Some("list"))
            .filter_map(|model| model.get("slug").and_then(Value::as_str))
            .fold(Vec::new(), |mut models, slug| {
                if !models.iter().any(|model| model == slug) {
                    models.push(slug.to_owned());
                }
                models
            });
        if supported.is_empty() {
            return Err(CodexModelValidationError::DiscoveryUnavailable(
                "model catalog returned no selectable models".to_owned(),
            )
            .into());
        }
        if supported.iter().any(|model| model == requested) {
            return Ok(());
        }
        Err(CodexModelValidationError::Unsupported {
            requested: requested.to_owned(),
            supported,
        }
        .into())
    }

    pub fn allows_restore_without_resume_id(&self, provider: &str) -> bool {
        provider == "claude" && self.custom_runtime_command
    }

    pub fn startup_settle_duration(&self) -> Duration {
        Duration::from_millis(self.start_settle_ms)
    }

    pub fn for_socket_name(&self, socket_name: Option<&str>) -> Self {
        let mut runtime = self.clone();
        runtime.socket_name = socket_name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        runtime
    }

    pub fn codex_fork_runtime_artifacts(
        &self,
        spec: &TmuxSessionSpec,
    ) -> Result<Option<CodexForkRuntimeArtifacts>> {
        if spec.provider != "codex-fork" {
            return Ok(None);
        }
        let (event_stream_path, control_socket_path) = codex_fork_artifact_paths(spec)?;
        Ok(Some(CodexForkRuntimeArtifacts {
            event_stream_path,
            control_socket_path,
        }))
    }

    pub fn create_session(&self, spec: &TmuxSessionSpec) -> Result<()> {
        if self.session_exists(&spec.tmux_session)? {
            bail!("tmux session already exists: {}", spec.tmux_session);
        }
        if !Path::new(&spec.working_dir).is_dir() {
            bail!("working dir does not exist: {}", spec.working_dir);
        }
        if let Some(parent) = spec.log_file.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log dir {}", parent.display()))?;
        }
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spec.log_file)
            .with_context(|| format!("failed to prepare log file {}", spec.log_file.display()))?;

        let prompt_mode = if spec.force_initial_prompt_stdin {
            "stdin".to_owned()
        } else {
            self.prompt_mode.to_ascii_lowercase()
        };
        if prompt_mode != "argv" && prompt_mode != "stdin" {
            bail!("unsupported runtime prompt mode: {}", self.prompt_mode);
        }
        let has_initial_stdin_prompt = initial_stdin_prompt(spec, &prompt_mode).is_some();

        let mut command = self.launch_command(spec, &prompt_mode)?;
        command = managed_session_command(
            &command,
            &spec.session_id,
            spec.session_credential.as_deref(),
        );

        let create_result = if self.tmux_history_limit.is_some()
            || (self.socket_name.is_some() && self.tmux_native_scrollback)
        {
            self.create_session_with_bootstrap(spec, &command)
        } else {
            let result = self.run_tmux([
                "new-session",
                "-d",
                "-s",
                spec.tmux_session.as_str(),
                "-c",
                spec.working_dir.as_str(),
                command.as_str(),
            ]);
            if result.is_ok() {
                self.ensure_server_options();
            }
            result
        };
        if let Err(error) = create_result {
            if has_initial_stdin_prompt && is_tmux_session_gone_error(&error) {
                bail!("tmux session exited before initial prompt could be delivered");
            }
            return Err(error);
        }

        if let Err(error) = self.attach_session_log(spec, &prompt_mode) {
            let _ = self.kill_session(&spec.tmux_session);
            return Err(error);
        }
        Ok(())
    }

    pub fn restore_session(
        &self,
        spec: &TmuxSessionSpec,
        provider: &str,
        resume_id: Option<&str>,
    ) -> Result<()> {
        let mut runtime = self.clone();
        if let Some(resume_id) = resume_id.map(str::trim).filter(|value| !value.is_empty()) {
            match provider {
                "claude" => {
                    runtime.claude_args.push("--resume".to_owned());
                    runtime.claude_args.push(resume_id.to_owned());
                }
                "codex-fork" => {
                    runtime.codex_fork_args =
                        prepend_arg_pair("resume", resume_id, &runtime.codex_fork_args);
                }
                "codex" => {
                    runtime.codex_args = prepend_arg_pair("resume", resume_id, &runtime.codex_args);
                }
                _ => {}
            };
        }
        let mut spec = spec.clone();
        spec.initial_message = None;
        runtime.create_session(&spec)
    }

    pub fn send_input(&self, tmux_session: &str, text: &str) -> Result<bool> {
        let _guard = self.lock_session_input(tmux_session)?;
        self.send_input_while_locked(tmux_session, text)
    }

    pub fn send_input_while_locked(&self, tmux_session: &str, text: &str) -> Result<bool> {
        if !self.session_exists(tmux_session)? {
            return Ok(false);
        }
        self.send_text_then_enter(tmux_session, text)?;
        Ok(true)
    }

    pub fn send_urgent_input(
        &self,
        tmux_session: &str,
        text: &str,
        background_claude_task: bool,
    ) -> Result<bool> {
        let _guard = self.lock_session_input(tmux_session)?;
        self.send_urgent_input_while_locked(tmux_session, text, background_claude_task)
    }

    fn send_urgent_input_while_locked(
        &self,
        tmux_session: &str,
        text: &str,
        background_claude_task: bool,
    ) -> Result<bool> {
        if !self.session_exists(tmux_session)? {
            return Ok(false);
        }
        if background_claude_task {
            self.send_key(tmux_session, "C-b")?;
            let _ = self.wait_for_prompt(tmux_session, Duration::from_millis(300));
        }
        self.send_key(tmux_session, "Escape")?;
        let _ = self.wait_for_prompt(tmux_session, Duration::from_millis(300));
        self.send_text_then_enter(tmux_session, text)?;
        Ok(true)
    }

    pub fn lock_session_input(&self, tmux_session: &str) -> Result<SessionInputGuard> {
        let key = format!(
            "{}:{tmux_session}",
            self.socket_name.as_deref().unwrap_or("default")
        );
        let lock = {
            let mut locks = SESSION_INPUT_LOCKS
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .map_err(|_| anyhow::anyhow!("session input lock registry poisoned"))?;
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(SessionInputLock {
                    held: Mutex::new(false),
                    available: Condvar::new(),
                });
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        };
        let mut held = lock
            .held
            .lock()
            .map_err(|_| anyhow::anyhow!("session input lock poisoned"))?;
        while *held {
            held = lock
                .available
                .wait(held)
                .map_err(|_| anyhow::anyhow!("session input lock poisoned"))?;
        }
        *held = true;
        drop(held);
        Ok(SessionInputGuard { lock })
    }

    pub fn send_review_sequence(
        &self,
        tmux_session: &str,
        mode: &str,
        base_branch: Option<&str>,
        commit_sha: Option<&str>,
        custom_prompt: Option<&str>,
        branch_position: Option<usize>,
        timing: &CodexReviewConfig,
    ) -> Result<bool> {
        let _guard = self.lock_session_input(tmux_session)?;
        if !self.session_exists(tmux_session)? {
            return Ok(false);
        }
        let mode = mode.trim();
        if mode == "custom" {
            let prompt = custom_prompt.unwrap_or("").trim();
            self.send_text_then_enter(tmux_session, &format!("/review {prompt}"))?;
            return Ok(true);
        }

        self.send_text_then_enter(tmux_session, "/review")?;
        thread::sleep(duration_from_seconds(timing.menu_settle_seconds));

        match mode {
            "branch" => {
                self.send_key(tmux_session, "Enter")?;
                thread::sleep(duration_from_seconds(timing.branch_settle_seconds));
                if base_branch.is_some() {
                    for _ in 0..branch_position.unwrap_or(0) {
                        self.send_key(tmux_session, "Down")?;
                    }
                }
                thread::sleep(self.compute_settle_delay(base_branch.unwrap_or("")));
                self.send_key(tmux_session, "Enter")?;
            }
            "uncommitted" => {
                self.send_key(tmux_session, "Down")?;
                thread::sleep(self.compute_settle_delay(mode));
                self.send_key(tmux_session, "Enter")?;
            }
            "commit" => {
                self.send_key(tmux_session, "Down")?;
                self.send_key(tmux_session, "Down")?;
                thread::sleep(self.compute_settle_delay(mode));
                self.send_key(tmux_session, "Enter")?;
                thread::sleep(duration_from_seconds(timing.branch_settle_seconds));
                if let Some(commit_sha) =
                    commit_sha.map(str::trim).filter(|value| !value.is_empty())
                {
                    self.send_text(tmux_session, commit_sha)?;
                    thread::sleep(self.compute_settle_delay(commit_sha));
                }
                self.send_key(tmux_session, "Enter")?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub fn send_steer_text(&self, tmux_session: &str, text: &str) -> Result<bool> {
        let _guard = self.lock_session_input(tmux_session)?;
        if !self.session_exists(tmux_session)? {
            return Ok(false);
        }
        self.send_key(tmux_session, "Enter")?;
        thread::sleep(self.compute_settle_delay(text));
        self.send_text_then_enter(tmux_session, text)?;
        Ok(true)
    }

    pub fn clear_session(
        &self,
        tmux_session: &str,
        clear_command: &str,
        prompt: Option<&str>,
        wake_completed: bool,
    ) -> Result<bool> {
        self.clear_session_inner(tmux_session, clear_command, prompt, wake_completed, None)
    }

    pub(crate) fn clear_claude_session_if<P, C, S>(
        &self,
        tmux_session: &str,
        prompt: &str,
        precondition: P,
        commit_clear: C,
        commit_prompt: S,
    ) -> Result<ConditionalClearOutcome>
    where
        P: FnOnce() -> Result<bool>,
        C: FnOnce(&mut dyn FnMut() -> Result<()>) -> Result<bool>,
        S: FnOnce(&mut dyn FnMut() -> Result<()>) -> Result<bool>,
    {
        let _guard = self.lock_session_input(tmux_session)?;
        if !self.session_exists(tmux_session)? {
            return Ok(ConditionalClearOutcome::SessionMissing);
        }
        if !precondition()? {
            return Ok(ConditionalClearOutcome::PreconditionFailed);
        }
        if !self.wait_for_prompt(tmux_session, Duration::from_secs_f64(3.0)) {
            return Ok(ConditionalClearOutcome::IdlePromptNotReady);
        }
        let Some(pre_clear_pane) = self.capture_pane_text(tmux_session) else {
            return Ok(ConditionalClearOutcome::IdlePromptNotReady);
        };
        let mut send_clear = || self.send_text_then_enter(tmux_session, "/clear");
        if !commit_clear(&mut send_clear)? {
            return Ok(ConditionalClearOutcome::PreconditionFailed);
        }

        if !self.wait_for_fresh_prompt(tmux_session, &pre_clear_pane, Duration::from_secs_f64(5.0))
        {
            bail!("Claude handoff timed out waiting for /clear to finish");
        }
        let mut send_prompt = || self.send_text_then_enter(tmux_session, prompt);
        if !commit_prompt(&mut send_prompt)? {
            return Ok(ConditionalClearOutcome::PostClearPreconditionFailed);
        }
        Ok(ConditionalClearOutcome::Cleared)
    }

    pub fn clear_codex_session_confirming_prompt(
        &self,
        tmux_session: &str,
        prompt: &str,
        event_stream_path: &Path,
        initial_event_offset: u64,
    ) -> Result<bool> {
        self.clear_session_inner(
            tmux_session,
            "/new",
            Some(prompt),
            false,
            Some((event_stream_path, initial_event_offset)),
        )
    }

    fn clear_session_inner(
        &self,
        tmux_session: &str,
        clear_command: &str,
        prompt: Option<&str>,
        wake_completed: bool,
        codex_prompt_confirmation: Option<(&Path, u64)>,
    ) -> Result<bool> {
        let _guard = self.lock_session_input(tmux_session)?;
        if !self.session_exists(tmux_session)? {
            return Ok(false);
        }
        if wake_completed {
            self.send_key(tmux_session, "Enter")?;
            let _ = self.wait_for_prompt(tmux_session, Duration::from_secs_f64(3.0));
        }

        self.send_key(tmux_session, "Escape")?;
        let _ = self.wait_for_prompt(tmux_session, Duration::from_secs_f64(3.0));

        let pre_reset_pane = (clear_command == "/new")
            .then(|| self.capture_pane_text(tmux_session))
            .flatten();
        self.send_text_then_enter(tmux_session, clear_command)?;
        if clear_command == "/new" {
            // Codex redraws and reinitializes its composer after `/new`. Its
            // full-screen prompt is not the bare `>` recognized by Claude's
            // prompt waiter. Require both a changed frame and a composer at the
            // live cursor row so transcript prompts cannot satisfy readiness.
            while !self.wait_for_codex_composer(
                tmux_session,
                pre_reset_pane.as_deref(),
                Duration::from_secs(10),
            ) {
                // `/new` has already destroyed the prior turn, so returning to
                // the event monitor here would deadlock the handoff: no old
                // turn remains to emit another completion. Keep the input lock
                // and retry readiness until the new composer appears, unless
                // the tmux session itself has gone away.
                if !self.session_exists(tmux_session)? {
                    return Ok(false);
                }
            }
        } else {
            let _ = self.wait_for_prompt(tmux_session, Duration::from_secs_f64(5.0));
        }

        if let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) {
            self.send_text_then_enter(tmux_session, prompt)?;
            if let Some((event_stream_path, initial_event_offset)) = codex_prompt_confirmation {
                let mut retry_at = Instant::now() + Duration::from_secs(2);
                let confirmation_deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    if codex_event_stream_has_user_turn(
                        event_stream_path,
                        initial_event_offset,
                        prompt,
                    ) {
                        break;
                    }
                    if !self.session_exists(tmux_session)? {
                        return Ok(false);
                    }
                    if Instant::now() >= confirmation_deadline {
                        anyhow::bail!(
                            "timed out confirming Codex handoff prompt in {}",
                            event_stream_path.display()
                        );
                    }
                    if Instant::now() >= retry_at {
                        self.send_key(tmux_session, "Enter")?;
                        retry_at = Instant::now() + Duration::from_secs(2);
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
        Ok(true)
    }

    pub fn kill_session(&self, tmux_session: &str) -> Result<bool> {
        if !self.session_exists(tmux_session)? {
            return Ok(false);
        }
        self.run_tmux(["kill-session", "-t", tmux_session])?;
        Ok(true)
    }

    /// Tear down a runtime without allowing an unresponsive tmux client to
    /// block recovery of other independent sessions.
    pub fn kill_session_with_timeout(&self, tmux_session: &str, timeout: Duration) -> Result<bool> {
        if !self.session_exists_with_timeout(tmux_session, timeout)? {
            return Ok(false);
        }
        self.run_tmux_with_timeout(["kill-session", "-t", tmux_session], timeout)?;
        Ok(true)
    }

    pub fn set_status_bar(&self, tmux_session: &str, friendly_name: &str) -> Result<bool> {
        if !self.session_exists(tmux_session)? {
            return Ok(false);
        }
        let status_left = format!("[{friendly_name}] ");
        self.run_tmux([
            "set-option",
            "-t",
            tmux_session,
            "status-left",
            status_left.as_str(),
        ])?;
        Ok(true)
    }

    pub fn session_exists(&self, tmux_session: &str) -> Result<bool> {
        let output = self
            .tmux_command(["has-session", "-t", tmux_session])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| "failed to run tmux has-session")?;
        Ok(output.status.success())
    }

    fn session_exists_with_timeout(&self, tmux_session: &str, timeout: Duration) -> Result<bool> {
        Ok(self
            .run_tmux_status_with_timeout(["has-session", "-t", tmux_session], timeout)?
            .success())
    }

    fn create_session_with_bootstrap(&self, spec: &TmuxSessionSpec, command: &str) -> Result<()> {
        self.run_tmux([
            "new-session",
            "-d",
            "-s",
            spec.tmux_session.as_str(),
            "-c",
            spec.working_dir.as_str(),
            "-n",
            "__sm_bootstrap",
        ])?;

        let result = (|| {
            self.ensure_server_options();
            if let Some(history_limit) = self.tmux_history_limit {
                let history_limit = history_limit.to_string();
                self.run_tmux([
                    "set-option",
                    "-t",
                    spec.tmux_session.as_str(),
                    "history-limit",
                    history_limit.as_str(),
                ])?;
            }
            self.run_tmux([
                "new-window",
                "-d",
                "-t",
                spec.tmux_session.as_str(),
                "-n",
                "main",
                "-c",
                spec.working_dir.as_str(),
                command,
            ])?;
            let bootstrap_window = format!("{}:__sm_bootstrap", spec.tmux_session);
            self.run_tmux(["kill-window", "-t", bootstrap_window.as_str()])?;
            let main_window = format!("{}:main", spec.tmux_session);
            self.run_tmux(["select-window", "-t", main_window.as_str()])?;
            Ok(())
        })();

        if result.is_err() {
            let _ = self.kill_session(&spec.tmux_session);
        }
        result
    }

    fn ensure_server_options(&self) {
        if self.socket_name.is_none() {
            return;
        }
        let _ = self.run_tmux(["set-option", "-g", "focus-events", "on"]);
        if !self.tmux_native_scrollback {
            return;
        }
        let current = self
            .tmux_command(["show-options", "-gqv", "terminal-overrides"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).to_string())
            .unwrap_or_default();
        if current.contains("smcup@:rmcup@") {
            return;
        }
        let _ = self.run_tmux([
            "set-option",
            "-as",
            "terminal-overrides",
            ",*:smcup@:rmcup@",
        ]);
    }

    fn pane_in_mode(&self, tmux_session: &str) -> Option<i32> {
        let output = self
            .tmux_command([
                "display-message",
                "-p",
                "-t",
                tmux_session,
                "#{pane_in_mode}",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        match String::from_utf8_lossy(&output.stdout).trim() {
            "0" => Some(0),
            "1" => Some(1),
            _ => None,
        }
    }

    pub fn pane_title(&self, tmux_session: &str) -> Option<String> {
        let output = self
            .tmux_command(["display-message", "-p", "-t", tmux_session, "#{pane_title}"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn exit_copy_mode_if_needed(&self, tmux_session: &str) {
        if self.pane_in_mode(tmux_session) == Some(1) {
            let _ = self.run_tmux(["send-keys", "-t", tmux_session, "-X", "cancel"]);
        }
    }

    fn send_text_then_enter(&self, tmux_session: &str, text: &str) -> Result<()> {
        self.send_text(tmux_session, text)?;
        thread::sleep(self.compute_settle_delay(text));
        self.send_key(tmux_session, "Enter")
    }

    fn send_text(&self, tmux_session: &str, text: &str) -> Result<()> {
        self.exit_copy_mode_if_needed(tmux_session);
        for chunk in split_send_text_chunks(text, self.send_keys_max_chunk_chars) {
            self.run_tmux(["send-keys", "-t", tmux_session, "-l", "--", chunk])?;
        }
        Ok(())
    }

    fn send_key(&self, tmux_session: &str, key: &str) -> Result<()> {
        self.run_tmux(["send-keys", "-t", tmux_session, key])
    }

    fn wait_for_prompt(&self, tmux_session: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.capture_pane_last_line(tmux_session).as_deref() == Some(">") {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_for_fresh_prompt(
        &self,
        tmux_session: &str,
        previous_pane: &str,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.capture_pane_text(tmux_session).is_some_and(|pane| {
                pane != previous_pane && pane_last_line(&pane).as_deref() == Some(">")
            }) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_for_codex_composer(
        &self,
        tmux_session: &str,
        pre_reset_pane: Option<&str>,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.codex_composer_is_ready(tmux_session, pre_reset_pane) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn codex_composer_is_ready(&self, tmux_session: &str, pre_reset_pane: Option<&str>) -> bool {
        let cursor = match self
            .tmux_command(["display-message", "-p", "-t", tmux_session, "#{cursor_y}"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<usize>()
                .ok(),
            _ => None,
        };
        let Some(cursor) = cursor else {
            return false;
        };
        self.capture_pane_text(tmux_session)
            .is_some_and(|text| codex_composer_after_reset(&text, cursor, pre_reset_pane))
    }

    pub fn capture_pane_text(&self, tmux_session: &str) -> Option<String> {
        let output = self
            .tmux_command(["capture-pane", "-p", "-t", tmux_session])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    }

    pub fn accept_codex_directory_trust_prompt(&self, tmux_session: &str) -> Result<bool> {
        let _guard = self.lock_session_input(tmux_session)?;
        if !self.session_exists(tmux_session)? {
            return Ok(false);
        }
        let Some(pane) = self.capture_pane_text(tmux_session) else {
            return Ok(false);
        };
        if !is_codex_directory_trust_prompt(&pane) {
            return Ok(false);
        }
        self.send_key(tmux_session, "Enter")?;
        Ok(true)
    }

    pub fn session_input_ready(&self, tmux_session: &str, provider: &str) -> bool {
        match provider {
            "codex" | "codex-fork" => self.codex_composer_is_ready(tmux_session, None),
            _ => self
                .capture_pane_text(tmux_session)
                .is_some_and(|pane| claude_composer_is_ready(&pane)),
        }
    }

    pub fn session_has_attached_clients(&self, tmux_session: &str) -> Result<bool> {
        let output = self
            .tmux_command(["list-clients", "-t", tmux_session, "-F", "#{client_name}"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .context("failed to list tmux clients")?;
        if !output.status.success() {
            return Ok(false);
        }
        Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
    }

    pub fn capture_pane_tail(&self, tmux_session: &str, lines: usize) -> Option<String> {
        if lines == 0 {
            return Some(String::new());
        }
        let start = format!("-{}", lines.min(500));
        let output = self
            .tmux_command([
                "capture-pane",
                "-p",
                "-S",
                start.as_str(),
                "-t",
                tmux_session,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(tail_text_lines(
            &String::from_utf8_lossy(&output.stdout),
            lines,
        ))
    }

    pub fn send_key_input(&self, tmux_session: &str, key: &str) -> Result<bool> {
        let _guard = self.lock_session_input(tmux_session)?;
        self.send_key_input_while_locked(tmux_session, key)
    }

    pub fn send_key_input_while_locked(&self, tmux_session: &str, key: &str) -> Result<bool> {
        if !self.session_exists(tmux_session)? {
            return Ok(false);
        }
        self.send_key(tmux_session, key)?;
        Ok(true)
    }

    pub fn list_buffer_ids(&self) -> Result<Vec<String>> {
        let output = self
            .tmux_command(["list-buffers", "-F", "#{buffer_name}"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| "failed to list tmux buffers")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("no buffers") {
                return Ok(Vec::new());
            }
            bail!("tmux list-buffers failed: {}", stderr.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    pub fn read_buffer(&self, buffer_id: &str) -> Result<String> {
        let output = self
            .tmux_command(["show-buffer", "-b", buffer_id])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| "failed to read tmux buffer")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux show-buffer failed: {}", stderr.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    pub fn delete_buffer(&self, buffer_id: &str) -> Result<()> {
        self.run_tmux(["delete-buffer", "-b", buffer_id])
    }

    fn capture_pane_last_line(&self, tmux_session: &str) -> Option<String> {
        let text = self.capture_pane_text(tmux_session)?;
        pane_last_line(&text)
    }

    fn run_tmux<'a>(&self, args: impl IntoIterator<Item = &'a str>) -> Result<()> {
        let output = self
            .tmux_command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| "failed to run tmux")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux command failed: {}", stderr.trim());
        }
        Ok(())
    }

    fn run_tmux_with_timeout<'a>(
        &self,
        args: impl IntoIterator<Item = &'a str>,
        timeout: Duration,
    ) -> Result<()> {
        if !self.run_tmux_status_with_timeout(args, timeout)?.success() {
            bail!("tmux command failed");
        }
        Ok(())
    }

    fn run_tmux_status_with_timeout<'a>(
        &self,
        args: impl IntoIterator<Item = &'a str>,
        timeout: Duration,
    ) -> Result<std::process::ExitStatus> {
        let mut child = self
            .tmux_command(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| "failed to run tmux")?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child
                .try_wait()
                .with_context(|| "failed to wait for tmux")?
            {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("tmux command timed out after {}ms", timeout.as_millis());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn tmux_command<'a>(&self, args: impl IntoIterator<Item = &'a str>) -> Command {
        let mut command = Command::new(&self.tmux_binary);
        if let Some(socket_name) = &self.socket_name {
            command.arg("-L").arg(socket_name);
        }
        command.args(args);
        command
    }

    fn launch_command(&self, spec: &TmuxSessionSpec, prompt_mode: &str) -> Result<String> {
        let mut parts = match spec.provider.as_str() {
            "claude" => command_parts(&self.claude_command, &self.claude_args),
            "codex" => command_parts(&self.codex_command, &self.codex_args),
            "codex-fork" => self.codex_fork_command_parts(spec)?,
            provider => bail!("Rust runtime does not support provider {provider}"),
        };
        let model = spec
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                if spec.provider == "codex" {
                    self.codex_default_model
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                } else if spec.provider == "codex-fork" {
                    self.codex_fork_default_model
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                } else {
                    None
                }
            });
        if let Some(model) = model {
            parts.push("--model".to_owned());
            parts.push(shell_quote(model));
        }
        if let Some(effort) = spec
            .reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if spec.provider == "claude" {
                parts.push("--effort".to_owned());
                parts.push(shell_quote(effort));
            } else {
                parts.push("-c".to_owned());
                parts.push(shell_quote(&format!("model_reasoning_effort={effort}")));
            }
        }
        if prompt_mode == "argv" {
            if let Some(initial_message) = spec
                .initial_message
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                parts.push("--".to_owned());
                parts.push(shell_quote(initial_message));
            }
        }
        Ok(parts.join(" "))
    }

    fn codex_fork_command_parts(&self, spec: &TmuxSessionSpec) -> Result<Vec<String>> {
        validate_launch_command(&self.codex_fork_command, Path::new(&spec.working_dir))?;
        let (event_stream_path, control_socket_path) = codex_fork_artifact_paths(spec)?;
        prepare_codex_fork_runtime_artifacts(&event_stream_path, &control_socket_path)?;
        let mut parts = executable_command_parts(&self.codex_fork_command, &self.codex_fork_args);
        parts.extend([
            "--event-stream".to_owned(),
            shell_quote_path(&event_stream_path),
            "--event-schema-version".to_owned(),
            shell_quote(&self.codex_fork_event_schema_version.to_string()),
            "--control-socket".to_owned(),
            shell_quote_path(&control_socket_path),
        ]);
        Ok(parts)
    }

    fn attach_session_log(&self, spec: &TmuxSessionSpec, prompt_mode: &str) -> Result<()> {
        let initial_stdin_prompt = initial_stdin_prompt(spec, prompt_mode);
        let pipe_command = format!("cat >> {}", shell_quote_path(&spec.log_file));
        if let Err(error) = self.run_tmux([
            "pipe-pane",
            "-t",
            spec.tmux_session.as_str(),
            pipe_command.as_str(),
        ]) {
            if initial_stdin_prompt.is_some() && is_tmux_session_gone_error(&error) {
                bail!("tmux session exited before initial prompt could be delivered");
            }
            return Err(error);
        }

        if let Some(initial_message) = initial_stdin_prompt {
            if spec.force_initial_prompt_stdin {
                self.deliver_verified_initial_brief(spec, initial_message)?;
            } else {
                thread::sleep(Duration::from_millis(self.start_settle_ms));
                match self.send_input(&spec.tmux_session, initial_message) {
                    Ok(true) => {}
                    Ok(false) => {
                        bail!("tmux session exited before initial prompt could be delivered");
                    }
                    Err(error) if is_tmux_session_gone_error(&error) => {
                        bail!("tmux session exited before initial prompt could be delivered");
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    /// Deliver an immutable spawn brief only after the provider exposes a
    /// usable composer, and only report success after provider-side evidence.
    ///
    /// This deliberately does not retry after submission: a missing event can
    /// mean the first submission was accepted, so another paste could run the
    /// brief twice.
    fn deliver_verified_initial_brief(&self, spec: &TmuxSessionSpec, prompt: &str) -> Result<()> {
        let _guard = self.lock_session_input(&spec.tmux_session)?;
        let claude_ready_pane =
            self.wait_for_initial_brief_readiness(&spec.tmux_session, &spec.provider)?;

        match spec.provider.as_str() {
            "codex-fork" => {
                let artifacts = self
                    .codex_fork_runtime_artifacts(spec)?
                    .expect("codex-fork must have runtime artifacts");
                let initial_event_offset = fs::metadata(&artifacts.event_stream_path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
                if !self.session_exists(&spec.tmux_session)? {
                    return Err(InitialBriefDeliveryError::SessionExited {
                        provider: spec.provider.clone(),
                    }
                    .into());
                }
                self.send_text_then_enter(&spec.tmux_session, prompt)?;
                self.wait_for_codex_fork_initial_brief_acceptance(
                    &spec.tmux_session,
                    &artifacts.event_stream_path,
                    initial_event_offset,
                    prompt,
                )
            }
            "claude" => {
                let ready_pane = claude_ready_pane.expect("Claude readiness returns its pane");
                if self.session_has_attached_clients(&spec.tmux_session)? {
                    bail!("refusing to submit the initial Claude spawn brief while a tmux client is attached");
                }
                self.send_text_then_enter(&spec.tmux_session, prompt)?;
                self.wait_for_claude_initial_brief_acceptance(&spec.tmux_session, &ready_pane)
            }
            _ => Err(
                InitialBriefDeliveryError::ProviderAcknowledgementUnavailable {
                    provider: spec.provider.clone(),
                }
                .into(),
            ),
        }
    }

    fn wait_for_initial_brief_readiness(
        &self,
        tmux_session: &str,
        provider: &str,
    ) -> Result<Option<ClaudeInitialBriefReadyPane>> {
        let deadline = Instant::now() + self.initial_brief_ready_timeout;
        let mut directory_trust_accepted = false;
        loop {
            if !self.session_exists(tmux_session)? {
                return Err(InitialBriefDeliveryError::SessionExited {
                    provider: provider.to_owned(),
                }
                .into());
            }
            if !directory_trust_accepted && matches!(provider, "codex" | "codex-fork") {
                if self
                    .capture_pane_text(tmux_session)
                    .is_some_and(|pane| is_codex_directory_trust_prompt(&pane))
                {
                    self.send_key(tmux_session, "Enter")?;
                    directory_trust_accepted = true;
                }
            }
            if provider == "claude" {
                if let Some(pane) = self.claude_initial_brief_composer_pane(tmux_session) {
                    return Ok(Some(pane));
                }
            } else if self.session_input_ready(tmux_session, provider) {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                return Err(InitialBriefDeliveryError::ProviderReadinessTimedOut {
                    provider: provider.to_owned(),
                }
                .into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Return a Claude pane only when its composer is empty and still owned by
    /// the provider.  Claude 2.1.226 renders the empty field as an inline
    /// `Try "..."` suggestion, so pane text alone cannot distinguish it from
    /// typed input.  The cursor must still be immediately after the composer
    /// prefix on that exact row before an immutable brief may be submitted.
    fn claude_initial_brief_composer_pane(
        &self,
        tmux_session: &str,
    ) -> Option<ClaudeInitialBriefReadyPane> {
        let pane = self.capture_pane_text(tmux_session)?;
        if !claude_composer_is_ready(&pane) {
            return None;
        }
        let (cursor_x, cursor_y) = self.pane_cursor_position(tmux_session)?;
        claude_empty_composer_cursor(&pane, cursor_x, cursor_y).then_some(
            ClaudeInitialBriefReadyPane {
                pane,
                composer_y: cursor_y,
            },
        )
    }

    fn pane_cursor_position(&self, tmux_session: &str) -> Option<(usize, usize)> {
        let output = self
            .tmux_command([
                "display-message",
                "-p",
                "-t",
                tmux_session,
                "#{cursor_x},#{cursor_y}",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let cursor = String::from_utf8_lossy(&output.stdout);
        let (x, y) = cursor.trim().split_once(',')?;
        Some((x.parse().ok()?, y.parse().ok()?))
    }

    fn wait_for_codex_fork_initial_brief_acceptance(
        &self,
        tmux_session: &str,
        event_stream_path: &Path,
        initial_event_offset: u64,
        prompt: &str,
    ) -> Result<()> {
        let deadline = Instant::now() + self.initial_brief_ack_timeout;
        loop {
            if codex_event_stream_has_user_turn(event_stream_path, initial_event_offset, prompt) {
                return Ok(());
            }
            if !self.session_exists(tmux_session)? {
                return Err(InitialBriefDeliveryError::SessionExited {
                    provider: "codex-fork".to_owned(),
                }
                .into());
            }
            if Instant::now() >= deadline {
                return Err(InitialBriefDeliveryError::ProviderAcceptanceTimedOut {
                    provider: "codex-fork".to_owned(),
                }
                .into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Claude has no structured event stream.  Its only provider-originated
    /// acknowledgement is the composer leaving the verified empty state and
    /// the main turn entering its active UI state.  We do not resend if that
    /// transition is missing: the original Enter may already have been seen.
    fn wait_for_claude_initial_brief_acceptance(
        &self,
        tmux_session: &str,
        ready_pane: &ClaudeInitialBriefReadyPane,
    ) -> Result<()> {
        let deadline = Instant::now() + self.initial_brief_ack_timeout;
        loop {
            if self.capture_pane_text(tmux_session).is_some_and(|pane| {
                claude_initial_brief_submission_observed(
                    &ready_pane.pane,
                    ready_pane.composer_y,
                    &pane,
                )
            }) {
                return Ok(());
            }
            if !self.session_exists(tmux_session)? {
                return Err(InitialBriefDeliveryError::SessionExited {
                    provider: "claude".to_owned(),
                }
                .into());
            }
            if Instant::now() >= deadline {
                return Err(InitialBriefDeliveryError::ProviderAcceptanceTimedOut {
                    provider: "claude".to_owned(),
                }
                .into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn compute_settle_delay(&self, text: &str) -> Duration {
        let base = self.send_keys_settle_ms;
        let max_delay = base.max(self.send_keys_settle_max_ms);
        let text_len = text.chars().count();
        let line_count = text.matches('\n').count() + 1;
        if text_len <= 512 && line_count <= 1 {
            return duration_from_millis(base);
        }

        let extra = ((text_len.saturating_sub(512) as f64) / 1024.0)
            * self.send_keys_settle_per_ki_ms
            + (line_count.saturating_sub(1) as f64) * self.send_keys_settle_per_extra_line_ms;
        duration_from_millis((base + extra).clamp(base, max_delay))
    }
}

fn codex_composer_after_reset(pane: &str, cursor_y: usize, pre_reset_pane: Option<&str>) -> bool {
    pre_reset_pane != Some(pane)
        && pane
            .lines()
            .nth(cursor_y)
            .is_some_and(|line| line.trim_start().starts_with('›'))
}

fn codex_event_stream_has_user_turn(path: &Path, initial_offset: u64, prompt: &str) -> bool {
    let Ok(mut file) = fs::OpenOptions::new().read(true).open(path) else {
        return false;
    };
    let read_offset = initial_offset.saturating_sub(1);
    if file.seek(SeekFrom::Start(read_offset)).is_err() {
        return false;
    }
    let mut content = Vec::new();
    if file.read_to_end(&mut content).is_err() {
        return false;
    }
    let start = if initial_offset == 0 {
        0
    } else if content.first() == Some(&b'\n') {
        1
    } else {
        let Some(relative_newline) = content.iter().position(|byte| *byte == b'\n') else {
            return false;
        };
        relative_newline + 1
    };
    let Ok(content) = std::str::from_utf8(&content[start..]) else {
        return false;
    };
    content.lines().any(|line| {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        let legacy_user_turn_matches = event
            .get("payload")
            .and_then(|payload| payload.get("UserTurn"))
            .and_then(codex_user_turn_text)
            .is_some_and(|text| codex_brief_text_matches(&text, prompt));
        let schema_v2_user_message_matches = event.get("schema_version").and_then(Value::as_u64)
            == Some(2)
            && event.get("event_type").and_then(Value::as_str) == Some("item/started")
            && event.pointer("/payload/item/type").and_then(Value::as_str) == Some("userMessage")
            && event
                .pointer("/payload/item/content")
                .and_then(codex_user_message_text)
                .is_some_and(|text| codex_brief_text_matches(&text, prompt));

        legacy_user_turn_matches || schema_v2_user_message_matches
    })
}

/// Match a provider acknowledgement to the immutable submitted brief.
///
/// Codex currently normalizes one terminal line-feed out of initial prompts.
/// Accept that one observed normalization and nothing broader: a provider value
/// otherwise has to equal the entire submitted brief exactly.
fn codex_brief_text_matches(provider_text: &str, submitted_brief: &str) -> bool {
    provider_text == submitted_brief
        || submitted_brief
            .strip_suffix('\n')
            .is_some_and(|without_terminal_newline| provider_text == without_terminal_newline)
}

/// Return the complete textual content of a schema-v2 `userMessage` item.
///
/// Acknowledgement is deliberately strict: every content part must be a text
/// part, and their concatenation must equal the submitted brief. This prevents
/// a matching substring, an agent message, or mixed content from confirming a
/// different prompt.
fn codex_user_message_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => codex_text_parts(parts),
        _ => None,
    }
}

/// Return the complete text represented by a retained legacy `UserTurn`.
fn codex_user_turn_text(turn: &Value) -> Option<String> {
    turn.get("items")
        .and_then(Value::as_array)
        .and_then(|parts| codex_text_parts(parts))
}

/// Normalize text-only event parts without accepting a matching fragment.
fn codex_text_parts(parts: &[Value]) -> Option<String> {
    parts.iter().try_fold(String::new(), |mut text, part| {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            return None;
        }
        text.push_str(part.get("text").and_then(Value::as_str)?);
        Some(text)
    })
}

fn split_send_text_chunks(text: &str, max_chunk_chars: usize) -> Vec<&str> {
    let max_chunk_chars = max_chunk_chars.max(1);
    if text.chars().count() <= max_chunk_chars {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.chars().count() <= max_chunk_chars {
            chunks.push(remaining);
            break;
        }

        let boundary = byte_index_after_chars(remaining, max_chunk_chars);
        let half_chars = max_chunk_chars / 2;
        let newline_split = remaining[..boundary].rfind('\n').and_then(|idx| {
            if remaining[..idx].chars().count() >= half_chars {
                Some(idx + '\n'.len_utf8())
            } else {
                None
            }
        });
        let split_at = newline_split.unwrap_or(boundary);
        chunks.push(&remaining[..split_at]);
        remaining = &remaining[split_at..];
    }
    chunks
}

fn tail_text_lines(text: &str, lines: usize) -> String {
    if lines == 0 {
        return String::new();
    }
    let mut rows = text.lines().collect::<Vec<_>>();
    while rows.last().is_some_and(|row| row.trim().is_empty()) {
        rows.pop();
    }
    if rows.is_empty() {
        return String::new();
    }
    let start = rows.len().saturating_sub(lines);
    let mut output = rows[start..].join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    output
}

fn byte_index_after_chars(value: &str, char_count: usize) -> usize {
    value
        .char_indices()
        .nth(char_count)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len())
}

fn finite_nonnegative_or_default(value: Option<f64>, default: f64) -> f64 {
    value
        .filter(|candidate| candidate.is_finite() && *candidate >= 0.0)
        .unwrap_or(default)
}

fn duration_from_millis(millis: f64) -> Duration {
    Duration::from_secs_f64((millis.max(0.0)) / 1000.0)
}

fn duration_from_seconds(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds.max(0.0))
}

fn is_tmux_session_gone_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("no server running")
        || message.contains("can't find session")
        || message.contains("no current target")
        || message.contains("server exited unexpectedly")
}

fn shell_quote_path(path: &Path) -> String {
    shell_quote(&path.display().to_string())
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn pane_last_line(text: &str) -> Option<String> {
    text.trim_end_matches('\n')
        .split('\n')
        .last()
        .map(str::trim)
        .map(ToOwned::to_owned)
}

/// Claude renders a status/footer block below its live composer. Looking only
/// at the final pane line therefore treats every normal idle Claude session as
/// busy. Claude 2.1.226 also displays a dim inline `Try "..."` suggestion in a
/// new empty composer. A live composer is either bare or that exact suggestion,
/// and must be immediately followed by Claude's chrome divider; an older prompt
/// followed by a spinner or response row is not a safe slash-command target.
fn claude_composer_is_ready(pane: &str) -> bool {
    let lines = pane.lines().map(str::trim).collect::<Vec<_>>();
    let Some(composer_index) = lines
        .iter()
        .rposition(|line| claude_composer_line_is_ready(line))
    else {
        return false;
    };
    let trailing = lines[composer_index + 1..]
        .iter()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    match trailing.first() {
        None => true,
        Some(line) => {
            claude_composer_footer_divider(line)
                && trailing
                    .iter()
                    .skip(1)
                    .all(|line| !claude_line_indicates_main_thread_activity(line))
        }
    }
}

fn claude_composer_line_is_ready(line: &str) -> bool {
    matches!(line, ">" | "❯") || claude_inline_placeholder(line)
}

/// Match the one observed Claude empty-composer rendering, not arbitrary text
/// beginning with the composer glyph.  `claude_empty_composer_cursor` adds the
/// second guard required before an initial spawn brief is submitted.
fn claude_inline_placeholder(line: &str) -> bool {
    let Some(placeholder) = line.strip_prefix('❯').or_else(|| line.strip_prefix('>')) else {
        return false;
    };
    let placeholder = placeholder.trim_start();
    placeholder.starts_with(concat!("Try ", "\""))
        && placeholder.ends_with('"')
        && placeholder.len() > concat!("Try ", "\"").len()
}

/// Confirm that tmux's cursor is immediately after the visible composer
/// prefix.  A user-typed string, including one that resembles Claude's `Try
/// "..."` placeholder, places the cursor farther right and is rejected.
fn claude_empty_composer_cursor(pane: &str, cursor_x: usize, cursor_y: usize) -> bool {
    let Some(line) = pane.lines().nth(cursor_y) else {
        return false;
    };
    let leading = line.len() - line.trim_start().len();
    let composer = &line[leading..];
    let prefix_width = if composer.starts_with(">") {
        1
    } else if composer.starts_with("❯\u{a0}") {
        2
    } else if composer.starts_with('❯') {
        1
    } else {
        return false;
    };
    claude_composer_line_is_ready(composer.trim()) && cursor_x == leading + prefix_width
}

fn claude_initial_brief_submission_observed(
    ready_pane: &str,
    composer_y: usize,
    pane: &str,
) -> bool {
    if pane == ready_pane {
        return false;
    }
    let Some(mut previous_markers) = claude_main_thread_activity_markers(ready_pane, composer_y)
    else {
        return false;
    };
    let Some(current_markers) = claude_main_thread_activity_markers(pane, composer_y) else {
        return false;
    };
    current_markers.into_iter().any(|marker| {
        match previous_markers
            .iter()
            .position(|previous| *previous == marker)
        {
            Some(index) => {
                previous_markers.remove(index);
                false
            }
            None => true,
        }
    })
}

/// Return a multiset of provider activity rows.  A ready pane can retain old
/// completed-turn chrome above its new composer, so acknowledgement must
/// observe an activity row that did not already exist before Enter.
fn claude_main_thread_activity_markers(pane: &str, composer_y: usize) -> Option<Vec<&str>> {
    let lines = pane.lines().collect::<Vec<_>>();
    let composer = lines.get(composer_y)?.trim_start();
    if !(composer.starts_with('❯') || composer.starts_with('>')) {
        return None;
    }
    // The cursor-confirmed row is the actual boundary of the live input.
    // Everything at or below it may be multiline prompt text, including
    // quotes and rows that resemble activity. Provider turn rows render
    // above the stationary composer. If that layout changes, fail closed.
    Some(
        lines
            .iter()
            .take(composer_y)
            .map(|line| line.trim_start())
            .filter(|line| claude_line_indicates_main_thread_activity(line))
            .collect(),
    )
}

fn claude_composer_footer_divider(line: &str) -> bool {
    let non_whitespace = line
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<Vec<_>>();
    non_whitespace.len() >= 3
        && non_whitespace
            .iter()
            .all(|ch| matches!(ch, '-' | '─' | '━' | '═' | '╌' | '╍'))
}

fn claude_line_indicates_main_thread_activity(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains("esc to interrupt")
        || trimmed.starts_with('⏺')
        || trimmed.starts_with('⎿')
        || trimmed.starts_with('·')
        || trimmed
            .chars()
            .next()
            .is_some_and(|ch| (0x2700..=0x27bf).contains(&(ch as u32)))
}

fn command_parts(command: &str, args: &[String]) -> Vec<String> {
    let mut parts = vec![command.to_owned()];
    parts.extend(args.iter().map(|arg| shell_quote(arg)));
    parts
}

fn executable_command_parts(command: &str, args: &[String]) -> Vec<String> {
    let mut parts = vec![shell_quote(command)];
    parts.extend(args.iter().map(|arg| shell_quote(arg)));
    parts
}

fn prepend_arg_pair(command: &str, value: &str, args: &[String]) -> Vec<String> {
    let mut prefixed = vec![command.to_owned(), value.to_owned()];
    prefixed.extend(args.iter().cloned());
    prefixed
}

fn codex_fork_artifact_paths(spec: &TmuxSessionSpec) -> Result<(PathBuf, PathBuf)> {
    let artifact_dir = spec
        .log_file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("runtime session missing log directory"))?;
    let artifact_basename = safe_session_artifact_basename(&spec.session_id);
    Ok((
        artifact_dir.join(format!("{artifact_basename}.codex-fork.events.jsonl")),
        artifact_dir.join(format!("{artifact_basename}.codex-fork.control.sock")),
    ))
}

fn prepare_codex_fork_runtime_artifacts(event_path: &Path, control_path: &Path) -> Result<()> {
    let parent = event_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("codex-fork event stream path missing parent"))?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create codex-fork artifact dir {}",
            parent.display()
        )
    })?;
    remove_file_if_exists(event_path)?;
    remove_file_if_exists(control_path)?;
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn validate_launch_command(command: &str, working_dir: &Path) -> Result<()> {
    resolve_launch_command(command, working_dir).map(|_| ())
}

fn resolve_launch_command(command: &str, working_dir: &Path) -> Result<PathBuf> {
    let command = command.trim();
    if command.is_empty() {
        bail!("Launch command is empty");
    }
    if command.starts_with('~') || command.contains('/') {
        let candidate = expand_launch_path(command, working_dir);
        if !candidate.exists() {
            bail!("Launch command does not exist: {}", candidate.display());
        }
        if !candidate.is_file() {
            bail!("Launch command is not a file: {}", candidate.display());
        }
        if !is_executable_file(&candidate) {
            bail!("Launch command is not executable: {}", candidate.display());
        }
        return fs::canonicalize(&candidate).with_context(|| {
            format!(
                "failed to resolve launch command to an absolute path: {}",
                candidate.display()
            )
        });
    }
    let candidate = find_in_path(command)
        .ok_or_else(|| anyhow::anyhow!("Launch command not found on PATH: {command}"))?;
    fs::canonicalize(&candidate).with_context(|| {
        format!(
            "failed to resolve launch command to an absolute path: {}",
            candidate.display()
        )
    })
}

fn command_output_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> Result<std::process::Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("failed to start model catalog command")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("model catalog stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("model catalog stderr was not captured"))?;
    let stdout_reader = spawn_stream_reader(stdout);
    let stderr_reader = spawn_stream_reader(stderr);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context("failed to inspect model catalog command")?
        {
            break status;
        }
        if started.elapsed() >= timeout {
            terminate_catalog_process(&mut child);
            bail!(
                "model catalog command timed out after {:.1}s",
                timeout.as_secs_f64()
            );
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout =
        receive_stream_output(&stdout_reader, started, timeout, "stdout").map_err(|error| {
            terminate_catalog_process(&mut child);
            error
        })?;
    let stderr =
        receive_stream_output(&stderr_reader, started, timeout, "stderr").map_err(|error| {
            terminate_catalog_process(&mut child);
            error
        })?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn spawn_stream_reader(
    mut stream: impl Read + Send + 'static,
) -> mpsc::Receiver<std::io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stream.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    receiver
}

fn receive_stream_output(
    receiver: &mpsc::Receiver<std::io::Result<Vec<u8>>>,
    started: Instant,
    timeout: Duration,
    name: &str,
) -> Result<Vec<u8>> {
    let remaining = timeout.saturating_sub(started.elapsed());
    receiver
        .recv_timeout(remaining)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => anyhow::anyhow!(
                "model catalog command timed out after {:.1}s while draining {name}",
                timeout.as_secs_f64()
            ),
            mpsc::RecvTimeoutError::Disconnected => {
                anyhow::anyhow!("model catalog {name} reader stopped unexpectedly")
            }
        })?
        .with_context(|| format!("failed to read model catalog {name}"))
}

fn terminate_catalog_process(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", &format!("-{}", child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn truncate_for_error(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn expand_launch_path(command: &str, working_dir: &Path) -> PathBuf {
    let path = if command == "~" {
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(command))
    } else if let Some(rest) = command.strip_prefix("~/") {
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(rest)
    } else {
        PathBuf::from(command)
    };
    if path.is_absolute() {
        path
    } else {
        working_dir.join(path)
    }
}

fn find_in_path(command: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|path| path.is_file() && is_executable_file(path))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn safe_session_artifact_basename(session_id: &str) -> String {
    format!(
        "{}-{}",
        sanitize_path_component(session_id),
        stable_session_id_hash(session_id)
    )
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

fn managed_session_command(
    command: &str,
    session_id: &str,
    session_credential: Option<&str>,
) -> String {
    let session_id = shell_quote(session_id);
    let credential_export = session_credential
        .map(shell_quote)
        .map(|credential| format!("export SM_SESSION_CREDENTIAL={credential}; "))
        .unwrap_or_default();
    format!(
        "export SESSION_MANAGER_ID={session_id}; \
         export CLAUDE_SESSION_MANAGER_ID={session_id}; \
         {credential_export}\
         unset CLAUDECODE; \
         export ENABLE_TOOL_SEARCH=false; \
         {command}"
    )
}

fn is_codex_directory_trust_prompt(pane: &str) -> bool {
    pane.contains("Do you trust the contents of this directory?")
        && pane.contains("Yes, continue")
        && pane.contains("No, quit")
        && pane.contains("Press enter to continue")
}

fn initial_stdin_prompt<'a>(spec: &'a TmuxSessionSpec, prompt_mode: &str) -> Option<&'a str> {
    if prompt_mode != "stdin" {
        return None;
    }
    let prompt = spec.initial_message.as_deref()?;
    if spec.force_initial_prompt_stdin {
        // Accepted briefs have already been validated as non-blank. Do not
        // trim them here: their recorded digest binds the exact UTF-8 bytes.
        (!prompt.is_empty()).then_some(prompt)
    } else {
        let trimmed = prompt.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn verified_spawn_stdin_prompt_preserves_outer_whitespace() {
        let prompt = "  # Exact brief\n\n  indented detail  \n";
        let spec = TmuxSessionSpec {
            session_id: "abc12345".to_owned(),
            session_credential: None,
            tmux_session: "sm-test".to_owned(),
            working_dir: "/tmp".to_owned(),
            log_file: PathBuf::from("/tmp/session.log"),
            provider: "claude".to_owned(),
            initial_message: Some(prompt.to_owned()),
            force_initial_prompt_stdin: true,
            model: None,
            reasoning_effort: None,
        };

        assert_eq!(initial_stdin_prompt(&spec, "stdin"), Some(prompt));
    }

    #[test]
    fn codex_model_validation_uses_selectable_models_from_live_catalog() {
        let (command, working_dir) = fake_codex_model_command(
            r#"printf '%s' '{"models":[{"slug":"gpt-5.6-luna","visibility":"list"},{"slug":"hidden-review","visibility":"hide"},{"slug":"gpt-5.6-sol","visibility":"list"}]}'"#,
        );
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        runtime.codex_fork_command = command.display().to_string();
        runtime.codex_fork_args = vec!["--configured-arg".to_owned()];

        runtime
            .validate_codex_fork_model("gpt-5.6-luna", &working_dir)
            .unwrap();
        let error = runtime
            .validate_codex_fork_model("luna", &working_dir)
            .unwrap_err();
        let validation = error.downcast_ref::<CodexModelValidationError>().unwrap();
        assert!(matches!(
            validation,
            CodexModelValidationError::Unsupported { requested, supported }
                if requested == "luna"
                    && supported == &["gpt-5.6-luna".to_owned(), "gpt-5.6-sol".to_owned()]
        ));
        assert_eq!(
            error.to_string(),
            "Unsupported Codex model: luna. Supported models: gpt-5.6-luna, gpt-5.6-sol"
        );
    }

    #[test]
    fn codex_model_validation_drains_large_catalog_output() {
        let padding = "x".repeat(128 * 1024);
        let script = format!(
            "printf '%s' '{}'",
            serde_json::json!({
                "padding": padding,
                "models": [{"slug": "gpt-5.6-terra", "visibility": "list"}],
            })
        );
        let (command, working_dir) = fake_codex_model_command(&script);
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        runtime.codex_fork_command = command.display().to_string();

        runtime
            .validate_codex_fork_model("gpt-5.6-terra", &working_dir)
            .unwrap();
    }

    #[test]
    fn codex_model_validation_resolves_relative_command_before_changing_directory() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let relative_working_dir = PathBuf::from(format!(
            "target/sm-relative-codex-models-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&relative_working_dir).unwrap();
        let command = relative_working_dir.join("codex");
        fs::write(
            &command,
            r#"#!/bin/sh
printf '%s' '{"models":[{"slug":"gpt-5.6-luna","visibility":"list"}]}'
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&command).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&command, permissions).unwrap();
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        runtime.codex_fork_command = "./codex".to_owned();

        runtime
            .validate_codex_fork_model("gpt-5.6-luna", &relative_working_dir)
            .unwrap();

        fs::remove_dir_all(relative_working_dir).unwrap();
    }

    #[test]
    fn codex_model_validation_times_out() {
        let (command, working_dir) = fake_codex_model_command("sleep 1 & wait");
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        runtime.codex_fork_command = command.display().to_string();
        let started = Instant::now();

        let error = runtime
            .validate_codex_fork_model_with_timeout(
                "gpt-5.6-luna",
                &working_dir,
                Duration::from_millis(25),
            )
            .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<CodexModelValidationError>(),
            Some(CodexModelValidationError::DiscoveryUnavailable(detail))
                if detail.contains("timed out")
        ));
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn split_send_text_chunks_prefers_newline_after_half_chunk() {
        let chunks = split_send_text_chunks("abcd\nefghij", 8);
        assert_eq!(chunks, vec!["abcd\n", "efghij"]);
    }

    #[test]
    fn split_send_text_chunks_preserves_utf8_boundaries() {
        let chunks = split_send_text_chunks("åßçdé", 2);
        assert_eq!(chunks, vec!["åß", "çd", "é"]);
    }

    #[test]
    fn session_input_lock_serializes_the_same_tmux_target() {
        let runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        let session = "lock-test".to_owned();
        let guard = runtime.lock_session_input(&session).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let contender_runtime = runtime.clone();
        let contender_session = session.clone();
        let contender = thread::spawn(move || {
            let _guard = contender_runtime
                .lock_session_input(&contender_session)
                .unwrap();
            sender.send(()).unwrap();
        });

        assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
        drop(guard);
        receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        contender.join().unwrap();
    }

    #[test]
    fn capture_pane_tail_requests_rendered_scrollback_and_bounds_rows() {
        let (tmux_binary, log_path, _temp_dir) = fake_tmux_binary();
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        runtime.tmux_binary = tmux_binary.display().to_string();

        let output = runtime.capture_pane_tail("sm-test", 1).unwrap();

        assert_eq!(output, ">\n");
        let log = fs::read_to_string(log_path).unwrap();
        assert!(log.contains("capture-pane -p -S -1 -t sm-test"));
    }

    #[test]
    fn bounded_kill_session_does_not_wait_on_an_unresponsive_tmux_client() {
        let (tmux_binary, _log_path, _temp_dir) = fake_tmux_binary();
        fs::write(
            &tmux_binary,
            r#"#!/bin/sh
while :; do :; done
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux_binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux_binary, permissions).unwrap();
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        runtime.tmux_binary = tmux_binary.display().to_string();
        let started = Instant::now();

        let error = runtime
            .kill_session_with_timeout("sm-test", Duration::from_millis(25))
            .unwrap_err();

        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn claude_input_readiness_requires_the_live_composer_not_scrollback() {
        let (tmux_binary, _log_path, temp_dir) = fake_tmux_binary();
        let pane_path = temp_dir.join("pane");
        fs::write(&pane_path, "❯ prior prompt\n✽ Thinking through the task…\n").unwrap();
        fs::write(
            &tmux_binary,
            format!(
                r#"#!/bin/sh
if [ "$1" = "-L" ]; then
  shift 2
fi
case "$1" in
  capture-pane) cat "{}"; exit 0 ;;
  *) exit 0 ;;
esac
"#,
                pane_path.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux_binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux_binary, permissions).unwrap();

        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        runtime.tmux_binary = tmux_binary.display().to_string();
        assert!(!runtime.session_input_ready("sm-test", "claude"));

        fs::write(
            &pane_path,
            "✻ Finished\n────────────────────\n❯\n────────────────────\nFable 5\n⏵⏵ bypass permissions on\n",
        )
        .unwrap();
        assert!(runtime.session_input_ready("sm-test", "claude"));
    }

    #[test]
    fn claude_composer_readiness_rejects_a_stale_prompt_before_running_output() {
        let pane = "❯\n────────────────────\nFable 5\n✽ Thinking through the task…\n";
        assert!(!claude_composer_is_ready(pane));
    }

    #[test]
    fn claude_placeholder_readiness_requires_the_empty_composer_cursor() {
        let pane = concat!(
            "○ low · /effort\n",
            "────────────────────\n",
            "❯\u{a0}Try \"how does this work?\"\n",
            "────────────────────\n",
            "Fable 5\n",
            "⏵⏵ bypass permissions on\n",
        );

        assert!(claude_composer_is_ready(pane));
        assert!(claude_empty_composer_cursor(pane, 2, 2));
        assert!(!claude_empty_composer_cursor(pane, 27, 2));

        let typed = pane.replace(
            "❯\u{a0}Try \"how does this work?\"",
            "❯ deploy --production",
        );
        assert!(!claude_composer_is_ready(&typed));
    }

    #[test]
    fn claude_initial_brief_acceptance_requires_a_provider_turn_transition() {
        let ready = "Idle header\n❯\n────────────────────\nFable 5\n";
        assert!(!claude_initial_brief_submission_observed(
            ready,
            1,
            "Idle header\n❯\n────────────────────\nFable 5\nupdated idle footer\n",
        ));
        assert!(!claude_initial_brief_submission_observed(
            ready,
            1,
            "Idle header\n❯\n────────────────────\nFable 5\n",
        ));
        assert!(claude_initial_brief_submission_observed(
            ready,
            1,
            "✽ Thinking through the task…\n❯\n────────────────────\nFable 5\n",
        ));
    }

    #[test]
    fn claude_initial_brief_acceptance_rejects_stale_activity_from_the_ready_pane() {
        let ready = concat!(
            "⏺ Completed startup housekeeping\n",
            "❯\n",
            "────────────────────\n",
            "Fable 5\n",
        );

        assert!(!claude_initial_brief_submission_observed(
            ready,
            1,
            concat!(
                "⏺ Completed startup housekeeping\n",
                "❯ pasted-but-not-submitted\n",
                "────────────────────\n",
                "Fable 5\n",
            ),
        ));
        assert!(claude_initial_brief_submission_observed(
            ready,
            1,
            concat!(
                "✽ Thinking through the initial brief…\n",
                "❯\n",
                "────────────────────\n",
                "Fable 5\n",
            ),
        ));
    }

    #[test]
    fn claude_initial_brief_acceptance_rejects_marker_shaped_multiline_input() {
        let ready = concat!(
            "⏺ Completed startup housekeeping\n",
            "❯\n",
            "────────────────────\n",
            "Fable 5\n",
        );

        // A failed Enter leaves every continuation row in Claude's composer.
        // The second row deliberately resembles a provider activity marker.
        assert!(!claude_initial_brief_submission_observed(
            ready,
            1,
            concat!(
                "⏺ Completed startup housekeeping\n",
                "❯ first line of immutable brief\n",
                "✽ marker-shaped continuation, still unsubmitted\n",
                "────────────────────\n",
                "Fable 5\n",
            ),
        ));
    }

    #[test]
    fn claude_initial_brief_acceptance_anchors_input_at_the_ready_cursor_row() {
        let ready = concat!(
            "⏺ Completed startup housekeeping\n",
            "❯\n",
            "────────────────────\n",
            "Fable 5\n",
        );

        assert!(!claude_initial_brief_submission_observed(
            ready,
            1,
            concat!(
                "⏺ Completed startup housekeeping\n",
                "❯ first line of immutable brief\n",
                "✽ marker-shaped continuation, still unsubmitted\n",
                "> quoted continuation, still unsubmitted\n",
                "────────────────────\n",
                "Fable 5\n",
            ),
        ));
    }

    #[test]
    fn codex_directory_trust_prompt_is_accepted_without_matching_incidental_text() {
        let (tmux_binary, log_path, _temp_dir) = fake_tmux_binary();
        fs::write(
            &tmux_binary,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> "{}"
case "$1" in
  has-session) exit 0 ;;
  capture-pane)
    printf '%s\n' '> You are in /repo' \
      'Do you trust the contents of this directory?' \
      '1. Yes, continue' '2. No, quit' 'Press enter to continue'
    exit 0
    ;;
  *) exit 0 ;;
esac
"#,
                log_path.display(),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux_binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux_binary, permissions).unwrap();
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        runtime.tmux_binary = tmux_binary.display().to_string();

        assert!(runtime
            .accept_codex_directory_trust_prompt("sm-test")
            .unwrap());
        assert!(fs::read_to_string(&log_path)
            .unwrap()
            .contains("send-keys -t sm-test Enter"));
    }

    #[test]
    fn incidental_trust_text_does_not_submit_input() {
        assert!(!is_codex_directory_trust_prompt(
            "The docs ask: Do you trust the contents of this directory?"
        ));
    }

    #[test]
    fn settle_delay_grows_for_large_multiline_input() {
        let runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        assert_eq!(
            runtime.compute_settle_delay("short"),
            Duration::from_millis(300)
        );
        assert!(runtime.compute_settle_delay(&"x".repeat(2048)) > Duration::from_millis(300));
        assert!(runtime.compute_settle_delay("one\ntwo\nthree") > Duration::from_millis(300));
    }

    #[test]
    fn managed_session_command_exports_canonical_and_legacy_session_ids() {
        let command = managed_session_command("claude", "session'42", Some("credential'42"));
        assert!(command.contains("export SESSION_MANAGER_ID='session'\\''42'"));
        assert!(command.contains("export CLAUDE_SESSION_MANAGER_ID='session'\\''42'"));
        assert!(command.contains("export SM_SESSION_CREDENTIAL='credential'\\''42'"));
        assert!(command.contains("unset CLAUDECODE"));
        assert!(command.contains("export ENABLE_TOOL_SEARCH=false"));
        assert!(command.ends_with("; claude"));
    }

    #[test]
    fn tmux_no_current_target_counts_as_session_gone() {
        let error = anyhow::anyhow!("tmux command failed: no current target");
        assert!(is_tmux_session_gone_error(&error));
    }

    #[test]
    fn set_status_bar_updates_tmux_status_left() {
        let (tmux_binary, log_path, _temp_dir) = fake_tmux_binary();
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        runtime.tmux_binary = tmux_binary.display().to_string();

        assert!(runtime.set_status_bar("sm-test", "deskbar-name").unwrap());

        let log = fs::read_to_string(log_path).unwrap();
        assert!(log.contains("has-session -t sm-test"));
        assert!(log.contains("set-option -t sm-test status-left [deskbar-name]"));
    }

    #[test]
    fn create_session_applies_native_scrollback_and_history_before_provider_window() {
        let (tmux_binary, log_path, temp_dir) = fake_tmux_binary_with_has_session(false);
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig {
            tmux_socket_name: Some("session-manager".to_owned()),
            tmux_native_scrollback: Some(true),
            tmux_history_limit: Some(100000),
            ..RustCoreConfig::default()
        });
        runtime.tmux_binary = tmux_binary.display().to_string();
        let working_dir = temp_dir.join("repo");
        fs::create_dir_all(&working_dir).unwrap();
        let spec = TmuxSessionSpec {
            session_id: "abc12345".to_owned(),
            session_credential: None,
            tmux_session: "sm-test".to_owned(),
            working_dir: working_dir.display().to_string(),
            log_file: temp_dir.join("session.log"),
            provider: "claude".to_owned(),
            initial_message: None,
            force_initial_prompt_stdin: false,
            model: None,
            reasoning_effort: None,
        };

        runtime.create_session(&spec).unwrap();

        let log = fs::read_to_string(log_path).unwrap();
        let lines = log.lines().collect::<Vec<_>>();
        let bootstrap = position_after(&lines, "-L session-manager new-session -d -s sm-test", 0);
        let focus_events = position_after(
            &lines,
            "-L session-manager set-option -g focus-events on",
            bootstrap + 1,
        );
        let terminal_overrides = position_after(
            &lines,
            "-L session-manager set-option -as terminal-overrides ,*:smcup@:rmcup@",
            focus_events + 1,
        );
        let history_limit = position_after(
            &lines,
            "-L session-manager set-option -t sm-test history-limit 100000",
            terminal_overrides + 1,
        );
        let provider_window = position_after(
            &lines,
            "-L session-manager new-window -d -t sm-test -n main",
            history_limit + 1,
        );
        let pipe_pane = position_after(
            &lines,
            "-L session-manager pipe-pane -t sm-test",
            provider_window + 1,
        );

        assert!(bootstrap < focus_events);
        assert!(focus_events < terminal_overrides);
        assert!(terminal_overrides < history_limit);
        assert!(history_limit < provider_window);
        assert!(provider_window < pipe_pane);
    }

    #[test]
    fn runtime_from_default_app_config_uses_tmux_defaults() {
        let runtime = TmuxRuntime::from_app_config(&AppConfig::default());

        assert!(runtime.tmux_native_scrollback);
        assert_eq!(runtime.tmux_history_limit, Some(100000));
    }

    #[test]
    fn create_session_initializes_socket_options_without_bootstrap() {
        let (tmux_binary, log_path, temp_dir) = fake_tmux_binary_with_has_session(false);
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig {
            tmux_socket_name: Some("session-manager".to_owned()),
            tmux_native_scrollback: Some(false),
            tmux_history_limit: None,
            ..RustCoreConfig::default()
        });
        runtime.tmux_binary = tmux_binary.display().to_string();
        let working_dir = temp_dir.join("repo");
        fs::create_dir_all(&working_dir).unwrap();
        let spec = TmuxSessionSpec {
            session_id: "abc12345".to_owned(),
            session_credential: None,
            tmux_session: "sm-test".to_owned(),
            working_dir: working_dir.display().to_string(),
            log_file: temp_dir.join("session.log"),
            provider: "claude".to_owned(),
            initial_message: None,
            force_initial_prompt_stdin: false,
            model: None,
            reasoning_effort: None,
        };

        runtime.create_session(&spec).unwrap();

        let log = fs::read_to_string(log_path).unwrap();
        let lines = log.lines().collect::<Vec<_>>();
        let direct_session =
            position_after(&lines, "-L session-manager new-session -d -s sm-test", 0);
        let focus_events = position_after(
            &lines,
            "-L session-manager set-option -g focus-events on",
            direct_session + 1,
        );

        assert!(direct_session < focus_events);
        assert!(!log.contains("__sm_bootstrap"));
        assert!(!log.contains("terminal-overrides ,*:smcup@:rmcup@"));
        assert!(!log.contains("history-limit"));
    }

    #[test]
    fn create_session_does_not_mutate_default_tmux_server_options() {
        let (tmux_binary, log_path, temp_dir) = fake_tmux_binary_with_has_session(false);
        let mut config = RustCoreConfig::default();
        config.tmux_native_scrollback = Some(true);
        let mut runtime = TmuxRuntime::from_config(&config);
        runtime.tmux_binary = tmux_binary.display().to_string();
        let working_dir = temp_dir.join("repo");
        fs::create_dir_all(&working_dir).unwrap();
        let spec = TmuxSessionSpec {
            session_id: "abc12345".to_owned(),
            session_credential: None,
            tmux_session: "sm-test".to_owned(),
            working_dir: working_dir.display().to_string(),
            log_file: temp_dir.join("session.log"),
            provider: "claude".to_owned(),
            initial_message: None,
            force_initial_prompt_stdin: false,
            model: None,
            reasoning_effort: None,
        };

        runtime.create_session(&spec).unwrap();

        let log = fs::read_to_string(log_path).unwrap();

        assert!(log.contains("new-session -d -s sm-test"));
        assert!(!log.contains("set-option -g focus-events on"));
        assert!(!log.contains("terminal-overrides ,*:smcup@:rmcup@"));
        assert!(!log.contains("-L session-manager"));
    }

    #[test]
    fn restore_claude_session_preserves_configured_args_before_resume() {
        let (tmux_binary, log_path, temp_dir) = fake_tmux_binary_with_has_session(false);
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        runtime.tmux_binary = tmux_binary.display().to_string();
        runtime.claude_command = "claude".to_owned();
        runtime.claude_args = vec![
            "--dangerously-skip-permissions".to_owned(),
            "--some flag".to_owned(),
        ];
        let working_dir = temp_dir.join("repo");
        fs::create_dir_all(&working_dir).unwrap();
        let spec = TmuxSessionSpec {
            session_id: "abc12345".to_owned(),
            session_credential: None,
            tmux_session: "sm-test".to_owned(),
            working_dir: working_dir.display().to_string(),
            log_file: temp_dir.join("session.log"),
            provider: "claude".to_owned(),
            initial_message: None,
            force_initial_prompt_stdin: false,
            model: None,
            reasoning_effort: Some("high".to_owned()),
        };

        runtime
            .restore_session(&spec, "claude", Some("resume'id"))
            .unwrap();

        let log = fs::read_to_string(log_path).unwrap();
        let command_line = log
            .lines()
            .find(|line| line.contains("claude") && line.contains("SESSION_MANAGER_ID"))
            .unwrap_or_else(|| panic!("missing claude restore launch in log: {log}"));
        let dangerous = command_line
            .find("'--dangerously-skip-permissions'")
            .expect(command_line);
        let custom_arg = command_line.find("'--some flag'").expect(command_line);
        let effort_flag = command_line.find("--effort 'high'").expect(command_line);
        let resume_flag = command_line.find("'--resume'").expect(command_line);
        let resume_id = command_line.find("'resume'\\''id'").expect(command_line);

        assert!(dangerous < custom_arg);
        assert!(custom_arg < resume_flag);
        assert!(resume_flag < resume_id);
        assert!(resume_id < effort_flag);
    }

    #[test]
    fn clear_session_interrupts_waits_and_prompts_before_success() {
        let (tmux_binary, log_path, _temp_dir) = fake_tmux_binary();
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig {
            send_keys_settle_ms: Some(0.0),
            send_keys_settle_max_ms: Some(0.0),
            ..RustCoreConfig::default()
        });
        runtime.tmux_binary = tmux_binary.display().to_string();

        assert!(runtime
            .clear_session("sm-test", "/clear", Some("fresh task"), true)
            .unwrap());

        let log = fs::read_to_string(log_path).unwrap();
        let lines = log.lines().collect::<Vec<_>>();
        let wake_enter = position_after(&lines, "send-keys -t sm-test Enter", 0);
        let escape = position_after(&lines, "send-keys -t sm-test Escape", wake_enter + 1);
        let clear_text = position_after(&lines, "send-keys -t sm-test -l -- /clear", escape + 1);
        let clear_enter = position_after(&lines, "send-keys -t sm-test Enter", clear_text + 1);
        let post_clear_wait = position_after(&lines, "capture-pane -p -t sm-test", clear_enter + 1);
        let prompt_text = position_after(
            &lines,
            "send-keys -t sm-test -l -- fresh task",
            post_clear_wait + 1,
        );
        let prompt_enter = position_after(&lines, "send-keys -t sm-test Enter", prompt_text + 1);

        assert!(wake_enter < escape);
        assert!(escape < clear_text);
        assert!(clear_text < clear_enter);
        assert!(clear_enter < post_clear_wait);
        assert!(post_clear_wait < prompt_text);
        assert!(prompt_text < prompt_enter);
    }

    #[test]
    fn conditional_claude_clear_revalidates_before_touching_the_pane() {
        let (tmux_binary, log_path, _temp_dir) = fake_tmux_binary();
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig {
            send_keys_settle_ms: Some(0.0),
            send_keys_settle_max_ms: Some(0.0),
            ..RustCoreConfig::default()
        });
        runtime.tmux_binary = tmux_binary.display().to_string();

        let checks = std::cell::Cell::new(0);
        let outcome = runtime
            .clear_claude_session_if(
                "sm-test",
                "handoff prompt",
                || {
                    checks.set(checks.get() + 1);
                    Ok(true)
                },
                |_send_clear| {
                    checks.set(checks.get() + 1);
                    Ok(false)
                },
                |_send_prompt| unreachable!("prompt submission must not be reached"),
            )
            .unwrap();

        assert_eq!(outcome, ConditionalClearOutcome::PreconditionFailed);
        assert_eq!(checks.get(), 2);
        let log = fs::read_to_string(log_path).unwrap();
        assert!(!log.contains("send-keys -t sm-test -l -- /clear"));
        assert!(!log.contains("handoff prompt"));
    }

    #[test]
    fn conditional_claude_clear_keeps_the_prompt_when_redraw_never_finishes() {
        let (tmux_binary, log_path, _temp_dir) = fake_tmux_binary_with_stuck_clear();
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig {
            send_keys_settle_ms: Some(0.0),
            send_keys_settle_max_ms: Some(0.0),
            ..RustCoreConfig::default()
        });
        runtime.tmux_binary = tmux_binary.display().to_string();

        let error = runtime
            .clear_claude_session_if(
                "sm-test",
                "handoff prompt",
                || Ok(true),
                |send_clear| {
                    send_clear()?;
                    Ok(true)
                },
                |send_prompt| {
                    send_prompt()?;
                    Ok(true)
                },
            )
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("timed out waiting for /clear to finish"));
        let log = fs::read_to_string(log_path).unwrap();
        assert!(log.contains("send-keys -t sm-test -l -- /clear"));
        assert!(!log.contains("handoff prompt"));
    }

    #[test]
    fn conditional_claude_clear_revalidates_before_submitting_the_prompt() {
        let (tmux_binary, log_path, _temp_dir) = fake_tmux_binary_with_fresh_clear();
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig {
            send_keys_settle_ms: Some(0.0),
            send_keys_settle_max_ms: Some(0.0),
            ..RustCoreConfig::default()
        });
        runtime.tmux_binary = tmux_binary.display().to_string();

        let outcome = runtime
            .clear_claude_session_if(
                "sm-test",
                "handoff prompt",
                || Ok(true),
                |send_clear| {
                    send_clear()?;
                    Ok(true)
                },
                |_send_prompt| Ok(false),
            )
            .unwrap();

        assert_eq!(
            outcome,
            ConditionalClearOutcome::PostClearPreconditionFailed
        );
        let log = fs::read_to_string(log_path).unwrap();
        assert!(log.contains("send-keys -t sm-test -l -- /clear"));
        assert!(!log.contains("handoff prompt"));
    }

    #[test]
    fn fresh_prompt_wait_rejects_the_unchanged_pre_clear_prompt() {
        let (tmux_binary, _log_path, _temp_dir) = fake_tmux_binary();
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig::default());
        runtime.tmux_binary = tmux_binary.display().to_string();

        assert!(!runtime.wait_for_fresh_prompt("sm-test", "ready\n>\n", Duration::ZERO,));
    }

    #[test]
    fn codex_composer_readiness_requires_the_cursor_row() {
        let pane = "› stale transcript prompt\nWorking\n\n› live composer\n\nmodel status\n";

        assert!(!codex_composer_after_reset(pane, 1, None));
        assert!(codex_composer_after_reset(pane, 3, None));
        assert!(!codex_composer_after_reset(pane, 5, None));
        assert!(!codex_composer_after_reset(pane, 3, Some(pane)));
    }

    #[test]
    fn codex_prompt_confirmation_requires_exact_user_turn_after_offset() {
        let (_tmux_binary, _log_path, temp_dir) = fake_tmux_binary();
        let events = temp_dir.join("events.jsonl");
        let old = r#"{"event_type":"op_submitted","payload":{"UserTurn":{"items":[{"type":"text","text":"handoff prompt"}]}}}"#;
        fs::write(&events, format!("{old}\n")).unwrap();
        let offset = fs::metadata(&events).unwrap().len();
        fs::write(
            &events,
            format!(
                "{old}\n{}\n{}\n",
                r#"{"event_type":"op_submitted","payload":{"ListSkills":{}}}"#,
                r#"{"event_type":"op_submitted","payload":{"UserTurn":{"items":[{"type":"text","text":"handoff prompt"}]}}}"#
            ),
        )
        .unwrap();

        assert!(codex_event_stream_has_user_turn(
            &events,
            offset,
            "handoff prompt"
        ));
        assert!(!codex_event_stream_has_user_turn(
            &events,
            offset,
            "different prompt"
        ));
        assert!(codex_event_stream_has_user_turn(
            &events,
            offset - 3,
            "handoff prompt"
        ));
    }

    #[test]
    fn codex_prompt_confirmation_allows_only_one_terminal_newline_omission() {
        assert!(codex_brief_text_matches(
            "handoff prompt",
            "handoff prompt\n"
        ));
        assert!(!codex_brief_text_matches(
            "handoff prompt",
            "handoff prompt\n\n"
        ));
        assert!(!codex_brief_text_matches(
            "handoff prompt",
            "handoff prompt \n"
        ));
        let fragmented_legacy_turn = serde_json::json!({
            "items": [
                {"type": "text", "text": "handoff prompt"},
                {"type": "text", "text": " unexpected suffix"}
            ]
        });
        assert!(!codex_user_turn_text(&fragmented_legacy_turn)
            .is_some_and(|text| codex_brief_text_matches(&text, "handoff prompt\n")));

        let (_tmux_binary, _log_path, temp_dir) = fake_tmux_binary();
        let legacy_events = temp_dir.join("legacy-events.jsonl");
        let legacy_prefix = r#"{"event_type":"op_submitted","payload":{"ListSkills":{}}}"#;
        let legacy_acknowledgement = serde_json::json!({
            "event_type": "op_submitted",
            "payload": {"UserTurn": {"items": [{"type": "text", "text": "handoff prompt"}]}}
        });
        fs::write(
            &legacy_events,
            format!(
                "{legacy_prefix}\n{}\n",
                serde_json::to_string(&legacy_acknowledgement).unwrap()
            ),
        )
        .unwrap();
        assert!(codex_event_stream_has_user_turn(
            &legacy_events,
            legacy_prefix.len() as u64 + 1,
            "handoff prompt\n"
        ));

        let schema_v2_events = temp_dir.join("schema-v2-events.jsonl");
        let schema_v2_prefix = r#"{"schema_version":2,"event_type":"thread/started","payload":{}}"#;
        let schema_v2_acknowledgement = serde_json::json!({
            "schema_version": 2,
            "event_type": "item/started",
            "payload": {"item": {"type": "userMessage", "content": [{"type": "text", "text": "handoff prompt"}]}}
        });
        fs::write(
            &schema_v2_events,
            format!(
                "{schema_v2_prefix}\n{}\n",
                serde_json::to_string(&schema_v2_acknowledgement).unwrap()
            ),
        )
        .unwrap();
        assert!(codex_event_stream_has_user_turn(
            &schema_v2_events,
            schema_v2_prefix.len() as u64 + 1,
            "handoff prompt\n"
        ));
    }

    #[test]
    fn codex_prompt_confirmation_accepts_exact_schema_v2_large_user_message_after_offset() {
        let (_tmux_binary, _log_path, temp_dir) = fake_tmux_binary();
        let events = temp_dir.join("events.jsonl");
        let prompt = format!(
            "# Large immutable brief\n\n{}\n\nfinal acceptance sentinel\n",
            "multiline evidence paragraph\n".repeat(512)
        );
        let pre_offset = serde_json::json!({
            "event_type": "item/started",
            "payload": {"item": {"type": "userMessage", "content": [{"type": "text", "text": prompt}]}}
        });
        let pre_offset_line = serde_json::to_string(&pre_offset).unwrap();
        fs::write(&events, format!("{pre_offset_line}\n")).unwrap();
        let offset = fs::metadata(&events).unwrap().len();

        let agent_echo = serde_json::json!({
            "event_type": "item/started",
            "payload": {"item": {"type": "agentMessage", "content": [{"type": "text", "text": prompt}]}}
        });
        let partial_user_message = serde_json::json!({
            "event_type": "item/started",
            "payload": {"item": {"type": "userMessage", "content": [{"type": "text", "text": &prompt[..32]}]}}
        });
        let unversioned_full_user_message = serde_json::json!({
            "event_type": "item/started",
            "payload": {"item": {"type": "userMessage", "content": [{"type": "text", "text": prompt.strip_suffix('\n').unwrap()}]}}
        });
        let agent_echo_line = serde_json::to_string(&agent_echo).unwrap();
        let partial_user_message_line = serde_json::to_string(&partial_user_message).unwrap();
        let unversioned_full_user_message_line =
            serde_json::to_string(&unversioned_full_user_message).unwrap();
        fs::write(
            &events,
            format!(
                "{pre_offset_line}\n{agent_echo_line}\n{partial_user_message_line}\n{unversioned_full_user_message_line}\n",
            ),
        )
        .unwrap();
        assert!(!codex_event_stream_has_user_turn(&events, offset, &prompt));

        let exact_user_message = serde_json::json!({
            "schema_version": 2,
            "event_type": "item/started",
            "payload": {"item": {"type": "userMessage", "content": [{"type": "text", "text": prompt.strip_suffix('\n').unwrap()}]}}
        });
        let exact_user_message_line = serde_json::to_string(&exact_user_message).unwrap();
        fs::write(
            &events,
            format!(
                "{pre_offset_line}\n{agent_echo_line}\n{partial_user_message_line}\n{unversioned_full_user_message_line}\n{exact_user_message_line}\n"
            ),
        )
        .unwrap();

        assert!(codex_event_stream_has_user_turn(&events, offset, &prompt));
    }

    #[test]
    fn urgent_input_backgrounds_interrupts_and_sends_payload() {
        let (tmux_binary, log_path, _temp_dir) = fake_tmux_binary();
        let mut runtime = TmuxRuntime::from_config(&RustCoreConfig {
            send_keys_settle_ms: Some(0.0),
            send_keys_settle_max_ms: Some(0.0),
            ..RustCoreConfig::default()
        });
        runtime.tmux_binary = tmux_binary.display().to_string();

        assert!(runtime
            .send_urgent_input("sm-test", "urgent task", true)
            .unwrap());

        let log = fs::read_to_string(log_path).unwrap();
        let lines = log.lines().collect::<Vec<_>>();
        let background = position_after(&lines, "send-keys -t sm-test C-b", 0);
        let background_wait = position_after(&lines, "capture-pane -p -t sm-test", background + 1);
        let escape = position_after(&lines, "send-keys -t sm-test Escape", background_wait + 1);
        let interrupt_wait = position_after(&lines, "capture-pane -p -t sm-test", escape + 1);
        let payload = position_after(
            &lines,
            "send-keys -t sm-test -l -- urgent task",
            interrupt_wait + 1,
        );
        let enter = position_after(&lines, "send-keys -t sm-test Enter", payload + 1);

        assert!(background < background_wait);
        assert!(background_wait < escape);
        assert!(escape < interrupt_wait);
        assert!(interrupt_wait < payload);
        assert!(payload < enter);
    }

    fn fake_tmux_binary() -> (PathBuf, PathBuf, PathBuf) {
        fake_tmux_binary_with_has_session(true)
    }

    fn fake_codex_model_command(script_body: &str) -> (PathBuf, PathBuf) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!(
            "sm-runtime-fake-codex-models-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&temp_dir).unwrap();
        let command = temp_dir.join("codex");
        fs::write(&command, format!("#!/bin/sh\n{script_body}\n")).unwrap();
        let mut permissions = fs::metadata(&command).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&command, permissions).unwrap();
        (command, temp_dir)
    }

    fn fake_tmux_binary_with_has_session(has_session: bool) -> (PathBuf, PathBuf, PathBuf) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!(
            "sm-runtime-fake-tmux-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&temp_dir).unwrap();
        let tmux_binary = temp_dir.join("tmux");
        let log_path = temp_dir.join("tmux.log");
        fs::write(
            &tmux_binary,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> "{}"
if [ "$1" = "-L" ]; then
  shift 2
fi
case "$1" in
  has-session) exit {} ;;
  display-message) echo 0; exit 0 ;;
  capture-pane) printf 'ready\n>\n'; exit 0 ;;
  show-options) exit 0 ;;
  *) exit 0 ;;
esac
"#,
                log_path.display(),
                if has_session { 0 } else { 1 }
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux_binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux_binary, permissions).unwrap();
        (tmux_binary, log_path, temp_dir)
    }

    fn fake_tmux_binary_with_stuck_clear() -> (PathBuf, PathBuf, PathBuf) {
        let (tmux_binary, log_path, temp_dir) = fake_tmux_binary();
        fs::write(
            &tmux_binary,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> "{}"
case "$1" in
  has-session) exit 0 ;;
  display-message) echo 0; exit 0 ;;
  capture-pane)
    if grep -q 'send-keys -t sm-test -l -- /clear' "{}"; then
      printf 'clearing\n'
    else
      printf 'ready\n>\n'
    fi
    exit 0
    ;;
  show-options) exit 0 ;;
  *) exit 0 ;;
esac
"#,
                log_path.display(),
                log_path.display(),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux_binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux_binary, permissions).unwrap();
        (tmux_binary, log_path, temp_dir)
    }

    fn fake_tmux_binary_with_fresh_clear() -> (PathBuf, PathBuf, PathBuf) {
        let (tmux_binary, log_path, temp_dir) = fake_tmux_binary();
        fs::write(
            &tmux_binary,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> "{}"
case "$1" in
  has-session) exit 0 ;;
  display-message) echo 0; exit 0 ;;
  capture-pane)
    if grep -q 'send-keys -t sm-test -l -- /clear' "{}"; then
      printf 'cleared\n>\n'
    else
      printf 'ready\n>\n'
    fi
    exit 0
    ;;
  show-options) exit 0 ;;
  *) exit 0 ;;
esac
"#,
                log_path.display(),
                log_path.display(),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux_binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux_binary, permissions).unwrap();
        (tmux_binary, log_path, temp_dir)
    }

    fn position_after(lines: &[&str], needle: &str, start: usize) -> usize {
        lines
            .iter()
            .enumerate()
            .skip(start)
            .find_map(|(index, line)| line.contains(needle).then_some(index))
            .unwrap_or_else(|| panic!("missing {needle:?} after line {start}; log: {lines:?}"))
    }
}
