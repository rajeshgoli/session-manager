//! Context handoff over HTTP (sm#1651). Owner controls (Appendix I.1):
//! `GET/PUT /handoff-defaults` and `GET/PUT /sessions/{id}/handoff-policy`.
//! Execution (Appendices E-H): `POST /sessions/{id}/handoff`, starting the
//! successor when the agent's turn ends, and moving its work in order.

use super::*;
use crate::handoff::execute::{self, Brief, HandoffNote};
use crate::handoff::policy::PolicyUpdate;
use crate::sessions::{
    HandoffAcceptOutcome, HandoffPolicyOutcome, HandoffWork, ReviewAsk, SuccessorPlan,
};

/// Header the `sm` client sends from inside a managed session.
pub(super) const SESSION_HEADER: &str = "x-sm-session";
const OWNER_ONLY: &str = "handoff policy is owner-only";

fn status(status: StatusCode, detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status,
        detail: detail.into(),
    }
}

/// Owner only: the usual session gate, then refuse anything that identifies
/// itself as an agent, then require a browser's `Origin` to match the host.
fn ensure_owner(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    path: &str,
    body: Option<&Value>,
) -> Result<(), ApiError> {
    // These controls share the watch page's owner login, including a verified
    // Cloudflare browser assertion. Agent and Origin restrictions still apply.
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    *request.headers_mut() = headers.clone();
    request.extensions_mut().insert(ConnectInfo(peer_addr));
    ensure_owner_page_read_allowed(state, &request)?;
    let from_agent = header_text(headers, SESSION_HEADER).is_some()
        || body.is_some_and(|body| body.get("requester_session_id").is_some());
    if from_agent {
        return Err(status(StatusCode::FORBIDDEN, OWNER_ONLY));
    }
    if let Some(origin) = header_text(headers, "origin") {
        let host =
            header_text(headers, "x-forwarded-host").or_else(|| header_text(headers, "host"));
        if !origin_matches_host(&origin, host.as_deref()) {
            return Err(status(StatusCode::FORBIDDEN, "Origin does not match host"));
        }
    }
    Ok(())
}

/// `https://sm.example.com` matches host `sm.example.com`. Non-browser
/// clients (the app, sm watch) send no Origin and skip this check.
pub(super) fn origin_matches_host(origin: &str, host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let Some((_, authority)) = origin.split_once("://") else {
        return false;
    };
    authority
        .trim_end_matches('/')
        .eq_ignore_ascii_case(host.trim())
}

/// Appendix C.3: a successful review request may ask its requester to hand
/// off. The ask rides in the response for the CLI to print; a failure to
/// evaluate it never fails the review request.
pub(super) fn add_review_handoff_ask(
    state: &AppState,
    requester_session_id: Option<&str>,
    ask: ReviewAsk<'_>,
    response: &mut Value,
) {
    let Some(requester) = requester_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    match state.session_store.review_handoff_ask(requester, ask) {
        Ok(Some(text)) => response["handoff_ask"] = json!(text),
        Ok(None) => {}
        Err(error) => eprintln!("handoff ask for {requester} failed: {error:#}"),
    }
}

pub(super) async fn get_handoff_defaults(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    ensure_owner(&state, &headers, peer_addr, "/handoff-defaults", None)?;
    Ok(Json(state.session_store.handoff_defaults()?.to_json()))
}

pub(super) async fn put_handoff_defaults(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    ensure_owner(
        &state,
        &headers,
        peer_addr,
        "/handoff-defaults",
        Some(&body),
    )?;
    ensure_core_writes_enabled(&state)?;
    match state.session_store.update_handoff_defaults(&body)? {
        Ok(_) => Ok(Json(state.session_store.handoff_defaults()?.to_json())),
        Err(detail) => Err(status(StatusCode::BAD_REQUEST, detail)),
    }
}

pub(super) async fn get_handoff_policy(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    ensure_owner(
        &state,
        &headers,
        peer_addr,
        "/sessions/handoff-policy",
        None,
    )?;
    state
        .session_store
        .handoff_policy_view(&session_id)?
        .map(Json)
        .ok_or(ApiError::NotFound("Session not found"))
}

pub(super) async fn put_handoff_policy(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    ensure_owner(
        &state,
        &headers,
        peer_addr,
        "/sessions/handoff-policy",
        Some(&body),
    )?;
    ensure_core_writes_enabled(&state)?;
    let update =
        PolicyUpdate::parse(&body).map_err(|detail| status(StatusCode::BAD_REQUEST, detail))?;
    match state.session_store.update_handoff_policy(
        &session_id,
        &update,
        &state.config.owner_name,
    )? {
        HandoffPolicyOutcome::Updated(view) => Ok(Json(view)),
        HandoffPolicyOutcome::NotFound => Err(ApiError::NotFound("Session not found")),
        HandoffPolicyOutcome::Conflict(detail) => Err(status(StatusCode::CONFLICT, detail)),
    }
}

pub(super) async fn get_ticket_handoff_policy(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i64)>,
) -> Result<Json<Value>, ApiError> {
    ensure_owner(&state, &headers, peer_addr, "/handoff-policy/ticket", None)?;
    if number <= 0 || owner.is_empty() || repo.is_empty() {
        return Err(status(StatusCode::BAD_REQUEST, "invalid ticket"));
    }
    Ok(Json(state.session_store.ticket_handoff_policy_view(
        &format!("{owner}/{repo}"),
        number,
    )?))
}

pub(super) async fn put_ticket_handoff_policy(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    ensure_owner(
        &state,
        &headers,
        peer_addr,
        "/handoff-policy/ticket",
        Some(&body),
    )?;
    ensure_core_writes_enabled(&state)?;
    if number <= 0 || owner.is_empty() || repo.is_empty() {
        return Err(status(StatusCode::BAD_REQUEST, "invalid ticket"));
    }
    let update =
        PolicyUpdate::parse(&body).map_err(|detail| status(StatusCode::BAD_REQUEST, detail))?;
    match state.session_store.update_ticket_handoff_policy(
        &format!("{owner}/{repo}"),
        number,
        &update,
    )? {
        Ok(view) => Ok(Json(view)),
        Err(detail) => Err(status(StatusCode::BAD_REQUEST, detail)),
    }
}

/// `sm handoff --link|--path` (Appendix E).
pub(super) async fn post_handoff(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/sessions/{session_id}/handoff"),
    )?;
    ensure_core_writes_enabled(&state)?;
    if body.get("file_path").is_some() {
        return Err(status(
            StatusCode::BAD_REQUEST,
            "sm handoff was updated; rerun sm handoff --path <file>",
        ));
    }
    let requester = body
        .get("requester_session_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let note = HandoffNote::parse(body.get("note").unwrap_or(&Value::Null))
        .map_err(|detail| status(StatusCode::BAD_REQUEST, detail))?;
    match state
        .session_store
        .accept_handoff(&session_id, requester, &note)?
    {
        HandoffAcceptOutcome::Accepted { ready } => {
            if ready {
                kick_handoff(state.clone(), session_id);
            }
            Ok(Json(json!({
                "status": "accepted",
                "message": execute::ACCEPTED_TEXT,
            })))
        }
        HandoffAcceptOutcome::Forbidden => Err(status(
            StatusCode::FORBIDDEN,
            "sm handoff is self-directed only",
        )),
        HandoffAcceptOutcome::NotFound => Err(ApiError::NotFound("Session not found")),
        HandoffAcceptOutcome::Conflict(detail) => Err(status(StatusCode::CONFLICT, detail)),
    }
}

/// Handoffs this process is driving, so the sweep never runs one twice.
fn running_handoffs() -> &'static std::sync::Mutex<BTreeSet<String>> {
    static RUNNING: std::sync::OnceLock<std::sync::Mutex<BTreeSet<String>>> =
        std::sync::OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

/// Holds a predecessor id in [`running_handoffs`] until dropped.
struct RunningHandoff(String);

impl RunningHandoff {
    fn take(predecessor_id: &str) -> Option<Self> {
        let mut running = running_handoffs()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        running
            .insert(predecessor_id.to_owned())
            .then(|| Self(predecessor_id.to_owned()))
    }
}

impl Drop for RunningHandoff {
    fn drop(&mut self) {
        running_handoffs()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.0);
    }
}

/// F.1: the agent's turn ended (or had already ended). Starts the successor
/// in the background if the handoff is accepted and the agent is idle.
pub(super) fn kick_handoff(state: Arc<AppState>, session_id: String) {
    tokio::spawn(async move {
        if let Err(error) = run_handoff(state, &session_id).await {
            eprintln!("handoff of {session_id} failed: {error:#}");
        }
    });
}

/// Every two seconds: start accepted handoffs whose agent went idle (the
/// codex-fork and codex-app idle signals arrive here), resume transfers a
/// restart interrupted, and move messages that reached a predecessor after
/// its handoff. The Claude Stop hook and `sm handoff` itself kick
/// a handoff directly, so this is the backstop, not the fast path.
pub(super) fn spawn_handoff_sweeper(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if state.shutdown().is_stopped() {
                return;
            }
            match state.session_store.stranded_handoff_messages() {
                Ok(stranded) => {
                    for (predecessor, successor) in stranded {
                        if let Err(error) = state
                            .session_store
                            .hand_off_pending_messages(&predecessor, &successor)
                        {
                            eprintln!("moving late messages of {predecessor} failed: {error:#}");
                        }
                    }
                }
                Err(error) => eprintln!("handoff message sweep failed: {error:#}"),
            }
            let work = match state.session_store.pending_handoff_work() {
                Ok(work) => work,
                Err(error) => {
                    eprintln!("handoff sweep failed: {error:#}");
                    continue;
                }
            };
            for item in work {
                let state = state.clone();
                match item {
                    HandoffWork::Start(session_id) => kick_handoff(state, session_id),
                    HandoffWork::Resume {
                        predecessor_id,
                        successor_id,
                    } => {
                        tokio::spawn(async move {
                            let Some(_running) = RunningHandoff::take(&predecessor_id) else {
                                return;
                            };
                            if let Err(error) =
                                complete_handoff(&state, &predecessor_id, &successor_id).await
                            {
                                eprintln!(
                                    "resuming the handoff of {predecessor_id} failed: {error:#}"
                                );
                            }
                        });
                    }
                }
            }
        }
    });
}

/// Start the successor, then move the work (Appendices F and G).
pub(super) async fn run_handoff(state: Arc<AppState>, predecessor_id: &str) -> anyhow::Result<()> {
    // Every Claude turn end lands here; most have no handoff to start, and
    // this read is served from the parsed-state cache.
    let accepted = state
        .session_store
        .get_session(predecessor_id)?
        .and_then(|session| session.handoff)
        .is_some_and(|record| record.get("state").and_then(Value::as_str) == Some("accepted"));
    if !accepted {
        return Ok(());
    }
    let Some(_running) = RunningHandoff::take(predecessor_id) else {
        return Ok(());
    };
    let Some(plan) = state.session_store.claim_handoff_start(predecessor_id)? else {
        return Ok(());
    };
    let successor = match create_successor(&state, &plan).await {
        Ok(successor) => successor,
        Err(error) => {
            state.session_store.fail_handoff(predecessor_id, &error)?;
            return Ok(());
        }
    };
    if state.config.rust_core.runtime_enabled {
        // Let the new agent's composer settle before its brief is typed.
        tokio::time::sleep(state.runtime().startup_settle_duration()).await;
    }
    complete_handoff(&state, predecessor_id, &successor.id).await
}

/// F.2: the same provider, model, effort, worktree, machine and parent,
/// through the core create path. No initial prompt: the brief follows the
/// transfer. The error is the D.6 reason.
async fn create_successor(
    state: &Arc<AppState>,
    plan: &SuccessorPlan,
) -> Result<SessionRecord, String> {
    let payload = CreateCoreSessionRequest {
        id: None,
        name: Some(plan.successor_name.clone()),
        working_dir: Some(plan.working_dir.clone()),
        provider: Some(plan.provider.clone()),
        parent_session_id: plan.parent_session_id.clone(),
        node: Some(plan.node.clone()),
        initial_message: None,
        model: plan.model.clone(),
        reasoning_effort: plan.reasoning_effort.clone(),
        wait: None,
        spawn_prompt_source: None,
        spawn_brief: None,
        started_by_sm: true,
    };
    let log_dir = state.config.rust_core.log_dir.as_deref().map(expand_home);
    let created = if state.config.rust_core.runtime_enabled {
        match ensure_core_runtime_provider_supported(&payload)
            .and_then(|()| ensure_core_runtime_request_node_supported(state, &payload))
        {
            Ok(()) => create_runtime_core_session(state.clone(), payload, log_dir).await,
            Err(error) => Err(error),
        }
    } else {
        state
            .session_store
            .create_core_session(payload, log_dir)
            .map_err(ApiError::Internal)
    };
    created.map_err(|error| api_error_reason(&error))
}

fn api_error_reason(error: &ApiError) -> String {
    match error {
        ApiError::Internal(error) => format!("{error:#}"),
        ApiError::NotFound(detail) => (*detail).to_owned(),
        ApiError::Status { detail, .. } => detail.clone(),
        ApiError::StatusBody { body, .. } => body.to_string(),
        ApiError::Auth { detail, .. } => (*detail).to_owned(),
    }
}

/// G steps 1-6. Every step is safe to repeat, so a restart resumes here.
async fn complete_handoff(
    state: &Arc<AppState>,
    predecessor_id: &str,
    successor_id: &str,
) -> anyhow::Result<()> {
    let store = state.session_store.clone();
    let (pred, succ) = (predecessor_id.to_owned(), successor_id.to_owned());
    let task_state = state.clone();
    let brief =
        tokio::task::spawn_blocking(move || move_work(&task_state, &store, &pred, &succ)).await??;
    // Step 3: the successor never acts before it holds the work, and reads
    // the brief before any message it inherits.
    state.session_store.queue_handoff_notice(
        predecessor_id,
        successor_id,
        &brief.text,
        &execute::brief_message_id(predecessor_id),
        &[successor_id, predecessor_id],
    )?;
    state
        .session_store
        .hand_off_pending_messages(predecessor_id, successor_id)?;
    // Step 4. Sends resolve to the successor once the predecessor has
    // stopped; anything that reached it before then moves now.
    retire_predecessor(state, predecessor_id, successor_id).await?;
    state
        .session_store
        .hand_off_pending_messages(predecessor_id, successor_id)?;
    // Step 5.
    if let Some(parent_id) = brief.parent_session_id.as_deref() {
        let notice = execute::parent_notice_text(
            (&brief.facts.predecessor_name, predecessor_id),
            (&brief.facts.successor_name, successor_id),
            brief.facts.percent,
            &brief.facts.note,
        );
        state.session_store.queue_handoff_notice(
            predecessor_id,
            parent_id,
            &notice,
            &execute::parent_notice_message_id(predecessor_id),
            &[],
        )?;
    }
    // Steps 5 (leftover asks) and 6.
    state.session_store.finish_handoff(predecessor_id)
}

struct PreparedBrief {
    text: String,
    parent_session_id: Option<String>,
    facts: crate::sessions::HandoffFacts,
}

/// G steps 1 and 2: lineage, then every row of the transfer table. Returns
/// the brief, built from what the successor now holds.
fn move_work(
    state: &AppState,
    store: &SessionStore,
    predecessor_id: &str,
    successor_id: &str,
) -> anyhow::Result<PreparedBrief> {
    store.transfer_handoff_json(predecessor_id, successor_id)?;
    let successor_name = store
        .get_session(successor_id)?
        .map(|session| {
            session
                .friendly_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(session.name)
        })
        .unwrap_or_else(|| successor_id.to_owned());
    let db_path = expand_home(&state.config.sm_send.db_path);
    let claims = claims::work_claim_store(state);
    claims.hand_off(predecessor_id, successor_id, &successor_name)?;
    let queue = RetainedQueueStore::new(db_path.clone());
    queue.hand_off_rows(predecessor_id, successor_id)?;
    let queue_dir = expand_home(&state.config.queue_runner_state_dir().to_string_lossy());
    RetainedQueueStore::hand_off_queue_jobs(&queue_dir, predecessor_id, successor_id)?;
    docs::owner_doc_store(state).hand_off(predecessor_id, successor_id, &successor_name)?;
    crate::owner_push::OwnerPushStore::new(crate::owner_push::push_db_path(&state.config))
        .hand_off_follows(predecessor_id, successor_id, &successor_name)?;

    let facts = store.handoff_facts(predecessor_id, successor_id)?;
    let held = claims.claims_for_session(successor_id, true)?;
    let branch = held.iter().find_map(|view| view.claim.branch.clone());
    let mut pending = queue.handoff_pending_items(successor_id)?;
    pending.extend(RetainedQueueStore::handoff_queue_job_items(
        &queue_dir,
        successor_id,
    )?);
    let brief = Brief {
        predecessor_name: facts.predecessor_name.clone(),
        predecessor_id: predecessor_id.to_owned(),
        percent: facts.percent,
        note: facts.note.clone(),
        working_dir: facts.working_dir.clone(),
        branch,
        claims: store.handoff_claims_text(successor_id),
        roles: facts.roles.clone(),
        pending,
        children: facts.children.clone(),
        original_brief: facts.original_brief.clone(),
    };
    Ok(PreparedBrief {
        text: brief.text(),
        parent_session_id: facts.parent_session_id.clone(),
        facts,
    })
}

/// H.1: server-driven retire, which root protection does not block. The
/// claims have moved, so retire ends none and worktree cleanup finds none.
async fn retire_predecessor(
    state: &Arc<AppState>,
    predecessor_id: &str,
    successor_id: &str,
) -> anyhow::Result<()> {
    let authority = RetireAuthority::handoff(successor_id);
    let task_state = state.clone();
    let pred = predecessor_id.to_owned();
    let outcome = tokio::task::spawn_blocking(move || {
        if task_state.config.rust_core.runtime_enabled {
            let runtime = task_state.runtime();
            task_state
                .session_store
                .retire_core_session_with_runtime_authorized(&pred, authority, None, &runtime)
        } else {
            task_state
                .session_store
                .retire_core_session_authorized(&pred, authority, None)
        }
    })
    .await??;
    match outcome {
        CoreRetireOutcome::Retired(_) | CoreRetireOutcome::NotFound => {}
        other => anyhow::bail!("retiring {predecessor_id} after its handoff: {other:?}"),
    }
    if let Err(error) = teardown_btw_requests_for_session(state, predecessor_id) {
        eprintln!("handed off {predecessor_id} but BTW teardown failed: {error:#}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::origin_matches_host;

    #[test]
    fn origin_must_match_host() {
        assert!(origin_matches_host(
            "https://sm.rajeshgo.li",
            Some("sm.rajeshgo.li")
        ));
        assert!(origin_matches_host(
            "http://localhost:8420",
            Some("localhost:8420")
        ));
        assert!(!origin_matches_host(
            "https://evil.example",
            Some("sm.rajeshgo.li")
        ));
        assert!(!origin_matches_host("null", Some("sm.rajeshgo.li")));
        assert!(!origin_matches_host("https://sm.rajeshgo.li", None));
    }
}
