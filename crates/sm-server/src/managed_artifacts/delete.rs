//! Delete only direct children of pinned roots; never follow a tree's symlinks.
use super::*;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub fn target(parent: &Path, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    if fs::canonicalize(parent)? != parent {
        bail!("artifact parent changed: {}", parent.display());
    }
    let path = parent.join(name);
    if path.parent() != Some(parent) {
        bail!("target is outside artifact roots");
    }
    Ok(path)
}
pub(super) fn remove_entry(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !metadata.is_dir() {
        return fs::remove_file(path);
    }
    // remove_dir_all does not follow symlinks and uses descriptor-relative removal
    // on Unix, protecting descendants from symlink replacement during traversal.
    remove_directory(path, |path| fs::remove_dir_all(path))
}
fn remove_directory(
    path: &Path,
    remove_tree: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    match remove_tree(path) {
        Err(e) if e.raw_os_error() == Some(libc::ENOTEMPTY) => {
            match fs::remove_file(path.join(".DS_Store")) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            };
            fs::remove_dir(path)
        }
        result => result,
    }
}
impl ManagedArtifacts {
    pub fn delete(
        &self,
        root: &ArtifactRoot,
        name: &str,
        actor: &str,
        reason: &str,
    ) -> Result<Value> {
        self.delete_inner(root, name, actor, reason, false, false)
    }
    /// Called only after the owning session has ended. Rechecks age and use
    /// under the same write transaction as an explicit deletion.
    pub fn delete_orphan(&self, root: &ArtifactRoot, name: &str) -> Result<Value> {
        self.delete_inner(root, name, "sm", "orphan", true, false)
    }
    pub fn recover_deletions(&self, root: &ArtifactRoot) -> Result<()> {
        for a in self.store.list(&root.path)? {
            if a["op"] == "deleting" {
                let _ = self.delete_inner(
                    root,
                    a["name"].as_str().unwrap(),
                    a["deleted_by"].as_str().unwrap_or("sm"),
                    a["delete_reason"]
                        .as_str()
                        .unwrap_or("interrupted deletion"),
                    false,
                    true,
                );
            }
        }
        Ok(())
    }
    fn delete_inner(
        &self,
        root: &ArtifactRoot,
        name: &str,
        actor: &str,
        reason: &str,
        automatic: bool,
        recovering: bool,
    ) -> Result<Value> {
        validate_name(name)?;
        if reason.trim().is_empty() {
            bail!("deletion requires a reason");
        }
        let mut conn = self.store.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Fetch and re-check inside the write transaction immediately before removal.
        let a = artifact(&tx, &root.path, name)?;
        let members = store::query(
            &tx,
            "SELECT * FROM artifacts WHERE root=?1 AND set_id=?2",
            &[&root.path, &a["set_id"].as_str().unwrap_or(name)],
        )?;
        for member in &members {
            let n = member["name"].as_str().unwrap();
            if automatic {
                let created =
                    OffsetDateTime::parse(member["created_at"].as_str().unwrap_or(""), &Rfc3339)?;
                if member["state"] != "building"
                    || (OffsetDateTime::now_utc() - created).whole_days()
                        < self.config.orphan_building_days
                {
                    bail!("{n} is not an expired orphan build");
                }
            }
            if member["state"] == "deleted"
                || !(member["op"].is_null() || recovering && member["op"] == "deleting")
            {
                bail!("{n} is deleted or busy");
            }
            if (member["state"] == "frozen" || member["location"] == "cold")
                && !super::moves::cold_mounted(root)
            {
                bail!("cold store is not connected");
            }
            if member["project"] == 1 {
                bail!("{n} is kept for the project; remove the mark first");
            }
            ensure_not_in_use(&tx, &root.path, n)?;
            let dependents=store::query(&tx,"SELECT DISTINCT i.name FROM artifact_inputs i JOIN artifacts a ON a.root=i.root AND a.name=i.name WHERE i.root=?1 AND i.input=?2 AND a.state!='deleted'",&[&root.path,&n])?;
            if !dependents.is_empty() {
                bail!("{n} is an input of {}", json!(dependents));
            }
            let failed=store::query(&tx,"SELECT at FROM artifact_events WHERE root=?1 AND name=?2 AND kind='op_failed' AND at>=?3",&[&root.path,&n,&format!("{}T00:00:00Z",OffsetDateTime::now_utc().date())])?;
            if !failed.is_empty() {
                bail!("{n} deletion failed today; inspect the failure before retrying tomorrow");
            }
        }
        if automatic && self.config.deletion_mode == "report_only" {
            for member in &members {
                let n = member["name"].as_str().unwrap();
                let exists:i64=tx.query_row("SELECT COUNT(*) FROM artifact_events WHERE root=?1 AND name=?2 COLLATE NOCASE AND kind='would_delete'",params![root.path,n],|r|r.get(0))?;
                if exists == 0 {
                    event(
                        &tx,
                        &root.path,
                        n,
                        "would_delete",
                        "sm",
                        json!({"reason":"orphan"}),
                    )?;
                }
            }
            let result = store::detail(&tx, &root.path, name)?;
            tx.commit()?;
            return Ok(result);
        }
        for member in &members {
            tx.execute("UPDATE artifacts SET op='deleting',deleted_by=?3,delete_reason=COALESCE(CASE WHEN ?5 THEN delete_reason END,?4) WHERE root=?1 AND name=?2",params![root.path,member["name"].as_str().unwrap(),actor,if automatic {"orphan"} else {"agent"},recovering])?;
        }
        tx.commit()?;
        // The durable op excludes new submissions. Recheck live use under the
        // removal transaction, including after recovery of an interrupted delete.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for member in &members {
            ensure_not_in_use(&tx, &root.path, member["name"].as_str().unwrap())?;
        }
        let result = (|| {
            for member in &members {
                let n = member["name"].as_str().unwrap();
                let path = target(Path::new(&root.path), n)?;
                let mut paths = Vec::new();
                if member["state"] == "frozen" {
                    let frozen = Path::new(&root.cold_store).join(".frozen");
                    paths.push(target(&frozen, n)?);
                } else if member["location"] == "cold" {
                    let cold = target(Path::new(&root.cold_store), n)?;
                    if let Ok(meta) = path.symlink_metadata() {
                        if !meta.file_type().is_symlink() || fs::read_link(&path)? != cold {
                            bail!("cold artifact link changed: {n}");
                        }
                    }
                    paths.push(cold);
                }
                paths.push(path);
                let aliases = store::query(
                    &tx,
                    "SELECT alias FROM artifact_aliases WHERE root=?1 AND target_name=?2",
                    &[&root.path, &n],
                )?;
                for alias in aliases {
                    let alias = target(Path::new(&root.path), alias["alias"].as_str().unwrap())?;
                    if let Ok(metadata) = alias.symlink_metadata() {
                        if !metadata.file_type().is_symlink() {
                            bail!("artifact alias is no longer a symlink: {}", alias.display());
                        }
                        paths.push(alias);
                    }
                }
                for path in paths {
                    remove_entry(&path)?;
                }
            }
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(error) = result {
            for member in &members {
                event(
                    &tx,
                    &root.path,
                    member["name"].as_str().unwrap(),
                    "op_failed",
                    actor,
                    json!({"operation":"delete","error":error.to_string()}),
                )?;
            }
            tx.commit()?;
            return Err(error);
        }
        for member in &members {
            let n = member["name"].as_str().unwrap();
            tx.execute("UPDATE artifacts SET state='deleted',op=NULL,deleted_at=?3,deleted_by=?4,rebuildable=?5 WHERE root=?1 AND name=?2",params![root.path,n,now(),actor,member["recipe_complete"]==1 && !store::has_gpu(member)])?;
            tx.execute(
                "DELETE FROM artifact_aliases WHERE root=?1 AND target_name=?2",
                params![root.path, n],
            )?;
            event(
                &tx,
                &root.path,
                n,
                "deleted",
                actor,
                json!({"reason":reason}),
            )?;
        }
        let result = store::detail(&tx, &root.path, name)?;
        tx.commit()?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_artifacts_ds_store_retry_removes_only_finder_residue() {
        let f = crate::managed_artifacts::tests::Fixture::new();
        let path = f.path("finder");
        fs::create_dir(&path).unwrap();
        remove_directory(&path, |path| {
            fs::write(path.join(".DS_Store"), "raced with removal")?;
            Err(io::Error::from_raw_os_error(libc::ENOTEMPTY))
        })
        .unwrap();
        assert!(!path.exists());
        fs::create_dir(&path).unwrap();
        assert!(remove_directory(&path, |path| {
            fs::write(path.join(".DS_Store"), "finder")?;
            fs::write(path.join("new-output"), "a writer is active")?;
            Err(io::Error::from_raw_os_error(libc::ENOTEMPTY))
        })
        .is_err());
        assert_eq!(
            fs::read_to_string(path.join("new-output")).unwrap(),
            "a writer is active"
        );
    }
}
