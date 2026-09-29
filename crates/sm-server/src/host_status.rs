//! On-demand host measurements. No task or timer runs between requests.
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::Mutex;

async fn read_command(program: &str, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(4),
        Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

// Serialize sampling and share bursts of requests. Nothing runs between requests.
struct SnapshotCache {
    sample: Mutex<Option<(Instant, Value)>>,
    ttl: Duration,
}

impl SnapshotCache {
    async fn get<F, Fut>(&self, sample: F) -> Value
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Value>,
    {
        let mut cached = self.sample.lock().await;
        if let Some((sampled_at, value)) = cached.as_ref() {
            if sampled_at.elapsed() < self.ttl {
                return value.clone();
            }
        }
        let value = sample().await;
        *cached = Some((Instant::now(), value.clone()));
        value
    }
}

static SNAPSHOT_CACHE: SnapshotCache = SnapshotCache {
    sample: Mutex::const_new(None),
    ttl: Duration::from_secs(5),
};

/// The live bar's reading: the utilization recorder's newest sample while it
/// is fresh (sm#1609), otherwise an on-demand `top` reading.
pub async fn snapshot() -> Value {
    let now_ms = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    if let Some(sample) =
        crate::utilization::latest_host_sample().filter(|sample| is_fresh(sample, now_ms))
    {
        return recorder_snapshot(&sample, hostname().await);
    }
    let mut value = SNAPSHOT_CACHE.get(collect_snapshot).await;
    if let Some(object) = value.as_object_mut() {
        object.insert("source".into(), json!("live"));
        object.insert("memory_available_bytes".into(), Value::Null);
    }
    value
}

fn is_fresh(sample: &crate::utilization::HostSample, now_ms: i64) -> bool {
    let age = now_ms - sample.sampled_at_ms;
    (0..3 * sample.interval_ms.max(1000)).contains(&age)
}

async fn hostname() -> Option<String> {
    static HOSTNAME: tokio::sync::OnceCell<Option<String>> = tokio::sync::OnceCell::const_new();
    HOSTNAME
        .get_or_init(|| async {
            read_command("/bin/hostname", &[])
                .await
                .map(|s| s.trim().to_owned())
        })
        .await
        .clone()
}

fn recorder_snapshot(sample: &crate::utilization::HostSample, host: Option<String>) -> Value {
    let sampled_at = time::OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(sample.sampled_at_ms) * 1_000_000,
    )
    .ok()
    .and_then(|at| {
        at.format(&time::format_description::well_known::Rfc3339)
            .ok()
    });
    json!({
        // A fresh sample is a reading; a failed measurement is its own null.
        "available": true,
        "host": host,
        "sampled_at": sampled_at,
        "memory_total_bytes": sample.mem_total_bytes,
        "memory_used_bytes": sample.mem_used_bytes,
        "memory_available_bytes": sample.mem_available_bytes,
        "memory_pressure": sample.pressure_level.and_then(|level| pressure_label(&level.to_string())),
        "cpu_percent": sample.cpu_busy_pct,
        "gpu_percent": sample.gpu_busy_pct,
        "source": "recorder",
    })
}

async fn collect_snapshot() -> Value {
    if !cfg!(target_os = "macos") {
        return json!({"available": false, "error": "Host statistics are not available on this system."});
    }
    let (top, total, pressure, gpu, host) = tokio::join!(
        read_command("/usr/bin/top", &["-l", "2", "-s", "1", "-n", "0"]),
        read_command("/usr/sbin/sysctl", &["-n", "hw.memsize"]),
        read_command(
            "/usr/sbin/sysctl",
            &["-n", "kern.memorystatus_vm_pressure_level"]
        ),
        read_command(
            "/usr/sbin/ioreg",
            &["-r", "-c", "AGXAccelerator", "-d", "1"]
        ),
        read_command("/bin/hostname", &[]),
    );
    let top = top.unwrap_or_default();
    json!({
        "available": !top.is_empty(),
        "host": host.map(|s| s.trim().to_owned()),
        "sampled_at": time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).ok(),
        "memory_total_bytes": total.and_then(|s| s.trim().parse::<u64>().ok()),
        "memory_used_bytes": memory_used(&top),
        "memory_pressure": pressure.as_deref().and_then(pressure_label),
        "cpu_percent": cpu_percent(&top),
        "gpu_percent": gpu.as_deref().and_then(gpu_percent),
    })
}

fn pressure_label(value: &str) -> Option<&'static str> {
    // XNU's sysctl exposes NOTE_MEMORYSTATUS_PRESSURE_* notification flags.
    match value.trim() {
        "1" => Some("Normal"),
        "2" => Some("Elevated"),
        "4" => Some("Critical"),
        _ => None,
    }
}

fn cpu_percent(top: &str) -> Option<f64> {
    let line = top.lines().rfind(|line| line.starts_with("CPU usage:"))?;
    let idle = line
        .split(',')
        .find(|part| part.contains("idle"))?
        .trim()
        .split('%')
        .next()?
        .parse::<f64>()
        .ok()?;
    idle.is_finite().then(|| (100.0 - idle).clamp(0.0, 100.0))
}

fn memory_used(top: &str) -> Option<u64> {
    let amount = top
        .lines()
        .filter_map(|line| line.strip_prefix("PhysMem: "))
        .next_back()?
        .split_whitespace()
        .next()?;
    let unit = amount.chars().last()?;
    let multiplier = match unit {
        'B' => 1.0,
        'K' => 1024.0,
        'M' => 1024.0_f64.powi(2),
        'G' => 1024.0_f64.powi(3),
        'T' => 1024.0_f64.powi(4),
        _ => return None,
    };
    let value = amount[..amount.len() - 1].parse::<f64>().ok()?;
    (value.is_finite() && value >= 0.0).then_some((value * multiplier) as u64)
}

pub(crate) fn gpu_percent(ioreg: &str) -> Option<f64> {
    let (_, value) = ioreg.split_once("\"Device Utilization %\"=")?;
    let number = value
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>();
    number
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn concurrent_requests_share_a_sample_and_refresh_only_after_expiry() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let cache = Arc::new(SnapshotCache {
            sample: Mutex::new(None),
            ttl: Duration::from_secs(5),
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let cache = cache.clone();
            let calls = calls.clone();
            tasks.spawn(async move {
                cache
                    .get(|| async {
                        let index = calls.fetch_add(1, Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        json!({"sample": index})
                    })
                    .await
            });
        }
        while let Some(result) = tasks.join_next().await {
            assert_eq!(result.unwrap(), json!({"sample": 0}));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        cache.sample.lock().await.as_mut().unwrap().0 = Instant::now() - Duration::from_secs(6);
        assert_eq!(
            cache.get(|| async { json!({"sample": 1}) }).await,
            json!({"sample": 1})
        );
    }

    #[test]
    fn recorder_samples_answer_while_fresh() {
        let sample = crate::utilization::HostSample {
            sampled_at_ms: 1_000_000,
            interval_ms: 5000,
            cpu_busy_pct: Some(42.5),
            mem_total_bytes: Some(256),
            mem_used_bytes: Some(87),
            mem_available_bytes: Some(200),
            pressure_level: Some(2),
            ..Default::default()
        };
        assert!(is_fresh(&sample, 1_000_000 + 14_999));
        assert!(!is_fresh(&sample, 1_000_000 + 15_000));
        assert!(!is_fresh(&sample, 999_000));
        let value = recorder_snapshot(&sample, Some("studio".into()));
        assert_eq!(value["source"], "recorder");
        assert_eq!(value["memory_pressure"], "Elevated");
        assert_eq!(value["memory_available_bytes"], 200);
        assert_eq!(value["cpu_percent"], 42.5);
        assert_eq!(value["sampled_at"], "1970-01-01T00:16:40Z");
        assert_eq!(value["gpu_percent"], Value::Null);
        let empty = crate::utilization::HostSample {
            sampled_at_ms: 1_000_000,
            interval_ms: 5000,
            ..Default::default()
        };
        assert_eq!(recorder_snapshot(&empty, None)["available"], true);
    }

    #[test]
    fn uses_recent_cpu_sample_and_preserves_missing_measurements() {
        let sample = "CPU usage: 10% user, 20% sys, 70% idle\nPhysMem: 12G used (1G wired), 4G unused.\nCPU usage: 1% user, 2% sys, 97% idle\nPhysMem: 12500M used (1G wired), 4G unused.";
        assert_eq!(cpu_percent(sample), Some(3.0));
        assert_eq!(memory_used(sample), Some(12500 * 1024 * 1024));
        assert_eq!(cpu_percent("unavailable"), None);
        assert_eq!(gpu_percent("unavailable"), None);
        assert_eq!(
            gpu_percent("{\"Device Utilization %\"=42,\"other\"=99}"),
            Some(42.0)
        );
        assert_eq!(pressure_label("1\n"), Some("Normal"));
        assert_eq!(pressure_label("2"), Some("Elevated"));
        assert_eq!(pressure_label("4"), Some("Critical"));
        assert_eq!(pressure_label("unknown"), None);
    }
}
