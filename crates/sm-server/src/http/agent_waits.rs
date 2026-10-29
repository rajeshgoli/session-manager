//! Agent waits HTTP surface (#2477): `POST /claims/waits`,
//! `GET /claims/waits`, `POST /claims/waits/cancel`, the `sm send` hook that
//! ends a wait as delivered, the session feed's `agent_wait` entries and
//! `awaited_by`, and the task-complete notice to an awaited seat.

use super::*;
use crate::work_claims::waits::{
    until_text, validate_deliverable, validate_until, AgentWait, EndedJob, WaitTarget,
};
use crate::work_claims::{ClaimView, HolderState, SessionDirectory};

#[derive(Debug, Deserialize)]
pub(super) struct OpenWaitRequest {
    waiter_session_id: String,
    target: String,
    #[serde(rename = "for")]
    deliverable: String,
    until: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct CancelWaitRequest {
    waiter_session_id: String,
    #[serde(default)]
    target: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ListWaitsQuery {
    #[serde(default)]
    session_id: Option<String>,
}

pub(super) async fn post_wait(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<OpenWaitRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), "/claims/waits")?;
    ensure_core_writes_enabled(&state)?;
    let waiter = claims::claimant(&state, &payload.waiter_session_id)?;
    let now = time::OffsetDateTime::now_utc();
    let deliverable = validate_deliverable(&payload.deliverable).map_err(claims::bad_request)?;
    let until = validate_until(&payload.until, now).map_err(claims::bad_request)?;
    let target = parse_target(&state, &waiter.id, &payload.target, false)?;
    let store = claims::work_claim_store(&state);
    let (wait, created) = store.open_wait(&waiter.id, &target, &deliverable, until, now)?;
    let sessions = claims::session_directory(&state)?;
    let mut view = wait_view(&wait, &sessions, &ticket_titles(&state, [&wait])?);
    view["created"] = json!(created);
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(view)))
}

pub(super) async fn list_waits(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListWaitsQuery>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_owner_web_or_session_read(&state, &request)?;
    let identifier = query
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| claims::bad_request("session_id is required"))?;
    let session_id = resolve_session_or_registry_role(&state, identifier)?
        .map(|session| session.id)
        .unwrap_or_else(|| identifier.to_owned());
    let store = claims::work_claim_store(&state);
    let waits = store.open_waits()?;
    let held = store.claims_for_session(&session_id, true)?;
    let sessions = claims::session_directory(&state)?;
    let titles = ticket_titles(&state, &waits)?;
    let views = |filter: &dyn Fn(&AgentWait) -> bool| {
        waits
            .iter()
            .filter(|wait| filter(wait))
            .map(|wait| wait_view(wait, &sessions, &titles))
            .collect::<Vec<_>>()
    };
    Ok(Json(json!({
        "waiting_on": views(&|wait| wait.waiter_session_id == session_id),
        "awaited_by": views(&|wait| wait.waiter_session_id != session_id
            && awaits(wait, &session_id, &held)),
    })))
}

pub(super) async fn cancel_wait(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<CancelWaitRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        "/claims/waits/cancel",
    )?;
    ensure_core_writes_enabled(&state)?;
    let waiter = claims::claimant(&state, &payload.waiter_session_id)?;
    let raw = payload
        .target
        .as_deref()
        .map(str::trim)
        .filter(|target| !target.is_empty());
    let target = raw
        .map(|raw| parse_target(&state, &waiter.id, raw, true))
        .transpose()?;
    let cancelled = claims::work_claim_store(&state).cancel_waits(
        &waiter.id,
        target.as_ref(),
        time::OffsetDateTime::now_utc(),
    )?;
    if let (Some(raw), 0) = (raw, cancelled) {
        return Err(ApiError::Status {
            status: StatusCode::NOT_FOUND,
            detail: format!("no open wait on {raw}"),
        });
    }
    Ok(Json(json!({ "cancelled": cancelled })))
}

/// The longest reference to work outside sm, in characters.
const MAX_REFERENCE_CHARS: usize = 100;

/// `owner/repo#N` is a ticket; `queue:<job id>` or a bare `job_…` id a
/// queue job; any other `<kind>:<id>` work outside sm (sm#2611); anything
/// else a seat, resolved like an `sm send` recipient. A cancel may name a
/// seat that has since retired, or a job that has ended.
fn parse_target(
    state: &AppState,
    waiter: &str,
    raw: &str,
    allow_retired: bool,
) -> Result<WaitTarget, ApiError> {
    let raw = raw.trim();
    let job = raw
        .strip_prefix("queue:")
        .map(str::trim)
        .or_else(|| raw.starts_with("job_").then_some(raw));
    if let Some(id) = job {
        if !allow_retired {
            let path = queue_db(state);
            match RetainedQueueStore::get_queue_job_strict_from_path(&path, id) {
                Ok(Some(job)) if matches!(job.state.as_str(), "pending" | "running") => {}
                Ok(Some(job)) => {
                    return Err(claims::bad_request(format!(
                        "queue job {id} already {}",
                        job.state
                    )))
                }
                _ => return Err(claims::bad_request(format!("queue job {id} not found"))),
            }
        }
        return Ok(WaitTarget::Job(id.to_owned()));
    }
    if let Some((kind, id)) = raw.split_once(':') {
        let valid = |part: &str| {
            !part.is_empty() && !part.chars().any(|c| c.is_whitespace() || c.is_control())
        };
        if !valid(kind) || !valid(id) || raw.chars().count() > MAX_REFERENCE_CHARS {
            return Err(claims::bad_request(format!(
                "an outside reference is <kind>:<id> with no spaces, at most {MAX_REFERENCE_CHARS} characters"
            )));
        }
        return Ok(WaitTarget::External(raw.to_owned()));
    }
    if raw.starts_with('#') {
        return Err(claims::bad_request(
            "a ticket target needs its repo: owner/repo#N",
        ));
    }
    if let Some((repo, number)) = raw.rsplit_once('#') {
        let number = number
            .parse::<i64>()
            .ok()
            .filter(|number| *number > 0)
            .ok_or_else(|| claims::bad_request("ticket number must be a positive integer"))?;
        let repo = claims::validated_repo(repo)?;
        return Ok(WaitTarget::Ticket { repo, number });
    }
    let Some(seat) = resolve_session_or_registry_role(state, raw)? else {
        return Err(claims::bad_request(format!("Session {raw} not found")));
    };
    if seat.id == waiter {
        return Err(claims::bad_request("you cannot wait on yourself"));
    }
    if !allow_retired && claims::is_retired(&seat) {
        return Err(claims::bad_request(format!("Session {raw} is retired")));
    }
    Ok(WaitTarget::Seat(seat.id))
}

/// Whether `wait` is on `session_id`, directly or through a ticket it holds.
fn awaits(wait: &AgentWait, session_id: &str, held: &[ClaimView]) -> bool {
    match wait.target() {
        WaitTarget::Seat(seat) => seat == session_id,
        WaitTarget::Ticket { repo, number } => held.iter().any(|view| {
            view.claim.kind == "ticket" && view.claim.repo == repo && view.claim.number == number
        }),
        WaitTarget::Job(_) | WaitTarget::External(_) => false,
    }
}

fn queue_db(state: &AppState) -> std::path::PathBuf {
    expand_home(&state.config.queue_runner_state_dir().to_string_lossy()).join("queue_runner.db")
}

/// The queue jobs the open waits name that are no longer pending or
/// running, or that the queue no longer has. A job the queue cannot read
/// now is left for the next pass.
pub(super) fn ended_jobs(state: &AppState, waits: &[AgentWait]) -> BTreeMap<String, EndedJob> {
    let path = queue_db(state);
    let mut ended = BTreeMap::new();
    for wait in waits {
        let WaitTarget::Job(id) = wait.target() else {
            continue;
        };
        if ended.contains_key(&id) {
            continue;
        }
        let job = match RetainedQueueStore::get_queue_job_strict_from_path(&path, &id) {
            Ok(Some(job)) if matches!(job.state.as_str(), "pending" | "running") => continue,
            Ok(Some(job)) => EndedJob {
                text: format!("{} ({id}) {}", job.label, job.state),
                // Only the notify target gets a completion wake; an
                // application-owned job's requester gets none.
                woken: job.notify_session_id,
            },
            Ok(None) => EndedJob {
                text: format!("{id} was not found"),
                woken: None,
            },
            Err(error) => {
                eprintln!("agent wait on queue job {id}: {error:#}");
                continue;
            }
        };
        ended.insert(id, job);
    }
    ended
}

/// Cached titles of the tickets the waits name, by `(repo, number)`.
fn ticket_titles<'a>(
    state: &AppState,
    waits: impl IntoIterator<Item = &'a AgentWait>,
) -> anyhow::Result<BTreeMap<(String, i64), String>> {
    let store = claims::work_claim_store(state);
    let mut titles = BTreeMap::new();
    for wait in waits {
        if let WaitTarget::Ticket { repo, number } = wait.target() {
            if let Some(item) = store.item(&repo, number)? {
                titles.insert((repo, number), item.title);
            }
        }
    }
    Ok(titles)
}

/// One wait as the routes and the session feed show it.
fn wait_view(
    wait: &AgentWait,
    sessions: &SessionDirectory,
    titles: &BTreeMap<(String, i64), String>,
) -> Value {
    let name_of = |id: &str| {
        sessions
            .get(id)
            .map(|session| session.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| id.chars().take(8).collect())
    };
    let (target_label, target_name, title) = match wait.target() {
        WaitTarget::Seat(seat) => {
            let name = name_of(&seat);
            (name.clone(), Some(name), None)
        }
        WaitTarget::Ticket { repo, number } => (
            format!("#{number}"),
            None,
            titles
                .get(&(repo, number))
                .filter(|t| !t.is_empty())
                .cloned(),
        ),
        WaitTarget::Job(id) | WaitTarget::External(id) => (id, None, None),
    };
    json!({
        "kind": "agent_wait", "id": wait.id,
        "label": format!("{target_label} · {}", wait.deliverable),
        "state": "waiting", "since": wait.created_at, "until": wait.until_at,
        "until_text": until_text(wait),
        "target_kind": wait.target_kind, "target_session_id": wait.target_session_id,
        "target_name": target_name, "repo": wait.repo, "number": wait.number, "title": title,
        "target_ref": wait.target_ref,
        "waiter_session_id": wait.waiter_session_id,
        "waiter_name": name_of(&wait.waiter_session_id),
        "for": wait.deliverable,
    })
}

/// The session feed: each open wait in its live waiter's `waiting_on`, and
/// in `awaited_by` of the seat it names or of each seat holding its ticket.
pub(super) fn project_waits(
    state: &AppState,
    records: &[SessionRecord],
    claims: &[ClaimView],
    sessions: &mut BTreeMap<String, Value>,
) -> anyhow::Result<()> {
    let waits = claims::work_claim_store(state).open_waits()?;
    if waits.is_empty() {
        return Ok(());
    }
    let directory = SessionDirectory::new(records.iter().map(claims::session_info));
    let titles = ticket_titles(state, &waits)?;
    for wait in &waits {
        let waiter = &wait.waiter_session_id;
        if directory
            .get(waiter)
            .is_none_or(|session| session.state == HolderState::Retired)
        {
            continue;
        }
        let view = wait_view(wait, &directory, &titles);
        let targets: Vec<String> = match wait.target() {
            WaitTarget::Seat(seat) => vec![seat],
            WaitTarget::Ticket { repo, number } => claims
                .iter()
                .filter(|claim| {
                    claim.claim.kind == "ticket"
                        && claim.claim.repo == repo
                        && claim.claim.number == number
                        && claim.claim.session_id != *waiter
                })
                .map(|claim| claim.claim.session_id.clone())
                .collect(),
            WaitTarget::Job(_) | WaitTarget::External(_) => Vec::new(),
        };
        sessions
            .entry(waiter.clone())
            .or_insert_with(|| new_obligation_entry(waiter))["waiting_on"]
            .as_array_mut()
            .unwrap()
            .push(view.clone());
        for target in targets {
            let entry = sessions
                .entry(target.clone())
                .or_insert_with(|| new_obligation_entry(&target));
            if !entry["awaited_by"].is_array() {
                entry["awaited_by"] = json!([]);
            }
            entry["awaited_by"]
                .as_array_mut()
                .unwrap()
                .push(view.clone());
        }
    }
    Ok(())
}

/// After an accepted `sm send`: ends the recipient's waits on the sender.
/// Never fails the send; a store error is logged.
pub(super) fn deliver_on_send(state: &AppState, sender: Option<&str>, recipient: &str) {
    let Some(sender) = sender.map(str::trim).filter(|sender| !sender.is_empty()) else {
        return;
    };
    let sender = match state.session_store.get_session(sender) {
        Ok(Some(record)) => record.id,
        _ => return,
    };
    if let Err(error) = claims::work_claim_store(state).deliver_waits(
        &sender,
        recipient,
        time::OffsetDateTime::now_utc(),
    ) {
        eprintln!("agent wait delivery from {sender} to {recipient} failed: {error:#}");
    }
}

/// At task-complete: tells the session, in the background, who still waits
/// on it. Never fails or delays task-complete.
pub(super) fn schedule_task_complete_notice(state: &Arc<AppState>, session_id: &str) {
    let state = state.clone();
    let session_id = session_id.to_owned();
    tokio::spawn(async move {
        let task_state = state.clone();
        let result = tokio::task::spawn_blocking(move || {
            let sessions = claims::session_directory(&task_state)?;
            let notified = claims::work_claim_store(&task_state)
                .notify_awaited_at_task_complete(&session_id, &sessions)?;
            claims::deliver_claim_notices(&task_state, &notified);
            anyhow::Ok(())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("agent wait task-complete notice failed: {error:#}"),
            Err(error) => eprintln!("agent wait task-complete notice task failed: {error}"),
        }
    });
}
