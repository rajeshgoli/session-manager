//! Non-checkout entries directly under the configured worktree root (sm#2338).
use super::*;
use crate::queue::QueueJobRecord;
use std::os::unix::fs::MetadataExt;

type PreviousEntry = (u64, u64, String, Option<String>, Option<String>);

pub const RETENTION: time::Duration = time::Duration::days(14);
const WALK_LIMIT: usize = 200_000;

pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS loose_entries (
        path TEXT PRIMARY KEY, dev INTEGER NOT NULL, ino INTEGER NOT NULL,
        first_seen_at TEXT NOT NULL, last_mtime INTEGER NOT NULL,
        scan_complete INTEGER NOT NULL, size_bytes INTEGER NOT NULL DEFAULT 0,
        deleted_at TEXT, deleted_reason TEXT, quarantine_path TEXT
    );",
    )?;
    Ok(())
}

pub struct Context<'a> {
    pub root: &'a Path,
    pub sessions: &'a [CleanupSession],
    pub jobs: &'a [QueueJobRecord],
    pub pending: &'a [PathBuf],
    pub now: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct LooseEntry {
    pub path: String,
    pub size_bytes: u64,
    pub first_seen_at: String,
    pub deletes_after: String,
    pub blocked_by: Option<String>,
    pub kept: Option<String>,
}

struct Walk {
    newest: i64,
    bytes: u64,
    complete: bool,
    reason: Option<&'static str>,
}

fn walk(path: &Path, cutoff: i64, limit: usize) -> Result<Walk> {
    let mut result = Walk {
        newest: i64::MIN,
        bytes: 0,
        complete: true,
        reason: None,
    };
    let mut pending = vec![path.to_owned()];
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        if visited >= limit {
            result.complete = false;
            result.reason = Some("too large to check");
            break;
        }
        visited += 1;
        let meta = fs::symlink_metadata(&path)?;
        result.newest = result.newest.max(meta.mtime());
        result.bytes = result.bytes.saturating_add(meta.len());
        if meta.mtime() >= cutoff {
            result.complete = false;
            result.reason = Some("recent change");
            break;
        }
        if meta.is_dir() {
            // Iterate lazily: even a single wide directory is bounded by the cap.
            for child in fs::read_dir(&path)? {
                if visited + pending.len() >= limit {
                    result.complete = false;
                    result.reason = Some("too large to check");
                    break;
                }
                pending.push(child?.path());
            }
        }
    }
    Ok(result)
}

fn same(meta: &fs::Metadata, dev: u64, ino: u64) -> bool {
    meta.dev() == dev && meta.ino() == ino
}
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
fn loose(path: &Path) -> bool {
    // A symlink to a checkout remains a loose symlink; never traverse its target.
    fs::symlink_metadata(path).is_ok_and(|m| !m.is_dir() || !exists(&path.join(".git")))
}
fn within(cwd: &str, path: &Path) -> bool {
    let cwd = if let Some(rest) = cwd.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest)
    } else {
        PathBuf::from(cwd)
    };
    cwd.starts_with(path) || fs::canonicalize(&cwd).is_ok_and(|cwd| cwd.starts_with(path))
}
fn argv_mentions(arg: &str, path: &str) -> bool {
    arg.match_indices(path).any(|(at, _)| {
        arg[at + path.len()..].chars().next().is_none_or(|c| {
            c == '/' || c.is_whitespace() || matches!(c, '\'' | '"' | ';' | ')' | ',' | ':')
        })
    })
}
fn in_use(context: &Context<'_>, path: &Path) -> Option<String> {
    if context.pending.iter().any(|p| p.starts_with(path)) {
        return Some("pending handoff note".into());
    }
    let absolute = path.to_string_lossy();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let tilde = home
        .as_ref()
        .and_then(|h| path.strip_prefix(h).ok())
        .map(|p| format!("~/{}", p.display()));
    for job in context.jobs {
        if crate::queue::is_terminal_queue_state(&job.state) {
            continue;
        }
        if within(&job.cwd, path)
            || job.argv.iter().flatten().any(|arg| {
                argv_mentions(arg, &absolute)
                    || tilde.as_ref().is_some_and(|p| argv_mentions(arg, p))
            })
        {
            return Some(format!("queue job {}", job.id));
        }
    }
    context
        .sessions
        .iter()
        .find(|s| s.local && !s.stopped && !s.retired && within(&s.working_dir, path))
        .map(|s| format!("session {}", s.name))
}

/// Rename first, verify the scanned object, and remove only inside quarantine.
/// A mismatched replacement is restored, or retained visibly if its name is taken.
fn remove(
    conn: &Connection,
    context: &Context<'_>,
    path: &Path,
    dev: u64,
    ino: u64,
    after_move: impl FnOnce(&Path),
) -> Result<bool> {
    let quarantine = context.root.join(".sm-quarantine");
    if exists(&quarantine) && !fs::symlink_metadata(&quarantine)?.is_dir() {
        anyhow::bail!("quarantine is not a directory");
    }
    fs::create_dir_all(&quarantine)?;
    let rowid: i64 = conn.query_row(
        "SELECT rowid FROM loose_entries WHERE path=?1",
        [path.to_string_lossy().as_ref()],
        |r| r.get(0),
    )?;
    let target = quarantine.join(format!(
        "{}-{}",
        rowid,
        path.file_name().unwrap().to_string_lossy()
    ));
    if exists(&target) {
        anyhow::bail!("quarantine destination already exists");
    }
    // Persist the destination before moving, so a crash cannot hide the entry.
    conn.execute(
        "UPDATE loose_entries SET quarantine_path=?2 WHERE path=?1",
        params![path.to_string_lossy(), target.to_string_lossy()],
    )?;
    if let Err(error) = fs::rename(path, &target) {
        conn.execute(
            "UPDATE loose_entries SET quarantine_path=NULL WHERE path=?1",
            [path.to_string_lossy().as_ref()],
        )?;
        return Err(error.into());
    }
    after_move(&target);
    let moved = fs::symlink_metadata(&target)?;
    if !same(&moved, dev, ino) || (moved.is_dir() && exists(&target.join(".git"))) {
        let kept = if exists(path) {
            Some(target.display().to_string())
        } else {
            fs::rename(&target, path)?;
            None
        };
        conn.execute("UPDATE loose_entries SET dev=?2,ino=?3,first_seen_at=?4,scan_complete=0,deleted_reason='replaced during removal',quarantine_path=?5 WHERE path=?1",
            params![path.to_string_lossy(),moved.dev(),moved.ino(),context.now.format(&Rfc3339)?,kept])?;
        return Ok(false);
    }
    if moved.is_dir() {
        fs::remove_dir_all(&target)?;
    } else {
        fs::remove_file(&target)?;
    }
    conn.execute("UPDATE loose_entries SET deleted_at=?2,deleted_reason='removed',quarantine_path=NULL WHERE path=?1",
        params![path.to_string_lossy(),context.now.format(&Rfc3339)?])?;
    Ok(true)
}

// An interrupted removal gets restored and a fresh grace period. A name conflict
// remains visible as a separate quarantine row, leaving the new root entry alone.
fn recover_quarantine(conn: &Connection, context: &Context<'_>) -> Result<()> {
    let mut stmt=conn.prepare("SELECT path,quarantine_path FROM loose_entries WHERE quarantine_path IS NOT NULL AND deleted_at IS NULL")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (original, target) in rows {
        if original == target {
            continue;
        }
        let path = Path::new(&original);
        let q = Path::new(&target);
        if q.parent() != Some(context.root.join(".sm-quarantine").as_path()) {
            anyhow::bail!("invalid quarantine path")
        }
        let retained = exists(q) && exists(path);
        if exists(q) && !exists(path) {
            fs::rename(q, path)?;
        }
        let key = if retained { &target } else { &original };
        conn.execute("UPDATE loose_entries SET path=?2,quarantine_path=?3,first_seen_at=?4,scan_complete=0,deleted_reason='replaced during removal' WHERE path=?1",
            params![original,key,if retained {Some(&target)} else {None},context.now.format(&Rfc3339)?])?;
    }
    Ok(())
}

/// Called hourly by the existing cleanup pass. A read of the page never deletes.
pub fn sweep(store: &WorkClaimStore, context: &Context<'_>) -> Result<()> {
    let _guard = lock(cleanup_lock());
    if !context.root.is_dir() {
        return Ok(());
    }
    let conn = store.open_write()?;
    recover_quarantine(&conn, context)?;
    let mut seen = BTreeSet::new();
    let cutoff = (context.now - RETENTION).unix_timestamp();
    for child in fs::read_dir(context.root)? {
        let path = child?.path();
        if path
            .file_name()
            .is_none_or(|n| n.to_string_lossy().starts_with('.'))
            || !loose(&path)
        {
            continue;
        }
        let key = path.display().to_string();
        seen.insert(key.clone());
        let meta = fs::symlink_metadata(&path)?;
        let old: Option<PreviousEntry> = conn.query_row(
            "SELECT dev,ino,first_seen_at,deleted_at,quarantine_path FROM loose_entries WHERE path=?1",[&key],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        if old.as_ref().is_some_and(|r| r.4.is_some()) {
            continue;
        }
        let first = old
            .filter(|r| same(&meta, r.0, r.1) && r.3.is_none())
            .map(|r| r.2)
            .unwrap_or(context.now.format(&Rfc3339)?);
        // Errors inspecting content block the entry and do not abort other entries.
        let scan = walk(&path, cutoff, WALK_LIMIT).unwrap_or(Walk {
            newest: meta.mtime(),
            bytes: 0,
            complete: false,
            reason: Some("could not check"),
        });
        conn.execute("INSERT INTO loose_entries(path,dev,ino,first_seen_at,last_mtime,scan_complete,size_bytes,deleted_reason)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(path) DO UPDATE SET dev=excluded.dev,ino=excluded.ino,
            first_seen_at=excluded.first_seen_at,last_mtime=excluded.last_mtime,scan_complete=excluded.scan_complete,
            size_bytes=excluded.size_bytes,deleted_at=NULL,deleted_reason=excluded.deleted_reason",
            params![key,meta.dev(),meta.ino(),first,scan.newest,scan.complete,scan.bytes,scan.reason])?;
        let old_enough = parse_time(Some(&first)).is_some_and(|at| context.now - at > RETENTION);
        if old_enough
            && scan.complete
            && !load_keeps(&conn)?.contains_key(&key)
            && in_use(context, &path).is_none()
        {
            // Re-read keeps immediately before removal, after the potentially long walk.
            if !load_keeps(&conn)?.contains_key(&key) {
                remove(&conn, context, &path, meta.dev(), meta.ino(), |_| {})?;
            }
        }
    }
    let mut stmt = conn.prepare("SELECT path FROM loose_entries WHERE quarantine_path IS NULL")?;
    let paths = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for path in paths {
        if !seen.contains(&path) {
            conn.execute("DELETE FROM loose_entries WHERE path=?1", [path])?;
        }
    }
    Ok(())
}

pub fn list(store: &WorkClaimStore, context: &Context<'_>) -> Result<Vec<LooseEntry>> {
    let Some(conn) = store.open_existing()? else {
        return Ok(vec![]);
    };
    let keeps = load_keeps(&conn)?;
    let mut stmt = conn.prepare("SELECT path,first_seen_at,size_bytes,scan_complete,deleted_reason,quarantine_path FROM loose_entries WHERE deleted_at IS NULL ORDER BY path")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u64>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut result = vec![];
    for (path, first, size, complete, reason, quarantined) in rows {
        let path = quarantined.unwrap_or(path);
        if !exists(Path::new(&path)) {
            continue;
        }
        let kept = keeps.get(&path).cloned();
        let blocked = if kept.is_some() {
            Some("keep".to_owned())
        } else {
            in_use(context, Path::new(&path))
                .or_else(|| (!complete).then(|| reason.unwrap_or_else(|| "recent change".into())))
        };
        let deletes = parse_time(Some(&first)).unwrap_or(context.now) + RETENTION;
        result.push(LooseEntry {
            path,
            size_bytes: size,
            first_seen_at: first,
            deletes_after: deletes.format(&Rfc3339)?,
            blocked_by: blocked,
            kept,
        });
    }
    Ok(result)
}

/// `None` means this is a checkout: let the existing delete handler decide.
pub fn delete(
    store: &WorkClaimStore,
    context: &Context<'_>,
    path: &Path,
) -> Result<Option<std::result::Result<DeleteOutcome, String>>> {
    let _guard = lock(cleanup_lock());
    let conn = store.open_write()?;
    let key = path.to_string_lossy();
    let enrolled: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM loose_entries WHERE path=?1 AND deleted_at IS NULL)",
        [key.as_ref()],
        |r| r.get(0),
    )?;
    if !enrolled {
        return Ok(None);
    }
    let in_root = path.parent() == Some(context.root);
    let in_quarantine = path.parent() == Some(context.root.join(".sm-quarantine").as_path());
    if (!in_root && !in_quarantine) || !loose(path) {
        return Ok(Some(Err("entry is no longer loose".into())));
    }
    if let Some(reason) = in_use(context, path) {
        return Ok(Some(Err(reason)));
    }
    let meta = fs::symlink_metadata(path)?;
    let removed = remove(&conn, context, path, meta.dev(), meta.ino(), |_| {})?;
    if !removed {
        return Ok(Some(Err("replaced during removal".into())));
    }
    conn.execute("DELETE FROM worktree_keeps WHERE path=?1", [key.as_ref()])?;
    Ok(Some(Ok(DeleteOutcome {
        path: key.into_owned(),
        removed: true,
        cleared: vec![],
        rescued: None,
    })))
}

#[cfg(test)]
mod tests;
