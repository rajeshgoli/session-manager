//! Host restart recovery over HTTP (sm#2054): the startup cohort, the
//! restore and leave actions behind `sm recover` and the app's banner, and
//! the one notice each restored agent gets.
//!
//! - `GET /host-restarts/latest`: the latest restart and its cohort.
//! - `POST /host-restarts/{id}/restore`: restore the open members, or only
//!   `session_ids`, through the ordinary restore path.
//! - `POST /host-restarts/{id}/members/{session_id}/leave`: retire one.

use super::*;
use crate::host_restart::{
    self as restarts, CauseSources, CohortMember, HostRestart, HostRestartStore, KilledJob,
    DECISION_FAILED, DECISION_LEFT, DECISION_RESTORED,
};
use crate::work_claims::WorkClaimStore;

/// A cause scan that found nothing is retried for this long: macOS can
/// write the panic report after sm starts.
const CAUSE_RESCAN_SECONDS: i64 = 3600;

fn store_for(config: &AppConfig) -> HostRestartStore {
    HostRestartStore::beside_state_file(&expand_home(&config.paths.state_file))
}

fn queue_db_path(config: &AppConfig) -> PathBuf {
    expand_home(&config.queue_runner_state_dir().to_string_lossy()).join("queue_runner.db")
}

fn display_name(session: &SessionRecord) -> String {
    session
        .friendly_name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| session.name.clone())
}

/// Startup: stop sessions whose runtime vanished and, on a new boot, record
/// them as the restart's cohort. Returns the restart id when this start
/// detected one. Detection trouble never blocks startup.
pub(super) fn startup(
    config: &AppConfig,
    sessions: &SessionStore,
) -> anyhow::Result<Option<String>> {
    let store = store_for(config);
    let boot = config
        .rust_core
        .runtime_enabled
        .then(restarts::current_boot)
        .flatten();
    let detected =
        boot.as_ref()
            .and_then(|boot| match store.detect(boot, OffsetDateTime::now_utc()) {
                Ok(restart) => restart,
                Err(error) => {
                    eprintln!("host restart detection failed: {error:#}");
                    None
                }
            });
    let message = detected.as_ref().map(|restart| {
        format!(
            "Interrupted when the Mac restarted at {}; `sm recover` restores it",
            restarts::restart_time_text(restart)
        )
    });
    let stopped = sessions.reconcile_missing_session_runtimes(message.as_deref())?;
    let Some(restart) = detected else {
        return Ok(None);
    };
    let claims = WorkClaimStore::new(expand_home(&config.sm_send.db_path));
    let mut members = stopped
        .iter()
        .map(|session| cohort_member(session, &claims))
        .collect::<Vec<_>>();
    // An earlier start on this boot may have stopped agents and died before
    // recording them: anything stopped since the boot and not retired joins,
    // its prior state unknown. Members already recorded are kept as they are.
    if let Some(boot) = restart.booted_at_time() {
        for session in sessions.list_sessions(true)? {
            let stopped_since_boot = session
                .stopped_at
                .as_deref()
                .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok())
                .is_some_and(|at| at >= boot);
            if session.is_stopped()
                && !session.is_retired()
                && stopped_since_boot
                && !members.iter().any(|member| member.session_id == session.id)
            {
                let mut member = cohort_member(&session, &claims);
                member.prior_status = "unknown".to_owned();
                members.push(member);
            }
        }
    }
    // The boot counts as handled only once its cohort is durable, so a
    // failure here leaves the next start to detect it again.
    match store.add_members(&restart.id, &members) {
        Ok(()) => {
            if let Some(boot) = &boot {
                if let Err(error) = store.commit_boot(boot, OffsetDateTime::now_utc()) {
                    eprintln!("recording boot {} failed: {error:#}", boot.id);
                }
            }
        }
        Err(error) => eprintln!("recording the {} cohort failed: {error:#}", restart.id),
    }
    eprintln!(
        "host restart {}: {} agents interrupted",
        restart.id,
        members.len()
    );
    scan_and_store_cause(config, sessions, &store, &restart);
    Ok(Some(restart.id))
}

pub(super) fn cohort_member(session: &SessionRecord, claims: &WorkClaimStore) -> CohortMember {
    let prior_status = match session.status.as_str() {
        "running" | "starting" => "running",
        _ => "idle",
    };
    let claims = claims
        .claims_for_session(&session.id, true)
        .unwrap_or_default()
        .into_iter()
        .map(|view| {
            format!(
                "{} {}#{}",
                view.claim.kind, view.claim.repo, view.claim.number
            )
        })
        .collect();
    CohortMember::new(
        &session.id,
        display_name(session),
        &session.provider,
        prior_status,
        Some(session.last_activity.clone()),
        &session.working_dir,
        claims,
    )
}

fn scan_and_store_cause(
    config: &AppConfig,
    sessions: &SessionStore,
    store: &HostRestartStore,
    restart: &HostRestart,
) -> crate::host_restart::RestartCause {
    let utilization = expand_home(&config.utilization.db_path);
    let queue = queue_db_path(config);
    let name = |id: &str| {
        sessions
            .get_session(id)
            .ok()
            .flatten()
            .map(|s| display_name(&s))
    };
    let cause = restarts::scan_cause(
        restart,
        &CauseSources {
            reports_dir: std::path::Path::new(restarts::DIAGNOSTIC_REPORTS_DIR),
            utilization_db: Some(&utilization),
            queue_db: Some(&queue),
            session_name: &name,
        },
    );
    if let Err(error) = store.set_cause(&restart.id, &cause) {
        eprintln!("recording the cause of {} failed: {error:#}", restart.id);
    }
    cause
}

/// The latest restart, rescanning a cause that is still unknown shortly
/// after the restart.
fn latest(state: &AppState) -> anyhow::Result<Option<(HostRestart, Vec<CohortMember>)>> {
    let store = store_for(&state.config);
    let Some((mut restart, members)) = store.latest()? else {
        return Ok(None);
    };
    let recent = OffsetDateTime::parse(&restart.detected_at, &Rfc3339)
        .is_ok_and(|at| (OffsetDateTime::now_utc() - at).whole_seconds() < CAUSE_RESCAN_SECONDS);
    if state.config.rust_core.runtime_enabled
        && (!restart.cause_scanned || (recent && restart.cause.is_unknown()))
    {
        restart.cause = scan_and_store_cause(&state.config, &state.session_store, &store, &restart);
    }
    Ok(Some((restart, members)))
}

fn killed_jobs(state: &AppState, restart: &HostRestart) -> Vec<KilledJob> {
    restarts::killed_jobs(&queue_db_path(&state.config), restart).unwrap_or_else(|error| {
        eprintln!("reading jobs killed by {} failed: {error:#}", restart.id);
        Vec::new()
    })
}

fn view(state: &AppState, restart: &HostRestart, members: &[CohortMember]) -> Value {
    let killed = killed_jobs(state, restart);
    let members = members
        .iter()
        .map(|member| {
            let mut value = serde_json::to_value(member).unwrap_or(Value::Null);
            value["mid_turn"] = json!(member.was_mid_turn());
            value["open"] = json!(member.is_open());
            value["killed_jobs"] = json!(killed
                .iter()
                .filter(|job| job.session_id.as_deref() == Some(member.session_id.as_str()))
                .collect::<Vec<_>>());
            value
        })
        .collect::<Vec<_>>();
    let open = members
        .iter()
        .filter(|member| member["open"] == json!(true))
        .count();
    json!({
        "id": restart.id,
        "booted_at": restart.booted_at,
        "restarted_at_text": restarts::restart_time_text(restart),
        "previous_booted_at": restart.previous_booted_at,
        "detected_at": restart.detected_at,
        "cause": restart.cause,
        "cause_summary": restart.cause.summary(),
        "open_count": open,
        "members": members,
    })
}

/// After any restore of `session_id` succeeds: when it is an open cohort
/// member, mark it restored and send it the restart notice, once.
pub(super) fn after_restore(state: &AppState, session_id: &str) {
    let store = store_for(&state.config);
    let membership = match store.open_membership(session_id) {
        Ok(membership) => membership,
        Err(error) => {
            eprintln!("reading the restart cohort for {session_id} failed: {error:#}");
            return;
        }
    };
    let Some((restart, member)) = membership else {
        return;
    };
    // The member stays open until its notice is queued, so a failed enqueue
    // is retried by the next restore of the cohort.
    let now = OffsetDateTime::now_utc();
    let restart_id = restart.id.clone();
    let close = || {
        if let Err(error) = store.decide(&restart_id, session_id, DECISION_RESTORED, None, now) {
            eprintln!("recording the restore of {session_id} failed: {error:#}");
        }
    };
    if member.noticed_at.is_some() {
        close();
        return;
    }
    let restart = match latest(state) {
        Ok(Some((latest, _))) if latest.id == restart.id => latest,
        _ => restart,
    };
    let text = restarts::notice_text(&restart, &member, &killed_jobs(state, &restart));
    let sent = RetainedQueueStore::new(expand_home(&state.config.sm_send.db_path))
        .enqueue_message_once_with_metadata(
            &format!("host-restart-{}-{session_id}", restart.id),
            session_id,
            &text,
            "sequential",
            crate::queue::QueueMessageMetadata {
                message_category: Some("host_restart".to_owned()),
                ..Default::default()
            },
        );
    match sent {
        Ok(()) => {
            if let Err(error) = store.mark_noticed(&restart.id, session_id, now) {
                eprintln!("recording the restart notice to {session_id} failed: {error:#}");
            }
            close();
        }
        Err(error) => eprintln!("sending the restart notice to {session_id} failed: {error:#}"),
    }
}

fn api_error_text(error: &ApiError) -> String {
    match error {
        ApiError::Internal(error) => format!("{error:#}"),
        ApiError::NotFound(detail) => (*detail).to_owned(),
        ApiError::Status { detail, .. } => detail.clone(),
        ApiError::StatusBody { body, .. } => body["detail"]
            .as_str()
            .map_or_else(|| body.to_string(), ToOwned::to_owned),
        ApiError::Auth { detail, .. } => (*detail).to_owned(),
    }
}

/// Restore the open members of `restart_id` (only `only`, when given).
/// Each goes through the ordinary restore path; a failure stays open.
pub(super) fn restore_cohort(
    state: &AppState,
    restart_id: &str,
    only: Option<&[String]>,
) -> Result<Vec<Value>, ApiError> {
    let store = store_for(&state.config);
    let Some((_, members)) = store.get(restart_id)? else {
        return Err(ApiError::NotFound("Host restart not found"));
    };
    let mut outcomes = Vec::new();
    for member in members.iter().filter(|member| member.is_open()) {
        if only.is_some_and(|only| !only.contains(&member.session_id)) {
            continue;
        }
        let session = state.session_store.get_session(&member.session_id)?;
        // Retired some other way since: the owner decided, so it stays retired.
        if session.as_ref().is_some_and(SessionRecord::is_retired) {
            store.decide(
                restart_id,
                &member.session_id,
                DECISION_LEFT,
                None,
                OffsetDateTime::now_utc(),
            )?;
            outcomes.push(
                json!({"session_id": member.session_id, "name": member.name, "outcome": "left"}),
            );
            continue;
        }
        let already_running = session.is_some_and(|session| !session.is_stopped());
        let result = if already_running {
            // Restored some other way since; it still needs its notice.
            after_restore(state, &member.session_id);
            Ok(())
        } else {
            auto_retire::restore_session_with_work(state, &member.session_id).map(|_| ())
        };
        let outcome = match result {
            Ok(()) => {
                json!({"session_id": member.session_id, "name": member.name, "outcome": "restored"})
            }
            Err(error) => {
                let detail = api_error_text(&error);
                store.decide(
                    restart_id,
                    &member.session_id,
                    DECISION_FAILED,
                    Some(&detail),
                    OffsetDateTime::now_utc(),
                )?;
                json!({"session_id": member.session_id, "name": member.name, "outcome": "failed", "error": detail})
            }
        };
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

impl AppState {
    /// The restart this server's start detected, if any.
    pub fn detected_host_restart(&self) -> Option<&str> {
        self.detected_host_restart.as_deref()
    }

    /// Restore the detected restart's cohort when the owner leaves
    /// "restore interrupted agents after a restart" on. Queue jobs are
    /// never resubmitted.
    pub fn auto_restore_after_host_restart(&self) {
        let Some(restart_id) = self.detected_host_restart() else {
            return;
        };
        let enabled = self
            .session_store
            .owner_settings()
            .map(|settings| crate::owner_settings::restore_after_host_restart(&settings))
            .unwrap_or(true);
        if !enabled {
            eprintln!("host restart {restart_id}: auto-restore is off; waiting for sm recover");
            return;
        }
        match restore_cohort(self, restart_id, None) {
            Ok(outcomes) => eprintln!(
                "host restart {restart_id}: auto-restore {}",
                serde_json::to_string(&outcomes).unwrap_or_default()
            ),
            Err(error) => eprintln!(
                "host restart {restart_id}: auto-restore failed: {}",
                api_error_text(&error)
            ),
        }
    }
}

fn guard(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    method: &str,
    path: &str,
) -> Result<Option<String>, ApiError> {
    let owner = owner_web_guard(state, headers, Some(peer_addr), method)?;
    if owner.is_none() {
        ensure_session_allowed_from_parts(&state.config, headers, Some(peer_addr), path)?;
    }
    Ok(owner)
}

async fn blocking<T: Send + 'static>(
    state: &Arc<AppState>,
    work: impl FnOnce(&AppState) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || work(&state))
        .await
        .map_err(|error| ApiError::from(anyhow::anyhow!(error)))?
}

/// `GET /host-restarts/latest`.
pub(super) async fn get_latest(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    guard(&state, &headers, peer_addr, "GET", "/host-restarts/latest")?;
    let restart = blocking(&state, |state| {
        Ok(latest(state)?.map(|(restart, members)| view(state, &restart, &members)))
    })
    .await?;
    Ok(Json(json!({ "restart": restart })))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RestoreRequest {
    #[serde(default)]
    session_ids: Option<Vec<String>>,
}

/// `POST /host-restarts/{id}/restore`.
pub(super) async fn post_restore(
    State(state): State<Arc<AppState>>,
    Path(restart_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Option<Json<RestoreRequest>>,
) -> Result<Json<Value>, ApiError> {
    guard(
        &state,
        &headers,
        peer_addr,
        "POST",
        &format!("/host-restarts/{restart_id}/restore"),
    )?;
    ensure_core_writes_enabled(&state)?;
    let only = body.and_then(|Json(body)| body.session_ids);
    let id = restart_id.clone();
    let outcomes = blocking(&state, move |state| {
        restore_cohort(state, &id, only.as_deref())
    })
    .await?;
    board::request_recompute(&state);
    let restart = blocking(&state, move |state| {
        Ok(store_for(&state.config)
            .get(&restart_id)?
            .map(|(restart, members)| view(state, &restart, &members)))
    })
    .await?;
    Ok(Json(json!({ "results": outcomes, "restart": restart })))
}

/// `POST /host-restarts/{id}/members/{session_id}/leave`: retire the agent
/// instead of restoring it.
pub(super) async fn post_leave(
    State(state): State<Arc<AppState>>,
    Path((restart_id, session_id)): Path<(String, String)>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let owner = guard(
        &state,
        &headers,
        peer_addr,
        "POST",
        &format!("/host-restarts/{restart_id}/members/{session_id}/leave"),
    )?;
    ensure_core_writes_enabled(&state)?;
    let (id, sid) = (restart_id.clone(), session_id.clone());
    let open = blocking(&state, move |state| {
        Ok(store_for(&state.config)
            .get(&id)?
            .ok_or(ApiError::NotFound("Host restart not found"))?
            .1
            .into_iter()
            .any(|member| member.session_id == sid && member.is_open()))
    })
    .await?;
    if !open {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: format!("{session_id} is not waiting on a decision in {restart_id}"),
        });
    }
    let Json(retired) = retire_session_after_auth(
        state.clone(),
        session_id.clone(),
        peer_addr,
        headers,
        RetireSessionRequest {
            requester_session_id: None,
            if_finished_idle: false,
        },
        owner,
    )
    .await?;
    if let Some(error) = retired.get("error") {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: error.as_str().unwrap_or("retire failed").to_owned(),
        });
    }
    let restart = blocking(&state, move |state| {
        let store = store_for(&state.config);
        store.decide(
            &restart_id,
            &session_id,
            DECISION_LEFT,
            None,
            OffsetDateTime::now_utc(),
        )?;
        Ok(store
            .get(&restart_id)?
            .map(|(restart, members)| view(state, &restart, &members)))
    })
    .await?;
    board::request_recompute(&state);
    Ok(Json(json!({ "retired": retired, "restart": restart })))
}
