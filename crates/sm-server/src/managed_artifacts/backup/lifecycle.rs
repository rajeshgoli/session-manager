//! Durable per-device request state. HTTP authenticates the device; the queue
//! owns preparation processes. Only a successful acknowledgement advances head.
use super::*;
use crate::managed_artifacts::{
    config::ManagedArtifactsConfig, now, store as artifact_store, ManagedArtifacts,
};
use anyhow::{ensure, Context};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use std::{fs, path::PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Check {
    pub ok: bool,
    pub files_checked: u64,
    #[serde(default)]
    pub bad: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Ack {
    pub applied: bool,
    pub fetched_bytes: u64,
    pub fetch_seconds: f64,
    pub apply_seconds: f64,
    pub held_deletions: u64,
    pub check: Option<Check>,
    pub error: Option<String>,
    /// The mini's self-update tonight, if it tried one (#2264). Optional: older
    /// clients never send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<super::update::UpdateReport>,
    /// The remote disk's size, holding and drive health (#2276). Optional:
    /// older clients never send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<super::disk::Disk>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestStatus {
    pub id: String,
    pub state: String,
    pub retry_after_seconds: Option<u64>,
    pub estimate_bytes: u64,
    pub manifest_id: Option<String>,
    pub parts: Vec<Part>,
    pub manifest: Option<TransferManifest>,
    pub deletions: Vec<String>,
    pub allow_deletes: bool,
    pub max_delete_fraction: f64,
    pub error: Option<String>,
    /// Free space the remote disk keeps by deleting old holding folders
    /// (#2276). Absent until the disk has reported its size, and from older
    /// servers.
    #[serde(default)]
    pub space_floor_bytes: Option<u64>,
}
#[derive(Debug)]
pub enum RequestResult {
    Open(Box<RequestStatus>),
    Mismatch(Option<String>),
}
fn epoch() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
fn request_id() -> String {
    use rand_core::RngCore;
    let mut bytes = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut bytes);
    format!(
        "backup-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}
fn row(conn: &Connection, device: &str, id: &str) -> Result<Value> {
    artifact_store::query(
        conn,
        "SELECT * FROM backup_requests WHERE device=?1 AND id=?2",
        &[&device, &id],
    )?
    .pop()
    .context("backup request not found")
}
fn status_row(r: &Value) -> Result<RequestStatus> {
    let state = r["state"].as_str().context("request state")?.to_owned();
    let ready = state == "ready";
    let manifest: Option<TransferManifest> = if ready {
        r["transfer_json"]
            .as_str()
            .map(serde_json::from_str)
            .transpose()?
    } else {
        None
    };
    let estimate = r["estimate_bytes"].as_u64().unwrap_or(0);
    Ok(RequestStatus {
        id: r["id"].as_str().context("request id")?.into(),
        retry_after_seconds: if state == "preparing" {
            Some(
                ((r["remaining_input"].as_f64().unwrap_or(0.0)
                    / r["packing_rate"].as_f64().unwrap_or(1e9))
                .ceil() as u64)
                    .saturating_add(30),
            )
        } else {
            None
        },
        state,
        estimate_bytes: estimate,
        manifest_id: r["manifest_id"].as_str().map(str::to_owned),
        parts: if ready {
            serde_json::from_str(r["parts_json"].as_str().unwrap_or("[]"))?
        } else {
            vec![]
        },
        deletions: manifest
            .as_ref()
            .map(|m| m.deleted.clone())
            .unwrap_or_default(),
        manifest,
        allow_deletes: r["allow_deletes"] == 1,
        max_delete_fraction: r["max_delete_fraction"].as_f64().unwrap_or(0.1),
        error: r["error"].as_str().map(str::to_owned),
        space_floor_bytes: None,
    })
}
fn packing_rate(conn: &Connection) -> Result<f64> {
    let mut stmt=conn.prepare("SELECT packed_input_bytes/packing_seconds FROM backup_requests WHERE packing_seconds>0 AND packed_input_bytes>0 ORDER BY ready_at DESC LIMIT 5")?;
    let mut values = stmt
        .query_map([], |r| r.get::<_, f64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    values.sort_by(f64::total_cmp);
    Ok(if values.is_empty() {
        1e9
    } else {
        (values[(values.len() - 1) / 2] + values[values.len() / 2]) / 2.0
    })
}
pub fn request(
    manager: &ManagedArtifacts,
    device: &str,
    have: Option<&str>,
) -> Result<RequestResult> {
    ensure!(manager.config.backup.enabled, "backup is disabled");
    expire(manager, epoch())?;
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(r) = artifact_store::query(
        &tx,
        "SELECT * FROM backup_requests WHERE device=?1 AND state IN ('preparing','ready')",
        &[&device],
    )?
    .pop()
    {
        let mut status = status_row(&r)?;
        status.space_floor_bytes = space_floor(&tx, manager, device)?;
        return Ok(RequestResult::Open(Box::new(status)));
    }
    let head = confirmed(&tx, device)?.map(|m| m.manifest_id);
    if have != head.as_deref() {
        return Ok(RequestResult::Mismatch(head));
    }
    let id = request_id();
    let config = serde_json::to_string(&manager.config)?;
    let outbox = crate::sessions::expand_home(&manager.config.backup.outbox).join(&id);
    tx.execute("INSERT INTO backup_requests(id,device,state,base,created_at,config_json,outbox,packing_rate,max_delete_fraction) VALUES(?1,?2,'preparing',?3,?4,?5,?6,?7,?8)",params![id,device,have,epoch(),config,outbox.to_string_lossy(),packing_rate(&tx)?,manager.config.backup.max_delete_fraction])?;
    let mut result = status_row(&row(&tx, device, &id)?)?;
    result.space_floor_bytes = space_floor(&tx, manager, device)?;
    tx.commit()?;
    Ok(RequestResult::Open(Box::new(result)))
}
pub fn status(manager: &ManagedArtifacts, device: &str, id: &str) -> Result<RequestStatus> {
    expire(manager, epoch())?;
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let r = row(&tx, device, id)?;
    // Consume approval into this request, not into a single GET response: retries
    // must see the same approval, but a subsequent request must not inherit it.
    if r["state"] == "ready" {
        let key = format!("backup_allow_deletes:{device}");
        if artifact_store::meta(&tx, &key)?.as_deref() == Some("1") {
            tx.execute(
                "UPDATE backup_requests SET allow_deletes=1 WHERE id=?1",
                [id],
            )?;
            artifact_store::set_meta(&tx, &key, "0")?;
        }
    }
    let mut result = status_row(&row(&tx, device, id)?)?;
    result.space_floor_bytes = space_floor(&tx, manager, device)?;
    tx.commit()?;
    Ok(result)
}
/// The device's latest disk report, from its acknowledgements (#2276).
fn latest_disk(conn: &Connection, device: &str) -> Result<Option<(i64, super::disk::Disk)>> {
    let mut stmt = conn.prepare(
        "SELECT acked_at,ack_json FROM backup_requests WHERE device=?1 AND ack_json IS NOT NULL ORDER BY acked_at DESC,created_at DESC,rowid DESC",
    )?;
    let mut rows = stmt.query([device])?;
    while let Some(r) = rows.next()? {
        let ack: Value = serde_json::from_str(&r.get::<_, String>(1)?)?;
        if let Some(disk) = ack.get("disk").filter(|d| !d.is_null()) {
            return Ok(Some((
                r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                serde_json::from_value(disk.clone())?,
            )));
        }
    }
    Ok(None)
}
/// `max(space_floor_percent × total, 50 GB)` once the disk has reported its size.
fn space_floor(conn: &Connection, manager: &ManagedArtifacts, device: &str) -> Result<Option<u64>> {
    Ok(latest_disk(conn, device)?.map(|(_, disk)| {
        super::disk::floor_bytes(manager.config.backup.space_floor_percent, disk.total_bytes)
    }))
}
pub fn allow_deletes(manager: &ManagedArtifacts, device: &str) -> Result<()> {
    artifact_store::set_meta(
        &manager.store.open()?,
        &format!("backup_allow_deletes:{device}"),
        "1",
    )
}

/// Reconcile after restart as well as immediately after request creation. Holding
/// the artifact write lock serializes duplicate request/reconciliation calls.
pub fn schedule(manager: &ManagedArtifacts, queue_dir: &Path, executable: &Path) -> Result<()> {
    use crate::queue::{CreateQueueJob, RetainedQueueStore};
    fs::create_dir_all(queue_dir)?;
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for r in artifact_store::query(
        &tx,
        "SELECT * FROM backup_requests WHERE state='preparing'",
        &[],
    )? {
        let id = r["id"].as_str().context("request id")?;
        let owner = format!("backup:{id}");
        let existing = RetainedQueueStore::owned_queue_job(queue_dir, &owner)?;
        let job = if let Some(job) = existing {
            job
        } else {
            RetainedQueueStore::create_owned_queue_job(
                queue_dir,
                CreateQueueJob {
                    local_submitter: None,
                    job_type: "background".into(),
                    label: format!("Backup preparation {id}"),
                    requester_session_id: None,
                    notify_session_id: String::new(),
                    cwd: queue_dir.display().to_string(),
                    argv: Some(vec![
                        executable.display().to_string(),
                        "--backup-prepare".into(),
                        id.into(),
                        "--backup-db".into(),
                        manager.store.path.display().to_string(),
                    ]),
                    script: None,
                    env: BTreeMap::new(),
                    timeout_seconds: 3 * 3600,
                    cpu_percent: None,
                    gpu_percent: None,
                    memory_bytes: None,
                    rank_tickets: None,
                },
                &owner,
            )?
        };
        tx.execute(
            "UPDATE backup_requests SET queue_job_id=?2 WHERE id=?1",
            params![id, job.id],
        )?;
        if matches!(
            job.state.as_str(),
            "succeeded" | "failed" | "cancelled" | "timed_out" | "interrupted"
        ) {
            tx.execute("UPDATE backup_requests SET state='failed',error=?2 WHERE id=?1 AND state='preparing'",params![id,format!("preparation job ended {} before publishing ready",job.state)])?;
        }
    }
    tx.commit()?;
    cleanup(manager)
}

/// Invoked only by the queue's application-owned worker, before server startup.
pub fn run_worker(db: PathBuf, id: &str) -> Result<()> {
    let store = artifact_store::ArtifactStore::new(db);
    let conn = store.open()?;
    let config: String = conn.query_row(
        "SELECT config_json FROM backup_requests WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    let config: ManagedArtifactsConfig = serde_json::from_str(&config)?;
    ensure!(
        crate::sessions::expand_home(&config.db_path) == store.path,
        "backup worker database mismatch"
    );
    let manager = ManagedArtifacts::new(&config)?;
    prepare_request(&manager, id)
}
pub fn prepare_request(manager: &ManagedArtifacts, id: &str) -> Result<()> {
    let conn = manager.store.open()?;
    let r = artifact_store::query(&conn, "SELECT * FROM backup_requests WHERE id=?1", &[&id])?
        .pop()
        .context("backup request not found")?;
    ensure!(
        r["state"] == "preparing",
        "backup request is no longer preparing"
    );
    let device = r["device"].as_str().context("device")?;
    let started = std::time::Instant::now();
    let result = (|| -> Result<()> {
        let scopes = collect_scope(manager, device)?;
        let base = r["base"]
            .as_str()
            .map(|base| load_manifest(&conn, device, base))
            .transpose()?;
        let (mut remaining, estimate) = super::outbox::pending_input(&scopes, base.as_ref())?;
        conn.execute(
            "UPDATE backup_requests SET estimate_bytes=?2,remaining_input=?3 WHERE id=?1",
            params![id, estimate, remaining],
        )?;
        let mut coverage = Vec::new();
        for scope in &scopes {
            coverage.push(json!({"source":scope.source,"prefix":scope.prefix(),"stamp":scope.measure()?.stamp()}));
        }
        let mut progress_at = std::time::Instant::now();
        let prepared = super::outbox::prepare_with_progress(
            manager,
            &scopes,
            base.as_ref(),
            id,
            None,
            |bytes| {
                remaining = remaining.saturating_sub(bytes);
                if remaining == 0 || progress_at.elapsed().as_secs() >= 1 {
                    conn.execute(
                        "UPDATE backup_requests SET remaining_input=?2 WHERE id=?1",
                        params![id, remaining],
                    )?;
                    progress_at = std::time::Instant::now();
                }
                Ok(())
            },
        )?;
        coverage.retain(|scope| {
            !prepared
                .deferred
                .iter()
                .any(|path| path.starts_with(&format!("{}/", scope["prefix"].as_str().unwrap())))
        });
        let mut conn = manager.store.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            row(&tx, device, id)?["state"] == "preparing",
            "backup request changed during preparation"
        );
        save_manifest(&tx, device, &prepared.manifest)?;
        tx.execute(
            "UPDATE backup_requests SET coverage_json=?2 WHERE id=?1",
            params![id, serde_json::to_string(&coverage)?],
        )?;
        let input: u64 = prepared.transfer.changed.iter().map(|f| f.size).sum();
        tx.execute("UPDATE backup_requests SET state='ready',ready_at=?2,manifest_id=?1,parts_json=?3,transfer_json=?4,packed_input_bytes=?5,packing_seconds=?6 WHERE id=?1",params![id,epoch(),serde_json::to_string(&prepared.parts)?,serde_json::to_string(&prepared.transfer)?,input,started.elapsed().as_secs_f64()])?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(error) = &result {
        conn.execute(
            "UPDATE backup_requests SET state='failed',error=?2 WHERE id=?1 AND state='preparing'",
            params![id, format!("{error:#}")],
        )?;
        cleanup(manager)?;
    }
    result
}

pub fn acknowledge(manager: &ManagedArtifacts, device: &str, id: &str, ack: &Ack) -> Result<()> {
    ensure!(
        ack.fetch_seconds.is_finite()
            && ack.fetch_seconds >= 0.0
            && ack.apply_seconds.is_finite()
            && ack.apply_seconds >= 0.0,
        "invalid backup timing"
    );
    ensure!(
        ack.fetched_bytes <= i64::MAX as u64 && ack.held_deletions <= i64::MAX as u64,
        "invalid backup count"
    );
    if let Some(update) = &ack.update {
        ensure!(
            update.from.len() <= 200
                && update.to.len() <= 200
                && update.error.as_ref().is_none_or(|e| e.len() <= 4000),
            "invalid backup update report"
        );
    }
    if let Some(disk) = &ack.disk {
        ensure!(
            super::disk::HEALTH.contains(&disk.health.as_str())
                && disk.holding_oldest.as_ref().is_none_or(|d| d.len() <= 10)
                && disk.free_bytes <= disk.total_bytes
                && disk.total_bytes <= i64::MAX as u64,
            "invalid backup disk report"
        );
    }
    if ack.applied && ack.fetched_bytes > 0 {
        ensure!(
            ack.fetch_seconds > 0.0 && ack.apply_seconds > 0.0,
            "nonempty backup needs positive timings"
        );
    }
    expire(manager, epoch())?;
    let mut conn = manager.store.open()?;
    let body = serde_json::to_string(ack)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let r = row(&tx, device, id)?;
    if r["ack_json"].as_str() == Some(&body) {
        return Ok(());
    }
    ensure!(r["state"] == "ready", "backup request is not ready");
    if ack.applied {
        confirm(&tx, device, id)?;
        if ack.fetched_bytes > 0 {
            record_rate(&tx, device, "fetch", ack.fetched_bytes, ack.fetch_seconds)?;
        }
        // Application processes every request part, including cached bytes from
        // previous download attempts. Use the server's immutable part sizes.
        let parts: Vec<Part> =
            serde_json::from_str(r["parts_json"].as_str().context("request parts missing")?)?;
        let applied_bytes = parts.iter().try_fold(0u64, |total, part| {
            total
                .checked_add(part.bytes)
                .context("backup part size overflow")
        })?;
        if applied_bytes > 0 {
            ensure!(
                ack.apply_seconds > 0.0,
                "nonempty application needs positive timing"
            );
            record_rate(&tx, device, "apply", applied_bytes, ack.apply_seconds)?;
        }
    }
    tx.execute(
        "UPDATE backup_requests SET state=?2,acked_at=?3,ack_json=?4,error=?5 WHERE id=?1",
        params![
            id,
            if ack.applied { "acked" } else { "failed" },
            epoch(),
            body,
            ack.error
        ],
    )?;
    let update = ack.update.as_ref().map(serde_json::to_string).transpose()?;
    tx.execute("INSERT INTO backup_runs(queue_job_id,started_at,finished_at,state,include_count,include_bytes,update_json) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![r["queue_job_id"].as_str(),time::OffsetDateTime::from_unix_timestamp(r["created_at"].as_i64().context("request creation time")?)?.format(&time::format_description::well_known::Rfc3339)?,now(),if ack.applied {"acked"}else{"failed"},load_manifest(&tx,device,id)?.files.len() as i64,ack.fetched_bytes,update])?;
    if ack.check.as_ref().is_some_and(|c| !c.ok) {
        alert(
            &tx,
            &format!("backup-check:{id}"),
            "Backup integrity check failed",
            &format!(
                "The remote disk reported files that failed its integrity check: {}",
                ack.check.as_ref().unwrap().bad.join(", ")
            ),
            true,
        )?;
    }
    if ack.held_deletions > 0 {
        let total = load_manifest(&tx, device, id)?.files.len() as u64 + ack.held_deletions;
        alert(&tx,&format!("backup-held:{id}"),"Backup deletions need confirmation",&format!("The remote disk held back {} deletions ({:.1}% of its files). If you meant this, run `sm backup allow-deletes`.",ack.held_deletions,100.0*ack.held_deletions as f64/total.max(1) as f64),true)?;
    }
    if let Some(disk) = &ack.disk {
        drive_health_alert(&tx, device, &disk.health, epoch())?;
    }
    if ack.applied {
        let key = format!("backup_missed:{device}");
        if artifact_store::meta(&tx, &key)?.as_deref() == Some("1") {
            alert(
                &tx,
                &format!("backup-recovered:{id}"),
                "Backup is confirming again",
                "The remote disk is confirming again.",
                false,
            )?;
            artifact_store::set_meta(&tx, &key, "0")?;
        }
    }
    tx.commit()?;
    // Confirm the device first. Display-only scans cannot let an in-flight
    // acknowledgement expire, and never hold the artifact writer transaction.
    if ack.applied {
        mark_covered(&conn, manager, &r)?;
    }
    cleanup(manager)
}
pub fn expire(manager: &ManagedArtifacts, at: i64) -> Result<()> {
    let ttl = i64::try_from(manager.config.backup.ready_ttl_hours)?
        .checked_mul(3600)
        .context("invalid backup TTL")?;
    manager.store.open()?.execute("UPDATE backup_requests SET state='expired',error='outbox expired without acknowledgement' WHERE state='ready' AND ready_at<=?1",[at.saturating_sub(ttl)])?;
    cleanup(manager)
}
fn cleanup(manager: &ManagedArtifacts) -> Result<()> {
    let conn = manager.store.open()?;
    for r in artifact_store::query(&conn,"SELECT id,outbox FROM backup_requests WHERE state IN ('acked','failed','expired') AND cleaned=0",&[])? {
        let path=PathBuf::from(r["outbox"].as_str().context("outbox path")?);
        match fs::remove_dir_all(&path) {Ok(())=>{},Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into())}
        conn.execute("UPDATE backup_requests SET cleaned=1 WHERE id=?1",[r["id"].as_str()])?;
    }
    Ok(())
}
fn alert(conn: &Connection, key: &str, title: &str, body: &str, blocking: bool) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO backup_alerts(key,title,body,blocking) VALUES(?1,?2,?3,?4)",
        params![key, title, body, blocking],
    )?;
    Ok(())
}
/// One blocking message when the drive first reports failing, then one every
/// seven days while it stays failing (#2276). Any other report clears it.
pub(super) fn drive_health_alert(
    conn: &Connection,
    device: &str,
    health: &str,
    at: i64,
) -> Result<()> {
    let key = format!("backup_drive_failing:{device}");
    if health != "failing" {
        return artifact_store::set_meta(conn, &key, "");
    }
    let last = artifact_store::meta(conn, &key)?.and_then(|s| s.parse::<i64>().ok());
    if last.is_none_or(|last| at.saturating_sub(last) >= 7 * 86400) {
        alert(
            conn,
            // The previous message's time keeps a reminder's key distinct.
            &format!("backup-failing:{device}:{at}:{}", last.unwrap_or(0)),
            "Backup drive is failing",
            "The backup drive reports it is failing (SMART). Replace it, then reseed with `sm backup seed`.",
            true,
        )?;
        artifact_store::set_meta(conn, &key, &at.to_string())?;
    }
    Ok(())
}
pub fn missed_alerts(manager: &ManagedArtifacts, at: i64) -> Result<()> {
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for device in &manager.config.backup.puller_devices {
        let enabled_key = format!("backup_enabled:{device}");
        let enabled = artifact_store::meta(&tx, &enabled_key)?
            .map(|s| s.parse::<i64>())
            .transpose()?
            .unwrap_or(at);
        artifact_store::set_meta(&tx, &enabled_key, &enabled.to_string())?;
        let last: Option<i64> = tx.query_row(
            "SELECT MAX(acked_at) FROM backup_requests WHERE device=?1 AND state='acked'",
            [device],
            |r| r.get(0),
        )?;
        let since = last.unwrap_or(enabled);
        let days = at.saturating_sub(since) / 86400;
        if days >= manager.config.backup.alert_after_missed_nights as i64 {
            let last_state:Option<String>=tx.query_row("SELECT state || COALESCE(': ' || error,'') FROM backup_requests WHERE device=?1 ORDER BY created_at DESC LIMIT 1",[device],|r|r.get(0)).optional()?;
            let date = time::OffsetDateTime::from_unix_timestamp(since)?
                .to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC))
                .format(&time::format_description::well_known::Rfc3339)?;
            alert(&tx,&format!("backup-missed:{device}:{}",at/86400),"Backup confirmation is overdue",&format!("Backup: the remote disk has not confirmed since {date} ({days} nights). Last state: {}. Nothing is lost yet.",last_state.as_deref().unwrap_or("no request")),false)?;
            artifact_store::set_meta(&tx, &format!("backup_missed:{device}"), "1")?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[derive(Default)]
pub(super) struct Coverage {
    static_paths: Vec<(PathBuf, String)>,
    artifacts: Vec<(String, Value, Option<String>)>,
}
impl Coverage {
    /// No filesystem work here: callers may persist inside their confirmation
    /// transaction. Never copy a content stamp published after verification.
    pub(super) fn persist(self, conn: &Connection) -> Result<()> {
        for (path, stamp) in self.static_paths {
            conn.execute(
                "UPDATE backup_static SET backed_up_at=?2,backup_stamp=?3 WHERE path=?1",
                params![path.to_string_lossy(), now(), stamp],
            )?;
        }
        for (root, artifact, stamp) in self.artifacts {
            conn.execute(
                "UPDATE artifacts SET backed_up_at=?3,backup_stamp=?4 WHERE root=?1 AND name=?2 AND state=?5 AND location=?6 AND content_stamp IS ?7 AND created_at=?8 AND op IS NULL",
                params![root, artifact["name"].as_str(), now(), stamp,
                    artifact["state"].as_str(), artifact["location"].as_str(),
                    artifact["content_stamp"].as_str(), artifact["created_at"].as_str()],
            )?;
        }
        Ok(())
    }
}

// Missing or modified local data never invalidates a remote confirmation.
pub(super) fn mark_covered(
    conn: &Connection,
    manager: &ManagedArtifacts,
    request: &Value,
) -> Result<()> {
    measure_coverage(conn, manager, request)?.persist(conn)
}

/// Snapshot artifact versions before scanning. Scans must run outside a writer
/// transaction; persistence checks the snapshot again under SQLite's write lock.
pub(super) fn measure_coverage(
    conn: &Connection,
    manager: &ManagedArtifacts,
    request: &Value,
) -> Result<Coverage> {
    #[cfg(test)]
    BEFORE_COVERAGE_MEASURE.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    let mut result = Coverage::default();
    let covered: Vec<Value> =
        serde_json::from_str(request["coverage_json"].as_str().unwrap_or("[]"))?;
    for scope in covered {
        let source = PathBuf::from(scope["source"].as_str().context("coverage source")?);
        let stamp = scope["stamp"].as_str().context("coverage stamp")?;
        let mut artifacts = Vec::new();
        for root in &manager.config.roots {
            for a in artifact_store::query(conn,"SELECT name,state,location,content_stamp,created_at FROM artifacts WHERE root=?1 AND state IN ('live','frozen','unrecorded') AND op IS NULL",&[&root.path])? {
                artifacts.push((root, a));
            }
        }
        // Re-measure with the exclusions the preparation stamp used.
        let static_identity = manager
            .config
            .backup
            .static_paths
            .iter()
            .map(|p| crate::sessions::expand_home(p))
            .find(|p| fs::canonicalize(p).is_ok_and(|p| p == source));
        let exclude = match &static_identity {
            Some(identity) => super::Exclude::for_static(&manager.config.backup, identity)?,
            None => super::Exclude::default(),
        };
        if !crate::managed_artifacts::scan::measure_excluding(&source, |p| exclude.skips(p))
            .is_ok_and(|size| size.stamp() == stamp)
        {
            continue;
        }
        for path in &manager.config.backup.static_paths {
            let path = crate::sessions::expand_home(path);
            if fs::canonicalize(&path).is_ok_and(|p| p == source) {
                result.static_paths.push((path, stamp.to_owned()));
            }
        }
        for (root, a) in artifacts {
            let name = a["name"].as_str().context("artifact name")?;
            let path = if a["state"] == "frozen" {
                PathBuf::from(&root.cold_store).join(".frozen").join(name)
            } else {
                PathBuf::from(&root.path).join(name)
            };
            // A link into another source is backed up as the link itself (#2354).
            let link = a["state"] != "frozen"
                && fs::canonicalize(&root.path).is_ok_and(|r| r.join(name) == source);
            if link || fs::canonicalize(path).is_ok_and(|p| p == source) {
                // Frozen stamps describe the original tree, not compressed
                // files. Live stamps describe the measurement path, which
                // may be a symlink even when backup reads its target.
                let backup_stamp = if a["state"] == "frozen" {
                    a["content_stamp"].as_str().map(str::to_owned)
                } else {
                    let base = if a["location"] == "cold" {
                        &root.cold_store
                    } else {
                        &root.path
                    };
                    let measured_path = PathBuf::from(base).join(name);
                    if fs::symlink_metadata(measured_path).is_ok_and(|m| m.file_type().is_symlink())
                    {
                        Some(crate::managed_artifacts::scan::Size::default().stamp())
                    } else {
                        Some(stamp.to_owned())
                    }
                };
                result.artifacts.push((root.path.clone(), a, backup_stamp));
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
thread_local! {
    pub(super) static BEFORE_COVERAGE_MEASURE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

fn rfc3339(at: i64) -> Result<String> {
    Ok(time::OffsetDateTime::from_unix_timestamp(at)?
        .format(&time::format_description::well_known::Rfc3339)?)
}
/// The Backup card (#2276): the last seven nights, last night's transfer, the
/// weekly check, and the remote disk, from the device's acknowledgements.
pub(super) fn card(conn: &Connection, device: &str, at: i64) -> Result<Value> {
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let date = |t: i64| -> Result<time::Date> {
        Ok(time::OffsetDateTime::from_unix_timestamp(t)?
            .to_offset(offset)
            .date())
    };
    let requests = artifact_store::query(
        conn,
        "SELECT state,created_at,acked_at,ack_json,error FROM backup_requests WHERE device=?1 ORDER BY created_at DESC,id DESC LIMIT 400",
        &[&device],
    )?;
    // Nights before backup began are not misses.
    let began = artifact_store::meta(conn, &format!("backup_enabled:{device}"))?
        .and_then(|s| s.parse::<i64>().ok())
        .into_iter()
        .chain(requests.last().and_then(|r| r["created_at"].as_i64()))
        .min();
    let today = date(at)?;
    let mut nights = Vec::new();
    for back in (0..7).rev() {
        let night = today - time::Duration::days(back);
        if began.is_none_or(|b| date(b).is_ok_and(|b| b > night)) {
            continue;
        }
        let runs: Vec<&Value> = requests
            .iter()
            .filter(|r| {
                r["created_at"]
                    .as_i64()
                    .is_some_and(|t| date(t).is_ok_and(|d| d == night))
            })
            .collect();
        // Only finished nights: tonight before its run, or a run under way,
        // has nothing to report yet.
        let (ok, error) = if runs.iter().any(|r| r["state"] == "acked") {
            (true, Value::Null)
        } else if runs
            .iter()
            .any(|r| matches!(r["state"].as_str(), Some("preparing" | "ready")))
            || (runs.is_empty() && night == today)
        {
            continue;
        } else if let Some(r) = runs.first() {
            (false, json!(r["error"].as_str().unwrap_or("not applied")))
        } else {
            (false, json!("no request"))
        };
        nights.push(json!({"date":night.to_string(),"ok":ok,"error":error}));
    }
    let mut last_night = Value::Null;
    let mut weekly_check = Value::Null;
    let mut disk = Value::Null;
    let mut pruned_on = Value::Null;
    for r in &requests {
        let Some(acked_at) = r["acked_at"].as_i64() else {
            continue;
        };
        let ack: Value = serde_json::from_str(r["ack_json"].as_str().unwrap_or("{}"))?;
        if last_night.is_null() && r["state"] == "acked" {
            last_night = json!({
                "bytes": ack["fetched_bytes"],
                "seconds": acked_at.saturating_sub(r["created_at"].as_i64().unwrap_or(acked_at)),
                "confirmed_at": rfc3339(acked_at)?,
            });
        }
        if weekly_check.is_null() && ack["check"].is_object() {
            let check = &ack["check"];
            weekly_check = json!({
                "date": date(acked_at)?.to_string(),
                "ok": check["ok"],
                "files_checked": check["files_checked"],
                "bad": check["bad"].as_array().map_or(0, Vec::len),
            });
        }
        if disk.is_null() && ack["disk"].is_object() {
            disk = ack["disk"].clone();
            disk["reported_at"] = json!(rfc3339(acked_at)?);
        }
        if pruned_on.is_null()
            && at.saturating_sub(acked_at) <= 30 * 86400
            && ack["disk"]["pruned"].is_object()
        {
            pruned_on = json!(date(acked_at)?.to_string());
        }
    }
    if disk.is_object() {
        disk["pruned_on"] = pruned_on;
    }
    // Which speeds are measured rather than the configured default.
    let mut measured = serde_json::Map::new();
    for kind in ["fetch", "apply", "probe"] {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM backup_rate_samples WHERE device=?1 AND kind=?2 AND (kind='probe' OR bytes>1000000000)",
            params![device, kind],
            |r| r.get(0),
        )?;
        measured.insert(kind.into(), json!(count > 0));
    }
    Ok(
        json!({"nights":nights,"last_night":last_night,"weekly_check":weekly_check,"disk":disk,"measured":measured}),
    )
}

pub fn owner_summary(manager: &ManagedArtifacts) -> Result<Value> {
    let conn = manager.store.open()?;
    let mut devices = Vec::new();
    for device in &manager.config.backup.puller_devices {
        let runs=artifact_store::query(&conn,"SELECT id,state,created_at,ready_at,acked_at,estimate_bytes,ack_json,error FROM backup_requests WHERE device=?1 ORDER BY created_at DESC,id DESC LIMIT 7",&[device])?;
        devices.push(json!({"device":device,"confirmed":confirmed(&conn,device)?.map(|m|m.manifest_id),"runs":runs,"rates":rates(&conn,device,parse_rate(&manager.config.backup.backup_rate_default)?)?,"card":card(&conn,device,epoch())?}));
    }
    let mut runs=artifact_store::query(&conn,"SELECT id,state,created_at,ready_at,acked_at,estimate_bytes,ack_json,error FROM backup_requests ORDER BY created_at DESC,id DESC LIMIT 7",&[])?;
    for run in &mut runs {
        for (source, target) in [("created_at", "started_at"), ("acked_at", "finished_at")] {
            run[target] = run[source]
                .as_i64()
                .map(|at| {
                    time::OffsetDateTime::from_unix_timestamp(at).map(|at| {
                        at.format(&time::format_description::well_known::Rfc3339)
                            .expect("UTC timestamp formats")
                    })
                })
                .transpose()?
                .map(Value::String)
                .unwrap_or(Value::Null);
        }
        let ack: Value = serde_json::from_str(run["ack_json"].as_str().unwrap_or("{}"))?;
        run["include_bytes"] = ack
            .get("fetched_bytes")
            .cloned()
            .unwrap_or_else(|| run["estimate_bytes"].clone());
    }
    let rates = devices
        .first()
        .map(|d| d["rates"].clone())
        .unwrap_or(Value::Null);
    let card = devices
        .first()
        .map(|d| d["card"].clone())
        .unwrap_or(Value::Null);
    let confirmed_at = artifact_store::query(
        &conn,
        "SELECT acked_at FROM backup_requests WHERE state='acked' ORDER BY acked_at DESC LIMIT 1",
        &[],
    )?
    .first()
    .and_then(|r| r["acked_at"].as_i64())
    .map(time::OffsetDateTime::from_unix_timestamp)
    .transpose()?
    .map(|at| at.format(&time::format_description::well_known::Rfc3339))
    .transpose()?;
    // The latest self-update the mini reported, success or failure (#2264).
    let update = artifact_store::query(
        &conn,
        "SELECT finished_at,update_json FROM backup_runs WHERE update_json IS NOT NULL ORDER BY id DESC LIMIT 1",
        &[],
    )?
    .pop()
    .map(|r| -> Result<Value> {
        let mut update: Value = serde_json::from_str(r["update_json"].as_str().unwrap_or("{}"))?;
        update["at"] = r["finished_at"].clone();
        Ok(update)
    })
    .transpose()?;
    Ok(
        json!({"enabled":manager.config.backup.enabled,"devices":devices,"runs":runs,"confirmed_at":confirmed_at,"update":update,"rate_default":manager.config.backup.backup_rate_default,"scope_roots":manager.config.backup.static_paths,"fetch_rate":rates["fetch"],"apply_rate":rates["apply"],"probe_rate":rates["probe"],"effective_rate":rates["effective"],"card":card}),
    )
}
