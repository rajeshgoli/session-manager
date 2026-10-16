//! Local USB seeding. Confirmation happens only after the destination verifies
//! and applies the complete scope; a concurrent remote request makes seed fail.
use super::{client::Destination, lifecycle, *};
use crate::managed_artifacts::{archive, scan, ManagedArtifacts};
use anyhow::{ensure, Context};
use rusqlite::{params, TransactionBehavior};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
};
// Seeds have no durable request until confirmation, so lifecycle expiry cannot
// reclaim an outbox left behind by an earlier validation/application error.
struct SeedOutbox(PathBuf);
impl Drop for SeedOutbox {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "could not remove seed temporary directory {}: {error}",
                    self.0.display()
                );
            }
        }
    }
}
struct AdoptionMove(Option<(PathBuf, PathBuf)>);
impl Drop for AdoptionMove {
    fn drop(&mut self) {
        if let Some((source, target)) = &self.0 {
            if let Err(error) = fs::rename(target, source) {
                eprintln!(
                    "could not return adopted folder {}: {error}",
                    source.display()
                );
            }
        }
    }
}
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
    // Verified adopted files form a local base: preparation includes their
    // records but does not spool or transmit a second copy of their payloads.
    let mut adopted = Manifest::default();
    if let Some(source) = adopt {
        let plain: Vec<_> = scopes.iter().filter(|s| s.plain).collect();
        ensure!(
            plain.len() == 1,
            "adopt-plain requires exactly one plain scope"
        );
        let scope = plain[0];
        let source = if source.exists() {
            source.to_path_buf()
        } else {
            dest.root.join(scope.prefix())
        };
        for entry in archive::inventory(&source)? {
            if entry.kind == "directory" || entry.path.split('/').any(|p| p.starts_with("._")) {
                continue;
            }
            ensure!(
                entry.kind == "file",
                "adoption requires ordinary data files"
            );
            let original = scope.source.join(&entry.path);
            let metadata = fs::symlink_metadata(&original)?;
            use std::os::unix::fs::MetadataExt;
            ensure!(
                metadata.is_file() && metadata.len() == entry.size,
                "adoption size/kind mismatch: {}",
                entry.path
            );
            let mtime_ns =
                i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec());
            let adopted_mtime =
                i128::from(entry.mtime) * 1_000_000_000 + i128::from(entry.mtime_nsec);
            ensure!(
                (mtime_ns - adopted_mtime).abs() <= 2_000_000_000,
                "adoption mtime mismatch: {}",
                entry.path
            );
            let path = format!("{}/{}", scope.prefix(), entry.path);
            let record = FileRecord {
                path: path.clone(),
                size: entry.size,
                mtime_ns,
                mode: metadata.mode() & 0o7777,
                sha256: Some(super::outbox::hash_file(&original)?),
                target: None,
            };
            super::client::verify(&source.join(&entry.path), &record)?;
            adopted.files.insert(path, record);
        }
    }
    let mut prepared = prepare(manager, &scopes, adopt.map(|_| &adopted), &id)?;
    let _outbox = SeedOutbox(prepared.directory.clone());
    ensure!(
        prepared.deferred.is_empty(),
        "seed source changed during preparation; retry"
    );
    // Hold the source-store write transaction from the active-request check
    // through confirmation so no pull can start against the previous head.
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM backup_requests WHERE device=?1 AND state IN ('preparing','ready'))",[device],|r|r.get(0))?;
    ensure!(
        !active,
        "remote backup request is active; seed confirmation deferred"
    );
    ensure!(
        confirmed(&tx, device)?.map(|m| m.manifest_id) == previous,
        "backup head changed during seed preparation; retry"
    );
    let _incoming = SeedOutbox(dest.root.join(".incoming").join(&id));
    let mut adoption_move = AdoptionMove(None);
    if let Some(adopt) = adopt {
        let plain: Vec<_> = scopes.iter().filter(|s| s.plain).collect();
        ensure!(
            plain.len() == 1,
            "adopt-plain requires exactly one plain scope"
        );
        let prefix = format!("{}/", plain[0].prefix());
        ensure!(!dest.records()?.keys().any(|path| path.starts_with(&prefix)),
            "adopt-plain requires an untracked plain scope; use seed without adoption to refresh an existing backup");
        let target = dest.directory("plain")?.join(&plain[0].segment);
        if adopt.exists() {
            ensure!(!target.exists(), "plain destination already exists");
            let source = fs::canonicalize(adopt)?;
            ensure!(
                !dest.root.starts_with(&source),
                "adoption source contains the backup destination"
            );
            if let Ok(relative) = source.strip_prefix(&dest.root) {
                let first = relative
                    .components()
                    .next()
                    .context("empty adoption source")?;
                let name = first.as_os_str().to_string_lossy();
                ensure!(
                    !matches!(
                        name.as_ref(),
                        "plain" | "packed" | "state.db" | "state.json"
                    ) && !name.starts_with('.'),
                    "adoption source overlaps managed backup data"
                );
            }
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
            adoption_move.0 = Some((source, target));
        } else {
            ensure!(target.is_dir(), "adoption source is missing");
        }
    }
    // Preparation omitted these payloads. Verify the complete saved set after
    // the move, including entries that vanished between directory scans.
    for (path, record) in &adopted.files {
        if prepared.manifest.files.get(path) == Some(record)
            && !prepared.transfer.changed.iter().any(|r| &r.path == path)
        {
            super::client::verify(&dest.root.join(path), record)
                .with_context(|| format!("adopted file missing or changed: {path}"))?;
        }
    }
    prepared.transfer.deleted = dest
        .records()?
        .keys()
        .filter(|p| !prepared.manifest.files.contains_key(*p))
        .cloned()
        .collect();
    prepared.transfer.base = previous.clone();
    let staging = dest.unpack(
        &id,
        &prepared.transfer,
        &prepared.parts,
        &prepared.directory,
    )?;
    if let Err(error) = dest.apply(&id, &prepared.transfer, &staging, true, 1.0) {
        dest.rollback(&id)
            .context("restore destination after failed seed application")?;
        return Err(error);
    }
    let mut source_committed = false;
    let at = time::OffsetDateTime::now_utc().unix_timestamp();
    let confirmation = (|| -> Result<()> {
        let destination_tx = dest.conn.transaction()?;
        // Register only verified adopted files omitted from the transfer. Changed
        // files still go through normal apply and history retention.
        for (path, record) in &adopted.files {
            if prepared.manifest.files.get(path) == Some(record)
                && !prepared.transfer.changed.iter().any(|r| &r.path == path)
            {
                destination_tx.execute("INSERT INTO files(path,body) VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET body=excluded.body", params![path, serde_json::to_string(record)?])?;
            }
        }
        prepared.manifest.base = previous.clone();
        save_manifest(&tx, device, &prepared.manifest)?;
        confirm(&tx, device, &id)?;
        lifecycle::mark_covered(
            &tx,
            manager,
            &json!({"coverage_json":serde_json::to_string(&coverage)?}),
        )?;
        tx.execute("INSERT INTO backup_requests(id,device,state,base,created_at,ready_at,acked_at,config_json,outbox,manifest_id,parts_json,transfer_json,ack_json) VALUES(?1,?2,'acked',?3,?4,?4,?4,?5,?6,?1,?7,?8,?9)",params![id,device,previous,at,serde_json::to_string(&manager.config)?,prepared.directory.to_string_lossy(),serde_json::to_string(&prepared.parts)?,serde_json::to_string(&prepared.transfer)?,json!({"applied":true,"seed":true,"fetched_bytes":0}).to_string()])?;
        tx.commit()?;
        source_committed = true;
        adoption_move.0 = None;
        destination_tx.commit()?;
        Ok(())
    })();
    if let Err(error) = confirmation {
        if !source_committed {
            dest.rollback(&id)
                .context("restore destination after failed seed confirmation")?;
        }
        return Err(error);
    }
    dest.confirm(&id)?;
    lifecycle::expire(manager, at)?;
    if staging.exists() {
        fs::remove_dir_all(staging)?;
    }
    Ok(id)
}
