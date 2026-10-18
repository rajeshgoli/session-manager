use super::super::{moves, ManagedArtifacts};
use super::*;
use anyhow::{ensure, Context};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferManifest {
    pub manifest_id: String,
    pub base: Option<String>,
    pub changed: Vec<FileRecord>,
    pub deleted: Vec<String>,
    pub holding_days: BTreeMap<String, u64>,
}
#[derive(Debug)]
pub struct Prepared {
    pub manifest: Manifest,
    pub transfer: TransferManifest,
    pub parts: Vec<Part>,
    pub deferred: Vec<String>,
    pub directory: PathBuf,
}
struct Staging(PathBuf, bool);
impl Drop for Staging {
    fn drop(&mut self) {
        if !self.1 {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
struct PartWriter {
    tar: tar::Builder<zstd::stream::write::Encoder<'static, File>>,
    input_bytes: u64,
    path: PathBuf,
    n: usize,
}
impl PartWriter {
    fn new(directory: &Path, n: usize) -> Result<Self> {
        let path = directory.join(format!("part-{n:04}.tar.zst"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok(Self {
            tar: tar::Builder::new(zstd::stream::write::Encoder::new(file, 1)?),
            input_bytes: 0,
            path,
            n,
        })
    }
    fn append(&mut self, record: &FileRecord, spool: &Path) -> Result<()> {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(fs::metadata(spool)?.len());
        header.set_mode(record.mode);
        header.set_mtime((record.mtime_ns / 1_000_000_000).max(0) as u64);
        // Always PAX, including UTF-8/long paths; the fallback header is ignored.
        self.tar
            .append_pax_extensions([("path", record.path.as_bytes())])?;
        header.set_path("payload")?;
        header.set_cksum();
        self.tar.append(&header, File::open(spool)?)?;
        self.input_bytes = self.input_bytes.saturating_add(record.size);
        Ok(())
    }
    fn finish(self) -> Result<Part> {
        self.tar.into_inner()?.finish()?.sync_all()?;
        let bytes = fs::metadata(&self.path)?.len();
        let sha256 = hash_file(&self.path)?;
        Ok(Part {
            n: self.n,
            file: self.path.file_name().unwrap().to_str().unwrap().into(),
            bytes,
            sha256,
        })
    }
}
pub(super) fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn mtime(m: &fs::Metadata) -> i128 {
    i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec())
}
fn same(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && mtime(a) == mtime(b)
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
        && a.mode() == b.mode()
}

/// Stream a file to private staging and fingerprint its original bytes. The
/// staging payload is only committed to a part if both fd and path stay stable.
fn stage(
    source: &Path,
    spool: &Path,
    packed: bool,
    before: &fs::Metadata,
) -> Result<Option<String>> {
    let mut input = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(source)
    {
        Ok(input) => input,
        // Removed after its metadata was read: treated as changed.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !same(before, &input.metadata()?) {
        return Ok(None);
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(spool)?;
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut copy = |writer: &mut dyn Write| -> Result<()> {
        let mut buffer = [0u8; 128 * 1024];
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            writer.write_all(&buffer[..n])?;
            count += n as u64;
        }
        Ok(())
    };
    if packed {
        let mut encoder = zstd::stream::write::Encoder::new(file, 9)?;
        copy(&mut encoder)?;
        encoder.finish()?.sync_all()?;
    } else {
        let mut file = file;
        copy(&mut file)?;
        file.sync_all()?;
    }
    #[cfg(test)]
    AFTER_READ.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook(source);
        }
    });
    if count != before.len()
        || !same(before, &input.metadata()?)
        || !fs::symlink_metadata(source).is_ok_and(|m| same(before, &m))
    {
        return Ok(None);
    }
    Ok(Some(format!("{:x}", hash.finalize())))
}
#[cfg(test)]
type ReadHook = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
thread_local! { pub(super) static AFTER_READ: std::cell::RefCell<Option<ReadHook>> = const { std::cell::RefCell::new(None) }; }

/// Create one immutable outbox. Caller persists the candidate manifest and only
/// publishes ready after this succeeds. A failed preparation removes its staging.
pub fn prepare(
    manager: &ManagedArtifacts,
    scopes: &[Scope],
    base: Option<&Manifest>,
    id: &str,
) -> Result<Prepared> {
    prepare_with_progress(manager, scopes, base, id, |_| Ok(()))
}

pub(super) fn prepare_with_progress(
    manager: &ManagedArtifacts,
    scopes: &[Scope],
    base: Option<&Manifest>,
    id: &str,
    mut progress: impl FnMut(u64) -> Result<()>,
) -> Result<Prepared> {
    safe_relative(id)?;
    ensure!(!id.contains('/'), "outbox id must be a single segment");
    scope::validate_scopes(scopes)?;
    let config = &manager.config.backup;
    ensure!(
        config.outbox_part_bytes > 0,
        "backup part limit must be positive"
    );
    let outbox = crate::sessions::expand_home(&config.outbox);
    let parent = outbox.parent().context("outbox has no parent")?;
    let parent_meta = fs::metadata(parent).context("backup cold store unavailable")?;
    ensure!(
        manager.config.roots.iter().any(|r| moves::cold_mounted(r)
            && fs::metadata(&r.cold_store).is_ok_and(|m| m.dev() == parent_meta.dev())),
        "outbox is not on a mounted cold store"
    );
    let (_, estimate) = pending_input(scopes, base)?;
    let estimate = u128::from(estimate);
    let disk = nix::sys::statvfs::statvfs(parent)?;
    let free = u128::from(disk.blocks_available()) * u128::from(disk.fragment_size());
    let floor = u128::from(disk.blocks())
        * u128::from(disk.fragment_size())
        * u128::from(manager.config.floor_percent)
        / 100;
    ensure!(
        free >= estimate + floor,
        "backup outbox would cross cold-store floor"
    );
    match fs::create_dir(&outbox) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let m = fs::symlink_metadata(&outbox)?;
            ensure!(
                m.is_dir() && m.dev() == parent_meta.dev(),
                "unsafe backup outbox directory"
            );
        }
        Err(e) => return Err(e.into()),
    }
    let directory = outbox.join(id);
    fs::create_dir(&directory)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let mut guard = Staging(directory.clone(), false);
    let mut manifest = Manifest {
        manifest_id: id.into(),
        base: base.map(|m| m.manifest_id.clone()),
        holding_days: base.map(|m| m.holding_days.clone()).unwrap_or_default(),
        ..Default::default()
    };
    let mut changed = Vec::new();
    let mut deferred = Vec::new();
    let mut parts = Vec::new();
    let mut writer: Option<PartWriter> = None;
    // Sorting by destination guarantees stable path order even for direct callers.
    let mut scopes: Vec<_> = scopes.iter().collect();
    scopes.sort_by_key(|s| s.prefix());
    for scope in scopes {
        let prefix = scope.prefix();
        manifest
            .holding_days
            .insert(prefix.clone(), scope.holding_days);
        for entry in scope.inventory()? {
            if entry.kind == "directory" || entry.path.split('/').any(|p| p.starts_with("._")) {
                continue;
            }
            let source = scope.path_of(&entry.path);
            let path = format!(
                "{prefix}/{}{}",
                entry.path,
                if !scope.plain && entry.kind == "file" {
                    ".zst"
                } else {
                    ""
                }
            );
            safe_relative(&path)?;
            // E.g. a symlink named foo.zst cannot alias a packed file foo.
            ensure!(
                !manifest.files.contains_key(&path),
                "backup layout collision: {path}"
            );
            let metadata = match fs::symlink_metadata(&source) {
                Ok(m)
                    if m.is_file() == (entry.kind == "file")
                        && m.file_type().is_symlink() == (entry.kind == "symlink") =>
                {
                    Some(m)
                }
                Ok(_) => None,
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            // Removed or replaced since the inventory: like a change, it waits for
            // the next preparation, which sees its new state.
            let Some(metadata) = metadata else {
                if let Some(old) = base.and_then(|m| m.files.get(&path)) {
                    manifest.files.insert(path.clone(), old.clone());
                }
                deferred.push(path);
                continue;
            };
            let mut record = FileRecord {
                path: path.clone(),
                size: entry.size,
                mtime_ns: i128::from(entry.mtime) * 1_000_000_000 + i128::from(entry.mtime_nsec),
                mode: entry.mode,
                sha256: None,
                target: entry.target,
            };
            let previous = base.and_then(|m| m.files.get(&path));
            if let Some(old) = previous.filter(|old| {
                old.size == record.size
                    && old.mtime_ns == record.mtime_ns
                    && old.mode == record.mode
                    && old.target == record.target
            }) {
                manifest.files.insert(path, old.clone());
                continue;
            }
            let spool = directory.join(".payload");
            let stable = if record.target.is_some() {
                fs::read_link(&source)
                    .ok()
                    .as_deref()
                    .and_then(Path::to_str)
                    == record.target.as_deref()
                    && mtime(&metadata) == record.mtime_ns
            } else if metadata.len() != entry.size || mtime(&metadata) != record.mtime_ns {
                false
            } else {
                record.sha256 = stage(&source, &spool, !scope.plain, &metadata)?;
                record.sha256.is_some()
            };
            if !stable {
                if let Some(old) = previous {
                    manifest.files.insert(path.clone(), old.clone());
                }
                deferred.push(path);
            } else {
                if record.target.is_none() {
                    if writer.as_ref().is_some_and(|w| {
                        w.input_bytes.saturating_add(record.size) > config.outbox_part_bytes
                            && w.input_bytes > 0
                    }) {
                        parts.push(writer.take().unwrap().finish()?);
                    }
                    if writer.is_none() {
                        writer = Some(PartWriter::new(&directory, parts.len())?);
                    }
                    writer.as_mut().unwrap().append(&record, &spool)?;
                }
                manifest.files.insert(path, record.clone());
                changed.push(record);
            }
            progress(entry.size)?;
            if spool.exists() {
                fs::remove_file(spool)?;
            }
        }
    }
    if let Some(writer) = writer {
        parts.push(writer.finish()?);
    }
    let deleted = base
        .into_iter()
        .flat_map(|m| m.files.keys())
        .filter(|p| !manifest.files.contains_key(*p))
        .cloned()
        .collect();
    let transfer = TransferManifest {
        manifest_id: id.into(),
        base: manifest.base.clone(),
        changed,
        deleted,
        holding_days: manifest.holding_days.clone(),
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join("manifest.json"))?;
    serde_json::to_writer(&mut file, &transfer)?;
    file.sync_all()?;
    File::open(&directory)?.sync_all()?;
    guard.1 = true;
    Ok(Prepared {
        manifest,
        transfer,
        parts,
        deferred,
        directory,
    })
}

/// Count only changed regular files; full-scope estimates must not reserve a
/// second complete backup for an incremental request.
pub(super) fn pending_input(scopes: &[Scope], base: Option<&Manifest>) -> Result<(u64, u64)> {
    let mut input = 0u64;
    let mut estimate = 0u64;
    for scope in scopes {
        let entries = scope.inventory()?;
        let mut total = 0u64;
        let mut changed = 0u64;
        for entry in entries {
            if entry.kind != "file" || entry.path.split('/').any(|p| p.starts_with("._")) {
                continue;
            }
            total = total
                .checked_add(entry.size)
                .context("backup size overflow")?;
            let path = format!(
                "{}/{}{}",
                scope.prefix(),
                entry.path,
                if scope.plain { "" } else { ".zst" }
            );
            let unchanged = base.and_then(|m| m.files.get(&path)).is_some_and(|old| {
                old.target.is_none()
                    && old.size == entry.size
                    && old.mode == entry.mode
                    && old.mtime_ns
                        == i128::from(entry.mtime) * 1_000_000_000 + i128::from(entry.mtime_nsec)
            });
            if !unchanged {
                changed = changed
                    .checked_add(entry.size)
                    .context("backup size overflow")?;
            }
        }
        input = input.checked_add(changed).context("backup size overflow")?;
        let packed = if total == 0 {
            0
        } else {
            (u128::from(changed) * u128::from(scope.estimated_bytes)).div_ceil(u128::from(total))
        };
        estimate = estimate
            .checked_add(u64::try_from(packed)?)
            .context("backup estimate overflow")?;
    }
    Ok((input, estimate))
}
