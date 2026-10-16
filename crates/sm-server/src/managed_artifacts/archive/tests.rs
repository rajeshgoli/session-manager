use super::*;
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sm-archive-{}-{}-{serial}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn config() -> ManagedArtifactsConfig {
    ManagedArtifactsConfig {
        freeze_part_bytes: 8,
        freeze_parallel_parts: 2,
        thaw_parallel_parts: 2,
        ..Default::default()
    }
}
fn entry(path: &str, size: u64) -> Entry {
    Entry {
        path: path.into(),
        kind: "file".into(),
        size,
        mtime: 1,
        mtime_nsec: 0,
        mode: 0o644,
        target: None,
    }
}

#[test]
fn managed_artifacts_archive_part_boundaries_and_oversized_file() {
    let entries: Vec<_> = [3, 5, 1, 9, 0, 8, 1]
        .into_iter()
        .enumerate()
        .map(|(i, n)| entry(&i.to_string(), n))
        .collect();
    assert_eq!(
        plan(&entries, 8).unwrap(),
        vec![0..2, 2..3, 3..4, 4..6, 6..7]
    );
    assert!(plan(&entries, 0).is_err());
    assert!(plan(&[], 8).unwrap().is_empty());
    assert_eq!(
        plan(&[entry("sparse", 3 * 1024 * 1024 * 1024)], 1024).unwrap(),
        vec![0..1]
    );
}

#[test]
fn managed_artifacts_archive_round_trip_metadata_symlinks_and_empty_dirs() {
    let f = Fixture::new();
    let source = f.path("source");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("empty")).unwrap();
    fs::create_dir(source.join("nested")).unwrap();
    let long = format!("nested/{}", "x".repeat(180));
    for (name, data) in [
        ("._literal", "ordinary file"),
        ("nested/data", "abcdefghijk"),
        (long.as_str(), "long path"),
    ] {
        fs::write(source.join(name), data).unwrap();
    }
    fs::write(source.join(".DS_Store"), "ignored").unwrap();
    std::os::unix::fs::symlink("../._literal", source.join("nested/link")).unwrap();
    std::os::unix::fs::symlink("/absent/outside", source.join("dangling")).unwrap();
    let mut meta = entry("nested/data", 11);
    meta.mode = 0o751;
    meta.mtime = 123456789;
    meta.mtime_nsec = 123456789;
    restore_metadata(&source.join("nested/data"), &meta).unwrap();
    let before = inventory(&source).unwrap();
    let archive = f.path("archive");
    let manifest = pack(&source, &archive, "sample", &config()).unwrap();
    assert!(manifest.parts.len() > 1);
    let manifest = Manifest::read(&archive).unwrap();
    let dest = f.path("dest");
    extract(&archive, &dest, &manifest, 2).unwrap();
    assert_eq!(inventory(&dest).unwrap(), before);
    for e in &before {
        if e.kind == "file" {
            assert_eq!(
                fs::read(source.join(&e.path)).unwrap(),
                fs::read(dest.join(&e.path)).unwrap()
            );
        }
    }
    assert!(!dest.join(".DS_Store").exists());
    assert!(source.join(".DS_Store").exists());
}

#[test]
fn managed_artifacts_archive_checksum_corruption_is_detected() {
    let f = Fixture::new();
    let source = f.path("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("data"), "data").unwrap();
    let archive = f.path("archive");
    let manifest = pack(&source, &archive, "sample", &config()).unwrap();
    let path = archive.join(&manifest.parts[0].file);
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(path, bytes).unwrap();
    assert!(verify(&archive, &manifest, 2).is_err());
    assert!(extract(&archive, &f.path("dest"), &manifest, 2).is_err());
    assert_eq!(fs::read_to_string(source.join("data")).unwrap(), "data");
}

#[test]
fn managed_artifacts_archive_rejects_unsafe_paths_and_symlink_parents() {
    for path in ["", "/absolute", "../escape", "a/../escape", "a/./b", "a//b"] {
        assert!(safe_path(path).is_err(), "{path}");
    }
    let f = Fixture::new();
    let source = f.path("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("x"), "x").unwrap();
    let archive = f.path("archive");
    let mut manifest = pack(&source, &archive, "sample", &config()).unwrap();
    manifest.entries[0].path = "../escape".into();
    assert!(extract(&archive, &f.path("dest"), &manifest, 2).is_err());
    assert!(!f.path("dest").exists());
    manifest.entries[0].path = "parent".into();
    manifest.entries[0].kind = "symlink".into();
    manifest.entries[0].size = 0;
    manifest.entries[0].target = Some("/outside".into());
    manifest.entries.push(entry("parent/file", 1));
    assert!(manifest.validate().is_err());
}

#[test]
fn managed_artifacts_archive_empty_tree_and_no_overwrite() {
    let f = Fixture::new();
    let source = f.path("source");
    fs::create_dir(&source).unwrap();
    let archive = f.path("archive");
    let manifest = pack(&source, &archive, "empty", &config()).unwrap();
    assert!(manifest.parts.is_empty());
    extract(&archive, &f.path("dest"), &manifest, 2).unwrap();
    assert!(extract(&archive, &f.path("dest"), &manifest, 2).is_err());
    assert!(pack(&source, &archive, "empty", &config()).is_err());
}

#[test]
fn managed_artifacts_archive_verification_rejects_manifest_drift() {
    let f = Fixture::new();
    let source = f.path("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("x"), "x").unwrap();
    let archive = f.path("archive");
    let mut manifest = pack(&source, &archive, "sample", &config()).unwrap();
    manifest.entries[0].mode = 0o777;
    assert!(verify(&archive, &manifest, 1).is_err());
    manifest.parts[0].file = "../outside.tar.zst".into();
    assert!(verify(&archive, &manifest, 1).is_err());
}

#[test]
fn managed_artifacts_archive_source_changes_abort_commit() {
    let f = Fixture::new();
    let source = f.path("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), "before").unwrap();
    let before = inventory(&source).unwrap();
    let archive = f.path("archive");
    fs::create_dir(&archive).unwrap();
    pack_part(&source, &archive.join("part"), &before, 1, 1).unwrap();
    for change in ["added", "resized", "removed", "mtime"] {
        match change {
            "added" => fs::write(source.join("extra"), "new").unwrap(),
            "resized" => {
                fs::remove_file(source.join("extra")).unwrap();
                fs::write(source.join("file"), "larger data").unwrap();
            }
            "removed" => fs::remove_file(source.join("file")).unwrap(),
            _ => {
                fs::write(source.join("file"), "before").unwrap();
                let mut meta = before[0].clone();
                meta.mtime += 1;
                restore_metadata(&source.join("file"), &meta).unwrap();
            }
        }
        assert!(verify_source(&source, &before).is_err(), "{change}");
    }
}

#[test]
fn managed_artifacts_archive_pax_timestamps_verified_including_before_epoch() {
    let f = Fixture::new();
    let source = f.path("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), "x").unwrap();
    let mut meta = entry("file", 1);
    meta.mtime = -1;
    meta.mtime_nsec = 500_000_000;
    restore_metadata(&source.join("file"), &meta).unwrap();
    assert_eq!(pax_mtime(&meta), "-0.500000000");
    let archive = f.path("archive");
    let mut manifest = pack(&source, &archive, "sample", &config()).unwrap();
    extract(&archive, &f.path("dest"), &manifest, 2).unwrap();
    manifest.entries[0].mtime_nsec += 1;
    assert!(verify(&archive, &manifest, 2).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn managed_artifacts_archive_custom_symlink_modes_do_not_change_target() {
    let f = Fixture::new();
    let source = f.path("source");
    fs::create_dir(&source).unwrap();
    let outside = f.path("outside");
    fs::write(&outside, "outside target").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o640)).unwrap();
    std::os::unix::fs::symlink(&outside, source.join("link")).unwrap();
    let link = std::ffi::CString::new(source.join("link").as_os_str().as_bytes()).unwrap();
    // SAFETY: link is a live C string; NOFOLLOW leaves the target unchanged.
    assert_eq!(
        unsafe {
            libc::fchmodat(
                libc::AT_FDCWD,
                link.as_ptr(),
                0o700,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        },
        0
    );
    assert_eq!(
        fs::symlink_metadata(source.join("link")).unwrap().mode() & 0o7777,
        0o700
    );
    let archive = f.path("archive");
    let manifest = pack(&source, &archive, "sample", &config()).unwrap();
    let dest = f.path("dest");
    extract(&archive, &dest, &manifest, 2).unwrap();
    assert_eq!(
        fs::symlink_metadata(dest.join("link")).unwrap().mode() & 0o7777,
        0o700
    );
    assert_eq!(fs::metadata(&outside).unwrap().mode() & 0o7777, 0o640);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside target");
}

#[test]
fn managed_artifacts_archive_root_metadata_round_trip() {
    let f = Fixture::new();
    let source = f.path("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), "data").unwrap();
    let mut root = root_metadata(&source).unwrap();
    root.mode = 0o755;
    root.mtime = 123456789;
    root.mtime_nsec = 987654321;
    restore_metadata(&source, &root).unwrap();
    let archive = f.path("archive");
    let manifest = pack(&source, &archive, "sample", &config()).unwrap();
    assert_eq!(manifest.root_metadata, root);
    extract(&archive, &f.path("dest"), &manifest, 2).unwrap();
    assert_eq!(root_metadata(&f.path("dest")).unwrap(), root);
}

#[test]
#[ignore = "writes more than 2 GiB; run explicitly for archive acceptance"]
fn managed_artifacts_archive_sparse_over_two_gib_round_trip() {
    use sha2::{Digest, Sha256};
    use std::io::{Seek, SeekFrom};
    fn digest(path: &Path) -> Vec<u8> {
        let mut file = File::open(path).unwrap();
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 1024 * 1024];
        loop {
            let n = file.read(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        hash.finalize().to_vec()
    }
    let f = Fixture::new();
    let source = f.path("source");
    fs::create_dir(&source).unwrap();
    let path = source.join("large");
    let mut file = File::create(&path).unwrap();
    let size = 2 * 1024 * 1024 * 1024 + 123u64;
    file.set_len(size).unwrap();
    file.write_all(b"begin").unwrap();
    file.seek(SeekFrom::End(-3)).unwrap();
    file.write_all(b"end").unwrap();
    file.sync_all().unwrap();
    let archive = f.path("archive");
    let mut settings = config();
    settings.freeze_level = 1;
    settings.freeze_part_bytes = 1024 * 1024;
    let manifest = pack(&source, &archive, "large", &settings).unwrap();
    assert_eq!(manifest.parts.len(), 1);
    let dest = f.path("dest");
    extract(&archive, &dest, &manifest, 2).unwrap();
    assert_eq!(fs::metadata(dest.join("large")).unwrap().len(), size);
    assert_eq!(digest(&path), digest(&dest.join("large")));
}
