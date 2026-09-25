//! Worktree lifecycle (sm#1452, ticket #1487): the managed worktree on a
//! claim, `sm worktree keep`, and deletion at retire (appendix G).
//!
//! Deletion is durable by derivation. A retired session's candidates are
//! every `worktree_path` on its claims, plus its working directory when that
//! is on the branch of a merged PR it claimed. A candidate stays pending
//! until a `worktree.removed` event, or a `worktree.left` event with a final
//! reason, is written for its path after the session retired. Every pass
//! processes every pending candidate, so a crash only delays the work.

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
use rusqlite::{params, Connection};
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

const REMOVED: &str = "worktree.removed";
const LEFT: &str = "worktree.left";
const ABSENT: &str = "absent";

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
    Removed { reason: String },
    Left { reason: String, retryable: bool },
}

fn cleanup_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
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
    let mut candidates = gather(&conn, &request, &retired)?
        .into_values()
        .filter(|candidate| is_pending(candidate, &events))
        .collect::<Vec<_>>();
    let first = request.first.unwrap_or_default();
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
    let mut processes = None;
    let mut outcomes = Vec::new();
    for candidate in &candidates {
        let grace = candidate.sessions.contains(first);
        let decision = decide(
            &conn,
            candidate,
            &request,
            &retired,
            &keeps,
            &mut processes,
            grace,
        )?;
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
fn path_key(path: &str) -> String {
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

/// Pending until a removal, or a final `worktree.left`, is recorded for the
/// path after the latest of its sessions retired.
fn is_pending(candidate: &Candidate, events: &BTreeMap<String, Vec<PathEvent>>) -> bool {
    !events.get(&candidate.path).is_some_and(|events| {
        events.iter().any(|event| {
            after_retire(event, candidate) && (event.kind == REMOVED || !event.retryable)
        })
    })
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

fn decide(
    conn: &Connection,
    candidate: &Candidate,
    request: &CleanupRequest<'_>,
    retired: &BTreeMap<String, Option<OffsetDateTime>>,
    keeps: &BTreeMap<String, String>,
    processes: &mut Option<ProcessListing>,
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
        return final_left("not a linked worktree".to_owned());
    };
    if git_dir == common_dir || *top != candidate.path {
        return final_left("not a linked worktree".to_owned());
    }
    // 2. No keep.
    if let Some(reason) = keeps.get(&candidate.path) {
        return retry_left(format!("kept: {reason}"));
    }
    // 3. No other session that is not retired works inside it.
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
            return retry_left(format!("in use by {}", session.name));
        }
    }
    // 4. No process has its cwd inside. The retired agent's own processes
    // may take a moment to exit after its panes close.
    let deadline = Instant::now()
        + if grace {
            RETIRE_PROCESS_GRACE
        } else {
            Duration::ZERO
        };
    loop {
        let listing = match processes.get_or_insert_with(list_process_cwds) {
            Ok(listing) => listing,
            Err(error) => return retry_left(format!("process check failed: {error}")),
        };
        let Some((pid, command)) = processes_inside(&candidate.path, listing) else {
            break;
        };
        if Instant::now() >= deadline {
            return retry_left(format!("process {pid} ({command}) runs in it"));
        }
        thread::sleep(Duration::from_millis(500));
        *processes = None;
    }
    // 5. Nothing committed would be lost.
    let Some(head) = git(path, &["rev-parse", "HEAD"]) else {
        return final_left("commits not in a merged PR".to_owned());
    };
    let Some(removed_reason) = safe_head(conn, candidate, &head)? else {
        return final_left("commits not in a merged PR".to_owned());
    };
    // A keep acknowledged while this pass was running wins: re-read it right
    // before the removal instead of trusting the pass's first snapshot.
    if let Some(reason) = load_keeps(conn)?.get(&candidate.path) {
        return retry_left(format!("kept: {reason}"));
    }
    // 6. git removes it, refusing modified or untracked files.
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(common_dir)
        .args(["worktree", "remove"])
        .arg(&candidate.path)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let line = stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("git worktree remove failed");
            let line = line.strip_prefix("fatal: ").unwrap_or(line);
            return final_left(format!("git refused: {line}"));
        }
        Err(error) => return final_left(format!("git refused: {error}")),
    }
    // Squash merges never make the branch an ancestor of main: -D, but only
    // when its tip is still the HEAD that was checked.
    if let Some(branch) = candidate_branch(candidate) {
        let tip = Command::new("git")
            .arg("--git-dir")
            .arg(common_dir)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}"))
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        if tip.as_deref() == Some(head.as_str()) {
            let _ = Command::new("git")
                .arg("--git-dir")
                .arg(common_dir)
                .args(["branch", "-D", branch])
                .output();
        }
    }
    Ok(Decision::Removed {
        reason: removed_reason,
    })
}

/// Condition 5: HEAD is the frozen head of a merged PR one of the
/// candidate's sessions claimed in its repo, or a managed worktree's base
/// (nothing was ever committed). The removal reason, or `None`.
fn safe_head(conn: &Connection, candidate: &Candidate, head: &str) -> Result<Option<String>> {
    let repos = candidate
        .sources
        .iter()
        .map(|source| source.repo.as_str())
        .collect::<BTreeSet<_>>();
    for session in &candidate.sessions {
        let claims = query_claims(
            conn,
            "WHERE session_id = ?1 AND kind = 'pr' AND reserved_at IS NULL ORDER BY claimed_at",
            params![session],
        )?;
        for claim in claims.iter().filter(|c| repos.contains(c.repo.as_str())) {
            let Some(item) = get_item(conn, &claim.repo, claim.number)? else {
                continue;
            };
            if item.state == "merged" && item.head_sha.as_deref() == Some(head) {
                return Ok(Some(format!("PR #{} merged", claim.number)));
            }
        }
    }
    if candidate
        .sources
        .iter()
        .any(|source| source.managed_base.as_deref() == Some(head))
    {
        return Ok(Some("no commits".to_owned()));
    }
    Ok(None)
}

/// The branch recorded for the path: a managed claim's first, else any.
fn candidate_branch(candidate: &Candidate) -> Option<&str> {
    candidate
        .sources
        .iter()
        .filter(|source| source.managed_base.is_some())
        .chain(candidate.sources.iter())
        .find_map(|source| source.branch.as_deref().filter(|b| !b.is_empty()))
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
    let branch = candidate_branch(candidate);
    let now = now_rfc3339();
    let (kind, reason, removed, payload) = match decision {
        Decision::Removed { reason } => (
            REMOVED,
            reason.clone(),
            true,
            json!({"path": candidate.path, "branch": branch, "reason": reason}),
        ),
        Decision::Left { reason, retryable } => {
            let mut payload = json!({"path": candidate.path, "branch": branch, "reason": reason});
            if retryable {
                payload["retryable"] = json!(true);
            }
            (LEFT, reason, false, payload)
        }
    };
    let history = events.get(&candidate.path);
    let skip = match kind {
        // A path already removed after the retire: nothing more to say.
        LEFT if reason == ABSENT => history.is_some_and(|events| {
            events
                .iter()
                .any(|event| event.kind == REMOVED && after_retire(event, candidate))
        }),
        // A retry writes only when the reason changes.
        LEFT => history
            .and_then(|events| events.last())
            .is_some_and(|last| last.kind == LEFT && last.reason == reason && last.retryable),
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

/// `--path` for `sm worktree keep`, as stored: absolute, symlinks resolved.
pub fn keep_path_key(path: &str) -> Option<String> {
    let path = PathBuf::from(path.trim());
    path.is_absolute()
        .then(|| path_key(&path.display().to_string()))
}

#[cfg(test)]
mod tests;
