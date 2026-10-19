use super::super::{moves, ManagedArtifacts};
use super::*;
use anyhow::{ensure, Context};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    time::{Duration, Instant},
};

/// `fsync(2)` on a path. Rust's `sync_all`/`sync_data` are `F_FULLFSYNC` on
/// macOS, a full drive-cache flush that costs about 45 ms on a USB hard disk;
/// batches pair plain `fsync`s with one [`full_fsync`] (#2317).
pub(super) fn fsync_path(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}
/// Flush the drive's cache as well, once per batch.
pub(super) fn full_fsync(file: &File) -> Result<()> {
    file.sync_all()?;
    Ok(())
}
/// Durability barrier for many files: start writing everything, then wait for
/// each path, then flush the drive's cache once through `anchor`.
pub(super) fn barrier<'a>(
    paths: impl IntoIterator<Item = &'a PathBuf>,
    anchor: &File,
) -> Result<()> {
    unsafe { libc::sync() };
    for path in paths {
        fsync_path(path)?;
    }
    full_fsync(anchor)
}

/// Direct-mode staging made durable in batches: files are placed without
/// waiting for the disk, and their index lines are appended only after one
/// barrier covers the whole batch, so a line never vouches for a copy a crash
/// could lose.
#[derive(Default)]
struct Batch {
    paths: std::collections::BTreeSet<PathBuf>,
    files: usize,
    bytes: u64,
    lines: Vec<u8>,
    started: Option<Instant>,
}
impl Batch {
    fn add(
        &mut self,
        placed: PathBuf,
        dirs: impl IntoIterator<Item = PathBuf>,
        bytes: u64,
        line: &[u8],
    ) {
        self.started.get_or_insert_with(Instant::now);
        self.paths.insert(placed);
        self.paths.extend(dirs);
        self.files += 1;
        self.bytes += bytes;
        self.lines.extend_from_slice(line);
    }
    fn due(&self) -> bool {
        #[cfg(test)]
        if let Some(files) = BATCH_FILES.get() {
            return self.files >= files;
        }
        self.files >= 512
            || self.bytes >= 4 << 30
            || self
                .started
                .is_some_and(|t| t.elapsed() >= Duration::from_secs(30))
    }
    fn flush(&mut self, index: &mut File) -> Result<()> {
        if self.files == 0 {
            return Ok(());
        }
        unsafe { libc::sync() };
        for path in &self.paths {
            fsync_path(path)?;
        }
        index.write_all(&self.lines)?;
        full_fsync(index)?;
        *self = Self::default();
        Ok(())
    }
}

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
    /// Files, and their source bytes, an earlier seed had already staged.
    pub reused: (u64, u64),
}
/// Seed staging index: one line per placed file, appended and synced as it is
/// placed, so a later seed can reuse what an interrupted one staged (#2301).
pub(super) const INDEX: &str = "index.jsonl";
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Staged {
    pub record: FileRecord,
    /// Size of the staged copy, which for packed scopes is compressed.
    pub staged_bytes: u64,
    /// The staged copy's own mtime. Renames keep it and every new copy gets a
    /// new one, so a line matches only the copy it was written for.
    pub staged_mtime_ns: i128,
}
/// Staged files by path, the last line for a path winning. A line torn by a
/// crash is cut off so the next append starts on a line of its own.
pub(super) fn staged_index(staging: &Path) -> Result<BTreeMap<String, Staged>> {
    let path = staging.join(INDEX);
    let body = match fs::read(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    let whole = body.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    if whole < body.len() {
        let file = OpenOptions::new().write(true).open(&path)?;
        file.set_len(whole as u64)?;
        file.sync_all()?;
    }
    Ok(body[..whole]
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Staged>(line).ok())
        .map(|staged| (staged.record.path.clone(), staged))
        .collect())
}
/// Delete staged files no record kept, then the directories that leaves empty.
fn sweep(dir: &Path, keep: &std::collections::BTreeSet<PathBuf>, top: bool) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        // On exFAT, macOS keeps a file's attributes in a `._<name>` sidecar and
        // unlinks it with the file, so an entry listed above may be gone.
        let metadata = match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            metadata => metadata?,
        };
        if metadata.is_dir() {
            sweep(&path, keep, false)?;
        } else if !(keep.contains(&path) || top && path.file_name() == Some(INDEX.as_ref())) {
            match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
    }
    if !top && fs::read_dir(dir)?.next().is_none() {
        fs::remove_dir(dir)?;
    }
    Ok(())
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
    durable: bool,
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
    #[cfg(test)]
    READS.with(|reads| {
        if let Some(reads) = reads.borrow_mut().as_mut() {
            reads.push(source.to_path_buf());
        }
    });
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
        // Standard zstd output either way; workers only shorten large files.
        encoder.multithread(std::thread::available_parallelism().map_or(1, |n| n.get() as u32))?;
        copy(&mut encoder)?;
        let file = encoder.finish()?;
        if durable {
            file.sync_all()?;
        }
    } else {
        let mut file = file;
        copy(&mut file)?;
        if durable {
            file.sync_all()?;
        }
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
thread_local! { pub(super) static DIRECT_FLOOR: std::cell::Cell<Option<u128>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
thread_local! { pub(super) static READS: std::cell::RefCell<Option<Vec<PathBuf>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
thread_local! { pub(super) static CRASH_AFTER_PLACED: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
thread_local! { pub(super) static BATCH_FILES: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
thread_local! { pub(super) static FAIL_AFTER_PLACED: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }
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
    prepare_with_progress(manager, scopes, base, id, None, |_| Ok(()))
}

/// Seeding: place each staged file at `<staging>/<record path>`, the layout
/// `Destination::unpack` produces, instead of packing transit parts. Free space
/// on the staging disk is checked per file against a 5% floor, since the raw
/// scope can far exceed the compressed result. Existing staging is resumed: a
/// file whose source still matches its index line is reused unread, and staged
/// files no source still needs are deleted. Staging is never removed here.
pub(super) fn prepare_direct(
    manager: &ManagedArtifacts,
    scopes: &[Scope],
    base: Option<&Manifest>,
    id: &str,
    staging: &Path,
) -> Result<Prepared> {
    prepare_with_progress(manager, scopes, base, id, Some(staging), |_| Ok(()))
}

pub(super) fn prepare_with_progress(
    manager: &ManagedArtifacts,
    scopes: &[Scope],
    base: Option<&Manifest>,
    id: &str,
    direct: Option<&Path>,
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
    let resumed = direct.is_some_and(Path::exists);
    let directory = match direct {
        Some(staging) => {
            if !resumed {
                fs::create_dir(staging)?;
                // Resuming needs the staging folder itself to survive a crash.
                let incoming = staging.parent().context("staging has no parent")?;
                for dir in [Some(incoming), incoming.parent()].into_iter().flatten() {
                    File::open(dir)?.sync_all()?;
                }
            }
            for stale in [".payload", "manifest.json"] {
                match fs::remove_file(staging.join(stale)) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                    _ => {}
                }
            }
            staging.to_path_buf()
        }
        None => outbox_directory(manager, scopes, base, id)?,
    };
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let mut guard = Staging(directory.clone(), direct.is_some());
    let index = if resumed {
        staged_index(&directory)?
    } else {
        BTreeMap::new()
    };
    let mut index_file = match direct {
        Some(_) => Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(directory.join(INDEX))?,
        ),
        None => None,
    };
    let mut reused = (0u64, 0u64);
    let mut batch = Batch::default();
    // `<dest>/.incoming/<id>`: an interrupted apply leaves staged copies here.
    let destination = direct.and_then(Path::parent).and_then(Path::parent);
    #[cfg(test)]
    let mut placed_count = 0usize;
    let direct_floor = match direct {
        Some(_) => {
            let disk = nix::sys::statvfs::statvfs(&directory)?;
            let floor = u128::from(disk.blocks()) * u128::from(disk.fragment_size()) * 5 / 100;
            #[cfg(test)]
            let floor = DIRECT_FLOOR.get().unwrap_or(floor);
            Some(floor)
        }
        None => None,
    };
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
        let inventory = match scope.inventory() {
            Ok(entries) => entries,
            // Moved by a freeze or thaw since the scope was collected: like a
            // removed file, it waits for the next preparation, never a deletion.
            Err(e)
                if e.chain().any(|c| {
                    c.downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
                }) =>
            {
                let inside = format!("{prefix}/");
                for (path, old) in base
                    .into_iter()
                    .flat_map(|m| &m.files)
                    .filter(|(p, _)| p.starts_with(&inside))
                {
                    manifest.files.insert(path.clone(), old.clone());
                    deferred.push(path.clone());
                }
                continue;
            }
            Err(e) => {
                return Err(e.context(format!(
                    "backup source unreadable: {}",
                    scope.source.display()
                )))
            }
        };
        for entry in inventory {
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
            // The rule above, against the staging index of an interrupted seed.
            if let Some(staged) = index.get(&path).filter(|s| {
                s.record.size == record.size
                    && s.record.mtime_ns == record.mtime_ns
                    && s.record.mode == record.mode
                    && s.record.target.is_none()
                    && record.target.is_none()
                    // In staging, or already moved to its destination path by
                    // an interrupted apply (a rename keeps size and mtime).
                    && [Some(directory.join(&path)), destination.map(|d| d.join(&path))]
                        .into_iter()
                        .flatten()
                        .any(|copy| {
                            fs::symlink_metadata(copy).is_ok_and(|m| {
                                m.is_file()
                                    && m.len() == s.staged_bytes
                                    && mtime(&m) == s.staged_mtime_ns
                            })
                        })
            }) {
                reused = (reused.0 + 1, reused.1 + record.size);
                manifest.files.insert(path, staged.record.clone());
                changed.push(staged.record.clone());
                progress(entry.size)?;
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
                if let Some(floor) = direct_floor {
                    let disk = nix::sys::statvfs::statvfs(&directory)?;
                    let free =
                        u128::from(disk.blocks_available()) * u128::from(disk.fragment_size());
                    ensure!(
                        free >= u128::from(entry.size) + floor,
                        "backup destination would cross its floor"
                    );
                }
                record.sha256 = stage(&source, &spool, !scope.plain, &metadata, direct.is_none())?;
                record.sha256.is_some()
            };
            if !stable {
                if let Some(old) = previous {
                    manifest.files.insert(path.clone(), old.clone());
                }
                deferred.push(path);
            } else {
                if record.target.is_none() && direct.is_some() {
                    let placed = directory.join(&record.path);
                    super::client::verify(&spool, &record)?;
                    let spooled = fs::metadata(&spool)?;
                    let parent = placed.parent().context("staged path has no parent")?;
                    // Each folder created here is synced into its own parent.
                    // Each folder created here must reach its own parent.
                    let created: Vec<_> = parent
                        .ancestors()
                        .take_while(|dir| !dir.exists())
                        .filter_map(Path::parent)
                        .map(Path::to_path_buf)
                        .collect();
                    fs::create_dir_all(parent)?;
                    // The batch makes the placed file durable before its line,
                    // and the line names the copy's own mtime, so an index line
                    // never vouches for an older copy of the path.
                    fs::rename(&spool, &placed)?;
                    let mut line = serde_json::to_vec(&Staged {
                        record: record.clone(),
                        staged_bytes: spooled.len(),
                        staged_mtime_ns: mtime(&spooled),
                    })?;
                    line.push(b'\n');
                    batch.add(
                        placed.clone(),
                        created
                            .into_iter()
                            .chain([parent.to_path_buf(), directory.clone()]),
                        spooled.len(),
                        &line,
                    );
                    if batch.due() {
                        batch.flush(index_file.as_mut().context("seed index not open")?)?;
                    }
                    #[cfg(test)]
                    {
                        placed_count += 1;
                        // A stop after N durable files, or a crash that loses
                        // the batch not yet flushed.
                        if FAIL_AFTER_PLACED.get() == Some(placed_count) {
                            batch.flush(index_file.as_mut().context("seed index not open")?)?;
                            anyhow::bail!("injected stop after {placed_count} staged files");
                        }
                        if CRASH_AFTER_PLACED.get() == Some(placed_count) {
                            anyhow::bail!("injected crash after {placed_count} staged files");
                        }
                    }
                } else if record.target.is_none() {
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
    if let Some(index_file) = index_file.as_mut() {
        batch.flush(index_file)?;
    }
    if resumed {
        let keep = changed
            .iter()
            .filter(|r| r.target.is_none())
            .map(|r| directory.join(&r.path))
            .collect();
        sweep(&directory, &keep, true)?;
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
        reused,
    })
}

/// The cold-store outbox for one request, after its location and capacity checks.
fn outbox_directory(
    manager: &ManagedArtifacts,
    scopes: &[Scope],
    base: Option<&Manifest>,
    id: &str,
) -> Result<PathBuf> {
    let config = &manager.config.backup;
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
    Ok(directory)
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
