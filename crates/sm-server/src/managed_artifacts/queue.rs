//! Durable queue/artifact integration. The queue keeps the submission intent so
//! an interrupted cross-database commit can be repaired before starting work.
use super::*;
use anyhow::Context;
use rusqlite::OptionalExtension;
use std::{os::unix::fs::MetadataExt, path::Path};
use submission::{attach_store, PreparedSubmission};

/// Installed by the freeze/thaw and pressure workers. An absent worker leaves
/// the durable request pending; admission never assumes it freed space.
pub trait Operations: Send + Sync {
    fn thaw(&self, manager: &ManagedArtifacts, root: &ArtifactRoot, name: &str) -> Result<()>;
    fn pressure(&self, manager: &ManagedArtifacts, root: &ArtifactRoot, needed: u64) -> Result<()>;
}
fn operations(
) -> &'static std::sync::Mutex<std::collections::BTreeMap<String, std::sync::Arc<dyn Operations>>> {
    static OPS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<String, std::sync::Arc<dyn Operations>>>,
    > = std::sync::OnceLock::new();
    OPS.get_or_init(Default::default)
}
pub fn install_operations(root: &str, worker: std::sync::Arc<dyn Operations>) {
    operations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(root.to_owned(), worker);
}
fn worker(root: &str) -> Option<std::sync::Arc<dyn Operations>> {
    operations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(root)
        .cloned()
}

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS queue_artifact_submissions(job_id TEXT PRIMARY KEY, config_json TEXT NOT NULL, prepared_json TEXT NOT NULL, warnings_json TEXT NOT NULL DEFAULT '[]', first_hold_at TEXT, last_pressure_at TEXT, completed INTEGER NOT NULL DEFAULT 0); CREATE TABLE IF NOT EXISTS queue_artifact_context(id INTEGER PRIMARY KEY CHECK(id=1),config_json TEXT NOT NULL);")?;
    Ok(())
}
pub fn save(
    tx: &rusqlite::Transaction<'_>,
    manager: &ManagedArtifacts,
    prepared: &PreparedSubmission,
    id: &str,
) -> Result<()> {
    let warnings = prepared.persist(tx, id)?;
    for input in store::query(
        tx,
        "SELECT inputs_json FROM artifact_job_reservations WHERE job_id=?1",
        &[&id],
    )? {
        for name in names(&input, "inputs_json")? {
            if artifact(tx, &prepared.root.path, &name)?["state"] == "frozen" {
                request_operation(tx, &prepared.root.path, &name, "thaw_requested", id)?;
            }
        }
    }
    tx.execute("INSERT INTO queue_artifact_submissions(job_id,config_json,prepared_json,warnings_json) VALUES(?1,?2,?3,?4)",params![id,serde_json::to_string(&manager.config)?,serde_json::to_string(prepared)?,serde_json::to_string(&warnings)?])?;
    Ok(())
}
fn load(conn: &Connection, id: &str) -> Result<Option<(ManagedArtifacts, PreparedSubmission)>> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT config_json,prepared_json FROM queue_artifact_submissions WHERE job_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(config, prepared)| {
        let config = serde_json::from_str(&config)?;
        let manager = ManagedArtifacts::new(&config)?;
        attach_store(conn, &manager.store)?;
        Ok((manager, serde_json::from_str(&prepared)?))
    })
    .transpose()
}

/// Operations implemented by the freeze/thaw and pressure tickets. Requests
/// remain durable in artifact_events until their worker is installed.
pub fn request_operation(
    conn: &Connection,
    root: &str,
    name: &str,
    kind: &str,
    job: &str,
) -> Result<()> {
    let previous: i64 = conn.query_row(
        "SELECT COUNT(*) FROM artifact_events WHERE root=?1 AND name=?2 AND kind=?3 AND actor=?4",
        params![root, name, kind, job],
        |r| r.get(0),
    )?;
    if previous == 0 {
        event(conn, root, name, kind, job, json!({"job_id":job}))?;
    }
    Ok(())
}

pub fn repair(conn: &Connection, messages: Option<&Path>) -> Result<()> {
    let config: Option<String> = conn
        .query_row(
            "SELECT config_json FROM queue_artifact_context WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let Some(config) = config else { return Ok(()) };
    let manager = ManagedArtifacts::new(&serde_json::from_str(&config)?)?;
    attach_store(conn, &manager.store)?;
    // Artifact-side-only submission commits must not retain in-use protection
    // forever. Keep the records and failure history, never delete their files.
    let tx = conn.unchecked_transaction()?;
    tx.execute("UPDATE artifact_jobs SET state='failed' WHERE job_id IN (SELECT r.job_id FROM artifact_job_reservations r LEFT JOIN queue_jobs q ON q.id=r.job_id WHERE q.id IS NULL)",[])?;
    tx.execute(
        "DELETE FROM artifact_job_reservations WHERE job_id NOT IN (SELECT id FROM queue_jobs)",
        [],
    )?;
    tx.commit()?;
    let rows=store::query(conn,"SELECT s.job_id,j.state FROM queue_artifact_submissions s JOIN queue_jobs j ON j.id=s.job_id WHERE s.completed=0 OR EXISTS(SELECT 1 FROM artifact_job_reservations r WHERE r.job_id=s.job_id)",&[])?;
    for row in rows {
        let id = row["job_id"].as_str().context("job id")?;
        let Some((_, prepared)) = load(conn, id)? else {
            continue;
        };
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM artifact_job_reservations WHERE job_id=?1",
            [id],
            |r| r.get(0),
        )?;
        if exists == 0 && row["state"] == "pending" {
            let tx = conn.unchecked_transaction()?;
            prepared.persist(&tx, id)?;
            tx.commit()?;
        }
        if let Some(messages) = messages {
            if let Err(error) = submission_notices(conn, id, messages) {
                eprintln!("artifact submission notices deferred for {id}: {error:#}");
            }
        }
        if !matches!(row["state"].as_str(), Some("pending" | "running")) {
            if let Err(error) = complete(conn, id, messages) {
                eprintln!("artifact completion deferred for {id}: {error:#}");
            }
        }
    }
    Ok(())
}

pub fn remaining(conn: &Connection, id: &str, _reason: Option<&str>) -> Result<Option<i64>> {
    // Once granted, the artifact wait window survives temporary scheduler
    // holds. The current reason describes the blocker, not a new deadline.
    let row:Option<(String,String)>=conn.query_row("SELECT first_hold_at,config_json FROM queue_artifact_submissions WHERE job_id=?1 AND first_hold_at IS NOT NULL",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    row.map(|(at, config)| {
        let config: ManagedArtifactsConfig = serde_json::from_str(&config)?;
        let elapsed =
            (OffsetDateTime::now_utc() - OffsetDateTime::parse(&at, &Rfc3339)?).whole_seconds();
        Ok((config
            .disk_hold_max_wait_hours
            .saturating_mul(3600)
            .min(i64::MAX as u64) as i64)
            .saturating_sub(elapsed))
    })
    .transpose()
}

pub fn fits(free: u64, floor: u64, outstanding: u64, disk: u64) -> bool {
    floor
        .checked_add(outstanding)
        .and_then(|n| n.checked_add(disk))
        .is_some_and(|needed| free >= needed)
}
fn names(row: &Value, key: &str) -> Result<Vec<String>> {
    Ok(serde_json::from_str(
        row[key].as_str().context("reservation names")?,
    )?)
}
fn size(conn: &Connection, root: &ArtifactRoot, outputs: &[String]) -> Result<u64> {
    let mut total = 0u64;
    for name in outputs {
        let a = artifact(conn, &root.path, name)?;
        let base = if a["location"] == "cold" {
            &root.cold_store
        } else {
            &root.path
        };
        let path = Path::new(base).join(name);
        match path.symlink_metadata() {
            Ok(_) => total = total.saturating_add(scan::measure(&path)?.disk_bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(total)
}
fn fs_stats(path: &str, percent: u64) -> Result<(u64, u64, u64)> {
    let path = Path::new(path);
    let stat = nix::sys::statvfs::statvfs(path)?;
    Ok((
        u64::from(stat.blocks_available()).saturating_mul(stat.fragment_size()),
        u64::from(stat.blocks())
            .saturating_mul(stat.fragment_size())
            .saturating_mul(percent)
            / 100,
        std::fs::metadata(path)?.dev(),
    ))
}
fn outstanding(conn: &Connection, manager: &ManagedArtifacts, device: u64) -> Result<u64> {
    let mut result = 0u64;
    for r in store::query(
        conn,
        "SELECT * FROM artifact_job_reservations WHERE start_bytes IS NOT NULL",
        &[],
    )? {
        let root = manager
            .config
            .roots
            .iter()
            .find(|root| r["root"] == root.path)
            .context("reservation root")?;
        let base = if r["placed"] == "cold" {
            &root.cold_store
        } else {
            &root.path
        };
        let Ok(metadata) = std::fs::metadata(base) else {
            continue;
        };
        if metadata.dev() != device {
            continue;
        }
        let written =
            cached_size(conn, root, &r)?.saturating_sub(r["start_bytes"].as_u64().unwrap_or(0));
        result = result.saturating_add(
            r["disk_bytes"]
                .as_u64()
                .unwrap_or(0)
                .saturating_sub(written),
        );
    }
    Ok(result)
}
fn cached_size(conn: &Connection, root: &ArtifactRoot, reservation: &Value) -> Result<u64> {
    let id = reservation["job_id"].as_str().context("job id")?;
    let key = format!("queue-size:{id}");
    if let Some(value) = store::meta(conn, &key)? {
        let (at, bytes): (i64, u64) = serde_json::from_str(&value)?;
        if OffsetDateTime::now_utc()
            .unix_timestamp()
            .saturating_sub(at)
            < 300
        {
            return Ok(bytes);
        }
    }
    let bytes = size(conn, root, &names(reservation, "outputs_json")?)?;
    store::set_meta(
        conn,
        &key,
        &serde_json::to_string(&(OffsetDateTime::now_utc().unix_timestamp(), bytes))?,
    )?;
    Ok(bytes)
}

pub fn submission_notices(conn: &Connection, id: &str, db: &Path) -> Result<()> {
    let Some((manager, p)) = load(conn, id)? else {
        return Ok(());
    };
    if let Some(worker) = worker(&p.root.path) {
        let reservation = store::query(
            conn,
            "SELECT inputs_json FROM artifact_job_reservations WHERE job_id=?1",
            &[&id],
        )?
        .pop();
        if let Some(reservation) = reservation {
            for input in names(&reservation, "inputs_json")? {
                if artifact(conn, &p.root.path, &input)?["state"] == "frozen" {
                    worker.thaw(&manager, &p.root, &input)?;
                }
            }
        }
    }
    let warnings: String = conn.query_row(
        "SELECT warnings_json FROM queue_artifact_submissions WHERE job_id=?1",
        [id],
        |r| r.get(0),
    )?;
    for (index, warning) in serde_json::from_str::<Vec<String>>(&warnings)?
        .iter()
        .enumerate()
    {
        notify(
            Some(db),
            &format!("artifact-input-{id}-{index}"),
            &p.request.notify_session_id,
            warning,
        )?;
    }
    Ok(())
}

fn hold(conn: &Connection, id: &str, reason: &str) -> Result<Option<String>> {
    conn.execute("UPDATE queue_artifact_submissions SET first_hold_at=COALESCE(first_hold_at,?2) WHERE job_id=?1",params![id,now()])?;
    conn.execute("UPDATE artifact_job_reservations SET first_disk_hold_at=COALESCE(first_disk_hold_at,?2) WHERE job_id=?1",params![id,now()])?;
    Ok(Some(reason.into()))
}

/// Called only after ordinary admission rules, including perf, pass. Output
/// creation and reservation happen before process spawn, including forced starts.
pub fn admit(conn: &Connection, id: &str) -> Result<Option<String>> {
    let Some((manager, p)) = load(conn, id)? else {
        return Ok(None);
    };
    let r = store::query(
        conn,
        "SELECT * FROM artifact_job_reservations WHERE job_id=?1",
        &[&id],
    )?
    .pop()
    .context("artifact reservation missing; recover before admission")?;
    let outputs = names(&r, "outputs_json")?;
    for active in store::query(conn,"SELECT outputs_json FROM artifact_job_reservations WHERE root=?1 AND job_id<>?2 AND start_bytes IS NOT NULL",&[&p.root.path,&id])? {
        if names(&active,"outputs_json")?.iter().any(|other|outputs.iter().any(|name|name.eq_ignore_ascii_case(other))) {
            return Ok(Some("artifact_writer".into()));
        }
    }
    for input in names(&r, "inputs_json")? {
        let a = artifact(conn, &p.root.path, &input)?;
        if a["state"] == "frozen" || a["op"] == "thawing" {
            request_operation(conn, &p.root.path, &input, "thaw_requested", id)?;
            if let Some(worker) = worker(&p.root.path) {
                if let Err(error) = worker.thaw(&manager, &p.root, &input) {
                    eprintln!("artifact thaw deferred for {id}: {error:#}");
                }
            }
            return hold(conn, id, "thawing");
        }
    }
    let disk = r["disk_bytes"].as_u64().unwrap_or(0);
    let outputs = names(&r, "outputs_json")?;
    let mut existing = None;
    for name in &outputs {
        let a = artifact(conn, &p.root.path, name)?;
        if Path::new(&p.root.path)
            .join(name)
            .symlink_metadata()
            .is_ok()
        {
            let location = a["location"].as_str().context("location")?;
            if existing.as_deref().is_some_and(|old| old != location) {
                bail!("outputs span filesystems; submit separate jobs");
            }
            existing = Some(location.to_owned());
        }
    }
    let mut placement = None;
    let root_device = std::fs::metadata(&p.root.path)?.dev();
    for (location, path) in [("root", &p.root.path), ("cold", &p.root.cold_store)] {
        if existing.as_deref().is_some_and(|old| old != location) {
            continue;
        }
        let Ok((free, floor, device)) = fs_stats(path, manager.config.floor_percent) else {
            continue;
        };
        if location == "cold" && device == root_device {
            continue;
        }
        if fits(free, floor, outstanding(conn, &manager, device)?, disk) {
            placement = Some(location);
            break;
        }
        if location == "root" {
            let last: Option<String> = conn.query_row(
                "SELECT last_pressure_at FROM queue_artifact_submissions WHERE job_id=?1",
                [id],
                |r| r.get(0),
            )?;
            let due = last
                .and_then(|s| OffsetDateTime::parse(&s, &Rfc3339).ok())
                .is_none_or(|at| (OffsetDateTime::now_utc() - at).whole_seconds() >= 600);
            if due {
                event(
                    conn,
                    &p.root.path,
                    "",
                    "pressure_requested",
                    id,
                    json!({"job_id":id,"disk_bytes":disk}),
                )?;
                conn.execute(
                    "UPDATE queue_artifact_submissions SET last_pressure_at=?2 WHERE job_id=?1",
                    params![id, now()],
                )?;
                if let Some(worker) = worker(&p.root.path) {
                    let required = floor
                        .saturating_add(outstanding(conn, &manager, device)?)
                        .saturating_add(disk);
                    if let Err(error) =
                        worker.pressure(&manager, &p.root, required.saturating_sub(free))
                    {
                        eprintln!("artifact pressure pass deferred for {id}: {error:#}");
                    }
                    let (free, floor, device) = fs_stats(path, manager.config.floor_percent)?;
                    if fits(free, floor, outstanding(conn, &manager, device)?, disk) {
                        placement = Some(location);
                        break;
                    }
                }
            }
        }
    }
    let Some(location) = placement else {
        return hold(conn, id, "disk_space");
    };
    let tx = conn.unchecked_transaction()?;
    for name in &outputs {
        let a = artifact(&tx, &p.root.path, name)?;
        if a["state"] != "building" || !a["op"].is_null() {
            bail!("output {name} is no longer available");
        }
        let base = if location == "cold" {
            &p.root.cold_store
        } else {
            &p.root.path
        };
        let physical = Path::new(base).join(name);
        let logical = Path::new(&p.root.path).join(name);
        if physical.symlink_metadata().is_err() {
            std::fs::create_dir(&physical)?;
        }
        let metadata = physical.symlink_metadata()?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!("output {name} is not a managed directory");
        }
        if location == "cold" {
            if logical.symlink_metadata().is_err() {
                std::os::unix::fs::symlink(&physical, &logical)?;
            }
            if std::fs::canonicalize(&logical)? != physical {
                bail!("output {name} has an unexpected symlink");
            }
        }
        tx.execute(
            "UPDATE artifacts SET location=?3 WHERE root=?1 AND name=?2",
            params![p.root.path, name, location],
        )?;
    }
    let start = size(&tx, &p.root, &outputs)?;
    tx.execute(
        "UPDATE artifact_job_reservations SET placed=?2,start_bytes=?3 WHERE job_id=?1",
        params![id, location, start],
    )?;
    tx.execute(
        "UPDATE artifact_jobs SET state='running' WHERE job_id=?1",
        [id],
    )?;
    tx.commit()?;
    Ok(None)
}

fn notify(db: Option<&Path>, id: &str, target: &str, text: &str) -> Result<()> {
    let db = db.context("message database required for artifact notification")?;
    crate::queue::RetainedQueueStore::new(db.to_path_buf()).enqueue_message_once_with_metadata(
        id,
        target,
        text,
        "sequential",
        crate::queue::QueueMessageMetadata {
            message_category: Some("queue-completion".into()),
            ..Default::default()
        },
    )?;
    Ok(())
}
pub fn check_overrun(conn: &Connection, id: &str, db: &Path) -> Result<()> {
    let Some((manager, p)) = load(conn, id)? else {
        return Ok(());
    };
    let Some(r)=store::query(conn,"SELECT * FROM artifact_job_reservations WHERE job_id=?1 AND start_bytes IS NOT NULL AND overrun_warned=0",&[&id])?.pop() else {return Ok(())};
    let written = size(conn, &p.root, &names(&r, "outputs_json")?)?
        .saturating_sub(r["start_bytes"].as_u64().unwrap_or(0));
    let disk = r["disk_bytes"].as_u64().unwrap_or(0);
    if written as f64 > manager.config.overrun_factor * disk as f64 {
        notify(
            Some(db),
            &format!("artifact-overrun-{id}"),
            &p.request.notify_session_id,
            &format!(
                "[sm artifact] {} has written {written} bytes, declared {disk}",
                p.request.label
            ),
        )?;
        let tx = conn.unchecked_transaction()?;
        event(
            &tx,
            &p.root.path,
            "",
            "overrun",
            id,
            json!({"written":written,"disk":disk}),
        )?;
        tx.execute(
            "UPDATE artifact_job_reservations SET overrun_warned=1 WHERE job_id=?1",
            [id],
        )?;
        tx.commit()?;
    }
    Ok(())
}

pub fn complete(conn: &Connection, id: &str, db: Option<&Path>) -> Result<()> {
    let Some((manager, p)) = load(conn, id)? else {
        return Ok(());
    };
    let ledger = store::query(
        conn,
        "SELECT * FROM queue_artifact_submissions WHERE job_id=?1",
        &[&id],
    )?
    .pop()
    .context("submission")?;
    let reservation_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_job_reservations WHERE job_id=?1)",
        [id],
        |r| r.get(0),
    )?;
    if !reservation_exists {
        // Artifact-side completion may already have committed. Preserve its
        // measured bytes and records when repairing the queue-side marker.
        conn.execute(
            "UPDATE queue_artifact_submissions SET completed=1 WHERE job_id=?1",
            [id],
        )?;
        return Ok(());
    }
    let job = store::query(conn, "SELECT * FROM queue_jobs WHERE id=?1", &[&id])?
        .pop()
        .context("queue job")?;
    let warnings: Vec<String> =
        serde_json::from_str(ledger["warnings_json"].as_str().context("warnings")?)?;
    for (index, warning) in warnings.iter().enumerate() {
        notify(
            db,
            &format!("artifact-input-{id}-{index}"),
            &p.request.notify_session_id,
            warning,
        )?;
    }
    let tx = conn.unchecked_transaction()?;
    let reservation = store::query(
        &tx,
        "SELECT * FROM artifact_job_reservations WHERE job_id=?1",
        &[&id],
    )?
    .pop();
    let wall = job["started_at"]
        .as_str()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .zip(
            job["finished_at"]
                .as_str()
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok()),
        )
        .map_or(0, |(start, end)| (end - start).whole_seconds().max(0));
    let outputs = &p.declarations.outputs;
    let written = if let Some(r) = &reservation {
        size(&tx, &p.root, &names(r, "outputs_json")?)?
            .saturating_sub(r["start_bytes"].as_u64().unwrap_or(0))
    } else {
        0
    };
    tx.execute("UPDATE artifact_jobs SET state=?2,exit_code=?3,wall_seconds=?4,bytes_written=?5 WHERE job_id=?1",params![id,job["state"].as_str(),job["exit_code"].as_i64(),wall,written])?;
    if let Some(r) = &reservation {
        for name in names(r, "inputs_json")?
            .into_iter()
            .chain(names(r, "outputs_json")?)
        {
            tx.execute("UPDATE artifacts SET last_used_at=CASE WHEN ?3 IS NOT NULL AND (last_used_at IS NULL OR last_used_at<?3) THEN ?3 ELSE last_used_at END WHERE root=?1 AND name=?2",params![p.root.path,name,job["started_at"].as_str()])?;
        }
    }
    for name in outputs {
        let a = artifact(&tx, &p.root.path, name)?;
        let base = if a["location"] == "cold" {
            &p.root.cold_store
        } else {
            &p.root.path
        };
        let path = Path::new(base).join(name);
        if path.exists() {
            let measured = scan::measure(&path)?;
            tx.execute("UPDATE artifacts SET disk_bytes=?3,data_bytes=?4,content_stamp=?5,size_measured_at=?6 WHERE root=?1 AND name=?2",params![p.root.path,name,measured.disk_bytes,measured.data_bytes,measured.stamp(),now()])?;
        }
    }
    tx.execute(
        "DELETE FROM artifact_meta WHERE key=?1",
        [format!("queue-size:{id}")],
    )?;
    tx.execute(
        "DELETE FROM artifact_job_reservations WHERE job_id=?1",
        [id],
    )?;
    if job["state"] == "succeeded" && !outputs.is_empty() {
        let a = artifact(&tx, &p.root.path, &outputs[0])?;
        let set = a["set_id"].as_str().context("set")?;
        let members = store::query(
            &tx,
            "SELECT name FROM artifacts WHERE root=?1 AND set_id=?2",
            &[&p.root.path, &set],
        )?;
        let members: Vec<String> = members
            .iter()
            .map(|a| a["name"].as_str().unwrap().to_owned())
            .collect();
        let busy = members
            .iter()
            .any(|name| super::ensure_not_in_use(&tx, &p.root.path, name).is_err());
        if !busy {
            let jobs=store::query(&tx,"SELECT DISTINCT s.prepared_json,j.seq FROM artifact_jobs j JOIN queue_artifact_submissions s ON s.job_id=j.job_id JOIN artifacts a ON a.root=j.root AND a.name=j.name WHERE a.root=?1 AND a.set_id=?2 AND j.state='succeeded' ORDER BY j.seq",&[&p.root.path,&set])?;
            let mut merged: Option<RecordFlags> = None;
            for j in jobs {
                let prepared: PreparedSubmission =
                    serde_json::from_str(j["prepared_json"].as_str().context("prepared")?)?;
                if let Some(mut flags) = prepared.declarations.record {
                    if let Some(previous) = merged {
                        flags.consumers.extend(previous.consumers);
                        flags.consumers.sort();
                        flags.consumers.dedup();
                        flags.project |= previous.project;
                        flags.recipe_complete |= previous.recipe_complete;
                        flags.keep_until = flags.keep_until.max(previous.keep_until);
                    }
                    merged = Some(flags);
                }
            }
            if merged.is_none() {
                for name in &members {
                    let today = OffsetDateTime::now_utc().date().to_string();
                    if attribution::request_due(&tx, &p.root.path, name, &today)? {
                        notify(db,&format!("artifact-record:{}:{}:{today}",p.root.path,name.to_ascii_lowercase()),&p.request.notify_session_id,&format!("[sm artifact] {name} built successfully with no record. Run sm artifact record {name} --note \"one plain-English line\" --consumer '#N'."))?;
                        event(
                            &tx,
                            &p.root.path,
                            name,
                            "request_sent",
                            "sm",
                            json!({"session":p.request.notify_session_id}),
                        )?;
                    }
                }
            }
            if let Some(mut flags) = merged {
                if flags.recipe_complete {
                    if let Err(error) =
                        super::recipe::preserve(&tx, &manager.config, &p.root, &members)
                    {
                        flags.recipe_complete = false;
                        notify(
                            db,
                            &format!("artifact-recipe-{id}"),
                            &p.request.notify_session_id,
                            &format!(
                                "[sm artifact] {} recorded without recipe-complete: {error}",
                                p.request.label
                            ),
                        )?;
                    }
                }
                for name in &members {
                    super::apply_record(&tx, &p.root, name, &flags, &p.request.notify_session_id)?;
                    tx.execute("UPDATE artifacts SET state='live',recipe_complete=?3 WHERE root=?1 AND name=?2",params![p.root.path,name,flags.recipe_complete])?;
                    event(
                        &tx,
                        &p.root.path,
                        name,
                        "recorded",
                        id,
                        json!({"job_id":id}),
                    )?;
                }
            }
        }
    }
    tx.execute(
        "UPDATE queue_artifact_submissions SET completed=1 WHERE job_id=?1",
        [id],
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests;

/// Read-only owner surface, sharing admission's reservation arithmetic.
pub fn disk_status(manager: &ManagedArtifacts, root: &ArtifactRoot) -> Result<Vec<Value>> {
    let conn = manager.store.open()?;
    let waiting=store::query(&conn,"SELECT job_id,disk_bytes,first_disk_hold_at FROM artifact_job_reservations WHERE root=?1 AND first_disk_hold_at IS NOT NULL AND start_bytes IS NULL",&[&root.path])?;
    let mut result = Vec::new();
    for (role, path) in [("root", &root.path), ("cold", &root.cold_store)] {
        match fs_stats(path,manager.config.floor_percent) {
            Ok((free,floor,device)) if role=="root" || std::fs::metadata(&root.path)?.dev()!=device => {
                let stat=nix::sys::statvfs::statvfs(Path::new(path))?;
                result.push(json!({"path":path,"role":role,"mounted":true,"bytes":u64::from(stat.blocks())*stat.fragment_size(),"free":free,"floor":floor,"enough":floor,"outstanding":outstanding(&conn,manager,device)?,"waiting_jobs":waiting}));
            }
            _=>result.push(json!({"path":path,"role":role,"mounted":false,"bytes":null,"free":null,"floor":null,"enough":null,"outstanding":0,"waiting_jobs":waiting})),
        }
    }
    Ok(result)
}
