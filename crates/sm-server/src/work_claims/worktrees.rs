//! Worktree lifecycle (sm#1452, ticket #1487): the managed worktree on a
//! claim, `sm worktree keep`, and deletion at retire (appendix G).
//!
//! Deletion is durable by derivation. A retired session's candidates are
//! every `worktree_path` on its claims, plus its working directory when that
//! is on the branch of a merged PR it claimed. A candidate stays pending
//! until a `worktree.removed` event, or a `worktree.left` event saying the
//! path is absent, is written for its path after the session retired. A
//! candidate left for a passing reason (a keep, a live session or process)
//! is checked on every pass; one left for its content (commits not on any
//! remote, uncommitted changes, a failed removal) is checked again every
//! `RECHECK_INTERVAL`, so a later merge or push frees it (sm#1987). A crash
//! only delays the work.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use super::{
    get_claim, get_item, item_keys, now_rfc3339, query_claims, write_event, WorkClaim,
    WorkClaimStore, WorkKind,
};

/// How long a just-retired session's own processes get to exit before a
/// process still inside its worktree counts as a reason to keep it.
pub const RETIRE_PROCESS_GRACE: Duration = Duration::from_secs(5);

/// How often a worktree left for its content is checked again.
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Ignored directories that are build output: rebuilt by the next build, so
/// deleting them never loses work. Matched by name at any depth.
const BUILD_DIRS: [&str; 6] = [
    "target",
    "node_modules",
    "build",
    "dist",
    ".gradle",
    ".next",
];

const REMOVED: &str = "worktree.removed";
const LEFT: &str = "worktree.left";
const REBUILT: &str = "worktree.rebuilt";
const KEEP_EXPIRED: &str = "worktree.keep_expired";
const ABSENT: &str = "absent";
/// Never deleted and never listed as left over: not a worktree sm made.
const MAIN_CHECKOUT: &str = "a main checkout";
const INSIDE_CHECKOUT: &str = "inside another checkout";
const NOT_A_WORKTREE: &str = "not a git worktree";

/// Reasons that settle a candidate for good.
fn settles(reason: &str) -> bool {
    matches!(reason, ABSENT | MAIN_CHECKOUT | INSIDE_CHECKOUT)
}

impl WorkClaimStore {
    /// `POST /claims/worktree`: records the managed worktree on the caller's
    /// active claim. A `None` base keeps the recorded one (a rerun reusing
    /// the worktree). `None` when the caller holds no such claim.
    pub fn set_claim_worktree(
        &self,
        session_id: &str,
        claim_id: &str,
        path: &str,
        branch: &str,
        base_sha: Option<&str>,
    ) -> Result<Option<WorkClaim>> {
        let conn = self.open_write()?;
        let changed = conn.execute(
            "UPDATE work_claims
                SET worktree_path = ?3, branch = ?4, base_sha = COALESCE(?5, base_sha),
                    managed_worktree = 1
              WHERE id = ?1 AND session_id = ?2 AND ended_at IS NULL AND reserved_at IS NULL",
            params![claim_id, session_id, path, branch, base_sha],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        get_claim(&conn, claim_id)
    }

    /// `sm worktree keep`: don't delete `path` at retire.
    pub fn set_worktree_keep(&self, path: &str, session_id: &str, reason: &str) -> Result<()> {
        self.open_write()?.execute(
            "INSERT INTO worktree_keeps (path, session_id, reason, kept_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET session_id = excluded.session_id,
                 reason = excluded.reason, kept_at = excluded.kept_at",
            params![path, session_id, reason, now_rfc3339()],
        )?;
        Ok(())
    }

    /// `sm worktree keep --off`. Returns whether a keep existed.
    pub fn clear_worktree_keep(&self, path: &str) -> Result<bool> {
        Ok(self
            .open_write()?
            .execute("DELETE FROM worktree_keeps WHERE path = ?1", params![path])?
            > 0)
    }

    /// Every keep: path → reason.
    pub fn worktree_keeps(&self) -> Result<BTreeMap<String, String>> {
        let Some(conn) = self.open_read()? else {
            return Ok(BTreeMap::new());
        };
        load_keeps(&conn)
    }
}

/// A session as the deletion pass sees it.
#[derive(Debug, Clone)]
pub struct CleanupSession {
    pub id: String,
    pub name: String,
    pub working_dir: String,
    /// Retired or killed, per the session record.
    pub retired: bool,
    /// Not running: stopped, retired or killed.
    pub stopped: bool,
    /// When it was retired (`completed_at`, else `stopped_at`).
    pub retired_at: Option<String>,
    /// On this machine; sessions on remote nodes are skipped.
    pub local: bool,
}

/// One candidate's result, as the retire response carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeOutcome {
    pub path: String,
    pub removed: bool,
    pub reason: String,
}

/// Filled while a pass runs, so a caller with a deadline reads what is done.
#[derive(Debug, Default)]
pub struct CleanupProgress {
    /// The `first` session's candidate paths, once gathered.
    pub first_paths: Option<Vec<String>>,
    pub outcomes: Vec<WorktreeOutcome>,
}

#[derive(Default)]
pub struct CleanupRequest<'a> {
    pub sessions: &'a [CleanupSession],
    /// The session just retired: its candidates go first, it counts as
    /// retired even when its record only says stopped, and its own processes
    /// get `RETIRE_PROCESS_GRACE` to exit.
    pub first: Option<&'a str>,
    pub progress: Option<&'a Mutex<CleanupProgress>>,
    /// How long a worktree left for its content waits before it is checked
    /// again; `None` is `RECHECK_INTERVAL`.
    pub recheck_after: Option<Duration>,
}

/// Where a candidate came from; the pass reads these to decide and to key
/// its event.
#[derive(Debug, Clone)]
struct Source {
    session_id: String,
    repo: String,
    kind: WorkKind,
    number: i64,
    branch: Option<String>,
    managed_base: Option<String>,
}

#[derive(Debug, Clone)]
struct Candidate {
    path: String,
    sources: Vec<Source>,
    sessions: BTreeSet<String>,
    retired_at: Option<OffsetDateTime>,
}

#[derive(Debug)]
struct PathEvent {
    at: Option<OffsetDateTime>,
    kind: String,
    reason: String,
    retryable: bool,
}

enum Decision {
    /// `branch` is the branch the worktree had checked out, when it had one;
    /// `git_common_dir` is its repository, which a restore rebuilds from.
    Removed {
        reason: String,
        branch: Option<String>,
        git_common_dir: String,
    },
    Left {
        reason: String,
        retryable: bool,
    },
}

fn cleanup_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

/// When each worktree left for its content was last checked, so it waits
/// `RECHECK_INTERVAL` before the next. Empty after a restart, which checks
/// each once.
fn last_checked() -> &'static Mutex<BTreeMap<String, Instant>> {
    static CHECKED: OnceLock<Mutex<BTreeMap<String, Instant>>> = OnceLock::new();
    CHECKED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// What one pass reads once and reuses: the process listing, and each
/// repository's fetch of its remotes.
#[derive(Default)]
struct PassCache {
    processes: Option<ProcessListing>,
    fetched: BTreeMap<String, Result<(), String>>,
}

/// Retired sessions whose working directory was checked and is not a
/// candidate (or is settled): not re-run with git on every pass.
fn working_dirs_done() -> &'static Mutex<HashSet<String>> {
    static DONE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    DONE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// One deletion pass over every pending candidate. Holds a process-wide
/// lock so two passes never act on one path.
pub fn run_worktree_cleanup(
    store: &WorkClaimStore,
    request: CleanupRequest<'_>,
) -> Result<Vec<WorktreeOutcome>> {
    let _guard = cleanup_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(conn) = store.open_existing()? else {
        if let Some(progress) = request.progress {
            lock(progress).first_paths = Some(Vec::new());
        }
        return Ok(Vec::new());
    };
    prune_keeps(&conn)?;
    let retired = retired_sessions(&conn, &request)?;
    let events = path_events(&conn)?;
    let first = request.first.unwrap_or_default();
    let recheck_after = request.recheck_after.unwrap_or(RECHECK_INTERVAL);
    let checked = lock(last_checked()).clone();
    let mut candidates = gather(&conn, &request, &retired)?
        .into_values()
        .filter(|candidate| is_pending(candidate, &events))
        .filter(|candidate| {
            candidate.sessions.contains(first)
                || !waits_for_recheck(
                    candidate,
                    &events,
                    checked.get(&candidate.path),
                    recheck_after,
                )
        })
        .collect::<Vec<_>>();
    candidates
        .sort_by_key(|candidate| (!candidate.sessions.contains(first), candidate.path.clone()));
    if let Some(progress) = request.progress {
        lock(progress).first_paths = Some(
            candidates
                .iter()
                .filter(|candidate| candidate.sessions.contains(first))
                .map(|candidate| candidate.path.clone())
                .collect(),
        );
    }
    let keeps = load_keeps(&conn)?;
    let mut cache = PassCache::default();
    let mut outcomes = Vec::new();
    for candidate in &candidates {
        let grace = candidate.sessions.contains(first);
        let decision = decide(
            &conn, candidate, &request, &retired, &keeps, &mut cache, grace,
        )?;
        if let Decision::Left {
            retryable: false, ..
        } = &decision
        {
            lock(last_checked()).insert(candidate.path.clone(), Instant::now());
        }
        let outcome = record(&conn, candidate, decision, &events)?;
        if let Some(progress) = request.progress {
            lock(progress).outcomes.push(outcome.clone());
        }
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn parse_time(value: Option<&str>) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value?, &Rfc3339).ok()
}

/// Local sessions that count as retired, with when: retired per the record,
/// or stopped with a claim that retire ended (`sm retire` on a stopped
/// session), or the session just retired.
fn retired_sessions(
    conn: &Connection,
    request: &CleanupRequest<'_>,
) -> Result<BTreeMap<String, Option<OffsetDateTime>>> {
    let mut retired_by_claims = BTreeMap::<String, Option<OffsetDateTime>>::new();
    let mut statement = conn.prepare(
        "SELECT session_id, MAX(ended_at) FROM work_claims
          WHERE end_reason = 'retired' GROUP BY session_id",
    )?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })? {
        let (session_id, ended_at) = row?;
        retired_by_claims.insert(session_id, parse_time(ended_at.as_deref()));
    }
    let mut retired = BTreeMap::new();
    for session in request.sessions.iter().filter(|session| session.local) {
        let at = if session.retired {
            Some(parse_time(session.retired_at.as_deref()))
        } else if Some(session.id.as_str()) == request.first {
            Some(retired_by_claims.get(&session.id).copied().flatten())
        } else if session.stopped {
            retired_by_claims.get(&session.id).copied()
        } else {
            None
        };
        if let Some(at) = at {
            retired.insert(session.id.clone(), at);
        }
    }
    Ok(retired)
}

/// The key a path is deduplicated and recorded under: symlinks resolved
/// when it exists, without a trailing slash.
pub(crate) fn path_key(path: &str) -> String {
    let trimmed = path.trim();
    let trimmed = if trimmed.len() > 1 {
        trimmed.trim_end_matches('/')
    } else {
        trimmed
    };
    fs::canonicalize(trimmed)
        .map(|resolved| resolved.display().to_string())
        .unwrap_or_else(|_| trimmed.to_owned())
}

fn gather(
    conn: &Connection,
    request: &CleanupRequest<'_>,
    retired: &BTreeMap<String, Option<OffsetDateTime>>,
) -> Result<BTreeMap<String, Candidate>> {
    let mut candidates = BTreeMap::<String, Candidate>::new();
    let mut add = |path: String, source: Source, at: Option<OffsetDateTime>| {
        let key = path_key(&path);
        let entry = candidates.entry(key.clone()).or_insert_with(|| Candidate {
            path: key,
            sources: Vec::new(),
            sessions: BTreeSet::new(),
            retired_at: None,
        });
        entry.sessions.insert(source.session_id.clone());
        entry.retired_at = match (entry.retired_at, at) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        entry.sources.push(source);
    };
    for session in request.sessions {
        let Some(at) = retired.get(&session.id) else {
            continue;
        };
        let claims = query_claims(
            conn,
            "WHERE session_id = ?1 AND reserved_at IS NULL ORDER BY claimed_at, id",
            params![session.id],
        )?;
        for claim in &claims {
            let Some(path) = claim
                .worktree_path
                .as_deref()
                .filter(|p| !p.trim().is_empty())
            else {
                continue;
            };
            add(
                path.to_owned(),
                Source {
                    session_id: session.id.clone(),
                    repo: claim.repo.clone(),
                    kind: claim.kind(),
                    number: claim.number,
                    branch: claim.branch.clone(),
                    managed_base: claim
                        .managed_worktree
                        .then(|| claim.base_sha.clone())
                        .flatten(),
                },
                *at,
            );
        }
        if let Some((path, source)) = working_dir_candidate(conn, session, &claims)? {
            add(path, source, *at);
        }
    }
    Ok(candidates)
}

/// The session's working directory when its current branch is the head
/// branch of a merged PR it claimed.
fn working_dir_candidate(
    conn: &Connection,
    session: &CleanupSession,
    claims: &[WorkClaim],
) -> Result<Option<(String, Source)>> {
    if lock(working_dirs_done()).contains(&session.id) {
        return Ok(None);
    }
    let mut merged = Vec::new();
    for claim in claims.iter().filter(|claim| claim.kind() == WorkKind::Pr) {
        if let Some(item) = get_item(conn, &claim.repo, claim.number)? {
            if item.state == "merged" {
                if let Some(head_ref) = item.head_ref.filter(|r| !r.is_empty()) {
                    merged.push((head_ref, claim.repo.clone(), claim.number));
                }
            }
        }
    }
    let dir = Path::new(&session.working_dir);
    let found = (!merged.is_empty() && dir.is_dir())
        .then(|| git(dir, &["branch", "--show-current"]))
        .flatten()
        .and_then(|branch| {
            let (_, repo, pr) = merged.iter().find(|(head, _, _)| *head == branch)?;
            let top = git(dir, &["rev-parse", "--show-toplevel"])?;
            Some((
                top,
                Source {
                    session_id: session.id.clone(),
                    repo: repo.clone(),
                    kind: WorkKind::Pr,
                    number: *pr,
                    branch: Some(branch),
                    managed_base: None,
                },
            ))
        });
    if found.is_none() {
        lock(working_dirs_done()).insert(session.id.clone());
    }
    Ok(found)
}

/// Every worktree event by path, oldest first.
fn path_events(conn: &Connection) -> Result<BTreeMap<String, Vec<PathEvent>>> {
    let mut statement = conn.prepare(
        "SELECT ts, kind, payload FROM events
          WHERE kind IN ('worktree.removed', 'worktree.left') ORDER BY id",
    )?;
    let mut events = BTreeMap::<String, Vec<PathEvent>>::new();
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })? {
        let (ts, kind, payload) = row?;
        let payload: Value = payload
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null);
        let Some(path) = payload["path"].as_str() else {
            continue;
        };
        events.entry(path.to_owned()).or_default().push(PathEvent {
            at: parse_time(Some(&ts)),
            kind,
            reason: payload["reason"].as_str().unwrap_or_default().to_owned(),
            retryable: payload["retryable"].as_bool() == Some(true),
        });
    }
    Ok(events)
}

fn after_retire(event: &PathEvent, candidate: &Candidate) -> bool {
    match (event.at, candidate.retired_at) {
        (Some(at), Some(retired)) => at >= retired,
        _ => true,
    }
}

/// Pending until a removal, or a `worktree.left` that settles it (the path
/// is absent, or is not a worktree sm made), is recorded for the path after
/// the latest of its sessions retired.
fn is_pending(candidate: &Candidate, events: &BTreeMap<String, Vec<PathEvent>>) -> bool {
    !events.get(&candidate.path).is_some_and(|events| {
        events.iter().any(|event| {
            after_retire(event, candidate) && (event.kind == REMOVED || settles(&event.reason))
        })
    })
}

/// Left for its content (not a passing reason) by its latest check since
/// the retire, and checked in this process less than `recheck_after` ago.
fn waits_for_recheck(
    candidate: &Candidate,
    events: &BTreeMap<String, Vec<PathEvent>>,
    checked: Option<&Instant>,
    recheck_after: Duration,
) -> bool {
    checked.is_some_and(|at| at.elapsed() < recheck_after)
        && latest_after_retire(candidate, events).is_some_and(|event| !event.retryable)
}

fn latest_after_retire<'e>(
    candidate: &Candidate,
    events: &'e BTreeMap<String, Vec<PathEvent>>,
) -> Option<&'e PathEvent> {
    events
        .get(&candidate.path)?
        .iter()
        .rev()
        .find(|event| after_retire(event, candidate))
}

fn load_keeps(conn: &Connection) -> Result<BTreeMap<String, String>> {
    let mut statement = conn.prepare("SELECT path, reason FROM worktree_keeps")?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<BTreeMap<String, String>>>()?;
    Ok(rows)
}

/// A keep ends when its path no longer exists.
fn prune_keeps(conn: &Connection) -> Result<()> {
    for path in load_keeps(conn)?.into_keys() {
        if !Path::new(&path).exists() {
            conn.execute("DELETE FROM worktree_keeps WHERE path = ?1", params![path])?;
        }
    }
    Ok(())
}

/// `git -C dir args`, trimmed stdout on success.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn is_inside(path: &str, root: &str) -> bool {
    path == root || path.starts_with(&format!("{}/", root.trim_end_matches('/')))
}

/// Processes whose current directory is `root` or under it:
/// `lsof -a -d cwd -Fpcn`, run from the server's own cwd (appendix K, G8).
/// A shell is usually just the wrapper around the process that matters, so
/// another process inside is named first when there is one.
fn processes_inside(root: &str, listing: &[(u32, String, String)]) -> Option<(u32, String)> {
    const SHELLS: [&str; 6] = ["zsh", "bash", "sh", "fish", "dash", "ksh"];
    let inside = || listing.iter().filter(|(_, _, cwd)| is_inside(cwd, root));
    inside()
        .find(|(_, command, _)| !SHELLS.contains(&command.trim_start_matches('-')))
        .or_else(|| inside().next())
        .map(|(pid, command, _)| (*pid, command.clone()))
}

/// Every process's cwd, or why the listing could not be made. A failed
/// listing must never read as "no processes".
type ProcessListing = Result<Vec<(u32, String, String)>, String>;

#[cfg(test)]
thread_local! {
    /// Tests substitute a missing program to exercise the failure path.
    static LSOF_PROGRAM: std::cell::Cell<&'static str> = const { std::cell::Cell::new("lsof") };
}

#[cfg(test)]
fn lsof_program() -> &'static str {
    LSOF_PROGRAM.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn lsof_program() -> &'static str {
    "lsof"
}

fn list_process_cwds() -> ProcessListing {
    let output = Command::new(lsof_program())
        .args(["-a", "-d", "cwd", "-Fpcn"])
        .output()
        .map_err(|error| format!("lsof could not run: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    // lsof exits 1 when some process could not be read but still lists the
    // rest; only an empty listing with a failure status is no listing at all.
    if !output.status.success() && text.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "lsof failed: {}",
            stderr.lines().next().unwrap_or("no output").trim()
        ));
    }
    Ok(parse_lsof(&text))
}

/// `p<pid>`, `c<command>`, `n<path>` records.
pub(crate) fn parse_lsof(text: &str) -> Vec<(u32, String, String)> {
    let own = std::process::id();
    let mut rows = Vec::new();
    let (mut pid, mut command) = (None, String::new());
    for line in text.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => {
                pid = value.parse::<u32>().ok();
                command.clear();
            }
            "c" => command = value.to_owned(),
            "n" => {
                if let Some(pid) = pid.filter(|pid| *pid != own) {
                    rows.push((pid, command.clone(), value.to_owned()));
                }
            }
            _ => {}
        }
    }
    rows
}

/// Why `path` is not a linked worktree sm may delete, or its git dirs:
/// `(common_dir)` on success.
fn linked_worktree(path: &Path, key: &str) -> std::result::Result<String, &'static str> {
    let dirs = git(
        path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
            "--show-toplevel",
        ],
    );
    let dirs = dirs
        .as_deref()
        .map(|text| text.lines().map(path_key).collect::<Vec<_>>())
        .unwrap_or_default();
    let [git_dir, common_dir, top] = dirs.as_slice() else {
        return Err(NOT_A_WORKTREE);
    };
    if top != key {
        return Err(INSIDE_CHECKOUT);
    }
    if git_dir == common_dir {
        return Err(MAIN_CHECKOUT);
    }
    Ok(common_dir.clone())
}

/// Why a live session or process makes `candidate` unsafe to touch now.
fn in_use(
    conn: &Connection,
    candidate: &Candidate,
    request: &CleanupRequest<'_>,
    retired: &BTreeMap<String, Option<OffsetDateTime>>,
    processes: &mut Option<ProcessListing>,
    grace: bool,
) -> Result<Option<String>> {
    // No other session that is not retired works inside it.
    for session in request.sessions {
        if !session.local
            || retired.contains_key(&session.id)
            || candidate.sessions.contains(&session.id)
        {
            continue;
        }
        let cwd = path_key(&session.working_dir);
        let holds = query_claims(
            conn,
            "WHERE session_id = ?1 AND ended_at IS NULL AND worktree_path IS NOT NULL",
            params![session.id],
        )?
        .iter()
        .any(|claim| {
            claim
                .worktree_path
                .as_deref()
                .is_some_and(|p| path_key(p) == candidate.path)
        });
        if is_inside(&cwd, &candidate.path) || holds {
            return Ok(Some(format!("in use by {}", session.name)));
        }
    }
    // No process has its cwd inside. The retired agent's own processes may
    // take a moment to exit after its panes close.
    let deadline = Instant::now()
        + if grace {
            RETIRE_PROCESS_GRACE
        } else {
            Duration::ZERO
        };
    loop {
        let listing = match processes.get_or_insert_with(list_process_cwds) {
            Ok(listing) => listing,
            Err(error) => return Ok(Some(format!("process check failed: {error}"))),
        };
        let Some((pid, command)) = processes_inside(&candidate.path, listing) else {
            return Ok(None);
        };
        if Instant::now() >= deadline {
            return Ok(Some(format!("process {pid} ({command}) runs in it")));
        }
        thread::sleep(Duration::from_millis(500));
        *processes = None;
    }
}

fn decide(
    conn: &Connection,
    candidate: &Candidate,
    request: &CleanupRequest<'_>,
    retired: &BTreeMap<String, Option<OffsetDateTime>>,
    keeps: &BTreeMap<String, String>,
    cache: &mut PassCache,
    grace: bool,
) -> Result<Decision> {
    let path = Path::new(&candidate.path);
    if !path.exists() {
        return Ok(Decision::Left {
            reason: ABSENT.to_owned(),
            retryable: false,
        });
    }
    let final_left = |reason: String| {
        Ok(Decision::Left {
            reason,
            retryable: false,
        })
    };
    let retry_left = |reason: String| {
        Ok(Decision::Left {
            reason,
            retryable: true,
        })
    };
    // 1. A linked worktree, never a main checkout.
    let common_dir = match linked_worktree(path, &candidate.path) {
        Ok(common_dir) => common_dir,
        Err(reason) => return final_left(reason.to_owned()),
    };
    // 2. No keep, unless every PR it names has merged or closed.
    if let Some(reason) = keeps.get(&candidate.path) {
        if !expire_keep(conn, candidate, reason)? {
            return retry_left(format!("kept: {reason}"));
        }
    }
    // 3-4. No live session or process inside.
    if let Some(reason) = in_use(
        conn,
        candidate,
        request,
        retired,
        &mut cache.processes,
        grace,
    )? {
        return retry_left(reason);
    }
    // 5. Nothing would be lost: no uncommitted change, and HEAD is on a
    // remote or is a merged PR's head.
    let Some(head) = git(path, &["rev-parse", "HEAD"]) else {
        return final_left("no commit checked out".to_owned());
    };
    match git(path, &["status", "--porcelain"]) {
        None => return final_left("git status failed".to_owned()),
        Some(changes) if !changes.is_empty() => {
            return final_left(count(changes.lines().count(), "uncommitted change"));
        }
        Some(_) => {}
    }
    let removed_reason = match safe_head(conn, candidate, &head, &common_dir, &mut cache.fetched)? {
        Ok(reason) => reason,
        Err(reason) => return final_left(reason),
    };
    // A keep acknowledged while this pass was running wins: re-read it right
    // before the removal instead of trusting the pass's first snapshot.
    if let Some(reason) = load_keeps(conn)?.get(&candidate.path) {
        return retry_left(format!("kept: {reason}"));
    }
    // The branch to delete afterwards is the one checked out: agents may
    // rename the branch setup made (sm#1567).
    let checked_out = git(path, &["branch", "--show-current"]).filter(|b| !b.is_empty());
    // 6. git removes it, refusing modified or untracked files. Build output
    // goes first: a build still writing into `target/` made the removal die
    // on "Directory not empty" with the worktree half gone.
    if let Err(error) = remove_worktree(path, &common_dir, false) {
        return final_left(format!("git refused: {error}"));
    }
    // Squash merges never make the branch an ancestor of main: -D, but only
    // when its tip is still the HEAD that was checked. A detached HEAD falls
    // back to the branches the claims recorded.
    let branches = match &checked_out {
        Some(branch) => vec![branch.as_str()],
        None => recorded_branches(candidate),
    };
    for branch in branches {
        let tip = git_in(
            &common_dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .ok();
        if tip.as_deref() == Some(head.as_str()) {
            let _ = git_in(&common_dir, &["branch", "-D", branch]);
        }
    }
    Ok(Decision::Removed {
        reason: removed_reason,
        branch: checked_out,
        git_common_dir: common_dir,
    })
}

/// `git --git-dir <common_dir> args`: trimmed stdout, or the first stderr
/// line without `fatal: `.
fn git_in(common_dir: &str, args: &[&str]) -> std::result::Result<String, String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(common_dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("git failed");
    Err(line.strip_prefix("fatal: ").unwrap_or(line).to_owned())
}

/// `n thing` / `n things`.
fn count(n: usize, thing: &str) -> String {
    format!("{n} {thing}{}", if n == 1 { "" } else { "s" })
}

/// Condition 5: HEAD is a managed worktree's base (nothing was ever
/// committed), the head of any merged PR sm knows in the repo (a squash
/// merge whose branch is deleted), or reachable from a remote-tracking ref
/// after a fetch. `Ok` is the removal reason, `Err` why it is kept.
fn safe_head(
    conn: &Connection,
    candidate: &Candidate,
    head: &str,
    common_dir: &str,
    fetched: &mut BTreeMap<String, Result<(), String>>,
) -> Result<std::result::Result<String, String>> {
    if candidate
        .sources
        .iter()
        .any(|source| source.managed_base.as_deref() == Some(head))
    {
        return Ok(Ok("no commits".to_owned()));
    }
    let repos = candidate
        .sources
        .iter()
        .map(|source| source.repo.as_str())
        .collect::<BTreeSet<_>>();
    for repo in repos {
        let merged = conn
            .query_row(
                "SELECT number FROM work_items
                  WHERE repo = ?1 AND kind = 'pr' AND state = 'merged' AND head_sha = ?2
                  ORDER BY number LIMIT 1",
                params![repo, head],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(number) = merged {
            return Ok(Ok(format!("PR #{number} merged")));
        }
    }
    let fetch = fetched
        .entry(common_dir.to_owned())
        .or_insert_with(|| git_in(common_dir, &["fetch", "--all", "--prune", "--quiet"]).map(drop));
    if let Err(error) = fetch {
        return Ok(Err(format!("fetch failed: {error}")));
    }
    if let Some(remote) = remote_ref_containing(common_dir, head) {
        return Ok(Ok(format!("pushed to {remote}")));
    }
    let local = git_in(
        common_dir,
        &["rev-list", "--count", head, "--not", "--remotes"],
    )
    .ok()
    .and_then(|n| n.parse::<usize>().ok())
    .unwrap_or_default();
    Ok(Err(format!(
        "{} only on this Mac",
        count(local.max(1), "commit")
    )))
}

/// A remote-tracking branch whose history holds `head`, preferring a named
/// branch over `origin/HEAD`.
fn remote_ref_containing(common_dir: &str, head: &str) -> Option<String> {
    let refs = git_in(
        common_dir,
        &[
            "for-each-ref",
            "--contains",
            head,
            "--format=%(refname:short)",
            "refs/remotes",
        ],
    )
    .ok()?;
    let refs = refs
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    refs.iter()
        .find(|name| !name.ends_with("/HEAD") && name.contains('/'))
        .or(refs.first())
        .map(|name| (*name).to_owned())
}

/// PR numbers a keep's reason names: `PR #12`, `PR 12`, `pr#12`, or a
/// `/pull/12` link.
pub(crate) fn keep_prs(reason: &str) -> Vec<i64> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r"(?i)\bPR\s*#?\s*(\d+)|/pull/(\d+)").expect("valid pattern")
    });
    let mut numbers = pattern
        .captures_iter(reason)
        .filter_map(|caps| caps.get(1).or(caps.get(2))?.as_str().parse().ok())
        .collect::<Vec<i64>>();
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

/// A keep whose reason names PRs ends once every one of them has merged or
/// closed in one of the candidate's repos (sm#1987): the reason it was kept
/// is over. Deletes the keep and records `worktree.keep_expired`.
fn expire_keep(conn: &Connection, candidate: &Candidate, reason: &str) -> Result<bool> {
    let numbers = keep_prs(reason);
    if numbers.is_empty() {
        return Ok(false);
    }
    let repos = candidate
        .sources
        .iter()
        .map(|source| source.repo.as_str())
        .collect::<BTreeSet<_>>();
    for number in &numbers {
        let mut done = false;
        for repo in &repos {
            if let Some(item) = get_item(conn, repo, *number)? {
                done |= matches!(item.state.as_str(), "merged" | "closed");
            }
        }
        if !done {
            return Ok(false);
        }
    }
    conn.execute(
        "DELETE FROM worktree_keeps WHERE path = ?1",
        params![candidate.path],
    )?;
    let source = event_source(candidate);
    let (ticket, pr) = item_keys(conn, &source.repo, source.kind, source.number)?;
    write_event(
        conn,
        KEEP_EXPIRED,
        Some(&source.session_id),
        Some(&source.repo),
        ticket,
        pr,
        json!({"path": candidate.path, "reason": reason, "prs": numbers}),
        &now_rfc3339(),
    )?;
    Ok(true)
}

/// Ignored build-output directories under `path` (see `BUILD_DIRS`).
fn build_dirs(path: &Path) -> Vec<PathBuf> {
    let Some(listing) = git(
        path,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
    ) else {
        return Vec::new();
    };
    listing
        .lines()
        .filter_map(|line| line.strip_suffix('/'))
        .filter(|dir| {
            Path::new(dir)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| BUILD_DIRS.contains(&name))
        })
        .map(|dir| path.join(dir))
        .collect()
}

/// Deletes the ignored build output under `path`; the directories removed.
fn clear_build_output(path: &Path) -> std::result::Result<Vec<PathBuf>, String> {
    let dirs = build_dirs(path);
    for dir in &dirs {
        fs::remove_dir_all(dir).map_err(|error| format!("deleting {}: {error}", dir.display()))?;
    }
    Ok(dirs)
}

/// Build output first, then `git worktree remove` (`--force` drops
/// uncommitted changes too). A folder git no longer knows as a worktree is
/// an error, never silently deleted here.
fn remove_worktree(path: &Path, common_dir: &str, force: bool) -> std::result::Result<(), String> {
    clear_build_output(path)?;
    let target = path.display().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&target);
    git_in(common_dir, &args).map(drop)
}

/// The branch recorded for the path: a managed claim's first, else any.
fn candidate_branch(candidate: &Candidate) -> Option<&str> {
    recorded_branches(candidate).into_iter().next()
}

/// Every branch the claims recorded for the path, a managed claim's first.
fn recorded_branches(candidate: &Candidate) -> Vec<&str> {
    let mut branches = Vec::new();
    for branch in candidate
        .sources
        .iter()
        .filter(|source| source.managed_base.is_some())
        .chain(candidate.sources.iter())
        .filter_map(|source| source.branch.as_deref().filter(|b| !b.is_empty()))
    {
        if !branches.contains(&branch) {
            branches.push(branch);
        }
    }
    branches
}

/// The source that keys the event: a ticket claim before a PR claim.
fn event_source(candidate: &Candidate) -> &Source {
    candidate
        .sources
        .iter()
        .filter(|source| source.kind == WorkKind::Ticket)
        .chain(candidate.sources.iter())
        .next()
        .expect("a candidate has a source")
}

fn record(
    conn: &Connection,
    candidate: &Candidate,
    decision: Decision,
    events: &BTreeMap<String, Vec<PathEvent>>,
) -> Result<WorktreeOutcome> {
    let source = event_source(candidate);
    let (ticket, pr) = item_keys(conn, &source.repo, source.kind, source.number)?;
    let recorded = candidate_branch(candidate);
    let now = now_rfc3339();
    let (kind, reason, removed, payload) = match decision {
        Decision::Removed {
            reason,
            branch,
            git_common_dir,
        } => (
            REMOVED,
            reason.clone(),
            true,
            json!({"path": candidate.path, "branch": branch.as_deref().or(recorded),
                "reason": reason, "git_common_dir": git_common_dir}),
        ),
        Decision::Left { reason, retryable } => {
            let mut payload = json!({"path": candidate.path, "branch": recorded, "reason": reason});
            if retryable {
                payload["retryable"] = json!(true);
            }
            (LEFT, reason, false, payload)
        }
    };
    let retryable = payload["retryable"].as_bool() == Some(true);
    let history = events.get(&candidate.path);
    let skip = match kind {
        // A path already removed after the retire: nothing more to say.
        LEFT if reason == ABSENT => history.is_some_and(|events| {
            events
                .iter()
                .any(|event| event.kind == REMOVED && after_retire(event, candidate))
        }),
        // A retry or recheck writes only when the reason changes.
        LEFT => history
            .and_then(|events| events.last())
            .filter(|last| after_retire(last, candidate))
            .is_some_and(|last| {
                last.kind == LEFT && last.reason == reason && last.retryable == retryable
            }),
        _ => false,
    };
    if !skip {
        write_event(
            conn,
            kind,
            Some(&source.session_id),
            Some(&source.repo),
            ticket,
            pr,
            payload,
            &now,
        )?;
    }
    Ok(WorktreeOutcome {
        path: candidate.path.clone(),
        removed,
        reason,
    })
}

/// A worktree of a retired agent that sm has not deleted, as the leftover
/// worktrees page lists it (sm#1987).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Leftover {
    pub path: String,
    pub repo: String,
    pub ticket: Option<i64>,
    pub pr: Option<i64>,
    /// The retired sessions that worked in it, by name.
    pub sessions: Vec<String>,
    /// Why sm keeps it: the latest check's reason, or `not checked yet`.
    pub reason: String,
    /// The keep's reason, when one holds it.
    pub kept: Option<String>,
    pub retired_at: Option<String>,
    /// When the reason was recorded.
    pub checked_at: Option<String>,
}

/// Every candidate still pending whose folder exists.
pub fn leftover_worktrees(
    store: &WorkClaimStore,
    sessions: &[CleanupSession],
) -> Result<Vec<Leftover>> {
    let Some(conn) = store.open_existing()? else {
        return Ok(Vec::new());
    };
    let request = CleanupRequest {
        sessions,
        ..CleanupRequest::default()
    };
    let retired = retired_sessions(&conn, &request)?;
    let events = path_events(&conn)?;
    let keeps = load_keeps(&conn)?;
    let names = sessions
        .iter()
        .map(|session| (session.id.as_str(), session.name.as_str()))
        .collect::<BTreeMap<_, _>>();
    let format = |at: Option<OffsetDateTime>| at.and_then(|at| at.format(&Rfc3339).ok());
    let mut leftovers = Vec::new();
    for candidate in gather(&conn, &request, &retired)?.into_values() {
        if !is_pending(&candidate, &events) || !Path::new(&candidate.path).exists() {
            continue;
        }
        let source = event_source(&candidate);
        let (ticket, pr) = item_keys(&conn, &source.repo, source.kind, source.number)?;
        let latest = latest_after_retire(&candidate, &events);
        let kept = keeps.get(&candidate.path).cloned();
        leftovers.push(Leftover {
            path: candidate.path.clone(),
            repo: source.repo.clone(),
            ticket,
            pr,
            sessions: candidate
                .sessions
                .iter()
                .map(|id| names.get(id.as_str()).copied().unwrap_or(id).to_owned())
                .collect(),
            reason: match (&kept, latest) {
                (Some(reason), _) => format!("kept: {reason}"),
                (None, Some(event)) => event.reason.clone(),
                (None, None) => "not checked yet".to_owned(),
            },
            kept,
            retired_at: format(candidate.retired_at),
            checked_at: format(latest.and_then(|event| event.at)),
        });
    }
    Ok(leftovers)
}

/// What a leftover's owner action deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteScope {
    /// Only ignored build output (`BUILD_DIRS`): always safe.
    BuildOutput,
    /// The whole worktree, uncommitted changes and all. Its branch stays, so
    /// commits on it are not lost.
    Worktree,
}

/// What a delete did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeleteOutcome {
    pub path: String,
    pub removed: bool,
    /// Build-output directories deleted.
    pub cleared: Vec<String>,
}

/// The leftover page's Delete and Delete build output (sm#1987). Only a
/// path `leftover_worktrees` lists, and never while a live session or
/// process is inside. `Ok(Err)` is why it was refused.
pub fn delete_leftover(
    store: &WorkClaimStore,
    sessions: &[CleanupSession],
    path: &str,
    scope: DeleteScope,
    actor: &str,
) -> Result<std::result::Result<DeleteOutcome, String>> {
    let _guard = cleanup_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(conn) = store.open_existing()? else {
        return Ok(Err("no worktree records".to_owned()));
    };
    let key = path_key(path);
    let request = CleanupRequest {
        sessions,
        ..CleanupRequest::default()
    };
    let retired = retired_sessions(&conn, &request)?;
    let events = path_events(&conn)?;
    let Some(candidate) = gather(&conn, &request, &retired)?
        .remove(&key)
        .filter(|candidate| is_pending(candidate, &events) && Path::new(&key).exists())
    else {
        return Ok(Err(format!("{key} is not a left-over worktree")));
    };
    let mut processes = None;
    if let Some(reason) = in_use(&conn, &candidate, &request, &retired, &mut processes, false)? {
        return Ok(Err(reason));
    }
    let dir = Path::new(&key);
    let linked = linked_worktree(dir, &key);
    let cleared = |dirs: Vec<PathBuf>| {
        dirs.iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
    };
    if scope == DeleteScope::BuildOutput {
        let result = match &linked {
            Ok(_) => clear_build_output(dir),
            // A folder git no longer knows: its build output by name.
            Err(NOT_A_WORKTREE) => clear_named_build_dirs(dir),
            Err(reason) => Err(format!("refused: {reason}")),
        };
        return Ok(result.map(|dirs| DeleteOutcome {
            path: key.clone(),
            removed: false,
            cleared: cleared(dirs),
        }));
    }
    let branch = git(dir, &["branch", "--show-current"]).filter(|b| !b.is_empty());
    let (dirs, common_dir) = match &linked {
        Ok(common_dir) => {
            let dirs = build_dirs(dir);
            if let Err(error) = remove_worktree(dir, common_dir, true) {
                return Ok(Err(format!("git refused: {error}")));
            }
            (dirs, Some(common_dir.clone()))
        }
        Err(NOT_A_WORKTREE) => {
            if let Err(error) = fs::remove_dir_all(dir) {
                return Ok(Err(format!("deleting {key}: {error}")));
            }
            (Vec::new(), None)
        }
        Err(reason) => return Ok(Err(format!("refused: {reason}"))),
    };
    conn.execute("DELETE FROM worktree_keeps WHERE path = ?1", params![key])?;
    let source = event_source(&candidate);
    let (ticket, pr) = item_keys(&conn, &source.repo, source.kind, source.number)?;
    let mut payload = json!({"path": key, "branch": branch.as_deref().or(candidate_branch(&candidate)),
        "reason": format!("deleted by {actor}")});
    if let Some(common_dir) = common_dir {
        payload["git_common_dir"] = json!(common_dir);
    }
    write_event(
        &conn,
        REMOVED,
        Some(&source.session_id),
        Some(&source.repo),
        ticket,
        pr,
        payload,
        &now_rfc3339(),
    )?;
    Ok(Ok(DeleteOutcome {
        path: key,
        removed: true,
        cleared: cleared(dirs),
    }))
}

/// `BUILD_DIRS` children of a folder that is not a git worktree.
fn clear_named_build_dirs(dir: &Path) -> std::result::Result<Vec<PathBuf>, String> {
    let dirs = BUILD_DIRS
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    for path in &dirs {
        fs::remove_dir_all(path)
            .map_err(|error| format!("deleting {}: {error}", path.display()))?;
    }
    Ok(dirs)
}

/// Build-output directories under a leftover, for sizing: by git's ignore
/// rules in a worktree, by name in a folder git no longer knows.
pub fn leftover_build_dirs(path: &str) -> Vec<PathBuf> {
    let dir = Path::new(path);
    match linked_worktree(dir, &path_key(path)) {
        Ok(_) => build_dirs(dir),
        Err(_) => BUILD_DIRS
            .iter()
            .map(|name| dir.join(name))
            .filter(|path| path.is_dir())
            .collect(),
    }
}

/// `(repo, PR)` for every keep whose reason names a PR, the repo read from
/// the kept checkout's origin: what to refresh before a pass so a merged
/// PR expires its keep.
pub fn keep_pr_refs(store: &WorkClaimStore) -> Result<Vec<(String, i64)>> {
    let mut refs = Vec::new();
    for (path, reason) in store.worktree_keeps()? {
        let numbers = keep_prs(&reason);
        if numbers.is_empty() {
            continue;
        }
        let Some(repo) = crate::work_attribution::git_origin_github_repo(&path) else {
            continue;
        };
        let repo = super::canonical_repo(&repo);
        refs.extend(numbers.into_iter().map(|number| (repo.clone(), number)));
    }
    refs.sort();
    refs.dedup();
    Ok(refs)
}

/// What a restore did about the session's working directory (sm#1839,
/// spec 1821 E3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeRebuild {
    /// It exists; nothing to do.
    Present,
    /// Rebuilt at the same path on its branch.
    Rebuilt { branch: String },
    /// The branch is gone (merged and deleted): rebuilt detached at `base`.
    Detached { branch: String, base: String },
}

/// Rebuilds `working_dir` when it is gone and a claim of the session
/// recorded it as `worktree_path` with a `branch`: `git worktree add` at the
/// same path, because Claude finds a conversation by its folder. A branch
/// that is not local is fetched from origin; one that is gone there too
/// gives a detached worktree at origin's default branch. The repository is
/// the one the removal recorded, else the first of `repo_dirs` whose origin
/// is the claim's repo. `Err` is the reason restore cannot go ahead.
pub fn rebuild_worktree(
    store: &WorkClaimStore,
    session_id: &str,
    working_dir: &str,
    repo_dirs: &[String],
) -> std::result::Result<WorktreeRebuild, String> {
    if Path::new(working_dir).exists() {
        return Ok(WorktreeRebuild::Present);
    }
    let _guard = cleanup_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = missing_path_key(working_dir);
    let gone = || format!("worktree {path} is gone and has no branch to rebuild from");
    let conn = store
        .open_existing()
        .map_err(|error| format!("{error:#}"))?
        .ok_or_else(gone)?;
    let claim = query_claims(
        &conn,
        "WHERE session_id = ?1 AND worktree_path IS NOT NULL AND branch IS NOT NULL \
         ORDER BY claimed_at DESC, id",
        params![session_id],
    )
    .map_err(|error| format!("{error:#}"))?
    .into_iter()
    .find(|claim| {
        claim
            .worktree_path
            .as_deref()
            .is_some_and(|recorded| missing_path_key(recorded) == path)
    })
    .ok_or_else(gone)?;
    // The removal recorded the branch actually checked out, which an agent
    // may have renamed since its claim (sm#1567), and the repository.
    let removed = removed_event(&conn, &path);
    let branch = removed
        .as_ref()
        .and_then(|payload| payload["branch"].as_str())
        .filter(|branch| !branch.is_empty())
        .map(str::to_owned)
        .or_else(|| claim.branch.clone())
        .unwrap_or_default();
    let git_dir = removed
        .as_ref()
        .and_then(|payload| payload["git_common_dir"].as_str())
        .filter(|dir| Path::new(dir).is_dir())
        .map(str::to_owned)
        .or_else(|| {
            repo_dirs.iter().find_map(|dir| {
                (crate::work_attribution::git_origin_github_repo(dir)
                    .is_some_and(|repo| super::canonical_repo(&repo) == claim.repo))
                .then(|| {
                    git(
                        Path::new(dir),
                        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                    )
                })
                .flatten()
            })
        })
        .ok_or_else(|| {
            format!(
                "worktree {path} is gone and no checkout of {} is known",
                claim.repo
            )
        })?;
    let git_dir = Path::new(&git_dir);
    // `Err((exit code, first stderr line))`.
    let run_status = |args: &[&str]| -> std::result::Result<(), (Option<i32>, String)> {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .output()
            .map_err(|error| (None, error.to_string()))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err((
            output.status.code(),
            stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("git failed")
                .trim_start_matches("fatal: ")
                .to_owned(),
        ))
    };
    let run = |args: &[&str]| run_status(args).map_err(|(_, line)| line);
    let failed = |error: String| format!("rebuilding worktree {path} failed: {error}");
    // A worktree deleted by hand is still registered until pruned.
    let _ = run(&["worktree", "prune"]);
    let local = run(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch}"),
    ])
    .is_ok();
    // Gone only when origin answers that it has no such branch (exit 2); an
    // unreachable origin fails the restore so it can be retried.
    let on_origin = local
        || match run_status(&[
            "ls-remote",
            "--exit-code",
            "--heads",
            "origin",
            &format!("refs/heads/{branch}"),
        ]) {
            Ok(()) => true,
            Err((Some(2), _)) => false,
            Err((_, error)) => return Err(failed(format!("cannot read origin: {error}"))),
        };
    let fetched = !local && on_origin && {
        run(&[
            "fetch",
            "--quiet",
            "origin",
            &format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
        ])
        .map_err(failed)?;
        true
    };
    let outcome = if local {
        run(&["worktree", "add", &path, &branch]).map_err(failed)?;
        WorktreeRebuild::Rebuilt { branch }
    } else if fetched {
        run(&[
            "worktree",
            "add",
            "-b",
            &branch,
            &path,
            &format!("origin/{branch}"),
        ])
        .map_err(failed)?;
        WorktreeRebuild::Rebuilt { branch }
    } else {
        let _ = run(&["fetch", "--quiet", "origin"]);
        let base = Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|base| !base.is_empty())
            .unwrap_or_else(|| "origin/main".to_owned());
        run(&["worktree", "add", "--detach", &path, &base]).map_err(failed)?;
        WorktreeRebuild::Detached { branch, base }
    };
    let (ticket, pr) = item_keys(&conn, &claim.repo, claim.kind(), claim.number)
        .map_err(|error| format!("{error:#}"))?;
    let mut payload = json!({"path": path, "branch": branch_name(&outcome), "claim_id": claim.id});
    if let WorktreeRebuild::Detached { base, .. } = &outcome {
        payload["detached_at"] = json!(base);
    }
    if let Err(error) = write_event(
        &conn,
        REBUILT,
        Some(session_id),
        Some(&claim.repo),
        ticket,
        pr,
        payload,
        &now_rfc3339(),
    ) {
        eprintln!("recording the rebuild of {path} failed: {error:#}");
    }
    Ok(outcome)
}

/// `path_key` for a path that no longer exists: its parent's symlinks
/// resolved, so it matches the key recorded while it existed.
fn missing_path_key(path: &str) -> String {
    let trimmed = path.trim().trim_end_matches('/');
    let path = Path::new(trimmed);
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => fs::canonicalize(parent)
            .map(|parent| parent.join(name).display().to_string())
            .unwrap_or_else(|_| trimmed.to_owned()),
        _ => trimmed.to_owned(),
    }
}

/// The latest `worktree.removed` payload for `path`.
fn removed_event(conn: &Connection, path: &str) -> Option<Value> {
    let mut statement = conn
        .prepare("SELECT payload FROM events WHERE kind = ?1 ORDER BY id DESC")
        .ok()?;
    let found = statement
        .query_map([REMOVED], |row| row.get::<_, Option<String>>(0))
        .ok()?
        .flatten()
        .flatten()
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .find(|payload| payload["path"].as_str() == Some(path));
    found
}

fn branch_name(outcome: &WorktreeRebuild) -> Option<&str> {
    match outcome {
        WorktreeRebuild::Present => None,
        WorktreeRebuild::Rebuilt { branch } | WorktreeRebuild::Detached { branch, .. } => {
            Some(branch)
        }
    }
}

/// `--path` for `sm worktree keep`, as stored: absolute, symlinks resolved.
pub fn keep_path_key(path: &str) -> Option<String> {
    let path = PathBuf::from(path.trim());
    path.is_absolute()
        .then(|| path_key(&path.display().to_string()))
}

#[cfg(test)]
mod tests;
