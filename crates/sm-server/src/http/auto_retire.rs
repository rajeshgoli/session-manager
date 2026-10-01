//! Auto-retire and restore on message (sm#1839; spec
//! `docs/working/1821_fit_and_finish_2.html`, appendix E).
//!
//! Every minute the sweep retires agents sm started that have been finished
//! and idle for the owner's delay (60 minutes by default) with nothing
//! waiting on them. Anything addressed to such an agent afterwards restores
//! it: the restore rebuilds its worktree at the same path and reopens the
//! claims its retire ended.

use super::claims::work_claim_store;
use super::*;
use crate::work_claims::worktrees::{rebuild_worktree, WorktreeRebuild};

const SWEEP_EVERY: Duration = Duration::from_secs(60);
/// Providers History can restore; others are never auto-retired.
const RESTORABLE_PROVIDERS: [&str; 3] = ["claude", "codex", "codex-fork"];

/// The sweep, on the server that runs the runtime.
pub(super) fn spawn_auto_retire_sweeper(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; let startup recovery settle first.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(error) = sweep_auto_retire(state.clone(), OffsetDateTime::now_utc()).await {
                eprintln!("auto-retire sweep failed: {error:#}");
            }
        }
    });
}

/// One sweep as of `now`. Returns the sessions it retired.
pub async fn sweep_auto_retire(
    state: Arc<AppState>,
    now: OffsetDateTime,
) -> anyhow::Result<Vec<String>> {
    let work = state.clone();
    let due = tokio::task::spawn_blocking(move || due_sessions(&work, now))
        .await
        .map_err(|error| anyhow::anyhow!(error))??;
    let mut retired = Vec::new();
    for (session_id, idle_minutes) in due {
        let work = state.clone();
        let id = session_id.clone();
        let outcome = tokio::task::spawn_blocking(move || retire(&work, &id, idle_minutes))
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
        match outcome {
            Ok(CoreRetireOutcome::Retired(_)) => {
                if let Err(error) = teardown_btw_requests_for_session(&state, &session_id) {
                    eprintln!("auto-retired {session_id} but BTW teardown failed: {error:#}");
                }
                worktrees::cleanup_after_retire(&state, &session_id).await;
                retired.push(session_id);
            }
            // It worked, or something else changed it, since the check.
            Ok(_) => {}
            Err(error) => eprintln!("auto-retiring {session_id} failed: {error:#}"),
        }
    }
    if !retired.is_empty() {
        board::request_recompute(&state);
    }
    Ok(retired)
}

/// Retire, re-checking finished and idle under the store's lock.
fn retire(
    state: &AppState,
    session_id: &str,
    idle_minutes: u64,
) -> anyhow::Result<CoreRetireOutcome> {
    let authority = RetireAuthority::auto_retire(idle_minutes);
    let busy = |session: &SessionRecord| {
        live_activity_state(state, session).is_some_and(|activity| activity != "idle")
    };
    if state.config.rust_core.runtime_enabled {
        let runtime = TmuxRuntime::from_app_config(&state.config);
        state
            .session_store
            .retire_core_session_with_runtime_authorized_if_finished_idle(
                session_id, authority, None, &runtime, true, &busy,
            )
    } else {
        state
            .session_store
            .retire_core_session_authorized_if_finished_idle(
                session_id, authority, None, true, &busy,
            )
    }
}

/// What the eligibility check reads besides the session itself.
struct Context {
    spawned: BTreeSet<String>,
    roles: BTreeSet<String>,
    followed: BTreeSet<String>,
    parents_of_live: BTreeSet<String>,
    /// `sm watch` items by session id: `state` and `facts`.
    watch: BTreeMap<String, Value>,
}

/// Sessions to retire now, each with the delay that applied.
fn due_sessions(state: &AppState, now: OffsetDateTime) -> anyhow::Result<Vec<(String, u64)>> {
    let settings = state.session_store.owner_settings()?;
    let Some(minutes) = crate::owner_settings::auto_retire_minutes(&settings) else {
        return Ok(Vec::new());
    };
    let sessions = state.session_store.list_sessions(false)?;
    let watch = watch::watch_state(state, &watch::WatchParams::default())
        .map_err(|error| anyhow::anyhow!("watch state: {error:?}"))?;
    let context = Context {
        spawned: state.session_store.spawn_intent_session_ids()?,
        roles: state
            .session_store
            .list_agent_registrations()?
            .into_iter()
            .map(|registration| registration.session_id)
            .collect(),
        followed: crate::owner_push::OwnerPushStore::new(crate::owner_push::push_db_path(
            &state.config,
        ))
        .active()
        .unwrap_or_default()
        .into_iter()
        .filter(|follow| follow.target_kind == crate::owner_push::TARGET_SESSION)
        .map(|follow| follow.session_id)
        .collect(),
        parents_of_live: sessions
            .iter()
            .filter(|session| !session.is_stopped())
            .filter_map(|session| session.parent_session_id.clone())
            .collect(),
        watch: watch["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| {
                (
                    item["id"].as_str().unwrap_or_default().to_owned(),
                    item.clone(),
                )
            })
            .collect(),
    };
    Ok(sessions
        .iter()
        .filter(|session| why_not(session, &context, now, minutes).is_none())
        .map(|session| (session.id.clone(), minutes))
        .collect())
}

/// Why `session` does not auto-retire now, or `None` when it does
/// (spec 1821 E1).
fn why_not(
    session: &SessionRecord,
    context: &Context,
    now: OffsetDateTime,
    minutes: u64,
) -> Option<&'static str> {
    let started_by_sm = session.started_by_sm
        || session.predecessor_session_id.is_some()
        || context.spawned.contains(&session.id);
    if !started_by_sm {
        return Some("not started by sm");
    }
    if context.roles.contains(&session.id) {
        return Some("registered in a role");
    }
    if !is_primary_node(&session.node) {
        return Some("on another machine");
    }
    if !RESTORABLE_PROVIDERS.contains(&session.provider.as_str()) {
        return Some("provider cannot be restored");
    }
    if session.is_stopped() {
        return Some("stopped");
    }
    let Some(completed_at) = session.agent_task_completed_at.as_deref() else {
        return Some("not finished");
    };
    let Some(item) = context.watch.get(&session.id) else {
        return Some("not listed");
    };
    if item["state"] != "idle" {
        return Some("not idle");
    }
    let since = [completed_at, session.last_activity.as_str()]
        .into_iter()
        .filter_map(|at| OffsetDateTime::parse(at, &Rfc3339).ok())
        .max();
    let delay = time::Duration::minutes(i64::try_from(minutes).unwrap_or(i64::MAX));
    if since.is_none_or(|since| now - since < delay) {
        return Some("not idle long enough");
    }
    let facts = &item["facts"];
    match facts["you"]["kind"].as_str() {
        Some("message") => return Some("asked you a question"),
        Some("doc_review") => return Some("waits on your doc review"),
        Some(_) => return Some("waits on you"),
        None => {}
    }
    let jobs = &facts["jobs"];
    if jobs["running"].as_u64().unwrap_or(0) > 0 || jobs["waiting"].as_u64().unwrap_or(0) > 0 {
        return Some("has queue jobs");
    }
    if !jobs["review"].is_null() {
        return Some("waits on a review");
    }
    if context.parents_of_live.contains(&session.id) {
        return Some("has live child agents");
    }
    if context.followed.contains(&session.id) {
        return Some("followed");
    }
    None
}

/// Checkouts a rebuild may find a repository in: the owner's workspaces and
/// every session's working directory that exists.
fn repo_dirs(state: &AppState) -> Vec<String> {
    let mut dirs = Vec::new();
    if let Ok(settings) = state.session_store.owner_settings() {
        dirs.extend(
            settings["new_agent"]["workspaces"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
    }
    if let Ok(sessions) = state.session_store.list_sessions(true) {
        dirs.extend(
            sessions
                .into_iter()
                .map(|session| expand_home(&session.working_dir).display().to_string()),
        );
    }
    let mut seen = BTreeSet::new();
    dirs.retain(|dir| std::path::Path::new(dir).is_dir() && seen.insert(dir.clone()));
    dirs
}

/// Restore a stopped or retired session (sm#1839, spec 1821 E3): rebuild its
/// worktree when it is gone, relaunch it, reopen the claims its retire ended,
/// and tell it when its branch was gone. History's Restore, the retire
/// toast's Undo and a message to an auto-retired agent all come here.
pub(super) fn restore_session_with_work(
    state: &AppState,
    session_id: &str,
) -> Result<SessionRecord, ApiError> {
    let Some(before) = state.session_store.get_session(session_id)? else {
        return Err(ApiError::NotFound("Session not found"));
    };
    let runtime_enabled = state.config.rust_core.runtime_enabled;
    let mut note = None;
    if before.is_stopped() && is_primary_node(&before.node) {
        let working_dir = expand_home(&before.working_dir).display().to_string();
        match rebuild_worktree(
            &work_claim_store(state),
            &before.id,
            &working_dir,
            &repo_dirs(state),
        ) {
            Ok(WorktreeRebuild::Detached { branch, base }) => {
                note = Some(format!(
                    "[sm] Restored. Branch {branch} is gone (merged and deleted), so your \
                     worktree {working_dir} was rebuilt detached at {base}."
                ));
            }
            Ok(WorktreeRebuild::Present | WorktreeRebuild::Rebuilt { .. }) => {}
            // Only a relaunch needs the folder; the fixture store does not.
            Err(reason) if runtime_enabled => {
                return Err(ApiError::Status {
                    status: StatusCode::CONFLICT,
                    detail: reason,
                })
            }
            Err(_) => {}
        }
    }
    let outcome = if runtime_enabled {
        ensure_core_runtime_session_node_supported(state, session_id)?;
        let runtime = TmuxRuntime::from_app_config(&state.config);
        state
            .session_store
            .restore_core_session_with_runtime(session_id, &runtime)?
    } else {
        state.session_store.restore_core_session(session_id)?
    };
    let session = match outcome {
        None => return Err(ApiError::NotFound("Session not found")),
        Some(CoreRestoreOutcome::Restored(session)) => *session,
        Some(CoreRestoreOutcome::NotStopped) => {
            return Err(ApiError::Status {
                status: StatusCode::CONFLICT,
                detail: "Session is not stopped".to_owned(),
            })
        }
        Some(CoreRestoreOutcome::UnsupportedNode(node)) => {
            return Err(ApiError::Status {
                status: StatusCode::BAD_REQUEST,
                detail: format!("Rust runtime does not support remote node {node}"),
            })
        }
        Some(CoreRestoreOutcome::UnsupportedProvider(provider)) => {
            return Err(ApiError::Status {
                status: StatusCode::BAD_REQUEST,
                detail: format!("Rust runtime does not support provider {provider}"),
            })
        }
        Some(CoreRestoreOutcome::MissingProviderResumeId(provider)) => {
            return Err(ApiError::Status {
                status: StatusCode::CONFLICT,
                detail: format!("Cannot restore {provider} session without provider_resume_id"),
            })
        }
    };
    if let Some(retired_at) = before
        .completed_at
        .as_deref()
        .filter(|_| before.is_retired())
    {
        match work_claim_store(state).reopen_retired_claims(session_id, retired_at) {
            Ok(reopened) if !reopened.is_empty() => board::request_recompute(state),
            Ok(_) => {}
            Err(error) => eprintln!("reopening the claims of {session_id} failed: {error:#}"),
        }
    }
    if let Some(note) = note {
        if let Err(error) = RetainedQueueStore::new(expand_home(&state.config.sm_send.db_path))
            .enqueue_message(session_id, &note, "sequential", Some("restore"))
        {
            eprintln!("telling {session_id} its worktree is detached failed: {error:#}");
        }
    }
    Ok(session)
}

/// A message is about to be addressed to `recipient`: when that is an
/// auto-retired agent, restore it first. `None` when the restore failed,
/// and the caller falls back to the old route (successor, then parent).
pub(super) fn ready_for_message(
    state: &AppState,
    recipient: SessionRecord,
) -> Option<SessionRecord> {
    if !messages::restores(&recipient) {
        return Some(recipient);
    }
    match restore_session_with_work(state, &recipient.id) {
        Ok(session) => Some(session),
        Err(error) => {
            eprintln!(
                "restoring auto-retired {} for a message failed: {error:?}",
                recipient.id
            );
            None
        }
    }
}

/// `sm send` to an auto-retired agent restores it before the send, which
/// then queues as for any live agent. A failed restore leaves the send to
/// report the agent stopped, as before.
pub(super) async fn restore_send_target(
    state: &Arc<AppState>,
    identifier: &str,
) -> Result<(), ApiError> {
    let Some(session) = state.session_store.get_session(identifier)? else {
        return Ok(());
    };
    let session = forward_handed_off(state, session)?;
    if !messages::restores(&session) {
        return Ok(());
    }
    let work = state.clone();
    tokio::task::spawn_blocking(move || ready_for_message(&work, session))
        .await
        .map_err(|error| ApiError::from(anyhow::anyhow!(error)))?;
    Ok(())
}

#[cfg(test)]
mod tests;
