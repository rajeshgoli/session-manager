//! One quiet clock for alerts, the CLI, Queue page and Board (sm#1716).
use super::*;
use crate::queue::QueueJobRecord;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    pub cpu_seconds: Option<f64>,
    pub gpu_seconds: Option<f64>,
    pub footprint_bytes: Option<i64>,
    pub rss_bytes: Option<i64>,
    pub log_updated_at: Option<String>,
    pub quiet_since: Option<String>,
    pub low_cpu: bool,
}

static STATUS: RwLock<Option<(PathBuf, HashMap<String, Status>)>> = RwLock::new(None);

pub fn status(queue_db: &Path, job: &QueueJobRecord) -> Status {
    if job.state != "running" {
        return Status::default();
    }
    STATUS
        .read()
        .ok()
        .and_then(|s| {
            let (path, jobs) = s.as_ref()?;
            (path == queue_db)
                .then(|| jobs.get(&job.id).cloned())
                .flatten()
        })
        .unwrap_or_default()
}

pub fn timestamp(ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .expect("sample time fits timestamp")
        .format(&Rfc3339)
        .expect("UTC timestamp")
}

#[derive(Clone, Debug)]
struct Sample {
    at: i64,
    cpu: Option<f64>,
    gpu: Option<f64>,
    log: Option<i64>,
    notice: i64,
    footprint: Option<i64>,
    rss: Option<i64>,
}

fn log_changed(p: &Sample, c: &Sample) -> bool {
    p.log
        .zip(c.log)
        .is_some_and(|(p_log, c_log)| c_log - p_log != c.notice - p.notice)
}

fn active(p: &Sample, c: &Sample) -> bool {
    let dt = (c.at - p.at) as f64 / 1000.0;
    dt <= 0.0
        || dt > 60.0
        || p.cpu
            .zip(c.cpu)
            .is_none_or(|(p, c)| (c - p).max(0.0) >= 0.01 * dt)
        || p.gpu.zip(c.gpu).is_some_and(|(p, c)| c > p)
        || log_changed(p, c)
}

#[derive(Default)]
struct Clock {
    started_at: Option<String>,
    previous: Option<Sample>,
    last_active: i64,
    log_updated: Option<i64>,
    became_quiet: Option<i64>,
    cpu_window: std::collections::VecDeque<(i64, Option<f64>)>,
    low_cpu: bool,
}

impl Clock {
    fn observe(&mut self, sample: Sample, window_ms: i64) {
        self.low_cpu = false;
        if window_ms == 0 {
            self.cpu_window.clear();
        } else {
            self.cpu_window.push_back((sample.at, sample.cpu));
            let cutoff = sample.at.saturating_sub(window_ms);
            // Keep one boundary reading so the whole window has measured deltas.
            while self.cpu_window.len() > 2 && self.cpu_window[1].0 <= cutoff {
                self.cpu_window.pop_front();
            }
            if self.cpu_window.front().is_some_and(|p| p.0 <= cutoff) {
                let mut growth = 0.0;
                let mut covered = true;
                for (p, c) in self.cpu_window.iter().zip(self.cpu_window.iter().skip(1)) {
                    match p.1.zip(c.1) {
                        Some((old, new)) if c.0 > p.0 && c.0 - p.0 <= 60_000 => {
                            growth += (new - old).max(0.0);
                        }
                        _ => covered = false,
                    }
                }
                let elapsed = sample.at - self.cpu_window[0].0;
                self.low_cpu = covered && elapsed > 0 && growth * 100_000.0 < elapsed as f64;
            }
        }
        if let Some(previous) = &self.previous {
            if log_changed(previous, &sample) {
                self.log_updated = Some(sample.at);
            }
            if active(previous, &sample) {
                self.last_active = sample.at;
                self.became_quiet = None;
            }
        } else {
            self.last_active = sample.at;
        }
        if window_ms > 0 && sample.at - self.last_active >= window_ms {
            self.became_quiet.get_or_insert(sample.at);
        }
        self.previous = Some(sample);
    }

    fn status(&self) -> Status {
        let Some(p) = &self.previous else {
            return Status::default();
        };
        Status {
            cpu_seconds: p.cpu,
            gpu_seconds: p.gpu,
            footprint_bytes: p.footprint,
            rss_bytes: p.rss,
            log_updated_at: self.log_updated.map(timestamp),
            quiet_since: self.became_quiet.map(|_| timestamp(self.last_active)),
            low_cpu: self.low_cpu,
        }
    }
}

#[derive(Default)]
pub(super) struct Detector {
    clocks: HashMap<String, Clock>,
}

impl Detector {
    pub(super) fn tick(
        &mut self,
        conn: &Connection,
        settings: &RecorderSettings,
        jobs: &[ActiveQueueJob],
        now: i64,
    ) -> Result<()> {
        self.clocks
            .retain(|id, _| jobs.iter().any(|j| j.id == *id && j.state == "running"));
        let window = i64::try_from(settings.quiet_minutes)
            .unwrap_or(i64::MAX)
            .saturating_mul(60_000);
        let mut statuses = HashMap::new();
        let mut alert_error = None;
        for job in jobs.iter().filter(|j| j.state == "running") {
            let clock = self.clocks.entry(job.id.clone()).or_default();
            if clock.started_at != job.started_at {
                *clock = Clock {
                    started_at: job.started_at.clone(),
                    ..Clock::default()
                };
            }
            let after = clock.previous.as_ref().map(|p| p.at).unwrap_or(i64::MIN);
            let started = job
                .started_at
                .as_deref()
                .and_then(crate::queue::parse_queue_timestamp)
                .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
                .unwrap_or(i64::MIN);
            let mut statement = conn.prepare("SELECT sampled_at_ms, cpu_seconds_total, gpu_seconds_total, log_bytes, log_notice_bytes, footprint_bytes, rss_bytes FROM job_samples WHERE job_id = ?1 AND state = 'running' AND sampled_at_ms > ?2 AND sampled_at_ms >= ?3 ORDER BY sampled_at_ms")?;
            let samples = statement.query_map(params![job.id, after, started], |row| {
                Ok(Sample {
                    at: row.get(0)?,
                    cpu: row.get(1)?,
                    gpu: row.get(2)?,
                    log: row.get(3)?,
                    notice: row.get(4)?,
                    footprint: row.get(5)?,
                    rss: row.get(6)?,
                })
            })?;
            for sample in samples {
                clock.observe(sample?, window);
            }
            if window == 0 {
                clock.became_quiet = None;
            }
            let status = clock.status();
            if let Some(entered) = clock.became_quiet {
                if let Err(error) = crate::queue::quiet::alert(
                    settings,
                    job,
                    &status,
                    clock.last_active,
                    entered,
                    now,
                ) {
                    alert_error.get_or_insert(error);
                }
            }
            statuses.insert(job.id.clone(), status);
        }
        *STATUS.write().unwrap_or_else(|p| p.into_inner()) =
            Some((settings.queue_db_path.clone(), statuses));
        // Delivery is retried on the next tick. A broken inbox must not hide
        // quiet jobs, stop evaluation of later jobs, or freeze their clocks.
        alert_error.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn low_cpu_is_window_growth_and_zero_disables_it() {
        let mut clock = Clock::default();
        for i in 0..=120 {
            let mut reading = sample(i * 5000);
            // One short burst, then an exited process drops the counter.
            reading.cpu = Some(if (30..90).contains(&i) { 0.1 } else { 0.0 });
            clock.observe(reading, 600_000);
        }
        assert!(clock.status().low_cpu);
        assert!(clock.cpu_window.len() <= 121);
        clock.observe(sample(605_000), 0);
        assert!(!clock.status().low_cpu);
        assert!(clock.cpu_window.is_empty());
    }

    #[test]
    fn low_cpu_requires_known_contiguous_readings() {
        let mut clock = Clock::default();
        clock.observe(sample(0), 60_000);
        clock.observe(sample(70_000), 60_000);
        assert!(!clock.status().low_cpu);
        let mut missing = sample(75_000);
        missing.cpu = None;
        clock.observe(missing, 60_000);
        assert!(!clock.status().low_cpu);
    }

    static STATUS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn sample(at: i64) -> Sample {
        Sample {
            at,
            cpu: Some(0.0),
            gpu: Some(0.0),
            log: Some(10),
            notice: 0,
            footprint: None,
            rss: None,
        }
    }

    #[test]
    fn quiet_step_rules() {
        let p = sample(0);
        let mut c = sample(5000);
        c.cpu = Some(0.05);
        assert!(active(&p, &c));
        c.cpu = Some(0.04);
        assert!(!active(&p, &c));
        c.cpu = Some(-1.0);
        assert!(!active(&p, &c));
        c.gpu = Some(0.001);
        assert!(active(&p, &c));
        c.gpu = None;
        assert!(!active(&p, &c));
        for bytes in [9, 11] {
            c.log = Some(bytes);
            assert!(active(&p, &c));
        }
        c.log = None;
        assert!(!active(&p, &c));
        c.at = 60_000;
        assert!(!active(&p, &c));
        c.at += 1;
        assert!(active(&p, &c));
        c.at = 5000;
        c.cpu = None;
        assert!(active(&p, &c));
        c.cpu = Some(0.0);
        let mut unknown = p.clone();
        unknown.cpu = None;
        assert!(active(&unknown, &c));
        c.log = Some(110);
        c.notice = 100;
        assert!(!active(&p, &c), "sm's notice is not job activity");
        c.log = Some(111);
        assert!(active(&p, &c), "job output alongside a notice is activity");
    }

    #[test]
    fn quiet_window_boundary_activity_and_disabled_detection() {
        let mut clock = Clock::default();
        for at in (0..600_000).step_by(5000) {
            clock.observe(sample(at), 600_000);
        }
        clock.observe(sample(599_000), 600_000);
        assert_eq!(clock.status().quiet_since, None);
        clock.observe(sample(600_000), 600_000);
        assert_eq!(clock.status().quiet_since, Some(timestamp(0)));
        let mut busy = sample(605_000);
        busy.cpu = Some(1.0);
        clock.observe(busy, 600_000);
        assert_eq!(clock.status().quiet_since, None);
        for at in (610_000..=1_205_000).step_by(5000) {
            clock.observe(sample(at), 600_000);
        }
        assert_eq!(clock.status().quiet_since, Some(timestamp(605_000)));
        let mut disabled = Clock::default();
        for at in (0..=900_000).step_by(5000) {
            disabled.observe(sample(at), 0);
        }
        assert_eq!(disabled.status().quiet_since, None);
    }

    #[test]
    fn quiet_historical_replay_stalled_and_busy_workers() {
        let stalled = include_str!("../../tests/fixtures/quiet_jobs/job_4489b0dcd0d9.csv");
        let busy = include_str!("../../tests/fixtures/quiet_jobs/job_cc4c866663f3.csv");
        for (csv, expected) in [(stalled, 1), (busy, 0)] {
            let mut clock = Clock::default();
            let mut edges = Vec::new();
            for row in csv.lines().skip(1) {
                let fields: Vec<_> = row.split(',').collect();
                let s = Sample {
                    at: fields[0].parse().unwrap(),
                    cpu: fields[1].parse().ok(),
                    gpu: fields[2].parse().ok(),
                    ..sample(0)
                };
                let was_quiet = clock.became_quiet;
                clock.observe(s, 600_000);
                if let (None, Some(entered)) = (was_quiet, clock.became_quiet) {
                    edges.push((clock.last_active, entered));
                }
            }
            assert_eq!(edges.len(), expected);
            if expected == 1 {
                assert_eq!(edges[0], (1_790_722_901_085, 1_790_723_503_476));
            }
        }
    }

    #[test]
    fn quiet_delivery_failure_still_publishes_all_jobs_and_retries() {
        use crate::queue::{CreateQueueJob, RetainedQueueStore};
        let _guard = STATUS_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("quiet-delivery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let messages = dir.join("unavailable");
        std::fs::create_dir_all(&messages).unwrap(); // Opening a directory as SQLite fails.
        let settings = RecorderSettings {
            db_path: dir.join("samples.db"),
            queue_db_path: dir.join("queue_runner.db"),
            message_queue_db_path: messages.clone(),
            interval: Duration::from_secs(5),
            retention_days: 90,
            quiet_minutes: 10,
            quiet_alert_repeat_minutes: 120,
        };
        let create = |label: &str| {
            RetainedQueueStore::create_queue_job_in_state_dir(
                &dir,
                CreateQueueJob {
                    local_submitter: None,
                    job_type: "background".into(),
                    label: label.into(),
                    requester_session_id: None,
                    notify_session_id: "agent".into(),
                    cwd: dir.display().to_string(),
                    argv: Some(vec!["true".into()]),
                    script: None,
                    env: Default::default(),
                    timeout_seconds: 3600,
                    cpu_percent: None,
                    gpu_percent: None,
                    memory_bytes: None,
                    rank_tickets: None,
                },
            )
            .unwrap()
        };
        let first = create("first");
        let second = create("second");
        let queue = Connection::open(&settings.queue_db_path).unwrap();
        queue
            .execute(
                "UPDATE queue_jobs SET state='running', started_at='1970-01-01T00:00:00Z'",
                [],
            )
            .unwrap();
        let mut conn = open_for_write(&settings.db_path).unwrap();
        for at in (0..=600_000).step_by(5000) {
            let rows = [&first, &second].map(|j| JobSample {
                job_id: j.id.clone(),
                job_type: "background".into(),
                state: "running".into(),
                cpu_seconds_total: Some(0.0),
                ..JobSample::default()
            });
            write_sample(
                &mut conn,
                &HostSample {
                    sampled_at_ms: at,
                    ..HostSample::default()
                },
                &rows,
            )
            .unwrap();
        }
        let jobs = active_queue_jobs_for_sampling(&settings.queue_db_path).unwrap();
        let mut detector = Detector::default();
        assert!(detector.tick(&conn, &settings, &jobs, 600_000).is_err());
        for id in [&first.id, &second.id] {
            let record = RetainedQueueStore::get_queue_job_from_path(&settings.queue_db_path, id)
                .unwrap()
                .unwrap();
            assert_eq!(
                status(&settings.queue_db_path, &record).quiet_since,
                Some(timestamp(0))
            );
            assert!(record.quiet_alerted_at.is_none());
        }
        std::fs::remove_dir(&messages).unwrap();
        detector.tick(&conn, &settings, &jobs, 605_000).unwrap();
        detector.tick(&conn, &settings, &jobs, 610_000).unwrap();
        let count: i64 = Connection::open(&messages)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM message_queue WHERE id LIKE 'queue-quiet-%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        drop(conn);
        drop(queue);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quiet_replay_and_incremental_updates_agree() {
        let _guard = STATUS_TEST_LOCK.lock().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let settings = RecorderSettings {
            db_path: PathBuf::new(),
            queue_db_path: PathBuf::from("quiet-replay"),
            interval: Duration::from_secs(5),
            retention_days: 90,
            quiet_minutes: 10,
            quiet_alert_repeat_minutes: 120,
            message_queue_db_path: PathBuf::new(),
        };
        let jobs = [ActiveQueueJob {
            id: "service".into(),
            job_type: "service".into(),
            state: "running".into(),
            holding_reason: None,
            process_group_id: None,
            started_at: Some(timestamp(0)),
            log_path: None,
            log_notice_bytes: 0,
        }];
        let mut incremental = Detector::default();
        for at in (0..=600_000).step_by(5000) {
            conn.execute("INSERT INTO job_samples (sampled_at_ms,job_id,job_type,state,cpu_seconds_total,log_bytes) VALUES (?1,'service','service','running',0,10)", [at]).unwrap();
            incremental.tick(&conn, &settings, &jobs, at).unwrap();
        }
        let expected = incremental.clocks["service"].status().quiet_since;
        assert_eq!(expected, Some(timestamp(0)));
        let mut restarted = Detector::default();
        restarted.tick(&conn, &settings, &jobs, 600_000).unwrap();
        assert_eq!(restarted.clocks["service"].status().quiet_since, expected);
        assert_eq!(restarted.clocks["service"].became_quiet, Some(600_000));
        conn.execute("INSERT INTO job_samples (sampled_at_ms,job_id,job_type,state,cpu_seconds_total) VALUES (665000,'service','service','running',0)", []).unwrap();
        restarted.tick(&conn, &settings, &jobs, 665_000).unwrap();
        assert!(restarted.clocks["service"].status().quiet_since.is_none());
        restarted.tick(&conn, &settings, &[], 670_000).unwrap();
        assert!(restarted.clocks.is_empty());
    }
}
