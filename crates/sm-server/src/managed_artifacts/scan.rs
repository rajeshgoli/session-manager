use super::*;
use std::{collections::HashSet, fs, os::unix::fs::MetadataExt, path::Path};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Size {
    pub disk_bytes: u64,
    pub data_bytes: u64,
    pub files: u64,
    pub max_mtime: i128,
}
impl Size {
    pub fn stamp(&self) -> String {
        format!("{}:{}:{}", self.files, self.data_bytes, self.max_mtime)
    }
}
pub fn measure(path: &Path) -> Result<Size> {
    measure_excluding(path, |_| false)
}

/// As [`measure`], leaving out every entry whose path relative to `path` `skip`
/// accepts, together with everything beneath a skipped directory.
pub fn measure_excluding(path: &Path, skip: impl Fn(&str) -> bool) -> Result<Size> {
    fn walk(
        root: &Path,
        path: &Path,
        device: u64,
        skip: &dyn Fn(&str) -> bool,
        seen: &mut HashSet<(u64, u64)>,
        size: &mut Size,
    ) -> Result<()> {
        if path.file_name().is_some_and(|n| n == ".DS_Store") {
            return Ok(());
        }
        if path
            .strip_prefix(root)
            .ok()
            .and_then(Path::to_str)
            .is_some_and(|relative| !relative.is_empty() && skip(relative))
        {
            return Ok(());
        }
        let m = fs::symlink_metadata(path)?;
        if m.dev() != device || m.file_type().is_symlink() || !seen.insert((m.dev(), m.ino())) {
            return Ok(());
        }
        size.disk_bytes += m.blocks() * 512;
        if m.is_file() {
            size.data_bytes += m.len();
            size.files += 1;
            size.max_mtime = size
                .max_mtime
                .max(i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()));
        }
        if m.is_dir() {
            for entry in fs::read_dir(path)? {
                walk(root, &entry?.path(), device, skip, seen, size)?;
            }
        }
        Ok(())
    }
    let mut size = Size::default();
    walk(
        path,
        path,
        fs::symlink_metadata(path)?.dev(),
        &skip,
        &mut HashSet::new(),
        &mut size,
    )?;
    Ok(size)
}
impl ManagedArtifacts {
    pub fn scan(&self, root: &ArtifactRoot) -> Result<()> {
        let mut conn = self.store.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = format!("scanned:{}", root.path);
        let initial = store::meta(&tx, &key)?.is_none();
        let grace = time::Duration::days(if initial {
            self.config.initial_grace_days
        } else {
            self.config.unrecorded_grace_days
        });
        let grace_until = (OffsetDateTime::now_utc() + grace).format(&Rfc3339)?;
        let mut entries = fs::read_dir(&root.path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        // Directories first, so aliases resolve even on the very first scan.
        entries.sort_by_key(|e| e.file_type().map(|t| t.is_symlink()).unwrap_or(false));
        tx.execute("DELETE FROM artifact_aliases WHERE root=?1", [&root.path])?;
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if artifact(&tx, &root.path, &name).is_ok() {
                continue;
            }
            let mut location = "root";
            if entry.file_type()?.is_symlink() {
                if let Ok(resolved) = fs::canonicalize(entry.path()) {
                    if let Ok(relative) = resolved.strip_prefix(&root.path) {
                        let mut parts = relative.components();
                        if let Some(first) = parts.next() {
                            let target = first.as_os_str().to_string_lossy();
                            if artifact(&tx, &root.path, &target).is_ok() {
                                tx.execute("INSERT OR REPLACE INTO artifact_aliases(root,alias,target_name,target_rel) VALUES(?1,?2,?3,?4)",params![root.path,name,target,parts.as_path().to_string_lossy()])?;
                                continue;
                            }
                        }
                    }
                    if on_cold_store(root, &name, &resolved) {
                        location = "cold";
                    }
                }
            }
            // Discovery records arbitrary existing names; commands that touch the
            // filesystem still require the spec's safe single-segment grammar.
            tx.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at,first_seen_at,grace_until) VALUES(?1,?2,'unrecorded',?3,?2,'unrecorded',?4,?5,?5,?6)",params![root.path,name,location,root.repo,now(),grace_until])?;
            event(
                &tx,
                &root.path,
                &name,
                "discovered",
                "sm",
                json!({"initial":initial}),
            )?;
        }
        let mut relocated = Vec::new();
        for a in store::query(
            &tx,
            "SELECT * FROM artifacts WHERE root=?1 AND state IN ('live','building','frozen')",
            &[&root.path],
        )? {
            let name = a["name"].as_str().unwrap();
            if !a["op"].is_null() {
                continue;
            }
            match fs::symlink_metadata(Path::new(&root.path).join(name)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Queue submission records outputs before admission creates
                    // their folders. Only an unstarted reservation excuses absence.
                    if a["state"] == "building"
                        && super::submission::awaiting_placement(&tx, &root.path, name)?
                    {
                        continue;
                    }
                    tx.execute("UPDATE artifacts SET state='deleted',deleted_at=?3,deleted_by='unknown',delete_reason='vanished' WHERE root=?1 AND name=?2",params![root.path,name,now()])?;
                    event(&tx, &root.path, name, "vanished", "sm", json!({}))?;
                }
                Err(e) => return Err(e.into()),
                // A folder moved between disks by hand, behind or out of a
                // link, is measured where it now lives (sm#2414).
                Ok(m) if a["state"] == "live" => {
                    let path = Path::new(&root.path).join(name);
                    let location = if m.file_type().is_symlink() {
                        fs::canonicalize(&path)
                            .is_ok_and(|resolved| on_cold_store(root, name, &resolved))
                            .then_some("cold")
                    } else {
                        m.is_dir().then_some("root")
                    };
                    if let Some(location) = location.filter(|l| a["location"] != *l) {
                        tx.execute(
                            "UPDATE artifacts SET location=?3 WHERE root=?1 AND name=?2",
                            params![root.path, name, location],
                        )?;
                        event(
                            &tx,
                            &root.path,
                            name,
                            "relocated",
                            "sm",
                            json!({"location":location}),
                        )?;
                        relocated.push(name.to_owned());
                    }
                }
                Ok(_) => {}
            }
        }
        store::set_meta(&tx, &key, &now())?;
        tx.commit()?;
        // The old size measured the link, not the folder; do not wait for the night walk.
        for name in relocated {
            if let Err(error) = self.measure_one(root, &name) {
                eprintln!(
                    "artifact size unavailable for {}/{name} after it moved: {error:#}",
                    root.path
                );
            }
        }
        Ok(())
    }
    /// Measure unknown folders too: an absent record is agent work, not zero bytes.
    pub fn measure_one(&self, root: &ArtifactRoot, name: &str) -> Result<Value> {
        validate_name(name)?;
        let a = self.store.detail(&root.path, name)?;
        if !matches!(a["state"].as_str(), Some("live" | "unrecorded")) || !a["op"].is_null() {
            bail!("artifact must be live or unrecorded and not busy to measure");
        }
        let path = Path::new(if a["location"] == "cold" {
            &root.cold_store
        } else {
            &root.path
        })
        .join(name);
        // measure() never follows symlinks or crosses filesystem boundaries.
        let size = measure(&path)?;
        self.store.open()?.execute("UPDATE artifacts SET disk_bytes=?3,data_bytes=?4,content_stamp=?5,size_measured_at=?6 WHERE root=?1 AND name=?2 AND state IN ('live','unrecorded') AND op IS NULL",params![root.path,name,size.disk_bytes,size.data_bytes,size.stamp(),now()])?;
        self.store.open()?.execute(
            "DELETE FROM artifact_meta WHERE key=?1",
            [format!("size_error:{}:{name}", root.path)],
        )?;
        self.store.detail(&root.path, name)
    }
    pub fn measure_root(&self, root: &ArtifactRoot) -> Result<()> {
        for a in self.store.list(&root.path)? {
            if !matches!(a["state"].as_str(), Some("live" | "unrecorded")) || !a["op"].is_null() {
                continue;
            }
            let name = a["name"].as_str().unwrap();
            if let Err(error) = self.measure_one(root, name) {
                // One missing or unreadable folder must not starve the rest.
                let conn = self.store.open()?;
                store::set_meta(
                    &conn,
                    &format!("size_error:{}:{name}", root.path),
                    &error.to_string(),
                )?;
                eprintln!(
                    "artifact size unavailable for {}/{name}; continuing management: {error:#}",
                    root.path
                );
            }
        }
        Ok(())
    }
}
/// A root entry whose link resolves to `<cold_store>/<name>` lives on the cold store.
fn on_cold_store(root: &ArtifactRoot, name: &str, resolved: &Path) -> bool {
    resolved == Path::new(&root.cold_store).join(name)
}
pub fn ticket_prefix(name: &str) -> Option<i64> {
    let end = name.find(['_', '-'])?;
    if !(2..=6).contains(&end) || !name[..end].bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    name[..end].parse().ok()
}
