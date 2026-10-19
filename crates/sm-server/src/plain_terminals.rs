//! Durable plain shells, separate from provider transcripts and agent lifecycle.
use std::{path::PathBuf, process::Command, time::Duration};

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{runtime::TmuxRuntime, usage_db::UsageDbPool};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Limits {
    #[serde(rename = "plain_terminal_enabled")]
    pub enabled: bool,
    pub max_plain_terminals: usize,
    pub idle_reap_minutes: i64,
    pub tunnel_linger_minutes: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            enabled: true,
            max_plain_terminals: 4,
            idle_reap_minutes: 180,
            tunnel_linger_minutes: 15,
        }
    }
}

impl Limits {
    pub fn clamped(mut self) -> Self {
        self.max_plain_terminals = self.max_plain_terminals.clamp(1, 16);
        self.idle_reap_minutes = self.idle_reap_minutes.clamp(15, 1440);
        self.tunnel_linger_minutes = self.tunnel_linger_minutes.clamp(1, 120);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Terminal {
    pub id: String,
    pub cwd: String,
    pub name: Option<String>,
    pub created_by_user: String,
    pub created_by_device: Option<String>,
    pub created_via: String,
    pub tmux_session: String,
    pub state: String,
    pub last_attach_at: Option<String>,
    pub last_input_at: Option<String>,
    pub busy_since: Option<String>,
    pub created_at: String,
    pub retired_at: Option<String>,
    pub retired_reason: Option<String>,
}

impl Terminal {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            cwd: row.get(1)?,
            name: row.get(13)?,
            created_by_user: row.get(2)?,
            created_by_device: row.get(3)?,
            created_via: row.get(4)?,
            tmux_session: row.get(5)?,
            state: row.get(6)?,
            last_attach_at: row.get(7)?,
            last_input_at: row.get(8)?,
            busy_since: row.get(9)?,
            created_at: row.get(10)?,
            retired_at: row.get(11)?,
            retired_reason: row.get(12)?,
        })
    }

    pub fn idle_expired(&self, now: OffsetDateTime, minutes: i64) -> bool {
        if self.busy_since.is_some() {
            return false;
        }
        [&self.last_attach_at, &self.last_input_at]
            .into_iter()
            .all(|stamp| {
                // Corrupt timestamps must never authorize destructive cleanup.
                OffsetDateTime::parse(stamp.as_deref().unwrap_or(&self.created_at), &Rfc3339)
                    .is_ok_and(|then| now - then >= time::Duration::minutes(minutes))
            })
    }
}

const COLUMNS: &str = "id,cwd,created_by_user,created_by_device,created_via,tmux_session,state,last_attach_at,last_input_at,busy_since,created_at,retired_at,retired_reason,name";

#[derive(Clone, Debug)]
pub struct Store {
    pool: UsageDbPool,
}
impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self {
            pool: UsageDbPool::new(path, Duration::from_secs(5)),
        }
    }
    pub fn ensure_schema(&self) -> Result<()> {
        self.pool.get()?.execute_batch(
            "CREATE TABLE IF NOT EXISTS plain_terminals (
            id TEXT PRIMARY KEY, cwd TEXT NOT NULL, created_by_user TEXT NOT NULL,
            created_by_device TEXT, created_via TEXT NOT NULL, tmux_session TEXT NOT NULL,
            state TEXT NOT NULL, last_attach_at TEXT, last_input_at TEXT, busy_since TEXT,
            created_at TEXT NOT NULL, retired_at TEXT, retired_reason TEXT
        ); CREATE INDEX IF NOT EXISTS plain_terminals_state ON plain_terminals(state);",
        )?;
        let conn = self.pool.get()?;
        let has_name = conn
            .prepare("PRAGMA table_info(plain_terminals)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|column| column == "name");
        if !has_name {
            conn.execute("ALTER TABLE plain_terminals ADD COLUMN name TEXT", [])?;
        }
        Ok(())
    }
    pub fn list(&self, retired: bool) -> Result<Vec<Terminal>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM plain_terminals WHERE ?1 OR state='live' ORDER BY created_at,id"
        ))?;
        let records = stmt
            .query_map([retired], Terminal::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
    }
    pub fn get(&self, id: &str) -> Result<Option<Terminal>> {
        Ok(self
            .pool
            .get()?
            .query_row(
                &format!("SELECT {COLUMNS} FROM plain_terminals WHERE id=?1"),
                [id],
                Terminal::from_row,
            )
            .optional()?)
    }
    /// Reserve before launching. The caller serializes reservation, launch, retire and settings.
    pub fn insert(&self, t: &Terminal) -> Result<()> {
        self.pool.get()?.execute("INSERT INTO plain_terminals (id,cwd,created_by_user,created_by_device,created_via,tmux_session,state,created_at,name) VALUES (?1,?2,?3,?4,?5,?6,'live',?7,?8)", params![t.id,t.cwd,t.created_by_user,t.created_by_device,t.created_via,t.tmux_session,t.created_at,t.name])?;
        Ok(())
    }
    pub fn retire(&self, id: &str, reason: &str) -> Result<()> {
        self.pool.get()?.execute("UPDATE plain_terminals SET state='retired',retired_at=?2,retired_reason=?3,busy_since=NULL WHERE id=?1 AND state='live'", params![id, timestamp(), reason])?;
        Ok(())
    }
    pub fn observe(&self, id: &str, busy: bool, attached: bool) -> Result<()> {
        self.pool.get()?.execute("UPDATE plain_terminals SET busy_since=CASE WHEN ?2 THEN COALESCE(busy_since,?4) ELSE NULL END,last_attach_at=CASE WHEN ?3 THEN ?4 ELSE last_attach_at END WHERE id=?1 AND state='live'", params![id,busy,attached,timestamp()])?;
        Ok(())
    }
    pub fn attached_at(&self, id: &str, at: &str) -> Result<()> {
        self.pool.get()?.execute("UPDATE plain_terminals SET last_attach_at=MAX(COALESCE(last_attach_at,?2),?2) WHERE id=?1 AND state='live'",params![id,at])?;
        Ok(())
    }
    pub fn input(&self, id: &str) -> Result<()> {
        self.input_at(id, &timestamp())
    }
    pub fn input_at(&self, id: &str, at: &str) -> Result<()> {
        self.pool.get()?.execute(
            "UPDATE plain_terminals SET last_input_at=MAX(COALESCE(last_input_at,?2),?2) WHERE id=?1 AND state='live'",
            params![id, at],
        )?;
        Ok(())
    }
}

pub fn timestamp() -> String {
    OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("valid nanoseconds")
        .format(&Rfc3339)
        .expect("valid timestamp")
}

#[derive(Debug)]
pub struct Observation {
    pub busy: bool,
    pub attached: bool,
    pub command: String,
    pub last_input_at: Option<String>,
    pub last_attach_at: Option<String>,
    pub last_activity_at: Option<OffsetDateTime>,
}

impl Observation {
    /// tmux retains session activity after a native client detaches. Output also
    /// advances it, deliberately delaying cleanup when idle cannot be proved.
    pub fn idle_expired(&self, now: OffsetDateTime, minutes: i64) -> bool {
        !self.busy
            && !self.attached
            && self
                .last_activity_at
                .is_some_and(|at| now - at >= time::Duration::minutes(minutes))
    }
}

pub fn exists(runtime: &TmuxRuntime, t: &Terminal) -> Result<bool> {
    let command = runtime.tmux_command(["has-session", "-t", &format!("={}", t.tmux_session)])?;
    let output = crate::runtime::command_output_with_timeout(command, Duration::from_secs(3))?;
    if output.status.success() {
        return Ok(true);
    }
    let error = String::from_utf8_lossy(&output.stderr);
    if [
        "can't find session",
        "no server running",
        "No such file or directory",
    ]
    .iter()
    .any(|s| error.contains(s))
    {
        return Ok(false);
    }
    bail!("tmux presence inconclusive: {error}")
}

pub fn retire_runtime(runtime: &TmuxRuntime, t: &Terminal) -> Result<()> {
    let command = runtime.tmux_command(["kill-session", "-t", &format!("={}", t.tmux_session)])?;
    let output = crate::runtime::command_output_with_timeout(command, Duration::from_secs(3))?;
    if !output.status.success() && exists(runtime, t)? {
        bail!(
            "shell retirement failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// A child implies a descendant. Even a stopped or background child protects the shell.
/// Failed or malformed probes return errors, never an idle observation.
pub fn observe(runtime: &TmuxRuntime, t: &Terminal) -> Result<Observation> {
    // -u: without a UTF-8 locale (launchd sets none) tmux prints each tab as `_`.
    let command = runtime.tmux_command([
        "-u",
        "list-panes",
        "-s",
        "-t",
        &format!("={}", t.tmux_session),
        "-F",
        "#{pane_pid}\t#{pane_dead}\t#{session_attached}\t#{pane_current_command}\t#{session_last_attached}\t#{session_activity}",
    ])?;
    let output = crate::runtime::command_output_with_timeout(command, Duration::from_secs(3))?;
    if !output.status.success() {
        bail!("tmux pane inspection failed");
    }
    let text = String::from_utf8(output.stdout)?;
    let panes = text.lines().collect::<Vec<_>>();
    if panes.is_empty() {
        bail!("no shell panes");
    }
    let mut command = Command::new("/bin/ps");
    command.args(["-axo", "pid=,ppid="]);
    let processes = crate::runtime::command_output_with_timeout(command, Duration::from_secs(3))?;
    if !processes.status.success() {
        bail!("process inspection failed");
    }
    let pids = parse_processes(&String::from_utf8(processes.stdout)?)?;
    let mut observation = Observation {
        busy: false,
        attached: false,
        command: "zsh".into(),
        last_input_at: None,
        last_attach_at: None,
        last_activity_at: None,
    };
    for pane in panes {
        let fields = pane.splitn(6, '\t').collect::<Vec<_>>();
        if fields.len() != 6 {
            bail!("invalid pane observation");
        }
        let pid = fields[0].parse::<i32>()?;
        if fields[1] != "0" {
            bail!("shell pane exited");
        }
        observation.attached |= fields[2].parse::<u32>()? > 0;
        // Verify the root was in this snapshot; absence can be a process exit race.
        if !pids.iter().any(|(p, _)| *p == pid) {
            bail!("shell process missing");
        }
        let attached = if fields[4].is_empty() {
            0
        } else {
            fields[4].parse::<i64>()?
        };
        if attached > 0 {
            observation.last_attach_at =
                Some(OffsetDateTime::from_unix_timestamp(attached)?.format(&Rfc3339)?);
        }
        let activity = OffsetDateTime::from_unix_timestamp(fields[5].parse()?)?;
        observation.last_activity_at = Some(
            observation
                .last_activity_at
                .map_or(activity, |previous| previous.max(activity)),
        );
        // exec can replace zsh at the same PID without creating descendants.
        if !matches!(fields[3], "zsh" | "-zsh") || pids.iter().any(|(_, parent)| *parent == pid) {
            observation.busy = true;
            observation.command = fields[3].into();
        }
    }
    if observation.attached {
        let command = runtime.tmux_command([
            "list-clients",
            "-t",
            &format!("={}", t.tmux_session),
            "-F",
            "#{client_activity}",
        ])?;
        let output = crate::runtime::command_output_with_timeout(command, Duration::from_secs(3))?;
        if !output.status.success() {
            bail!("client inspection failed");
        }
        let text = String::from_utf8(output.stdout)?;
        if let Some(at) = text
            .lines()
            .map(str::parse::<i64>)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .max()
        {
            observation.last_input_at =
                Some(OffsetDateTime::from_unix_timestamp(at)?.format(&Rfc3339)?);
        }
    }
    Ok(observation)
}
fn parse_processes(text: &str) -> Result<Vec<(i32, i32)>> {
    text.lines()
        .filter(|s| !s.trim().is_empty())
        .map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next().context("missing pid")?.parse()?;
            let parent = fields.next().context("missing parent pid")?.parse()?;
            Ok((pid, parent))
        })
        .collect()
}

#[cfg(test)]
mod tests;
