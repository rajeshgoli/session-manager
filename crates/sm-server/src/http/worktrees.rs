//! Worktree lifecycle HTTP surface (sm#1452, ticket #1487):
//! `POST /claims/worktree`, `POST /worktrees/keep`, deletion at retire, and
//! the leftover worktrees page's `GET /worktrees/leftover` and
//! `POST /worktrees/delete` (sm#1987).

use std::{
    collections::BTreeMap as StdBTreeMap,
    sync::{Mutex as StdMutex, OnceLock},
    time::Instant,
};

use super::claims::work_claim_store;
use super::*;
use crate::work_claims::{
    worktrees::{
        delete_leftover, keep_path_key, keep_pr_refs, leftover_build_dirs, leftover_worktrees,
        run_worktree_cleanup, CleanupProgress, CleanupRequest, CleanupSession, DeleteScope,
        WorktreeOutcome, RECHECK_INTERVAL,
    },
    WorkKind, MAX_ALIASES_PER_QUERY,
};

/// How long `sm retire` waits for its worktree results; the rest report
/// `cleanup pending` and finish in the background.
const RETIRE_CLEANUP_WAIT: Duration = Duration::from_secs(10);
const CLEANUP_PENDING: &str = "cleanup pending";

fn bad_request(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: detail.into(),
    }
}

fn not_found(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::NOT_FOUND,
        detail: detail.into(),
    }
}

/// The requesting session must exist and not be retired.
fn managed_requester(state: &AppState, session_id: &str) -> Result<SessionRecord, ApiError> {
    let session_id = session_id.trim();
    match state.session_store.get_session(session_id)? {
        Some(session)
            if !matches!(
                session.completion_status.as_deref(),
                Some("retired" | "killed")
            ) =>
        {
            Ok(session)
        }
        Some(_) => Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: format!("Session {session_id} is retired"),
        }),
        None => Err(bad_request(format!("Session {session_id} not found"))),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct PostClaimWorktreeRequest {
    requester_session_id: String,
    claim_id: String,
    state: String,
    worktree_path: String,
    branch: String,
    #[serde(default)]
    base_sha: Option<String>,
}

/// `sm ticket --setup-worktree`: the intent before any git write, then the
/// confirmation after `git worktree add`. Both set the same fields, so either
/// can be repeated.
pub(super) async fn post_claim_worktree(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<PostClaimWorktreeRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        "/claims/worktree",
    )?;
    ensure_core_writes_enabled(&state)?;
    if !matches!(payload.state.trim(), "intent" | "created") {
        return Err(bad_request("state must be intent or created"));
    }
    let path = payload.worktree_path.trim();
    if !std::path::Path::new(path).is_absolute() {
        return Err(bad_request("worktree_path must be absolute"));
    }
    let branch = payload.branch.trim();
    if branch.is_empty() {
        return Err(bad_request("branch is required"));
    }
    let session = managed_requester(&state, &payload.requester_session_id)?;
    let base_sha = trimmed(&payload.base_sha);
    let store = work_claim_store(&state);
    let Some(claim) = store.set_claim_worktree(
        &session.id,
        payload.claim_id.trim(),
        path,
        branch,
        base_sha.as_deref(),
    )?
    else {
        return Err(not_found(format!(
            "You hold no active claim {}.",
            payload.claim_id.trim()
        )));
    };
    Ok(Json(json!({ "claim": claim })))
}

#[derive(Debug, Deserialize)]
pub(super) struct PostWorktreeKeepRequest {
    #[serde(default)]
    requester_session_id: String,
    path: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    off: bool,
}

/// `sm worktree keep`: any managed session may set or clear a keep, and so
/// may the owner from the leftover worktrees page.
pub(super) async fn post_worktree_keep(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<PostWorktreeKeepRequest>,
) -> Result<Json<Value>, ApiError> {
    let actor = requester(
        &state,
        &headers,
        peer_addr,
        &payload.requester_session_id,
        "/worktrees/keep",
    )?;
    let Some(path) = keep_path_key(&payload.path) else {
        return Err(bad_request("path must be absolute"));
    };
    let store = work_claim_store(&state);
    if payload.off {
        let existed = store.clear_worktree_keep(&path)?;
        return Ok(Json(
            json!({ "path": path, "kept": false, "existed": existed }),
        ));
    }
    let Some(reason) = trimmed(&payload.reason) else {
        return Err(bad_request("--reason is required"));
    };
    store.set_worktree_keep(&path, &actor, &reason)?;
    Ok(Json(
        json!({ "path": path, "kept": true, "reason": reason }),
    ))
}

/// Who acts on a worktree: the owner in the browser (`owner`), else the
/// managed session named by `requester_session_id`. A request with no
/// session id passes only from the owner's own machine or SM login, as the
/// `sm` CLI run by hand does (`owner`).
fn requester(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    session_id: &str,
    route: &str,
) -> Result<String, ApiError> {
    let web_owner = owner_web_guard(state, headers, Some(peer_addr), "POST")?.is_some();
    if !web_owner {
        ensure_session_allowed_from_parts(&state.config, headers, Some(peer_addr), route)?;
    }
    ensure_core_writes_enabled(state)?;
    if session_id.trim().is_empty() {
        return Ok("owner".to_owned());
    }
    Ok(managed_requester(state, session_id)?.id)
}

#[derive(Debug, Clone, Copy)]
struct Sizes {
    bytes: u64,
    build_bytes: u64,
}

/// Sizes go stale after this; measuring a 20 GB `target/` takes seconds.
const SIZE_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

fn size_cache() -> &'static StdMutex<StdBTreeMap<String, (Instant, Sizes)>> {
    static CACHE: OnceLock<StdMutex<StdBTreeMap<String, (Instant, Sizes)>>> = OnceLock::new();
    CACHE.get_or_init(|| StdMutex::new(StdBTreeMap::new()))
}

/// `du -sk` of each path, in bytes; 0 for one it cannot read.
fn disk_usage(paths: &[std::path::PathBuf]) -> u64 {
    if paths.is_empty() {
        return 0;
    }
    let Ok(output) = std::process::Command::new("du")
        .arg("-sk")
        .args(paths)
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next()?.parse::<u64>().ok())
        .sum::<u64>()
        * 1024
}

/// Each path's size and build-output size, measured in parallel and cached
/// for `SIZE_TTL`.
fn leftover_sizes(paths: &[String]) -> StdBTreeMap<String, Sizes> {
    let cached = size_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let mut sizes = StdBTreeMap::new();
    let mut handles = Vec::new();
    for path in paths {
        match cached.get(path) {
            Some((at, size)) if at.elapsed() < SIZE_TTL => {
                sizes.insert(path.clone(), *size);
            }
            _ => {
                let path = path.clone();
                handles.push(std::thread::spawn(move || {
                    let size = Sizes {
                        bytes: disk_usage(&[std::path::PathBuf::from(&path)]),
                        build_bytes: disk_usage(&leftover_build_dirs(&path)),
                    };
                    (path, size)
                }));
            }
        }
    }
    let mut cache = size_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for handle in handles {
        if let Ok((path, size)) = handle.join() {
            cache.insert(path.clone(), (Instant::now(), size));
            sizes.insert(path, size);
        }
    }
    sizes
}

/// `GET /worktrees/leftover`: every retired agent's worktree sm has not
/// deleted, with why, its size and how much of that is build output.
pub(super) async fn get_leftover_worktrees(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    if owner_web_guard(&state, &headers, Some(peer_addr), "GET")?.is_none() {
        ensure_session_allowed_from_parts(
            &state.config,
            &headers,
            Some(peer_addr),
            "/worktrees/leftover",
        )?;
    }
    if !expand_home(&state.config.sm_send.db_path).exists() {
        return Ok(Json(json!({ "worktrees": [] })));
    }
    let task_state = state.clone();
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let sessions = cleanup_sessions(&task_state)?;
        let leftovers = leftover_worktrees(&work_claim_store(&task_state), &sessions)?;
        let paths = leftovers
            .iter()
            .map(|leftover| leftover.path.clone())
            .collect::<Vec<_>>();
        let sizes = leftover_sizes(&paths);
        Ok(leftovers
            .into_iter()
            .map(|leftover| {
                let size = sizes.get(&leftover.path).copied();
                let mut row = serde_json::to_value(&leftover).unwrap_or(Value::Null);
                row["bytes"] = json!(size.map(|size| size.bytes));
                row["build_bytes"] = json!(size.map(|size| size.build_bytes));
                row
            })
            .collect())
    })
    .await
    .map_err(|error| anyhow::anyhow!("leftover worktrees task failed: {error}"))??;
    Ok(Json(json!({ "worktrees": rows })))
}

#[derive(Debug, Deserialize)]
pub(super) struct PostWorktreeDeleteRequest {
    #[serde(default)]
    requester_session_id: String,
    path: String,
    /// `worktree` (the default) or `build` for build output only.
    #[serde(default)]
    scope: Option<String>,
}

/// `POST /worktrees/delete`: the leftover page's Delete and Delete build
/// output, and `sm worktree delete`.
pub(super) async fn post_worktree_delete(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<PostWorktreeDeleteRequest>,
) -> Result<Json<Value>, ApiError> {
    let actor = requester(
        &state,
        &headers,
        peer_addr,
        &payload.requester_session_id,
        "/worktrees/delete",
    )?;
    let scope = match payload.scope.as_deref().map(str::trim) {
        None | Some("") | Some("worktree") => DeleteScope::Worktree,
        Some("build") => DeleteScope::BuildOutput,
        Some(other) => return Err(bad_request(format!("unknown scope {other}"))),
    };
    let Some(path) = keep_path_key(&payload.path) else {
        return Err(bad_request("path must be absolute"));
    };
    let task_state = state.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let sessions = cleanup_sessions(&task_state)?;
        delete_leftover(
            &work_claim_store(&task_state),
            &sessions,
            &path,
            scope,
            &actor,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("worktree delete task failed: {error}"))??;
    match outcome {
        Ok(outcome) => {
            size_cache()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&outcome.path);
            Ok(Json(serde_json::to_value(outcome)?))
        }
        Err(reason) => Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: reason,
        }),
    }
}

fn cleanup_session(record: &SessionRecord) -> CleanupSession {
    CleanupSession {
        id: record.id.clone(),
        name: record
            .friendly_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| record.name.clone()),
        working_dir: record.working_dir.clone(),
        retired: matches!(
            record.completion_status.as_deref(),
            Some("retired" | "killed")
        ),
        stopped: record.is_stopped(),
        retired_at: record
            .completed_at
            .clone()
            .or_else(|| record.stopped_at.clone()),
        local: crate::sessions::is_primary_node(&record.node),
    }
}

fn cleanup_sessions(state: &AppState) -> anyhow::Result<Vec<CleanupSession>> {
    Ok(state
        .session_store
        .list_sessions(true)?
        .iter()
        .map(cleanup_session)
        .collect())
}

/// A deletion pass over every pending candidate: server start and after
/// every sync pass.
pub(super) fn run_cleanup_pass(state: &AppState) -> anyhow::Result<()> {
    refresh_keep_prs(state);
    let sessions = cleanup_sessions(state)?;
    run_worktree_cleanup(
        &work_claim_store(state),
        CleanupRequest {
            sessions: &sessions,
            ..CleanupRequest::default()
        },
    )?;
    Ok(())
}

/// PRs named by keeps that sm has as open or does not know, fetched at most
/// every `RECHECK_INTERVAL`, so a keep naming a PR expires once it merges.
fn refresh_keep_prs(state: &AppState) {
    static LAST: StdMutex<Option<Instant>> = StdMutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.is_some_and(|at| at.elapsed() < RECHECK_INTERVAL) {
            return;
        }
        *last = Some(Instant::now());
    }
    let store = work_claim_store(state);
    let Ok(refs) = keep_pr_refs(&store) else {
        return;
    };
    let mut by_repo = BTreeMap::<String, Vec<i64>>::new();
    for (repo, number) in refs {
        let open = store
            .item(&repo, number)
            .ok()
            .flatten()
            .is_none_or(|item| item.state == "open");
        if open {
            by_repo.entry(repo).or_default().push(number);
        }
    }
    for (repo, numbers) in by_repo {
        for chunk in numbers.chunks(MAX_ALIASES_PER_QUERY) {
            let fetched = state.work_item_source.fetch(&repo, chunk);
            if let Err(error) = store.record_fetch(&repo, chunk, &fetched) {
                eprintln!("refreshing {repo} PRs named by worktree keeps failed: {error:#}");
            }
        }
    }
}

/// The retired session's PRs that sm still has as open: fetched fresh, so a
/// PR merged minutes before the retire counts as merged when the pass
/// checks the worktree's HEAD against it. Claims ended by the retire leave
/// the sync's tracked set, so nothing else would refresh them.
fn refresh_open_prs(state: &AppState, session_id: &str) {
    let store = work_claim_store(state);
    let Ok(claims) = store.claims_for_session(session_id, false) else {
        return;
    };
    let mut by_repo = BTreeMap::<String, Vec<i64>>::new();
    for view in claims {
        if view.claim.kind() == WorkKind::Pr && view.state == "open" {
            by_repo
                .entry(view.claim.repo.clone())
                .or_default()
                .push(view.claim.number);
        }
    }
    for (repo, mut numbers) in by_repo {
        numbers.sort_unstable();
        numbers.dedup();
        for chunk in numbers.chunks(MAX_ALIASES_PER_QUERY) {
            let fetched = state.work_item_source.fetch(&repo, chunk);
            if let Err(error) = store.record_fetch(&repo, chunk, &fetched) {
                eprintln!("refreshing {repo} PRs before worktree cleanup failed: {error:#}");
            }
        }
    }
}

/// Right after a retire: the session's candidates first. Returns their
/// results available within `RETIRE_CLEANUP_WAIT`; the rest say
/// `cleanup pending` while the pass finishes in the background.
pub(super) async fn cleanup_after_retire(
    state: &Arc<AppState>,
    session_id: &str,
) -> Vec<WorktreeOutcome> {
    if !expand_home(&state.config.sm_send.db_path).exists() {
        return Vec::new();
    }
    let progress = Arc::new(StdMutex::new(CleanupProgress::default()));
    let task_state = state.clone();
    let task_progress = progress.clone();
    let task_session = session_id.to_owned();
    let task = tokio::task::spawn_blocking(move || {
        if let Err(error) = super::ask::cleanup_reader_worktree(&task_state, &task_session) {
            eprintln!("reader worktree cleanup after retiring {task_session} failed: {error:#}");
        }
        refresh_open_prs(&task_state, &task_session);
        let sessions = cleanup_sessions(&task_state)?;
        run_worktree_cleanup(
            &work_claim_store(&task_state),
            CleanupRequest {
                sessions: &sessions,
                first: Some(&task_session),
                progress: Some(&task_progress),
                recheck_after: None,
            },
        )
    });
    match tokio::time::timeout(RETIRE_CLEANUP_WAIT, task).await {
        Ok(Ok(Err(error))) => {
            eprintln!("worktree cleanup after retiring {session_id} failed: {error:#}")
        }
        Ok(Err(error)) => {
            eprintln!("worktree cleanup task after retiring {session_id} failed: {error}")
        }
        _ => {}
    }
    let progress = progress
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(paths) = progress.first_paths.as_ref() else {
        return Vec::new();
    };
    paths
        .iter()
        .map(|path| {
            progress
                .outcomes
                .iter()
                .find(|outcome| outcome.path == *path)
                .cloned()
                .unwrap_or_else(|| WorktreeOutcome {
                    path: path.clone(),
                    removed: false,
                    reason: CLEANUP_PENDING.to_owned(),
                })
        })
        .collect()
}

/// `worktree_naming` in a ticket claim's response: where
/// `sm ticket --setup-worktree` puts the worktree.
pub(super) fn worktree_naming(config: &AppConfig, repo: &str) -> Value {
    json!({
        "root": config.work_claims.worktree_root_path().display().to_string(),
        "prefix": config.work_claims.worktree_prefix(repo),
    })
}
