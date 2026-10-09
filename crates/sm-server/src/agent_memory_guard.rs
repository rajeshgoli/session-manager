//! Kills processes an agent runs straight from its shell once they pass a
//! memory limit, so heavy work goes through `sm queue run` (sm#2137).
//!
//! The guard looks only at the process tree under each live sm session's tmux
//! pane, found by parent PID so processes that `setsid` away are still seen.
//! The pane's own process is the agent harness and is never touched, nor is
//! any process outside those trees. Queue jobs keep their own guards.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use rusqlite::{params, Connection};
use serde::Serialize;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::queue::{
    memory_amount_text, open_queue_jobs_connection, queue_shutdown, QueueMessageMetadata,
    RetainedQueueStore,
};
use crate::sessions::SessionStore;

/// How often the guard samples agent process trees. The 2026-10-08 panic
/// climbed from nothing to 260 GB in about six minutes.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
/// How long a stopped process gets to exit on SIGTERM before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// A killed PID is not reported again while it is still exiting.
const RECENT_KILL_WINDOW: Duration = Duration::from_secs(30);
/// Executables that are an agent harness wherever they sit in the tree.
const HARNESS_COMMANDS: [&str; 4] = ["claude", "codex", "codex-fork", "opencode"];
/// Kills `sm queue list` shows.
const LISTED_KILL_WINDOW: time::Duration = time::Duration::hours(24);
const MAX_COMMAND_CHARS: usize = 400;

#[derive(Debug, Clone)]
pub struct AgentMemoryGuardConfig {
    pub session_state_file: PathBuf,
    pub queue_state_dir: PathBuf,
    pub message_queue_db_path: PathBuf,
    /// Memory one agent-run process may use; 0 disables the guard.
    pub limit_bytes: i64,
    /// Executable names (basename) the guard never kills, besides harnesses.
    pub exempt_commands: Vec<String>,
}

/// One row of a process listing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessRow {
    pid: i32,
    ppid: i32,
    rss_kib: i64,
    command: String,
}

/// The tmux pane of one live sm session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentPane {
    session_id: String,
    session_name: String,
    pane_pid: i32,
}

/// A process over the limit, with every descendant to stop alongside it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Victim {
    session_id: String,
    session_name: String,
    pid: i32,
    executable: String,
    memory_bytes: i64,
    /// The victim first, then its descendants.
    pids: Vec<i32>,
}

/// A recorded kill, as `sm queue list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentProcessKill {
    pub killed_at: String,
    pub session_id: String,
    pub session_name: String,
    pub pid: i64,
    pub command: String,
    pub memory_bytes: i64,
    pub limit_bytes: i64,
    pub descendants: i64,
}

pub fn spawn_agent_memory_guard(config: AgentMemoryGuardConfig) {
    if config.limit_bytes <= 0 {
        eprintln!("agent memory guard disabled: queue_runner.memory.agent_process_max_bytes is 0");
        return;
    }
    let shutdown = queue_shutdown();
    thread::spawn(move || {
        let mut recent: HashMap<i32, Instant> = HashMap::new();
        loop {
            thread::sleep(SAMPLE_INTERVAL);
            if shutdown.is_stopped() {
                break;
            }
            recent.retain(|_, killed| killed.elapsed() < RECENT_KILL_WINDOW);
            if let Err(error) = guard_pass(&config, &mut recent) {
                eprintln!("agent memory guard check failed: {error:#}");
            }
        }
    });
}

fn guard_pass(config: &AgentMemoryGuardConfig, recent: &mut HashMap<i32, Instant>) -> Result<()> {
    let panes = live_agent_panes(&config.session_state_file)?;
    if panes.is_empty() {
        return Ok(());
    }
    let Some(listing) = process_listing() else {
        return Ok(());
    };
    let rows = parse_process_listing(&listing);
    let victims = agent_tree_victims(
        &rows,
        &panes,
        config.limit_bytes,
        &config.exempt_commands,
        process_footprint,
    );
    for victim in victims {
        if recent.contains_key(&victim.pid) {
            continue;
        }
        // Read before the signal: a dead process has no arguments to read.
        let command = process_command_line(victim.pid).unwrap_or_else(|| victim.executable.clone());
        stop_processes(&victim.pids);
        recent.insert(victim.pid, Instant::now());
        let kill = AgentProcessKill {
            killed_at: now_rfc3339(),
            session_id: victim.session_id.clone(),
            session_name: victim.session_name.clone(),
            pid: i64::from(victim.pid),
            command,
            memory_bytes: victim.memory_bytes,
            limit_bytes: config.limit_bytes,
            descendants: i64::try_from(victim.pids.len().saturating_sub(1)).unwrap_or(i64::MAX),
        };
        eprintln!(
            "agent memory guard killed pid {} of session {} ({}): {} over limit {}",
            kill.pid, kill.session_id, kill.command, kill.memory_bytes, kill.limit_bytes
        );
        if let Err(error) = record_kill(&config.queue_state_dir, &kill) {
            eprintln!(
                "agent memory guard could not record kill of {}: {error:#}",
                kill.pid
            );
        }
        if let Err(error) = RetainedQueueStore::new(config.message_queue_db_path.clone())
            .enqueue_message_with_metadata(
                &kill.session_id,
                &kill_notice_text(&kill),
                "sequential",
                QueueMessageMetadata {
                    // Retried until the agent is idle, like a queue completion.
                    message_category: Some("queue-completion".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
        {
            eprintln!(
                "agent memory guard could not message {} about pid {}: {error:#}",
                kill.session_id, kill.pid
            );
        }
    }
    Ok(())
}

/// The over-limit processes in each pane's tree. The pane's process and any
/// harness or exempt executable are never victims, though their children
/// are checked. A victim's descendants go with it and are not checked again.
fn agent_tree_victims(
    rows: &[ProcessRow],
    panes: &[AgentPane],
    limit_bytes: i64,
    exempt_commands: &[String],
    footprint: impl Fn(i32) -> Option<i64>,
) -> Vec<Victim> {
    let mut children: HashMap<i32, Vec<&ProcessRow>> = HashMap::new();
    for row in rows {
        if row.pid != row.ppid {
            children.entry(row.ppid).or_default().push(row);
        }
    }
    let exempt = |row: &ProcessRow| {
        let name = executable_name(&row.command);
        HARNESS_COMMANDS.contains(&name) || exempt_commands.iter().any(|c| c == name)
    };
    let mut victims = Vec::new();
    let mut seen = HashSet::new();
    for pane in panes {
        let mut stack: Vec<&ProcessRow> = children
            .get(&pane.pane_pid)
            .map(|kids| kids.to_vec())
            .unwrap_or_default();
        seen.insert(pane.pane_pid);
        while let Some(row) = stack.pop() {
            if !seen.insert(row.pid) {
                continue;
            }
            let memory = footprint(row.pid).unwrap_or_else(|| row.rss_kib.saturating_mul(1024));
            if !exempt(row) && memory > limit_bytes {
                let pids = subtree(&children, row.pid);
                seen.extend(pids.iter().copied());
                victims.push(Victim {
                    session_id: pane.session_id.clone(),
                    session_name: pane.session_name.clone(),
                    pid: row.pid,
                    executable: row.command.clone(),
                    memory_bytes: memory,
                    pids,
                });
                continue;
            }
            if let Some(kids) = children.get(&row.pid) {
                stack.extend(kids.iter().copied());
            }
        }
    }
    victims
}

fn subtree(children: &HashMap<i32, Vec<&ProcessRow>>, root: i32) -> Vec<i32> {
    let mut pids = vec![root];
    let mut next = 0;
    while next < pids.len() {
        if let Some(kids) = children.get(&pids[next]) {
            for kid in kids {
                if !pids.contains(&kid.pid) {
                    pids.push(kid.pid);
                }
            }
        }
        next += 1;
    }
    pids
}

fn executable_name(command: &str) -> &str {
    command.rsplit('/').next().unwrap_or(command)
}

/// SIGTERM the whole tree at once, then SIGKILL whatever is left after
/// [`KILL_GRACE`]. Signalled by PID, never by process group, so nothing
/// outside the tree is touched.
fn stop_processes(pids: &[i32]) {
    for pid in pids {
        let _ = kill(Pid::from_raw(*pid), Signal::SIGTERM);
    }
    let pids = pids.to_vec();
    thread::spawn(move || {
        thread::sleep(KILL_GRACE);
        for pid in pids {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    });
}

/// Live sm sessions and their tmux pane process, one `list-panes` per socket.
fn live_agent_panes(session_state_file: &Path) -> Result<Vec<AgentPane>> {
    let sessions = SessionStore::new(session_state_file.to_path_buf()).list_sessions(false)?;
    let mut by_socket: HashMap<Option<String>, Vec<_>> = HashMap::new();
    for session in sessions {
        if session.tmux_session.trim().is_empty() {
            continue;
        }
        by_socket
            .entry(session.tmux_socket_name.clone())
            .or_default()
            .push(session);
    }
    let mut panes = Vec::new();
    for (socket, sessions) in by_socket {
        let mut command = Command::new("tmux");
        if let Some(socket) = &socket {
            command.arg("-L").arg(socket);
        }
        let Ok(output) = command
            .args(["list-panes", "-a", "-F", "#{session_name} #{pane_pid}"])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let pane_pids = parse_pane_listing(&String::from_utf8_lossy(&output.stdout));
        for session in sessions {
            if let Some(pid) = pane_pids.get(&session.tmux_session) {
                panes.push(AgentPane {
                    session_id: session.id.clone(),
                    session_name: session.name.clone(),
                    pane_pid: *pid,
                });
            }
        }
    }
    Ok(panes)
}

fn parse_pane_listing(text: &str) -> HashMap<String, i32> {
    text.lines()
        .filter_map(|line| {
            let (name, pid) = line.trim().rsplit_once(' ')?;
            Some((name.to_owned(), pid.parse().ok()?))
        })
        .collect()
}

fn process_listing() -> Option<String> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss=,comm="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_process_listing(listing: &str) -> Vec<ProcessRow> {
    listing
        .lines()
        .filter_map(|line| {
            // Columns are space-padded; the command may itself hold spaces.
            let field = |text: &str| -> Option<(String, String)> {
                let (value, rest) = text.trim_start().split_once(char::is_whitespace)?;
                Some((value.to_owned(), rest.to_owned()))
            };
            let (pid, rest) = field(line)?;
            let (ppid, rest) = field(&rest)?;
            let (rss, command) = field(&rest)?;
            Some(ProcessRow {
                pid: pid.parse().ok()?,
                ppid: ppid.parse().ok()?,
                rss_kib: rss.parse().ok()?,
                command: command.trim().to_owned(),
            })
        })
        .collect()
}

fn process_footprint(pid: i32) -> Option<i64> {
    #[cfg(target_os = "macos")]
    {
        crate::utilization::mac::phys_footprint(pid)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        None
    }
}

fn process_command_line(pid: i32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !text.is_empty()).then(|| truncate_chars(&text, MAX_COMMAND_CHARS))
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

fn kill_notice_text(kill: &AgentProcessKill) -> String {
    let children = match kill.descendants {
        0 => String::new(),
        1 => " and its 1 child process".to_owned(),
        n => format!(" and its {n} child processes"),
    };
    format!(
        "[sm memory guard] sm killed PID {pid}{children}: `{command}` reached {memory}, over the {limit} limit for processes an agent runs from its own shell. \
Do not rerun it from your shell; it will be killed again. Resubmit it through the queue: \
`sm queue run --type <tests|perf|background> --label <label> --memory <size> -- <command>`. \
Its peak was at least {memory} and it may have been still climbing, so set --memory above what you expect it to need.",
        pid = kill.pid,
        command = kill.command,
        memory = memory_amount_text(kill.memory_bytes),
        limit = memory_amount_text(kill.limit_bytes),
    )
}

fn init_kills_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS agent_process_kills (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            killed_at TEXT NOT NULL,
            session_id TEXT NOT NULL,
            session_name TEXT NOT NULL,
            pid INTEGER NOT NULL,
            command TEXT NOT NULL,
            memory_bytes INTEGER NOT NULL,
            limit_bytes INTEGER NOT NULL,
            descendants INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS agent_process_kills_killed_at
            ON agent_process_kills(killed_at);
        "#,
    )?;
    Ok(())
}

fn record_kill(queue_state_dir: &Path, kill: &AgentProcessKill) -> Result<()> {
    let conn = open_queue_jobs_connection(&queue_state_dir.join("queue_runner.db"))?;
    insert_kill(&conn, kill)
}

fn insert_kill(conn: &Connection, kill: &AgentProcessKill) -> Result<()> {
    init_kills_schema(conn)?;
    conn.execute(
        r#"
        INSERT INTO agent_process_kills
            (killed_at, session_id, session_name, pid, command, memory_bytes, limit_bytes, descendants)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        "#,
        params![
            kill.killed_at,
            kill.session_id,
            kill.session_name,
            kill.pid,
            kill.command,
            kill.memory_bytes,
            kill.limit_bytes,
            kill.descendants,
        ],
    )
    .context("failed to record agent process kill")?;
    Ok(())
}

/// Kills from the last 24 hours, newest first, for one session or all.
pub fn recent_agent_process_kills(
    queue_db_path: &Path,
    session_id: Option<&str>,
) -> Result<Vec<AgentProcessKill>> {
    if !queue_db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = open_queue_jobs_connection(queue_db_path)?;
    list_kills_since(
        &conn,
        session_id,
        &(OffsetDateTime::now_utc() - LISTED_KILL_WINDOW)
            .format(&Rfc3339)
            .unwrap_or_default(),
    )
}

fn list_kills_since(
    conn: &Connection,
    session_id: Option<&str>,
    since: &str,
) -> Result<Vec<AgentProcessKill>> {
    init_kills_schema(conn)?;
    let mut statement = conn.prepare(
        r#"
        SELECT killed_at, session_id, session_name, pid, command, memory_bytes, limit_bytes, descendants
        FROM agent_process_kills
        WHERE killed_at >= ?1 AND (?2 IS NULL OR session_id = ?2)
        ORDER BY killed_at DESC, id DESC
        LIMIT 50
        "#,
    )?;
    let rows = statement.query_map(params![since, session_id], |row| {
        Ok(AgentProcessKill {
            killed_at: row.get(0)?,
            session_id: row.get(1)?,
            session_name: row.get(2)?,
            pid: row.get(3)?,
            command: row.get(4)?,
            memory_bytes: row.get(5)?,
            limit_bytes: row.get(6)?,
            descendants: row.get(7)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read agent process kills")
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests;
