#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::config::{AppConfig, CodexReviewConfig, RustCoreConfig};

const DEFAULT_SEND_KEYS_SETTLE_MS: f64 = 300.0;
const DEFAULT_SEND_KEYS_SETTLE_MAX_MS: f64 = 900.0;
const DEFAULT_SEND_KEYS_SETTLE_PER_KI_MS: f64 = 60.0;
const DEFAULT_SEND_KEYS_SETTLE_PER_EXTRA_LINE_MS: f64 = 15.0;
const DEFAULT_SEND_KEYS_MAX_CHUNK_CHARS: usize = 4096;

#[derive(Debug, Clone)]
pub struct TmuxRuntime {
    socket_name: Option<String>,
    tmux_binary: String,
    claude_command: String,
    claude_args: Vec<String>,
    codex_command: String,
    codex_args: Vec<String>,
    codex_default_model: Option<String>,
    codex_fork_command: String,
    codex_fork_args: Vec<String>,
    codex_fork_default_model: Option<String>,
    codex_fork_event_schema_version: u32,
    prompt_mode: String,
    start_settle_ms: u64,
    send_keys_settle_ms: f64,
    send_keys_settle_max_ms: f64,
    send_keys_settle_per_ki_ms: f64,
    send_keys_settle_per_extra_line_ms: f64,
    send_keys_max_chunk_chars: usize,
}

#[derive(Debug, Clone)]
pub struct TmuxSessionSpec {
    pub session_id: String,
    pub tmux_session: String,
    pub working_dir: String,
    pub log_file: PathBuf,
    pub provider: String,
    pub initial_message: Option<String>,
    pub model: Option<String>,
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
        runtime
    }

    pub fn socket_name(&self) -> Option<&str> {
        self.socket_name.as_deref()
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

        let mut command = self.launch_command(spec, &prompt_mode)?;
        command = managed_session_command(&command, &spec.session_id);

        self.run_tmux([
            "new-session",
            "-d",
            "-s",
            spec.tmux_session.as_str(),
            "-c",
            spec.working_dir.as_str(),
            command.as_str(),
        ])?;

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
                    runtime.claude_command = format!(
                        "{} --resume {}",
                        runtime.claude_command,
                        shell_quote(resume_id)
                    );
                }
                "codex-fork" => {
                    runtime.codex_fork_args =
                        prepend_arg_pair("resume", resume_id, &runtime.codex_fork_args);
                }
                "codex" => {
                    runtime.claude_command = format!(
                        "{} resume {}",
                        runtime.claude_command,
                        shell_quote(resume_id)
                    );
                }
                _ => {}
            };
        }
        let mut spec = spec.clone();
        spec.initial_message = None;
        runtime.create_session(&spec)
    }

    pub fn send_input(&self, tmux_session: &str, text: &str) -> Result<bool> {
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
        if !self.session_exists(tmux_session)? {
            return Ok(false);
        }
        if wake_completed {
            self.send_key(tmux_session, "Enter")?;
            let _ = self.wait_for_prompt(tmux_session, Duration::from_secs_f64(3.0));
        }

        self.send_key(tmux_session, "Escape")?;
        let _ = self.wait_for_prompt(tmux_session, Duration::from_secs_f64(3.0));

        self.send_text_then_enter(tmux_session, clear_command)?;
        let _ = self.wait_for_prompt(tmux_session, Duration::from_secs_f64(5.0));

        if let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) {
            self.send_text_then_enter(tmux_session, prompt)?;
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

    fn capture_pane_last_line(&self, tmux_session: &str) -> Option<String> {
        let output = self
            .tmux_command(["capture-pane", "-p", "-t", tmux_session])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .split('\n')
            .last()
            .map(str::trim)
            .map(ToOwned::to_owned)
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
    message.contains("no server running") || message.contains("can't find session")
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

fn managed_session_command(command: &str, session_id: &str) -> String {
    let session_id = shell_quote(session_id);
    format!(
        "export SESSION_MANAGER_ID={session_id}; \
         export CLAUDE_SESSION_MANAGER_ID={session_id}; \
         unset CLAUDECODE; \
         export ENABLE_TOOL_SEARCH=false; \
         {command}"
    )
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
        let command = managed_session_command("claude", "session'42");
        assert!(command.contains("export SESSION_MANAGER_ID='session'\\''42'"));
        assert!(command.contains("export CLAUDE_SESSION_MANAGER_ID='session'\\''42'"));
        assert!(command.contains("unset CLAUDECODE"));
        assert!(command.contains("export ENABLE_TOOL_SEARCH=false"));
        assert!(command.ends_with("; claude"));
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
case "$1" in
  has-session) exit 0 ;;
  display-message) echo 0; exit 0 ;;
  capture-pane) printf 'ready\n>\n'; exit 0 ;;
  *) exit 0 ;;
esac
"#,
                log_path.display()
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
