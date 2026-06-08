use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use anyhow::{bail, Context, Result};

use crate::config::RustCoreConfig;

const DEFAULT_SEND_KEYS_SETTLE_MS: f64 = 300.0;
const DEFAULT_SEND_KEYS_SETTLE_MAX_MS: f64 = 900.0;
const DEFAULT_SEND_KEYS_SETTLE_PER_KI_MS: f64 = 60.0;
const DEFAULT_SEND_KEYS_SETTLE_PER_EXTRA_LINE_MS: f64 = 15.0;
const DEFAULT_SEND_KEYS_MAX_CHUNK_CHARS: usize = 4096;

#[derive(Debug, Clone)]
pub struct TmuxRuntime {
    socket_name: Option<String>,
    command: String,
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
    pub initial_message: Option<String>,
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
            command: config
                .runtime_command
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("claude")
                .to_owned(),
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

    pub fn socket_name(&self) -> Option<&str> {
        self.socket_name.as_deref()
    }

    pub fn for_socket_name(&self, socket_name: Option<&str>) -> Self {
        let mut runtime = self.clone();
        runtime.socket_name = socket_name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        runtime
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

        let mut command = self.command.clone();
        if prompt_mode == "argv" {
            if let Some(initial_message) = spec
                .initial_message
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                command = format!("{command} -- {}", shell_quote(initial_message));
            }
        }
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

    pub fn send_input(&self, tmux_session: &str, text: &str) -> Result<bool> {
        if !self.session_exists(tmux_session)? {
            return Ok(false);
        }
        self.exit_copy_mode_if_needed(tmux_session);
        for chunk in split_send_text_chunks(text, self.send_keys_max_chunk_chars) {
            self.run_tmux(["send-keys", "-t", tmux_session, "-l", "--", chunk])?;
        }
        thread::sleep(self.compute_settle_delay(text));
        self.run_tmux(["send-keys", "-t", tmux_session, "Enter"])?;
        Ok(true)
    }

    pub fn kill_session(&self, tmux_session: &str) -> Result<bool> {
        if !self.session_exists(tmux_session)? {
            return Ok(false);
        }
        self.run_tmux(["kill-session", "-t", tmux_session])?;
        Ok(true)
    }

    fn session_exists(&self, tmux_session: &str) -> Result<bool> {
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

    fn exit_copy_mode_if_needed(&self, tmux_session: &str) {
        if self.pane_in_mode(tmux_session) == Some(1) {
            let _ = self.run_tmux(["send-keys", "-t", tmux_session, "-X", "cancel"]);
        }
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
        let mut command = Command::new("tmux");
        if let Some(socket_name) = &self.socket_name {
            command.arg("-L").arg(socket_name);
        }
        command.args(args);
        command
    }

    fn attach_session_log(&self, spec: &TmuxSessionSpec, prompt_mode: &str) -> Result<()> {
        let pipe_command = format!("cat >> {}", shell_quote_path(&spec.log_file));
        self.run_tmux([
            "pipe-pane",
            "-t",
            spec.tmux_session.as_str(),
            pipe_command.as_str(),
        ])?;

        if prompt_mode == "stdin" {
            if let Some(initial_message) = spec
                .initial_message
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                thread::sleep(Duration::from_millis(self.start_settle_ms));
                if !self.send_input(&spec.tmux_session, initial_message)? {
                    bail!("tmux session exited before initial prompt could be delivered");
                }
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

fn shell_quote_path(path: &Path) -> String {
    shell_quote(&path.display().to_string())
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
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
}
