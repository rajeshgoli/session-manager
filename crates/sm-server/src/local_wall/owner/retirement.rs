//! A new immutable launch may replace a retired one, never a live authority.
use super::*;

#[derive(Serialize, Deserialize)]
struct Lifecycle {
    configuration_sha256: String,
    boot_seconds: Option<i64>,
}
fn lifecycle(config: &Configuration) -> Result<Lifecycle> {
    Ok(Lifecycle {
        configuration_sha256: format!("{:x}", Sha256::digest(serde_json::to_vec(config)?)),
        boot_seconds: crate::host_restart::system_boot_time().map(|boot| boot.unix_timestamp()),
    })
}
fn read_lifecycle(path: &Path) -> Result<Option<Lifecycle>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 4096
    {
        bail!("invalid owner lifecycle receipt")
    }
    Ok(Some(serde_json::from_reader(file.take(4097))?))
}
pub(super) fn stage_lock(root: &Path, operation: i32) -> Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(root.join("stage.lock"))?;
    let metadata = lock.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || unsafe { libc::flock(lock.as_raw_fd(), operation) } != 0
    {
        bail!("durable owner is still active or its staging lock is invalid")
    }
    Ok(lock)
}
pub(super) fn record_started(config: &Configuration) -> Result<()> {
    let root = directory(&config.queue, &config.agent.id)?;
    atomic_write(
        &root.join("started.json"),
        &serde_json::to_vec(&lifecycle(config)?)?,
    )?;
    let retired = root.join("retired.json");
    if read_lifecycle(&retired)?.is_some() {
        fs::remove_file(retired)?;
        File::open(&root)?.sync_all()?;
    }
    Ok(())
}
pub(super) fn record_staged(config: &Configuration) -> Result<()> {
    let root = directory(&config.queue, &config.agent.id)?;
    for name in ["started.json", "retired.json"] {
        let path = root.join(name);
        if read_lifecycle(&path)?.is_some() {
            fs::remove_file(path)?;
        }
    }
    atomic_write(
        &root.join("staged.json"),
        &serde_json::to_vec(&lifecycle(config)?)?,
    )
}
pub(super) fn record_retired(config: &Configuration) -> Result<()> {
    atomic_write(
        &directory(&config.queue, &config.agent.id)?.join("retired.json"),
        &serde_json::to_vec(&lifecycle(config)?)?,
    )
}

/// The host first stops the tmux launcher so it cannot restart this owner.
/// No HTTP caller can invoke this. The owner lifetime lock excludes a running
/// or starting owner; a matching clean receipt (or a later kernel boot) proves
/// its provider and queued descendants are gone. Pending queue jobs refuse the
/// transition because they still depend on the old immutable registration.
/// Conversation files, logs and staged executables remain for diagnosis.
pub fn retire_for_restaging(queue: &Path, agent: &str) -> Result<()> {
    let _admission = crate::queue::admission_guard();
    physical_directory(queue)?;
    let root = directory(queue, agent)?;
    let path = root.join("launch.json");
    if !path.try_exists()? {
        return Ok(());
    }
    if root.canonicalize()? != root {
        bail!("owner state has aliases")
    }
    let _stage = stage_lock(&root, libc::LOCK_EX | libc::LOCK_NB)?;
    let config = read_configuration(&path)?;
    if config.queue != queue || config.agent.id != agent {
        bail!("owner retirement identity changed")
    }
    let expected = lifecycle(&config)?;
    let staged = read_lifecycle(&root.join("staged.json"))?;
    let started = read_lifecycle(&root.join("started.json"))?;
    let retired = read_lifecycle(&root.join("retired.json"))?;
    for receipt in staged.iter().chain(started.iter()).chain(retired.iter()) {
        if receipt.configuration_sha256 != expected.configuration_sha256 {
            bail!("owner lifecycle receipt belongs to another launch")
        }
    }
    if staged.is_none() && started.is_none() && retired.is_none() {
        bail!("legacy owner has no lifecycle proof; retirement is unproven")
    }
    let rebooted = started
        .as_ref()
        .and_then(|receipt| receipt.boot_seconds)
        .zip(expected.boot_seconds)
        .is_some_and(|(old, now)| old < now);
    if started.is_some() && retired.is_none() && !rebooted {
        bail!("owner retirement is unproven; its descendants may still be running")
    }
    // Exclude old generation-owned preparations too, including ones which
    // predate the durable owner's shared staging lock.
    let state = config.host.state_root.join(agent);
    if state.canonicalize()? != state {
        bail!("agent state has aliases")
    }
    let config_dir = state.join("xdg/config");
    let preparation = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(config_dir.join("host.lock"))?;
    let metadata = preparation.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || unsafe { libc::flock(preparation.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
    {
        bail!("agent wall preparation is still active or its lock is invalid")
    }
    crate::queue::local_wall::retire_registration(queue, agent)?;
    // Remove launch authority before its receipts. If cleanup is interrupted,
    // ordinary stage can write fresh proof only while holding this same lock.
    fs::remove_file(path)?;
    File::open(&root)?.sync_all()?;
    for name in ["retired.json", "started.json", "staged.json"] {
        let receipt = root.join(name);
        if receipt.try_exists()? {
            fs::remove_file(receipt)?;
        }
    }
    File::open(root)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
#[path = "retirement_tests.rs"]
mod tests;
