//! Durable set-wide filesystem operations. Staging is disposable until the
//! journal commits to publication; thereafter recovery only rolls forward.
//! A filesystem lock excludes workers across server handovers as well as threads.
use super::*;
use anyhow::{ensure, Context};
use std::{
    fs::{self, File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::symlink},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Member {
    name: String,
    location: String,
    /// Where a thaw extracts to, decided when it starts copying (sm#2414):
    /// the cold store when it fits there, else the Mac when the Mac keeps
    /// its own headroom. Absent before then and on journals written earlier,
    /// which thaw onto the cold store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thaw_to: Option<String>,
}
impl Member {
    fn thaws_to_root(&self, kind: &str) -> bool {
        kind == "thawing" && self.thaw_to.as_deref() == Some("root")
    }
}
#[derive(Debug)]
struct Operation {
    id: i64,
    root: ArtifactRoot,
    kind: String,
    phase: String,
    members: Vec<Member>,
    actor: String,
}

fn paths(root: &ArtifactRoot, member: &Member, kind: &str) -> (PathBuf, PathBuf, PathBuf) {
    let path = Path::new(&root.path).join(&member.name);
    if member.thaws_to_root(kind) {
        // The extracted folder itself is published at the artifact path.
        let stage = Path::new(&root.path).join(format!(".{}.thawing", member.name));
        return (path.clone(), stage, path);
    }
    let cold = Path::new(&root.cold_store);
    let stage = if kind == "freezing" {
        cold.join(".frozen")
            .join(format!(".{}.freezing", member.name))
    } else {
        cold.join(format!(".{}.{}", member.name, kind))
    };
    let finished = if kind == "freezing" {
        cold.join(".frozen").join(&member.name)
    } else {
        cold.join(&member.name)
    };
    (path, stage, finished)
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
fn rename(from: &Path, to: &Path) -> Result<()> {
    ensure!(
        to.symlink_metadata()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "destination already exists: {}",
        to.display()
    );
    fs::rename(from, to)?;
    sync_rename_parents(from, to)
}
pub(super) fn sync_rename_parents(from: &Path, to: &Path) -> Result<()> {
    sync_dir(to.parent().context("destination parent")?)?;
    if from.parent() != to.parent() {
        sync_dir(from.parent().context("source parent")?)?;
    }
    Ok(())
}
fn mounted(root: &ArtifactRoot) -> Result<()> {
    ensure!(moves::cold_mounted(root), "cold store is not connected");
    ensure!(
        fs::canonicalize(&root.path)? == Path::new(&root.path),
        "artifact root changed"
    );
    ensure!(
        fs::canonicalize(&root.cold_store)? == Path::new(&root.cold_store),
        "cold store path changed"
    );
    let frozen = Path::new(&root.cold_store).join(".frozen");
    if frozen.symlink_metadata().is_ok() {
        ensure!(
            fs::canonicalize(&frozen)? == frozen && fs::symlink_metadata(&frozen)?.is_dir(),
            "archive parent changed"
        );
    }
    Ok(())
}
fn source(root: &ArtifactRoot, m: &Member) -> Result<PathBuf> {
    let path = delete::target(Path::new(&root.path), &m.name)?;
    if m.location == "cold" {
        let cold = delete::target(Path::new(&root.cold_store), &m.name)?;
        ensure!(fs::read_link(&path)? == cold, "cold artifact link changed");
        ensure!(
            fs::symlink_metadata(&cold)?.is_dir(),
            "cold artifact is not a directory"
        );
        Ok(cold)
    } else {
        ensure!(
            fs::symlink_metadata(&path)?.is_dir(),
            "artifact is not a directory"
        );
        Ok(path)
    }
}
fn thaw_not_written(conn: &Connection, root: &str, name: &str) -> Result<()> {
    for r in store::query(
        conn,
        "SELECT outputs_json FROM artifact_job_reservations WHERE root=?1",
        &[&root],
    )? {
        let names: Vec<String> = serde_json::from_str(r["outputs_json"].as_str().unwrap_or("[]"))?;
        ensure!(
            !names.iter().any(|n| n.eq_ignore_ascii_case(name)),
            "{name} is in use by a writer"
        );
    }
    Ok(())
}

/// Claims the whole set in a short transaction. Call kick after the surrounding
/// queue transaction releases its lock; no filesystem copy runs in this call.
pub fn keep_live(
    manager: &ManagedArtifacts,
    root: &ArtifactRoot,
    name: &str,
    actor: &str,
) -> Result<Value> {
    let a = manager.store.detail(&root.path, name)?;
    if a["state"] == "frozen" {
        request(manager, root, name, "thawing", actor)?;
    }
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let a = artifact(&tx, &root.path, name)?;
    ensure!(
        a["state"] == "live" || a["op"] == "thawing",
        "artifact must be live or thawing"
    );
    ensure!(
        a["op"].is_null() || a["op"] == "thawing",
        "artifact is busy"
    );
    let until = (OffsetDateTime::now_utc().date()
        + time::Duration::days(manager.config.keep_live_days))
    .to_string();
    tx.execute(
        "UPDATE artifacts SET keep_live_until=?3 WHERE root=?1 AND set_id=?2",
        params![root.path, a["set_id"].as_str(), until],
    )?;
    event(
        &tx,
        &root.path,
        name,
        "owner_keep_live",
        actor,
        json!({"until":until}),
    )?;
    let result = store::detail(&tx, &root.path, name)?;
    tx.commit()?;
    Ok(result)
}

pub fn request(
    manager: &ManagedArtifacts,
    root: &ArtifactRoot,
    name: &str,
    kind: &str,
    actor: &str,
) -> Result<Value> {
    ensure!(
        matches!(kind, "freezing" | "thawing" | "moving"),
        "invalid artifact operation"
    );
    validate_name(name)?;
    mounted(root)?;
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let first = artifact(&tx, &root.path, name)?;
    if first["op"] == kind {
        // An explicit request adopts sm's queued operation, so the thaw outlives
        // the queue jobs that first asked for it (sm#2500).
        if actor != "sm" {
            tx.execute("UPDATE artifact_operations SET actor=?3 WHERE root=?1 AND set_id=?2 AND phase NOT IN ('done','failed') AND actor='sm'",params![root.path,first["set_id"].as_str(),actor])?;
        }
        let result = store::detail(&tx, &root.path, name)?;
        tx.commit()?;
        return Ok(result);
    }
    let set = first["set_id"].as_str().context("artifact set")?;
    let members = store::query(
        &tx,
        "SELECT * FROM artifacts WHERE root=?1 AND set_id=?2 ORDER BY name",
        &[&root.path, &set],
    )?;
    let mut saved = Vec::new();
    for a in members {
        let n = a["name"].as_str().context("artifact name")?;
        validate_name(n)?;
        ensure!(a["op"].is_null(), "{n} is busy");
        if kind == "thawing" {
            ensure!(a["state"] == "frozen", "{n} is not frozen");
            thaw_not_written(&tx, &root.path, n)?;
        } else {
            let expired = a["state"] == "unrecorded"
                && a["grace_until"]
                    .as_str()
                    .is_some_and(|s| s < now().as_str());
            ensure!(
                a["state"] == "live"
                    || (kind == "freezing" && matches!(actor, "sm" | "sm-nightly") && expired),
                "{n} is not live"
            );
            ensure_not_in_use(&tx, &root.path, n)?;
            if kind == "freezing" {
                ensure!(a["project"] != 1, "{n} is kept for the project");
                ensure!(
                    a["keep_live_until"]
                        .as_str()
                        .is_none_or(|s| s < now().as_str()),
                    "{n} is kept live"
                );
            }
            if kind == "moving" {
                ensure!(a["location"] == "root", "{n} is already on cold store");
            }
        }
        if kind == "freezing" && actor == "sm-nightly" && a["state"] == "live" {
            ensure!(
                !super::hotcold::is_hot(
                    &tx,
                    &root.path,
                    n,
                    manager.config.cold_after_days,
                    OffsetDateTime::now_utc()
                )?,
                "{n} is no longer cold"
            );
        }
        let m = Member {
            name: n.into(),
            location: a["location"].as_str().context("location")?.into(),
            thaw_to: None,
        };
        let (_, stage, finished) = paths(root, &m, kind);
        for path in [
            stage,
            finished,
            Path::new(&root.path).join(format!(".{n}.old")),
            Path::new(&root.path).join(format!(".{n}.placeholder")),
            Path::new(&root.path).join(format!(".{n}.thawing")),
        ] {
            ensure!(
                path.symlink_metadata()
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                "operation destination already exists: {}",
                path.display()
            );
        }
        if kind != "thawing" {
            source(root, &m)?;
        }
        tx.execute(
            "UPDATE artifacts SET op=?3 WHERE root=?1 AND name=?2",
            params![root.path, n, kind],
        )?;
        saved.push(m);
    }
    tx.execute("INSERT INTO artifact_operations(root,set_id,kind,phase,members_json,actor,created_at) VALUES(?1,?2,?3,'queued',?4,?5,?6)", params![root.path,set,kind,serde_json::to_string(&saved)?,actor,now()])?;
    // A queued operation can wait hours behind another set's copy; the event
    // is the requester's evidence that the request landed.
    let id = tx.last_insert_rowid();
    for m in &saved {
        event(
            &tx,
            &root.path,
            &m.name,
            "op_requested",
            actor,
            json!({"operation_id":id,"operation":kind}),
        )?;
    }
    let result = store::detail(&tx, &root.path, name)?;
    tx.commit()?;
    Ok(result)
}

/// Queue jobs still waiting to read a member of the set. Each one would
/// request the thaw again, so a thaw they need is never cancelled.
fn waiting_readers(conn: &Connection, root: &str, members: &[Member]) -> Result<Vec<String>> {
    let mut jobs = Vec::new();
    for r in store::query(
        conn,
        "SELECT job_id,inputs_json FROM artifact_job_reservations WHERE root=?1 ORDER BY job_id",
        &[&root],
    )? {
        let names: Vec<String> = serde_json::from_str(r["inputs_json"].as_str().unwrap_or("[]"))?;
        if names
            .iter()
            .any(|n| members.iter().any(|m| m.name.eq_ignore_ascii_case(n)))
        {
            jobs.push(r["job_id"].as_str().unwrap_or_default().to_owned());
        }
    }
    Ok(jobs)
}
/// Ends a queued operation before it writes anything: no staging folder
/// exists yet and the archive or source is untouched (sm#2500).
fn cancel_queued(
    tx: &Connection,
    id: i64,
    root: &str,
    kind: &str,
    members: &[Member],
    actor: &str,
    reason: &str,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE artifact_operations SET phase='failed',finished_at=?2,error=?3 WHERE id=?1 AND phase='queued'",
        params![id, now(), format!("Cancelled by {actor}: {reason}")],
    )?;
    ensure!(changed == 1, "operation {id} is no longer queued");
    for m in members {
        tx.execute(
            "UPDATE artifacts SET op=NULL WHERE root=?1 AND name=?2 AND op=?3",
            params![root, m.name, kind],
        )?;
        event(
            tx,
            root,
            &m.name,
            "op_cancelled",
            actor,
            json!({"operation_id":id,"operation":kind,"reason":reason}),
        )?;
    }
    Ok(())
}
/// Cancels the set's queued operation. One that has started copying runs to
/// its end; a thaw a waiting queue job reads stays until that job is cancelled.
/// Only the owner cancels what the owner requested.
pub fn cancel(
    manager: &ManagedArtifacts,
    root: &ArtifactRoot,
    name: &str,
    actor: &str,
    owner: bool,
) -> Result<Value> {
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let a = artifact(&tx, &root.path, name)?;
    let set = a["set_id"].as_str().context("artifact set")?;
    let row = store::query(
        &tx,
        "SELECT * FROM artifact_operations WHERE root=?1 AND set_id=?2 AND phase NOT IN ('done','failed')",
        &[&root.path, &set],
    )?
    .pop()
    .with_context(|| format!("{name} has no operation to cancel"))?;
    let id = row["id"].as_i64().context("operation id")?;
    let kind = row["kind"].as_str().context("kind")?;
    let phase = row["phase"].as_str().context("phase")?;
    ensure!(
        phase == "queued",
        "operation {id} is {phase}; only a queued operation can be cancelled"
    );
    let requester = row["actor"].as_str().unwrap_or("sm");
    ensure!(
        owner || !requester.contains('@'),
        "operation {id} was requested by the owner; only the owner can cancel it"
    );
    let members: Vec<Member> =
        serde_json::from_str(row["members_json"].as_str().context("members")?)?;
    if kind == "thawing" {
        let jobs = waiting_readers(&tx, &root.path, &members)?;
        ensure!(
            jobs.is_empty(),
            "queue jobs {} still wait on this thaw; cancel them first",
            jobs.join(", ")
        );
    }
    cancel_queued(
        &tx,
        id,
        &root.path,
        kind,
        &members,
        actor,
        "requested cancellation",
    )?;
    let result = store::detail(&tx, &root.path, name)?;
    tx.commit()?;
    Ok(result)
}
/// A thaw sm requested for queue jobs ends once none of them waits on it,
/// so a cancelled job leaves no restore running behind it (sm#2500).
fn abandon_unneeded_thaw(manager: &ManagedArtifacts, op: &Operation) -> Result<bool> {
    if op.kind != "thawing" || op.phase != "queued" || op.actor != "sm" {
        return Ok(false);
    }
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let phase: String = tx.query_row(
        "SELECT phase FROM artifact_operations WHERE id=?1",
        [op.id],
        |r| r.get(0),
    )?;
    if phase != "queued" || !waiting_readers(&tx, &op.root.path, &op.members)?.is_empty() {
        return Ok(false);
    }
    cancel_queued(
        &tx,
        op.id,
        &op.root.path,
        &op.kind,
        &op.members,
        "sm",
        "no waiting queue job reads it",
    )?;
    tx.commit()?;
    Ok(true)
}

/// Splits a freeze archive whose source sits on the cold store's own disk into
/// what stays there for good and what it only borrows (sm#2488): publication
/// removes the source, so only an archive larger than its source grows the disk.
pub(super) fn split_archive(archive: u64, source_disk: u64) -> (u64, u64) {
    (
        archive.saturating_sub(source_disk),
        archive.min(source_disk),
    )
}
/// Whether the cold store takes `kept` bytes for good plus `borrowed` bytes it
/// gets back when the operation publishes. Kept bytes must leave the floor
/// intact; borrowed bytes need only room, so a freeze on a disk already below
/// its floor can still run and lift it back (sm#2488).
pub(super) fn fits_on_cold(
    free: u64,
    floor: u64,
    outstanding: u64,
    kept: u64,
    borrowed: u64,
) -> bool {
    let room = free.saturating_sub(outstanding);
    room.saturating_sub(floor) >= kept && room >= kept.saturating_add(borrowed)
}
/// The disk the operation writes to, or `None` while neither has room. Only a
/// thaw may fall back to the Mac (sm#2414).
fn space(manager: &ManagedArtifacts, op: &Operation) -> Result<Option<&'static str>> {
    let conn = manager.store.open()?;
    let shared_disk = queue::fs_stats(&op.root.path, manager.config.floor_percent)?.2
        == queue::fs_stats(&op.root.cold_store, manager.config.floor_percent)?.2;
    let mut required = 0u64;
    let mut borrowed = 0u64;
    for m in &op.members {
        let a = artifact(&conn, &op.root.path, &m.name)?;
        let (kept, lent) = if op.kind == "thawing" {
            let bytes = archive::Manifest::read(
                &Path::new(&op.root.cold_store).join(".frozen").join(&m.name),
            )?
            .data_bytes;
            (bytes, 0)
        } else {
            let size = scan::measure(&source(&op.root, m)?)?;
            if op.kind == "freezing" {
                let ratio = a["ratio_estimate"]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .unwrap_or(1.0);
                let bytes = (size.data_bytes as f64 / ratio * 1.2).ceil() as u64;
                if m.location == "cold" || shared_disk {
                    split_archive(bytes, size.disk_bytes)
                } else {
                    (bytes, 0)
                }
            } else {
                (size.disk_bytes.max(size.data_bytes), 0)
            }
        };
        required = required
            .checked_add(kept)
            .context("operation size overflow")?;
        borrowed = borrowed
            .checked_add(lent)
            .context("operation size overflow")?;
    }
    let disks = queue::disk_status(manager, &op.root)?;
    let cold = disks
        .iter()
        .find(|d| d["role"] == "cold")
        .context("cold disk status")?;
    if fits_on_cold(
        cold["free"].as_u64().unwrap_or(0),
        cold["floor"].as_u64().unwrap_or(0),
        cold["outstanding"].as_u64().unwrap_or(0),
        required,
        borrowed,
    ) {
        Ok(Some("cold"))
    } else if op.kind == "thawing"
        && super::pressure::thaw_fits_on_root(&conn, manager, &op.root, required)?
    {
        Ok(Some("root"))
    } else {
        Ok(None)
    }
}
fn stage(manager: &ManagedArtifacts, op: &mut Operation) -> Result<bool> {
    let Some(disk) = space(manager, op)? else {
        manager.store.open()?.execute("UPDATE artifact_operations SET error='Waiting for disk space above the floor and reservations' WHERE id=?1 AND phase=?2",params![op.id, op.phase])?;
        return Ok(false);
    };
    store::set_meta(
        &manager.store.open()?,
        &format!("operation_started:{}", op.id),
        &OffsetDateTime::now_utc().unix_timestamp_nanos().to_string(),
    )?;
    if op.kind == "thawing" {
        for m in &mut op.members {
            m.thaw_to = Some(disk.into());
        }
    }
    // The destination is journaled with the phase, so recovery discards the
    // right staging folder and publishes where the copy went.
    // A cancellation that landed while this pass measured space wins.
    if manager.store.open()?.execute(
        "UPDATE artifact_operations SET phase='copying',members_json=?2,error=NULL WHERE id=?1 AND phase=?3",
        params![op.id, serde_json::to_string(&op.members)?, op.phase],
    )? == 0
    {
        return Ok(false);
    }
    for m in &op.members {
        mounted(&op.root)?;
        let (_, staging, _) = paths(&op.root, m, &op.kind);
        if op.kind == "freezing" {
            let source = source(&op.root, m)?;
            moves::remove_finder_files(&source)?;
            let parent = staging.parent().context("archive parent")?;
            if !parent.exists() {
                fs::create_dir(parent)?;
                sync_dir(Path::new(&op.root.cold_store))?;
            }
            ensure!(
                fs::symlink_metadata(parent)?.is_dir(),
                "archive parent is not a directory"
            );
            archive::pack(&source, &staging, &m.name, &manager.config)?;
        } else if op.kind == "thawing" {
            let frozen = Path::new(&op.root.cold_store).join(".frozen").join(&m.name);
            let manifest = archive::Manifest::read(&frozen)?;
            ensure!(
                manifest.name == m.name,
                "archive belongs to another artifact"
            );
            archive::extract(
                &frozen,
                &staging,
                &manifest,
                manager.config.thaw_parallel_parts,
            )?;
        } else {
            let source = source(&op.root, m)?;
            moves::remove_finder_files(&source)?;
            moves::copy_verified(&source, &staging)?;
        }
        sync_dir(staging.parent().context("staging parent")?)?;
    }
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for m in &op.members {
        if op.kind != "thawing" {
            ensure_not_in_use(&tx, &op.root.path, &m.name)?;
            if op.kind == "freezing" {
                let current = artifact(&tx, &op.root.path, &m.name)?;
                ensure!(
                    current["project"] != 1,
                    "{} is kept for the project",
                    m.name
                );
                ensure!(
                    current["keep_live_until"]
                        .as_str()
                        .is_none_or(|s| s < now().as_str()),
                    "{} is kept live",
                    m.name
                );
                if op.actor == "sm-nightly" && current["state"] == "live" {
                    ensure!(
                        !super::hotcold::is_hot(
                            &tx,
                            &op.root.path,
                            &m.name,
                            manager.config.cold_after_days,
                            OffsetDateTime::now_utc()
                        )?,
                        "{} is no longer cold",
                        m.name
                    );
                }
                let (_, staging, _) = paths(&op.root, m, &op.kind);
                let manifest = archive::Manifest::read(&staging)?;
                archive::verify_source(&source(&op.root, m)?, &manifest.entries)?;
                tx.execute("UPDATE artifacts SET state='frozen',frozen_at=?3,frozen_bytes=?4,content_stamp=?5,data_bytes=?6 WHERE root=?1 AND name=?2",params![op.root.path,m.name,manifest.frozen_at,manifest.parts.iter().map(|p| p.output_bytes).sum::<u64>(),manifest.content_stamp,manifest.data_bytes])?;
                let bytes = manifest
                    .parts
                    .iter()
                    .map(|p| p.output_bytes)
                    .sum::<u64>()
                    .max(1);
                tx.execute("UPDATE artifacts SET ratio_estimate=?3,ratio_sampled_at=?4 WHERE root=?1 AND name=?2",params![op.root.path,m.name,manifest.data_bytes as f64 / bytes as f64,now()])?;
                store::set_meta(
                    &tx,
                    &format!("ratio_stamp:{}:{}", op.root.path, m.name),
                    &manifest.content_stamp,
                )?;
            }
        } else {
            thaw_not_written(&tx, &op.root.path, &m.name)?;
        }
    }
    // Once prepared, a crash must finish publication, never discard staging.
    tx.execute(
        "UPDATE artifact_operations SET phase='prepared' WHERE id=?1",
        [op.id],
    )?;
    tx.commit()?;
    Ok(true)
}

fn valid_published(path: &Path, target: Option<&Path>) -> bool {
    if let Some(target) = target {
        fs::read_link(path).is_ok_and(|p| p == target)
    } else {
        path.symlink_metadata().is_ok_and(|m| m.is_dir()) && path.join("FROZEN.md").is_file()
    }
}
/// A thaw onto the Mac is published once its staging folder has become the
/// artifact's own directory.
fn valid_root_thaw(path: &Path, staging: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|m| m.is_dir()) && staging.symlink_metadata().is_err()
}
fn swap(op: &Operation, m: &Member, target: Option<&Path>) -> Result<()> {
    let root = Path::new(&op.root.path);
    let path = root.join(&m.name);
    let old = root.join(format!(".{}.old", m.name));
    let placeholder = root.join(format!(".{}.placeholder", m.name));
    if old.symlink_metadata().is_ok() && valid_published(&path, target) {
        return Ok(());
    }
    if path.symlink_metadata().is_ok() {
        rename(&path, &old)?;
    }
    if let Some(target) = target {
        if placeholder.symlink_metadata().is_err() {
            symlink(target, &placeholder)?;
        }
        ensure!(
            fs::read_link(&placeholder)? == target,
            "replacement link changed"
        );
    } else {
        if placeholder.symlink_metadata().is_err() {
            fs::create_dir(&placeholder)?;
        }
        ensure!(
            fs::symlink_metadata(&placeholder)?.is_dir(),
            "replacement is not a directory"
        );
        let manifest =
            archive::Manifest::read(&Path::new(&op.root.cold_store).join(".frozen").join(&m.name))?;
        // Rewrite even after an interrupted write; publication never exposes a
        // partial FROZEN.md. The temporary directory belongs to this journal.
        fs::write(placeholder.join("FROZEN.md"), format!("# {} is frozen\nFrozen {} by sm. {} bytes of data in {} bytes of archives.\nThaw: sm artifact thaw {} --wait (about {:.0} seconds)\nQueue jobs thaw it automatically when they name its path or pass --input {}.\nRecord: sm artifact show {}\n",m.name,manifest.frozen_at,manifest.data_bytes,manifest.parts.iter().map(|p|p.output_bytes).sum::<u64>(),m.name,manifest.data_bytes as f64 / (100_000_000_000.0 / 60.0),m.name,m.name))?;
        File::open(placeholder.join("FROZEN.md"))?.sync_all()?;
        sync_dir(&placeholder)?;
    }
    rename(&placeholder, &path)?;
    Ok(())
}
fn publish(manager: &ManagedArtifacts, op: &Operation) -> Result<()> {
    for m in &op.members {
        mounted(&op.root)?;
        let (path, staging, finished) = paths(&op.root, m, &op.kind);
        if m.thaws_to_root(&op.kind) {
            // The extracted folder replaces FROZEN.md's folder at the path itself.
            if staging.symlink_metadata().is_ok() {
                let old = Path::new(&op.root.path).join(format!(".{}.old", m.name));
                if path.symlink_metadata().is_ok() {
                    rename(&path, &old)?;
                }
                rename(&staging, &path)?;
            }
            ensure!(
                valid_root_thaw(&path, &staging),
                "published destination is not a directory"
            );
            continue;
        }
        if staging.symlink_metadata().is_ok() {
            rename(&staging, &finished)?;
        }
        ensure!(
            finished.symlink_metadata()?.is_dir(),
            "published destination is not a directory"
        );
        if op.kind == "freezing" {
            swap(op, m, None)?;
        } else {
            swap(op, m, Some(&finished))?;
        }
    }
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for m in &op.members {
        if op.kind == "thawing" {
            let location = if m.thaws_to_root(&op.kind) {
                "root"
            } else {
                "cold"
            };
            tx.execute("UPDATE artifacts SET state='live',location=?4,thawed_at=?3,last_used_at=?3 WHERE root=?1 AND name=?2",params![op.root.path,m.name,now(),location])?;
        } else if op.kind == "moving" {
            tx.execute(
                "UPDATE artifacts SET location='cold' WHERE root=?1 AND name=?2",
                params![op.root.path, m.name],
            )?;
        }
    }
    tx.execute(
        "UPDATE artifact_operations SET phase='cleanup' WHERE id=?1",
        [op.id],
    )?;
    tx.commit()?;
    cleanup(manager, op)
}
fn cleanup(manager: &ManagedArtifacts, op: &Operation) -> Result<()> {
    mounted(&op.root)?;
    for m in &op.members {
        let (path, staging, finished) = paths(&op.root, m, &op.kind);
        ensure!(
            if m.thaws_to_root(&op.kind) {
                valid_root_thaw(&path, &staging)
            } else {
                valid_published(
                    &path,
                    if op.kind == "freezing" {
                        None
                    } else {
                        Some(&finished)
                    },
                )
            },
            "artifact publication changed before cleanup"
        );
        delete::remove_entry(&Path::new(&op.root.path).join(format!(".{}.old", m.name)))?;
        if op.kind == "freezing" && m.location == "cold" {
            delete::remove_entry(&delete::target(Path::new(&op.root.cold_store), &m.name)?)?;
        } else if op.kind == "thawing" {
            delete::remove_entry(&Path::new(&op.root.cold_store).join(".frozen").join(&m.name))?;
        }
    }
    sync_dir(Path::new(&op.root.path))?;
    sync_dir(Path::new(&op.root.cold_store))?;
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for m in &op.members {
        tx.execute(
            "UPDATE artifacts SET op=NULL WHERE root=?1 AND name=?2",
            params![op.root.path, m.name],
        )?;
        // History keeps the sizes this operation saw after later ones change them.
        let a = artifact(&tx, &op.root.path, &m.name)?;
        let frozen = if op.kind == "freezing" {
            a["frozen_bytes"].clone()
        } else {
            Value::Null
        };
        event(
            &tx,
            &op.root.path,
            &m.name,
            match op.kind.as_str() {
                "freezing" => "frozen",
                "thawing" => "thawed",
                _ => "moved",
            },
            &op.actor,
            json!({"operation_id":op.id,"data_bytes":a["data_bytes"],"frozen_bytes":frozen}),
        )?;
    }
    if op.kind == "thawing" {
        let started = store::meta(&tx, &format!("operation_started:{}", op.id))?
            .and_then(|s| s.parse::<i128>().ok());
        if let Some(started) = started {
            let seconds = (OffsetDateTime::now_utc().unix_timestamp_nanos() - started) as f64
                / 1_000_000_000.0;
            let mut bytes = 0u64;
            for member in &op.members {
                bytes += artifact(&tx, &op.root.path, &member.name)?["data_bytes"]
                    .as_u64()
                    .unwrap_or(0);
            }
            if seconds > 0.0 && bytes > 0 {
                event(
                    &tx,
                    &op.root.path,
                    &op.members[0].name,
                    "thaw_measured",
                    &op.actor,
                    json!({"seconds":seconds,"data_bytes":bytes,"operation_id":op.id}),
                )?;
            }
        }
    }
    tx.execute(
        "UPDATE artifact_operations SET phase='done',finished_at=?2,error=NULL WHERE id=?1",
        params![op.id, now()],
    )?;
    tx.commit()?;
    Ok(())
}
fn discard_staging(op: &Operation) -> Result<()> {
    for m in &op.members {
        let (_, staging, _) = paths(&op.root, m, &op.kind);
        delete::remove_entry(&staging)?;
    }
    Ok(())
}
fn run(manager: &ManagedArtifacts, op: &mut Operation) -> Result<()> {
    mounted(&op.root)?;
    match op.phase.as_str() {
        "queued" | "copying" => {
            if op.phase == "copying" {
                discard_staging(op)?;
            }
            if stage(manager, &mut *op)? {
                publish(manager, op)
            } else {
                Ok(())
            }
        }
        "prepared" => publish(manager, op),
        "cleanup" => cleanup(manager, op),
        _ => bail!("invalid artifact operation phase"),
    }
}

/// Serializes filesystem operations across server processes. Archive part work
/// remains parallel; one set at a time stays below max_concurrent_ops.
pub fn resume(manager: &ManagedArtifacts) -> Result<()> {
    let conn = manager.store.open()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(manager.store.path.with_extension("operations.lock"))?;
    // SAFETY: the file owns its descriptor until all operation work finishes.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(());
        }
        return Err(error.into());
    }
    for root in &manager.config.roots {
        manager.recover_deletions(root)?;
        if !moves::cold_mounted(root) {
            continue;
        }
        for a in store::query(&conn, "SELECT DISTINCT a.name FROM artifacts a JOIN artifact_events e ON e.root=a.root AND e.name=a.name WHERE a.root=?1 AND a.state='frozen' AND a.op IS NULL AND e.kind='thaw_requested' AND EXISTS(SELECT 1 FROM artifact_job_reservations r, json_each(r.inputs_json) i WHERE r.root=a.root AND i.value=a.name COLLATE NOCASE)", &[&root.path])? {
            let _ = request(manager, root, a["name"].as_str().context("thaw name")?, "thawing", "sm");
        }
    }
    // A request made while this pass holds the lock finds it taken and returns;
    // look again after each batch so it runs now, not at the next scan (sm#2325).
    let mut after = 0i64;
    loop {
        let rows = store::query(
        &conn,
        "SELECT * FROM artifact_operations WHERE phase NOT IN ('done','failed') AND id>?1 ORDER BY id",
        &[&after],
    )?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            after = row["id"].as_i64().context("operation id")?;
            let root = manager
                .config
                .roots
                .iter()
                .find(|r| Some(r.path.as_str()) == row["root"].as_str())
                .context("operation root no longer configured")?
                .clone();
            if !moves::cold_mounted(&root) {
                continue;
            }
            let mut op = Operation {
                id: row["id"].as_i64().context("operation id")?,
                root,
                kind: row["kind"].as_str().context("kind")?.into(),
                phase: row["phase"].as_str().context("phase")?.into(),
                members: serde_json::from_str(row["members_json"].as_str().context("members")?)?,
                actor: row["actor"].as_str().unwrap_or("sm").into(),
            };
            if abandon_unneeded_thaw(manager, &op)? {
                continue;
            }
            if let Err(error) = run(manager, &mut op) {
                let mut db = manager.store.open()?;
                let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let phase: String = tx.query_row(
                    "SELECT phase FROM artifact_operations WHERE id=?1",
                    [op.id],
                    |r| r.get(0),
                )?;
                if phase == "failed" {
                    // Cancelled while this pass worked on it; nothing was written.
                    continue;
                }
                // Committed operations retain both their journal and op exclusion.
                // Their remaining swaps/cleanup are idempotent on the next pass.
                let abort =
                    matches!(phase.as_str(), "queued" | "copying") && moves::cold_mounted(&op.root);
                if abort {
                    discard_staging(&op)?;
                }
                for m in &op.members {
                    if abort {
                        tx.execute(
                            "UPDATE artifacts SET op=NULL WHERE root=?1 AND name=?2",
                            params![op.root.path, m.name],
                        )?;
                    }
                    event(
                        &tx,
                        &op.root.path,
                        &m.name,
                        "op_failed",
                        &op.actor,
                        json!({"operation_id":op.id,"operation":op.kind,"error":format!("{error:#}")}),
                    )?;
                }
                tx.execute("UPDATE artifact_operations SET error=?2,phase=CASE WHEN ?3 THEN 'failed' ELSE phase END,finished_at=CASE WHEN ?3 THEN ?4 ELSE finished_at END WHERE id=?1",params![op.id,format!("{error:#}"),abort,now()])?;
                tx.commit()?;
            }
        }
    }
    Ok(())
}
/// Persist the rate limit only after the owner inbox accepts the message.
pub fn notify_missing(
    manager: &ManagedArtifacts,
    root: &ArtifactRoot,
    at: OffsetDateTime,
    mut send: impl FnMut(&str, &str) -> Result<()>,
) -> Result<()> {
    if moves::cold_mounted(root) {
        return Ok(());
    }
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let key = format!("cold_missing:{}", root.path);
    if store::meta(&tx, &key)?
        .and_then(|s| s.parse::<i64>().ok())
        .is_some_and(|last| at.unix_timestamp() - last < 6 * 3600)
    {
        return Ok(());
    }
    let frozen: i64 = tx.query_row(
        "SELECT COUNT(*) FROM artifacts WHERE root=?1 AND state='frozen'",
        [&root.path],
        |r| r.get(0),
    )?;
    let moved: i64 = tx.query_row("SELECT COUNT(*) FROM artifacts WHERE root=?1 AND location='cold' AND state IN ('live','building')",[&root.path],|r|r.get(0))?;
    if frozen + moved == 0 {
        return Ok(());
    }
    let delivery = format!("{key}:{}", at.unix_timestamp());
    send(&delivery,&format!("The cold store is not connected; {frozen} frozen and {moved} moved artifacts are unreadable."))?;
    store::set_meta(&tx, &key, &at.unix_timestamp().to_string())?;
    tx.commit()?;
    Ok(())
}

pub struct QueueOperations;
impl queue::Operations for QueueOperations {
    fn thaw(&self, manager: &ManagedArtifacts, _root: &ArtifactRoot, _name: &str) -> Result<()> {
        kick(manager.clone());
        Ok(())
    }
    fn pressure(
        &self,
        manager: &ManagedArtifacts,
        root: &ArtifactRoot,
        available_target: u64,
    ) -> Result<()> {
        super::pressure::kick(manager.clone(), root.clone(), available_target);
        Ok(())
    }
}

pub fn kick(manager: ManagedArtifacts) {
    std::thread::spawn(move || {
        if let Err(error) = resume(&manager) {
            eprintln!("artifact operation deferred: {error:#}");
        }
    });
}

#[cfg(test)]
mod tests;
