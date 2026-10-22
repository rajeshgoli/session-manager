//! A running job can release job-count capacity without losing resource guards.
use super::*;
use crate::config::QueueNearIdleConfig;
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub(crate) struct ReleasedSlot {
    pub since: i64,
    pub started_at: Option<String>,
}

type Slots = HashMap<String, ReleasedSlot>;
static RELEASED: RwLock<Option<HashMap<PathBuf, Slots>>> = RwLock::new(None);

// SQLite resolves symlinks (notably /var -> /private/var on macOS), while
// recorder settings keep the configured spelling of the same database.
fn queue_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn since(
    queue_db: &Path,
    id: &str,
    state: &str,
    job_type: &str,
    started_at: &Option<String>,
) -> Option<i64> {
    if state != "running" || job_type == crate::queue::shell::SHELL_JOB_TYPE {
        return None;
    }
    let key = queue_key(queue_db);
    let all = RELEASED.read().ok()?;
    let slot = all.as_ref()?.get(&key)?.get(id)?;
    (&slot.started_at == started_at).then_some(slot.since)
}

/// Caller holds the admission lock, so a wake cannot race an admission pass.
pub(crate) fn publish(queue_db: &Path, slots: Slots) {
    let key = queue_key(queue_db);
    RELEASED
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(key, slots);
}

#[derive(Clone, Debug)]
struct Sample {
    at: i64,
    cpu: Option<f64>,
    gpu: Option<f64>,
    memory: Option<i64>,
}

fn window_ms(minutes: u64) -> i64 {
    i64::try_from(minutes)
        .unwrap_or(i64::MAX)
        .saturating_mul(60_000)
}

/// Positive counter deltas only; interpolate the interval crossing the boundary.
/// Unknown readings and gaps never qualify a job for slot release.
fn growth(samples: &VecDeque<Sample>, cutoff: i64, cpu: bool) -> Option<f64> {
    if samples.front()?.at > cutoff {
        return None;
    }
    let mut total = 0.0;
    for (p, c) in samples.iter().zip(samples.iter().skip(1)) {
        if c.at <= cutoff {
            continue;
        }
        let elapsed = c.at - p.at;
        if !(1..=60_000).contains(&elapsed) {
            return None;
        }
        let (old, new) = if cpu {
            p.cpu.zip(c.cpu)?
        } else {
            p.gpu.zip(c.gpu)?
        };
        if !old.is_finite() || !new.is_finite() {
            return None;
        }
        total += (new - old).max(0.0) * (c.at - p.at.max(cutoff)) as f64 / elapsed as f64;
    }
    Some(total)
}

#[derive(Default)]
struct Clock {
    started_at: Option<String>,
    samples: VecDeque<Sample>,
    since: Option<i64>,
    eligible_after: i64,
}

impl Clock {
    fn observe(&mut self, sample: Sample, config: &QueueNearIdleConfig) {
        let at = sample.at;
        self.samples.push_back(sample);
        let entry_window = window_ms(config.minutes);
        let cutoff = at.saturating_sub(entry_window);
        while self.samples.len() > 2 && self.samples[1].at <= cutoff {
            self.samples.pop_front();
        }
        let memory = self.samples.back().and_then(|s| s.memory);
        if let Some(since) = self.since {
            let wake_cutoff = at.saturating_sub(window_ms(config.wake_minutes)).max(since);
            let seconds = (at - wake_cutoff) as f64 / 1000.0;
            let busy = |cpu, percent| {
                seconds > 0.0
                    && growth(&self.samples, wake_cutoff, cpu)
                        .is_some_and(|value| value > percent / 100.0 * seconds)
            };
            if memory.is_some_and(|bytes| bytes as f64 > config.wake_memory_gib * 1073741824.0)
                || busy(true, config.wake_cpu_percent)
                || busy(false, config.wake_gpu_percent)
            {
                self.since = None;
                self.eligible_after = at;
                // Keep the waking sample as the start of the fresh window.
                let waking = self.samples.back().cloned().expect("observed sample");
                self.samples.clear();
                self.samples.push_back(waking);
            }
            return;
        }
        if at.saturating_sub(self.eligible_after) < entry_window {
            return;
        }
        let seconds = entry_window as f64 / 1000.0;
        if growth(&self.samples, cutoff, true)
            .is_some_and(|value| value < config.cpu_percent / 100.0 * seconds)
            && growth(&self.samples, cutoff, false)
                .is_some_and(|value| value < config.gpu_percent / 100.0 * seconds)
            && self.samples.iter().filter(|s| s.at >= cutoff).all(|s| {
                s.memory
                    .is_some_and(|bytes| (bytes as f64) < config.memory_gib * 1073741824.0)
            })
        {
            self.since = Some(at);
        }
    }
}

#[derive(Default)]
pub(super) struct Detector {
    clocks: HashMap<String, Clock>,
}

impl Detector {
    /// Returns whether a newly released slot needs an admission pass.
    pub(super) fn tick(
        &mut self,
        conn: &Connection,
        settings: &RecorderSettings,
        jobs: &[ActiveQueueJob],
        now: i64,
    ) -> Result<bool> {
        let config = &settings.near_idle;
        self.clocks.retain(|id, _| {
            config.enabled && jobs.iter().any(|j| j.id == *id && j.state == "running")
        });
        let mut slots = HashMap::new();
        let mut released = false;
        for job in jobs.iter().filter(|j| {
            config.enabled
                && j.state == "running"
                && j.job_type != crate::queue::shell::SHELL_JOB_TYPE
        }) {
            let started = job
                .started_at
                .as_deref()
                .and_then(crate::queue::parse_queue_timestamp)
                .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
                .unwrap_or(now);
            let clock = self.clocks.entry(job.id.clone()).or_default();
            if clock.started_at != job.started_at {
                *clock = Clock {
                    started_at: job.started_at.clone(),
                    eligible_after: started,
                    ..Clock::default()
                };
            }
            let was_released = clock.since.is_some();
            let after = clock.samples.back().map(|s| s.at).unwrap_or(i64::MIN);
            // On first use, bound the replay to one window plus a boundary sample.
            let floor = now
                .saturating_sub(window_ms(config.minutes))
                .saturating_sub(60_000)
                .max(started);
            let mut statement = conn.prepare("SELECT sampled_at_ms, cpu_seconds_total, gpu_seconds_total, COALESCE(footprint_bytes,rss_bytes) FROM job_samples WHERE job_id=?1 AND state='running' AND sampled_at_ms>?2 AND sampled_at_ms>=?3 ORDER BY sampled_at_ms")?;
            let samples = statement.query_map(params![job.id, after, floor], |row| {
                Ok(Sample {
                    at: row.get(0)?,
                    cpu: row.get(1)?,
                    gpu: row.get(2)?,
                    memory: row.get(3)?,
                })
            })?;
            for sample in samples {
                clock.observe(sample?, config);
            }
            if let Some(since) = clock.since {
                slots.insert(
                    job.id.clone(),
                    ReleasedSlot {
                        since,
                        started_at: job.started_at.clone(),
                    },
                );
                released |= !was_released;
            }
        }
        crate::queue::publish_near_idle_slots(&settings.queue_db_path, slots);
        Ok(released)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::RngCore;

    const GIB: i64 = 1073741824;
    fn sample(at: i64, cpu: f64, gpu: f64, memory: Option<i64>) -> Sample {
        Sample {
            at,
            cpu: Some(cpu),
            gpu: Some(gpu),
            memory,
        }
    }
    fn feed(
        clock: &mut Clock,
        start: i64,
        end: i64,
        cpu: f64,
        gpu: f64,
        config: &QueueNearIdleConfig,
    ) {
        for at in (start..=end).step_by(5000) {
            clock.observe(
                sample(
                    at,
                    at as f64 / 1000.0 * cpu,
                    at as f64 / 1000.0 * gpu,
                    Some(GIB),
                ),
                config,
            );
        }
    }

    #[test]
    fn full_window_and_start_age_are_required() {
        let config = QueueNearIdleConfig::default();
        let mut clock = Clock {
            eligible_after: 5000,
            ..Clock::default()
        };
        feed(&mut clock, 0, 1_795_000, 0.02, 0.0, &config);
        assert_eq!(clock.since, None);
        feed(&mut clock, 1_800_000, 1_800_000, 0.02, 0.0, &config);
        assert_eq!(clock.since, None); // job only 29:55 old
        feed(&mut clock, 1_805_000, 1_805_000, 0.02, 0.0, &config);
        assert_eq!(clock.since, Some(1_805_000));
        let mut clock = Clock::default();
        feed(&mut clock, 0, 1_795_000, 0.0, 0.0, &config);
        assert_eq!(clock.since, None);
        feed(&mut clock, 1_800_000, 1_800_000, 0.0, 0.0, &config);
        assert_eq!(clock.since, Some(1_800_000));
    }

    #[test]
    fn each_entry_condition_independently_keeps_the_slot() {
        let config = QueueNearIdleConfig::default();
        for condition in [
            "cpu",
            "cpu_equal",
            "gpu",
            "gpu_equal",
            "memory",
            "gap",
            "null",
            "cpu_null",
            "gpu_null",
        ] {
            let mut clock = Clock::default();
            for at in (0..=1_800_000).step_by(5000) {
                if condition == "gap" && (5000..=65_000).contains(&at) {
                    continue;
                }
                let cpu = match condition {
                    "cpu" => 0.51,
                    "cpu_equal" => 0.5,
                    _ => 0.0,
                };
                let gpu = match condition {
                    "gpu" => 0.11,
                    "gpu_equal" => 0.1,
                    _ => 0.0,
                };
                let memory = if at == 900_000 && condition == "null" {
                    None
                } else {
                    Some(if at == 900_000 && condition == "memory" {
                        5 * GIB
                    } else {
                        GIB
                    })
                };
                let mut s = sample(
                    at,
                    at as f64 / 1000.0 * cpu,
                    at as f64 / 1000.0 * gpu,
                    memory,
                );
                if at == 900_000 && condition == "cpu_null" {
                    s.cpu = None;
                }
                if at == 900_000 && condition == "gpu_null" {
                    s.gpu = None;
                }
                clock.observe(s, &config);
            }
            assert_eq!(clock.since, None, "{condition}");
        }
    }

    #[test]
    fn counter_drops_are_not_activity_and_boundary_deltas_are_clipped() {
        let config = QueueNearIdleConfig::default();
        let mut clock = Clock::default();
        for at in (0..=1_800_000).step_by(5000) {
            clock.observe(
                sample(at, if at < 900_000 { 100.0 } else { 0.0 }, 0.0, Some(GIB)),
                &config,
            );
        }
        assert!(clock.since.is_some());
        let samples = VecDeque::from([
            sample(0, 0.0, 0.0, Some(GIB)),
            sample(5000, 10.0, 0.0, Some(GIB)),
        ]);
        assert_eq!(growth(&samples, 2500, true), Some(5.0));
    }

    #[test]
    fn each_wake_trigger_and_fresh_reentry_window() {
        let config = QueueNearIdleConfig::default();
        for trigger in ["cpu", "gpu", "memory"] {
            let mut clock = Clock::default();
            feed(&mut clock, 0, 1_800_000, 0.0, 0.0, &config);
            clock.observe(
                sample(
                    1_805_000,
                    if trigger == "cpu" { 6.0 } else { 0.0 },
                    if trigger == "gpu" { 1.1 } else { 0.0 },
                    Some(if trigger == "memory" { 11 * GIB } else { GIB }),
                ),
                &config,
            );
            assert_eq!(clock.since, None, "{trigger}");
            feed(&mut clock, 1_810_000, 3_600_000, 0.0, 0.0, &config);
            assert_eq!(clock.since, None, "fresh window: {trigger}");
            feed(&mut clock, 3_605_000, 3_610_000, 0.0, 0.0, &config);
            assert!(clock.since.is_some(), "reentry: {trigger}");
        }
    }

    #[test]
    fn wake_uses_five_minute_average_and_strict_thresholds() {
        let config = QueueNearIdleConfig::default();
        let mut clock = Clock::default();
        feed(&mut clock, 0, 2_100_000, 0.0, 0.0, &config);
        for at in (2_105_000..=2_160_000).step_by(5000) {
            clock.observe(
                sample(
                    at,
                    (at - 2_100_000) as f64 / 1000.0 * 2.0,
                    0.0,
                    Some(10 * GIB),
                ),
                &config,
            );
        }
        assert!(clock.since.is_some()); // one minute at two cores averages 0.4 cores
        clock.observe(sample(2_165_000, 300.0, 60.0, Some(10 * GIB)), &config);
        assert!(clock.since.is_some()); // equality does not wake
        clock.observe(sample(2_170_000, 300.1, 60.1, Some(10 * GIB)), &config);
        assert_eq!(clock.since, None);
    }

    #[test]
    fn detector_all_types_logs_rss_fallback_disable_and_queue_scope() {
        let dir =
            std::env::temp_dir().join(format!("sm-near-idle-{}", rand_core::OsRng.next_u64()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut conn = open_for_write(&dir.join("utilization.db")).unwrap();
        let mut settings = RecorderSettings {
            near_idle: QueueNearIdleConfig::default(),
            admission_policy: Default::default(),
            cancel_grace_seconds: 0,
            db_path: dir.join("utilization.db"),
            queue_db_path: dir.join("queue_runner.db"),
            interval: Duration::from_secs(5),
            retention_days: 1,
            quiet_minutes: 10,
            quiet_alert_repeat_minutes: 120,
            message_queue_db_path: dir.join("messages.db"),
        };
        let jobs: Vec<_> = ["background", "service", "tests", "perf", "review", "shell"]
            .into_iter()
            .map(|kind| ActiveQueueJob {
                id: kind.into(),
                job_type: kind.into(),
                state: "running".into(),
                started_at: Some(quiet::timestamp(0)),
                log_path: None,
                log_notice_bytes: 0,
                holding_reason: None,
                process_group_id: None,
            })
            .collect();
        for at in (0..=1_800_000).step_by(5000) {
            let samples: Vec<_> = jobs
                .iter()
                .map(|j| JobSample {
                    job_id: j.id.clone(),
                    job_type: j.job_type.clone(),
                    state: j.state.clone(),
                    cpu_seconds_total: Some(0.0),
                    gpu_seconds_total: Some(0.0),
                    rss_bytes: Some(GIB),
                    log_bytes: Some(at),
                    ..JobSample::default()
                })
                .collect();
            write_sample(
                &mut conn,
                &HostSample {
                    sampled_at_ms: at,
                    ..HostSample::default()
                },
                &samples,
            )
            .unwrap();
        }
        let mut detector = Detector::default();
        assert!(detector.tick(&conn, &settings, &jobs, 1_800_000).unwrap());
        for j in &jobs {
            assert_eq!(
                since(
                    &settings.queue_db_path,
                    &j.id,
                    &j.state,
                    &j.job_type,
                    &j.started_at
                )
                .is_some(),
                j.job_type != "shell"
            );
            assert!(since(
                &dir.join("other.db"),
                &j.id,
                &j.state,
                &j.job_type,
                &j.started_at
            )
            .is_none());
            assert!(since(
                &settings.queue_db_path,
                &j.id,
                "pending",
                &j.job_type,
                &j.started_at
            )
            .is_none());
            assert!(since(
                &settings.queue_db_path,
                &j.id,
                &j.state,
                &j.job_type,
                &Some(quiet::timestamp(5000))
            )
            .is_none());
        }
        assert!(!detector.tick(&conn, &settings, &jobs, 1_800_000).unwrap());
        settings.near_idle.enabled = false;
        assert!(!detector.tick(&conn, &settings, &jobs, 1_800_000).unwrap());
        assert!(since(
            &settings.queue_db_path,
            "background",
            "running",
            "background",
            &jobs[0].started_at
        )
        .is_none());
        drop(conn);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
