//! Shell jobs (sm#2412): commands an agent ran from its own shell that sm
//! took into the queue once one of their processes passed the agent process
//! memory limit, instead of killing them.
//!
//! A shell job is identified by its process group: the command's own group,
//! or for a command the agent detached, the group of the shell that started
//! it. It is already running when sm finds it, so it never waits, takes no
//! queue slot and sends no completion wake; the agent's shell reports how it
//! ended. Its memory reservation only decides the host memory guard's stop
//! order (appendix D of docs/working/2412_shell_jobs.html).

use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, OnceLock},
};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use super::{
    finish_queue_job_conn, generate_queue_job_id, get_queue_job_runtime_conn,
    init_queue_jobs_schema, memory_amount_text, now_rfc3339, open_queue_jobs_connection,
    process_group_exists, QueueJobRuntimeRecord, MEMORY_GUARD_HOST_PRESSURE,
};

pub const SHELL_JOB_TYPE: &str = "shell";

const GIB: i64 = 1024 * 1024 * 1024;

/// (headroom at least, reservation): sized from the week of queue readings
/// in the memo's "Sizing the reservation" table.
pub const RESERVATION_TIERS: [(i64, i64); 2] = [(100 * GIB, 16 * GIB), (20 * GIB, 8 * GIB)];

const MAX_LABEL_CHARS: usize = 120;

/// The reservation for a command adopted with `headroom` to spare, or `None`
/// when there is too little to adopt it.
pub fn reservation_for_headroom(headroom: i64) -> Option<i64> {
    RESERVATION_TIERS
        .iter()
        .find(|(at_least, _)| headroom >= *at_least)
        .map(|(_, reservation)| *reservation)
}

/// Free memory less the reserve and what running jobs reserved but have not
/// used yet: `(reservation, current use)` per running job with a reservation.
pub fn headroom_bytes(available: i64, reserve: i64, reserved: &[(i64, i64)]) -> i64 {
    let unspent = reserved
        .iter()
        .map(|(reservation, current)| reservation.saturating_sub(*current).max(0))
        .fold(0i64, i64::saturating_add);
    available.saturating_sub(reserve).saturating_sub(unspent)
}

/// Latest and peak memory of each running shell job, as the agent memory
/// guard last measured them. Not persisted: a restart starts the peak over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ShellJobUsage {
    pub current_bytes: i64,
    pub peak_bytes: i64,
}

fn usage_map() -> &'static Mutex<HashMap<String, ShellJobUsage>> {
    static MAP: OnceLock<Mutex<HashMap<String, ShellJobUsage>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn record_usage(job_id: &str, bytes: i64) {
    let mut map = usage_map().lock().unwrap_or_else(|p| p.into_inner());
    let usage = map.entry(job_id.to_owned()).or_default();
    usage.current_bytes = bytes;
    usage.peak_bytes = usage.peak_bytes.max(bytes);
}

pub fn usage(job_id: &str) -> Option<ShellJobUsage> {
    usage_map()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(job_id)
        .copied()
}

fn forget_usage(job_id: &str) {
    usage_map()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(job_id);
}

/// Over its reservation: the host memory guard stops these first.
pub fn over_reservation(job_id: &str, reservation: Option<i64>) -> bool {
    match (usage(job_id), reservation) {
        (Some(usage), Some(reservation)) => usage.current_bytes > reservation,
        _ => false,
    }
}

/// What sm recorded when it adopted the command, kept in
/// `queue_jobs.shell_adoption_json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptionRecord {
    pub adopted_at: String,
    /// The process that passed the limit, and its memory then.
    pub trigger_pid: i64,
    pub trigger_bytes: i64,
    pub headroom_bytes: i64,
    /// The kernel identity of the command's root process.
    pub root_identity: Option<String>,
    /// The agent detached the command; its group outlived the shell.
    pub detached: bool,
}

/// A command to adopt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    pub session_id: String,
    pub root_pid: i64,
    pub process_group_id: i64,
    pub label: String,
    pub command: String,
    pub cwd: String,
    pub reservation_bytes: i64,
    /// When the root process started; the job's `queued_at`.
    pub process_started_at: Option<String>,
    pub record: AdoptionRecord,
}

/// Inserts the running shell job and returns its id. `queued_at` is when the
/// command started and `started_at` when sm adopted it.
pub fn adopt_conn(conn: &Connection, adoption: &Adoption) -> Result<String> {
    init_queue_jobs_schema(conn)?;
    let id = generate_queue_job_id();
    let adopted_at = adoption.record.adopted_at.clone();
    conn.execute(
        r#"
        INSERT INTO queue_jobs (
            id, type, label, requester_session_id, notify_session_id, cwd, argv_json,
            env_json, timeout_seconds, max_wait_seconds, memory_bytes, state,
            queued_at, started_at, pid, process_group_id, shell_adoption_json
        ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, '{}', 0, 0, ?7, 'running', ?8, ?9, ?10, ?11, ?12)
        "#,
        params![
            id,
            SHELL_JOB_TYPE,
            adoption.label,
            adoption.session_id,
            adoption.cwd,
            serde_json::to_string(&[&adoption.command])?,
            adoption.reservation_bytes,
            adoption
                .process_started_at
                .clone()
                .unwrap_or_else(|| adopted_at.clone()),
            adopted_at,
            adoption.root_pid,
            adoption.process_group_id,
            serde_json::to_string(&adoption.record)?,
        ],
    )?;
    Ok(id)
}

pub fn adoption_record_conn(conn: &Connection, job_id: &str) -> Result<Option<AdoptionRecord>> {
    let raw: Option<Option<String>> = conn
        .query_row(
            "SELECT shell_adoption_json FROM queue_jobs WHERE id = ?1",
            params![job_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok()))
}

/// The adoption record of a job, read on its own connection.
pub fn adoption_record(queue_db_path: &Path, job_id: &str) -> Option<AdoptionRecord> {
    let conn = open_queue_jobs_connection(queue_db_path).ok()?;
    adoption_record_conn(&conn, job_id).ok().flatten()
}

/// A running shell job as the agent memory guard tracks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningShellJob {
    pub id: String,
    pub session_id: String,
    pub process_group_id: i64,
    pub reservation_bytes: Option<i64>,
}

pub fn running_shell_jobs_conn(conn: &Connection) -> Result<Vec<RunningShellJob>> {
    init_queue_jobs_schema(conn)?;
    let mut statement = conn.prepare(
        r#"
        SELECT id, notify_session_id, process_group_id, memory_bytes
        FROM queue_jobs
        WHERE type = ?1 AND state = 'running' AND process_group_id IS NOT NULL
        ORDER BY started_at, id
        "#,
    )?;
    let rows = statement.query_map(params![SHELL_JOB_TYPE], |row| {
        Ok(RunningShellJob {
            id: row.get(0)?,
            session_id: row.get(1)?,
            process_group_id: row.get(2)?,
            reservation_bytes: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Finishes a shell job whose processes are all gone. A stop sm began sets
/// the terminal state: the host memory guard's `memory_terminating` finishes
/// as `memory_exceeded`, a cancel's `cancelling` as `cancelled`; otherwise
/// the command simply `ended`.
pub fn finish_gone_conn(
    conn: &Connection,
    job_id: &str,
    message_queue_db_path: &Path,
) -> Result<()> {
    let Some(job) = get_queue_job_runtime_conn(conn, job_id)? else {
        forget_usage(job_id);
        return Ok(());
    };
    let state = match job.holding_reason.as_deref() {
        Some("memory_terminating") => "memory_exceeded",
        Some("cancelling") => "cancelled",
        _ => "ended",
    };
    finish_queue_job_conn(conn, &job, state, None, Some(message_queue_db_path))?;
    forget_usage(job_id);
    Ok(())
}

/// Startup check for a running shell job: keep it while its process group
/// lives, otherwise finish it. A job adopted before the Mac last booted died
/// with it, whatever now holds its group id. A stop sm began before the
/// restart (`cancelling` or `memory_terminating`) is signalled again with a
/// fresh grace, since nothing else resumes it; the guard's next pass after
/// the group exits finishes the job. Returns whether it still runs.
pub(super) fn recover_conn(
    conn: &Connection,
    job: &QueueJobRuntimeRecord,
    message_queue_db_path: &Path,
    started_before_boot: bool,
    cancel_grace_seconds: u64,
) -> Result<bool> {
    if started_before_boot {
        finish_queue_job_conn(conn, job, "host_restart", None, Some(message_queue_db_path))?;
        forget_usage(&job.id);
        return Ok(false);
    }
    // The group id alone may have been reused since the command ended: keep
    // the job only while its root is the very process sm adopted.
    let root_matches = || {
        let recorded = adoption_record_conn(conn, &job.id)
            .ok()
            .flatten()?
            .root_identity?;
        let pid = i32::try_from(job.pid?).ok()?;
        Some(crate::agent_memory_guard::process_identity_text(pid)? == recorded)
    };
    if job.process_group_id.is_some_and(process_group_exists) && root_matches() == Some(true) {
        if let (Some(pgid), Some("cancelling" | "memory_terminating")) =
            (job.process_group_id, job.holding_reason.as_deref())
        {
            std::thread::spawn(move || {
                super::terminate_process_group_with_grace(pgid, cancel_grace_seconds)
            });
        }
        return Ok(true);
    }
    finish_gone_conn(conn, &job.id, message_queue_db_path)?;
    Ok(false)
}

/// The command an agent ran, from its root process's arguments. Claude's
/// shell wraps it as `zsh -c '…eval '<command>' < /dev/null && pwd -P >| …'`;
/// Codex and opencode run it bare or as `<shell> -c|-lc <command>`.
pub fn command_text(args: &str) -> String {
    let args = args.trim();
    if let Some(start) = args.find(" eval '") {
        if let Some(command) = single_quoted(&args[start + " eval '".len()..]) {
            return command;
        }
    }
    let mut words = args.splitn(3, ' ');
    if let (Some(shell), Some(flag), Some(script)) = (words.next(), words.next(), words.next()) {
        let shell = shell.rsplit('/').next().unwrap_or(shell);
        if matches!(shell, "sh" | "bash" | "zsh" | "dash" | "fish")
            && matches!(flag, "-c" | "-lc" | "-ic")
        {
            return script.trim().to_owned();
        }
    }
    args.to_owned()
}

/// The command an agent ran, from its root process's argument vector, which
/// keeps argument boundaries that [`command_text`] cannot recover from `ps`.
/// A shell wrapper yields its script (Claude's `eval '…'` unwrapped); a
/// program run directly yields its arguments, each quoted as the shell needs.
pub fn command_from_argv(argv: &[String]) -> String {
    // Arguments after the script become its $0, $1…: keep the whole argv.
    if let [shell, flag, script] = argv {
        let name = shell.rsplit('/').next().unwrap_or(shell);
        if matches!(name, "sh" | "bash" | "zsh" | "dash" | "fish")
            && matches!(flag.as_str(), "-c" | "-lc" | "-ic")
        {
            if let Some(start) = script.find("eval '") {
                if let Some(command) = single_quoted(&script[start + "eval '".len()..]) {
                    return command;
                }
            }
            return script.clone();
        }
    }
    argv.iter()
        .map(|arg| {
            let plain = !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
            if plain {
                arg.clone()
            } else {
                super::shell_quote(arg)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The text of a single-quoted shell word, `'\''` read as a quote; `text`
/// starts just after the opening quote.
fn single_quoted(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = text;
    loop {
        let end = rest.find('\'')?;
        out.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        match rest.strip_prefix("\\''") {
            Some(after) => {
                out.push('\'');
                rest = after;
            }
            None => return Some(out),
        }
    }
}

/// The queue label: the command on one line, at most 120 characters.
pub fn label_text(command: &str) -> String {
    let line = command.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(MAX_LABEL_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line,
    }
}

/// The command without leading `cd … &&` / `cd …;` segments.
fn without_cd(command: &str) -> &str {
    let mut rest = command.trim();
    while let Some(after_cd) = rest.strip_prefix("cd ") {
        let and = after_cd.find("&&").map(|at| (at, 2));
        let semi = after_cd.find(';').map(|at| (at, 1));
        let Some((at, len)) = [and, semi].into_iter().flatten().min() else {
            break;
        };
        rest = after_cd[at + len..].trim_start();
    }
    rest
}

/// `tests` when the command is plainly a test run or a build check, the
/// work `sm queue run --type tests` is for. sm cannot know what anything
/// else is for, so it leaves the type to the agent.
pub fn type_hint(command: &str) -> Option<&'static str> {
    let command = without_cd(command);
    let words: Vec<&str> = command.split_whitespace().take(3).collect();
    let first = words.first().copied().unwrap_or_default();
    let first = first.rsplit('/').next().unwrap_or(first);
    let second = words.get(1).copied().unwrap_or_default();
    let tests = match first {
        "cargo" => matches!(second, "test" | "nextest" | "build" | "check" | "clippy"),
        "pytest" | "jest" | "gradle" | "gradlew" => true,
        "python" | "python3" => second == "-m" && words.get(2) == Some(&"pytest"),
        "npx" => second == "jest",
        _ => command.starts_with("scripts/test") || command.starts_with("./scripts/test"),
    };
    tests.then_some("tests")
}

/// A `--label` for resubmitting: the command's first two words.
fn label_slug(command: &str) -> String {
    let slug = without_cd(command)
        .split_whitespace()
        .take(2)
        .map(|word| {
            let word = word.rsplit('/').next().unwrap_or(word);
            word.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("-");
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        "shell-command".to_owned()
    } else {
        slug
    }
}

/// The command as `sm queue run` arguments: bare when it is plain words,
/// else through `zsh -lc` so pipes and `&&` keep working.
fn command_arguments(command: &str) -> String {
    let plain = command
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || " -_./=:,+@%".contains(c));
    if plain {
        command.to_owned()
    } else {
        format!("zsh -lc '{}'", command.replace('\'', "'\\''"))
    }
}

/// Whole gigabytes, rounded up, at 1.5 times the peak.
fn suggested_memory_gb(peak_bytes: i64) -> i64 {
    let bytes = peak_bytes.saturating_mul(3) / 2;
    ((bytes + GIB - 1) / GIB).max(2)
}

/// The resubmit line and, when sm cannot name the type, how to pick it.
pub fn resubmit_text(command: &str, peak_bytes: i64, cwd: Option<&str>) -> String {
    let cwd = cwd
        .filter(|cwd| !cwd.is_empty())
        .map(|cwd| format!(" --cwd {}", super::shell_quote(cwd)))
        .unwrap_or_default();
    let line = format!(
        "  sm queue run --type {} --label {}{cwd} --memory {}G -- {}",
        type_hint(command).unwrap_or("<type>"),
        label_slug(command),
        suggested_memory_gb(peak_bytes),
        command_arguments(command),
    );
    if type_hint(command).is_some() {
        line
    } else {
        format!(
            "{line}\n\nPick the type: tests for a test suite or build check; background for other long work \
(a performance run may stop it to get the machine quiet); perf only for a timing measurement, \
which needs --timeout and --cpu as well."
        )
    }
}

/// `HH:MM:SS UTC`, or the text as given when it does not parse.
pub fn clock_text(at: &str) -> String {
    OffsetDateTime::parse(at, &Rfc3339)
        .map(|at| format!("{:02}:{:02}:{:02} UTC", at.hour(), at.minute(), at.second()))
        .unwrap_or_else(|_| at.to_owned())
}

/// The message to the agent when a shell job ends, or `None` when it needs
/// none: an `ended` command already reported to the agent's own shell.
pub(super) fn completion_text(
    job: &QueueJobRuntimeRecord,
    state: &str,
    finished_at: &str,
) -> Option<String> {
    let detail = job
        .termination_detail_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<JsonValue>(raw).ok());
    let bytes = |key: &str| {
        detail
            .as_ref()
            .and_then(|detail| detail.get(key))
            .and_then(JsonValue::as_i64)
    };
    let usage = usage(&job.id).unwrap_or_default();
    let peak = usage
        .peak_bytes
        .max(bytes("process_group_rss_bytes").unwrap_or(0));
    let reserved = job
        .memory_bytes
        .map(|bytes| {
            format!(
                ", above the {} sm had set aside for it",
                memory_amount_text(bytes)
            )
        })
        .filter(|_| job.memory_bytes.is_some_and(|reserved| peak > reserved))
        .unwrap_or_default();
    let cause = match state {
        "memory_exceeded" => {
            let cause = detail
                .as_ref()
                .and_then(|detail| detail.get("cause"))
                .and_then(JsonValue::as_str);
            match (
                cause,
                bytes("host_available_bytes"),
                bytes("effective_reserve_bytes"),
            ) {
                (Some(MEMORY_GUARD_HOST_PRESSURE), Some(available), Some(reserve)) => format!(
                    "The Mac ran short of memory ({} free; it keeps {} in reserve), and this command was using {}{reserved}.",
                    memory_amount_text(available),
                    memory_amount_text(reserve),
                    memory_amount_text(peak),
                ),
                _ => format!(
                    "The Mac ran short of memory, and this command was using {}{reserved}.",
                    memory_amount_text(peak)
                ),
            }
        }
        "cancelled" => {
            let by = detail
                .as_ref()
                .and_then(|detail| detail.get("cancelled_by"))
                .and_then(JsonValue::as_str)
                .filter(|by| !by.trim().is_empty());
            match by {
                Some(by) if job.notify_session_id.as_deref() != Some(by) => {
                    format!("It was stopped by {by}.")
                }
                _ => "It was cancelled with sm queue cancel.".to_owned(),
            }
        }
        _ => return None,
    };
    let note = detail
        .as_ref()
        .and_then(|detail| detail.get("note"))
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|note| !note.is_empty())
        .map(|note| {
            let stop = if note.ends_with(['.', '!', '?']) {
                ""
            } else {
                "."
            };
            format!(" Note: {note}{stop}")
        })
        .unwrap_or_default();
    let cause = format!("{cause}{note}");
    // The exact command as adopted; the label is cut and collapsed for display.
    let command = job
        .argv_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .and_then(|argv| argv.into_iter().next())
        .unwrap_or_else(|| job.label.clone());
    Some(format!(
        "[sm queue] At {at} sm stopped `{label}` (PID {pid}), which you started from your shell at {started}. \
{cause} Rerun it through the queue so it waits for memory instead of being stopped:\n\n{resubmit}",
        at = clock_text(finished_at),
        label = job.label,
        pid = job.pid.unwrap_or_default(),
        started = clock_text(&job.queued_at),
        resubmit = resubmit_text(&command, peak, None),
    ))
}

/// The Queue screen's lines for a running shell job: a summary when it is
/// over its reservation, and how it came to be in the queue.
pub fn holding_text(
    job_id: &str,
    agent: &str,
    reservation: Option<i64>,
    adopted_at: Option<&str>,
) -> (Option<String>, String) {
    let summary = over_reservation(job_id, reservation)
        .then(|| "Over its reservation · stops first if memory runs short".to_owned());
    let at = adopted_at
        .map(clock_text)
        .unwrap_or_else(|| "some point".to_owned());
    let set_aside = reservation
        .map(|bytes| format!(" with {} set aside", memory_amount_text(bytes)))
        .unwrap_or_default();
    let detail = format!(
        "{agent} started this from its own shell. At {at} it passed 1 GB and sm took it into the queue{set_aside}."
    );
    (summary, detail)
}

/// Now, as the adoption record's time.
pub fn adopted_now() -> String {
    now_rfc3339()
}

#[cfg(test)]
mod tests;
