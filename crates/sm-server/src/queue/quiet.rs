//! Durable quiet-job notification and shared human-readable formatting.
use super::*;
use crate::utilization::{quiet::Status, RecorderSettings};
use std::io::{Read, Seek, SeekFrom, Write};

pub fn age(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h {:02}m", minutes / 60, minutes % 60)
    }
}

pub fn local_time(at: &str) -> String {
    parse_queue_timestamp(at)
        .and_then(local_now_naive)
        .map(|local| {
            let hour = local.hour();
            format!(
                "{}:{:02} {}",
                if hour % 12 == 0 { 12 } else { hour % 12 },
                local.minute(),
                if hour < 12 { "am" } else { "pm" }
            )
        })
        .unwrap_or_else(|| at.to_owned())
}

pub fn memory(footprint: Option<i64>, rss: Option<i64>) -> String {
    footprint
        .or(rss)
        .map(memory_amount_text)
        .unwrap_or_else(|| "an unknown amount".into())
}

fn tail(path: Option<&str>) -> String {
    let read = || -> std::io::Result<String> {
        let mut file = std::fs::File::open(path.unwrap_or(""))?;
        let len = file.metadata()?.len();
        let offset = len.saturating_sub(64 * 1024);
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take(64 * 1024).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        // At a bounded read's edge, discard the incomplete first line.
        let text = if offset > 0 {
            text.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
        } else {
            &text
        };
        let mut lines: Vec<_> = text
            .lines()
            .rev()
            .filter(|s| !s.trim().is_empty())
            .take(3)
            .collect();
        lines.reverse();
        Ok(lines
            .into_iter()
            .map(|line| {
                let truncated = if line.chars().count() > 200 {
                    format!("{}…", line.chars().take(199).collect::<String>())
                } else {
                    line.to_owned()
                };
                format!("  {truncated}")
            })
            .collect::<Vec<_>>()
            .join("\n"))
    };
    read()
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(log is empty)".into())
}

fn text(
    job: &QueueJobRecord,
    status: &Status,
    since: i64,
    now: i64,
    limit: usize,
    waiting: usize,
    repeat: u64,
) -> String {
    let label = if job.label.trim().is_empty() {
        &job.id
    } else {
        &job.label
    };
    let waiting = if waiting == 0 {
        String::new()
    } else {
        format!("; {waiting} {} jobs are waiting for a slot", job.job_type)
    };
    let repeat = if repeat % 60 == 0 {
        format!("{} hours", repeat / 60)
    } else {
        format!("{repeat} minutes")
    };
    format!("[sm queue] {label} has been quiet for {}: no CPU, GPU or log output since {}.\nIt holds {} of memory and one of {limit} {} slots{waiting}.\nLast log lines:\n{}\nIf it is hung, cancel it: sm queue cancel {}. If this is expected, do nothing; sm will not ask about this job again for {repeat}.\nLog: {}. ID: {}",
        age(now - since), local_time(&crate::utilization::quiet::timestamp(since)), memory(status.footprint_bytes, status.rss_bytes), job.job_type, tail(job.log_path.as_deref()), job.id, job.log_path.as_deref().unwrap_or("-"), job.id)
}

/// Rechecking an eligible edge is intentional: failed enqueues retry. The entry
/// time, rather than now, gates cooldown so an ineligible stretch never alerts
/// just because two hours elapsed. Replay reconstructs that same entry time.
pub(crate) fn alert(
    settings: &RecorderSettings,
    sampled: &ActiveQueueJob,
    status: &Status,
    since: i64,
    entered: i64,
    now: i64,
) -> Result<()> {
    if sampled.job_type == "service" {
        return Ok(());
    }
    let conn = open_queue_jobs_connection(&settings.queue_db_path)?;
    let _guard = QUEUE_ADMISSION_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let Some(job) = get_queue_job_conn(&conn, &sampled.id)? else {
        return Ok(());
    };
    if job.state != "running" || job.started_at != sampled.started_at {
        return Ok(());
    }
    let Some(target) = job
        .notify_session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(());
    };
    if let Some(last) = job
        .quiet_alerted_at
        .as_deref()
        .and_then(parse_queue_timestamp)
    {
        let last = (last.unix_timestamp_nanos() / 1_000_000) as i64;
        let repeat = i64::try_from(settings.quiet_alert_repeat_minutes)
            .unwrap_or(i64::MAX)
            .saturating_mul(60_000);
        if last >= since || entered.saturating_sub(last) < repeat {
            return Ok(());
        }
    }
    let message_id = format!("queue-quiet-{}-{since}", job.id);
    let queue = RetainedQueueStore::new(settings.message_queue_db_path.clone());
    // Recover a crash between enqueue and recording the alert time without
    // trying to enqueue the same id with a different age or log tail.
    let existing: Option<String> = queue.with_connection(|messages| {
        Ok(messages
            .query_row(
                "SELECT queued_at FROM message_queue WHERE id = ?1",
                [&message_id],
                |r| r.get(0),
            )
            .optional()?)
    })?;
    let alerted_at = if let Some(at) = existing {
        at
    } else {
        let waiting: usize = conn.query_row(
            "SELECT COUNT(*) FROM queue_jobs WHERE state = 'pending' AND type = ?1",
            [&job.job_type],
            |r| r.get(0),
        )?;
        let state_dir = settings.queue_db_path.parent().unwrap_or(Path::new(""));
        let policy = live_admission_policies()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(state_dir)
            .map(|p| *p.read().unwrap_or_else(|p| p.into_inner()))
            .unwrap_or_default();
        let limit = match job.job_type.as_str() {
            "tests" => policy.tests_max_concurrent,
            "perf" => policy.perf_max_concurrent,
            "background" => policy.background_max_concurrent,
            _ => policy.service_max_concurrent,
        };
        let text = text(
            &job,
            status,
            since,
            now,
            limit,
            waiting,
            settings.quiet_alert_repeat_minutes,
        );
        queue.enqueue_message_once_with_metadata(
            &message_id,
            target,
            &text,
            "sequential",
            QueueMessageMetadata {
                message_category: Some("queue-completion".into()),
                ..QueueMessageMetadata::default()
            },
        )?;
        if let Some(path) = &job.log_path {
            let notice = format!("\n{}\n", text.lines().next().unwrap_or_default());
            if let Ok(mut file) = OpenOptions::new().append(true).open(path) {
                if file.write_all(notice.as_bytes()).is_ok() {
                    conn.execute("UPDATE queue_jobs SET quiet_log_bytes = quiet_log_bytes + ?2 WHERE id = ?1", params![job.id, notice.len() as i64])?;
                }
            }
        }
        crate::utilization::quiet::timestamp(now)
    };
    conn.execute(
        "UPDATE queue_jobs SET quiet_alerted_at = ?2 WHERE id = ?1",
        params![job.id, alerted_at],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
        settings: RecorderSettings,
        conn: Connection,
        sampled: ActiveQueueJob,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "sm-quiet-{}-{}",
                std::process::id(),
                rand_core::OsRng.next_u64()
            ));
            fs::create_dir_all(&dir).unwrap();
            let settings = RecorderSettings {
                db_path: dir.join("utilization.db"),
                queue_db_path: dir.join("queue_runner.db"),
                message_queue_db_path: dir.join("messages.db"),
                interval: StdDuration::from_secs(5),
                retention_days: 90,
                quiet_minutes: 10,
                quiet_alert_repeat_minutes: 120,
            };
            let conn = Connection::open(&settings.queue_db_path).unwrap();
            init_queue_jobs_schema(&conn).unwrap();
            let log = dir.join("job.log");
            fs::write(&log, "first\nsecond\nthird\n").unwrap();
            conn.execute("INSERT INTO queue_jobs (id,type,label,notify_session_id,cwd,env_json,timeout_seconds,state,queued_at,started_at,log_path) VALUES ('job','background','probe','agent','/tmp','{}',20000,'running','1970-01-01T00:00:00Z','1970-01-01T00:00:00Z',?1)", [log.to_str().unwrap()]).unwrap();
            let sampled = active_queue_jobs_for_sampling(&settings.queue_db_path)
                .unwrap()
                .remove(0);
            Self {
                dir,
                settings,
                conn,
                sampled,
            }
        }
        fn alert(&self, since: i64, entered: i64, now: i64) {
            alert(
                &self.settings,
                &self.sampled,
                &Status {
                    footprint_bytes: Some(1024 * 1024 * 1024),
                    ..Status::default()
                },
                since,
                entered,
                now,
            )
            .unwrap();
        }
        fn count(&self) -> i64 {
            if !self.settings.message_queue_db_path.exists() {
                return 0;
            }
            Connection::open(&self.settings.message_queue_db_path)
                .unwrap()
                .query_row("SELECT COUNT(*) FROM message_queue", [], |r| r.get(0))
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn quiet_alert_is_once_per_stretch_and_cooldown_survives_restart() {
        let f = Fixture::new();
        f.alert(0, 600_000, 600_000);
        f.alert(0, 600_000, 605_000);
        f.alert(0, 600_000, 8_000_000);
        assert_eq!(f.count(), 1);
        let messages = Connection::open(&f.settings.message_queue_db_path).unwrap();
        let (target, mode, category): (String, String, String) = messages
            .query_row(
                "SELECT target_session_id,delivery_mode,message_category FROM message_queue",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (target.as_str(), mode.as_str(), category.as_str()),
            ("agent", "sequential", "queue-completion")
        );
        let job = get_queue_job_conn(&f.conn, "job").unwrap().unwrap();
        assert_eq!(
            job.quiet_alerted_at.as_deref(),
            Some("1970-01-01T00:10:00Z")
        );
        let notice: i64 = f
            .conn
            .query_row("SELECT quiet_log_bytes FROM queue_jobs", [], |r| r.get(0))
            .unwrap();
        assert!(notice > 0);
        f.alert(700_000, 1_300_000, 1_300_000);
        f.alert(700_000, 1_300_000, 9_000_000); // same suppressed edge, even after restart/cooldown
        assert_eq!(f.count(), 1);
        f.alert(7_200_000, 7_800_000, 7_800_000);
        assert_eq!(f.count(), 2);
        f.conn
            .execute("UPDATE queue_jobs SET state='cancelled'", [])
            .unwrap();
        f.alert(16_000_000, 16_600_000, 16_600_000);
        assert_eq!(f.count(), 2);
    }

    #[test]
    fn quiet_service_and_missing_target_never_alert() {
        let mut f = Fixture::new();
        f.sampled.job_type = "service".into();
        f.alert(0, 600_000, 600_000);
        assert_eq!(f.count(), 0);
        f.sampled.job_type = "background".into();
        f.conn
            .execute("UPDATE queue_jobs SET notify_session_id='  '", [])
            .unwrap();
        f.alert(0, 600_000, 600_000);
        assert_eq!(f.count(), 0);
    }

    #[test]
    fn quiet_recovers_enqueue_before_timestamp_write() {
        let f = Fixture::new();
        f.alert(0, 600_000, 600_000);
        f.conn
            .execute("UPDATE queue_jobs SET quiet_alerted_at=NULL", [])
            .unwrap();
        let before = fs::read(f.sampled.log_path.as_ref().unwrap()).unwrap();
        f.alert(0, 600_000, 605_000);
        assert_eq!(f.count(), 1);
        assert!(get_queue_job_conn(&f.conn, "job")
            .unwrap()
            .unwrap()
            .quiet_alerted_at
            .is_some());
        assert_eq!(
            fs::read(f.sampled.log_path.as_ref().unwrap()).unwrap(),
            before
        );
    }

    #[test]
    fn quiet_message_text_tail_and_fallbacks() {
        let f = Fixture::new();
        let job = get_queue_job_conn(&f.conn, "job").unwrap().unwrap();
        let output = text(&job, &Status::default(), 0, 600_000, 2, 7, 120);
        assert_eq!(output,format!("[sm queue] probe has been quiet for 10m: no CPU, GPU or log output since {}.\nIt holds an unknown amount of memory and one of 2 background slots; 7 background jobs are waiting for a slot.\nLast log lines:\n  first\n  second\n  third\nIf it is hung, cancel it: sm queue cancel job. If this is expected, do nothing; sm will not ask about this job again for 2 hours.\nLog: {}. ID: job",local_time("1970-01-01T00:00:00Z"),job.log_path.as_deref().unwrap()));
        let output = text(&job, &Status::default(), 0, 3_900_000, 2, 0, 90);
        assert!(!output.contains("waiting for a slot"));
        assert!(output.contains("1h 05m"));
        assert!(output.contains("90 minutes"));
        assert_eq!(memory(None, Some(1024 * 1024 * 1024)), "1.0 GiB");
        assert_eq!(tail(None), "(log is empty)");
        let log = job.log_path.as_deref().unwrap();
        fs::write(log, format!("{}\n\nlast\n", "界".repeat(400))).unwrap();
        let result = tail(Some(log));
        assert_eq!(result.lines().next().unwrap().chars().count(), 202);
        assert!(result.ends_with("…\n  last"));
        fs::write(log, "").unwrap();
        assert_eq!(tail(Some(log)), "(log is empty)");
    }
}
