//! Worktree lifecycle HTTP surface (sm#1452, ticket #1487):
//! `POST /claims/worktree`, `POST /worktrees/keep`, and deletion at retire.

use std::sync::Mutex as StdMutex;

use super::claims::work_claim_store;
use super::*;
use crate::work_claims::{
    worktrees::{
        keep_path_key, run_worktree_cleanup, CleanupProgress, CleanupRequest, CleanupSession,
        WorktreeOutcome,
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
    requester_session_id: String,
    path: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    off: bool,
}

/// `sm worktree keep`: any managed session may set or clear a keep.
pub(super) async fn post_worktree_keep(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<PostWorktreeKeepRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), "/worktrees/keep")?;
    ensure_core_writes_enabled(&state)?;
    let session = managed_requester(&state, &payload.requester_session_id)?;
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
    store.set_worktree_keep(&path, &session.id, &reason)?;
    Ok(Json(
        json!({ "path": path, "kept": true, "reason": reason }),
    ))
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
        refresh_open_prs(&task_state, &task_session);
        let sessions = cleanup_sessions(&task_state)?;
        run_worktree_cleanup(
            &work_claim_store(&task_state),
            CleanupRequest {
                sessions: &sessions,
                first: Some(&task_session),
                progress: Some(&task_progress),
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
