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
}

impl Clock {
    fn observe(&mut self, sample: Sample, window_ms: i64) {
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
                crate::queue::quiet::alert(
                    settings,
                    job,
                    &status,
                    clock.last_active,
                    entered,
                    now,
                )?;
            }
            statuses.insert(job.id.clone(), status);
        }
        *STATUS.write().unwrap_or_else(|p| p.into_inner()) =
            Some((settings.queue_db_path.clone(), statuses));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn quiet_replay_and_incremental_updates_agree() {
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
