//! The remote disk's own report (#2276): its size, free space, what holding
//! keeps, and the drive's SMART verdict. Space is managed here too: the mini
//! deletes whole holding folders, oldest first, to keep free space above the
//! floor the server advertises. Current files are never touched.
use super::client::Destination;
use super::*;
use anyhow::Context;
use std::{
    fs, io,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pruned {
    pub folders: u64,
    pub bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Disk {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub holding_bytes: u64,
    /// The date of the oldest holding folder, `YYYY-MM-DD`.
    pub holding_oldest: Option<String>,
    /// `verified`, `failing`, or `not_reported`.
    pub health: String,
    /// Holding folders deleted for space tonight; null when none were.
    pub pruned: Option<Pruned>,
}
pub const HEALTH: [&str; 3] = ["verified", "failing", "not_reported"];
/// Before the server has advertised a floor, keep this share of the disk free.
pub const DEFAULT_FLOOR_PERCENT: u64 = 10;
/// Where the last floor the server advertised is kept on the remote disk.
pub const FLOOR_KEY: &str = "space_floor_bytes";

/// The `SMART Status:` line of `diskutil info`. Many USB enclosures do not pass
/// SMART through, so anything but Verified or Failing is `not_reported`.
pub fn parse_smart(diskutil: &str) -> &'static str {
    let status = diskutil
        .lines()
        .find_map(|line| line.trim().strip_prefix("SMART Status:"))
        .map(str::trim);
    match status {
        Some("Verified") => "verified",
        Some("Failing") => "failing",
        _ => "not_reported",
    }
}

/// The mount point holding `path`: its highest ancestor on the same device.
fn mount_point(path: &Path) -> Result<PathBuf> {
    let device = fs::metadata(path)?.dev();
    let mut mount = path.to_path_buf();
    while let Some(parent) = mount.parent() {
        if fs::metadata(parent)?.dev() != device {
            break;
        }
        mount = parent.to_path_buf();
    }
    Ok(mount)
}

/// The drive's SMART verdict. Never an error: a failed or slow `diskutil` is
/// `not_reported`, like an enclosure that does not pass SMART through.
pub fn health(path: &Path) -> &'static str {
    // Tests run in parallel threads: a spawned child can briefly hold another
    // test's destination lock. `parse_smart` is tested on its own.
    if cfg!(test) {
        return "not_reported";
    }
    let run = || -> Result<String> {
        let mut child = Command::new("/usr/sbin/diskutil")
            .arg("info")
            .arg(mount_point(path)?)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.try_wait()?.is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("diskutil timed out");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut text = String::new();
        io::Read::read_to_string(
            &mut child.stdout.take().context("diskutil stdout")?,
            &mut text,
        )?;
        Ok(text)
    };
    run()
        .map(|text| parse_smart(&text))
        .unwrap_or("not_reported")
}

/// `(total, free)` bytes of the filesystem holding `path`.
pub fn space(path: &Path) -> Result<(u64, u64)> {
    let disk = nix::sys::statvfs::statvfs(path)?;
    let fragment = disk.fragment_size();
    Ok((
        u64::from(disk.blocks()).saturating_mul(fragment),
        u64::from(disk.blocks_available()).saturating_mul(fragment),
    ))
}

pub(super) fn tree_bytes(path: &Path) -> Result<u64> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Ok(if meta.is_file() { meta.len() } else { 0 });
    }
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        total = total.saturating_add(tree_bytes(&entry?.path())?);
    }
    Ok(total)
}

/// The floor `max(percent × total, 50 GB)` the server sends with each request.
pub fn floor_bytes(percent: f64, total: u64) -> u64 {
    ((percent / 100.0 * total as f64) as u64).max(50_000_000_000)
}

impl Destination {
    /// Holding folders, oldest first. Each is named `<date>-<request>`; runs of
    /// one date are ordered by when they first held a file.
    fn holding_folders(&self) -> Result<Vec<(String, PathBuf)>> {
        let holding = self.root.join(".holding");
        let mut folders = Vec::new();
        match fs::read_dir(&holding) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() {
                        if let Some(name) = entry.file_name().to_str() {
                            folders.push((name.to_owned(), entry.path()));
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut keyed = Vec::new();
        for (name, path) in folders {
            let held: Option<i64> = self.conn.query_row(
                "SELECT MIN(held_at) FROM history WHERE run=?1",
                [&name],
                |r| r.get(0),
            )?;
            let date = name.get(..10).unwrap_or(&name).to_owned();
            keyed.push(((date, held.unwrap_or(i64::MAX), name.clone()), (name, path)));
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(keyed.into_iter().map(|(_, folder)| folder).collect())
    }

    /// The floor to keep free: the last one the server advertised, else 10%.
    pub fn space_floor(&self, total: u64) -> Result<u64> {
        Ok(match self.meta(FLOOR_KEY)? {
            Some(value) => value.parse()?,
            None => total / 100 * DEFAULT_FLOOR_PERCENT,
        })
    }

    /// Delete whole holding folders, oldest first, while `free − need` is below
    /// `floor`. `keep` names tonight's request, whose folder a rollback still
    /// needs. `free` measures the filesystem (injected by tests). Returns what
    /// was deleted, and the bytes still missing when deleting all was not enough.
    pub fn prune_holding(
        &self,
        need: u64,
        floor: u64,
        keep: Option<&str>,
        mut free: impl FnMut(&Path) -> Result<u64>,
        pruned: &mut Pruned,
    ) -> Result<u64> {
        let wanted = need.saturating_add(floor);
        for (run, path) in self.holding_folders()? {
            if free(&self.root)? >= wanted {
                break;
            }
            if keep.is_some_and(|id| run.ends_with(&format!("-{id}"))) {
                continue;
            }
            let bytes = tree_bytes(&path)?;
            // History first: once a version is unlisted, restore --as-of never
            // offers it, even if the folder outlives a crash.
            self.conn
                .execute("DELETE FROM history WHERE run=?1", [&run])?;
            fs::remove_dir_all(&path)?;
            pruned.folders += 1;
            pruned.bytes = pruned.bytes.saturating_add(bytes);
        }
        Ok(wanted.saturating_sub(free(&self.root)?))
    }

    pub fn disk_report(&self, pruned: Option<Pruned>, health: &str) -> Result<Disk> {
        let (total_bytes, free_bytes) = space(&self.root)?;
        let folders = self.holding_folders()?;
        let mut holding_bytes = 0u64;
        for (_, path) in &folders {
            holding_bytes = holding_bytes.saturating_add(tree_bytes(path)?);
        }
        let holding_oldest = folders
            .iter()
            .filter_map(|(name, _)| name.get(..10))
            .find(|date| {
                time::Date::parse(
                    date,
                    time::macros::format_description!("[year]-[month]-[day]"),
                )
                .is_ok()
            })
            .map(str::to_owned);
        Ok(Disk {
            total_bytes,
            free_bytes,
            holding_bytes,
            holding_oldest,
            health: health.into(),
            pruned: pruned.filter(|p| p.folders > 0),
        })
    }
}
