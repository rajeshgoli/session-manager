//! Watches the commands agents run from their own shells (sm#2137, sm#2412).
//!
//! Every live sm session's tmux panes are walked by parent PID. Inside each
//! pane, an agent harness (`claude`, `codex`, `codex-fork`, `opencode`) runs
//! each command as a child: that child with everything under it is one
//! *command*. Harnesses and the infrastructure above them are never touched.
//! A command the agent detached (`nohup … &`) is reparented to PID 1 but
//! keeps the process group of the shell that started it; the guard records
//! every command's group, on macOS also from kernel fork notices, so such a
//! command is still traced to its agent.
//!
//! When one of a command's processes passes the per-process limit, the
//! command is adopted into the queue as a running shell job with a memory
//! reservation sized from the host's headroom ([`crate::queue::shell`]). With
//! too little headroom, or no process group of its own to track it by, it is
//! stopped as before and the agent is told how to resubmit it. Queue jobs
//! keep their own guards.
//!
//! Each pass also publishes every session's commands that are not the
//! harness's own helpers, which idle agents show as background jobs.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock, RwLock},
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
    memory_amount_text, open_queue_jobs_connection, queue_shutdown, shell, QueueMessageMetadata,
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
/// Harness helpers that are never an agent's background job: Claude keeps the
/// Mac awake with `caffeinate`; Codex runs its REPL and code-mode hosts.
const HELPER_COMMANDS: [&str; 3] = ["caffeinate", "codex-code-mode-host", "node_repl"];
const HELPER_ARGUMENT_MARKERS: [&str; 1] = ["cua-repl"];
/// A command started this soon after its harness is part of the harness's
/// startup (MCP servers, REPL helpers), not something the agent ran.
const HELPER_STARTUP_SECONDS: i64 = 10;
/// Kills `sm queue list` shows.
const LISTED_KILL_WINDOW: time::Duration = time::Duration::hours(24);
const MAX_COMMAND_CHARS: usize = 400;
/// Arguments read for a command's label; Claude's wrapper alone is ~250.
const MAX_ARGUMENT_CHARS: usize = 8000;

#[derive(Debug, Clone)]
pub struct AgentMemoryGuardConfig {
    pub session_state_file: PathBuf,
    pub queue_state_dir: PathBuf,
    pub message_queue_db_path: PathBuf,
    /// Memory one agent-run process may use; 0 disables the guard.
    pub limit_bytes: i64,
    /// Executable names (basename) the guard never kills, besides harnesses.
    pub exempt_commands: Vec<String>,
    /// Executable names (basename) never shown as background jobs.
    pub background_ignore_commands: Vec<String>,
    /// The queue's configured free-memory reserve, before kernel pressure.
    pub memory_min_free_bytes: i64,
}

/// One row of a process listing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessRow {
    pid: i32,
    ppid: i32,
    pgid: i32,
    rss_kib: i64,
    command: String,
}

/// One tmux pane of a live sm session. A local agent has two: the window
/// that attaches its UI and the one serving it, where commands run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentPane {
    session_id: String,
    session_name: String,
    pane_pid: i32,
}

/// The session whose command leads a process group.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupOwner {
    session_id: String,
    session_name: String,
}

/// One command an agent started, with every process under it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandRoot {
    session_id: String,
    session_name: String,
    pid: i32,
    pgid: i32,
    /// The harness that started it; `None` for a detached command.
    harness_pid: Option<i32>,
    /// The root first, then its descendants (for a detached command, every
    /// member of its group and their descendants).
    pids: Vec<i32>,
    detached: bool,
}

/// A command an idle agent left running, as the session listing shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackgroundJob {
    pub pid: i64,
    pub command: String,
    pub started_at: Option<String>,
    pub memory_bytes: i64,
    /// Set when the command is also a shell job in the queue.
    pub shell_job_id: Option<String>,
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

/// Why a command over the limit was stopped rather than adopted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopCause {
    /// The Mac had less headroom than the smallest reservation.
    NoHeadroom { headroom: i64 },
    /// The command shares its harness's process group, so sm cannot track it.
    Untrackable,
}

/// Each session's background jobs; `None` until the guard's first pass.
type BackgroundSnapshot = RwLock<Option<HashMap<String, Vec<BackgroundJob>>>>;

fn background_snapshot() -> &'static BackgroundSnapshot {
    static SNAPSHOT: OnceLock<BackgroundSnapshot> = OnceLock::new();
    SNAPSHOT.get_or_init(|| RwLock::new(None))
}

/// The commands a session has running outside the queue, from the guard's
/// latest pass; `None` while the guard is not running, so callers can fall
/// back to other signals.
pub fn background_jobs(session_id: &str) -> Option<Vec<BackgroundJob>> {
    background_snapshot()
        .read()
        .ok()?
        .as_ref()
        .map(|all| all.get(session_id).cloned().unwrap_or_default())
}

fn publish_background_jobs(snapshot: HashMap<String, Vec<BackgroundJob>>) {
    if let Ok(mut current) = background_snapshot().write() {
        *current = Some(snapshot);
    }
}

pub fn spawn_agent_memory_guard(config: AgentMemoryGuardConfig) {
    if config.limit_bytes <= 0 {
        eprintln!("agent memory guard disabled: queue_runner.memory.agent_process_max_bytes is 0");
        return;
    }
    let shutdown = queue_shutdown();
    thread::spawn(move || {
        let mut guard = Guard::new();
        loop {
            thread::sleep(SAMPLE_INTERVAL);
            if shutdown.is_stopped() {
                break;
            }
            guard
                .recent
                .retain(|_, killed| killed.elapsed() < RECENT_KILL_WINDOW);
            if let Err(error) = guard.pass(&config) {
                eprintln!("agent memory guard check failed: {error:#}");
            }
        }
    });
}

/// What one process looked like when the guard first saw it.
#[derive(Debug, Clone)]
struct ProcessFacts {
    identity: Option<String>,
    started_at: Option<OffsetDateTime>,
    /// The `ps` display string, for helper markers.
    args: Option<String>,
    /// The command the agent ran, from the argument vector when readable.
    command: Option<String>,
}

struct Guard {
    recent: HashMap<i32, Instant>,
    groups: Arc<Mutex<HashMap<i32, GroupOwner>>>,
    facts: HashMap<i32, ProcessFacts>,
    #[cfg(target_os = "macos")]
    forks: fork_watch::ForkWatcher,
    /// Linux has no fork notice; a reparented process's own environment
    /// names its session instead. Read once per process.
    #[cfg(not(target_os = "macos"))]
    environ_sessions: HashMap<i32, Option<String>>,
}

impl Guard {
    fn new() -> Self {
        let groups = Arc::new(Mutex::new(HashMap::new()));
        Self {
            recent: HashMap::new(),
            #[cfg(target_os = "macos")]
            forks: fork_watch::ForkWatcher::spawn(groups.clone()),
            groups,
            facts: HashMap::new(),
            #[cfg(not(target_os = "macos"))]
            environ_sessions: HashMap::new(),
        }
    }

    /// Arguments and start time of `pid`, read once per process.
    fn facts(&mut self, pid: i32) -> ProcessFacts {
        let identity = process_identity_text(pid);
        if let Some(known) = self.facts.get(&pid) {
            if known.identity == identity {
                return known.clone();
            }
        }
        let args = process_arguments_text(pid, MAX_ARGUMENT_CHARS);
        let command = crate::queue::process_argv(i64::from(pid))
            .map(|argv| shell::command_from_argv(&argv))
            .or_else(|| args.as_deref().map(shell::command_text));
        let facts = ProcessFacts {
            started_at: process_started_at(pid),
            args,
            command,
            identity,
        };
        self.facts.insert(pid, facts.clone());
        facts
    }

    fn pass(&mut self, config: &AgentMemoryGuardConfig) -> Result<()> {
        let panes = live_agent_panes(&config.session_state_file)?;
        let Some(listing) = process_listing() else {
            return Ok(());
        };
        let rows = parse_process_listing(&listing);
        let live: HashSet<i32> = rows.iter().map(|row| row.pid).collect();
        self.facts.retain(|pid, _| live.contains(pid));
        #[cfg(not(target_os = "macos"))]
        self.attribute_by_environment(&rows, &panes, &live);
        let groups = {
            let mut groups = self.groups.lock().unwrap_or_else(|p| p.into_inner());
            let listed: HashSet<i32> = rows.iter().map(|row| row.pgid).collect();
            groups.retain(|pgid, _| listed.contains(pgid));
            groups.clone()
        };
        let (roots, harnesses) = command_roots(&rows, &panes, &groups);
        {
            let mut groups = self.groups.lock().unwrap_or_else(|p| p.into_inner());
            for root in &roots {
                if !root.detached && root.pgid == root.pid {
                    groups.insert(
                        root.pgid,
                        GroupOwner {
                            session_id: root.session_id.clone(),
                            session_name: root.session_name.clone(),
                        },
                    );
                }
            }
        }
        #[cfg(target_os = "macos")]
        self.forks.watch(&harnesses);
        #[cfg(not(target_os = "macos"))]
        let _ = harnesses;

        let by_pid: HashMap<i32, &ProcessRow> = rows.iter().map(|row| (row.pid, row)).collect();
        let mut memory: HashMap<i32, i64> = HashMap::new();
        let mut memory_of = |pid: i32| -> i64 {
            *memory.entry(pid).or_insert_with(|| {
                process_footprint(pid).unwrap_or_else(|| {
                    by_pid
                        .get(&pid)
                        .map_or(0, |row| row.rss_kib.saturating_mul(1024))
                })
            })
        };

        let conn = open_queue_jobs_connection(&config.queue_state_dir.join("queue_runner.db"))?;
        let running = shell::running_shell_jobs_conn(&conn)?;
        let mut adopted: HashMap<i32, String> = HashMap::new();
        for job in &running {
            let Ok(pgid) = i32::try_from(job.process_group_id) else {
                continue;
            };
            let members = group_tree(&rows, pgid);
            if members.is_empty() {
                if let Err(error) =
                    shell::finish_gone_conn(&conn, &job.id, &config.message_queue_db_path)
                {
                    eprintln!("shell job {} could not be finished: {error:#}", job.id);
                }
                continue;
            }
            let bytes = members
                .iter()
                .map(|pid| memory_of(*pid))
                .fold(0i64, i64::saturating_add);
            shell::record_usage(&job.id, bytes);
            adopted.insert(pgid, job.id.clone());
        }

        let mut reserved = reserved_memory(&conn, &running);
        for root in &roots {
            if adopted.contains_key(&root.pgid) {
                continue;
            }
            let trigger = root
                .pids
                .iter()
                .filter_map(|pid| by_pid.get(pid))
                .filter(|row| !is_harness(row) && !is_exempt(row, &config.exempt_commands))
                .map(|row| (row.pid, memory_of(row.pid)))
                .filter(|(_, bytes)| *bytes > config.limit_bytes)
                .max_by_key(|(_, bytes)| *bytes);
            let Some((trigger_pid, trigger_bytes)) = trigger else {
                continue;
            };
            if self.recent.contains_key(&root.pid) {
                continue;
            }
            let harness_pgid = root
                .harness_pid
                .and_then(|pid| by_pid.get(&pid))
                .map(|row| row.pgid);
            let trackable = root.pgid > 1
                && (root.detached || root.pgid == root.pid)
                && Some(root.pgid) != harness_pgid;
            let facts = self.facts(root.pid);
            let command = facts
                .command
                .clone()
                .unwrap_or_else(|| executable_name(&by_pid[&root.pid].command).to_owned());
            let cause = if trackable {
                let headroom = host_headroom(config.memory_min_free_bytes, &reserved);
                match headroom.and_then(shell::reservation_for_headroom) {
                    Some(reservation) => {
                        let bytes = root
                            .pids
                            .iter()
                            .map(|pid| memory_of(*pid))
                            .fold(0i64, i64::saturating_add);
                        match self.adopt(
                            &conn,
                            root,
                            &facts,
                            &command,
                            reservation,
                            (trigger_pid, trigger_bytes),
                            headroom.unwrap_or_default(),
                        ) {
                            Ok(job_id) => {
                                shell::record_usage(&job_id, bytes);
                                reserved.push((reservation, bytes));
                                adopted.insert(root.pgid, job_id);
                                continue;
                            }
                            Err(error) => {
                                eprintln!(
                                    "agent memory guard could not adopt pid {}: {error:#}",
                                    root.pid
                                );
                                StopCause::NoHeadroom {
                                    headroom: headroom.unwrap_or_default(),
                                }
                            }
                        }
                    }
                    None => StopCause::NoHeadroom {
                        headroom: headroom.unwrap_or_default(),
                    },
                }
            } else {
                StopCause::Untrackable
            };
            let command_bytes = root
                .pids
                .iter()
                .map(|pid| memory_of(*pid))
                .fold(0i64, i64::saturating_add)
                .max(trigger_bytes);
            self.stop(config, root, &command, command_bytes, cause);
        }

        let mut snapshot: HashMap<String, Vec<BackgroundJob>> = HashMap::new();
        for pane in &panes {
            snapshot.entry(pane.session_id.clone()).or_default();
        }
        for root in &roots {
            let row = by_pid[&root.pid];
            let facts = self.facts(root.pid);
            let harness_started = root.harness_pid.and_then(|pid| self.facts(pid).started_at);
            if is_helper(
                row,
                &facts,
                harness_started,
                &config.background_ignore_commands,
            ) {
                continue;
            }
            let bytes = root
                .pids
                .iter()
                .map(|pid| memory_of(*pid))
                .fold(0i64, i64::saturating_add);
            snapshot
                .entry(root.session_id.clone())
                .or_default()
                .push(BackgroundJob {
                    pid: i64::from(root.pid),
                    command: shell::label_text(
                        facts
                            .command
                            .as_deref()
                            .unwrap_or_else(|| executable_name(&row.command)),
                    ),
                    started_at: facts.started_at.and_then(|at| at.format(&Rfc3339).ok()),
                    memory_bytes: bytes,
                    shell_job_id: adopted.get(&root.pgid).cloned(),
                });
        }
        for jobs in snapshot.values_mut() {
            jobs.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.pid.cmp(&b.pid)));
        }
        publish_background_jobs(snapshot);
        Ok(())
    }

    /// Records the group of each reparented process whose environment names
    /// a live session, so it is traced like a fork notice would trace it.
    #[cfg(not(target_os = "macos"))]
    fn attribute_by_environment(
        &mut self,
        rows: &[ProcessRow],
        panes: &[AgentPane],
        live: &HashSet<i32>,
    ) {
        self.environ_sessions.retain(|pid, _| live.contains(pid));
        let owners: HashMap<&str, &AgentPane> = panes
            .iter()
            .map(|pane| (pane.session_id.as_str(), pane))
            .collect();
        let mut groups = self.groups.lock().unwrap_or_else(|p| p.into_inner());
        for row in rows.iter().filter(|row| row.ppid == 1) {
            if groups.contains_key(&row.pgid) {
                continue;
            }
            let session = self
                .environ_sessions
                .entry(row.pid)
                .or_insert_with(|| environ_session_id(row.pid))
                .clone();
            if let Some(pane) = session.as_deref().and_then(|id| owners.get(id)) {
                groups.insert(
                    row.pgid,
                    GroupOwner {
                        session_id: pane.session_id.clone(),
                        session_name: pane.session_name.clone(),
                    },
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // one adoption's measurements
    fn adopt(
        &mut self,
        conn: &Connection,
        root: &CommandRoot,
        facts: &ProcessFacts,
        command: &str,
        reservation: i64,
        (trigger_pid, trigger_bytes): (i32, i64),
        headroom: i64,
    ) -> Result<String> {
        let adoption = shell::Adoption {
            session_id: root.session_id.clone(),
            root_pid: i64::from(root.pid),
            process_group_id: i64::from(root.pgid),
            label: shell::label_text(command),
            command: command.to_owned(),
            cwd: process_cwd(root.pid).unwrap_or_default(),
            reservation_bytes: reservation,
            process_started_at: facts.started_at.and_then(|at| at.format(&Rfc3339).ok()),
            record: shell::AdoptionRecord {
                adopted_at: shell::adopted_now(),
                trigger_pid: i64::from(trigger_pid),
                trigger_bytes,
                headroom_bytes: headroom,
                root_identity: facts.identity.clone(),
                detached: root.detached,
            },
        };
        let id = shell::adopt_conn(conn, &adoption)?;
        eprintln!(
            "agent memory guard adopted pid {} of session {} as shell job {id} ({} reserved): {}",
            root.pid,
            root.session_id,
            memory_amount_text(reservation),
            adoption.label
        );
        Ok(id)
    }

    /// Stops a command sm cannot adopt, records it, and tells the agent.
    fn stop(
        &mut self,
        config: &AgentMemoryGuardConfig,
        root: &CommandRoot,
        command: &str,
        memory_bytes: i64,
        cause: StopCause,
    ) {
        stop_processes(&root.pids);
        self.recent.insert(root.pid, Instant::now());
        let kill = AgentProcessKill {
            killed_at: now_rfc3339(),
            session_id: root.session_id.clone(),
            session_name: root.session_name.clone(),
            pid: i64::from(root.pid),
            command: truncate_chars(command, MAX_COMMAND_CHARS),
            memory_bytes,
            limit_bytes: config.limit_bytes,
            descendants: i64::try_from(root.pids.len().saturating_sub(1)).unwrap_or(i64::MAX),
        };
        eprintln!(
            "agent memory guard killed pid {} of session {} ({}): {} over limit {} ({cause:?})",
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
                &kill_notice_text(&kill, command, cause),
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
}

/// (reservation, current use) of every running queue job with a
/// reservation: shell jobs from the guard's own measurements, others from
/// the utilization recorder's latest sample.
fn reserved_memory(conn: &Connection, running_shell: &[shell::RunningShellJob]) -> Vec<(i64, i64)> {
    let mut reserved: Vec<(i64, i64)> = running_shell
        .iter()
        .filter_map(|job| {
            let reservation = job.reservation_bytes?;
            Some((
                reservation,
                shell::usage(&job.id).map_or(0, |usage| usage.current_bytes),
            ))
        })
        .collect();
    let usage = crate::utilization::latest_job_usage()
        .map(|(_, usage)| usage)
        .unwrap_or_default();
    let others = conn
        .prepare(
            "SELECT id, memory_bytes FROM queue_jobs
             WHERE state = 'running' AND type != ?1 AND memory_bytes IS NOT NULL",
        )
        .and_then(|mut statement| {
            statement
                .query_map(params![shell::SHELL_JOB_TYPE], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap_or_default();
    for (id, reservation) in others {
        let current = usage
            .get(&id)
            .and_then(|usage| usage.memory_bytes)
            .unwrap_or(0);
        reserved.push((reservation, current));
    }
    reserved
}

/// Headroom for a new shell job, or `None` when host memory is unreadable.
fn host_headroom(memory_min_free_bytes: i64, reserved: &[(i64, i64)]) -> Option<i64> {
    let (_, available) = crate::queue::host_memory_capacity()?;
    let reserve = crate::queue::effective_memory_reserve_bytes(memory_min_free_bytes);
    Some(shell::headroom_bytes(available, reserve, reserved))
}

fn is_harness(row: &ProcessRow) -> bool {
    HARNESS_COMMANDS.contains(&executable_name(&row.command))
}

fn is_exempt(row: &ProcessRow, exempt_commands: &[String]) -> bool {
    let name = executable_name(&row.command);
    exempt_commands.iter().any(|c| c == name)
}

/// The harness's own helpers, which are not something the agent ran.
fn is_helper(
    row: &ProcessRow,
    facts: &ProcessFacts,
    harness_started: Option<OffsetDateTime>,
    ignore_commands: &[String],
) -> bool {
    let name = executable_name(&row.command);
    if HELPER_COMMANDS.contains(&name) || ignore_commands.iter().any(|c| c == name) {
        return true;
    }
    if facts.args.as_deref().is_some_and(|args| {
        HELPER_ARGUMENT_MARKERS
            .iter()
            .any(|marker| args.contains(marker))
    }) {
        return true;
    }
    matches!(
        (facts.started_at, harness_started),
        (Some(started), Some(harness)) if (started - harness).whole_seconds() <= HELPER_STARTUP_SECONDS
    )
}

/// Every command in the panes' trees, plus detached commands whose process
/// group is on the record; and every harness process, to watch for forks.
///
/// Within a pane, a non-harness child of a harness is a command root, taken
/// with its whole subtree. Processes above the harness (a local agent's
/// launcher, wall owner, judge and supervisor) are infrastructure and never
/// roots. A pane with no harness anywhere treats the pane process as one.
fn command_roots(
    rows: &[ProcessRow],
    panes: &[AgentPane],
    groups: &HashMap<i32, GroupOwner>,
) -> (Vec<CommandRoot>, Vec<(i32, GroupOwner)>) {
    let mut children: HashMap<i32, Vec<&ProcessRow>> = HashMap::new();
    for row in rows {
        if row.pid != row.ppid {
            children.entry(row.ppid).or_default().push(row);
        }
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|row| row.pid);
    }
    let by_pid: HashMap<i32, &ProcessRow> = rows.iter().map(|row| (row.pid, row)).collect();
    let mut roots = Vec::new();
    let mut harnesses = Vec::new();
    let mut claimed: HashSet<i32> = HashSet::new();
    for pane in panes {
        let Some(pane_row) = by_pid.get(&pane.pane_pid) else {
            continue;
        };
        let owner = GroupOwner {
            session_id: pane.session_id.clone(),
            session_name: pane.session_name.clone(),
        };
        let tree = subtree(&children, pane.pane_pid);
        let any_harness = tree
            .iter()
            .filter_map(|pid| by_pid.get(pid))
            .any(|row| is_harness(row));
        let mut stack = vec![*pane_row];
        while let Some(row) = stack.pop() {
            if !claimed.insert(row.pid) {
                continue;
            }
            let harness = if row.pid == pane.pane_pid {
                !any_harness || is_harness(row)
            } else {
                is_harness(row)
            };
            if harness {
                harnesses.push((row.pid, owner.clone()));
            }
            for kid in children.get(&row.pid).into_iter().flatten() {
                if harness && !is_harness(kid) {
                    let pids = subtree(&children, kid.pid);
                    claimed.extend(pids.iter().copied());
                    roots.push(CommandRoot {
                        session_id: pane.session_id.clone(),
                        session_name: pane.session_name.clone(),
                        pid: kid.pid,
                        pgid: kid.pgid,
                        harness_pid: Some(row.pid),
                        pids,
                        detached: false,
                    });
                } else {
                    stack.push(kid);
                }
            }
        }
    }
    let mut detached: BTreeMap<i32, Vec<i32>> = BTreeMap::new();
    for row in rows {
        if row.ppid == 1 && groups.contains_key(&row.pgid) && !claimed.contains(&row.pid) {
            detached.entry(row.pgid).or_default().push(row.pid);
        }
    }
    for (pgid, members) in detached {
        let owner = &groups[&pgid];
        let mut pids: Vec<i32> = Vec::new();
        for member in &members {
            for pid in subtree(&children, *member) {
                if claimed.insert(pid) {
                    pids.push(pid);
                }
            }
        }
        if pids.is_empty() {
            continue;
        }
        roots.push(CommandRoot {
            session_id: owner.session_id.clone(),
            session_name: owner.session_name.clone(),
            pid: pids[0],
            pgid,
            harness_pid: None,
            pids,
            detached: true,
        });
    }
    (roots, harnesses)
}

fn subtree(children: &HashMap<i32, Vec<&ProcessRow>>, root: i32) -> Vec<i32> {
    let mut pids = vec![root];
    let mut seen: HashSet<i32> = HashSet::from([root]);
    let mut next = 0;
    while next < pids.len() {
        if let Some(kids) = children.get(&pids[next]) {
            for kid in kids {
                if seen.insert(kid.pid) {
                    pids.push(kid.pid);
                }
            }
        }
        next += 1;
    }
    pids
}

/// Members of process group `pgid` and every descendant of theirs: one
/// shell job's processes.
fn group_tree(rows: &[ProcessRow], pgid: i32) -> Vec<i32> {
    let mut children: HashMap<i32, Vec<&ProcessRow>> = HashMap::new();
    for row in rows {
        if row.pid != row.ppid {
            children.entry(row.ppid).or_default().push(row);
        }
    }
    let mut pids: Vec<i32> = Vec::new();
    let mut seen = HashSet::new();
    for row in rows.iter().filter(|row| row.pgid == pgid) {
        for pid in subtree(&children, row.pid) {
            if seen.insert(pid) {
                pids.push(pid);
            }
        }
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
    // Identities taken before SIGTERM: a PID freed and reused during the
    // grace period must not receive the SIGKILL.
    let identities: Vec<_> = pids
        .iter()
        .map(|pid| (*pid, process_identity(*pid)))
        .collect();
    for pid in pids {
        let _ = kill(Pid::from_raw(*pid), Signal::SIGTERM);
    }
    thread::spawn(move || {
        thread::sleep(KILL_GRACE);
        for (pid, identity) in identities {
            if identity.is_some() && process_identity(pid) == identity {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        }
    });
}

#[cfg(target_os = "macos")]
type ProcessIdentity = crate::local_sockets::identity::ProcessIdentity;
#[cfg(not(target_os = "macos"))]
type ProcessIdentity = String;

/// The kernel's never-reused identity of a live process.
fn process_identity(pid: i32) -> Option<ProcessIdentity> {
    #[cfg(target_os = "macos")]
    {
        let pid = u32::try_from(pid).ok()?;
        crate::local_sockets::identity::snapshot(pid)
            .ok()
            .map(|snapshot| snapshot.identity)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Start time in clock ticks, field 22 of /proc/<pid>/stat.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_command = stat.rsplit_once(')')?.1;
        after_command.split_whitespace().nth(19).map(str::to_owned)
    }
}

/// A process's kernel identity as shell jobs record it, to tell the process
/// sm adopted from a later one that reused its PID.
pub(crate) fn process_identity_text(pid: i32) -> Option<String> {
    process_identity(pid).map(|identity| format!("{identity:?}"))
}

/// `CLAUDE_SESSION_MANAGER_ID` from a process's environment, which sm sets
/// in every agent's shell and every child inherits.
#[cfg(not(target_os = "macos"))]
fn environ_session_id(pid: i32) -> Option<String> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    environ_value(&environ, "CLAUDE_SESSION_MANAGER_ID")
}

#[cfg_attr(target_os = "macos", allow(dead_code))] // Linux only, tested everywhere
fn environ_value(environ: &[u8], key: &str) -> Option<String> {
    environ.split(|byte| *byte == 0).find_map(|entry| {
        let entry = std::str::from_utf8(entry).ok()?;
        let (name, value) = entry.split_once('=')?;
        (name == key && !value.is_empty()).then(|| value.to_owned())
    })
}

fn process_started_at(pid: i32) -> Option<OffsetDateTime> {
    #[cfg(target_os = "macos")]
    {
        let identity = process_identity(pid)?;
        let nanos = i128::from(identity.started_seconds) * 1_000_000_000
            + i128::from(identity.started_microseconds) * 1_000;
        OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Field 22 of /proc/<pid>/stat: start time in clock ticks after boot.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ticks: i64 = stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(19)?
            .parse()
            .ok()?;
        let boot: i64 = std::fs::read_to_string("/proc/stat")
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .trim()
            .parse()
            .ok()?;
        // SAFETY: sysconf reads a constant; it returns -1 when unknown.
        let hz = i128::from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1));
        let nanos = i128::from(boot) * 1_000_000_000 + i128::from(ticks) * 1_000_000_000 / hz;
        OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()
    }
}

/// The process's working directory.
fn process_cwd(pid: i32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
        // SAFETY: the kernel fills the correctly sized C-layout record.
        let copied = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if copied != size {
            return None;
        }
        // SAFETY: an exact-size copy initialized the record.
        let info = unsafe { info.assume_init() };
        let path = &info.pvi_cdir.vip_path;
        let bytes: Vec<u8> = path
            .iter()
            .flat_map(|row| row.iter())
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .map(|path| path.display().to_string())
    }
}

/// Live sm sessions and every one of their tmux panes, one `list-panes` per
/// socket.
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
            for pid in pane_pids.get(&session.tmux_session).into_iter().flatten() {
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

/// Pane pids by tmux session name; a session can have several windows.
fn parse_pane_listing(text: &str) -> HashMap<String, Vec<i32>> {
    let mut panes: HashMap<String, Vec<i32>> = HashMap::new();
    for line in text.lines() {
        let Some((name, pid)) = line.trim().rsplit_once(' ') else {
            continue;
        };
        if let Ok(pid) = pid.parse() {
            panes.entry(name.to_owned()).or_default().push(pid);
        }
    }
    panes
}

fn process_listing() -> Option<String> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,rss=,comm="])
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
            let (pgid, rest) = field(&rest)?;
            let (rss, command) = field(&rest)?;
            Some(ProcessRow {
                pid: pid.parse().ok()?,
                ppid: ppid.parse().ok()?,
                pgid: pgid.parse().ok()?,
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

fn process_arguments_text(pid: i32, max: usize) -> Option<String> {
    let output = Command::new("ps")
        .args(["-ww", "-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !text.is_empty()).then(|| truncate_chars(&text, max))
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

fn kill_notice_text(kill: &AgentProcessKill, command: &str, cause: StopCause) -> String {
    let children = match kill.descendants {
        0 => String::new(),
        1 => " and its 1 child process".to_owned(),
        n => format!(" and its {n} child processes"),
    };
    let why = match cause {
        StopCause::NoHeadroom { headroom } => format!(
            "It passed {limit} while the Mac had only {headroom} of memory to spare beyond its reserve, \
too little to set aside for it, so sm stopped it instead of taking it into the queue.",
            limit = memory_amount_text(kill.limit_bytes),
            headroom = memory_amount_text(headroom.max(0)),
        ),
        StopCause::Untrackable => format!(
            "It passed {limit} and shares a process group with your agent program, so sm could not \
take it into the queue on its own and stopped it.",
            limit = memory_amount_text(kill.limit_bytes),
        ),
    };
    format!(
        "[sm queue] At {at} sm stopped `{label}` (PID {pid}{children}), which you started from your shell. {why} \
It was using {memory} and may have been still climbing. Rerun it through the queue so it waits for memory:\n\n{resubmit}",
        at = notice_time(&kill.killed_at),
        label = shell::label_text(command),
        pid = kill.pid,
        memory = memory_amount_text(kill.memory_bytes),
        resubmit = shell::resubmit_text(command, kill.memory_bytes, None),
    )
}

#[cfg(target_os = "macos")]
mod fork_watch {
    //! Kernel fork notices on agent harnesses. The shell behind a detached
    //! command (`nohup … &`) exits within milliseconds, too fast for the
    //! 2-second pass, so each new child's process group is recorded the
    //! moment its harness forks.

    use std::{
        collections::HashMap,
        sync::{mpsc, Arc, Mutex},
        thread,
    };

    use super::GroupOwner;

    pub(super) struct ForkWatcher {
        sender: Mutex<mpsc::Sender<(i32, GroupOwner)>>,
    }

    impl ForkWatcher {
        pub(super) fn spawn(groups: Arc<Mutex<HashMap<i32, GroupOwner>>>) -> Self {
            let (sender, receiver) = mpsc::channel::<(i32, GroupOwner)>();
            thread::spawn(move || run(receiver, groups));
            Self {
                sender: Mutex::new(sender),
            }
        }

        pub(super) fn watch(&self, harnesses: &[(i32, GroupOwner)]) {
            let sender = self.sender.lock().unwrap_or_else(|p| p.into_inner());
            for harness in harnesses {
                let _ = sender.send(harness.clone());
            }
        }
    }

    fn run(
        receiver: mpsc::Receiver<(i32, GroupOwner)>,
        groups: Arc<Mutex<HashMap<i32, GroupOwner>>>,
    ) {
        // SAFETY: kqueue takes no arguments; a negative result is an error.
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            eprintln!("agent memory guard: kqueue unavailable; detached commands are seen only by sampling");
            return;
        }
        let mut watched: HashMap<i32, GroupOwner> = HashMap::new();
        let mut events: Vec<libc::kevent> = vec![empty_event(); 32];
        loop {
            loop {
                match receiver.try_recv() {
                    Ok((pid, owner)) => {
                        if watched.contains_key(&pid) {
                            continue;
                        }
                        let change = proc_event(pid, libc::EV_ADD | libc::EV_CLEAR);
                        // SAFETY: one valid change record, no event output.
                        let added = unsafe {
                            libc::kevent(
                                queue,
                                &change,
                                1,
                                std::ptr::null_mut(),
                                0,
                                std::ptr::null(),
                            )
                        };
                        if added == 0 {
                            watched.insert(pid, owner);
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 500_000_000,
            };
            // SAFETY: `events` has room for the count passed.
            let ready = unsafe {
                libc::kevent(
                    queue,
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as i32,
                    &timeout,
                )
            };
            for event in events.iter().take(usize::try_from(ready).unwrap_or(0)) {
                let Ok(pid) = i32::try_from(event.ident) else {
                    continue;
                };
                if event.fflags & libc::NOTE_EXIT != 0 {
                    watched.remove(&pid);
                    continue;
                }
                if event.fflags & libc::NOTE_FORK == 0 {
                    continue;
                }
                let Some(owner) = watched.get(&pid) else {
                    continue;
                };
                let mut record = groups.lock().unwrap_or_else(|p| p.into_inner());
                for child in child_pids(pid) {
                    // SAFETY: getpgid reads another process's group; -1 on error.
                    if unsafe { libc::getpgid(child) } == child {
                        record.insert(child, owner.clone());
                    }
                }
            }
        }
    }

    fn empty_event() -> libc::kevent {
        proc_event(0, 0)
    }

    fn proc_event(pid: i32, flags: u16) -> libc::kevent {
        libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags,
            fflags: libc::NOTE_FORK | libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        }
    }

    fn child_pids(pid: i32) -> Vec<i32> {
        let mut pids = vec![0i32; 256];
        let bytes = (pids.len() * std::mem::size_of::<i32>()) as i32;
        // SAFETY: the buffer holds `bytes` bytes of pid slots.
        let filled = unsafe { libc::proc_listchildpids(pid, pids.as_mut_ptr().cast(), bytes) };
        let count = usize::try_from(filled).unwrap_or(0).min(pids.len());
        pids.truncate(count);
        pids.retain(|pid| *pid > 0);
        pids
    }
}

/// `HH:MM:SS UTC`. The notice waits until the agent is idle, so without a
/// time it can read as being about whatever the agent ran last (sm#2142).
fn notice_time(killed_at: &str) -> String {
    OffsetDateTime::parse(killed_at, &Rfc3339)
        .map(|at| format!("{:02}:{:02}:{:02} UTC", at.hour(), at.minute(), at.second()))
        .unwrap_or_else(|_| killed_at.to_owned())
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

pub(crate) fn recent_agent_process_kills_conn(
    conn: &Connection,
    session_id: Option<&str>,
) -> Result<Vec<AgentProcessKill>> {
    list_kills_since(
        conn,
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
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='agent_process_kills')",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(Vec::new());
    }
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
