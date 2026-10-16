//! Verified staging copy. Publication and removal belong to the durable
//! operation coordinator; this module never removes the source tree.
use super::{archive, config::ArtifactRoot};
use anyhow::{ensure, Result};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

#[cfg(test)]
thread_local! { pub(super) static TEST_MOUNTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

pub fn cold_mounted(root: &ArtifactRoot) -> bool {
    #[cfg(test)]
    if TEST_MOUNTED.get() {
        return Path::new(&root.cold_store).is_dir();
    }
    match (
        fs::metadata(&root.path),
        fs::symlink_metadata(&root.cold_store),
    ) {
        (Ok(hot), Ok(cold)) => cold.is_dir() && cold.dev() != hot.dev(),
        _ => false,
    }
}

/// Refuse to cross devices or follow links while removing Finder residue.
/// This is intentionally separate from copying so the coordinator can remove
/// residue before recording the source metadata and space requirements.
pub fn remove_finder_files(root: &Path) -> Result<()> {
    fn walk(path: &Path, device: u64) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        ensure!(metadata.dev() == device, "artifact crosses filesystem");
        if !metadata.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_name() == ".DS_Store" && !entry.file_type()?.is_dir() {
                fs::remove_file(entry.path())?;
            } else if entry.file_type()?.is_dir() {
                walk(&entry.path(), device)?;
            }
        }
        Ok(())
    }
    walk(root, fs::symlink_metadata(root)?.dev())
}

#[cfg(target_os = "macos")]
fn copy_file(source: &Path, dest: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)?;
    let dest = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)?;
    // SAFETY: both descriptors remain owned and open for the complete call.
    // DATA|STAT|XATTR preserves decmpfs resource forks and UF_COMPRESSED,
    // unlike a read/write stream which expands compressed data on disk.
    if unsafe {
        libc::fcopyfile(
            source.as_raw_fd(),
            dest.as_raw_fd(),
            std::ptr::null_mut(),
            libc::COPYFILE_DATA | libc::COPYFILE_STAT | libc::COPYFILE_XATTR,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    dest.sync_all()?;
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn copy_file(source: &Path, dest: &Path) -> Result<()> {
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)?;
    let mut dest = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)?;
    io::copy(&mut source, &mut dest)?;
    dest.sync_all()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_metadata(source: &Path, dest: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes())?;
    let dest = CString::new(dest.as_os_str().as_bytes())?;
    // SAFETY: both strings remain valid; NOFOLLOW applies to both endpoints.
    if unsafe {
        libc::copyfile(
            source.as_ptr(),
            dest.as_ptr(),
            std::ptr::null_mut(),
            libc::COPYFILE_STAT | libc::COPYFILE_XATTR | libc::COPYFILE_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn copy_metadata(_source: &Path, _dest: &Path) -> Result<()> {
    Ok(())
}

pub fn copy_verified(source: &Path, staging: &Path) -> Result<()> {
    let root = archive::root_metadata(source)?;
    let before = archive::inventory(source)?;
    fs::create_dir(staging)?;
    for entry in &before {
        let dest = staging.join(&entry.path);
        match entry.kind.as_str() {
            "directory" => fs::create_dir(&dest)?,
            "file" => copy_file(&source.join(&entry.path), &dest)?,
            "symlink" => std::os::unix::fs::symlink(entry.target.as_ref().unwrap(), &dest)?,
            _ => unreachable!("inventory rejects unsupported kinds"),
        }
    }
    for entry in before.iter().rev() {
        let dest = staging.join(&entry.path);
        copy_metadata(&source.join(&entry.path), &dest)?;
        let handle = if entry.kind != "symlink" {
            Some(File::open(&dest)?)
        } else {
            None
        };
        archive::restore_metadata(&dest, entry)?;
        if let Some(handle) = handle {
            handle.sync_all()?;
        }
    }
    ensure!(
        archive::inventory(staging)? == before,
        "move verification failed"
    );
    archive::verify_source(source, &before)?;
    ensure!(
        archive::root_metadata(source)? == root,
        "source root changed during move"
    );
    let handle = File::open(staging)?;
    copy_metadata(source, staging)?;
    archive::restore_metadata(staging, &root)?;
    handle.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_artifacts_move_preserves_literal_appledouble_and_symlinks() {
        let f = crate::managed_artifacts::tests::Fixture::new();
        let source = f.path("source");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("empty")).unwrap();
        fs::write(source.join("._literal"), "ordinary data").unwrap();
        fs::write(source.join(".DS_Store"), "finder").unwrap();
        std::os::unix::fs::symlink("._literal", source.join("link")).unwrap();
        remove_finder_files(&source).unwrap();
        let before = archive::inventory(&source).unwrap();
        let dest = f.path("dest");
        copy_verified(&source, &dest).unwrap();
        assert_eq!(archive::inventory(&dest).unwrap(), before);
        assert_eq!(
            archive::root_metadata(&dest).unwrap(),
            archive::root_metadata(&source).unwrap()
        );
        assert_eq!(
            fs::read_to_string(dest.join("._literal")).unwrap(),
            "ordinary data"
        );
        assert!(!dest.join(".DS_Store").exists());
        assert!(copy_verified(&source, &dest).is_err());
    }
    #[test]
    fn managed_artifacts_cold_store_requires_different_device() {
        let f = crate::managed_artifacts::tests::Fixture::new();
        let root = ArtifactRoot {
            path: f.path("").display().to_string(),
            cold_store: f.path("").display().to_string(),
            ..Default::default()
        };
        assert!(!cold_mounted(&root));
        assert!(!cold_mounted(&ArtifactRoot {
            cold_store: f.path("missing").display().to_string(),
            ..root
        }));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;
    use std::{os::macos::fs::MetadataExt as MacMetadataExt, process::Command};

    #[test]
    fn managed_artifacts_move_keeps_xattrs_and_transparent_compression() {
        let f = crate::managed_artifacts::tests::Fixture::new();
        let source = f.path("source");
        fs::create_dir(&source).unwrap();
        let input = f.path("input");
        let bytes = vec![b'x'; 1024 * 1024];
        fs::write(&input, &bytes).unwrap();
        // ditto constructs only the compressed test fixture. Production copies
        // use copyfile, including literal ._ files ditto would interpret.
        assert!(Command::new("/usr/bin/ditto")
            .arg("--hfsCompression")
            .arg(&input)
            .arg(source.join("compressed"))
            .status()
            .unwrap()
            .success());
        let original = source.join("compressed");
        assert_ne!(
            fs::metadata(&original).unwrap().st_flags() & libc::UF_COMPRESSED,
            0
        );
        for path in [&source, &original] {
            assert!(Command::new("/usr/bin/xattr")
                .args(["-w", "test.sm.archive", "retained"])
                .arg(path)
                .status()
                .unwrap()
                .success());
        }
        let dest = f.path("dest");
        copy_verified(&source, &dest).unwrap();
        let copied = dest.join("compressed");
        assert_ne!(
            fs::metadata(&copied).unwrap().st_flags() & libc::UF_COMPRESSED,
            0
        );
        assert_eq!(fs::read(&copied).unwrap(), bytes);
        for path in [&dest, &copied] {
            let output = Command::new("/usr/bin/xattr")
                .args(["-p", "test.sm.archive"])
                .arg(path)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "retained");
        }
    }
}
