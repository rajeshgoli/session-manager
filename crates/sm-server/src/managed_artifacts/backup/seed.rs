//! Local USB seeding. Confirmation happens only after the destination verifies
//! and applies the complete scope; a concurrent remote request makes seed fail.
use super::{client::Destination, lifecycle, *};
use crate::managed_artifacts::{archive, scan, ManagedArtifacts};
use anyhow::{ensure, Context};
use rusqlite::{params, TransactionBehavior};
use serde_json::json;
use std::{fs, path::Path};
pub fn seed(
    manager: &ManagedArtifacts,
    device: &str,
    dest: &mut Destination,
    adopt: Option<&Path>,
) -> Result<String> {
    ensure!(
        manager
            .config
            .backup
            .puller_devices
            .iter()
            .any(|d| d == device),
        "unknown backup device"
    );
    let conn = manager.store.open()?;
    let previous = confirmed(&conn, device)?.map(|m| m.manifest_id);
    let scopes = collect_scope(manager, device)?;
    let id = format!(
        "seed-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let coverage:Vec<_>=scopes.iter().map(|s|Ok(json!({"source":s.source,"prefix":s.prefix(),"stamp":scan::measure(&s.source)?.stamp()}))).collect::<Result<_>>()?;
    let mut prepared = prepare(manager, &scopes, None, &id)?;
    ensure!(
        prepared.deferred.is_empty(),
        "seed source changed during preparation; retry"
    );
    if let Some(adopt) = adopt {
        let plain: Vec<_> = scopes.iter().filter(|s| s.plain).collect();
        ensure!(
            plain.len() == 1,
            "adopt-plain requires exactly one plain scope"
        );
        let target = dest.directory("plain")?.join(&plain[0].segment);
        if adopt.exists() {
            ensure!(!target.exists(), "plain destination already exists");
            let source = fs::canonicalize(adopt)?;
            ensure!(
                !source.starts_with(&dest.root),
                "adoption source is already inside destination"
            );
            for entry in archive::inventory(&source)? {
                if entry.kind == "directory" || entry.path.split('/').any(|p| p.starts_with("._")) {
                    continue;
                }
                ensure!(
                    entry.kind == "file",
                    "adoption requires ordinary data files"
                );
                let key = format!("{}/{}", plain[0].prefix(), entry.path);
                let expected = prepared
                    .manifest
                    .files
                    .get(&key)
                    .context("adoption file no longer exists on source Mac")?;
                ensure!(
                    entry.size == expected.size,
                    "adoption size mismatch: {}",
                    entry.path
                );
                let mtime = i128::from(entry.mtime) * 1_000_000_000 + i128::from(entry.mtime_nsec);
                ensure!(
                    (mtime - expected.mtime_ns).abs() <= 2_000_000_000,
                    "adoption mtime mismatch: {}",
                    entry.path
                );
                super::client::verify(&source.join(entry.path), expected)?;
            }
            fs::rename(&source, &target)
                .context("adopt-plain must be on the destination filesystem")?;
        } else {
            ensure!(target.is_dir(), "adoption source is missing");
        }
    }
    let staging = dest.unpack(
        &id,
        &prepared.transfer,
        &prepared.parts,
        &prepared.directory,
    )?;
    dest.apply(&id, &prepared.transfer, &staging, true, 1.0)?;
    prepared.manifest.base = previous.clone();
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM backup_requests WHERE device=?1 AND state IN ('preparing','ready'))",[device],|r|r.get(0))?;
    ensure!(
        !active,
        "remote backup request is active; seed confirmation deferred"
    );
    save_manifest(&tx, device, &prepared.manifest)?;
    confirm(&tx, device, &id)?;
    lifecycle::mark_covered(
        &tx,
        manager,
        &json!({"coverage_json":serde_json::to_string(&coverage)?}),
    )?;
    let at = time::OffsetDateTime::now_utc().unix_timestamp();
    tx.execute("INSERT INTO backup_requests(id,device,state,base,created_at,ready_at,acked_at,config_json,outbox,manifest_id,parts_json,transfer_json,ack_json) VALUES(?1,?2,'acked',?3,?4,?4,?4,?5,?6,?1,?7,?8,?9)",params![id,device,previous,at,serde_json::to_string(&manager.config)?,prepared.directory.to_string_lossy(),serde_json::to_string(&prepared.parts)?,serde_json::to_string(&prepared.transfer)?,json!({"applied":true,"seed":true,"fetched_bytes":0}).to_string()])?;
    tx.commit()?;
    dest.confirm(&id)?;
    lifecycle::expire(manager, at)?;
    if staging.exists() {
        fs::remove_dir_all(staging)?;
    }
    Ok(id)
}
