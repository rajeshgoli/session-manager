//! Set-wide pressure eligibility and deterministic recovery-cost ranking.
use super::*;
use anyhow::{ensure, Context};
use std::{collections::BTreeSet, fs::File, io::Read, path::Path};

#[derive(Clone, Debug)]
pub struct Candidate {
    pub root: ArtifactRoot,
    pub name: String,
    pub set_id: String,
    pub action: &'static str,
    pub freed: u64,
    pub wait_minutes: f64,
}
impl Candidate {
    pub fn score(&self) -> f64 {
        if self.action == "moving" {
            f64::INFINITY
        } else {
            self.freed as f64 / 1e9 / self.wait_minutes.max(0.1)
        }
    }
}
pub fn sort(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| {
        b.score()
            .total_cmp(&a.score())
            .then(b.freed.cmp(&a.freed))
            .then(a.name.cmp(&b.name))
            .then(a.root.path.cmp(&b.root.path))
            .then(a.action.cmp(b.action))
    });
}

/// Shared with the deletion transaction: a ranking never authorizes a stale delete.
pub fn deletion_eligible(conn: &Connection, root: &str, members: &[Value]) -> Result<()> {
    let set = members.first().context("empty set")?["set_id"]
        .as_str()
        .context("set id")?;
    for a in members {
        let name = a["name"].as_str().context("name")?;
        ensure!(
            matches!(a["state"].as_str(), Some("live" | "frozen")),
            "{name} is not live or frozen"
        );
        ensure!(
            a["recipe_complete"] == 1 && !store::has_gpu(a),
            "{name} has no CPU-only complete recipe"
        );
        ensure!(a["project"] != 1, "{name} is kept for the project");
        ensure!(
            a["keep_live_until"]
                .as_str()
                .is_none_or(|s| s < now().as_str()),
            "{name} is kept live"
        );
        ensure_not_in_use(conn, root, name)?;
        let failed_today: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM artifact_events WHERE root=?1 AND name=?2 AND kind='op_failed' AND at>=?3)",params![root,name,format!("{}T00:00:00Z",OffsetDateTime::now_utc().date())],|r|r.get(0))?;
        ensure!(!failed_today, "{name} operation failed today");
        let blocked: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM artifact_inputs i JOIN artifacts a ON a.root=i.root AND a.name=i.name WHERE i.root=?1 AND i.input=?2 AND a.state!='deleted' AND a.set_id<>?3)", params![root,name,set], |r|r.get(0))?;
        ensure!(!blocked, "{name} is an input of another artifact");
        let missing: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM artifact_inputs i LEFT JOIN artifacts a ON a.root=i.root AND a.name=i.input WHERE i.root=?1 AND i.name=?2 AND (a.name IS NULL OR a.state='deleted'))", params![root,name], |r|r.get(0))?;
        ensure!(!missing, "{name} has a deleted or missing input");
        let failed: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM artifact_rebuilds WHERE root=?1 AND set_id=?2 AND state='failed' AND started_at>COALESCE((SELECT MAX(started_at) FROM artifact_rebuilds WHERE root=?1 AND set_id=?2 AND state='succeeded'),''))",params![root,set],|r|r.get(0))?;
        ensure!(!failed, "{name} has a failed rebuild");
    }
    Ok(())
}

pub fn ratio(manager: &ManagedArtifacts, root: &ArtifactRoot, a: &Value) -> Result<f64> {
    let name = a["name"].as_str().context("name")?;
    let base = if a["location"] == "cold" {
        &root.cold_store
    } else {
        &root.path
    };
    let path = Path::new(base).join(name);
    let size = scan::measure(&path)?;
    let stamp = size.stamp();
    let conn = manager.store.open()?;
    let key = format!("ratio_stamp:{}:{name}", root.path);
    if store::meta(&conn, &key)?.as_deref() == Some(&stamp) {
        if let Some(r) = a["ratio_estimate"]
            .as_f64()
            .filter(|r| r.is_finite() && *r > 0.0)
        {
            return Ok(r);
        }
    }
    const LIMIT: u64 = 256 * 1024 * 1024;
    let files: Vec<_> = archive::inventory(&path)?
        .into_iter()
        .filter(|e| e.kind == "file")
        .collect();
    let k = size.data_bytes.div_ceil(LIMIT).max(1) as usize;
    let mut sample = Vec::new();
    for entry in files.iter().step_by(k) {
        let remaining = LIMIT.saturating_sub(sample.len() as u64);
        if remaining == 0 {
            break;
        }
        File::open(path.join(&entry.path))?
            .take(remaining)
            .read_to_end(&mut sample)?;
    }
    let compressed = zstd::stream::encode_all(sample.as_slice(), manager.config.freeze_level)?;
    let ratio = if sample.is_empty() {
        1.0
    } else {
        sample.len() as f64 / compressed.len().max(1) as f64
    };
    ensure!(
        scan::measure(&path)?.stamp() == stamp,
        "{name} changed during ratio sampling"
    );
    conn.execute("UPDATE artifacts SET ratio_estimate=?3,ratio_sampled_at=?4,content_stamp=?5,disk_bytes=?6,data_bytes=?7,size_measured_at=?4 WHERE root=?1 AND name=?2",params![root.path,name,ratio,now(),stamp,size.disk_bytes,size.data_bytes])?;
    store::set_meta(&conn, &key, &stamp)?;
    Ok(ratio)
}

pub fn candidates(manager: &ManagedArtifacts, device: u64) -> Result<Vec<Candidate>> {
    let conn = manager.store.open()?;
    let mut result = Vec::new();
    for root in &manager.config.roots {
        let root_dev = queue::fs_stats(&root.path, manager.config.floor_percent)?.2;
        let mounted = moves::cold_mounted(root);
        let cold_dev = if mounted {
            queue::fs_stats(&root.cold_store, manager.config.floor_percent)
                .ok()
                .map(|s| s.2)
        } else {
            None
        };
        let mut seen = BTreeSet::new();
        for a in manager.store.list(&root.path)? {
            let set = a["set_id"].as_str().context("set id")?;
            if !seen.insert(set.to_owned()) {
                continue;
            }
            let mut members = store::query(
                &conn,
                "SELECT * FROM artifacts WHERE root=?1 AND set_id=?2 ORDER BY name",
                &[&root.path, &set],
            )?;
            if members.iter().any(|m| {
                !m["op"].is_null()
                    || ensure_not_in_use(&conn, &root.path, m["name"].as_str().unwrap()).is_err()
            }) {
                continue;
            }
            // Refresh estimates before ranking. A failed/changed sample excludes freezing,
            // never makes deletion safe.
            let mut sampled = true;
            for m in &mut members {
                if m["state"] == "live" {
                    match ratio(manager, root, m) {
                        Ok(_) => *m = artifact(&conn, &root.path, m["name"].as_str().unwrap())?,
                        Err(_) => sampled = false,
                    }
                }
            }
            let on_device = |m: &Value| {
                if m["state"] == "frozen" || m["location"] == "cold" {
                    cold_dev == Some(device)
                } else {
                    root_dev == device
                }
            };
            let freed: u64 = members
                .iter()
                .filter(|m| on_device(m))
                .map(|m| {
                    if m["state"] == "frozen" {
                        m["frozen_bytes"].as_u64().unwrap_or(0)
                    } else {
                        m["disk_bytes"].as_u64().unwrap_or(0)
                    }
                })
                .sum();
            if freed == 0 {
                continue;
            }
            let make = |action, freed, wait_minutes| Candidate {
                root: root.clone(),
                name: members[0]["name"].as_str().unwrap().into(),
                set_id: set.into(),
                action,
                freed,
                wait_minutes,
            };
            let live = members.iter().all(|m| m["state"] == "live");
            let cold_disk = if mounted {
                let (free, floor, dev) =
                    queue::fs_stats(&root.cold_store, manager.config.floor_percent)?;
                (free, floor, queue::outstanding(&conn, manager, dev)?)
            } else {
                (0, 0, 0)
            };
            let cold_available = cold_disk
                .0
                .saturating_sub(cold_disk.1)
                .saturating_sub(cold_disk.2);
            let move_bytes: u64 = members
                .iter()
                .map(|m| m["disk_bytes"].as_u64().unwrap_or(0))
                .sum();
            if device == root_dev
                && mounted
                && live
                && members.iter().all(|m| m["location"] == "root")
                && cold_available >= move_bytes
            {
                result.push(make("moving", freed, 0.0));
            }
            if live
                && mounted
                && sampled
                && members.iter().all(|m| {
                    m["project"] != 1
                        && m["keep_live_until"]
                            .as_str()
                            .is_none_or(|s| s < now().as_str())
                })
            {
                let (mut kept, mut borrowed) = (0u64, 0u64);
                for m in &members {
                    let archive = (m["data_bytes"].as_f64().unwrap_or(0.0)
                        / m["ratio_estimate"].as_f64().unwrap_or(1.0)
                        * 1.2)
                        .ceil() as u64;
                    let (k, b) = if m["location"] == "cold" || cold_dev == Some(root_dev) {
                        ops::split_archive(archive, m["disk_bytes"].as_u64().unwrap_or(0))
                    } else {
                        (archive, 0)
                    };
                    kept = kept.saturating_add(k);
                    borrowed = borrowed.saturating_add(b);
                }
                let savings: f64 = members
                    .iter()
                    .filter(|m| on_device(m))
                    .map(|m| {
                        m["disk_bytes"].as_f64().unwrap_or(0.0)
                            - m["data_bytes"].as_f64().unwrap_or(0.0)
                                / m["ratio_estimate"].as_f64().unwrap_or(1.0)
                    })
                    .sum();
                if savings > 0.0
                    && ops::fits_on_cold(cold_disk.0, cold_disk.1, cold_disk.2, kept, borrowed)
                {
                    let seconds: f64 = members
                        .iter()
                        .map(|m| m["data_bytes"].as_f64().unwrap_or(0.0))
                        .sum::<f64>()
                        / store::thaw_rate(&conn, &root.path)?;
                    result.push(make("freezing", savings as u64, seconds / 60.0));
                }
            }
            if (mounted
                || members
                    .iter()
                    .all(|m| m["state"] != "frozen" && m["location"] != "cold"))
                && deletion_eligible(&conn, &root.path, &members).is_ok()
            {
                let seconds: f64 = conn.query_row("SELECT COALESCE(SUM(seconds),0) FROM (SELECT j.job_id,MAX(j.wall_seconds) seconds FROM artifact_jobs j JOIN artifacts a ON a.root=j.root AND a.name=j.name WHERE a.root=?1 AND a.set_id=?2 AND j.state='succeeded' GROUP BY j.job_id)",params![root.path,set],|r|r.get(0))?;
                let input_bytes: f64 = conn.query_row("SELECT COALESCE(SUM(data_bytes),0) FROM artifacts WHERE root=?1 AND state='frozen' AND name IN (SELECT i.input FROM artifact_inputs i JOIN artifacts a ON a.root=i.root AND a.name=i.name WHERE a.root=?1 AND a.set_id=?2)",params![root.path,set],|r|r.get(0))?;
                result.push(make(
                    "deleting",
                    freed,
                    (seconds + input_bytes / store::thaw_rate(&conn, &root.path)?) / 60.0,
                ));
            }
        }
    }
    sort(&mut result);
    Ok(result)
}
