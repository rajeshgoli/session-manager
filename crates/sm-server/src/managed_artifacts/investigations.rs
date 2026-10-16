//! A kernel-held lock distinguishes an interrupted investigation from a live
//! request, including requests in another server during a handover.
use std::{
    fs::{File, OpenOptions},
    os::fd::AsRawFd,
};

use anyhow::Result;

use super::ManagedArtifacts;
use crate::{sessions::SessionStore, work_claims::WorkClaimStore};

pub fn try_lock(manager: &ManagedArtifacts) -> Result<Option<File>> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(manager.store.path.with_extension("investigations.lock"))?;
    // SAFETY: the returned file owns the descriptor and releases the lock on
    // drop or process death. Never unlink the file while requests can use it.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok(Some(lock))
}

/// Git can outlive a crashed server. Its inherited descriptor keeps recovery
/// out until worktree creation (including checkout hooks) actually finishes.
pub fn inherit_lock(command: &mut tokio::process::Command, lock: &File) {
    let fd = lock.as_raw_fd();
    // SAFETY: fcntl is async-signal-safe; the caller retains lock until spawn.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub fn recover(
    manager: &ManagedArtifacts,
    claims: &WorkClaimStore,
    sessions: &SessionStore,
) -> Result<()> {
    let Some(_lock) = try_lock(manager)? else {
        return Ok(());
    };
    // Mark first, release the artifact reservation second. Either operation
    // can be replayed after another crash. A persisted session owns lifecycle.
    claims.recover_interrupted_investigation_worktrees(|id| {
        Ok(sessions.get_session(id)?.is_some())
    })?;
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let reservations = {
        let mut stmt = tx.prepare(
            "SELECT key, value FROM artifact_meta WHERE key LIKE 'investigating:%' AND value <> ''",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (key, id) in reservations {
        if sessions.get_session(&id)?.is_none() {
            tx.execute(
                "DELETE FROM artifact_meta WHERE key=?1 AND value=?2",
                rusqlite::params![key, id],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}
