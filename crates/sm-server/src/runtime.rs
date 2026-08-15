#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::HashMap,
    env, fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Condvar, Mutex, OnceLock, Weak},
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
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CodexForkRuntimeArtifacts {
    pub event_stream_path: PathBuf,
    pub control_socket_path: PathBuf,
}

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

        let prompt_mode = self.prompt_mode.to_ascii_lowercase();
        if prompt_mode != "argv" && prompt_mode != "stdin" {
            bail!("unsupported runtime prompt mode: {}", self.prompt_mode);
        }
        let has_initial_stdin_prompt = prompt_mode == "stdin"
            && spec
                .initial_message
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty());

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
                .capture_pane_last_line(tmux_session)
                .is_some_and(|line| matches!(line.trim(), ">" | "❯")),
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
        let initial_stdin_prompt = (prompt_mode == "stdin")
            .then(|| {
                spec.initial_message
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            })
            .flatten();
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
        Ok(())
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
        event
            .get("payload")
            .and_then(|payload| payload.get("UserTurn"))
            .and_then(|turn| turn.get("items"))
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.get("text").and_then(Value::as_str) == Some(prompt))
            })
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
        return Ok(());
    }
    if find_in_path(command).is_none() {
        bail!("Launch command not found on PATH: {command}");
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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
