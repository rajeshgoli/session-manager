//! Host restart recovery (sm#2054).
//!
//! sm keeps the host's boot identity. When a server starts on a new boot,
//! every agent the reboot killed joins one *restart cohort*: the agents sm
//! stopped at startup because their runtime vanished. The cohort records what
//! each agent was doing, and why the host went down when macOS left a panic or
//! Jetsam (memory pressure) report. `sm recover` and the app's banner restore
//! the cohort through the ordinary restore path; each restored agent gets one
//! notice of what it lost.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

/// Where macOS writes panic and Jetsam reports.
pub const DIAGNOSTIC_REPORTS_DIR: &str = "/Library/Logs/DiagnosticReports";
/// How far before the last pre-restart sample a queue job's memory counts.
const LIKELY_JOB_WINDOW_MS: i64 = 30 * 60 * 1000;
/// A queue job is the likely cause when it held this share of host memory.
const LIKELY_JOB_HOST_SHARE: f64 = 0.25;
/// Without a host total, a job this large is the likely cause.
const LIKELY_JOB_FALLBACK_BYTES: i64 = 32 * 1024 * 1024 * 1024;
const JETSAM_TOP_PROCESSES: usize = 3;

pub const DECISION_PENDING: &str = "pending";
pub const DECISION_RESTORED: &str = "restored";
pub const DECISION_LEFT: &str = "left";
pub const DECISION_FAILED: &str = "failed";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootIdentity {
    pub id: String,
    pub booted_at: OffsetDateTime,
}

/// This boot's identity, or `None` where the platform gives none.
pub fn current_boot() -> Option<BootIdentity> {
    Some(BootIdentity {
        id: boot_id()?,
        booted_at: system_boot_time()?,
    })
}

fn boot_id() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "kern.bootsessionuuid"])
            .output()
            .ok()?;
        let id = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        (output.status.success() && !id.is_empty()).then_some(id)
    }
    #[cfg(target_os = "linux")]
    {
        let id = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let id = id.trim().to_owned();
        (!id.is_empty()).then_some(id)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Read the OS boot timestamp. Unknown platforms conservatively return `None`.
pub fn system_boot_time() -> Option<OffsetDateTime> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "kern.boottime"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let seconds = text
            .split("sec = ")
            .nth(1)?
            .split(',')
            .next()?
            .trim()
            .parse::<i64>()
            .ok()?;
        OffsetDateTime::from_unix_timestamp(seconds).ok()
    }
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string("/proc/stat").ok()?;
        let seconds = stat
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .trim()
            .parse::<i64>()
            .ok()?;
        OffsetDateTime::from_unix_timestamp(seconds).ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// "11:49 am" in the host's local time.
pub fn clock_text(at: OffsetDateTime) -> String {
    let Some(local) = crate::queue::local_now_naive(at) else {
        return rfc3339(at);
    };
    let hour = local.hour();
    format!(
        "{}:{:02} {}",
        if hour % 12 == 0 { 12 } else { hour % 12 },
        local.minute(),
        if hour < 12 { "am" } else { "pm" }
    )
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

fn parse_rfc3339(text: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(text.trim(), &Rfc3339).ok()
}

fn gib_text(bytes: i64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// Why the host went down, as far as its reports say.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RestartCause {
    /// First line of the kernel panic string, without its `panic(cpu …):` prefix.
    pub panic_reason: Option<String>,
    pub panic_report: Option<String>,
    /// The largest processes in the last Jetsam report before the restart.
    #[serde(default)]
    pub jetsam_top: Vec<JetsamProcess>,
    pub jetsam_report: Option<String>,
    pub likely_job: Option<LikelyJob>,
}

impl RestartCause {
    pub fn is_unknown(&self) -> bool {
        self.panic_reason.is_none() && self.jetsam_top.is_empty() && self.likely_job.is_none()
    }

    /// One line for a notice, the CLI, or the app.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(reason) = &self.panic_reason {
            parts.push(format!("kernel panic: {reason}."));
        }
        if let Some(job) = &self.likely_job {
            parts.push(format!(
                "Likely cause: queue job `{}` ({}{}), {} at its last sample.",
                job.label,
                job.job_id,
                job.session_name
                    .as_deref()
                    .map(|name| format!(", from {name}"))
                    .unwrap_or_default(),
                gib_text(job.peak_bytes)
            ));
        } else if !self.jetsam_top.is_empty() {
            let top = self
                .jetsam_top
                .iter()
                .map(|process| {
                    format!(
                        "{} (pid {}) {}",
                        process.name,
                        process.pid,
                        gib_text(process.bytes)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("Largest processes under memory pressure: {top}."));
        }
        if parts.is_empty() {
            "Cause unknown: macOS left no panic or memory-pressure report.".to_owned()
        } else {
            parts.join(" ")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JetsamProcess {
    pub name: String,
    pub pid: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LikelyJob {
    pub job_id: String,
    pub label: String,
    pub session_id: Option<String>,
    pub session_name: Option<String>,
    pub peak_bytes: i64,
}

/// A queue job the restart killed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KilledJob {
    pub job_id: String,
    pub label: String,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostRestart {
    pub id: String,
    pub boot_id: String,
    pub booted_at: String,
    pub previous_boot_id: String,
    pub previous_booted_at: String,
    pub detected_at: String,
    pub cause: RestartCause,
    pub cause_scanned: bool,
}

impl HostRestart {
    pub fn booted_at_time(&self) -> Option<OffsetDateTime> {
        parse_rfc3339(&self.booted_at)
    }
}

/// One agent the restart interrupted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CohortMember {
    pub session_id: String,
    pub name: String,
    pub provider: String,
    /// `running` (mid-turn) or `idle`, as last persisted before the restart.
    pub prior_status: String,
    pub last_activity: Option<String>,
    pub working_dir: String,
    /// Active ticket and PR claims, as `ticket owner/repo#N`.
    #[serde(default)]
    pub claims: Vec<String>,
    pub decision: String,
    pub decided_at: Option<String>,
    pub error: Option<String>,
    pub noticed_at: Option<String>,
}

impl CohortMember {
    pub fn new(
        session_id: impl Into<String>,
        name: impl Into<String>,
        provider: impl Into<String>,
        prior_status: impl Into<String>,
        last_activity: Option<String>,
        working_dir: impl Into<String>,
        claims: Vec<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            name: name.into(),
            provider: provider.into(),
            prior_status: prior_status.into(),
            last_activity,
            working_dir: working_dir.into(),
            claims,
            decision: DECISION_PENDING.to_owned(),
            decided_at: None,
            error: None,
            noticed_at: None,
        }
    }

    /// `unknown` when sm found the agent already stopped by an earlier,
    /// unfinished start on the same boot.
    pub fn was_mid_turn(&self) -> bool {
        self.prior_status == "running"
    }

    /// Still waiting on the owner: never restored nor left, or a restore failed.
    pub fn is_open(&self) -> bool {
        matches!(self.decision.as_str(), DECISION_PENDING | DECISION_FAILED)
    }
}

pub struct HostRestartStore {
    db_path: PathBuf,
}

impl HostRestartStore {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    /// Beside the session state file.
    pub fn beside_state_file(state_file: &Path) -> Self {
        Self::new(
            state_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("host_restarts.db"),
        )
    }

    fn open(&self) -> Result<Connection> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("opening {}", self.db_path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS host_boot (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                boot_id TEXT NOT NULL,
                booted_at TEXT NOT NULL,
                recorded_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS host_restarts (
                id TEXT PRIMARY KEY,
                boot_id TEXT NOT NULL UNIQUE,
                booted_at TEXT NOT NULL,
                previous_boot_id TEXT NOT NULL,
                previous_booted_at TEXT NOT NULL,
                detected_at TEXT NOT NULL,
                cause_json TEXT
            );
            CREATE TABLE IF NOT EXISTS host_restart_members (
                restart_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                name TEXT NOT NULL,
                provider TEXT NOT NULL,
                prior_status TEXT NOT NULL,
                last_activity TEXT,
                working_dir TEXT NOT NULL,
                claims_json TEXT NOT NULL DEFAULT '[]',
                decision TEXT NOT NULL DEFAULT 'pending',
                decided_at TEXT,
                error TEXT,
                noticed_at TEXT,
                PRIMARY KEY (restart_id, session_id)
            );
            "#,
        )?;
        Ok(conn)
    }

    /// Compare `current` with the boot sm last ran on. On a new boot, record a
    /// restart and return it; the first boot sm sees only records itself.
    /// The boot counts as handled only after [`Self::commit_boot`], once the
    /// cohort is durable: until then every start on it returns the restart
    /// again, and after it a handover or second start finds nothing.
    pub fn detect(
        &self,
        current: &BootIdentity,
        now: OffsetDateTime,
    ) -> Result<Option<HostRestart>> {
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let previous: Option<(String, String)> = tx
            .query_row(
                "SELECT boot_id, booted_at FROM host_boot WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let booted_at = rfc3339(current.booted_at);
        let now = rfc3339(now);
        if previous.is_none() {
            record_boot(&tx, current, &now)?;
        }
        let restart = match previous {
            Some((previous_id, previous_booted_at)) if previous_id != current.id => {
                let restart = HostRestart {
                    id: restart_id(&current.id),
                    boot_id: current.id.clone(),
                    booted_at,
                    previous_boot_id: previous_id,
                    previous_booted_at,
                    detected_at: now,
                    cause: RestartCause::default(),
                    cause_scanned: false,
                };
                // A start that died before committing the boot left this row.
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT detected_at FROM host_restarts WHERE boot_id = ?1",
                        params![restart.boot_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                let restart = HostRestart {
                    detected_at: existing.unwrap_or(restart.detected_at),
                    ..restart
                };
                tx.execute(
                    r#"
                    INSERT OR IGNORE INTO host_restarts
                        (id, boot_id, booted_at, previous_boot_id, previous_booted_at, detected_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                    "#,
                    params![
                        restart.id,
                        restart.boot_id,
                        restart.booted_at,
                        restart.previous_boot_id,
                        restart.previous_booted_at,
                        restart.detected_at
                    ],
                )?;
                Some(restart)
            }
            _ => None,
        };
        tx.commit()?;
        Ok(restart)
    }

    /// Mark `current` handled: later starts on it detect nothing.
    pub fn commit_boot(&self, current: &BootIdentity, now: OffsetDateTime) -> Result<()> {
        let conn = self.open()?;
        record_boot(&conn, current, &rfc3339(now))
    }

    pub fn add_members(&self, restart_id: &str, members: &[CohortMember]) -> Result<()> {
        let mut conn = self.open()?;
        let tx = conn.transaction()?;
        for member in members {
            tx.execute(
                r#"
                INSERT OR IGNORE INTO host_restart_members
                    (restart_id, session_id, name, provider, prior_status, last_activity,
                     working_dir, claims_json, decision)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                "#,
                params![
                    restart_id,
                    member.session_id,
                    member.name,
                    member.provider,
                    member.prior_status,
                    member.last_activity,
                    member.working_dir,
                    serde_json::to_string(&member.claims)?,
                    member.decision,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_cause(&self, restart_id: &str, cause: &RestartCause) -> Result<()> {
        self.open()?.execute(
            "UPDATE host_restarts SET cause_json = ?2 WHERE id = ?1",
            params![restart_id, serde_json::to_string(cause)?],
        )?;
        Ok(())
    }

    pub fn latest(&self) -> Result<Option<(HostRestart, Vec<CohortMember>)>> {
        let conn = self.open()?;
        let id: Option<String> = conn
            .query_row(
                r#"
                SELECT r.id FROM host_restarts r
                ORDER BY EXISTS (
                    SELECT 1 FROM host_restart_members m
                    WHERE m.restart_id = r.id AND m.decision IN ('pending', 'failed')
                ) DESC, r.booted_at DESC, r.detected_at DESC
                LIMIT 1
                "#,
                [],
                |row| row.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.get_with(&conn, &id),
            None => Ok(None),
        }
    }

    pub fn get(&self, restart_id: &str) -> Result<Option<(HostRestart, Vec<CohortMember>)>> {
        let conn = self.open()?;
        self.get_with(&conn, restart_id)
    }

    fn get_with(
        &self,
        conn: &Connection,
        restart_id: &str,
    ) -> Result<Option<(HostRestart, Vec<CohortMember>)>> {
        let restart = conn
            .query_row(
                r#"
                SELECT id, boot_id, booted_at, previous_boot_id, previous_booted_at,
                       detected_at, cause_json
                FROM host_restarts WHERE id = ?1
                "#,
                params![restart_id],
                |row| {
                    let cause_json: Option<String> = row.get(6)?;
                    Ok(HostRestart {
                        id: row.get(0)?,
                        boot_id: row.get(1)?,
                        booted_at: row.get(2)?,
                        previous_boot_id: row.get(3)?,
                        previous_booted_at: row.get(4)?,
                        detected_at: row.get(5)?,
                        cause_scanned: cause_json.is_some(),
                        cause: cause_json
                            .and_then(|json| serde_json::from_str(&json).ok())
                            .unwrap_or_default(),
                    })
                },
            )
            .optional()?;
        let Some(restart) = restart else {
            return Ok(None);
        };
        let mut statement = conn.prepare(
            r#"
            SELECT session_id, name, provider, prior_status, last_activity, working_dir,
                   claims_json, decision, decided_at, error, noticed_at
            FROM host_restart_members WHERE restart_id = ?1
            ORDER BY name, session_id
            "#,
        )?;
        let members = statement
            .query_map(params![restart_id], |row| {
                let claims: String = row.get(6)?;
                Ok(CohortMember {
                    session_id: row.get(0)?,
                    name: row.get(1)?,
                    provider: row.get(2)?,
                    prior_status: row.get(3)?,
                    last_activity: row.get(4)?,
                    working_dir: row.get(5)?,
                    claims: serde_json::from_str(&claims).unwrap_or_default(),
                    decision: row.get(7)?,
                    decided_at: row.get(8)?,
                    error: row.get(9)?,
                    noticed_at: row.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some((restart, members)))
    }

    /// The open cohort membership of `session_id`, newest restart first.
    pub fn open_membership(&self, session_id: &str) -> Result<Option<(HostRestart, CohortMember)>> {
        let conn = self.open()?;
        let restart_id: Option<String> = conn
            .query_row(
                r#"
                SELECT m.restart_id FROM host_restart_members m
                JOIN host_restarts r ON r.id = m.restart_id
                WHERE m.session_id = ?1 AND m.decision IN ('pending', 'failed')
                ORDER BY r.booted_at DESC LIMIT 1
                "#,
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(restart_id) = restart_id else {
            return Ok(None);
        };
        Ok(self
            .get_with(&conn, &restart_id)?
            .and_then(|(restart, members)| {
                members
                    .into_iter()
                    .find(|member| member.session_id == session_id)
                    .map(|member| (restart, member))
            }))
    }

    /// Record the owner's decision on an open member. False when the member
    /// is unknown or already decided.
    pub fn decide(
        &self,
        restart_id: &str,
        session_id: &str,
        decision: &str,
        error: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let changed = self.open()?.execute(
            r#"
            UPDATE host_restart_members
            SET decision = ?3, decided_at = ?4, error = ?5
            WHERE restart_id = ?1 AND session_id = ?2 AND decision IN ('pending', 'failed')
            "#,
            params![restart_id, session_id, decision, rfc3339(now), error],
        )?;
        Ok(changed > 0)
    }

    pub fn mark_noticed(
        &self,
        restart_id: &str,
        session_id: &str,
        now: OffsetDateTime,
    ) -> Result<()> {
        self.open()?.execute(
            r#"
            UPDATE host_restart_members SET noticed_at = ?3
            WHERE restart_id = ?1 AND session_id = ?2 AND noticed_at IS NULL
            "#,
            params![restart_id, session_id, rfc3339(now)],
        )?;
        Ok(())
    }
}

fn record_boot(conn: &Connection, current: &BootIdentity, now: &str) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO host_boot (singleton, boot_id, booted_at, recorded_at)
        VALUES (1, ?1, ?2, ?3)
        ON CONFLICT(singleton) DO UPDATE SET
            boot_id = excluded.boot_id,
            booted_at = excluded.booted_at,
            recorded_at = excluded.recorded_at
        "#,
        params![current.id, rfc3339(current.booted_at), now],
    )?;
    Ok(())
}

fn restart_id(boot_id: &str) -> String {
    let short: String = boot_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    format!("restart-{short}")
}

/// Inputs for explaining a restart.
pub struct CauseSources<'a> {
    pub reports_dir: &'a Path,
    pub utilization_db: Option<&'a Path>,
    pub queue_db: Option<&'a Path>,
    /// Session id → display name, for naming a likely job's owner.
    pub session_name: &'a dyn Fn(&str) -> Option<String>,
}

/// Read what macOS and sm recorded about the boot that ended at
/// `restart.booted_at`.
pub fn scan_cause(restart: &HostRestart, sources: &CauseSources<'_>) -> RestartCause {
    let previous_boot = parse_rfc3339(&restart.previous_booted_at);
    let boot = parse_rfc3339(&restart.booted_at);
    let mut cause = RestartCause::default();
    // macOS writes the panic report after the reboot; Jetsam reports before it.
    if let Some((path, text)) =
        newest_report(sources.reports_dir, "", ".panic", previous_boot, None)
    {
        cause.panic_reason = panic_reason(&text);
        cause.panic_report = Some(path.display().to_string());
    }
    if let Some((path, text)) = newest_report(
        sources.reports_dir,
        "JetsamEvent-",
        ".ips",
        previous_boot,
        boot,
    ) {
        cause.jetsam_top = jetsam_top(&text, JETSAM_TOP_PROCESSES);
        cause.jetsam_report = Some(path.display().to_string());
    }
    let killed = sources
        .queue_db
        .and_then(|db| killed_jobs(db, restart).ok())
        .unwrap_or_default();
    cause.likely_job = boot
        .and_then(|boot| {
            sources
                .utilization_db
                .and_then(|db| likely_job_from_samples(db, boot, &killed).ok().flatten())
        })
        .map(|mut job| {
            job.session_name = job.session_id.as_deref().and_then(sources.session_name);
            job
        });
    cause
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn newest_report(
    dir: &Path,
    prefix: &str,
    suffix: &str,
    after: Option<OffsetDateTime>,
    before: Option<OffsetDateTime>,
) -> Option<(PathBuf, String)> {
    let mut candidates = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = file_name(&entry.path());
            name.starts_with(prefix) && name.ends_with(suffix)
        })
        .filter_map(|entry| {
            let modified: OffsetDateTime = entry.metadata().ok()?.modified().ok()?.into();
            let in_window = after.is_none_or(|after| modified > after)
                && before.is_none_or(|before| modified < before);
            in_window.then(|| (modified, entry.path()))
        })
        .collect::<Vec<(OffsetDateTime, PathBuf)>>();
    candidates.sort();
    // An unreadable newest report should not hide an older readable one.
    candidates
        .into_iter()
        .rev()
        .find_map(|(_, path)| fs::read_to_string(&path).ok().map(|text| (path, text)))
}

/// A report is a one-line JSON header followed by the JSON body.
fn report_body(text: &str) -> Option<Value> {
    let (_, body) = text.split_once('\n')?;
    serde_json::from_str(body).ok()
}

pub fn panic_reason(text: &str) -> Option<String> {
    let body = report_body(text)?;
    let line = body["panicString"].as_str()?.lines().next()?.trim();
    // "panic(cpu 0 caller 0x…): userspace watchdog timeout: …"
    let reason = match line.strip_prefix("panic(") {
        Some(rest) => rest.split_once("): ").map_or(line, |(_, reason)| reason),
        None => line,
    };
    let reason = reason.trim();
    (!reason.is_empty()).then(|| reason.to_owned())
}

pub fn jetsam_top(text: &str, count: usize) -> Vec<JetsamProcess> {
    let Some(body) = report_body(text) else {
        return Vec::new();
    };
    let page_size = body["memoryStatus"]["pageSize"].as_i64().unwrap_or(16_384);
    let mut processes = body["processes"]
        .as_array()
        .map(|processes| {
            processes
                .iter()
                .filter_map(|process| {
                    Some(JetsamProcess {
                        name: process["name"].as_str()?.to_owned(),
                        pid: process["pid"].as_i64()?,
                        bytes: process["rpages"].as_i64()?.saturating_mul(page_size),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    processes.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.pid.cmp(&b.pid)));
    processes.truncate(count);
    processes
}

fn open_read_only(path: &Path) -> Result<Option<Connection>> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(Some(conn))
}

/// Queue jobs that were running when the host went down: sm finishes them
/// as `host_restart` at startup.
pub fn killed_jobs(queue_db: &Path, restart: &HostRestart) -> Result<Vec<KilledJob>> {
    let Some(conn) = open_read_only(queue_db)? else {
        return Ok(Vec::new());
    };
    let Some(boot) = restart.booted_at_time() else {
        return Ok(Vec::new());
    };
    let mut statement = conn.prepare(
        r#"
        SELECT id, label, COALESCE(requester_session_id, notify_session_id), started_at
        FROM queue_jobs WHERE state = 'host_restart' AND finished_at >= ?1
        ORDER BY started_at, id
        "#,
    )?;
    let rows = statement
        .query_map(params![restart.booted_at], |row| {
            Ok((
                KilledJob {
                    job_id: row.get(0)?,
                    label: row.get(1)?,
                    session_id: row.get(2)?,
                },
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, started)| {
            started
                .as_deref()
                .and_then(parse_rfc3339)
                .is_none_or(|started| started < boot)
        })
        .map(|(job, _)| job)
        .collect())
}

/// The killed job that held the most memory in the utilization recorder's
/// last half hour before the restart, when that was a large share of the
/// host. The recorder measures each job's whole process group.
fn likely_job_from_samples(
    utilization_db: &Path,
    boot: OffsetDateTime,
    killed: &[KilledJob],
) -> Result<Option<LikelyJob>> {
    let Some(conn) = open_read_only(utilization_db)? else {
        return Ok(None);
    };
    let boot_ms = (boot.unix_timestamp_nanos() / 1_000_000) as i64;
    let last: Option<(i64, Option<i64>)> = conn
        .query_row(
            r#"
            SELECT sampled_at_ms, mem_total_bytes FROM host_samples
            WHERE sampled_at_ms < ?1 ORDER BY sampled_at_ms DESC LIMIT 1
            "#,
            params![boot_ms],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((last_ms, total)) = last else {
        return Ok(None);
    };
    let mut statement = conn.prepare(
        r#"
        SELECT job_id, MAX(MAX(COALESCE(footprint_bytes, 0), COALESCE(rss_bytes, 0)))
        FROM job_samples
        WHERE state = 'running' AND sampled_at_ms >= ?1 AND sampled_at_ms <= ?2
        GROUP BY job_id
        "#,
    )?;
    let peaks = statement
        .query_map(params![last_ms - LIKELY_JOB_WINDOW_MS, last_ms], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let threshold = total.map_or(LIKELY_JOB_FALLBACK_BYTES, |total| {
        (total as f64 * LIKELY_JOB_HOST_SHARE) as i64
    });
    Ok(peaks
        .into_iter()
        .filter_map(|(job_id, peak)| {
            let job = killed.iter().find(|job| job.job_id == job_id)?;
            (peak >= threshold).then(|| LikelyJob {
                job_id,
                label: job.label.clone(),
                session_id: job.session_id.clone(),
                session_name: None,
                peak_bytes: peak,
            })
        })
        .max_by_key(|job| job.peak_bytes))
}

/// The message a restored agent receives once.
pub fn notice_text(restart: &HostRestart, member: &CohortMember, killed: &[KilledJob]) -> String {
    let when = restart
        .booted_at_time()
        .map(|boot| format!(" at {}", clock_text(boot)))
        .unwrap_or_default();
    let mut lines = vec![format!(
        "[sm] The Mac restarted{when} and interrupted you; sm restored you in the same conversation."
    )];
    lines.push(format!("Cause: {}", restart.cause.summary()));
    lines.push(match member.prior_status.as_str() {
        "running" => "You were mid-turn when it went down, so that turn was cut off: check what it finished before you continue.",
        "idle" => "You were idle when it went down.",
        _ => "sm could not tell whether you were mid-turn: check what your last turn finished before you continue.",
    }.to_owned());
    let mine: Vec<&KilledJob> = killed
        .iter()
        .filter(|job| job.session_id.as_deref() == Some(member.session_id.as_str()))
        .collect();
    if mine.is_empty() {
        lines.push("None of your queue jobs were running.".to_owned());
    } else {
        lines.push(format!(
            "The restart killed your queue jobs {}; sm does not resubmit them.",
            mine.iter()
                .map(|job| format!("`{}` ({})", job.label, job.job_id))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.push(
        "Anything you started outside sm queue (servers, watchers, background shells) is gone."
            .to_owned(),
    );
    if let Some(job) = restart
        .cause
        .likely_job
        .as_ref()
        .filter(|job| job.session_id.as_deref() == Some(member.session_id.as_str()))
    {
        lines.push(format!(
            "Your job `{}` likely caused this restart: reduce its memory or give it a `--memory` budget before resubmitting.",
            job.label
        ));
    }
    lines.push(format!(
        "Check `git status` in {}, then carry on.",
        member.working_dir
    ));
    lines.join("\n")
}

/// When the restart happened, for the app and CLI.
pub fn restart_time_text(restart: &HostRestart) -> String {
    restart
        .booted_at_time()
        .map(clock_text)
        .unwrap_or_else(|| restart.booted_at.clone())
}

#[cfg(test)]
fn set_mtime(path: &Path, at: OffsetDateTime) {
    let time = std::time::SystemTime::UNIX_EPOCH
        + std::time::Duration::from_secs(at.unix_timestamp() as u64);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sm-host-restart-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn boot(id: &str, at: OffsetDateTime) -> BootIdentity {
        BootIdentity {
            id: id.to_owned(),
            booted_at: at,
        }
    }

    #[test]
    fn detect_records_the_first_boot_and_reports_each_new_boot_once() {
        let dir = temp_dir();
        let store = HostRestartStore::new(dir.as_path().join("host_restarts.db"));
        let first = boot("AAAA-1111", datetime!(2026-10-01 08:00 UTC));
        let second = boot("BBBB-2222", datetime!(2026-10-07 18:49 UTC));
        assert_eq!(
            store
                .detect(&first, datetime!(2026-10-01 08:01 UTC))
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .detect(&first, datetime!(2026-10-02 08:01 UTC))
                .unwrap(),
            None
        );
        let restart = store
            .detect(&second, datetime!(2026-10-07 19:10 UTC))
            .unwrap()
            .expect("a new boot is a restart");
        assert_eq!(restart.id, "restart-bbbb2222");
        assert_eq!(restart.previous_boot_id, "AAAA-1111");
        assert_eq!(restart.booted_at, "2026-10-07T18:49:00Z");
        // Until its cohort is committed, a start on the same boot finds it again.
        let again = store
            .detect(&second, datetime!(2026-10-07 19:15 UTC))
            .unwrap()
            .unwrap();
        assert_eq!(again, restart);
        store
            .commit_boot(&second, datetime!(2026-10-07 19:15 UTC))
            .unwrap();
        // A handover onto the same boot finds nothing new.
        assert_eq!(
            store
                .detect(&second, datetime!(2026-10-07 19:20 UTC))
                .unwrap(),
            None
        );
        assert_eq!(store.latest().unwrap().unwrap().0.id, restart.id);
    }

    #[test]
    fn members_are_decided_once_and_a_failed_restore_stays_open() {
        let dir = temp_dir();
        let store = HostRestartStore::new(dir.as_path().join("host_restarts.db"));
        store
            .detect(
                &boot("A", datetime!(2026-10-01 08:00 UTC)),
                OffsetDateTime::now_utc(),
            )
            .unwrap();
        let restart = store
            .detect(
                &boot("B", datetime!(2026-10-07 18:49 UTC)),
                OffsetDateTime::now_utc(),
            )
            .unwrap()
            .unwrap();
        let member = |id: &str| {
            CohortMember::new(
                id,
                id,
                "claude",
                "running",
                None,
                "/w",
                vec!["ticket a/b#1".into()],
            )
        };
        store
            .add_members(&restart.id, &[member("s1"), member("s2")])
            .unwrap();
        let now = OffsetDateTime::now_utc();
        assert!(store
            .decide(&restart.id, "s1", DECISION_FAILED, Some("boom"), now)
            .unwrap());
        assert!(store.open_membership("s1").unwrap().is_some());
        assert!(store
            .decide(&restart.id, "s1", DECISION_RESTORED, None, now)
            .unwrap());
        assert!(!store
            .decide(&restart.id, "s1", DECISION_LEFT, None, now)
            .unwrap());
        assert!(store.open_membership("s1").unwrap().is_none());
        let (_, members) = store.get(&restart.id).unwrap().unwrap();
        assert_eq!(members[0].decision, DECISION_RESTORED);
        assert_eq!(members[0].claims, vec!["ticket a/b#1".to_owned()]);
        assert!(members[1].is_open());
        // A later restart with nobody waiting does not hide this one.
        store
            .commit_boot(&boot("B", datetime!(2026-10-07 18:49 UTC)), now)
            .unwrap();
        let later = store
            .detect(&boot("C", datetime!(2026-10-09 08:00 UTC)), now)
            .unwrap()
            .unwrap();
        assert_eq!(store.latest().unwrap().unwrap().0.id, restart.id);
        store
            .decide(&restart.id, "s2", DECISION_LEFT, None, now)
            .unwrap();
        assert_eq!(store.latest().unwrap().unwrap().0.id, later.id);
    }

    const PANIC: &str = concat!(
        r#"{"bug_type":"210","timestamp":"2026-10-07 12:09:51.00 -0700"}"#,
        "\n",
        r#"{"panicString":"panic(cpu 0 caller 0xfffffe0038b64a34): userspace watchdog timeout: no successful checkins from WindowServer\nservice: logd"}"#
    );
    const JETSAM: &str = concat!(
        r#"{"bug_type":"298"}"#,
        "\n",
        r#"{"memoryStatus":{"pageSize":16384},"processes":[{"name":"fseventsd","pid":327,"rpages":2277887},{"name":"python3.12","pid":8728,"rpages":9253562},{"name":"tiny","pid":1,"rpages":3}]}"#
    );

    #[test]
    fn reports_give_the_panic_reason_and_largest_processes() {
        assert_eq!(
            panic_reason(PANIC).as_deref(),
            Some("userspace watchdog timeout: no successful checkins from WindowServer")
        );
        let top = jetsam_top(JETSAM, 2);
        assert_eq!(top[0].name, "python3.12");
        assert_eq!(top[0].bytes, 9_253_562 * 16_384);
        assert_eq!(top[1].pid, 327);
        assert!(jetsam_top("not a report", 3).is_empty());
    }

    fn restart_fixture() -> HostRestart {
        HostRestart {
            id: "restart-bbbb".into(),
            boot_id: "B".into(),
            booted_at: "2026-10-07T18:49:48Z".into(),
            previous_boot_id: "A".into(),
            previous_booted_at: "2026-09-12T00:00:00Z".into(),
            detected_at: "2026-10-07T19:10:10Z".into(),
            cause: RestartCause::default(),
            cause_scanned: false,
        }
    }

    fn queue_db(dir: &Path) -> PathBuf {
        let path = dir.join("queue_runner.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE queue_jobs (id TEXT, label TEXT, requester_session_id TEXT,
                notify_session_id TEXT, state TEXT, started_at TEXT, finished_at TEXT);
            INSERT INTO queue_jobs VALUES
                ('job_big', '1978-summaries', 'far1978', 'far1978', 'host_restart',
                 '2026-10-07T18:00:00Z', '2026-10-07T19:10:11Z'),
                ('job_small', 'tests', NULL, 'sm2020', 'host_restart',
                 '2026-10-07T18:40:00Z', '2026-10-07T19:10:11Z'),
                ('job_old', 'old', 'x', 'x', 'host_restart',
                 '2026-09-01T00:00:00Z', '2026-09-01T01:00:00Z');
            "#,
        )
        .unwrap();
        path
    }

    fn utilization_db(dir: &Path) -> PathBuf {
        let path = dir.join("utilization.db");
        let conn = Connection::open(&path).unwrap();
        let last = datetime!(2026-10-07 18:48:33 UTC).unix_timestamp() * 1000;
        conn.execute_batch(
            "CREATE TABLE host_samples (sampled_at_ms INTEGER, mem_total_bytes INTEGER);
             CREATE TABLE job_samples (sampled_at_ms INTEGER, job_id TEXT, state TEXT,
                 rss_bytes INTEGER, footprint_bytes INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO host_samples VALUES (?1, ?2)",
            params![last, 256_i64 << 30],
        )
        .unwrap();
        for (at, job, rss, footprint) in [
            (
                last - 10 * 60_000,
                "job_big",
                Some(5_i64 << 30),
                Some(90_i64 << 30),
            ),
            (last, "job_big", None, None),
            (last - 60_000, "job_small", Some(2_i64 << 30), None),
            // Outside the half-hour window.
            (last - 3_600_000, "job_small", Some(200_i64 << 30), None),
        ] {
            conn.execute(
                "INSERT INTO job_samples VALUES (?1, ?2, 'running', ?3, ?4)",
                params![at, job, rss, footprint],
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn scan_names_the_job_that_held_the_most_memory_before_the_restart() {
        let dir = temp_dir();
        let reports = dir.as_path().join("reports");
        fs::create_dir(&reports).unwrap();
        let panic_path = reports.join("panic-full-2026-10-07-120951.0002.panic");
        fs::write(&panic_path, PANIC).unwrap();
        set_mtime(&panic_path, datetime!(2026-10-07 19:09:51 UTC));
        let jetsam = reports.join("JetsamEvent-2026-10-07-113911.ips");
        fs::write(&jetsam, JETSAM).unwrap();
        set_mtime(&jetsam, datetime!(2026-10-07 18:39:11 UTC));
        // Before the previous boot: not this restart's.
        let stale = reports.join("old.panic");
        fs::write(&stale, PANIC.replace("WindowServer", "stale")).unwrap();
        set_mtime(&stale, datetime!(2026-09-01 00:00 UTC));
        let queue = queue_db(dir.as_path());
        let utilization = utilization_db(dir.as_path());
        let restart = restart_fixture();
        let name = |id: &str| (id == "far1978").then(|| "far-1978".to_owned());
        let cause = scan_cause(
            &restart,
            &CauseSources {
                reports_dir: &reports,
                utilization_db: Some(&utilization),
                queue_db: Some(&queue),
                session_name: &name,
            },
        );
        assert_eq!(
            cause.panic_reason.as_deref(),
            Some("userspace watchdog timeout: no successful checkins from WindowServer")
        );
        assert_eq!(cause.jetsam_top[0].pid, 8728);
        let job = cause.likely_job.clone().unwrap();
        assert_eq!(job.job_id, "job_big");
        assert_eq!(job.peak_bytes, 90 << 30);
        assert_eq!(job.session_name.as_deref(), Some("far-1978"));
        assert!(cause.summary().contains(
            "Likely cause: queue job `1978-summaries` (job_big, from far-1978), 90.0 GiB"
        ));
        let killed = killed_jobs(&queue, &restart).unwrap();
        assert_eq!(
            killed
                .iter()
                .map(|job| job.job_id.as_str())
                .collect::<Vec<_>>(),
            vec!["job_big", "job_small"]
        );
        assert_eq!(killed[1].session_id.as_deref(), Some("sm2020"));
    }

    #[test]
    fn a_restart_without_reports_has_an_unknown_cause() {
        let dir = temp_dir();
        let name = |_: &str| None;
        let cause = scan_cause(
            &restart_fixture(),
            &CauseSources {
                reports_dir: dir.as_path(),
                utilization_db: None,
                queue_db: None,
                session_name: &name,
            },
        );
        assert!(cause.is_unknown());
        assert_eq!(
            cause.summary(),
            "Cause unknown: macOS left no panic or memory-pressure report."
        );
    }

    #[test]
    fn the_notice_says_what_the_agent_lost_and_blames_its_own_job() {
        let mut restart = restart_fixture();
        restart.cause.likely_job = Some(LikelyJob {
            job_id: "job_big".into(),
            label: "1978-summaries".into(),
            session_id: Some("far1978".into()),
            session_name: Some("far-1978".into()),
            peak_bytes: 141 << 30,
        });
        let killed = vec![
            KilledJob {
                job_id: "job_big".into(),
                label: "1978-summaries".into(),
                session_id: Some("far1978".into()),
            },
            KilledJob {
                job_id: "job_small".into(),
                label: "tests".into(),
                session_id: Some("sm2020".into()),
            },
        ];
        let culprit = CohortMember::new(
            "far1978",
            "far-1978",
            "claude",
            "running",
            None,
            "/w/far",
            vec![],
        );
        let text = notice_text(&restart, &culprit, &killed);
        assert!(text.contains("mid-turn"), "{text}");
        assert!(
            text.contains("killed your queue jobs `1978-summaries` (job_big)"),
            "{text}"
        );
        assert!(!text.contains("job_small"), "{text}");
        assert!(
            text.contains("Your job `1978-summaries` likely caused this restart"),
            "{text}"
        );
        assert!(text.contains("outside sm queue"), "{text}");
        let bystander =
            CohortMember::new("idle1", "idle", "codex", "idle", None, "/w/idle", vec![]);
        let text = notice_text(&restart, &bystander, &killed);
        assert!(text.contains("You were idle"), "{text}");
        assert!(
            text.contains("None of your queue jobs were running."),
            "{text}"
        );
        assert!(!text.contains("Your job"), "{text}");
        assert!(
            text.contains("Likely cause: queue job `1978-summaries`"),
            "{text}"
        );
    }
}
