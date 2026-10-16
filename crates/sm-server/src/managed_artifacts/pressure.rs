//! One pressure pass per physical filesystem, shared across roots and handovers.
use super::*;
use anyhow::Context;
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
};

pub fn enough(
    conn: &Connection,
    manager: &ManagedArtifacts,
    device: u64,
    floor: u64,
) -> Result<u64> {
    let since = (OffsetDateTime::now_utc()
        - time::Duration::days(manager.config.enough_window_days))
    .format(&Rfc3339)?;
    let mut result = floor;
    for root in &manager.config.roots {
        if queue::fs_stats(&root.path, manager.config.floor_percent)?.2 == device {
            let bytes: u64 = conn.query_row("SELECT COALESCE(MAX(bytes_written),0) FROM artifact_completions WHERE root=?1 AND placement='root' AND state='succeeded' AND finished_at>=?2",params![root.path,since],|r|r.get(0))?;
            result = result.max(bytes);
        }
        if moves::cold_mounted(root)
            && queue::fs_stats(&root.cold_store, manager.config.floor_percent)?.2 == device
        {
            let bytes:u64=conn.query_row("SELECT COALESCE(MAX(data_bytes),0) FROM artifacts WHERE root=?1 AND state='frozen'",[&root.path],|r|r.get(0))?;
            result = result.max(bytes);
        }
    }
    Ok(result)
}
pub fn target(enough: u64, floor: u64, waiting: u64) -> u64 {
    enough.max(floor.saturating_add(waiting))
}
pub fn satisfied(free: u64, outstanding: u64, target: u64, simulated: u64) -> bool {
    free.saturating_add(simulated).saturating_sub(outstanding) >= target
}
fn waiting(conn: &Connection, manager: &ManagedArtifacts, device: u64) -> Result<u64> {
    let mut largest = 0;
    for root in &manager.config.roots {
        if queue::fs_stats(&root.path, manager.config.floor_percent)?.2 != device
            && !(moves::cold_mounted(root)
                && queue::fs_stats(&root.cold_store, manager.config.floor_percent)?.2 == device)
        {
            continue;
        }
        let role = if queue::fs_stats(&root.path, manager.config.floor_percent)?.2 == device {
            "root"
        } else {
            "cold"
        };
        let n:u64=conn.query_row("SELECT COALESCE(MAX(r.disk_bytes),0) FROM artifact_job_reservations r JOIN artifact_disk_holds h USING(job_id) WHERE r.root=?1 AND r.start_bytes IS NULL AND h.role IN ('either',?2)",params![root.path,role],|r|r.get(0))?;
        largest = largest.max(n);
    }
    Ok(largest)
}

pub fn pass(manager: &ManagedArtifacts, path: &str, needed: u64) -> Result<()> {
    let (_, _, device) = queue::fs_stats(path, manager.config.floor_percent)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(
            manager
                .store
                .path
                .with_extension(format!("pressure-{device}.lock")),
        )?;
    // SAFETY: lock owns the descriptor throughout the pass. Competing triggers
    // wait here without a database transaction and remeasure after acquiring it.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let (initial, _, _) = queue::fs_stats(path, manager.config.floor_percent)?;
    let conn = manager.store.open()?;
    for root in &manager.config.roots {
        let stale = store::meta(&conn, &format!("evaluated:{}", root.path))?
            .and_then(|s| OffsetDateTime::parse(&s, &Rfc3339).ok())
            .is_none_or(|at| (OffsetDateTime::now_utc() - at).whole_hours() >= 1);
        if stale {
            hotcold::evaluate(manager, root, &crate::http::claims::GhCliWorkItemSource)?;
        }
    }
    let (_, floor, _) = queue::fs_stats(path, manager.config.floor_percent)?;
    let enough_target = enough(&conn, manager, device, floor)?;
    let universe: BTreeSet<_> = score::candidates(manager, device)?
        .into_iter()
        .map(|c| (c.root.path, c.set_id, c.action.to_owned()))
        .collect();
    let mut attempted = BTreeSet::new();
    let mut simulated = 0u64;
    loop {
        let (free, floor, _) = queue::fs_stats(path, manager.config.floor_percent)?;
        let outstanding = queue::outstanding(&conn, manager, device)?;
        let goal =
            target(enough_target, floor, waiting(&conn, manager, device)?).max(if needed > 0 {
                initial.saturating_add(needed).saturating_sub(outstanding)
            } else {
                0
            });
        if satisfied(free, outstanding, goal, simulated) {
            store::set_meta(&conn, &format!("pressure_pending:{device}"), "null")?;
            return Ok(());
        }
        let next = score::candidates(manager, device)?.into_iter().find(|c| {
            let key = (c.root.path.clone(), c.set_id.clone(), c.action.to_owned());
            universe.contains(&key) && !attempted.contains(&key)
        });
        let Some(c) = next else {
            let mut rows = Vec::new();
            for root in &manager.config.roots {
                for a in manager.store.list(&root.path)? {
                    if a["state"] == "deleted" {
                        continue;
                    }
                    let base = if a["state"] == "frozen" || a["location"] == "cold" {
                        &root.cold_store
                    } else {
                        &root.path
                    };
                    if std::fs::metadata(base).is_ok_and(|m| m.dev() == device) {
                        let name = a["name"].as_str().context("name")?;
                        let reason = if let Err(e) = ensure_not_in_use(&conn, &root.path, name) {
                            e.to_string()
                        } else if store::has_gpu(&a) {
                            format!("GPU output ({} h, ${})", a["gpu_hours"], a["gpu_cost_usd"])
                        } else if a["recipe_complete"] != 1 {
                            "no recipe".into()
                        } else if a["project"] == 1 {
                            "kept for the project".into()
                        } else {
                            "retained, busy, or no eligible action".into()
                        };
                        let bytes = if a["state"] == "frozen" {
                            a["frozen_bytes"].as_u64().unwrap_or(0)
                        } else {
                            a["disk_bytes"].as_u64().unwrap_or(0)
                        };
                        rows.push((bytes,format!("- {name}: {:.1} GB — {reason}. [Open artifact](/artifacts?name={name})",bytes as f64/1e9)));
                    }
                }
            }
            rows.sort_by_key(|row| std::cmp::Reverse(row.0));
            let holds = store::query(&conn,"SELECT h.root,h.role,h.label,r.disk_bytes FROM artifact_disk_holds h JOIN artifact_job_reservations r USING(job_id) WHERE r.start_bytes IS NULL",&[])?;
            let mut labels = Vec::new();
            for h in holds {
                if let Some(root) = manager.config.roots.iter().find(|r| h["root"] == r.path) {
                    let role =
                        if queue::fs_stats(&root.path, manager.config.floor_percent)?.2 == device {
                            "root"
                        } else if moves::cold_mounted(root)
                            && queue::fs_stats(&root.cold_store, manager.config.floor_percent)?.2
                                == device
                        {
                            "cold"
                        } else {
                            continue;
                        };
                    if h["role"] == role || h["role"] == "either" {
                        labels.push(format!(
                            "job {} ({:.1} GB)",
                            h["label"],
                            h["disk_bytes"].as_f64().unwrap_or(0.0) / 1e9
                        ));
                    }
                }
            }
            let jobs = if labels.is_empty() {
                String::new()
            } else {
                format!(" for {}", labels.join(", "))
            };
            let body=format!("Artifacts: {} needs {:.1} GB more free space{jobs}. sm cannot free more by itself.\n\n{}",path,goal.saturating_add(outstanding).saturating_sub(free.saturating_add(simulated)) as f64/1e9,rows.iter().take(10).map(|r|r.1.as_str()).collect::<Vec<_>>().join("\n"));
            store::set_meta(
                &conn,
                &format!("pressure_pending:{device}"),
                &json!({"body":body}).to_string(),
            )?;
            return Ok(());
        };
        attempted.insert((c.root.path.clone(), c.set_id.clone(), c.action.to_owned()));
        if c.action == "deleting" {
            let deleted = manager.delete_pressure(&c.root, &c.name).is_ok();
            if deleted {
                store::event(
                    &conn,
                    &c.root.path,
                    &c.name,
                    "pressure_action",
                    "sm",
                    json!({"action":c.action,"freed":c.freed,"report_only":manager.config.deletion_mode=="report_only"}),
                )?;
            }
            if deleted && manager.config.deletion_mode == "report_only" {
                simulated = simulated.saturating_add(c.freed);
                // The simulated set is gone; do not count another action on it.
                for action in ["moving", "freezing"] {
                    attempted.insert((c.root.path.clone(), c.set_id.clone(), action.into()));
                }
            }
        } else if ops::request(manager, &c.root, &c.name, c.action, "sm-pressure").is_ok() {
            ops::resume(manager)?;
            store::event(
                &conn,
                &c.root.path,
                &c.name,
                "pressure_action",
                "sm",
                json!({"action":c.action,"freed":c.freed}),
            )?;
            let a = artifact(&conn, &c.root.path, &c.name)?;
            if !a["op"].is_null() {
                return Ok(());
            } // worker owns it; next trigger remeasures
        }
    }
}

pub fn periodic(manager: &ManagedArtifacts) -> Result<()> {
    let conn = manager.store.open()?;
    let mut seen = BTreeSet::new();
    for root in &manager.config.roots {
        for (cold, path) in [(false, &root.path), (true, &root.cold_store)] {
            if cold && !moves::cold_mounted(root) {
                continue;
            }
            let (free, floor, device) = queue::fs_stats(path, manager.config.floor_percent)?;
            if !seen.insert(device) {
                continue;
            }
            let key = format!("pressure_check:{device}");
            let due = store::meta(&conn, &key)?
                .and_then(|s| s.parse::<i64>().ok())
                .is_none_or(|at| {
                    OffsetDateTime::now_utc().unix_timestamp() - at
                        >= manager.config.pressure_check_seconds as i64
                });
            if due
                && (free < enough(&conn, manager, device, floor)?
                    || waiting(&conn, manager, device)? > 0)
            {
                pass(manager, path, 0)?;
                store::set_meta(
                    &conn,
                    &key,
                    &OffsetDateTime::now_utc().unix_timestamp().to_string(),
                )?;
            } else if due {
                store::set_meta(&conn, &format!("pressure_pending:{device}"), "null")?;
            }
        }
    }
    Ok(())
}
pub fn kick(manager: ManagedArtifacts, root: ArtifactRoot, needed: u64) {
    std::thread::spawn(move || {
        if let Err(error) = pass(&manager, &root.path, needed) {
            eprintln!("artifact pressure deferred: {error:#}");
        }
    });
}

/// Inbox publication only, no ordinary Needs-you item generates a message.
pub fn notify(
    manager: &ManagedArtifacts,
    at: OffsetDateTime,
    mut send: impl FnMut(&str, &str) -> Result<()>,
) -> Result<()> {
    let conn = manager.store.open()?;
    for row in store::query(
        &conn,
        "SELECT key,value FROM artifact_meta WHERE key LIKE 'pressure_pending:%'",
        &[],
    )? {
        let value: Value = serde_json::from_str(row["value"].as_str().context("pending value")?)?;
        let Some(body) = value["body"].as_str() else {
            continue;
        };
        let key = row["key"].as_str().context("pending key")?;
        let sent = format!("{key}:sent");
        if store::meta(&conn, &sent)?
            .and_then(|s| s.parse::<i64>().ok())
            .is_some_and(|last| at.unix_timestamp() - last < 21600)
        {
            continue;
        }
        send(&format!("{key}:{}", at.unix_timestamp()), body)?;
        store::set_meta(&conn, &sent, &at.unix_timestamp().to_string())?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "pressure_tests.rs"]
mod tests;

/// Resolve already-sent blockers when a later measurement clears pressure.
/// Repeating this callback is harmless; owner message handling is idempotent.
pub fn resolve_notices(
    manager: &ManagedArtifacts,
    mut resolve: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    for row in store::query(
        &manager.store.open()?,
        "SELECT key FROM artifact_meta WHERE key LIKE 'pressure_pending:%' AND value='null'",
        &[],
    )? {
        resolve(&format!(
            "{}:",
            row["key"].as_str().context("pressure key")?
        ))?;
    }
    Ok(())
}
