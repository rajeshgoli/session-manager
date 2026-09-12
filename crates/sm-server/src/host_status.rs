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

pub async fn snapshot() -> Value {
    SNAPSHOT_CACHE.get(collect_snapshot).await
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
    let line = top
        .lines()
        .filter(|line| line.starts_with("CPU usage:"))
        .last()?;
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
        .last()?
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

fn gpu_percent(ioreg: &str) -> Option<f64> {
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
