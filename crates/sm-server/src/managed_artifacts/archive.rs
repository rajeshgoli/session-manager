//! Archive primitives only. The lifecycle owner claims the artifact, supplies
//! private staging directories, and commits placement only after verification.
use super::{config::ManagedArtifactsConfig, now, scan, validate_name};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub mtime: i64,
    #[serde(default)]
    pub mtime_nsec: i64,
    pub mode: u32,
    pub target: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Part {
    pub file: String,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub first: String,
    pub last: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub frozen_at: String,
    pub sm_version: String,
    pub level: i32,
    pub data_bytes: u64,
    pub file_count: u64,
    pub content_stamp: String,
    pub parts: Vec<Part>,
    pub entries: Vec<Entry>,
    pub root_metadata: Entry,
}

fn safe_path(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && !value.contains('\0'),
        "invalid archive path"
    );
    ensure!(
        Path::new(value)
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "unsafe archive path: {value}"
    );
    // Reject noncanonical spellings too, so duplicate paths cannot alias a file.
    ensure!(
        value
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != ".."),
        "unsafe archive path: {value}"
    );
    Ok(())
}

pub(super) fn root_metadata(path: &Path) -> Result<Entry> {
    let m = fs::symlink_metadata(path)?;
    ensure!(m.is_dir(), "artifact root must be a real directory");
    Ok(Entry {
        path: String::new(),
        kind: "directory".into(),
        size: 0,
        mtime: m.mtime(),
        mtime_nsec: m.mtime_nsec(),
        mode: m.mode() & 0o7777,
        target: None,
    })
}

/// No symlink traversal, no device crossings, and no lossy filename conversion.
/// Unsupported special files fail closed instead of silently disappearing.
pub fn inventory(root: &Path) -> Result<Vec<Entry>> {
    fn walk(root: &Path, dir: &Path, device: u64, entries: &mut Vec<Entry>) -> Result<()> {
        for item in fs::read_dir(dir)? {
            let path = item?.path();
            if path.file_name().is_some_and(|n| n == ".DS_Store") {
                continue;
            }
            let m = fs::symlink_metadata(&path)?;
            ensure!(
                m.dev() == device,
                "archive crosses filesystem: {}",
                path.display()
            );
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .context("archive path is not UTF-8")?
                .to_owned();
            safe_path(&relative)?;
            let kind = if m.is_file() {
                "file"
            } else if m.is_dir() {
                "directory"
            } else if m.file_type().is_symlink() {
                "symlink"
            } else {
                bail!("unsupported archive entry: {relative}")
            };
            let target = if kind == "symlink" {
                Some(
                    fs::read_link(&path)?
                        .to_str()
                        .context("symlink target is not UTF-8")?
                        .to_owned(),
                )
            } else {
                None
            };
            entries.push(Entry {
                path: relative,
                kind: kind.into(),
                size: if m.is_file() { m.len() } else { 0 },
                mtime: m.mtime(),
                mtime_nsec: m.mtime_nsec(),
                mode: m.mode() & 0o7777,
                target,
            });
            if m.is_dir() {
                walk(root, &path, device, entries)?;
            }
        }
        Ok(())
    }
    let metadata = fs::symlink_metadata(root)?;
    ensure!(metadata.is_dir(), "archive source must be a real directory");
    let mut entries = Vec::new();
    walk(root, root, metadata.dev(), &mut entries)?;
    entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    Ok(entries)
}

pub fn plan(entries: &[Entry], limit: u64) -> Result<Vec<std::ops::Range<usize>>> {
    ensure!(limit > 0, "archive part limit must be positive");
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut bytes = 0u64;
    for (i, entry) in entries.iter().enumerate() {
        if i > start && (bytes > limit || entry.size > limit.saturating_sub(bytes)) {
            ranges.push(start..i);
            start = i;
            bytes = 0;
        }
        bytes = bytes
            .checked_add(entry.size)
            .context("archive size overflow")?;
        if entry.size > limit {
            ranges.push(start..i + 1);
            start = i + 1;
            bytes = 0;
        }
    }
    if start < entries.len() {
        ranges.push(start..entries.len());
    }
    Ok(ranges)
}

fn parallel<T: Sync, R: Send>(
    items: &[T],
    width: usize,
    f: impl Fn(&T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    ensure!(width > 0, "archive parallelism must be positive");
    let mut results = Vec::new();
    for batch in items.chunks(width) {
        let batch = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|item| {
                    let f = &f;
                    scope.spawn(move || f(item))
                })
                .collect();
            // Join every worker, including when another failed; no writer outlives the call.
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("archive worker panicked")))
                })
                .collect::<Vec<_>>()
        });
        results.extend(batch.into_iter().collect::<Result<Vec<_>>>()?);
    }
    Ok(results)
}

fn pax_mtime(entry: &Entry) -> String {
    let nanos = i128::from(entry.mtime) * 1_000_000_000 + i128::from(entry.mtime_nsec);
    format!(
        "{}{}.{:09}",
        if nanos < 0 { "-" } else { "" },
        nanos.abs() / 1_000_000_000,
        nanos.abs() % 1_000_000_000
    )
}

/// Revalidate immediately before the lifecycle owner commits the freeze too.
pub fn verify_source(source: &Path, entries: &[Entry]) -> Result<()> {
    ensure!(inventory(source)? == entries, "changed during freeze");
    Ok(())
}

fn pack_part(
    source: &Path,
    dest: &Path,
    entries: &[Entry],
    level: i32,
    workers: u32,
) -> Result<()> {
    let output = OpenOptions::new().write(true).create_new(true).open(dest)?;
    let mut encoder = zstd::stream::write::Encoder::new(output, level)?;
    encoder.include_checksum(true)?;
    encoder.multithread(workers)?;
    let mut tar = tar::Builder::new(encoder);
    for entry in entries {
        let mut header = tar::Header::new_ustar();
        header.set_path("entry")?;
        header.set_mode(entry.mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(entry.mtime.max(0) as u64);
        header.set_size(entry.size);
        header.set_entry_type(match entry.kind.as_str() {
            "file" => tar::EntryType::Regular,
            "directory" => tar::EntryType::Directory,
            _ => tar::EntryType::Symlink,
        });
        let mtime = pax_mtime(entry);
        let mut pax = vec![("path", entry.path.as_bytes()), ("mtime", mtime.as_bytes())];
        if let Some(target) = &entry.target {
            pax.push(("linkpath", target.as_bytes()));
        }
        tar.append_pax_extensions(pax)?;
        header.set_cksum();
        if entry.kind == "file" {
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(source.join(&entry.path))?;
            ensure!(
                file.metadata()?.len() == entry.size,
                "changed during freeze"
            );
            tar.append(&header, &mut file)?;
        } else {
            tar.append(&header, io::empty())?;
        }
    }
    tar.into_inner()?.finish()?.sync_all()?;
    Ok(())
}

/// Writes only to a newly created staging directory. On failure the caller may
/// remove that directory; this function never removes or mutates the source.
pub fn pack(
    source: &Path,
    staging: &Path,
    name: &str,
    config: &ManagedArtifactsConfig,
) -> Result<Manifest> {
    validate_name(name)?;
    ensure!(
        config.freeze_parallel_parts > 0,
        "archive parallelism must be positive"
    );
    let root_entry = root_metadata(source)?;
    let entries = inventory(source)?;
    let ranges = plan(&entries, config.freeze_part_bytes)?;
    fs::create_dir(staging)?;
    fs::set_permissions(staging, fs::Permissions::from_mode(0o700))?;
    let workers =
        (std::thread::available_parallelism()?.get() / config.freeze_parallel_parts).max(1) as u32;
    let indexed: Vec<_> = ranges.into_iter().enumerate().collect();
    let parts = parallel(&indexed, config.freeze_parallel_parts, |(index, range)| {
        let group = &entries[range.clone()];
        let file = format!("part-{:05}.tar.zst", index + 1);
        pack_part(
            source,
            &staging.join(&file),
            group,
            config.freeze_level,
            workers,
        )?;
        Ok(Part {
            output_bytes: fs::metadata(staging.join(&file))?.len(),
            file,
            input_bytes: group.iter().map(|e| e.size).sum(),
            first: group[0].path.clone(),
            last: group[group.len() - 1].path.clone(),
        })
    })?;
    let manifest = Manifest {
        name: name.into(),
        frozen_at: now(),
        sm_version: env!("CARGO_PKG_VERSION").into(),
        level: config.freeze_level,
        data_bytes: entries.iter().map(|e| e.size).sum(),
        file_count: entries.iter().filter(|e| e.kind == "file").count() as u64,
        content_stamp: scan::measure(source)?.stamp(),
        parts,
        entries,
        root_metadata: root_entry,
    };
    verify(staging, &manifest, config.freeze_parallel_parts)?;
    verify_source(source, &manifest.entries)?;
    ensure!(
        root_metadata(source)? == manifest.root_metadata,
        "root changed during freeze"
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staging.join("manifest.json"))?;
    serde_json::to_writer(&mut file, &manifest)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    File::open(staging)?.sync_all()?;
    Ok(manifest)
}

impl Manifest {
    pub fn read(directory: &Path) -> Result<Self> {
        let value: Self = serde_json::from_reader(File::open(directory.join("manifest.json"))?)?;
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        ensure!(
            self.root_metadata.kind == "directory"
                && self.root_metadata.path.is_empty()
                && self.root_metadata.target.is_none()
                && self.root_metadata.size == 0
                && self.root_metadata.mode <= 0o7777
                && (0..1_000_000_000).contains(&self.root_metadata.mtime_nsec),
            "invalid root metadata"
        );
        let mut seen = BTreeMap::new();
        let mut previous: Option<&str> = None;
        let mut bytes = 0u64;
        for entry in &self.entries {
            safe_path(&entry.path)?;
            ensure!(
                previous.is_none_or(|p| p < entry.path.as_str()),
                "duplicate or unordered archive path"
            );
            previous = Some(&entry.path);
            ensure!(
                matches!(entry.kind.as_str(), "file" | "directory" | "symlink"),
                "invalid entry kind"
            );
            ensure!(
                (0..1_000_000_000).contains(&entry.mtime_nsec) && entry.mode <= 0o7777,
                "invalid entry metadata"
            );
            ensure!(
                (entry.kind == "symlink") == entry.target.is_some(),
                "invalid symlink target"
            );
            ensure!(
                entry.kind == "file" || entry.size == 0,
                "invalid non-file size"
            );
            if let Some(parent) = Path::new(&entry.path)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
            {
                ensure!(
                    seen.get(parent.to_str().context("invalid parent")?) == Some(&"directory"),
                    "archive parent is absent or not a directory"
                );
            }
            seen.insert(entry.path.as_str(), entry.kind.as_str());
            bytes = bytes
                .checked_add(entry.size)
                .context("archive size overflow")?;
        }
        ensure!(bytes == self.data_bytes, "manifest data size mismatch");
        ensure!(
            self.file_count == self.entries.iter().filter(|e| e.kind == "file").count() as u64,
            "manifest file count mismatch"
        );
        let mut cursor = 0;
        for (i, part) in self.parts.iter().enumerate() {
            ensure!(
                part.file == format!("part-{:05}.tar.zst", i + 1),
                "invalid archive part name"
            );
            ensure!(
                self.entries
                    .get(cursor)
                    .is_some_and(|e| e.path == part.first),
                "part boundary mismatch"
            );
            let end = self.entries[cursor..]
                .iter()
                .position(|e| e.path == part.last)
                .context("invalid part end")?
                + cursor
                + 1;
            ensure!(
                self.entries[cursor..end]
                    .iter()
                    .map(|e| e.size)
                    .sum::<u64>()
                    == part.input_bytes,
                "part size mismatch"
            );
            cursor = end;
        }
        ensure!(
            cursor == self.entries.len(),
            "manifest has unpacked entries"
        );
        Ok(())
    }
    fn entries_for(&self, part: &Part) -> &[Entry] {
        let start = self.entries.partition_point(|e| e.path < part.first);
        let end = self.entries.partition_point(|e| e.path <= part.last);
        &self.entries[start..end]
    }
}

fn read_part(directory: &Path, part: &Part, expected: &[Entry], dest: Option<&Path>) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join(&part.file))?;
    ensure!(
        file.metadata()?.len() == part.output_bytes,
        "archive part length mismatch"
    );
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut tar = tar::Archive::new(decoder);
    let mut count = 0;
    for value in tar.entries()? {
        let mut value = value?;
        let entry = expected.get(count).context("unexpected archive entry")?;
        let path = value.path()?;
        ensure!(path == Path::new(&entry.path), "archive path mismatch");
        let expected_mtime = pax_mtime(entry);
        let mut timestamp_matches = false;
        if let Some(extensions) = value.pax_extensions()? {
            for extension in extensions {
                let extension = extension?;
                if extension.key_bytes() == b"mtime" {
                    timestamp_matches = extension.value_bytes() == expected_mtime.as_bytes();
                }
            }
        }
        ensure!(timestamp_matches, "archive timestamp mismatch");
        let kind = value.header().entry_type();
        ensure!(
            match entry.kind.as_str() {
                "file" => kind.is_file(),
                "directory" => kind.is_dir(),
                _ => kind.is_symlink(),
            },
            "archive kind mismatch"
        );
        ensure!(
            value.size() == entry.size && value.header().mode()? == entry.mode,
            "archive metadata mismatch"
        );
        ensure!(
            value.link_name()?.as_deref() == entry.target.as_deref().map(Path::new),
            "archive link mismatch"
        );
        if let Some(dest) = dest.filter(|_| entry.kind == "file") {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW)
                .mode(0o600)
                .open(dest.join(&entry.path))?;
            ensure!(
                io::copy(&mut value, &mut output)? == entry.size,
                "archive file truncated"
            );
            output.sync_all()?;
        } else {
            io::copy(&mut value, &mut io::sink())?;
        }
        count += 1;
    }
    ensure!(count == expected.len(), "archive entries missing");
    // tar stops at its end marker, before zstd necessarily reads the checksum.
    // Drain the decoder to EOF and reject a hidden second tar after the marker.
    let mut decoder = tar.into_inner();
    let mut buffer = [0u8; 65536];
    loop {
        let n = decoder.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        ensure!(buffer[..n].iter().all(|b| *b == 0), "trailing archive data");
    }
    Ok(())
}

pub fn verify(directory: &Path, manifest: &Manifest, parallelism: usize) -> Result<()> {
    manifest.validate()?;
    parallel(&manifest.parts, parallelism, |part| {
        read_part(directory, part, manifest.entries_for(part), None)
    })?;
    Ok(())
}

pub(super) fn restore_metadata(path: &Path, entry: &Entry) -> Result<()> {
    if entry.kind != "symlink" {
        fs::set_permissions(path, fs::Permissions::from_mode(entry.mode))?;
    }
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    #[cfg(target_os = "macos")]
    if entry.kind == "symlink" {
        // SAFETY: path is a live NUL-terminated string; NOFOLLOW changes only
        // the symlink's mode, including when its target is outside staging.
        if unsafe {
            libc::fchmodat(
                libc::AT_FDCWD,
                path.as_ptr(),
                entry.mode as libc::mode_t,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
    }
    let times = [libc::timespec {
        tv_sec: entry.mtime,
        tv_nsec: entry.mtime_nsec,
    }; 2];
    // SAFETY: path and the two timespec values remain valid throughout the call.
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

/// Destination must not exist and must be private to the operation. Symlinks
/// are installed only after all parallel writes complete; directory metadata is
/// restored last so child creation cannot change saved directory mtimes.
pub fn extract(
    directory: &Path,
    staging: &Path,
    manifest: &Manifest,
    parallelism: usize,
) -> Result<()> {
    manifest.validate()?;
    ensure!(parallelism > 0, "archive parallelism must be positive");
    fs::create_dir(staging)?;
    fs::set_permissions(staging, fs::Permissions::from_mode(0o700))?;
    for entry in &manifest.entries {
        if entry.kind == "directory" {
            fs::create_dir(staging.join(&entry.path))?;
        }
    }
    parallel(&manifest.parts, parallelism, |part| {
        read_part(directory, part, manifest.entries_for(part), Some(staging))
    })?;
    for entry in manifest.entries.iter().rev() {
        let path = staging.join(&entry.path);
        if let Some(target) = &entry.target {
            std::os::unix::fs::symlink(target, &path)?;
        }
        restore_metadata(&path, entry)?;
    }
    ensure!(
        inventory(staging)? == manifest.entries,
        "extracted tree differs from manifest"
    );
    let root_handle = File::open(staging)?;
    restore_metadata(staging, &manifest.root_metadata)?;
    root_handle.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests;
