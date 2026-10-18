//! Remote-disk application and restore. Files remain independently readable;
//! SQLite journals history and retries, and state.json names the confirmed head.
use super::outbox::TransferManifest;
use super::*;
use anyhow::{ensure, Context};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::PathBuf,
};

pub struct Destination {
    pub root: PathBuf,
    pub conn: Connection,
    _lock: File,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub confirmed: Option<String>,
    /// Versions of `sm` that `sm backup pull --rollback` backed out; not reinstalled
    /// until the server advertises another version (#2264).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_versions: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Applied {
    pub request: String,
    pub held_deletions: u64,
    pub files: u64,
}
fn stamp() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
fn prefix(path: &str) -> Result<String> {
    safe_relative(path)?;
    let pieces: Vec<_> = path.split('/').collect();
    ensure!(
        pieces.len() >= 3 && matches!(pieces[0], "plain" | "packed"),
        "invalid backup layout path"
    );
    Ok(format!("{}/{}", pieces[0], pieces[1]))
}
/// All parents are real directories: no remote manifest can traverse a link on
/// the destination disk, including a link left by a previous interrupted run.
fn safe_path(root: &Path, relative: &str, parents: bool) -> Result<PathBuf> {
    safe_relative(relative)?;
    let mut path = root.to_path_buf();
    let parts: Vec<_> = relative.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(m) => ensure!(
                !m.file_type().is_symlink() && (i + 1 == parts.len() || m.is_dir()),
                "unsafe backup destination component: {}",
                path.display()
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if parents && i + 1 < parts.len() {
                    fs::create_dir(&path)?;
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}
fn digest(path: &Path, packed: bool, expected: u64) -> Result<String> {
    let file = File::open(path)?;
    let mut reader: Box<dyn Read> = if packed {
        Box::new(zstd::stream::read::Decoder::new(file)?)
    } else {
        Box::new(file)
    };
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count = count
            .checked_add(n as u64)
            .context("backup length overflow")?;
        ensure!(count <= expected, "backup file exceeds recorded size");
        hasher.update(&buf[..n]);
    }
    ensure!(count == expected, "backup file length mismatch");
    Ok(format!("{:x}", hasher.finalize()))
}
pub(super) fn verify(path: &Path, record: &FileRecord) -> Result<()> {
    ensure!(record.target.is_none(), "symlink has no payload");
    ensure!(
        record.sha256.as_deref()
            == Some(&digest(
                path,
                record.path.starts_with("packed/"),
                record.size
            )?),
        "backup file hash mismatch: {}",
        record.path
    );
    Ok(())
}
/// One journaled change to the destination, between its journal and record.
struct Change<'a> {
    path: &'a str,
    old: Option<&'a FileRecord>,
    new: Option<&'a FileRecord>,
    current: PathBuf,
    holding: PathBuf,
}
impl Destination {
    pub fn open_existing(path: &Path) -> Result<Self> {
        ensure!(
            path.is_dir(),
            "backup source does not exist: {}",
            path.display()
        );
        let root = fs::canonicalize(path)?;
        ensure!(
            safe_path(&root, "state.db", false)?.is_file()
                && safe_path(&root, "state.json", false)?.is_file(),
            "backup source is not initialized: {}",
            path.display()
        );
        Self::open(path)
    }
    pub fn open(path: &Path) -> Result<Self> {
        fs::create_dir_all(path)?;
        let root = fs::canonicalize(path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join(".sm-backup.lock"))?;
        // The lock's descriptor lives with Destination, including all retries.
        // A process forked elsewhere in this process inherits the descriptor until it
        // execs, so a just-dropped lock can stay held briefly; wait out that window (#2289).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            ensure!(
                std::time::Instant::now() < deadline,
                "backup destination is already in use"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let db = safe_path(&root, "state.db", false)?;
        let conn = Connection::open(db)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY,body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS history(run TEXT NOT NULL,path TEXT NOT NULL,held_at INTEGER NOT NULL,days INTEGER NOT NULL,body TEXT,PRIMARY KEY(run,path));
            CREATE TABLE IF NOT EXISTS pending_deletes(path TEXT PRIMARY KEY);
            CREATE TABLE IF NOT EXISTS applied(request TEXT PRIMARY KEY,body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);")?;
        Ok(Self {
            root,
            conn,
            _lock: lock,
        })
    }
    pub fn directory(&self, relative: &str) -> Result<PathBuf> {
        let path = safe_path(&self.root, relative, true)?;
        if !path.exists() {
            fs::create_dir(&path)?;
        }
        ensure!(path.is_dir(), "backup staging is not a directory");
        Ok(path)
    }
    pub fn state(&self) -> Result<State> {
        let path = safe_path(&self.root, "state.json", false)?;
        match fs::read(path) {
            Ok(body) => Ok(serde_json::from_slice(&body)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn confirm(&self, id: &str) -> Result<()> {
        let mut state = self.state()?;
        state.confirmed = Some(id.into());
        self.write_state(&state)
    }
    pub fn write_state(&self, state: &State) -> Result<()> {
        let path = safe_path(&self.root, "state.json.tmp", false)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        serde_json::to_writer(&mut file, state)?;
        file.sync_all()?;
        fs::rename(path, safe_path(&self.root, "state.json", false)?)?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub fn records(&self) -> Result<BTreeMap<String, FileRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path,body FROM files ORDER BY path")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(path, body)| Ok((path, serde_json::from_str(&body)?)))
            .collect()
    }
    pub fn applied(&self, id: &str) -> Result<Option<Applied>> {
        let body: Option<String> = self
            .conn
            .query_row("SELECT body FROM applied WHERE request=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        body.map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value])?;
        Ok(())
    }
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Verify every transit part and original-byte hash before touching current
    /// files. Tar entries are never blindly unpacked; only named regular files.
    pub fn unpack(
        &self,
        id: &str,
        manifest: &TransferManifest,
        parts: &[Part],
        download: &Path,
    ) -> Result<PathBuf> {
        safe_relative(id)?;
        ensure!(!id.contains('/'), "invalid backup request id");
        let staging = safe_path(&self.root, &format!(".incoming/{id}"), true)?;
        fs::create_dir_all(&staging)?;
        let mut wanted = BTreeMap::new();
        for record in &manifest.changed {
            prefix(&record.path)?;
            ensure!(
                wanted.insert(record.path.clone(), record).is_none(),
                "duplicate changed path"
            );
            if record.path.starts_with("packed/") && record.target.is_none() {
                ensure!(record.path.ends_with(".zst"), "packed file missing suffix");
            }
        }
        let mut seen = BTreeSet::new();
        for part in parts {
            safe_relative(&part.file)?;
            ensure!(!part.file.contains('/'), "invalid part name");
            let path = safe_path(download, &part.file, false)?;
            ensure!(
                fs::metadata(&path)?.len() == part.bytes
                    && super::outbox::hash_file(&path)? == part.sha256,
                "backup part hash mismatch"
            );
            let mut tar = tar::Archive::new(zstd::stream::read::Decoder::new(File::open(path)?)?);
            for entry in tar.entries()? {
                let mut entry = entry?;
                ensure!(
                    entry.header().entry_type().is_file(),
                    "backup part contains a non-file entry"
                );
                let path = entry
                    .path()?
                    .to_str()
                    .context("non-UTF8 backup path")?
                    .to_owned();
                let record = wanted.get(&path).context("unexpected backup payload")?;
                ensure!(
                    record.target.is_none() && seen.insert(path.clone()),
                    "duplicate or symlink backup payload"
                );
                let dest = safe_path(&staging, &path, true)?;
                let mut file = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&dest)?;
                io::copy(&mut entry, &mut file)?;
                file.sync_all()?;
                verify(&dest, record)?;
            }
        }
        ensure!(
            wanted
                .values()
                .filter(|r| r.target.is_none())
                .all(|r| seen.contains(&r.path)),
            "backup payload is incomplete"
        );
        Ok(staging)
    }

    pub fn apply(
        &mut self,
        id: &str,
        manifest: &TransferManifest,
        staging: &Path,
        allow_deletes: bool,
        max_fraction: f64,
    ) -> Result<Applied> {
        self.apply_with(id, manifest, staging, allow_deletes, max_fraction, false)
    }
    /// A seed's apply moves staged files into place instead of copying them, so
    /// it needs room for one copy of the scope, not two. Staging was verified as
    /// each file was placed; a failed seed calls `return_seed_staging` before
    /// rolling back, so a later seed reuses every staged file.
    pub fn apply_seed(
        &mut self,
        id: &str,
        manifest: &TransferManifest,
        staging: &Path,
    ) -> Result<Applied> {
        self.apply_with(id, manifest, staging, true, 1.0, true)
    }
    fn apply_with(
        &mut self,
        id: &str,
        manifest: &TransferManifest,
        staging: &Path,
        allow_deletes: bool,
        max_fraction: f64,
        consume: bool,
    ) -> Result<Applied> {
        if let Some(applied) = self.applied(id)? {
            return Ok(applied);
        }
        self.apply_journaled(id, manifest, staging, allow_deletes, max_fraction, consume)
    }
    fn apply_journaled(
        &mut self,
        id: &str,
        manifest: &TransferManifest,
        staging: &Path,
        allow_deletes: bool,
        max_fraction: f64,
        consume: bool,
    ) -> Result<Applied> {
        ensure!(manifest.manifest_id == id, "manifest request mismatch");
        ensure!(
            max_fraction.is_finite() && (0.0..=1.0).contains(&max_fraction),
            "invalid deletion fraction"
        );
        safe_relative(id)?;
        ensure!(!id.contains('/'), "invalid request id");
        let pending_key = format!("deletes:{id}");
        if self.meta(&pending_key)?.is_none() {
            let pending = self
                .conn
                .prepare("SELECT path FROM pending_deletes")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            self.set_meta(&pending_key, &serde_json::to_string(&pending)?)?;
        }
        let records = self.records()?;
        let mut deletes: BTreeSet<String> = manifest.deleted.iter().cloned().collect();
        let mut stmt = self.conn.prepare("SELECT path FROM pending_deletes")?;
        deletes.extend(
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
        drop(stmt);
        let changed: BTreeSet<_> = manifest.changed.iter().map(|r| r.path.clone()).collect();
        ensure!(
            changed.len() == manifest.changed.len(),
            "duplicate changed path"
        );
        for path in &changed {
            prefix(path)?;
            deletes.remove(path);
        }
        deletes.retain(|p| records.contains_key(p));
        let mut totals = BTreeMap::<String, u64>::new();
        let mut counts = BTreeMap::<String, u64>::new();
        for path in records.keys() {
            *totals.entry(prefix(path)?).or_default() += 1;
        }
        for path in &deletes {
            *counts.entry(prefix(path)?).or_default() += 1;
        }
        let held: BTreeSet<_> = deletes
            .iter()
            .filter(|p| {
                !allow_deletes
                    && counts[&prefix(p).unwrap()] as f64
                        > totals[&prefix(p).unwrap()] as f64 * max_fraction
            })
            .cloned()
            .collect();
        // Validate all replacement payloads before any history or current files change.
        for record in manifest.changed.iter().filter(|_| !consume) {
            if record.target.is_none() {
                verify(&safe_path(staging, &record.path, false)?, record)?;
            }
        }
        let work: Vec<(&str, Option<&FileRecord>)> = manifest
            .changed
            .iter()
            .map(|r| (r.path.as_str(), Some(r)))
            .chain(deletes.difference(&held).map(|p| (p.as_str(), None)))
            .collect();
        if consume {
            // A seed journals, publishes and records 512 changes at a time:
            // the chunk's journal commits before any of its renames, and one
            // barrier makes the renames durable before the chunk is recorded,
            // instead of commits and folder syncs per file (#2317).
            for chunk in work.chunks(512) {
                let changes = self.in_transaction(|d| {
                    chunk
                        .iter()
                        .filter_map(|(path, new)| {
                            d.journal(id, path, records.get(*path), *new, manifest)
                                .transpose()
                        })
                        .collect::<Result<Vec<_>>>()
                })?;
                let mut touched = BTreeSet::new();
                for change in &changes {
                    self.publish(id, change, staging, Some(&mut touched))?;
                }
                super::outbox::barrier(&touched, &File::open(&self.root)?)?;
                self.in_transaction(|d| changes.iter().try_for_each(|c| d.record(c, true)))?;
            }
        } else {
            for (path, new) in work {
                if let Some(change) = self.journal(id, path, records.get(path), new, manifest)? {
                    self.publish(id, &change, staging, None)?;
                    self.record(&change, false)?;
                }
            }
        }
        let result = Applied {
            request: id.into(),
            held_deletions: held.len() as u64,
            files: self.records()?.len() as u64,
        };
        // A savepoint nests inside a seed's transaction and is a transaction otherwise.
        let tx = self.conn.savepoint()?;
        tx.execute("DELETE FROM pending_deletes", [])?;
        for path in held {
            tx.execute("INSERT INTO pending_deletes(path) VALUES(?1)", [path])?;
        }
        tx.execute(
            "INSERT INTO applied(request,body) VALUES(?1,?2)",
            params![id, serde_json::to_string(&result)?],
        )?;
        tx.commit()?;
        Ok(result)
    }
    /// Run `f` in one SQLite transaction that commits even when `f` fails, so a
    /// partly written journal survives for recovery, as per-statement commits did.
    fn in_transaction<T>(&self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = f(self);
        self.conn.execute_batch("COMMIT")?;
        result
    }
    /// Journal one change before its renames: `None` when nothing changes.
    fn journal<'a>(
        &self,
        id: &str,
        path: &'a str,
        old: Option<&'a FileRecord>,
        new: Option<&'a FileRecord>,
        manifest: &TransferManifest,
    ) -> Result<Option<Change<'a>>> {
        let current = safe_path(&self.root, path, true)?;
        let key = format!("holding:{id}");
        let run = if let Some(run) = self.meta(&key)? {
            run
        } else {
            let run = format!("{}-{id}", time::OffsetDateTime::now_utc().date());
            self.set_meta(&key, &run)?;
            run
        };
        let holding = safe_path(&self.root, &format!(".holding/{run}/{path}"), true)?;
        if old.is_none() && current.exists() {
            ensure!(
                new.is_some_and(
                    |record| record.target.is_none() && verify(&current, record).is_ok()
                ),
                "untracked destination file differs: {path}"
            );
        }
        if old == new {
            return Ok(None);
        }
        let days = manifest
            .holding_days
            .get(&prefix(path)?)
            .copied()
            .unwrap_or(30);
        self.conn.execute(
            "INSERT OR IGNORE INTO history(run,path,held_at,days,body) VALUES(?1,?2,?3,?4,?5)",
            params![
                run,
                path,
                stamp(),
                days,
                old.map(serde_json::to_string).transpose()?
            ],
        )?;
        Ok(Some(Change {
            path,
            old,
            new,
            current,
            holding,
        }))
    }
    /// The journaled change's renames. A seed (`touched`) moves staged files and
    /// collects folders for its chunk's barrier; otherwise each folder is synced.
    fn publish(
        &self,
        id: &str,
        change: &Change,
        staging: &Path,
        touched: Option<&mut BTreeSet<PathBuf>>,
    ) -> Result<()> {
        let Change {
            path,
            old,
            new,
            current,
            holding,
        } = change;
        let consume = touched.is_some();
        // A deterministic holding path makes the filesystem half of the journal
        // repeatable when a process dies between rename and SQLite commit.
        if old.is_some_and(|r| r.target.is_none()) && !holding.exists() {
            ensure!(current.is_file(), "previous backup file missing: {path}");
            fs::rename(current, holding)?;
        }
        if let Some(record) = new.filter(|r| r.target.is_none()) {
            let source = safe_path(staging, path, false)?;
            if consume && !current.exists() {
                fs::rename(source, current)?;
            } else if !current.exists() || verify(current, record).is_err() {
                // Copy preserves staging for a repeat after interrupted apply.
                let temp = safe_path(&self.root, &format!(".incoming/{id}/publish/{path}"), true)?;
                fs::copy(source, &temp)?;
                File::open(&temp)?.sync_all()?;
                fs::rename(temp, current)?;
            }
        } else if current.exists() {
            fs::remove_file(current)?;
        }
        let parents = [current.parent(), holding.parent()].into_iter().flatten();
        match touched {
            // Folders safe_path created must reach their own parents too.
            Some(touched) => {
                for parent in parents {
                    touched.extend(
                        parent
                            .ancestors()
                            .take_while(|dir| dir.starts_with(&self.root))
                            .map(Path::to_path_buf),
                    );
                }
            }
            None => {
                for parent in parents {
                    File::open(parent)?.sync_all()?;
                }
            }
        }
        Ok(())
    }
    /// Record a published change in the destination's file list.
    fn record(&self, change: &Change, batched: bool) -> Result<()> {
        if let Some(record) = change.new {
            self.conn.execute("INSERT INTO files(path,body) VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET body=excluded.body",params![change.path,serde_json::to_string(record)?])?;
        } else {
            self.conn
                .execute("DELETE FROM files WHERE path=?1", [change.path])?;
        }
        if let Some(parent) = change.current.parent().filter(|_| !batched) {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
    /// Move each file a seed's apply placed back to its staging path, so the
    /// rollback that follows restores the previous files without discarding the
    /// staged copies (#2301). Only regular files of `records` that this run
    /// journaled count: adopted and deleted paths are never staged copies.
    pub fn return_seed_staging(
        &self,
        id: &str,
        records: &BTreeMap<String, FileRecord>,
    ) -> Result<()> {
        safe_relative(id)?;
        ensure!(!id.contains('/'), "invalid backup request id");
        let Some(run) = self.meta(&format!("holding:{id}"))? else {
            return Ok(());
        };
        let paths = self
            .conn
            .prepare("SELECT path FROM history WHERE run=?1")?
            .query_map([&run], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let staging = self.directory(&format!(".incoming/{id}"))?;
        for path in paths {
            if records.get(&path).is_none_or(|r| r.target.is_some()) {
                continue;
            }
            // Apply removes a staged file only by moving it into place.
            let current = safe_path(&self.root, &path, false)?;
            let staged = safe_path(&staging, &path, true)?;
            if staged.exists() || !fs::symlink_metadata(&current).is_ok_and(|m| m.is_file()) {
                continue;
            }
            fs::rename(&current, &staged)?;
            for parent in [current.parent(), staged.parent()].into_iter().flatten() {
                File::open(parent)?.sync_all()?;
            }
        }
        Ok(())
    }
    /// Reverse every journaled change before requesting a delta from the still
    /// confirmed head. Keep history intact until all file repairs are durable.
    pub fn rollback(&mut self, id: &str) -> Result<()> {
        safe_relative(id)?;
        ensure!(!id.contains('/'), "invalid backup request id");
        if let Some(run) = self.meta(&format!("holding:{id}"))? {
            let rows = self
                .conn
                .prepare("SELECT path,body FROM history WHERE run=?1")?
                .query_map([&run], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (path, body) in &rows {
                let current = safe_path(&self.root, path, true)?;
                let old: Option<FileRecord> =
                    body.as_ref().map(|b| serde_json::from_str(b)).transpose()?;
                if let Some(record) = old.as_ref().filter(|r| r.target.is_none()) {
                    let held = safe_path(&self.root, &format!(".holding/{run}/{path}"), false)?;
                    if held.exists() {
                        verify(&held, record)?;
                        let temp = safe_path(
                            &self.root,
                            &format!(".incoming/{id}/rollback/{path}"),
                            true,
                        )?;
                        fs::copy(held, &temp)?;
                        File::open(&temp)?.sync_all()?;
                        fs::rename(temp, &current)?;
                    } else {
                        // The process may have stopped after journaling but
                        // before moving the old file into holding.
                        verify(&current, record)?;
                    }
                } else if current.exists() {
                    fs::remove_file(&current)?;
                }
                File::open(current.parent().context("missing backup parent")?)?.sync_all()?;
            }
            let tx = self.conn.transaction()?;
            for (path, body) in rows {
                if let Some(body) = body {
                    tx.execute("INSERT INTO files(path,body) VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET body=excluded.body", params![path, body])?;
                } else {
                    tx.execute("DELETE FROM files WHERE path=?1", [path])?;
                }
            }
            tx.execute("DELETE FROM history WHERE run=?1", [&run])?;
            tx.execute("DELETE FROM applied WHERE request=?1", [id])?;
            tx.commit()?;
            let held = safe_path(&self.root, &format!(".holding/{run}"), false)?;
            if held.exists() {
                fs::remove_dir_all(held)?;
            }
        }
        if let Some(body) = self.meta(&format!("deletes:{id}"))? {
            let pending: Vec<String> = serde_json::from_str(&body)?;
            let tx = self.conn.transaction()?;
            tx.execute("DELETE FROM pending_deletes", [])?;
            for path in pending {
                tx.execute("INSERT INTO pending_deletes(path) VALUES(?1)", [path])?;
            }
            tx.commit()?;
        }
        Ok(())
    }
    pub fn expire_history(&self, at: i64) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT run,path FROM history WHERE held_at+days*86400<?1")?;
        let rows = stmt
            .query_map([at], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (run, path) in rows {
            let file = safe_path(&self.root, &format!(".holding/{run}/{path}"), false)?;
            if file.exists() {
                fs::remove_file(file)?;
            }
            self.conn.execute(
                "DELETE FROM history WHERE run=?1 AND path=?2",
                params![run, path],
            )?;
        }
        Ok(())
    }
    pub fn check(&self) -> Result<super::lifecycle::Check> {
        use rand_core::RngCore;
        let mut records: Vec<_> = self
            .records()?
            .into_values()
            .filter(|r| r.target.is_none())
            .collect();
        let count = records.len().div_ceil(100);
        for i in 0..count {
            let j = i + (rand_core::OsRng.next_u64() as usize % (records.len() - i));
            records.swap(i, j);
        }
        let mut bad = Vec::new();
        for record in records.iter().take(count) {
            if safe_path(&self.root, &record.path, false)
                .and_then(|p| verify(&p, record))
                .is_err()
            {
                bad.push(record.path.clone());
            }
        }
        Ok(super::lifecycle::Check {
            ok: bad.is_empty(),
            files_checked: count as u64,
            bad,
        })
    }
    pub fn restore(&self, to: &Path, as_of: Option<i64>, paths: &[String]) -> Result<u64> {
        fs::create_dir_all(to)?;
        let dest = fs::canonicalize(to)?;
        ensure!(
            !dest.starts_with(&self.root) && !self.root.starts_with(&dest),
            "restore destination overlaps backup"
        );
        for path in paths {
            safe_relative(path)?;
        }
        let mut records: BTreeMap<String, (FileRecord, PathBuf)> = self
            .records()?
            .into_iter()
            .map(|(p, r)| (p.clone(), (r, self.root.join(p))))
            .collect();
        if let Some(at) = as_of {
            let mut stmt=self.conn.prepare("SELECT run,path,body FROM history WHERE held_at>?1 ORDER BY held_at DESC,rowid DESC")?;
            let rows = stmt
                .query_map([at], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut seen = BTreeSet::new();
            for (run, path, body) in rows {
                if !seen.insert(path.clone()) {
                    continue;
                }
                if let Some(body) = body {
                    records.insert(
                        path.clone(),
                        (
                            serde_json::from_str(&body)?,
                            safe_path(&self.root, &format!(".holding/{run}/{path}"), false)?,
                        ),
                    );
                } else {
                    records.remove(&path);
                }
            }
        }
        let mut count = 0;
        // Files first, then links: a restored symlink never becomes an extraction parent.
        let mut ordered: Vec<_> = records.into_iter().collect();
        ordered.sort_by_key(|(_, (r, _))| r.target.is_some());
        for (path, (record, source)) in ordered {
            let relative = path.split_once('/').context("backup layout")?.1;
            let relative = if path.starts_with("packed/") && record.target.is_none() {
                relative.strip_suffix(".zst").context("packed suffix")?
            } else {
                relative
            };
            // A one-file scope restores as the file itself.
            let relative = relative
                .strip_suffix(&format!("/{FILE_ENTRY}"))
                .filter(|scope| !scope.contains('/'))
                .unwrap_or(relative);
            if !paths.is_empty()
                && !paths
                    .iter()
                    .any(|p| relative == p || relative.starts_with(&format!("{p}/")))
            {
                continue;
            }
            let output = safe_path(&dest, relative, true)?;
            ensure!(
                !output.exists(),
                "restore refuses existing path: {relative}"
            );
            if let Some(target) = &record.target {
                std::os::unix::fs::symlink(target, output)?;
            } else {
                verify(&source, &record)?;
                let mut writer = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(record.mode & 0o777)
                    .open(output)?;
                if path.starts_with("packed/") {
                    io::copy(
                        &mut zstd::stream::read::Decoder::new(File::open(source)?)?,
                        &mut writer,
                    )?;
                } else {
                    io::copy(&mut File::open(source)?, &mut writer)?;
                }
                let nanos = record.mtime_ns.unsigned_abs();
                let duration = std::time::Duration::new(
                    u64::try_from(nanos / 1_000_000_000)
                        .context("restore timestamp out of range")?,
                    (nanos % 1_000_000_000) as u32,
                );
                let modified = if record.mtime_ns < 0 {
                    std::time::UNIX_EPOCH.checked_sub(duration)
                } else {
                    std::time::UNIX_EPOCH.checked_add(duration)
                }
                .context("restore timestamp out of range")?;
                writer.set_times(std::fs::FileTimes::new().set_modified(modified))?;
                writer.sync_all()?;
            }
            count += 1;
        }
        Ok(count)
    }
}
