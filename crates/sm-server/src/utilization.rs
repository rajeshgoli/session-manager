//! Utilization recorder (sm#1609).
//!
//! Every few seconds, while the live server runs the queue, record the host's
//! CPU, memory and GPU use plus every pending or running queue job into
//! `utilization.db`. The file is separate from `queue_runner.db` so frequent
//! writes never compete with admission. The Queue page's "Held back?" card and
//! the Mac usage charts read it back through [`queue_stats`] and [`series`].
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::RwLock,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

use crate::queue::{active_queue_jobs_for_sampling, ActiveQueueJob};

/// Job types in admission order; the order every per-type output uses.
const JOB_TYPES: [&str; 4] = ["perf", "tests", "background", "service"];
const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const WRITE_WARNING_INTERVAL: Duration = Duration::from_secs(60);

/// Headroom: the machine could have taken more work (memo section 4).
const HEADROOM_CPU_BELOW_PCT: f64 = 60.0;
const HEADROOM_AVAILABLE_FRACTION: f64 = 0.25;
/// CPU above this counts as busy in the Mac usage summary.
const CPU_BUSY_ABOVE_PCT: f64 = 85.0;

#[derive(Debug, Clone)]
pub struct RecorderSettings {
    pub db_path: PathBuf,
    pub queue_db_path: PathBuf,
    pub interval: Duration,
    pub retention_days: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HostSample {
    pub sampled_at_ms: i64,
    pub interval_ms: i64,
    pub cpu_busy_pct: Option<f64>,
    pub gpu_busy_pct: Option<f64>,
    pub mem_total_bytes: Option<i64>,
    pub mem_used_bytes: Option<i64>,
    pub mem_available_bytes: Option<i64>,
    pub mem_free_bytes: Option<i64>,
    pub mem_speculative_bytes: Option<i64>,
    pub mem_active_bytes: Option<i64>,
    pub mem_inactive_bytes: Option<i64>,
    pub mem_wired_bytes: Option<i64>,
    pub mem_compressed_bytes: Option<i64>,
    pub pressure_level: Option<i64>,
    pub load_1m: Option<f64>,
    /// Running and pending counts, in [`JOB_TYPES`] order.
    pub running: [i64; 4],
    pub pending: [i64; 4],
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobSample {
    pub job_id: String,
    pub job_type: String,
    pub state: String,
    pub holding_reason: Option<String>,
    pub rss_bytes: Option<i64>,
    pub cpu_seconds_total: Option<f64>,
    pub process_count: Option<i64>,
}

static LATEST: RwLock<Option<HostSample>> = RwLock::new(None);

/// The newest sample the recorder wrote, if it is running.
pub fn latest_host_sample() -> Option<HostSample> {
    LATEST.read().ok().and_then(|latest| latest.clone())
}

pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS host_samples (
            sampled_at_ms INTEGER PRIMARY KEY,
            interval_ms INTEGER NOT NULL,
            cpu_busy_pct REAL,
            gpu_busy_pct REAL,
            mem_total_bytes INTEGER,
            mem_used_bytes INTEGER,
            mem_available_bytes INTEGER,
            mem_free_bytes INTEGER,
            mem_speculative_bytes INTEGER,
            mem_active_bytes INTEGER,
            mem_inactive_bytes INTEGER,
            mem_wired_bytes INTEGER,
            mem_compressed_bytes INTEGER,
            pressure_level INTEGER,
            load_1m REAL,
            running_tests INTEGER NOT NULL,
            running_perf INTEGER NOT NULL,
            running_background INTEGER NOT NULL,
            running_service INTEGER NOT NULL,
            pending_tests INTEGER NOT NULL,
            pending_perf INTEGER NOT NULL,
            pending_background INTEGER NOT NULL,
            pending_service INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS job_samples (
            sampled_at_ms INTEGER NOT NULL,
            job_id TEXT NOT NULL,
            job_type TEXT NOT NULL,
            state TEXT NOT NULL CHECK (state IN ('pending', 'running')),
            holding_reason TEXT,
            rss_bytes INTEGER,
            cpu_seconds_total REAL,
            process_count INTEGER,
            PRIMARY KEY (sampled_at_ms, job_id)
        );
        CREATE INDEX IF NOT EXISTS job_samples_by_job ON job_samples (job_id, sampled_at_ms);
        "#,
    )?;
    Ok(())
}

pub fn open_for_write(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open utilization db {}", path.display()))?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    init_schema(&conn)?;
    Ok(conn)
}

/// `None` when the recorder has never created the database.
fn open_for_read(path: &Path) -> Result<Option<Connection>> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open utilization db {}", path.display()))?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    let has_table: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'host_samples'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(has_table.map(|_| conn))
}

pub fn write_sample(conn: &mut Connection, host: &HostSample, jobs: &[JobSample]) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        r#"
        INSERT OR REPLACE INTO host_samples (
            sampled_at_ms, interval_ms, cpu_busy_pct, gpu_busy_pct,
            mem_total_bytes, mem_used_bytes, mem_available_bytes, mem_free_bytes,
            mem_speculative_bytes, mem_active_bytes, mem_inactive_bytes, mem_wired_bytes,
            mem_compressed_bytes, pressure_level, load_1m,
            running_perf, running_tests, running_background, running_service,
            pending_perf, pending_tests, pending_background, pending_service
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                  ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)
        "#,
        params![
            host.sampled_at_ms,
            host.interval_ms,
            host.cpu_busy_pct,
            host.gpu_busy_pct,
            host.mem_total_bytes,
            host.mem_used_bytes,
            host.mem_available_bytes,
            host.mem_free_bytes,
            host.mem_speculative_bytes,
            host.mem_active_bytes,
            host.mem_inactive_bytes,
            host.mem_wired_bytes,
            host.mem_compressed_bytes,
            host.pressure_level,
            host.load_1m,
            host.running[0],
            host.running[1],
            host.running[2],
            host.running[3],
            host.pending[0],
            host.pending[1],
            host.pending[2],
            host.pending[3],
        ],
    )?;
    {
        let mut insert = tx.prepare(
            r#"
            INSERT OR REPLACE INTO job_samples (
                sampled_at_ms, job_id, job_type, state, holding_reason,
                rss_bytes, cpu_seconds_total, process_count
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
        )?;
        for job in jobs
            .iter()
            .filter(|job| matches!(job.state.as_str(), "pending" | "running"))
        {
            insert.execute(params![
                host.sampled_at_ms,
                job.job_id,
                job.job_type,
                job.state,
                job.holding_reason,
                job.rss_bytes,
                job.cpu_seconds_total,
                job.process_count,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn prune(conn: &Connection, cutoff_ms: i64) -> Result<usize> {
    let hosts = conn.execute(
        "DELETE FROM host_samples WHERE sampled_at_ms < ?1",
        [cutoff_ms],
    )?;
    let jobs = conn.execute(
        "DELETE FROM job_samples WHERE sampled_at_ms < ?1",
        [cutoff_ms],
    )?;
    Ok(hosts + jobs)
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// Cumulative CPU ticks summed over cores: user, system, idle, nice. The
/// kernel keeps them in 32 bits, so they wrap; idle wraps within weeks.
pub type CpuTicks = [u32; 4];

/// Busy share between two tick snapshots; `None` when no time passed.
pub fn cpu_busy_pct(previous: CpuTicks, current: CpuTicks) -> Option<f64> {
    // A counter that stepped backwards would read as a near-2^32 advance.
    let delta = |index: usize| {
        let delta = current[index].wrapping_sub(previous[index]);
        (delta < 1 << 31).then_some(u64::from(delta))
    };
    let (user, system, idle, nice) = (delta(0)?, delta(1)?, delta(2)?, delta(3)?);
    let busy = user + system + nice;
    let total = busy + idle;
    (total > 0).then(|| (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VmPages {
    pub page_size: i64,
    pub free: i64,
    pub speculative: i64,
    pub active: i64,
    pub inactive: i64,
    pub wired: i64,
    pub compressed: i64,
}

/// Fill the memory fields: `used` counts as `top` does (total − free −
/// speculative), `available` is the kernel's own free percentage.
pub fn apply_memory(
    sample: &mut HostSample,
    total: Option<i64>,
    pages: Option<VmPages>,
    memorystatus_level: Option<i64>,
) {
    sample.mem_total_bytes = total;
    if let Some(pages) = pages {
        let bytes = |count: i64| count.checked_mul(pages.page_size);
        sample.mem_free_bytes = bytes(pages.free);
        sample.mem_speculative_bytes = bytes(pages.speculative);
        sample.mem_active_bytes = bytes(pages.active);
        sample.mem_inactive_bytes = bytes(pages.inactive);
        sample.mem_wired_bytes = bytes(pages.wired);
        sample.mem_compressed_bytes = bytes(pages.compressed);
        sample.mem_used_bytes = total.and_then(|total| {
            let unused = sample
                .mem_free_bytes?
                .checked_add(sample.mem_speculative_bytes?)?;
            Some(total.saturating_sub(unused).max(0))
        });
    }
    sample.mem_available_bytes = total
        .zip(memorystatus_level.filter(|level| (0..=100).contains(level)))
        .map(|(total, level)| total / 100 * level);
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GroupUsage {
    pub rss_bytes: i64,
    pub cpu_seconds: f64,
    pub processes: i64,
}

/// Sum `ps -axo pgid=,rss=,time=` output by process group.
pub fn parse_process_groups(text: &str) -> HashMap<i64, GroupUsage> {
    let mut groups: HashMap<i64, GroupUsage> = HashMap::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pgid), Some(rss), Some(time)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(pgid), Ok(rss_kib)) = (pgid.parse::<i64>(), rss.parse::<i64>()) else {
            continue;
        };
        let entry = groups.entry(pgid).or_default();
        entry.rss_bytes = entry.rss_bytes.saturating_add(rss_kib.saturating_mul(1024));
        entry.cpu_seconds += parse_cpu_time(time).unwrap_or(0.0);
        entry.processes += 1;
    }
    groups
}

/// `ps` CPU time: `[D-][[H:]M:]S[.frac]`; macOS prints minutes past 60.
pub fn parse_cpu_time(text: &str) -> Option<f64> {
    let (days, rest) = match text.split_once('-') {
        Some((days, rest)) => (days.parse::<f64>().ok()?, rest),
        None => (0.0, text),
    };
    let mut seconds = 0.0;
    for part in rest.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86_400.0 + seconds)
}

pub fn job_samples(jobs: &[ActiveQueueJob], groups: &HashMap<i64, GroupUsage>) -> Vec<JobSample> {
    jobs.iter()
        .map(|job| {
            let usage = (job.state == "running")
                .then(|| job.process_group_id.and_then(|pgid| groups.get(&pgid)))
                .flatten();
            JobSample {
                job_id: job.id.clone(),
                job_type: job.job_type.clone(),
                state: job.state.clone(),
                holding_reason: (job.state == "pending")
                    .then(|| job.holding_reason.clone())
                    .flatten(),
                rss_bytes: usage.map(|usage| usage.rss_bytes),
                cpu_seconds_total: usage.map(|usage| usage.cpu_seconds),
                process_count: usage.map(|usage| usage.processes),
            }
        })
        .collect()
}

fn type_index(job_type: &str) -> Option<usize> {
    JOB_TYPES.iter().position(|known| *known == job_type)
}

fn count_jobs(sample: &mut HostSample, jobs: &[ActiveQueueJob]) {
    for job in jobs {
        let Some(index) = type_index(&job.job_type) else {
            continue;
        };
        match job.state.as_str() {
            "running" => sample.running[index] += 1,
            "pending" => sample.pending[index] += 1,
            _ => {}
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::{CpuTicks, VmPages};
    use std::ffi::CString;
    use std::sync::OnceLock;

    fn host_port() -> libc::mach_port_t {
        static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
        // mach_host_self returns a new send right on every call; ask once.
        #[allow(deprecated)] // libc points at the mach2 crate; the symbol is stable.
        *HOST.get_or_init(|| unsafe { libc::mach_host_self() })
    }

    pub fn cpu_ticks() -> Option<CpuTicks> {
        let mut info = std::mem::MaybeUninit::<libc::host_cpu_load_info>::zeroed();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        let status = unsafe {
            libc::host_statistics(
                host_port(),
                libc::HOST_CPU_LOAD_INFO,
                info.as_mut_ptr().cast(),
                &mut count,
            )
        };
        if status != libc::KERN_SUCCESS {
            return None;
        }
        let ticks = unsafe { info.assume_init() }.cpu_ticks;
        Some([
            ticks[libc::CPU_STATE_USER as usize],
            ticks[libc::CPU_STATE_SYSTEM as usize],
            ticks[libc::CPU_STATE_IDLE as usize],
            ticks[libc::CPU_STATE_NICE as usize],
        ])
    }

    pub fn vm_pages() -> Option<VmPages> {
        let mut info = std::mem::MaybeUninit::<libc::vm_statistics64>::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let status = unsafe {
            libc::host_statistics64(
                host_port(),
                libc::HOST_VM_INFO64,
                info.as_mut_ptr().cast(),
                &mut count,
            )
        };
        if status != libc::KERN_SUCCESS {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some(VmPages {
            page_size: sysctl_i64("hw.pagesize")?,
            free: i64::from(info.free_count),
            speculative: i64::from(info.speculative_count),
            active: i64::from(info.active_count),
            inactive: i64::from(info.inactive_count),
            wired: i64::from(info.wire_count),
            compressed: i64::from(info.compressor_page_count),
        })
    }

    /// Integer sysctl of 4 or 8 bytes.
    pub fn sysctl_i64(name: &str) -> Option<i64> {
        let name = CString::new(name).ok()?;
        let mut buffer = [0u8; 8];
        let mut size = buffer.len();
        let status = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        match (status, size) {
            (0, 4) => Some(i64::from(i32::from_ne_bytes(buffer[..4].try_into().ok()?))),
            (0, 8) => Some(i64::from_ne_bytes(buffer)),
            _ => None,
        }
    }

    pub fn load_1m() -> Option<f64> {
        let mut loads = [0f64; 3];
        (unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) } >= 1).then_some(loads[0])
    }
}

/// Longest a sampling command may run; a hung `ioreg` or `ps` must not stall
/// every later sample.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(4);

fn run(program: &str, args: &[&str]) -> Option<String> {
    run_with_timeout(program, args, COMMAND_TIMEOUT)
}

/// Stdout of a successful run; `None` on failure or after killing it at
/// `timeout`.
fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    use std::io::Read;
    let mut child = std::process::Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Drain concurrently so a large listing cannot fill the pipe and stall.
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).ok().map(|_| text)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let text = reader.join().ok()??;
    status.success().then_some(text)
}

struct Recorder {
    settings: RecorderSettings,
    conn: Option<Connection>,
    previous_ticks: Option<CpuTicks>,
    last_prune: Option<Instant>,
    last_write_warning: Option<Instant>,
}

impl Recorder {
    #[cfg(target_os = "macos")]
    fn measure_host(&mut self, now_ms: i64) -> HostSample {
        let mut sample = HostSample {
            sampled_at_ms: now_ms,
            interval_ms: self.settings.interval.as_millis() as i64,
            ..HostSample::default()
        };
        let ticks = mac::cpu_ticks();
        sample.cpu_busy_pct = self
            .previous_ticks
            .zip(ticks)
            .and_then(|(a, b)| cpu_busy_pct(a, b));
        self.previous_ticks = ticks;
        apply_memory(
            &mut sample,
            mac::sysctl_i64("hw.memsize"),
            mac::vm_pages(),
            mac::sysctl_i64("kern.memorystatus_level"),
        );
        sample.pressure_level = mac::sysctl_i64("kern.memorystatus_vm_pressure_level");
        sample.load_1m = mac::load_1m();
        sample.gpu_busy_pct = run(
            "/usr/sbin/ioreg",
            &["-r", "-c", "AGXAccelerator", "-d", "1"],
        )
        .as_deref()
        .and_then(crate::host_status::gpu_percent);
        sample
    }

    #[cfg(not(target_os = "macos"))]
    fn measure_host(&mut self, now_ms: i64) -> HostSample {
        HostSample {
            sampled_at_ms: now_ms,
            interval_ms: self.settings.interval.as_millis() as i64,
            ..HostSample::default()
        }
    }

    fn tick(&mut self) -> Result<()> {
        let now_ms = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        let mut host = self.measure_host(now_ms);
        let jobs = active_queue_jobs_for_sampling(&self.settings.queue_db_path)?;
        count_jobs(&mut host, &jobs);
        let groups = if jobs.iter().any(|job| job.state == "running") {
            run("/bin/ps", &["-axo", "pgid=,rss=,time="])
                .map(|text| parse_process_groups(&text))
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        let job_rows = job_samples(&jobs, &groups);
        if let Ok(mut latest) = LATEST.write() {
            *latest = Some(host.clone());
        }
        if self.conn.is_none() {
            self.conn = Some(open_for_write(&self.settings.db_path)?);
        }
        let conn = self.conn.as_mut().expect("utilization db opened above");
        if let Err(error) = write_sample(conn, &host, &job_rows) {
            self.conn = None;
            return Err(error);
        }
        if self
            .last_prune
            .is_none_or(|at| at.elapsed() >= PRUNE_INTERVAL)
        {
            self.last_prune = Some(Instant::now());
            let cutoff = now_ms - self.settings.retention_days * 86_400_000;
            prune(conn, cutoff)?;
        }
        Ok(())
    }

    fn warn(&mut self, error: &anyhow::Error) {
        if self
            .last_write_warning
            .is_none_or(|at| at.elapsed() >= WRITE_WARNING_INTERVAL)
        {
            self.last_write_warning = Some(Instant::now());
            eprintln!("utilization recorder sample failed: {error:#}");
        }
    }
}

/// Start the recorder task. Callers decide whether it should run at all.
pub fn spawn_recorder(settings: RecorderSettings) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(settings.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let recorder = std::sync::Arc::new(std::sync::Mutex::new(Recorder {
            settings,
            conn: None,
            previous_ticks: None,
            last_prune: None,
            last_write_warning: None,
        }));
        loop {
            ticker.tick().await;
            let recorder = recorder.clone();
            let result = tokio::task::spawn_blocking(move || {
                let mut recorder = recorder
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Err(error) = recorder.tick() {
                    recorder.warn(&error);
                }
            })
            .await;
            if let Err(error) = result {
                eprintln!("utilization recorder task failed: {error}");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Reading: the "Held back?" card and the Mac usage charts
// ---------------------------------------------------------------------------

/// Host samples in `[start, end)` with their weight in milliseconds: the
/// configured interval, cut short by the next sample so outages add no time.
const WEIGHTED_HOSTS: &str = r#"
    WITH h AS (
        SELECT *,
               MIN(interval_ms, COALESCE(
                   LEAD(sampled_at_ms) OVER (ORDER BY sampled_at_ms) - sampled_at_ms,
                   interval_ms)) AS weight_ms,
               CASE
                   WHEN cpu_busy_pct IS NULL OR mem_available_bytes IS NULL
                        OR mem_total_bytes IS NULL OR pressure_level IS NULL THEN NULL
                   WHEN cpu_busy_pct < ?3 AND mem_available_bytes >= ?4 * mem_total_bytes
                        AND pressure_level = 1 THEN 1
                   ELSE 0
               END AS headroom
        FROM host_samples
        WHERE sampled_at_ms >= ?1 AND sampled_at_ms < ?2
    )
"#;

fn hold_group(reason: Option<&str>) -> &'static str {
    match reason {
        Some("concurrency_cap") => "limits",
        Some("perf_running" | "awaiting_tests" | "perf_cooldown" | "displacing") => "perf_rules",
        Some("memory_pressure" | "resource_budget_missing") => "memory",
        _ => "other",
    }
}

/// Nearest-rank percentile of an unsorted slice.
pub fn percentile(values: &mut [f64], pct: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let rank = ((pct / 100.0) * values.len() as f64).ceil().max(1.0) as usize;
    Some(values[rank.min(values.len()) - 1])
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

fn rfc3339_ms(ms: i64) -> Value {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|at| {
            at.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .map_or(Value::Null, Value::String)
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// `GET /client/queue/stats` (Appendix D).
pub fn queue_stats(db_path: &Path, hours: i64) -> Result<Value> {
    queue_stats_at(db_path, hours, now_ms())
}

pub fn queue_stats_at(db_path: &Path, hours: i64, now_ms: i64) -> Result<Value> {
    let Some(conn) = open_for_read(db_path)? else {
        return Ok(json!({"available": false}));
    };
    let start = now_ms - hours * 3_600_000;
    let args = params![
        start,
        now_ms,
        HEADROOM_CPU_BELOW_PCT,
        HEADROOM_AVAILABLE_FRACTION
    ];
    let covered_ms: Option<i64> = conn.query_row(
        &format!("{WEIGHTED_HOSTS} SELECT SUM(weight_ms) FROM h"),
        args,
        |row| row.get(0),
    )?;
    let Some(covered_ms) = covered_ms else {
        return Ok(json!({"available": false}));
    };

    let mut groups: Vec<(&str, [i64; 3])> = ["limits", "perf_rules", "memory", "other"]
        .into_iter()
        .map(|group| (group, [0; 3]))
        .collect();
    {
        let mut statement = conn.prepare(&format!(
            r#"{WEIGHTED_HOSTS}
            SELECT j.holding_reason, SUM(h.weight_ms),
                   SUM(CASE WHEN h.headroom = 1 THEN h.weight_ms ELSE 0 END),
                   SUM(CASE WHEN h.headroom IS NULL THEN h.weight_ms ELSE 0 END)
            FROM job_samples j JOIN h ON h.sampled_at_ms = j.sampled_at_ms
            WHERE j.state = 'pending'
            GROUP BY j.holding_reason"#
        ))?;
        let rows = statement.query_map(args, |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                [row.get::<_, i64>(1)?, row.get(2)?, row.get(3)?],
            ))
        })?;
        for row in rows {
            let (reason, sums) = row?;
            let group = hold_group(reason.as_deref());
            let entry = groups
                .iter_mut()
                .find(|(name, _)| *name == group)
                .expect("every hold group is listed");
            for (total, add) in entry.1.iter_mut().zip(sums) {
                *total += add;
            }
        }
    }
    let waiting: Vec<Value> = groups
        .iter()
        .map(|(group, [job, headroom, unknown])| {
            json!({
                "group": group,
                "job_seconds": job / 1000,
                "headroom_job_seconds": headroom / 1000,
                "unknown_job_seconds": unknown / 1000,
            })
        })
        .collect();

    // Peak memory per job and CPU cores per sampling interval, by type.
    let mut peaks: HashMap<String, (Vec<f64>, Vec<f64>)> = HashMap::new();
    {
        let mut statement = conn.prepare(
            r#"SELECT job_type, MAX(rss_bytes) FROM job_samples
               WHERE state = 'running' AND sampled_at_ms >= ?1 AND sampled_at_ms < ?2
               GROUP BY job_id, job_type"#,
        )?;
        let rows = statement.query_map(params![start, now_ms], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        for row in rows {
            let (job_type, rss) = row?;
            let entry = peaks.entry(job_type).or_default();
            if let Some(rss) = rss {
                entry.0.push(rss as f64);
            }
        }
        let mut statement = conn.prepare(
            r#"SELECT job_type,
                      (cpu_seconds_total - LAG(cpu_seconds_total) OVER w)
                        / ((sampled_at_ms - LAG(sampled_at_ms) OVER w) / 1000.0)
               FROM job_samples
               WHERE state = 'running' AND sampled_at_ms >= ?1 AND sampled_at_ms < ?2
               WINDOW w AS (PARTITION BY job_id ORDER BY sampled_at_ms)"#,
        )?;
        let rows = statement.query_map(params![start, now_ms], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?))
        })?;
        for row in rows {
            let (job_type, cores) = row?;
            if let Some(cores) = cores.filter(|cores| cores.is_finite() && *cores >= 0.0) {
                peaks.entry(job_type).or_default().1.push(cores);
            }
        }
    }
    let mut job_counts: HashMap<String, i64> = HashMap::new();
    {
        let mut statement = conn.prepare(
            r#"SELECT job_type, COUNT(DISTINCT job_id) FROM job_samples
               WHERE state = 'running' AND sampled_at_ms >= ?1 AND sampled_at_ms < ?2
               GROUP BY job_type"#,
        )?;
        let rows = statement.query_map(params![start, now_ms], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (job_type, count) = row?;
            job_counts.insert(job_type, count);
        }
    }
    let by_type: Vec<Value> = JOB_TYPES
        .iter()
        .filter_map(|job_type| {
            let jobs = *job_counts.get(*job_type)?;
            let (mut rss, mut cores) = peaks.remove(*job_type).unwrap_or_default();
            let as_bytes = |value: Option<f64>| value.map(|value| value as i64);
            Some(json!({
                "type": job_type,
                "jobs": jobs,
                "peak_rss_p50_bytes": as_bytes(percentile(&mut rss, 50.0)),
                "peak_rss_p95_bytes": as_bytes(percentile(&mut rss, 95.0)),
                "peak_rss_max_bytes": as_bytes(percentile(&mut rss, 100.0)),
                "cpu_cores_p95": percentile(&mut cores, 95.0).map(round1),
            }))
        })
        .collect();

    Ok(json!({
        "available": true,
        "hours": hours,
        "window_seconds": hours * 3600,
        "covered_seconds": covered_ms / 1000,
        "thresholds": {
            "cpu_busy_below_pct": HEADROOM_CPU_BELOW_PCT,
            "available_memory_at_least_fraction": HEADROOM_AVAILABLE_FRACTION,
            "pressure": "normal",
        },
        "waiting": waiting,
        "by_type": by_type,
    }))
}

/// Bucket width for each Mac usage range (Appendix D2).
pub fn series_bucket_seconds(hours: i64) -> Option<(i64, i64)> {
    match hours {
        1 => Some((15, 240)),
        24 => Some((300, 288)),
        168 => Some((1800, 336)),
        720 => Some((7200, 360)),
        _ => None,
    }
}

/// `GET /client/utilization/series` (Appendix D2).
pub fn series(db_path: &Path, hours: i64) -> Result<Value> {
    series_at(db_path, hours, now_ms())
}

pub fn series_at(db_path: &Path, hours: i64, now_ms: i64) -> Result<Value> {
    let (bucket_seconds, bucket_count) =
        series_bucket_seconds(hours).context("unsupported utilization range")?;
    let Some(conn) = open_for_read(db_path)? else {
        return Ok(json!({"available": false}));
    };
    let bucket_ms = bucket_seconds * 1000;
    let end = now_ms.div_euclid(bucket_ms) * bucket_ms + bucket_ms;
    let start = end - bucket_count * bucket_ms;

    let mut buckets: Vec<Value> = (0..bucket_count)
        .map(|index| json!({"start": rfc3339_ms(start + index * bucket_ms), "samples": 0}))
        .collect();
    let mut any = false;
    {
        let mut statement = conn.prepare(
            r#"SELECT (sampled_at_ms - ?1) / ?3 AS bucket, COUNT(*),
                      AVG(cpu_busy_pct), MAX(cpu_busy_pct), AVG(gpu_busy_pct), MAX(gpu_busy_pct),
                      AVG(mem_used_bytes), MAX(mem_used_bytes), MIN(mem_available_bytes),
                      MAX(pressure_level),
                      AVG(running_tests), AVG(running_perf), AVG(running_background),
                      AVG(running_service),
                      MAX(pending_tests + pending_perf + pending_background + pending_service)
               FROM host_samples
               WHERE sampled_at_ms >= ?1 AND sampled_at_ms < ?2
               GROUP BY bucket"#,
        )?;
        let mut rows = statement.query(params![start, end, bucket_ms])?;
        while let Some(row) = rows.next()? {
            let index: i64 = row.get(0)?;
            let Some(bucket) = usize::try_from(index)
                .ok()
                .and_then(|index| buckets.get_mut(index))
            else {
                continue;
            };
            any = true;
            let object = bucket.as_object_mut().expect("bucket is an object");
            object.insert("samples".into(), json!(row.get::<_, i64>(1)?));
            let mut put_f = |key: &str, value: Option<f64>| {
                if let Some(value) = value {
                    object.insert(key.into(), json!(round1(value)));
                }
            };
            put_f("cpu_avg", row.get(2)?);
            put_f("cpu_max", row.get(3)?);
            put_f("gpu_avg", row.get(4)?);
            put_f("gpu_max", row.get(5)?);
            let mut put_i = |key: &str, value: Option<f64>| {
                if let Some(value) = value {
                    object.insert(key.into(), json!(value.round() as i64));
                }
            };
            put_i("mem_used_avg", row.get(6)?);
            put_i(
                "mem_used_max",
                row.get::<_, Option<i64>>(7)?.map(|v| v as f64),
            );
            put_i(
                "mem_available_min",
                row.get::<_, Option<i64>>(8)?.map(|v| v as f64),
            );
            if let Some(pressure) = row.get::<_, Option<i64>>(9)? {
                object.insert("pressure_max".into(), json!(pressure));
            }
            let running = |value: f64| (value * 100.0).round() / 100.0;
            object.insert(
                "running".into(),
                json!({
                    "tests": running(row.get::<_, f64>(10)?),
                    "perf": running(row.get::<_, f64>(11)?),
                    "background": running(row.get::<_, f64>(12)?),
                    "service": running(row.get::<_, f64>(13)?),
                }),
            );
            object.insert("pending_max".into(), json!(row.get::<_, i64>(14)?));
        }
    }
    if !any {
        return Ok(json!({"available": false}));
    }

    let summary = conn.query_row(
        &format!(
            r#"{WEIGHTED_HOSTS}
            SELECT SUM(weight_ms),
                   SUM(weight_ms * cpu_busy_pct) / SUM(CASE WHEN cpu_busy_pct IS NOT NULL THEN weight_ms END),
                   SUM(CASE WHEN cpu_busy_pct > ?5 THEN weight_ms ELSE 0 END),
                   SUM(weight_ms * gpu_busy_pct) / SUM(CASE WHEN gpu_busy_pct IS NOT NULL THEN weight_ms END),
                   MAX(mem_used_bytes),
                   SUM(CASE WHEN pressure_level >= 2 THEN weight_ms ELSE 0 END),
                   SUM(CASE WHEN headroom = 1 THEN weight_ms ELSE 0 END),
                   SUM(CASE WHEN headroom IS NULL THEN weight_ms ELSE 0 END)
            FROM h"#
        ),
        params![
            start,
            end,
            HEADROOM_CPU_BELOW_PCT,
            HEADROOM_AVAILABLE_FRACTION,
            CPU_BUSY_ABOVE_PCT
        ],
        |row| {
            Ok(json!({
                "covered_seconds": row.get::<_, i64>(0)? / 1000,
                "cpu_avg": row.get::<_, Option<f64>>(1)?.map(round1),
                "cpu_busy_seconds": row.get::<_, i64>(2)? / 1000,
                "gpu_avg": row.get::<_, Option<f64>>(3)?.map(round1),
                "mem_used_max": row.get::<_, Option<i64>>(4)?,
                "pressure_elevated_seconds": row.get::<_, i64>(5)? / 1000,
                "headroom_seconds": row.get::<_, i64>(6)? / 1000,
                "unknown_seconds": row.get::<_, i64>(7)? / 1000,
            }))
        },
    )?;
    let memory_total: Option<i64> = conn
        .query_row(
            r#"SELECT mem_total_bytes FROM host_samples
               WHERE sampled_at_ms >= ?1 AND sampled_at_ms < ?2 AND mem_total_bytes IS NOT NULL
               ORDER BY sampled_at_ms DESC LIMIT 1"#,
            params![start, end],
            |row| row.get(0),
        )
        .optional()?;

    Ok(json!({
        "available": true,
        "hours": hours,
        "bucket_seconds": bucket_seconds,
        "start": rfc3339_ms(start),
        "end": rfc3339_ms(end),
        "memory_total_bytes": memory_total,
        "buckets": buckets,
        "summary": summary,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: i64 = 1024 * 1024 * 1024;

    fn host(at_ms: i64, cpu: Option<f64>, available: i64, pressure: i64) -> HostSample {
        HostSample {
            sampled_at_ms: at_ms,
            interval_ms: 5000,
            cpu_busy_pct: cpu,
            gpu_busy_pct: Some(10.0),
            mem_total_bytes: Some(256 * GIB),
            mem_used_bytes: Some(100 * GIB),
            mem_available_bytes: Some(available),
            pressure_level: Some(pressure),
            ..HostSample::default()
        }
    }

    fn pending(id: &str, job_type: &str, reason: &str) -> JobSample {
        JobSample {
            job_id: id.into(),
            job_type: job_type.into(),
            state: "pending".into(),
            holding_reason: Some(reason.into()),
            rss_bytes: None,
            cpu_seconds_total: None,
            process_count: None,
        }
    }

    /// Removes its directory on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "sm-utilization-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_db() -> (TempDir, PathBuf, Connection) {
        let dir = TempDir::new();
        let path = dir.0.join("utilization.db");
        let conn = open_for_write(&path).unwrap();
        (dir, path, conn)
    }

    #[test]
    fn cpu_busy_is_the_share_of_non_idle_ticks_between_snapshots() {
        assert_eq!(
            cpu_busy_pct([100, 50, 850, 0], [130, 60, 910, 0]),
            Some(40.0)
        );
        assert_eq!(cpu_busy_pct([1, 1, 1, 1], [1, 1, 1, 1]), None);
        // Idle wrapped past u32::MAX between the two readings.
        assert_eq!(
            cpu_busy_pct([10, 0, u32::MAX - 29, 0], [40, 0, 40, 0]),
            Some(30.0)
        );
        assert_eq!(cpu_busy_pct([5, 1, 1, 1], [4, 1, 9, 1]), None);
    }

    #[test]
    fn memory_used_matches_top_and_available_follows_the_kernel_level() {
        // Readings from the studio on 2026-09-28: top said 87G used, level 96.
        let mut sample = HostSample::default();
        let pages = VmPages {
            page_size: 16384,
            free: 10_776_647,
            speculative: 243_532,
            active: 2_157_904,
            inactive: 2_947_511,
            wired: 481_756,
            compressed: 92_928,
        };
        apply_memory(&mut sample, Some(274_877_906_944), Some(pages), Some(96));
        let used_gib = sample.mem_used_bytes.unwrap() as f64 / GIB as f64;
        assert!((87.0..89.0).contains(&used_gib), "{used_gib}");
        assert_eq!(sample.mem_available_bytes, Some(274_877_906_944 / 100 * 96));
        assert_eq!(sample.mem_wired_bytes, Some(481_756 * 16384));
        let mut missing = HostSample::default();
        apply_memory(&mut missing, None, None, Some(120));
        assert_eq!(missing.mem_used_bytes, None);
        assert_eq!(missing.mem_available_bytes, None);
    }

    #[test]
    fn process_groups_sum_rss_cpu_time_and_count() {
        let text = " 42 1024 1:02.50\n 7 9000 0:00.01\n 42 2048 910:10.11\n bad line\n 42 x 0:01\n";
        let groups = parse_process_groups(text);
        let group = groups[&42];
        assert_eq!(group.rss_bytes, 3072 * 1024);
        assert_eq!(group.processes, 2);
        assert!((group.cpu_seconds - (62.5 + 910.0 * 60.0 + 10.11)).abs() < 1e-6);
        assert_eq!(parse_cpu_time("1-02:03:04"), Some(86_400.0 + 7384.0));
        assert_eq!(parse_cpu_time("nope"), None);
    }

    #[test]
    fn job_samples_keep_hold_reasons_for_pending_and_usage_for_running() {
        let jobs = vec![
            ActiveQueueJob {
                id: "a".into(),
                job_type: "tests".into(),
                state: "running".into(),
                holding_reason: None,
                process_group_id: Some(42),
            },
            ActiveQueueJob {
                id: "b".into(),
                job_type: "background".into(),
                state: "pending".into(),
                holding_reason: Some("concurrency_cap".into()),
                process_group_id: Some(42),
            },
            ActiveQueueJob {
                id: "c".into(),
                job_type: "tests".into(),
                state: "running".into(),
                holding_reason: None,
                process_group_id: Some(99),
            },
        ];
        let groups = parse_process_groups(" 42 10 0:01.00\n");
        let rows = job_samples(&jobs, &groups);
        assert_eq!(rows[0].rss_bytes, Some(10 * 1024));
        assert_eq!(rows[0].holding_reason, None);
        assert_eq!(rows[1].holding_reason.as_deref(), Some("concurrency_cap"));
        assert_eq!(rows[1].rss_bytes, None);
        assert_eq!(rows[2].process_count, None);
        let mut sample = HostSample::default();
        count_jobs(&mut sample, &jobs);
        assert_eq!(sample.running, [0, 2, 0, 0]);
        assert_eq!(sample.pending, [0, 0, 1, 0]);
    }

    #[test]
    fn a_sample_writes_one_host_row_and_skips_jobs_that_are_not_active() {
        let (_dir, _path, mut conn) = temp_db();
        let mut done = pending("z", "tests", "x");
        done.state = "succeeded".into();
        write_sample(
            &mut conn,
            &host(1_000, Some(10.0), 200 * GIB, 1),
            &[pending("a", "tests", "concurrency_cap"), done],
        )
        .unwrap();
        let hosts: i64 = conn
            .query_row("SELECT COUNT(*) FROM host_samples", [], |row| row.get(0))
            .unwrap();
        let jobs: i64 = conn
            .query_row("SELECT COUNT(*) FROM job_samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!((hosts, jobs), (1, 1));
    }

    #[test]
    fn prune_removes_rows_older_than_the_cutoff_from_both_tables() {
        let (_dir, _path, mut conn) = temp_db();
        for at in [1_000, 2_000, 3_000] {
            write_sample(
                &mut conn,
                &host(at, Some(10.0), 200 * GIB, 1),
                &[pending(&format!("j{at}"), "tests", "concurrency_cap")],
            )
            .unwrap();
        }
        assert_eq!(prune(&conn, 2_500).unwrap(), 4);
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM host_samples WHERE sampled_at_ms = 3000",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(left, 1);
    }

    #[test]
    fn held_back_matches_the_memo_worked_example() {
        // 12 samples at 5 s; two background jobs held by the background limit;
        // 10 samples with headroom, 2 with a test build at 90% CPU.
        let (_dir, path, mut conn) = temp_db();
        let base = 1_000_000_000_000;
        for index in 0..12 {
            let cpu = if index < 10 { 30.0 } else { 90.0 };
            write_sample(
                &mut conn,
                &host(base + index * 5000, Some(cpu), 200 * GIB, 1),
                &[
                    pending("a", "background", "concurrency_cap"),
                    pending("b", "background", "concurrency_cap"),
                ],
            )
            .unwrap();
        }
        let stats = queue_stats_at(&path, 24, base + 60_000).unwrap();
        assert_eq!(stats["waiting"][0]["group"], "limits");
        assert_eq!(stats["waiting"][0]["job_seconds"], 120);
        assert_eq!(stats["waiting"][0]["headroom_job_seconds"], 100);
        assert_eq!(stats["waiting"][0]["unknown_job_seconds"], 0);
        assert_eq!(stats["waiting"][1]["job_seconds"], 0);
        assert_eq!(stats["covered_seconds"], 60);
        assert_eq!(stats["by_type"], json!([]));
    }

    #[test]
    fn outages_add_no_time_and_missing_readings_count_as_unknown() {
        let (_dir, path, mut conn) = temp_db();
        let base = 1_000_000_000_000;
        // A sample, then the server is down for 10 minutes.
        write_sample(
            &mut conn,
            &host(base, None, 200 * GIB, 1),
            &[pending("a", "perf", "perf_cooldown")],
        )
        .unwrap();
        write_sample(
            &mut conn,
            &host(base + 600_000, Some(10.0), 10 * GIB, 1),
            &[pending("a", "perf", "memory_pressure")],
        )
        .unwrap();
        let stats = queue_stats_at(&path, 24, base + 700_000).unwrap();
        assert_eq!(stats["covered_seconds"], 10);
        let perf_rules = &stats["waiting"][1];
        assert_eq!(perf_rules["job_seconds"], 5);
        assert_eq!(perf_rules["unknown_job_seconds"], 5);
        let memory = &stats["waiting"][2];
        assert_eq!(memory["job_seconds"], 5);
        assert_eq!(memory["headroom_job_seconds"], 0);
    }

    #[test]
    fn peaks_report_percentiles_per_type() {
        let (_dir, path, mut conn) = temp_db();
        let base = 1_000_000_000_000;
        for index in 0..3 {
            let jobs: Vec<JobSample> = (0..4)
                .map(|job| JobSample {
                    job_id: format!("bg{job}"),
                    job_type: "background".into(),
                    state: "running".into(),
                    holding_reason: None,
                    rss_bytes: Some((job + 1) * GIB + index),
                    cpu_seconds_total: Some((index * 5 * (job + 1)) as f64),
                    process_count: Some(3),
                })
                .collect();
            write_sample(
                &mut conn,
                &host(base + index * 5000, Some(50.0), 200 * GIB, 1),
                &jobs,
            )
            .unwrap();
        }
        let stats = queue_stats_at(&path, 24, base + 60_000).unwrap();
        let background = &stats["by_type"][0];
        assert_eq!(background["type"], "background");
        assert_eq!(background["jobs"], 4);
        assert_eq!(background["peak_rss_p50_bytes"], 2 * GIB + 2);
        assert_eq!(background["peak_rss_max_bytes"], 4 * GIB + 2);
        assert_eq!(background["cpu_cores_p95"], 4.0);
        let mut values = vec![5.0, 1.0, 3.0];
        assert_eq!(percentile(&mut values, 50.0), Some(3.0));
        assert_eq!(percentile(&mut [], 50.0), None);
    }

    #[test]
    fn stats_and_series_are_unavailable_without_a_database() {
        let dir = TempDir::new();
        let path = dir.0.join("missing.db");
        assert_eq!(queue_stats(&path, 24).unwrap(), json!({"available": false}));
        assert_eq!(series(&path, 24).unwrap(), json!({"available": false}));
    }

    #[test]
    fn series_aligns_buckets_and_leaves_gaps_empty() {
        // hours=1 at 12:00:07 → end 12:00:15, start 11:00:15, 240 buckets of 15 s.
        let (_dir, path, mut conn) = temp_db();
        let noon = 1_790_000_000_000 - 1_790_000_000_000 % 3_600_000 + 12 * 3_600_000;
        let start = noon - 3_600_000 + 15_000;
        for offset in [0, 5_000, 10_000, 15_000] {
            let mut sample = host(
                start + offset,
                Some(20.0 + offset as f64 / 1000.0),
                200 * GIB,
                1,
            );
            sample.running = [0, 2, 1, 0];
            sample.pending = [1, 0, 1, 0];
            write_sample(&mut conn, &sample, &[]).unwrap();
        }
        let mut hot = host(start + 20_000, Some(95.0), 20 * GIB, 2);
        hot.gpu_busy_pct = None;
        write_sample(&mut conn, &hot, &[]).unwrap();
        let body = series_at(&path, 1, noon + 7_000).unwrap();
        assert_eq!(body["bucket_seconds"], 15);
        let buckets = body["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 240);
        assert_eq!(body["start"], rfc3339_ms(start));
        assert_eq!(body["end"], rfc3339_ms(noon + 15_000));
        assert_eq!(buckets[0]["samples"], 3);
        assert_eq!(buckets[0]["cpu_avg"], 25.0);
        assert_eq!(buckets[0]["cpu_max"], 30.0);
        assert_eq!(buckets[0]["running"]["tests"], 2.0);
        assert_eq!(buckets[0]["pending_max"], 2);
        assert_eq!(buckets[1]["samples"], 2);
        assert_eq!(buckets[1]["pressure_max"], 2);
        assert_eq!(buckets[1]["gpu_avg"], 10.0);
        assert_eq!(
            buckets[2],
            json!({"start": rfc3339_ms(start + 30_000), "samples": 0})
        );
        let summary = &body["summary"];
        assert_eq!(summary["covered_seconds"], 25);
        assert_eq!(summary["cpu_busy_seconds"], 5);
        assert_eq!(summary["pressure_elevated_seconds"], 5);
        assert_eq!(summary["headroom_seconds"], 20);
        assert_eq!(summary["cpu_avg"], 41.0);
        assert_eq!(body["memory_total_bytes"], 256 * GIB);
        assert!(series_at(&path, 2, noon).is_err());
    }

    /// Cost check for the PR (Appendix B.3): `cargo test -p sm-server --lib
    /// recorder_sample_cost -- --ignored --nocapture`. Includes one process
    /// listing per sample, which the recorder only runs while a job runs.
    #[test]
    #[ignore = "measures this machine; run by hand"]
    fn recorder_sample_cost() {
        fn cpu_seconds() -> f64 {
            let usage = |who| {
                let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
                unsafe { libc::getrusage(who, usage.as_mut_ptr()) };
                let usage = unsafe { usage.assume_init() };
                let seconds = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
                seconds(usage.ru_utime) + seconds(usage.ru_stime)
            };
            usage(libc::RUSAGE_SELF) + usage(libc::RUSAGE_CHILDREN)
        }
        let dir = TempDir::new();
        let mut recorder = Recorder {
            settings: RecorderSettings {
                db_path: dir.0.join("utilization.db"),
                queue_db_path: dir.0.join("absent_queue.db"),
                interval: Duration::from_secs(5),
                retention_days: 90,
            },
            conn: None,
            previous_ticks: None,
            last_prune: None,
            last_write_warning: None,
        };
        let samples = 120;
        let part = |name: &str, step: &mut dyn FnMut()| {
            let start = cpu_seconds();
            for _ in 0..40 {
                step();
            }
            println!(
                "{name}: {:.2} ms CPU",
                (cpu_seconds() - start) * 1000.0 / 40.0
            );
        };
        part("cpu ticks + memory sysctls", &mut || {
            let _ = (
                mac::cpu_ticks(),
                mac::vm_pages(),
                mac::sysctl_i64("hw.memsize"),
            );
        });
        part("ioreg", &mut || {
            let _ = run(
                "/usr/sbin/ioreg",
                &["-r", "-c", "AGXAccelerator", "-d", "1"],
            );
        });
        part("ps", &mut || {
            let _ = run("/bin/ps", &["-axo", "pgid=,rss=,time="]);
        });
        let first = mac::cpu_ticks().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let second = mac::cpu_ticks().unwrap();
        println!(
            "ticks {first:?} -> {second:?}: {:?}%",
            cpu_busy_pct(first, second)
        );
        let (cpu_start, wall_start) = (cpu_seconds(), Instant::now());
        for _ in 0..samples {
            recorder.tick().unwrap();
            let listing = run("/bin/ps", &["-axo", "pgid=,rss=,time="]).unwrap();
            assert!(!parse_process_groups(&listing).is_empty());
            std::thread::sleep(Duration::from_millis(50));
        }
        let cpu_ms = (cpu_seconds() - cpu_start) * 1000.0 / samples as f64;
        let wall_ms = (wall_start.elapsed().as_secs_f64() * 1000.0) / samples as f64 - 50.0;
        let latest = latest_host_sample().unwrap();
        println!(
            "per sample: {cpu_ms:.1} ms CPU, {wall_ms:.1} ms wall; last: cpu={:?} gpu={:?} used={:?} available={:?} pressure={:?}",
            latest.cpu_busy_pct,
            latest.gpu_busy_pct,
            latest.mem_used_bytes,
            latest.mem_available_bytes,
            latest.pressure_level
        );
        assert!(cpu_ms < 50.0, "{cpu_ms} ms CPU per sample");
    }

    #[test]
    fn sampling_commands_are_killed_at_their_timeout() {
        let started = Instant::now();
        assert_eq!(
            run_with_timeout("/bin/sleep", &["10"], Duration::from_millis(200)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            run_with_timeout("/bin/echo", &["ok"], Duration::from_secs(4)).as_deref(),
            Some("ok\n")
        );
        assert_eq!(
            run_with_timeout("/usr/bin/false", &[], Duration::from_secs(4)),
            None
        );
    }

    #[test]
    fn series_bucket_counts_follow_the_range_table() {
        assert_eq!(series_bucket_seconds(1), Some((15, 240)));
        assert_eq!(series_bucket_seconds(24), Some((300, 288)));
        assert_eq!(series_bucket_seconds(168), Some((1800, 336)));
        assert_eq!(series_bucket_seconds(720), Some((7200, 360)));
        assert_eq!(series_bucket_seconds(48), None);
    }
}
