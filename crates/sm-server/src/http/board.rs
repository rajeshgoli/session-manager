//! sm board over HTTP (sm#1665, ticket #1681): the board JSON for agents
//! and the app, link writes, lane adds, the owner's lane order, the Board
//! count, and the GitHub read loop (spec appendices C, E, F).

use super::*;
use crate::board::{
    self,
    model::{EdgeKind, Key, WaitingKind, WaitingRecord},
    sync::{
        self as board_sync, BoardSource, IssueConnection, IssuesPage, LinkMutation, RefNode,
        ResolvedIssue, WriteError,
    },
    BoardStore, JsonContext, LinkRequest, Outside, Refusal, Unseen, NOTICE_BOARD_LANE_DONE,
    NOTICE_BOARD_READY,
};
use crate::owner_docs::{doc_readable_path, OwnerDocState, OwnerDocStore};
use crate::owner_messages::{derive_message_state, message_reader_path, OwnerMessageState};
use crate::owner_settings::{NewAgentSettings, OwnerSettings, Ticket};

/// `gh api graphql` for the board.
#[derive(Debug)]
pub(super) struct GhCliBoardSource;

impl BoardSource for GhCliBoardSource {
    fn issues_page(&self, repo: &str, cursor: Option<&str>) -> Result<IssuesPage, String> {
        let stdout = claims::gh_graphql_stdout(&board_sync::issues_query(repo, cursor))?;
        board_sync::parse_issues_page(&stdout)
    }

    fn connection_page(
        &self,
        repo: &str,
        number: i64,
        connection: IssueConnection,
        cursor: &str,
    ) -> Result<(Vec<RefNode>, Option<String>), String> {
        let stdout = claims::gh_graphql_stdout(&board_sync::connection_query(
            repo, number, connection, cursor,
        ))?;
        board_sync::parse_connection_page(&stdout, connection)
    }

    fn items(&self, repo: &str, numbers: &[i64]) -> Result<BTreeMap<i64, Option<RefNode>>, String> {
        let stdout = claims::gh_graphql_stdout(&crate::work_claims::items_query(repo, numbers))?;
        let (batch, _) = crate::work_claims::parse_items_response(&stdout, numbers)?;
        Ok(board_sync::items_from_batch(repo, numbers, &batch))
    }

    fn resolve(&self, issues: &[Key]) -> Result<Vec<Option<ResolvedIssue>>, String> {
        let stdout = claims::gh_graphql_stdout(&board_sync::resolve_query(issues))?;
        board_sync::parse_resolve(&stdout, issues.len())
    }

    fn write_link(&self, mutation: &LinkMutation) -> Result<(), WriteError> {
        let stdout = claims::gh_graphql_stdout(&board_sync::mutation_query(mutation))
            .map_err(WriteError::Transport)?;
        let value: Value = serde_json::from_slice(&stdout)
            .map_err(|error| WriteError::Transport(format!("invalid GraphQL response: {error}")))?;
        match board_sync::graphql_error(&value) {
            Some(message) => Err(WriteError::Refused(message)),
            None => Ok(()),
        }
    }
}

/// Pass requests for the read loop (C1): at most one pass runs and one
/// waits; further requests fold into the waiting one.
#[derive(Default)]
pub(super) struct BoardWake {
    notify: tokio::sync::Notify,
    pass: AtomicBool,
    recompute: AtomicBool,
}

pub(super) fn board_store(state: &AppState) -> BoardStore {
    BoardStore::new(expand_home(&state.config.sm_send.db_path))
}

/// Asks the loop for a read pass.
pub(super) fn request_pass(state: &AppState) {
    state.board_wake.pass.store(true, Ordering::SeqCst);
    state.board_wake.notify.notify_one();
}

/// Asks the loop to recompute without reading GitHub: a message or review
/// changed, so a Needs-you row may have.
pub(super) fn request_recompute(state: &AppState) {
    wake_recompute(&state.board_wake);
}

pub(super) fn wake_recompute(wake: &BoardWake) {
    wake.recompute.store(true, Ordering::SeqCst);
    wake.notify.notify_one();
}

fn config_repos(config: &AppConfig) -> Vec<String> {
    config
        .board
        .repos
        .iter()
        .map(|repo| crate::work_claims::canonical_repo(repo))
        .filter(|repo| crate::owner_docs::validate_repo_slug(repo).is_ok())
        .collect()
}

/// The waiting-on-you records (D1): unanswered blocking messages, and docs
/// whose latest publish waits for the owner's review.
fn waiting_records(
    state: &AppState,
    sessions: &crate::work_claims::SessionDirectory,
) -> anyhow::Result<Vec<WaitingRecord>> {
    let mut records = Vec::new();
    let messages = super::messages::owner_message_store(state);
    let since = crate::owner_push::format_ts(time::OffsetDateTime::now_utc());
    for (message, replied) in messages.for_obligations(&since)? {
        if !message.blocking {
            continue;
        }
        let sender_ended = sessions
            .get(&message.sender_session_id)
            .is_none_or(|session| session.state == crate::work_claims::HolderState::Retired);
        if derive_message_state(&message, replied, sender_ended) != OwnerMessageState::NeedsYou {
            continue;
        }
        records.push(WaitingRecord {
            kind: WaitingKind::Message,
            session_id: message.sender_session_id.clone(),
            pr: None,
            text: message.title.clone(),
            url: message_reader_path(&message.id),
            created_at: message.created_at.clone(),
        });
    }
    let docs = OwnerDocStore::new(expand_home(&state.config.sm_send.db_path));
    for summary in docs.summaries(None, false)? {
        if summary.state != OwnerDocState::ReviewRequested {
            continue;
        }
        let Some(publish) = docs.publishes(&summary.doc.id)?.pop() else {
            continue;
        };
        let repo = crate::work_claims::canonical_repo(&summary.doc.repo);
        let text = match publish.pr_number {
            Some(pr) => format!("PR #{pr} waits for your review"),
            None => format!("{} waits for your review", summary.doc.title),
        };
        records.push(WaitingRecord {
            kind: WaitingKind::Review,
            session_id: publish.session_id.clone(),
            pr: publish.pr_number.map(|pr| (repo, pr)),
            text,
            url: doc_readable_path(&summary.doc.repo, &summary.doc.path, &publish.commit_sha),
            created_at: publish.published_at.clone(),
        });
    }
    Ok(records)
}

pub(super) fn outside(state: &AppState) -> anyhow::Result<Outside> {
    let sessions = claims::session_directory(state)?;
    let waiting = waiting_records(state, &sessions)?;
    Ok(Outside {
        sessions,
        waiting,
        owner_name: state.config.owner_name.clone(),
        config_repos: config_repos(&state.config),
    })
}

/// One read pass under the board lock.
pub(super) fn run_pass(state: &AppState) -> anyhow::Result<board::Recomputed> {
    let _guard = state
        .board_lock
        .lock()
        .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
    let recomputed = board::run_pass(
        &board_store(state),
        state.board_source.as_ref(),
        &outside(state)?,
        time::OffsetDateTime::now_utc(),
    )?;
    send_alerts(state, &recomputed);
    Ok(recomputed)
}

/// A recompute under the board lock.
pub(super) fn recompute(state: &AppState) -> anyhow::Result<board::Recomputed> {
    let _guard = state
        .board_lock
        .lock()
        .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
    let recomputed =
        board_store(state).recompute(&outside(state)?, time::OffsetDateTime::now_utc())?;
    send_alerts(state, &recomputed);
    Ok(recomputed)
}

/// Appendix I: a recompute's alerts become owner notices. Called under the
/// board lock after every recompute; a failure is logged and the board
/// still shows the change.
fn send_alerts(state: &AppState, recomputed: &board::Recomputed) {
    let owner = follows::follow_owner_id(&state.config, None);
    if let Err(error) = board::pushes::send(
        &board_store(state),
        &follows::push_store(state),
        &owner,
        recomputed,
        time::OffsetDateTime::now_utc(),
    ) {
        eprintln!("board alerts failed: {error:#}");
    }
}

/// Schema at startup, then (live server only) the read loop.
pub(super) fn init_board(state: Arc<AppState>) {
    if expand_home(&state.config.sm_send.db_path).exists() {
        if let Err(error) = board_store(&state).ensure_schema() {
            eprintln!("board schema initialization failed: {error:#}");
        }
    }
    if !state.config.rust_core.runtime_enabled {
        return;
    }
    let interval = state.config.board.sync_interval();
    tokio::spawn(async move {
        let mut next_pass = tokio::time::Instant::now();
        loop {
            let timed_out = tokio::select! {
                _ = tokio::time::sleep_until(next_pass) => true,
                _ = state.board_wake.notify.notified() => false,
            };
            let pass = state.board_wake.pass.swap(false, Ordering::SeqCst) || timed_out;
            let recompute_only = state.board_wake.recompute.swap(false, Ordering::SeqCst);
            if !pass && !recompute_only {
                continue;
            }
            if pass {
                next_pass = tokio::time::Instant::now() + interval;
            }
            let pass_state = state.clone();
            let result = tokio::task::spawn_blocking(move || {
                // No lanes and no configured repos: nothing to read or show.
                let idle = config_repos(&pass_state.config).is_empty()
                    && board_store(&pass_state).active_lanes()?.is_empty();
                if idle {
                    Ok(())
                } else if pass {
                    run_pass(&pass_state).map(|_| ())
                } else {
                    recompute(&pass_state).map(|_| ())
                }
            })
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("board pass failed: {error:#}"),
                Err(error) => eprintln!("board pass task failed: {error}"),
            }
        }
    });
}

/// The Board count (appendix I): unacked, unopened board notices plus
/// Needs-you tickets the owner has not seen.
fn unseen(
    state: &AppState,
    store: &BoardStore,
    board: &board::model::Board,
) -> anyhow::Result<Unseen> {
    let owner = follows::follow_owner_id(&state.config, None);
    let seen = store.seen(&owner)?;
    let seen_event_id = seen.as_ref().map_or(0, |(_, id)| *id);
    let mut unseen = board::needs_you_unseen(board, &store.needs_you_since()?, seen_event_id);
    let now = time::OffsetDateTime::now_utc();
    for notice in follows::push_store(state).list_notices(&owner, now)? {
        if notice.kind != NOTICE_BOARD_READY && notice.kind != NOTICE_BOARD_LANE_DONE {
            continue;
        }
        let opened =
            board::pushes::notice_opened(&notice.subject_id, &notice.created_at, seen.as_ref());
        if notice.acked_at.is_some() || opened {
            continue;
        }
        unseen.count += 1;
        if let Some((lane_id, _)) = board::pushes::subject_event(&notice.subject_id) {
            unseen.lane_ids.insert(lane_id);
        }
    }
    Ok(unseen)
}

/// The board JSON (appendix F), with each clocked ticket's clock over the
/// last `clock_hours` when given (sm#1710, D7).
pub(super) fn board_payload(
    state: &AppState,
    lane_filter: Option<&Key>,
    clock_hours: Option<i64>,
) -> anyhow::Result<Value> {
    let store = board_store(state);
    let now = time::OffsetDateTime::now_utc();
    let (board, input) = store.board(&outside(state)?, now)?;
    // A clock that cannot be computed leaves the board as it was.
    let clocks = clock_hours
        .map(|hours| super::board_clock::clocks(state, &board, &input, hours, now))
        .transpose()
        .unwrap_or_else(|error| {
            eprintln!("board clock failed: {error:#}");
            None
        })
        .unwrap_or_default();
    let unseen = unseen(state, &store, &board)?;
    let events = store.events(500)?;
    let repos = store.repo_syncs()?;
    let mut payload = board::board_json(
        &board,
        &input,
        &JsonContext {
            events: &events,
            repos: &repos,
            unseen: &unseen,
            start_defaults: start_defaults(state)?,
            lane_filter,
            now,
            clocks: &clocks,
        },
    );
    let early = store.started_early()?;
    let links = super::board_links::BoardLinks::load(state, &input)?;
    for lane in payload["lanes"].as_array_mut().into_iter().flatten() {
        for ticket in lane["tickets"].as_array_mut().into_iter().flatten() {
            mark_started_early(ticket, &early);
            links.attach(ticket);
        }
    }
    for group in payload["other"].as_array_mut().into_iter().flatten() {
        for ticket in group["tickets"].as_array_mut().into_iter().flatten() {
            mark_started_early(ticket, &early);
            links.attach(ticket);
        }
    }
    add_review_fields(state, &mut payload)?;
    Ok(payload)
}

fn add_review_fields(state: &AppState, payload: &mut Value) -> anyhow::Result<()> {
    let db = expand_home(&state.config.sm_send.db_path);
    let policies = crate::review::policy::list(&db)?;
    let mut by_key = BTreeMap::new();
    for policy in policies {
        by_key.insert(
            (
                policy["scope"].as_str().unwrap_or("").to_owned(),
                policy["repo"].as_str().unwrap_or("").to_owned(),
                policy["number"].as_i64().unwrap_or(0),
            ),
            policy,
        );
    }
    let requests = RetainedQueueStore::list_active_codex_review_requests_from_path(&db)?;
    let mut by_pr = BTreeMap::new();
    for request in requests {
        by_pr.insert((request.repo.clone(), request.pr_number), request);
    }
    let decorate_ticket = |ticket: &mut Value| {
        let repo = ticket["repo"].as_str().unwrap_or("");
        let number = ticket["number"].as_i64().unwrap_or(0);
        ticket["review_policy"] = by_key
            .get(&("ticket".into(), repo.to_owned(), number))
            .map(|p| {
                json!({"reviewer":p["reviewer"],"fallback":p["fallback"],
                "set_by_name":p["set_by_name"],"set_at":p["set_at"]})
            })
            .unwrap_or(Value::Null);
        ticket["review"] = ticket["prs"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|pr| by_pr.get(&(pr["repo"].as_str()?.to_owned(), pr["number"].as_i64()?)))
            .map(|r| {
                json!({"state":r.state,"reviewer_label":r.reviewer_label,
            "since":r.step_started_at.as_deref().unwrap_or(&r.requested_at)})
            })
            .unwrap_or(Value::Null);
    };
    for lane in payload["lanes"].as_array_mut().into_iter().flatten() {
        let repo = lane["goal"]["repo"].as_str().unwrap_or("");
        let number = lane["goal"]["number"].as_i64().unwrap_or(0);
        lane["review_policy"] = by_key
            .get(&("lane".into(), repo.to_owned(), number))
            .map(|p| {
                json!({"reviewer":p["reviewer"],"fallback":p["fallback"],
                "set_by_name":p["set_by_name"],"set_at":p["set_at"]})
            })
            .unwrap_or(Value::Null);
        for ticket in lane["tickets"].as_array_mut().into_iter().flatten() {
            decorate_ticket(ticket);
        }
    }
    for group in payload["other"].as_array_mut().into_iter().flatten() {
        for ticket in group["tickets"].as_array_mut().into_iter().flatten() {
            decorate_ticket(ticket);
        }
    }
    Ok(())
}

fn mark_started_early(ticket: &mut Value, early: &BTreeSet<Key>) {
    let key = (
        ticket["repo"].as_str().unwrap_or_default().to_owned(),
        ticket["number"].as_i64().unwrap_or_default(),
    );
    ticket["started_early"] = json!(ticket["state"] != "done" && early.contains(&key));
}

async fn blocking<T: Send + 'static>(
    state: &Arc<AppState>,
    work: impl FnOnce(&AppState) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || work(&state))
        .await
        .map_err(|error| ApiError::Internal(anyhow::anyhow!("board task failed: {error}")))?
}

fn refusal_error(refusal: Refusal, lane: Option<Value>) -> ApiError {
    let status = match &refusal {
        Refusal::NotFound(_) => StatusCode::NOT_FOUND,
        Refusal::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
        Refusal::Conflict(..) => StatusCode::CONFLICT,
        Refusal::Unreachable(_) => StatusCode::BAD_GATEWAY,
    };
    let mut body = json!({ "detail": refusal.detail() });
    if let Some(lane) = lane {
        body["lane"] = lane;
    }
    ApiError::StatusBody { status, body }
}

fn bad_request(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: detail.into(),
    }
}

fn ticket_key(repo: &str, number: i64) -> Result<Key, ApiError> {
    let repo = crate::work_claims::canonical_repo(repo);
    crate::owner_docs::validate_repo_slug(&repo).map_err(|error| bad_request(error.to_string()))?;
    if number <= 0 {
        return Err(bad_request("numbers must be positive"));
    }
    Ok((repo, number))
}

/// Who a write is from: `sm:<session id>` and its name, or the owner.
fn actor(state: &AppState, session_id: Option<&str>) -> Result<(String, String), ApiError> {
    match session_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(session_id) => {
            let session = state
                .session_store
                .get_session(session_id)?
                .ok_or_else(|| bad_request(format!("Session {session_id} not found")))?;
            Ok((
                format!("sm:{}", session.id),
                claims::session_info(&session).name,
            ))
        }
        None => Ok(("sm:owner".to_owned(), state.config.owner_name.clone())),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct BoardQuery {
    #[serde(default)]
    lane: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    clock_hours: Option<i64>,
}

fn lane_filter(query: &BoardQuery) -> Result<Option<Key>, ApiError> {
    match query
        .lane
        .as_deref()
        .map(str::trim)
        .filter(|lane| !lane.is_empty())
    {
        Some(lane) => board::parse_ticket_ref(lane, "")
            .map(Some)
            .ok_or_else(|| bad_request("lane must be owner/name#N")),
        None => Ok(None),
    }
}

/// `GET /board`: the web app on the browser hostname, or board JSON when
/// explicitly requested.
pub(super) async fn get_board(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Query(query): Query<BoardQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    let wants_json = query.format.as_deref() == Some("json")
        || request
            .headers()
            .get("accept")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("application/json"));
    if wants_json {
        ensure_session_allowed_from_parts(
            &state.config,
            request.headers(),
            Some(peer_addr),
            "/board",
        )?;
    } else {
        ensure_owner_page_read_allowed(&state, &request)?;
    }
    if let Some(shell) = super::web::shell_page(&state, &request) {
        return Ok(shell);
    }
    // The Board page is the web app's; the phone shows its own Board tab.
    if !wants_json {
        return Err(ApiError::NotFound("Not found"));
    }
    let filter = lane_filter(&query)?;
    let hours = super::board_clock::clock_hours(query.clock_hours)?;
    let payload = blocking(&state, move |state| {
        Ok(board_payload(state, filter.as_ref(), Some(hours))?)
    })
    .await?;
    Ok(Json(payload).into_response())
}

#[derive(Debug, Deserialize)]
pub(super) struct PostLinkRequest {
    repo: String,
    number: i64,
    target_repo: String,
    target_number: i64,
    kind: String,
    #[serde(default)]
    remove: bool,
    #[serde(default)]
    session_id: Option<String>,
}

/// `POST /board/links` (C5).
pub(super) async fn post_link(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<PostLinkRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), "/board/links")?;
    let kind = match payload.kind.trim() {
        "after" => EdgeKind::After,
        "under" => EdgeKind::SubIssue,
        _ => return Err(bad_request("kind must be after or under")),
    };
    let ticket = ticket_key(&payload.repo, payload.number)?;
    let target = ticket_key(&payload.target_repo, payload.target_number)?;
    let (actor, actor_name) = actor(&state, payload.session_id.as_deref())?;
    let request = LinkRequest {
        ticket,
        target,
        kind,
        remove: payload.remove,
        actor,
        actor_name,
    };
    let body = blocking(&state, move |state| {
        let outcome = {
            let _guard = state
                .board_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
            let store = board_store(state);
            let outcome = board::write_link(
                &store,
                state.board_source.as_ref(),
                &request,
                time::OffsetDateTime::now_utc(),
            )?
            .map_err(|refusal| refusal_error(refusal, None))?;
            let recomputed = store.recompute(&outside(state)?, time::OffsetDateTime::now_utc())?;
            send_alerts(state, &recomputed);
            outcome
        };
        request_pass(state);
        Ok(json!({
            "outcome": outcome.as_str(),
            "message": board::link_message(&request, outcome),
        }))
    })
    .await?;
    Ok(Json(body))
}

#[derive(Debug, Deserialize)]
pub(super) struct PostLaneRequest {
    repo: String,
    number: i64,
    #[serde(default)]
    session_id: Option<String>,
}

/// D4: checks the goal, adds the lane at the bottom, and reads GitHub so
/// the lane's first members are its members as planned, not new ones.
fn add_lane(
    state: &AppState,
    goal: Key,
    added_by: String,
    added_by_name: String,
) -> Result<Value, ApiError> {
    let store = board_store(state);
    let lane_id = {
        let _guard = state
            .board_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        let now = time::OffsetDateTime::now_utc();
        store.ensure_schema()?;
        if let Err(refusal) = board::check_goal(&store, state.board_source.as_ref(), &goal, now)? {
            return Err(refusal_error(refusal, None));
        }
        let lane_id = match store.add_lane(&goal, &added_by, &added_by_name, now)? {
            Ok(lane_id) => lane_id,
            Err(refusal) => {
                let lane = match &refusal {
                    Refusal::Conflict(_, Some(id)) => {
                        Some(board::lane_json(&board_payload(state, None, None)?, *id))
                    }
                    _ => None,
                };
                return Err(refusal_error(refusal, lane));
            }
        };
        let recomputed = board::run_pass(
            &store,
            state.board_source.as_ref(),
            &outside(state)?,
            time::OffsetDateTime::now_utc(),
        )?;
        send_alerts(state, &recomputed);
        lane_id
    };
    let lane = board::lane_json(&board_payload(state, None, None)?, lane_id);
    let title = lane["goal"]["title"].as_str().unwrap_or_default();
    let message = format!(
        "Lane {}: {}#{} {title}. Added at the bottom; {} sets the order.",
        lane["rank"], goal.0, goal.1, state.config.owner_name
    );
    Ok(json!({ "lane": lane, "message": message }))
}

/// `POST /board/lanes`: an agent adds a lane.
pub(super) async fn post_lane(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<PostLaneRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), "/board/lanes")?;
    let goal = ticket_key(&payload.repo, payload.number)?;
    let (added_by, added_by_name) = match payload.session_id.as_deref() {
        Some(session_id) if !session_id.trim().is_empty() => {
            let (actor, name) = actor(&state, Some(session_id))?;
            (actor.trim_start_matches("sm:").to_owned(), name)
        }
        _ => ("owner".to_owned(), state.config.owner_name.clone()),
    };
    let body = blocking(&state, move |state| {
        add_lane(state, goal, added_by, added_by_name)
    })
    .await?;
    Ok(Json(body))
}

/// Board and its model picker accept either verified browser-owner login or
/// the existing mobile owner guard. Browser writes also require same-origin.
pub(super) fn owner_guard(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    method: &str,
    uri: &Uri,
    signed_write: bool,
) -> Result<String, ApiError> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    *request.headers_mut() = headers.clone();
    request.extensions_mut().insert(ConnectInfo(peer_addr));
    if request_cloudflare_access_application(state, &request)
        == Some(CloudflareAccessApplication::Browser)
        && !is_request_local_bypass(state, &request)
    {
        let assertion =
            header_text(headers, "cf-access-jwt-assertion").ok_or_else(|| ApiError::Status {
                status: StatusCode::FORBIDDEN,
                detail: "Browser owner login required".into(),
            })?;
        let context = classify_cloudflare_access_assertion_cached(
            state,
            CloudflareAccessApplication::Browser,
            &assertion,
        )
        .map_err(cloudflare_access_error)?;
        let email = browser_owner_email(state, &context)?.ok_or_else(|| ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Owner login required".into(),
        })?;
        if method != "GET" {
            if header_text(headers, handoff::SESSION_HEADER).is_some() {
                return Err(ApiError::Status {
                    status: StatusCode::FORBIDDEN,
                    detail: "Board actions are owner-only".into(),
                });
            }
            let origin = header_text(headers, "origin");
            let host =
                header_text(headers, "x-forwarded-host").or_else(|| header_text(headers, "host"));
            if !origin
                .as_deref()
                .is_some_and(|origin| handoff::origin_matches_host(origin, host.as_deref()))
            {
                return Err(ApiError::Status {
                    status: StatusCode::FORBIDDEN,
                    detail: "Origin does not match host".into(),
                });
            }
        }
        return Ok(follows::follow_owner_id(&state.config, Some(&email)));
    }
    let owner = follows::owner_guard(state, headers, peer_addr, method, uri)?;
    if signed_write && authenticated_user(headers, &state.config).is_none() {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Only the owner, signed in to sm, changes the board or starts agents"
                .to_owned(),
        });
    }
    Ok(owner)
}

fn owner_write_guard(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    method: &str,
    uri: &Uri,
) -> Result<(), ApiError> {
    owner_guard(state, headers, peer_addr, method, uri, true).map(|_| ())
}

/// `GET /client/board`: the board JSON, for the app.
pub(super) async fn client_board(
    State(state): State<Arc<AppState>>,
    Query(query): Query<BoardQuery>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner_guard(&state, &headers, peer_addr, "GET", &uri, false)?;
    let hours = super::board_clock::clock_hours(query.clock_hours)?;
    let payload = blocking(&state, move |state| {
        Ok(board_payload(state, None, Some(hours))?)
    })
    .await?;
    Ok(Json(payload))
}

#[derive(Debug, Deserialize)]
pub(super) struct OrderRequest {
    lane_ids: Vec<i64>,
}

/// `PUT /client/board/order`.
pub(super) async fn put_order(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(payload): Json<OrderRequest>,
) -> Result<Json<Value>, ApiError> {
    owner_write_guard(&state, &headers, peer_addr, "PUT", &uri)?;
    let body = blocking(&state, move |state| {
        {
            let _guard = state
                .board_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
            board_store(state)
                .reorder(
                    &payload.lane_ids,
                    &state.config.owner_name,
                    time::OffsetDateTime::now_utc(),
                )?
                .map_err(|refusal| refusal_error(refusal, None))?;
        }
        Ok(board_payload(
            state,
            None,
            Some(crate::board::clock::DEFAULT_CLOCK_HOURS),
        )?)
    })
    .await?;
    Ok(Json(body))
}

#[derive(Debug, Deserialize)]
pub(super) struct ClientLaneRequest {
    repo: String,
    number: i64,
}

/// `POST /client/board/lanes`: the owner adds a lane.
pub(super) async fn client_post_lane(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(payload): Json<ClientLaneRequest>,
) -> Result<Json<Value>, ApiError> {
    owner_write_guard(&state, &headers, peer_addr, "POST", &uri)?;
    let goal = ticket_key(&payload.repo, payload.number)?;
    let body = blocking(&state, move |state| {
        let name = state.config.owner_name.clone();
        add_lane(state, goal, "owner".to_owned(), name)
    })
    .await?;
    Ok(Json(body))
}

/// `DELETE /client/board/lanes/{id}`: the owner ends a lane.
pub(super) async fn client_delete_lane(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Path(lane_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    owner_write_guard(&state, &headers, peer_addr, "DELETE", &uri)?;
    let body = blocking(&state, move |state| {
        let ended = {
            let _guard = state
                .board_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
            let store = board_store(state);
            let ended = store.end_lane(lane_id, time::OffsetDateTime::now_utc())?;
            if ended {
                let recomputed =
                    store.recompute(&outside(state)?, time::OffsetDateTime::now_utc())?;
                send_alerts(state, &recomputed);
            }
            ended
        };
        if !ended {
            return Err(ApiError::NotFound("Lane not active"));
        }
        Ok(board_payload(
            state,
            None,
            Some(crate::board::clock::DEFAULT_CLOCK_HOURS),
        )?)
    })
    .await?;
    Ok(Json(body))
}

/// `GET /client/board/badge`: the Board count.
pub(super) async fn client_board_badge(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner_guard(&state, &headers, peer_addr, "GET", &uri, false)?;
    let count = blocking(&state, |state| {
        let store = board_store(state);
        let (board, _) = store.board(&outside(state)?, time::OffsetDateTime::now_utc())?;
        Ok(unseen(state, &store, &board)?.count)
    })
    .await?;
    Ok(Json(json!({ "count": count })))
}

/// `POST /client/board/seen`: the owner saw the board. Moves `seen_at`
/// and acks every unacked board notice.
pub(super) async fn client_board_seen(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let owner = owner_guard(&state, &headers, peer_addr, "POST", &uri, false)?;
    blocking(&state, move |state| {
        let now = time::OffsetDateTime::now_utc();
        board_store(state).set_seen(&owner, now)?;
        let push = follows::push_store(state);
        for notice in push.list_notices(&owner, now)? {
            if (notice.kind == NOTICE_BOARD_READY || notice.kind == NOTICE_BOARD_LANE_DONE)
                && notice.acked_at.is_none()
            {
                push.ack_notice(&owner, &notice.id, now)?;
            }
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /client/board/refresh`: a read pass now.
pub(super) async fn client_board_refresh(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    owner_guard(&state, &headers, peer_addr, "POST", &uri, false)?;
    request_pass(&state);
    Ok(StatusCode::ACCEPTED)
}

/// Resolve once on the server so both the model catalog and Start use the same checkout.
fn checkout(config: &AppConfig, repo: &str) -> Result<String, ApiError> {
    checkout_in(config, repo, &expand_home("~/projects"))
}

fn checkout_in(
    config: &AppConfig,
    repo: &str,
    projects: &std::path::Path,
) -> Result<String, ApiError> {
    if let Some(path) = config.board.checkouts.get(repo) {
        return Ok(expand_home(path).to_string_lossy().into_owned());
    }
    let path = projects.join(repo.rsplit('/').next().unwrap_or(repo));
    let path = path.to_string_lossy().into_owned();
    if git_origin_github_repo(&path).as_deref() == Some(repo) {
        return Ok(path);
    }
    Err(ApiError::Status {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        detail: format!(
            "sm doesn't know where {repo} is checked out; set board.checkouts in config"
        ),
    })
}

#[derive(Debug, Deserialize)]
pub(super) struct StartOptionsQuery {
    repo: String,
    number: i64,
    #[serde(default)]
    start_blocked: bool,
}

fn new_agent_settings(state: &AppState) -> anyhow::Result<NewAgentSettings> {
    let settings = state.session_store.owner_settings()?;
    Ok(OwnerSettings::from_effective(&settings)?.new_agent)
}

/// The board payload's `start_defaults`, from owner settings. A provider
/// default effort is left out rather than sent as null: the phone reads the
/// field as a string and falls back to its own default when it is absent.
fn start_defaults(state: &AppState) -> anyhow::Result<Value> {
    let settings = new_agent_settings(state)?;
    let defaults = settings.provider_defaults();
    let mut value = json!({ "provider": settings.provider, "model": defaults.model });
    if let Some(effort) = &defaults.effort {
        value["reasoning_effort"] = json!(effort);
    }
    Ok(value)
}

/// What Start fills in (spec 1710 appendix D4): the checkout, and the
/// provider, model, effort, name and first message from owner settings. A
/// null model or effort leaves the choice to the provider.
fn start_options_payload(
    state: &AppState,
    key: &Key,
    start_blocked: bool,
) -> Result<Value, ApiError> {
    let input = board_store(state).input(&outside(state)?)?;
    let item = input
        .items
        .get(key)
        .ok_or(ApiError::NotFound("Ticket not on the board"))?;
    let settings = new_agent_settings(state)?;
    let mut options = start_options_json(
        &settings,
        Ticket {
            repo: &key.0,
            number: key.1,
            title: &item.title,
            url: &item.url,
        },
        checkout(&state.config, &key.0)?,
    );
    if start_blocked {
        let blockers: Vec<i64> = input
            .edges
            .iter()
            .filter(|edge| {
                &edge.waiter == key
                    && input
                        .items
                        .get(&edge.blocker)
                        .is_some_and(|item| item.is_open())
            })
            .map(|edge| edge.blocker.1)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if !blockers.is_empty() {
            let names: Vec<String> = blockers.iter().map(|number| format!("#{number}")).collect();
            let list = match names.as_slice() {
                [one] => one.clone(),
                [first, second] => format!("{first} and {second}"),
                _ => format!(
                    "{} and {}",
                    names[..names.len() - 1].join(", "),
                    names.last().unwrap()
                ),
            };
            let plural = blockers.len() > 1;
            let paragraph = format!("Started early by {}. {list} {} not done yet: check what {} delivered so far and build on it; do not redo {} work.",
                state.config.owner_name, if plural { "are" } else { "is" },
                if plural { "they have" } else { "it has" },
                if plural { "their" } else { "its" });
            let brief = options["brief"].as_str().unwrap_or_default();
            options["brief"] = json!(format!("{brief}\n\n{paragraph}"));
            options["early_start_paragraph"] = json!(paragraph);
        }
    }
    Ok(options)
}

pub(super) fn start_options_json(
    settings: &NewAgentSettings,
    ticket: Ticket<'_>,
    working_dir: String,
) -> Value {
    let defaults = settings.provider_defaults();
    json!({
        "working_dir": working_dir,
        "name": settings.agent_name(ticket),
        "brief": settings.brief(ticket),
        "provider": settings.provider,
        "model": defaults.model,
        "reasoning_effort": defaults.effort,
    })
}

pub(super) async fn start_options(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Query(query): Query<StartOptionsQuery>,
) -> Result<Json<Value>, ApiError> {
    owner_guard(&state, &headers, peer_addr, "GET", &uri, false)?;
    let key = ticket_key(&query.repo, query.number)?;
    let start_blocked = query.start_blocked;
    Ok(Json(
        blocking(&state, move |state| {
            start_options_payload(state, &key, start_blocked)
        })
        .await?,
    ))
}

#[derive(Debug, Deserialize)]
pub(super) struct StartRequest {
    repo: String,
    number: i64,
    provider: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    name: Option<String>,
    brief: Option<String>,
    #[serde(default)]
    start_blocked: bool,
}

pub(super) async fn client_start(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(payload): Json<StartRequest>,
) -> Result<Json<Value>, ApiError> {
    owner_write_guard(&state, &headers, peer_addr, "POST", &uri)?;
    ensure_core_writes_enabled(&state)?;
    start(state, payload).await.map(Json)
}

#[derive(Debug, Deserialize)]
pub(super) struct CloseRequest {
    repo: String,
    number: i64,
}

pub(super) async fn client_close(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(payload): Json<CloseRequest>,
) -> Result<Json<Value>, ApiError> {
    owner_write_guard(&state, &headers, peer_addr, "POST", &uri)?;
    ensure_core_writes_enabled(&state)?;
    let key = ticket_key(&payload.repo, payload.number)?;
    blocking(&state, move |state| {
        let _guard = state
            .board_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        let (board, _) =
            board_store(state).board(&outside(state)?, time::OffsetDateTime::now_utc())?;
        let facts = board
            .facts
            .get(&key)
            .ok_or(ApiError::NotFound("Ticket not on the board"))?;
        if facts.state != crate::board::model::TicketState::CloseReady {
            return Err(ApiError::Status {
                status: StatusCode::CONFLICT,
                detail: format!("#{} is not ready to close; refresh the board", key.1),
            });
        }
        let refs = facts
            .sub_issues
            .iter()
            .map(|child| format!("#{}", child.1))
            .collect::<Vec<_>>()
            .join(", ");
        let comment = format!(
            "All {} sub-issues are done: {refs}. Closed from the sm board.",
            facts.sub_issues.len()
        );
        let args = vec![
            "issue".into(),
            "close".into(),
            key.1.to_string(),
            "-R".into(),
            key.0.clone(),
            "--comment".into(),
            comment,
        ];
        let output = gh_command_output(&args, Duration::from_secs(30)).map_err(|detail| {
            ApiError::Status {
                status: StatusCode::BAD_GATEWAY,
                detail,
            }
        })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("GitHub close failed")
                .to_owned();
            return Err(ApiError::Status {
                status: StatusCode::BAD_GATEWAY,
                detail,
            });
        }
        request_pass(state);
        Ok(Json(json!({"closed": true})))
    })
    .await
}

/// Called again under the board lock immediately before reserving the claim.
pub(super) struct StartValidation {
    pub lanes: Vec<i64>,
    pub started_early: bool,
}

pub(super) fn validate_start(
    state: &AppState,
    key: &Key,
    start_blocked: bool,
) -> Result<StartValidation, ApiError> {
    let (board, input) =
        board_store(state).board(&outside(state)?, time::OffsetDateTime::now_utc())?;
    let item = input
        .items
        .get(key)
        .ok_or(ApiError::NotFound("Ticket not on the board"))?;
    if !item.is_open() {
        return Err(ApiError::Status {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            detail: format!("#{} is closed", key.1),
        });
    }
    if let Some(facts) = board.facts.get(key) {
        if facts.needs_you.is_some() {
            return Err(ApiError::Status {
                status: StatusCode::CONFLICT,
                detail: format!("#{} is waiting on you", key.1),
            });
        }
        if let Some(holder) = &facts.holder {
            return Err(ApiError::Status {
                status: StatusCode::CONFLICT,
                detail: format!(
                    "#{} is held by {} ({})",
                    key.1,
                    holder.name,
                    holder.state.as_str()
                ),
            });
        }
    }
    let facts = board
        .facts
        .get(key)
        .ok_or(ApiError::NotFound("Ticket not on the board"))?;
    let startable = matches!(
        facts.state,
        crate::board::model::TicketState::Ready | crate::board::model::TicketState::CloseReady
    ) || (start_blocked
        && facts.state == crate::board::model::TicketState::Blocked
        && !facts.warnings.contains(&"stale")
        && !facts.warnings.contains(&"cycle"));
    if !startable || facts.warnings.contains(&"merged_not_closed") {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: format!("#{} is no longer ready to start; refresh the board", key.1),
        });
    }
    let lanes: Vec<i64> = board
        .lanes
        .iter()
        .filter(|lane| lane.contains(key))
        .map(|lane| lane.lane.id)
        .collect();
    Ok(StartValidation {
        lanes,
        started_early: facts.state == crate::board::model::TicketState::Blocked,
    })
}

async fn start(state: Arc<AppState>, payload: StartRequest) -> Result<Value, ApiError> {
    if !matches!(payload.provider.as_str(), "claude" | "codex-fork") {
        return Err(bad_request("provider must be claude or codex-fork"));
    }
    let key = ticket_key(&payload.repo, payload.number)?;
    let checked_key = key.clone();
    let (options, lanes) = blocking(&state, move |state| {
        let validation = validate_start(state, &checked_key, payload.start_blocked)?;
        Ok((
            start_options_payload(state, &checked_key, false)?,
            validation.lanes,
        ))
    })
    .await?;
    let id = state.session_store.allocate_session_id()?;
    let name = trimmed(&payload.name)
        .unwrap_or_else(|| options["name"].as_str().unwrap_or_default().to_owned());
    let reservation = claims::reserve_spawn_ticket(&state, claims::SpawnTicket {
        session_id: &id, check_board: true, start_blocked: payload.start_blocked, name: Some(&name), parent: None, ticket: key.1, repo: &key.0,
        worktree_path: None, branch: None,
    }).await.map_err(|error| match error {
        ApiError::StatusBody { status: StatusCode::CONFLICT, body } => ApiError::StatusBody {
            status: StatusCode::CONFLICT,
            body: json!({"detail": format!("#{} was claimed while starting; refresh the board", key.1), "holders": body["holders"]}),
        },
        other => other,
    })?;
    let options = if reservation.started_early {
        let early_key = key.clone();
        match blocking(&state, move |state| {
            start_options_payload(state, &early_key, true)
        })
        .await
        {
            Ok(options) => options,
            Err(error) => {
                claims::finish_spawn_ticket(&state, &reservation, false);
                return Err(error);
            }
        }
    } else {
        options
    };
    let created = create_session_from_request(
        state.clone(),
        CreateCoreSessionRequest {
            id: Some(id),
            name: Some(name.clone()),
            working_dir: options["working_dir"].as_str().map(str::to_owned),
            provider: Some(payload.provider),
            // Absent or blank leaves the choice to the provider.
            model: crate::config::trimmed(&payload.model),
            reasoning_effort: crate::config::trimmed(&payload.reasoning_effort),
            initial_message: Some(match trimmed(&payload.brief) {
                Some(brief) if reservation.started_early => format!(
                    "{brief}\n\n{}",
                    options["early_start_paragraph"]
                        .as_str()
                        .unwrap_or_default()
                ),
                Some(brief) => brief,
                None => options["brief"].as_str().unwrap_or_default().to_owned(),
            }),
            parent_session_id: None,
            node: None,
            wait: None,
            spawn_prompt_source: None,
            spawn_brief: None,
        },
    )
    .await;
    claims::finish_spawn_ticket(&state, &reservation, created.is_ok());
    let session = created?;
    if reservation.started_early {
        board_store(&state).record_started_early(&key, time::OffsetDateTime::now_utc())?;
    }
    // Session creation is committed. A reporting failure must not invite a duplicate Start.
    for lane in lanes {
        if let Err(error) = board::record_event(
            &board_store(&state),
            "agent_started",
            Some(lane),
            Some(&key),
            Some((&session.id, &name)),
            Some(&name),
            time::OffsetDateTime::now_utc(),
        ) {
            eprintln!("board agent_started event failed: {error:#}");
        }
    }
    request_pass(&state);
    Ok(json!({"session_id": session.id, "name": name}))
}

#[cfg(test)]
mod start_tests {
    use super::*;

    #[test]
    fn start_resolves_checkout() {
        let root = std::env::temp_dir().join(format!(
            "board-checkout-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.as_path().join("widgets");
        std::fs::create_dir(&path).unwrap();
        let mut config = AppConfig::default();
        assert!(checkout_in(&config, "acme/widgets", root.as_path()).is_err());
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&path)
            .status()
            .unwrap()
            .success());
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["remote", "add", "origin", "git@github.com:acme/widgets.git"])
            .status()
            .unwrap()
            .success());
        assert_eq!(
            checkout_in(&config, "acme/widgets", root.as_path()).unwrap(),
            path.to_string_lossy()
        );
        assert!(checkout_in(&config, "other/widgets", root.as_path()).is_err());
        config.board.checkouts.insert(
            "acme/widgets".into(),
            root.as_path().to_string_lossy().into_owned(),
        );
        assert_eq!(
            checkout_in(&config, "acme/widgets", root.as_path()).unwrap(),
            root.as_path().to_string_lossy()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
