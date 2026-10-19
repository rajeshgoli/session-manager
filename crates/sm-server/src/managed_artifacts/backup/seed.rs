//! Local USB seeding. Confirmation happens only after the destination verifies
//! and applies the complete scope; a concurrent remote request makes seed fail.
//! Staged files survive a failed or stopped seed, and the next seed resumes
//! them; only a confirmed seed, or `--fresh`, removes seed staging (#2301).
use super::{client::Destination, lifecycle, *};
use crate::managed_artifacts::{archive, ManagedArtifacts};
use anyhow::{ensure, Context};
use rusqlite::{params, Connection, TransactionBehavior};
use serde_json::json;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
};
struct AdoptionMove(Option<(PathBuf, PathBuf)>);
impl Drop for AdoptionMove {
    fn drop(&mut self) {
        if let Some((source, target)) = &self.0 {
            if let Err(error) = restore_adoption(source, target) {
                eprintln!(
                    "could not return adopted folder {}: {error}",
                    source.display()
                );
            }
        }
    }
}
fn sync_adoption(from: &Path, to: &Path) -> Result<()> {
    #[cfg(test)]
    if FAIL_ADOPTION_SYNC.get() {
        anyhow::bail!("injected adoption directory sync failure");
    }
    crate::managed_artifacts::ops::sync_rename_parents(from, to)
}
fn restore_adoption(source: &Path, target: &Path) -> Result<()> {
    if target.exists() && !source.exists() {
        fs::rename(target, source)?;
    }
    // A previous attempt may have renamed successfully but failed to sync.
    // Retry both parents before allowing recovery to discard the journal.
    sync_adoption(target, source)
}
#[derive(Serialize, Deserialize)]
struct Journal {
    id: String,
    manifest: Manifest,
    adoption: Option<(PathBuf, PathBuf)>,
}
#[cfg(test)]
thread_local! { pub(super) static FAIL_AFTER_SOURCE_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
thread_local! { pub(super) static FAIL_ADOPTION_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
thread_local! { pub(super) static BEFORE_APPLY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
thread_local! { pub(super) static AFTER_APPLY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }

fn check_head(conn: &Connection, device: &str, previous: &Option<String>) -> Result<()> {
    let active: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM backup_requests WHERE device=?1 AND state IN ('preparing','ready'))", [device], |r| r.get(0))?;
    ensure!(
        !active,
        "remote backup request is active; seed confirmation deferred; retry after the request finishes"
    );
    ensure!(
        confirmed(conn, device)?.map(|m| m.manifest_id) == *previous,
        "backup head changed during seed; retry"
    );
    Ok(())
}
fn pending(dest: &Destination) -> Result<Option<Journal>> {
    dest.meta("pending_seed")?
        .map(|body| serde_json::from_str(&body).map_err(Into::into))
        .transpose()
}
fn resolve(dest: &mut Destination, journal: &Journal, committed: bool) -> Result<()> {
    if committed {
        // A source commit can precede the adopted-record transaction or local
        // state file. The complete manifest repairs both without the source disk.
        for record in journal
            .manifest
            .files
            .values()
            .filter(|r| r.target.is_none())
        {
            super::client::verify(&dest.root.join(&record.path), record)?;
        }
        let tx = dest.conn.transaction()?;
        tx.execute("DELETE FROM files", [])?;
        for (path, record) in &journal.manifest.files {
            tx.execute(
                "INSERT INTO files(path,body) VALUES(?1,?2)",
                params![path, serde_json::to_string(record)?],
            )?;
        }
        tx.commit()?;
        dest.confirm(&journal.id)?;
        let incoming = dest.root.join(".incoming").join(&journal.id);
        if incoming.exists() {
            fs::remove_dir_all(incoming)?;
        }
    } else {
        dest.return_seed_staging(&journal.id, &journal.manifest.files)?;
        dest.rollback(&journal.id)?;
        if let Some((source, target)) = &journal.adoption {
            restore_adoption(source, target)?;
        }
    }
    dest.conn
        .execute("DELETE FROM metadata WHERE key='pending_seed'", [])?;
    Ok(())
}
pub(super) fn recover_remote(
    dest: &mut Destination,
    remote: &mut impl super::pull::Remote,
) -> Result<()> {
    if let Some(journal) = pending(dest)? {
        let (code, status) =
            remote.json("GET", &format!("/backup/requests/{}", journal.id), None)?;
        let committed = code == 200 && status["state"] == "acked";
        ensure!(
            committed || code == 404,
            "cannot determine pending seed confirmation: {code} {status}"
        );
        resolve(dest, &journal, committed)?;
    }
    Ok(())
}
fn recover_local(manager: &ManagedArtifacts, device: &str, dest: &mut Destination) -> Result<()> {
    if let Some(journal) = pending(dest)? {
        let conn = manager.store.open()?;
        let committed: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM backup_requests WHERE id=?1 AND device=?2 AND state='acked')", params![journal.id, device], |r| r.get(0))?;
        resolve(dest, &journal, committed)?;
    }
    Ok(())
}
/// Seed staging directories under `.incoming`, oldest first.
fn seed_staging(dest: &Destination) -> Result<Vec<PathBuf>> {
    let mut staging = Vec::new();
    for entry in fs::read_dir(dest.directory(".incoming")?)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("seed-") && entry.file_type()?.is_dir() {
            let stamp = name["seed-".len()..].parse::<i128>().unwrap_or(0);
            staging.push((stamp, entry.path()));
        }
    }
    staging.sort();
    Ok(staging.into_iter().map(|(_, path)| path).collect())
}
/// Move the newest seed staging to this seed's id, so preparation resumes it.
fn resume_staging(dest: &Destination, staging: &Path) -> Result<()> {
    let Some(previous) = seed_staging(dest)?.pop() else {
        return Ok(());
    };
    let index = super::outbox::staged_index(&previous)?;
    let bytes: u64 = index.values().map(|s| s.record.size).sum();
    println!(
        "Resuming an earlier seed: {} files ({:.1} GB) already staged; unchanged ones are reused. Pass --fresh to discard them.",
        index.len(),
        bytes as f64 / 1e9
    );
    fs::rename(&previous, staging)?;
    File::open(staging.parent().context("staging has no parent")?)?.sync_all()?;
    Ok(())
}
pub fn seed(
    manager: &ManagedArtifacts,
    device: &str,
    dest: &mut Destination,
    adopt: Option<&Path>,
    fresh: bool,
) -> Result<String> {
    recover_local(manager, device, dest)?;
    // With any pending seed resolved, its staged files are back in staging.
    if fresh {
        for staging in seed_staging(dest)? {
            fs::remove_dir_all(&staging)?;
            println!("Discarded seed staging {}", staging.display());
        }
    }
    let result = seed_inner(manager, device, dest, adopt);
    if result.is_err() {
        recover_local(manager, device, dest)?;
    }
    result
}
pub(super) fn seed_inner(
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
    let mut coverage: Vec<_> = scopes
        .iter()
        .map(|s| Ok(json!({"source":s.source,"prefix":s.prefix(),"stamp":s.measure()?.stamp()})))
        .collect::<Result<_>>()?;
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
        for entry in archive::inventory_excluding(&source, |p| scope.exclude.skips(p))? {
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
    // Files are staged straight onto the destination: no transit parts on the
    // cold store, and only the destination needs room for the compressed scope.
    let staging = dest.directory(".incoming")?.join(&id);
    resume_staging(dest, &staging)?;
    let mut prepared =
        super::outbox::prepare_direct(manager, &scopes, adopt.map(|_| &adopted), &id, &staging)?;
    if prepared.reused.0 > 0 {
        println!(
            "Reused {} staged files ({:.1} GB) from the earlier seed.",
            prepared.reused.0,
            prepared.reused.1 as f64 / 1e9
        );
    }
    // Files that changed or vanished while being read are left out of the
    // confirmed manifest, so the first remote request ships them as changed.
    coverage.retain(|scope| {
        !prepared.deferred.iter().any(|path| {
            path.starts_with(&format!(
                "{}/",
                scope["prefix"].as_str().unwrap_or_default()
            ))
        })
    });
    // Fail early for an existing pull, but do not hold the shared artifact
    // database's writer while verifying or applying files on the backup disk.
    // Confirmation checks again under a write transaction and rolls back the
    // destination if a remote request or another seed won the race.
    check_head(&conn, device, &previous)?;
    let mut journal = Journal {
        id: id.clone(),
        manifest: prepared.manifest.clone(),
        adoption: None,
    };
    dest.set_meta("pending_seed", &serde_json::to_string(&journal)?)?;
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
            for entry in archive::inventory_excluding(&source, |p| plain[0].exclude.skips(p))? {
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
            journal.adoption = Some((source.clone(), target.clone()));
            dest.set_meta("pending_seed", &serde_json::to_string(&journal)?)?;
            fs::rename(&source, &target)
                .context("adopt-plain must be on the destination filesystem")?;
            adoption_move.0 = Some((source.clone(), target.clone()));
            sync_adoption(&source, &target)?;
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
    #[cfg(test)]
    BEFORE_APPLY.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    if let Err(error) = dest.apply_seed(&id, &prepared.transfer, &staging) {
        dest.return_seed_staging(&id, &prepared.manifest.files)
            .context("return staged files after failed seed application")?;
        dest.rollback(&id)
            .context("restore destination after failed seed application")?;
        return Err(error);
    }
    let mut source_committed = false;
    let at = time::OffsetDateTime::now_utc().unix_timestamp();
    let confirmation = (|| -> Result<()> {
        #[cfg(test)]
        AFTER_APPLY.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
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
        let mut conn = manager.store.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_head(&tx, device, &previous)?;
        save_manifest(&tx, device, &prepared.manifest)?;
        confirm(&tx, device, &id)?;
        tx.execute("INSERT INTO backup_requests(id,device,state,base,created_at,ready_at,acked_at,config_json,outbox,manifest_id,parts_json,transfer_json,ack_json) VALUES(?1,?2,'acked',?3,?4,?4,?4,?5,?6,?1,?7,?8,?9)",params![id,device,previous,at,serde_json::to_string(&manager.config)?,prepared.directory.to_string_lossy(),serde_json::to_string(&prepared.parts)?,serde_json::to_string(&prepared.transfer)?,json!({"applied":true,"seed":true,"fetched_bytes":0}).to_string()])?;
        tx.commit()?;
        source_committed = true;
        adoption_move.0 = None;
        #[cfg(test)]
        if FAIL_AFTER_SOURCE_COMMIT.replace(false) {
            anyhow::bail!("injected crash after source commit");
        }
        destination_tx.commit()?;
        Ok(())
    })();
    if let Err(error) = confirmation {
        if !source_committed {
            dest.return_seed_staging(&id, &prepared.manifest.files)
                .context("return staged files after failed seed confirmation")?;
            dest.rollback(&id)
                .context("restore destination after failed seed confirmation")?;
        }
        return Err(error);
    }
    resolve(dest, &journal, true)?;
    // Display stamps re-scan source trees. They do not participate in atomic
    // backup confirmation and must not hold the artifact database writer.
    lifecycle::mark_covered(
        &conn,
        manager,
        &json!({"coverage_json":serde_json::to_string(&coverage)?}),
    )?;
    lifecycle::expire(manager, at)?;
    if staging.exists() {
        fs::remove_dir_all(staging)?;
    }
    Ok(id)
}
