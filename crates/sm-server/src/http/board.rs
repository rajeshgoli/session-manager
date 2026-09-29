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
    state.board_wake.recompute.store(true, Ordering::SeqCst);
    state.board_wake.notify.notify_one();
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
    board::run_pass(
        &board_store(state),
        state.board_source.as_ref(),
        &outside(state)?,
        time::OffsetDateTime::now_utc(),
    )
}

/// A recompute under the board lock.
pub(super) fn recompute(state: &AppState) -> anyhow::Result<board::Recomputed> {
    let _guard = state
        .board_lock
        .lock()
        .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
    board_store(state).recompute(&outside(state)?, time::OffsetDateTime::now_utc())
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
        // `board:{lane}:{event}`: the event id orders it against the seen.
        let event_id = notice
            .subject_id
            .rsplit(':')
            .next()
            .and_then(|id| id.parse::<i64>().ok());
        let opened = match (event_id, &seen) {
            (Some(event_id), Some(_)) => event_id <= seen_event_id,
            (None, Some((seen_at, _))) => seen_at.as_str() > notice.created_at.as_str(),
            (_, None) => false,
        };
        if notice.acked_at.is_some() || opened {
            continue;
        }
        unseen.count += 1;
        if let Some(lane_id) = notice
            .subject_id
            .strip_prefix("board:")
            .and_then(|rest| rest.split(':').next())
            .and_then(|id| id.parse().ok())
        {
            unseen.lane_ids.insert(lane_id);
        }
    }
    Ok(unseen)
}

/// The board JSON (appendix F).
pub(super) fn board_payload(state: &AppState, lane_filter: Option<&Key>) -> anyhow::Result<Value> {
    let store = board_store(state);
    let now = time::OffsetDateTime::now_utc();
    let (board, input) = store.board(&outside(state)?, now)?;
    let unseen = unseen(state, &store, &board)?;
    let events = store.events(500)?;
    let repos = store.repo_syncs()?;
    Ok(board::board_json(
        &board,
        &input,
        &JsonContext {
            events: &events,
            repos: &repos,
            unseen: &unseen,
            start_defaults: serde_json::to_value(&state.config.board.start_defaults)?,
            lane_filter,
            now,
        },
    ))
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
    #[allow(dead_code)] // The page (ticket #1683) answers requests without it.
    format: Option<String>,
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

/// `GET /board`: the board JSON.
pub(super) async fn get_board(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<BoardQuery>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), "/board")?;
    let filter = lane_filter(&query)?;
    let payload = blocking(&state, move |state| {
        Ok(board_payload(state, filter.as_ref())?)
    })
    .await?;
    Ok(Json(payload))
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
            store.recompute(&outside(state)?, time::OffsetDateTime::now_utc())?;
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
                        Some(board::lane_json(&board_payload(state, None)?, *id))
                    }
                    _ => None,
                };
                return Err(refusal_error(refusal, lane));
            }
        };
        board::run_pass(
            &store,
            state.board_source.as_ref(),
            &outside(state)?,
            time::OffsetDateTime::now_utc(),
        )?;
        lane_id
    };
    let lane = board::lane_json(&board_payload(state, None)?, lane_id);
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

/// The owner's write guard: a signed-in owner, as Start now on the queue.
fn owner_write_guard(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    method: &str,
    uri: &Uri,
) -> Result<(), ApiError> {
    follows::owner_guard(state, headers, peer_addr, method, uri)?;
    if authenticated_user(headers, &state.config).is_none() {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Only the owner, signed in to sm, orders or ends lanes".to_owned(),
        });
    }
    Ok(())
}

/// `GET /client/board`: the board JSON, for the app.
pub(super) async fn client_board(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    follows::owner_guard(&state, &headers, peer_addr, "GET", &uri)?;
    let payload = blocking(&state, |state| Ok(board_payload(state, None)?)).await?;
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
        Ok(board_payload(state, None)?)
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
                store.recompute(&outside(state)?, time::OffsetDateTime::now_utc())?;
            }
            ended
        };
        if !ended {
            return Err(ApiError::NotFound("Lane not active"));
        }
        Ok(board_payload(state, None)?)
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
    follows::owner_guard(&state, &headers, peer_addr, "GET", &uri)?;
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
    let owner = follows::owner_guard(&state, &headers, peer_addr, "POST", &uri)?;
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
    follows::owner_guard(&state, &headers, peer_addr, "POST", &uri)?;
    request_pass(&state);
    Ok(StatusCode::ACCEPTED)
}
