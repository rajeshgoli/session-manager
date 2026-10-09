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
    BoardStore, JsonContext, LinkRequest, Outside, Refusal, Unseen, NOTICE_BOARD_AUTO_START,
    NOTICE_BOARD_LANE_DONE, NOTICE_BOARD_READY,
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

    fn create_issue(&self, repo: &str, title: &str, body: &str) -> Result<(i64, String), String> {
        let args: Vec<String> = [
            "issue",
            "create",
            "-R",
            repo,
            "--title",
            title,
            "--body-file",
            "-",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let output = gh_command_output_with_input(&args, body.as_bytes(), Duration::from_secs(30))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr)
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("gh issue create failed")
                .to_owned());
        }
        super::bugs::parse_created_issue(&String::from_utf8_lossy(&output.stdout))
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
    run_pass_inner(state, true)
}

fn run_pass_inner(state: &AppState, alerts: bool) -> anyhow::Result<board::Recomputed> {
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
    if alerts {
        send_alerts(state, &recomputed);
    }
    Ok(recomputed)
}

/// A recompute under the board lock.
fn recompute_inner(state: &AppState, alerts: bool) -> anyhow::Result<board::Recomputed> {
    let _guard = state
        .board_lock
        .lock()
        .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
    let recomputed =
        board_store(state).recompute(&outside(state)?, time::OffsetDateTime::now_utc())?;
    if alerts {
        send_alerts(state, &recomputed);
    }
    Ok(recomputed)
}

/// Appendix I: a recompute's alerts become owner notices. Called under the
/// board lock after every recompute; a failure is logged and the board
/// still shows the change.
pub(super) fn send_alerts(state: &AppState, recomputed: &board::Recomputed) {
    send_alerts_excluding(state, recomputed, &BTreeSet::new());
}

fn send_alerts_excluding(
    state: &AppState,
    recomputed: &board::Recomputed,
    excluded: &BTreeSet<Key>,
) {
    let owner = follows::follow_owner_id(&state.config, None);
    if let Err(error) = board::pushes::send_excluding(
        &board_store(state),
        &follows::push_store(state),
        &owner,
        recomputed,
        time::OffsetDateTime::now_utc(),
        excluded,
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
        let mut stopped = state.shutdown().subscribe();
        loop {
            let timed_out = tokio::select! {
                _ = tokio::time::sleep_until(next_pass) => true,
                _ = state.board_wake.notify.notified() => false,
                _ = stopped.changed() => return,
            };
            if state.shutdown().is_stopped() {
                return;
            }
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
                    Ok(None)
                } else if pass {
                    run_pass_inner(&pass_state, false).map(Some)
                } else {
                    recompute_inner(&pass_state, false).map(Some)
                }
            })
            .await;
            match result {
                Ok(Ok(Some(recomputed))) => {
                    if let Err(error) = new_agent_settings(&state).and_then(|settings| {
                        board_store(&state).sync_launch_preferences(&recomputed.board, &settings)
                    }) {
                        eprintln!("launch preference capture failed: {error:#}");
                    }
                    let started = process_auto_starts(&state, &recomputed).await;
                    send_alerts_excluding(&state, &recomputed, &started);
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => eprintln!("board pass failed: {error:#}"),
                Err(error) => eprintln!("board pass task failed: {error}"),
            }
        }
    });
}

/// The single board loop is the start worker. Reservation rechecks readiness
/// under board_lock, so a concurrent manual Start or claim wins exactly once.
async fn process_auto_starts(
    state: &Arc<AppState>,
    recomputed: &board::Recomputed,
) -> BTreeSet<Key> {
    let mut started = BTreeSet::new();
    let settings = match new_agent_settings(state) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("auto-start settings failed: {error:#}");
            return started;
        }
    };
    let store = board_store(state);
    let records = match store.auto_starts() {
        Ok(records) => records,
        Err(error) => {
            eprintln!("auto-start read failed: {error:#}");
            return started;
        }
    };
    let (starts, cancellations) =
        board::auto_start::plan(recomputed, records, settings.auto_start_paused);
    for (key, reason) in cancellations {
        if let Err(error) = store.cancel_auto_start(&key, reason, time::OffsetDateTime::now_utc()) {
            eprintln!("auto-start cancel failed: {error:#}");
        }
    }
    for record in starts {
        let key = record.key();
        // A prior launch may take minutes. Owner writes take this same gate,
        // so Pause and Cancel submitted during it apply before the next one.
        let _launch_guard = state.board_auto_start_gate.lock().await;
        match authorization_current(state, &record) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                eprintln!("auto-start recheck failed: {error:#}");
                continue;
            }
        }
        let result = start(
            state.clone(),
            StartRequest {
                repo: key.0.clone(),
                number: key.1,
                provider: record.provider.clone(),
                model: record.model.clone(),
                reasoning_effort: record.effort.clone(),
                name: None,
                brief: record.brief.clone(),
                start_blocked: false,
                reviewer: None,
            },
            true,
        )
        .await;
        let now = time::OffsetDateTime::now_utc();
        match result {
            Ok(session) => {
                let id = session["session_id"].as_str().unwrap_or_default();
                if let Err(error) = store.auto_start_result(&key, Ok(id), now) {
                    eprintln!("auto-start success record failed: {error:#}");
                }
                started.insert(key.clone());
                let name = session["name"].as_str().unwrap_or("agent");
                auto_start_notice(
                    state,
                    &key,
                    &format!(
                        "Started #{} as {name} ({}, {})",
                        key.1,
                        record.model.as_deref().unwrap_or("default model"),
                        record.effort.as_deref().unwrap_or("default effort")
                    ),
                    now,
                );
            }
            Err(error) => {
                match store.ticket_claimed(&key) {
                    Ok(true) => {
                        if let Err(error) =
                            store.cancel_auto_start(&key, "ticket claimed or started by hand", now)
                        {
                            eprintln!("auto-start claim cancellation failed: {error:#}");
                        }
                        continue;
                    }
                    Ok(false) => {}
                    Err(error) => eprintln!("auto-start claim check failed: {error:#}"),
                }
                let message = match &error {
                    ApiError::Internal(error) => format!("{error:#}"),
                    ApiError::NotFound(detail) | ApiError::Auth { detail, .. } => {
                        (*detail).to_owned()
                    }
                    ApiError::Status { detail, .. } => detail.clone(),
                    ApiError::StatusBody { body, .. } => {
                        body["detail"].as_str().unwrap_or("start failed").to_owned()
                    }
                };
                if let Err(error) = store.auto_start_result(&key, Err(&message), now) {
                    eprintln!("auto-start failure record failed: {error:#}");
                }
                auto_start_notice(
                    state,
                    &key,
                    &format!("Could not start #{}: {message}", key.1),
                    now,
                );
            }
        }
    }
    started
}

fn authorization_current(
    state: &AppState,
    record: &board::auto_start::Record,
) -> anyhow::Result<bool> {
    if new_agent_settings(state)?.auto_start_paused {
        return Ok(false);
    }
    Ok(board_store(state)
        .auto_starts()?
        .into_iter()
        .find(|row| row.key() == record.key())
        .as_ref()
        == Some(record))
}

fn auto_start_notice(state: &AppState, key: &Key, message: &str, now: time::OffsetDateTime) {
    let owner = follows::follow_owner_id(&state.config, None);
    let event_id = match board::record_event(
        &board_store(state),
        "auto_start_notice",
        None,
        Some(key),
        None,
        Some(message),
        now,
    ) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("auto-start notice event failed: {error:#}");
            return;
        }
    };
    let notice = crate::owner_push::NewNotice {
        user_id: owner,
        kind: NOTICE_BOARD_AUTO_START.to_owned(),
        session_id: "board".to_owned(),
        session_name: "sm board".to_owned(),
        subject_id: format!("board:0:{event_id}"),
        title: message.to_owned(),
        body: message.to_owned(),
        reader_path: "/board".to_owned(),
        blocking: false,
    };
    if let Err(error) = follows::push_store(state).create_notice(&notice, now) {
        eprintln!("auto-start notice failed: {error:#}");
    }
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
        if notice.kind != NOTICE_BOARD_READY
            && notice.kind != NOTICE_BOARD_LANE_DONE
            && notice.kind != NOTICE_BOARD_AUTO_START
        {
            continue;
        }
        let opened =
            board::pushes::notice_opened(&notice.subject_id, &notice.created_at, seen.as_ref());
        if notice.acked_at.is_some() || opened {
            continue;
        }
        unseen.count += 1;
        if let Some((lane_id, _)) = board::pushes::subject_event(&notice.subject_id) {
            if lane_id > 0 {
                unseen.lane_ids.insert(lane_id);
            }
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
    payload["auto_start_paused"] = json!(new_agent_settings(state)?.auto_start_paused);
    let early = store.started_early()?;
    let types = new_agent_settings(state)?.agent_types;
    let tiers: BTreeMap<Key, String> = store
        .ticket_tiers()?
        .into_iter()
        .filter_map(|(key, tier)| {
            types
                .iter()
                .find(|kind| kind.name.eq_ignore_ascii_case(&tier))
                .map(|kind| (key, kind.name.clone()))
        })
        .collect();
    let auto_starts: BTreeMap<Key, Value> = store
        .auto_starts()?
        .into_iter()
        .filter(|record| record.state == "waiting" || record.state == "failed")
        .map(|record| (record.key(), record.chip()))
        .collect();
    let links = super::board_links::BoardLinks::load(state, &input)?;
    for lane in payload["lanes"].as_array_mut().into_iter().flatten() {
        for ticket in lane["tickets"].as_array_mut().into_iter().flatten() {
            mark_started_early(ticket, &early);
            add_auto_start_fields(ticket, &tiers, &auto_starts);
            links.attach(ticket);
        }
    }
    for group in payload["other"].as_array_mut().into_iter().flatten() {
        for ticket in group["tickets"].as_array_mut().into_iter().flatten() {
            mark_started_early(ticket, &early);
            add_auto_start_fields(ticket, &tiers, &auto_starts);
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

fn add_auto_start_fields(
    ticket: &mut Value,
    tiers: &BTreeMap<Key, String>,
    auto_starts: &BTreeMap<Key, Value>,
) {
    let key = (
        ticket["repo"].as_str().unwrap_or_default().to_owned(),
        ticket["number"].as_i64().unwrap_or_default(),
    );
    ticket["tier"] = tiers.get(&key).map_or(Value::Null, |tier| json!(tier));
    ticket["auto_start"] = auto_starts.get(&key).cloned().unwrap_or(Value::Null);
}

pub(super) async fn blocking<T: Send + 'static>(
    state: &Arc<AppState>,
    work: impl FnOnce(&AppState) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || work(&state))
        .await
        .map_err(|error| ApiError::Internal(anyhow::anyhow!("board task failed: {error}")))?
}

pub(super) fn refusal_error(refusal: Refusal, lane: Option<Value>) -> ApiError {
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

pub(super) fn bad_request(detail: impl Into<String>) -> ApiError {
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

#[derive(Deserialize)]
pub(super) struct NotBeforeRequest {
    repo: String,
    number: i64,
    not_before: Option<String>,
    #[serde(default)]
    clear: bool,
}

pub(super) async fn put_not_before(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<NotBeforeRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        "/board/not-before",
    )?;
    ensure_core_writes_enabled(&state)?;
    let key = ticket_key(&payload.repo, payload.number)?;
    let at = match (payload.clear, payload.not_before.as_deref()) {
        (true, None) => None,
        (false, Some(value)) => Some(
            time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                .map_err(|_| {
                    bad_request("not_before must be an RFC 3339 timestamp with a timezone")
                })?,
        ),
        _ => return Err(bad_request("provide not_before or clear, exclusively")),
    };
    blocking(&state, move |state| {
        let node = if payload.clear {
            None
        } else {
            let resolved = state
                .board_source
                .resolve(std::slice::from_ref(&key))
                .map_err(|error| ApiError::Status {
                    status: StatusCode::BAD_GATEWAY,
                    detail: error,
                })?;
            let issue = resolved
                .into_iter()
                .next()
                .flatten()
                .ok_or_else(|| ApiError::Status {
                    status: StatusCode::NOT_FOUND,
                    detail: "Ticket not found".into(),
                })?;
            if issue.node.state != "open" {
                return Err(bad_request("Ticket is closed"));
            }
            Some(issue.node)
        };
        let _guard = state
            .board_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        board_store(state).set_not_before(
            &key,
            node.as_ref().zip(at),
            time::OffsetDateTime::now_utc(),
        )?;
        request_recompute(state);
        Ok(json!({"cleared": payload.clear, "not_before": at.map(crate::owner_push::format_ts)}))
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
pub(super) struct WaitingRequest {
    repo: String,
    number: i64,
    text: Option<String>,
    url: Option<String>,
    #[serde(default)]
    clear: bool,
}

pub(super) async fn put_waiting(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<WaitingRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), "/board/waiting")?;
    ensure_core_writes_enabled(&state)?;
    let key = ticket_key(&payload.repo, payload.number)?;
    let text = payload.text.unwrap_or_default();
    let url = payload.url.unwrap_or_default();
    if !payload.clear {
        if text.trim().is_empty() || text.chars().count() > 120 {
            return Err(bad_request("text must contain 1 to 120 characters"));
        }
        if !url.parse::<Uri>().ok().is_some_and(|u| {
            u.scheme_str() == Some("https") && u.host().is_some_and(|host| !host.is_empty())
        }) {
            return Err(bad_request("url must be an https URL"));
        }
    }
    blocking(&state, move |state| {
        let node = if payload.clear {
            None
        } else {
            let resolved = state
                .board_source
                .resolve(std::slice::from_ref(&key))
                .map_err(|error| ApiError::Status {
                    status: StatusCode::BAD_GATEWAY,
                    detail: error,
                })?;
            let issue = resolved
                .into_iter()
                .next()
                .flatten()
                .ok_or_else(|| ApiError::Status {
                    status: StatusCode::NOT_FOUND,
                    detail: "Ticket not found".into(),
                })?;
            if issue.node.state != "open" {
                return Err(bad_request("Ticket is closed"));
            }
            Some(issue.node)
        };
        let _guard = state
            .board_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        board_store(state).set_waiting(
            &key,
            node.as_ref().map(|n| (n, text.as_str(), url.as_str())),
            time::OffsetDateTime::now_utc(),
        )?;
        request_recompute(state);
        Ok(json!({"cleared": payload.clear}))
    })
    .await
    .map(Json)
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
pub(super) fn add_lane(
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

pub(super) fn owner_write_guard(
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
        let mut payload = board_payload(state, None, Some(hours))?;
        super::board_launch::decorate(state, &mut payload)?;
        Ok(payload)
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
pub(super) fn checkout(config: &AppConfig, repo: &str) -> Result<String, ApiError> {
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
    // The Reviewer row starts on what a request for this ticket would use.
    let default = state.session_store.owner_settings()?["reviews"]["reviewer"].clone();
    let resolved = crate::review::policy::resolve(
        &expand_home(&state.config.sm_send.db_path),
        &key.0,
        None,
        Some(key.1),
        None,
        &default,
    )?;
    options["review_policy"] = json!({
        "resolved": resolved["reviewer"],
        "fallback": resolved["fallback"],
        "source": resolved["source"],
    });
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
    pub(super) repo: String,
    pub(super) number: i64,
    pub(super) provider: String,
    pub(super) model: Option<String>,
    pub(super) reasoning_effort: Option<String>,
    pub(super) name: Option<String>,
    pub(super) brief: Option<String>,
    #[serde(default)]
    pub(super) start_blocked: bool,
    /// Stored as the ticket's review policy, set by the owner (spec 1768 I2).
    #[serde(default)]
    pub(super) reviewer: Option<Value>,
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
    start(state, payload, false).await.map(Json)
}

#[derive(Deserialize)]
pub(super) struct AutoStartKey {
    repo: String,
    number: i64,
}

#[derive(Deserialize)]
pub(super) struct AutoStartLane {
    goal_repo: String,
    goal_number: i64,
    tickets: Vec<board::auto_start::Choice>,
}

fn validate_auto_choice(
    state: &AppState,
    board: &board::model::Board,
    choice: &board::auto_start::Choice,
) -> Result<Key, ApiError> {
    let key = ticket_key(&choice.repo, choice.number)?;
    let settings = new_agent_settings(state)?;
    if !matches!(choice.provider.as_str(), "claude" | "codex-fork") {
        return Err(bad_request("provider must be claude or codex-fork"));
    }
    if let Some(name) = &choice.agent_type {
        let agent = settings
            .agent_types
            .iter()
            .find(|agent| agent.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| bad_request(format!("unknown agent type {name}")))?;
        if choice.provider != agent.provider
            || choice.model.as_deref() != Some(agent.model.as_str())
            || choice.reasoning_effort.as_deref() != Some(agent.effort.as_str())
        {
            return Err(bad_request(format!(
                "agent type {name} does not match its provider, model and effort"
            )));
        }
    }
    if choice
        .model
        .as_deref()
        .is_some_and(|model| model.trim().is_empty())
    {
        return Err(bad_request("model must be non-empty or null"));
    }
    let efforts: &[&str] = if choice.provider == "claude" {
        &["low", "medium", "high", "xhigh", "max"]
    } else {
        &["medium", "high", "xhigh"]
    };
    if choice
        .reasoning_effort
        .as_deref()
        .is_some_and(|effort| !efforts.contains(&effort))
    {
        return Err(bad_request("reasoning_effort is invalid for the provider"));
    }
    let facts = board
        .facts
        .get(&key)
        .ok_or(ApiError::NotFound("Ticket not on the board"))?;
    if !facts.item.is_open()
        || !matches!(
            facts.state,
            crate::board::model::TicketState::Blocked | crate::board::model::TicketState::Ready
        )
    {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: format!("#{} is not open for auto-start", key.1),
        });
    }
    if facts.holder.is_some() {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: format!("#{} is already claimed", key.1),
        });
    }
    Ok(key)
}

pub(super) fn auto_start_owner_guard(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    uri: &Uri,
) -> Result<(), ApiError> {
    let Some(value) = headers.get("x-sm-partner-assertion") else {
        return owner_write_guard(state, headers, peer_addr, "PUT", uri);
    };
    let denied = |reason: &str| ApiError::Status {
        status: StatusCode::FORBIDDEN,
        detail: format!("Partner login: {reason}"),
    };
    // The public tunnel also connects over loopback: require a local host
    // and reject forwarding headers so it cannot widen this exception.
    if !is_local_bypass_request(headers, Some(peer_addr), &state.config)
        || [
            "forwarded",
            "x-forwarded-host",
            "x-forwarded-for",
            "cf-connecting-ip",
        ]
        .iter()
        .any(|name| headers.contains_key(*name))
    {
        return Err(denied("a direct loopback request is required"));
    }
    let assertion = value.to_str().map_err(|_| denied("malformed assertion"))?;
    if state.config.partner_access_audiences.is_empty() {
        return Err(denied("no partner audiences are configured"));
    }
    let issuer = state
        .config
        .cloudflare_access
        .expected_issuer()
        .ok_or_else(|| denied("Cloudflare team is not configured"))?;
    let kid =
        cloudflare_access_assertion_key_id(assertion).map_err(|_| denied("malformed assertion"))?;
    let had_cached = cloudflare_access_has_cached_jwks(state, &issuer);
    let mut jwks = cloudflare_access_cached_jwks(state, &issuer, false)
        .map_err(|_| denied("could not load team certificates"))?;
    if jwks.find(&kid).is_none()
        && had_cached
        && cloudflare_access_mark_unknown_key_refresh_if_allowed(state, &issuer)
    {
        jwks = cloudflare_access_cached_jwks(state, &issuer, true)
            .map_err(|_| denied("could not refresh team certificates"))?;
    }
    let claims = state
        .config
        .partner_access_audiences
        .iter()
        .find_map(|audience| {
            crate::cloudflare_access::verify_cloudflare_access_assertion_with_jwks(
                assertion, &issuer, audience, &jwks,
            )
            .ok()
        })
        .ok_or_else(|| denied("invalid signature, issuer, audience, or token lifetime"))?;
    // The shared verifier permits clock skew; partner authority must be unexpired.
    if claims.exp as i128 <= time::OffsetDateTime::now_utc().unix_timestamp() as i128 {
        return Err(denied("assertion has expired"));
    }
    if !claims
        .email
        .as_deref()
        .is_some_and(|email| allowlisted_google_email(&state.config, email))
    {
        return Err(denied("owner email is required"));
    }
    Ok(())
}

pub(super) async fn put_auto_start(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(mut choice): Json<board::auto_start::Choice>,
) -> Result<Json<Value>, ApiError> {
    blocking(&state, move |state| {
        auto_start_owner_guard(state, &headers, peer_addr, &uri)
    })
    .await?;
    ensure_core_writes_enabled(&state)?;
    let _launch_guard = state.board_auto_start_gate.lock().await;
    blocking(&state, move |state| {
        let _guard = state
            .board_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        let store = board_store(state);
        let (board, _) = store.board(&outside(state)?, time::OffsetDateTime::now_utc())?;
        let key = validate_auto_choice(state, &board, &choice)?;
        choice.repo = key.0;
        store.authorize_auto_starts(&[choice], time::OffsetDateTime::now_utc())?;
        request_recompute(state);
        Ok(json!({"state":"waiting"}))
    })
    .await
    .map(Json)
}

pub(super) async fn delete_auto_start(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Query(query): Query<AutoStartKey>,
) -> Result<Json<Value>, ApiError> {
    owner_write_guard(&state, &headers, peer_addr, "DELETE", &uri)?;
    ensure_core_writes_enabled(&state)?;
    let _launch_guard = state.board_auto_start_gate.lock().await;
    let key = ticket_key(&query.repo, query.number)?;
    blocking(&state, move |state| {
        let _guard = state
            .board_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        board_store(state).cancel_auto_start(
            &key,
            "cancelled by owner",
            time::OffsetDateTime::now_utc(),
        )?;
        Ok(json!({"state":"cancelled"}))
    })
    .await
    .map(Json)
}

pub(super) async fn put_auto_start_lane(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(mut body): Json<AutoStartLane>,
) -> Result<Json<Value>, ApiError> {
    owner_write_guard(&state, &headers, peer_addr, "PUT", &uri)?;
    ensure_core_writes_enabled(&state)?;
    let _launch_guard = state.board_auto_start_gate.lock().await;
    blocking(&state, move |state| {
        let _guard = state
            .board_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        let store = board_store(state);
        let (board, _) = store.board(&outside(state)?, time::OffsetDateTime::now_utc())?;
        let goal = ticket_key(&body.goal_repo, body.goal_number)?;
        let lane = board
            .lanes
            .iter()
            .find(|lane| lane.lane.goal == goal)
            .ok_or(ApiError::NotFound("Lane not on the board"))?;
        let mut seen = BTreeSet::new();
        for choice in &mut body.tickets {
            let key = validate_auto_choice(state, &board, choice)?;
            if !lane.contains(&key) || !seen.insert(key.clone()) {
                return Err(bad_request(format!(
                    "#{} is not a distinct ticket in this lane",
                    key.1
                )));
            }
            choice.repo = key.0;
        }
        store.authorize_auto_starts(&body.tickets, time::OffsetDateTime::now_utc())?;
        request_recompute(state);
        Ok(json!({"state":"waiting", "count":body.tickets.len()}))
    })
    .await
    .map(Json)
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
    if let Some(at) = &facts.waiting_until {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: format!(
                "#{} waits until {at}; clear its earliest start time to start earlier",
                key.1
            ),
        });
    }
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

pub(super) async fn start(
    state: Arc<AppState>,
    payload: StartRequest,
    auto: bool,
) -> Result<Value, ApiError> {
    if !matches!(
        payload.provider.as_str(),
        "claude" | "codex-fork" | "opencode"
    ) {
        return Err(bad_request(
            "provider must be claude, codex-fork or opencode",
        ));
    }
    let key = ticket_key(&payload.repo, payload.number)?;
    if let Some(reviewer) = &payload.reviewer {
        crate::review::policy::validate("ticket", reviewer).map_err(bad_request)?;
    }
    let checked_key = key.clone();
    let (options, lanes) = blocking(&state, move |state| {
        let validation = validate_start(state, &checked_key, payload.start_blocked)?;
        Ok((
            start_options_payload(state, &checked_key, false)?,
            validation.lanes,
        ))
    })
    .await?;
    if let Some(reviewer) = &payload.reviewer {
        crate::review::policy::set(
            &expand_home(&state.config.sm_send.db_path),
            crate::review::policy::PolicyChange {
                scope: "ticket",
                repo: &key.0,
                number: key.1,
                reviewer: Some(reviewer),
                session_id: None,
                name: &state.config.owner_name,
                now: &now_rfc3339(),
            },
        )?;
    }
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
            max_wait_seconds: None,
            spawn_prompt_source: None,
            spawn_brief: None,
            started_by_sm: true,
        },
    )
    .await;
    claims::finish_spawn_ticket(&state, &reservation, created.is_ok());
    let session = created?;
    if !auto {
        if let Err(error) = board_store(&state).cancel_auto_start(
            &key,
            "started by hand",
            time::OffsetDateTime::now_utc(),
        ) {
            eprintln!("board auto-start cancellation after manual start failed: {error:#}");
        }
    }
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

    #[tokio::test]
    async fn cancelled_or_paused_later_ticket_fails_fresh_launch_check() {
        let root = std::env::temp_dir().join(format!(
            "board-auto-start-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut config = AppConfig::default();
        config.paths.state_file = root.join("sessions.json").to_string_lossy().into_owned();
        config.sm_send.db_path = root.join("queue.db").to_string_lossy().into_owned();
        config.usage.enabled = false;
        config.rust_core.fixture_writes_enabled = true;
        let state = Arc::new(AppState::new(config));
        let store = board_store(&state);
        store.ensure_schema().unwrap();
        let choice = board::auto_start::Choice {
            repo: "acme/widgets".into(),
            number: 2,
            agent_type: None,
            provider: "claude".into(),
            model: Some("sonnet".into()),
            reasoning_effort: Some("high".into()),
            brief: None,
        };
        store
            .authorize_auto_starts(
                std::slice::from_ref(&choice),
                time::OffsetDateTime::now_utc(),
            )
            .unwrap();
        let record = store.auto_starts().unwrap().remove(0);
        assert!(authorization_current(&state, &record).unwrap());
        // Represent the earlier launch. The owner's cancellation waits for it,
        // then the next ticket must see the changed authorization.
        let launch_guard = state.board_auto_start_gate.lock().await;
        let cancelling_state = state.clone();
        let cancellation = tokio::spawn(async move {
            let _guard = cancelling_state.board_auto_start_gate.lock().await;
            board_store(&cancelling_state)
                .cancel_auto_start(
                    &("acme/widgets".into(), 2),
                    "cancelled by owner",
                    time::OffsetDateTime::now_utc(),
                )
                .unwrap();
        });
        tokio::task::yield_now().await;
        assert!(!cancellation.is_finished());
        drop(launch_guard);
        cancellation.await.unwrap();
        assert!(!authorization_current(&state, &record).unwrap());
        store
            .authorize_auto_starts(&[choice], time::OffsetDateTime::now_utc())
            .unwrap();
        let fresh = store.auto_starts().unwrap().remove(0);
        assert!(authorization_current(&state, &fresh).unwrap());
        state
            .session_store
            .update_owner_settings(&json!({"new_agent":{"auto_start_paused":true}}), |_| {
                Ok(Ok(()))
            })
            .unwrap()
            .unwrap();
        assert!(!authorization_current(&state, &fresh).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

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
