//! Work claims HTTP surface (sm#1452, ticket #1485): `POST /claims`,
//! `POST /claims/release`, `GET /claims`, implicit PR claims, the
//! `sm spawn --ticket` reservation, and the periodic GitHub sync.

use super::*;
use crate::work_claims::{
    closing_refs_page_query, items_query, parse_closing_refs_page, parse_items_response,
    BatchFetch, ClaimOutcome, ClaimRequest, ClaimResult, ClaimSource, ClaimView, HolderState,
    ItemFetch, SessionDirectory, SessionInfo, WorkClaimStore, WorkItemSource, WorkKind,
    MAX_ALIASES_PER_QUERY,
};

/// `gh api graphql`, parsed whatever the exit code (appendix D).
#[derive(Debug)]
pub(super) struct GhCliWorkItemSource;

fn gh_graphql_stdout(query: &str) -> Result<Vec<u8>, String> {
    let mut nonce = [0u8; 8];
    OsRng.fill_bytes(&mut nonce);
    let input = std::env::temp_dir().join(format!(
        "sm-claims-graphql-{}-{}.json",
        std::process::id(),
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    fs::write(
        &input,
        serde_json::to_vec(&json!({ "query": query })).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("failed to write GraphQL request: {error}"))?;
    let args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "--input".to_owned(),
        input.display().to_string(),
    ];
    let output = gh_command_output(&args, Duration::from_secs(30));
    let _ = fs::remove_file(&input);
    let output = output.map_err(|error| format!("gh api graphql failed: {error}"))?;
    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        return Err(format!(
            "gh api graphql failed: {}",
            command_stderr(&output)
        ));
    }
    Ok(output.stdout)
}

impl WorkItemSource for GhCliWorkItemSource {
    fn fetch(&self, repo: &str, numbers: &[i64]) -> Result<BatchFetch, String> {
        let stdout = gh_graphql_stdout(&items_query(repo, numbers))?;
        let (mut batch, more) = parse_items_response(&stdout, numbers)?;
        for (number, cursor) in more {
            let rest = (|| {
                let mut refs = Vec::new();
                let mut cursor = Some(cursor);
                while let Some(after) = cursor {
                    let stdout = gh_graphql_stdout(&closing_refs_page_query(repo, number, &after))?;
                    let (page, next) = parse_closing_refs_page(&stdout)?;
                    refs.extend(page);
                    cursor = next;
                }
                Ok::<_, String>(refs)
            })();
            if let Some(ItemFetch::Found(item)) = batch.get_mut(&number) {
                match rest {
                    Ok(refs) => {
                        if let Some(existing) = item.closing_refs.as_mut() {
                            existing.extend(refs);
                        }
                    }
                    // Links reconcile only from a complete set.
                    Err(_) => item.closing_refs = None,
                }
            }
        }
        Ok(batch)
    }
}

pub(super) fn work_claim_store(state: &AppState) -> WorkClaimStore {
    WorkClaimStore::new(expand_home(&state.config.sm_send.db_path))
}

fn is_retired(record: &SessionRecord) -> bool {
    matches!(
        record.completion_status.as_deref(),
        Some("retired" | "killed")
    )
}

fn bad_request(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: detail.into(),
    }
}

pub(super) fn session_info(record: &SessionRecord) -> SessionInfo {
    let state = if is_retired(record) {
        HolderState::Retired
    } else if record.is_stopped() {
        HolderState::Stopped
    } else if matches!(record.status.as_str(), "running" | "starting") {
        HolderState::Working
    } else {
        HolderState::Idle
    };
    SessionInfo {
        id: record.id.clone(),
        name: record
            .friendly_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| record.name.clone()),
        parent_session_id: record.parent_session_id.clone(),
        state,
        stopped_at: record.stopped_at.clone(),
    }
}

pub(super) fn session_directory(state: &AppState) -> anyhow::Result<SessionDirectory> {
    Ok(SessionDirectory::new(
        state
            .session_store
            .list_sessions(true)?
            .iter()
            .map(session_info),
    ))
}

/// The claim with its item and page links, as responses show it.
fn claim_view_json(config: &AppConfig, view: &ClaimView) -> Value {
    let mut value = serde_json::to_value(view).unwrap_or(Value::Null);
    if let Some(base) = docs::doc_browser_base_url(config) {
        value["history_url"] = json!(format!("{base}{}", view.history_path));
    }
    value
}

fn claim_json(state: &AppState, claim_id: &str) -> Result<Value, ApiError> {
    let store = work_claim_store(state);
    let claim = store
        .claim(claim_id)?
        .ok_or(ApiError::NotFound("Claim not found"))?;
    let item = store.item(&claim.repo, claim.number)?;
    let view = ClaimView {
        title: item.as_ref().map(|i| i.title.clone()).unwrap_or_default(),
        state: item
            .as_ref()
            .map(|i| i.state.clone())
            .unwrap_or_else(|| "open".to_owned()),
        url: item.map(|i| i.url).unwrap_or_default(),
        history_path: crate::work_claims::history_path(&claim.repo, claim.number),
        claim,
    };
    Ok(claim_view_json(&state.config, &view))
}

/// Delivers queued `[sm claim]` messages now instead of at the next drain.
fn deliver_claim_notices(state: &AppState, targets: &[String]) {
    if !state.config.rust_core.runtime_enabled {
        return;
    }
    let runtime = TmuxRuntime::from_app_config(&state.config);
    for target in targets {
        if let Err(error) = state
            .session_store
            .drain_runtime_pending_messages_for_session(target, &runtime)
        {
            eprintln!("work claim message delivery to {target} failed: {error:#}");
        }
    }
}

fn parse_kind(kind: &str) -> Result<WorkKind, ApiError> {
    WorkKind::parse(kind.trim()).ok_or_else(|| bad_request("kind must be ticket or pr"))
}

fn validated_repo(repo: &str) -> Result<String, ApiError> {
    let repo = crate::work_claims::canonical_repo(repo);
    crate::owner_docs::validate_repo_slug(&repo).map_err(|error| bad_request(error.to_string()))?;
    Ok(repo)
}

/// The requesting session: it must exist and not be retired.
fn claimant(state: &AppState, session_id: &str) -> Result<SessionRecord, ApiError> {
    let session_id = session_id.trim();
    let Some(session) = state.session_store.get_session(session_id)? else {
        return Err(bad_request(format!("Session {session_id} not found")));
    };
    if is_retired(&session) {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: format!("Session {session_id} is retired"),
        });
    }
    Ok(session)
}

/// The claim-time fetch: the item plus every `--ticket`, in one query.
async fn fetch_for_claim(
    state: &AppState,
    repo: &str,
    numbers: Vec<i64>,
) -> Result<BatchFetch, String> {
    let source = state.work_item_source.clone();
    let repo = repo.to_owned();
    tokio::task::spawn_blocking(move || source.fetch(&repo, &numbers))
        .await
        .map_err(|error| format!("fetch task failed: {error}"))?
}

async fn run_explicit_claim(
    state: &Arc<AppState>,
    request: ClaimRequest,
) -> Result<ClaimResult, ApiError> {
    if request.tickets.len() + 1 > MAX_ALIASES_PER_QUERY {
        return Err(bad_request("too many --ticket numbers"));
    }
    let mut numbers = vec![request.number];
    numbers.extend(request.tickets.iter().copied());
    let fetched = fetch_for_claim(state, &request.repo, numbers).await;
    let worker_state = state.clone();
    tokio::task::spawn_blocking(move || {
        let sessions = session_directory(&worker_state)?;
        work_claim_store(&worker_state).claim_explicit(&request, fetched, &sessions)
    })
    .await
    .map_err(|error| anyhow::anyhow!("claim task failed: {error}"))?
    .map_err(ApiError::from)
}

/// Every outcome but a new or held claim, as an API error.
fn claim_failure(outcome: ClaimOutcome) -> ApiError {
    match outcome {
        ClaimOutcome::Collision { holders } => ApiError::StatusBody {
            status: StatusCode::CONFLICT,
            body: json!({ "outcome": "collision", "holders": holders }),
        },
        ClaimOutcome::Rejected(detail) => ApiError::Status {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            detail,
        },
        ClaimOutcome::Unreachable(detail) => ApiError::Status {
            status: StatusCode::BAD_GATEWAY,
            detail,
        },
        ClaimOutcome::Claimed { .. } | ClaimOutcome::AlreadyHeld { .. } => {
            ApiError::Internal(anyhow::anyhow!("claim succeeded"))
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct PostClaimRequest {
    requester_session_id: String,
    kind: String,
    repo: String,
    number: i64,
    #[serde(default)]
    take: bool,
    #[serde(default)]
    worktree_path: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    tickets: Vec<i64>,
}

pub(super) async fn post_claim(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<PostClaimRequest>,
) -> Result<Response, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), "/claims")?;
    ensure_core_writes_enabled(&state)?;
    let kind = parse_kind(&payload.kind)?;
    let repo = validated_repo(&payload.repo)?;
    if payload.number <= 0 || payload.tickets.iter().any(|ticket| *ticket <= 0) {
        return Err(bad_request("numbers must be positive"));
    }
    let session = claimant(&state, &payload.requester_session_id)?;
    let request = ClaimRequest {
        repo,
        number: payload.number,
        kind,
        claimant: session_info(&session),
        source: ClaimSource::Explicit,
        take: payload.take,
        worktree_path: trimmed(&payload.worktree_path),
        branch: trimmed(&payload.branch),
        tickets: payload.tickets,
        reserve: false,
    };
    let result = run_explicit_claim(&state, request).await?;
    deliver_claim_notices(&state, &result.notified);
    match result.outcome {
        ClaimOutcome::Claimed {
            claim,
            taken,
            notes,
            warnings,
        } => Ok((
            StatusCode::CREATED,
            Json(json!({
                "outcome": if taken { "taken" } else { "claimed" },
                "claim": claim_json(&state, &claim.id)?,
                "notes": notes,
                "warnings": warnings,
            })),
        )
            .into_response()),
        ClaimOutcome::AlreadyHeld { claim, notes } => Ok(Json(json!({
            "outcome": "already_held",
            "claim": claim_json(&state, &claim.id)?,
            "notes": notes,
        }))
        .into_response()),
        outcome => Err(claim_failure(outcome)),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ReleaseClaimRequest {
    requester_session_id: String,
    kind: String,
    repo: String,
    number: i64,
}

pub(super) async fn release_claim(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<ReleaseClaimRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), "/claims/release")?;
    ensure_core_writes_enabled(&state)?;
    let kind = parse_kind(&payload.kind)?;
    let repo = validated_repo(&payload.repo)?;
    let session_id = payload.requester_session_id.trim();
    let Some(claim) = work_claim_store(&state).release(session_id, &repo, payload.number, kind)?
    else {
        return Err(ApiError::Status {
            status: StatusCode::NOT_FOUND,
            detail: format!("You don't hold {} #{}.", kind.noun(), payload.number),
        });
    };
    Ok(Json(json!({ "claim": claim_json(&state, &claim.id)? })))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ListClaimsQuery {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    active: Option<bool>,
}

pub(super) async fn list_claims(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListClaimsQuery>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_session_read_allowed(&state, &request)?;
    let active = query.active.unwrap_or(true);
    let store = work_claim_store(&state);
    let claims = match query
        .session
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(identifier) => {
            let session_id = resolve_session_or_registry_role(&state, identifier)?
                .map(|session| session.id)
                .unwrap_or_else(|| identifier.to_owned());
            store.claims_for_session(&session_id, active)?
        }
        None if active => store.active_claims()?,
        None => return Err(bad_request("active=false needs session=<id>")),
    };
    Ok(Json(json!({
        "claims": claims
            .iter()
            .map(|view| claim_view_json(&state.config, view))
            .collect::<Vec<_>>(),
    })))
}

/// The implicit PR claim after a Codex review request or a doc publish on a
/// PR (appendix E). Never fails the host command: a failure is logged and
/// the next sync's reconciliation records it. Returns the warning to print.
pub(super) fn record_implicit_pr_claim(
    state: &AppState,
    repo: &str,
    pr: i64,
    session_id: &str,
    source: ClaimSource,
) -> Option<String> {
    let result = (|| {
        let Some(session) = state.session_store.get_session(session_id)? else {
            return Ok(None);
        };
        let sessions = session_directory(state)?;
        work_claim_store(state).claim_implicit(repo, pr, &session_info(&session), source, &sessions)
    })();
    match result {
        Ok(Some(warnings)) if !warnings.is_empty() => Some(warnings.join("\n")),
        Ok(_) => None,
        Err(error) => {
            eprintln!(
                "implicit claim of {repo} PR #{pr} for {session_id} ({}) failed: {error:#}",
                source.as_str()
            );
            None
        }
    }
}

/// `sm spawn --ticket`: the claim, reserved under the new session's id.
pub(super) struct SpawnTicketReservation {
    pub claim_id: String,
    pub notes: Vec<String>,
}

pub(super) struct SpawnTicket<'a> {
    pub session_id: &'a str,
    pub name: Option<&'a str>,
    pub parent: &'a SessionRecord,
    pub ticket: i64,
    pub repo: &'a str,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
}

pub(super) async fn reserve_spawn_ticket(
    state: &Arc<AppState>,
    spawn: SpawnTicket<'_>,
) -> Result<SpawnTicketReservation, ApiError> {
    let repo = validated_repo(spawn.repo)?;
    if spawn.ticket <= 0 {
        return Err(bad_request("ticket must be positive"));
    }
    let request = ClaimRequest {
        repo,
        number: spawn.ticket,
        kind: WorkKind::Ticket,
        claimant: SessionInfo {
            id: spawn.session_id.to_owned(),
            name: spawn
                .name
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(spawn.session_id)
                .to_owned(),
            parent_session_id: Some(spawn.parent.id.clone()),
            state: HolderState::Working,
            stopped_at: None,
        },
        source: ClaimSource::Spawn,
        take: false,
        worktree_path: spawn.worktree_path,
        branch: spawn.branch,
        tickets: Vec::new(),
        reserve: true,
    };
    let result = run_explicit_claim(state, request).await?;
    deliver_claim_notices(state, &result.notified);
    match result.outcome {
        ClaimOutcome::Claimed { claim, notes, .. } => Ok(SpawnTicketReservation {
            claim_id: claim.id,
            notes,
        }),
        ClaimOutcome::AlreadyHeld { .. } => Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: format!("Session {} already holds the ticket", spawn.session_id),
        }),
        outcome => Err(claim_failure(outcome)),
    }
}

pub(super) fn finish_spawn_ticket(
    state: &AppState,
    reservation: &SpawnTicketReservation,
    created: bool,
) -> Option<Value> {
    let store = work_claim_store(state);
    if created {
        if let Err(error) = store.confirm_reservation(&reservation.claim_id) {
            // Recovery confirms it within minutes: the session exists.
            eprintln!(
                "spawn claim {} confirmation failed: {error:#}",
                reservation.claim_id
            );
        }
        let claim = claim_json(state, &reservation.claim_id).ok()?;
        Some(json!({ "claim": claim, "notes": reservation.notes }))
    } else {
        if let Err(error) = store.delete_reservation(&reservation.claim_id) {
            eprintln!(
                "spawn claim {} cleanup failed: {error:#}",
                reservation.claim_id
            );
        }
        None
    }
}

/// Startup and every sync pass: backfill once, settle old spawn
/// reservations, end claims of retired sessions, reconcile implicit claims,
/// then fetch the tracked set from GitHub (appendices B, D, E).
pub(super) fn run_sync_pass(state: &AppState) -> anyhow::Result<()> {
    let store = work_claim_store(state);
    if !expand_home(&state.config.sm_send.db_path).exists() {
        return Ok(());
    }
    store.ensure_schema()?;
    let sessions = session_directory(state)?;
    store.backfill(&sessions)?;
    store.recover_reservations(|id| sessions.get(id).is_some())?;
    store.end_claims_of_retired_sessions(&sessions)?;
    store.reconcile_implicit(&sessions)?;
    for (repo, numbers) in store.tracked_items()? {
        for chunk in numbers.chunks(MAX_ALIASES_PER_QUERY) {
            let fetched = state.work_item_source.fetch(&repo, chunk);
            if let Err(error) = &fetched {
                eprintln!("work claims sync of {repo} failed: {error}");
            }
            store.record_fetch(&repo, chunk, &fetched)?;
        }
    }
    Ok(())
}

/// Schema at startup, then (live server only) the sync loop.
pub(super) fn init_work_claims(state: Arc<AppState>) {
    if expand_home(&state.config.sm_send.db_path).exists() {
        if let Err(error) = work_claim_store(&state).ensure_schema() {
            eprintln!("work claims schema initialization failed: {error:#}");
        }
    }
    if !state.config.rust_core.runtime_enabled {
        return;
    }
    let interval = state.config.work_claims.sync_interval();
    tokio::spawn(async move {
        loop {
            let pass_state = state.clone();
            match tokio::task::spawn_blocking(move || run_sync_pass(&pass_state)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("work claims sync pass failed: {error:#}"),
                Err(error) => eprintln!("work claims sync task failed: {error}"),
            }
            tokio::time::sleep(interval).await;
        }
    });
}
