//! Owner messages over HTTP (sm#1580): `sm send <person>` stores a message,
//! the owner reads it at `/messages/{id}`, comments on passages, and replies;
//! the reply reaches the agent as an ordinary queued message. Spec:
//! `specs/1580_app_messages_replace_email.html`, appendices D and E.

use super::*;
use crate::owner_messages::{
    derive_message_state, derive_title, is_owner_message_id, message_reader_path,
    order_reply_comments, render_delivered_text, validate_title, CreateOwnerMessage,
    NewOwnerMessage, OwnerMessage, OwnerMessageReply, OwnerMessageState, OwnerMessageStore,
    RecordReply, MAX_DRAFTS_PER_MESSAGE, MAX_MESSAGE_CHARS, TOO_LONG_DETAIL, UNREAD_CAP,
};

/// Messages the agent card lists: the last 30 days, at most 20 per sender.
const CARD_MESSAGE_WINDOW: time::Duration = time::Duration::days(30);
const CARD_MESSAGE_LIMIT: usize = 20;

pub(super) fn owner_message_store(state: &AppState) -> OwnerMessageStore {
    OwnerMessageStore::new(expand_home(&state.config.sm_send.db_path))
}

pub(super) fn owner_answered(
    state: &AppState,
    session_id: &str,
    via: &str,
) -> Result<usize, ApiError> {
    if state
        .session_store
        .get_session(session_id)?
        .is_none_or(|session| session_ended(&session))
    {
        return Ok(0);
    }
    if via != "manual" {
        let mut recent = state
            .owner_answered_at
            .lock()
            .map_err(|_| anyhow::anyhow!("Owner answer rate limiter unavailable"))?;
        let now = std::time::Instant::now();
        recent.retain(|_, at| now.duration_since(*at) < Duration::from_secs(2));
        if recent.contains_key(session_id) {
            return Ok(0);
        }
        recent.insert(session_id.to_owned(), now);
    }
    let changed = owner_message_store(state).answer_session(session_id, via)?;
    if changed > 0 {
        super::board::request_recompute(state);
    }
    Ok(changed)
}

/// `POST /sessions/{id}/needs-you/answered`: the owner answered elsewhere.
pub(super) async fn answer_session_needs_you(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let path = format!("/sessions/{session_id}/needs-you/answered");
    if owner_web_guard(&state, &headers, Some(peer_addr), "POST")?.is_none() {
        ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), &path)?;
    }
    ensure_core_writes_enabled(&state)?;
    if state.session_store.get_session(&session_id)?.is_none() {
        return Err(ApiError::NotFound("Session not found"));
    }
    // ✓ clears what the card shows (sm#1851): an open question, a review
    // request ("No review needed", as Inbox Done does), or a Finished
    // summary. `kind` picks one; without it, the one shown is cleared.
    let requested: Value = if body.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        serde_json::from_slice(&body).map_err(|_| ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "Body must be JSON".into(),
        })?
    };
    let facts = super::watch::session_facts(&state, &session_id)?;
    let shown = match facts["you"]["kind"].as_str() {
        Some(kind) => kind,
        None if !facts["finished"].is_null() => "finished",
        None => "message",
    };
    let kind = requested["kind"].as_str().unwrap_or(shown);
    match kind {
        "message" => {
            let _guard = state.owner_message_lock.lock().await;
            owner_answered(&state, &session_id, "manual")?;
        }
        "doc_review" => {
            if let Some(doc_id) = facts["you"]["doc_id"].as_str() {
                let _guard = state.owner_doc_review_lock.lock().await;
                super::docs::owner_doc_store(&state).dismiss_review(doc_id)?;
                super::board::request_recompute(&state);
            }
        }
        "finished" => {
            if let Some(store) = state.session_store.turn_message_store() {
                store.mark_read(&session_id, OffsetDateTime::now_utc())?;
            }
        }
        "prompt" => {}
        _ => {
            return Err(ApiError::Status {
                status: StatusCode::BAD_REQUEST,
                detail: "kind must be message, doc_review or finished".into(),
            })
        }
    }
    Ok(Json(json!({
        "kind": kind,
        "facts": super::watch::session_facts(&state, &session_id)?,
    })))
}

/// `PUT /sessions/{id}/note`: pin a note to an agent, or remove it with
/// blank text (sm#1851). A pinned note replaces "stalled".
pub(super) async fn put_agent_note(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let path = format!("/sessions/{session_id}/note");
    if owner_web_guard(&state, &headers, Some(peer_addr), "PUT")?.is_none() {
        ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), &path)?;
    }
    ensure_core_writes_enabled(&state)?;
    if state.session_store.get_session(&session_id)?.is_none() {
        return Err(ApiError::NotFound("Session not found"));
    }
    let payload: Value = serde_json::from_slice(&body).map_err(|_| ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: "Body must be JSON with a text field".into(),
    })?;
    let Some(text) = payload["text"].as_str() else {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "text is required; blank removes the note".into(),
        });
    };
    let note = crate::agent_notes::AgentNoteStore::new(expand_home(&state.config.sm_send.db_path))
        .set(&session_id, text, &now_rfc3339())?;
    super::board::request_recompute(&state);
    Ok(Json(json!({
        "note": note,
        "facts": super::watch::session_facts(&state, &session_id)?,
    })))
}

/// `GET /sessions/{id}/last-turn`: the text the agent wrote at the end of
/// its latest turn (spec 1782 D4).
pub(super) async fn get_last_turn(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let path = format!("/sessions/{session_id}/last-turn");
    if owner_web_guard(&state, &headers, Some(peer_addr), "GET")?.is_none() {
        ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), &path)?;
    }
    let turn = state
        .session_store
        .turn_message_store()
        .map(|store| store.last_turn(&session_id))
        .transpose()?
        .flatten()
        .ok_or(ApiError::NotFound("No last turn message"))?;
    Ok(Json(json!({
        "at": turn.at,
        "html": crate::owner_doc_render::render_markdown_sanitized(&turn.text),
        "text": turn.text,
    })))
}

/// Retired or killed: the session will never read another message.
pub(super) fn session_ended(session: &SessionRecord) -> bool {
    matches!(
        session.completion_status.as_deref(),
        Some("retired" | "killed")
    )
}

/// Who a reply to `session_id` reaches: the session while it exists and has
/// not ended (a stopped one gets it on restore), else the first successor in
/// its handoff chain that has not ended, else its parent when that has not
/// ended, else nobody. Owner review wakes use the same rule.
pub(super) fn live_recipient(state: &AppState, session_id: &str) -> Option<SessionRecord> {
    let session = state.session_store.get_session(session_id).ok().flatten()?;
    if !session_ended(&session) {
        return Some(session);
    }
    // A session that handed off forwards along its successor chain
    // (sm#1651). A successor that stopped but was not retired keeps the
    // reply for its restore.
    let mut next = session.successor_session_id.clone();
    for _ in 0..crate::handoff::execute::MAX_FORWARD_HOPS {
        let Some(successor) = next
            .as_deref()
            .and_then(|id| state.session_store.get_session(id).ok().flatten())
        else {
            break;
        };
        if !session_ended(&successor) {
            return Some(successor);
        }
        next = successor.successor_session_id.clone();
    }
    let parent = session.parent_session_id.as_deref()?;
    state
        .session_store
        .get_session(parent)
        .ok()
        .flatten()
        .filter(|parent| !session_ended(parent))
}

/// Whether the sender has ended, for the needs-you state. A sender that is
/// gone from the registry has ended too.
fn sender_ended(state: &AppState, message: &OwnerMessage) -> bool {
    state
        .session_store
        .get_session(&message.sender_session_id)
        .ok()
        .flatten()
        .is_none_or(|session| session_ended(&session))
}

fn message_state(
    state: &AppState,
    store: &OwnerMessageStore,
    message: &OwnerMessage,
) -> Result<OwnerMessageState, ApiError> {
    Ok(derive_message_state(
        message,
        store.has_reply(&message.id)?,
        sender_ended(state, message),
    ))
}

fn bad_request(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: detail.into(),
    }
}

fn conflict(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::CONFLICT,
        detail: detail.into(),
    }
}

pub(super) const NO_RECIPIENT: &str = "No agent is left to reply to";

fn find_message(state: &AppState, message_id: &str) -> Result<OwnerMessage, ApiError> {
    if !is_owner_message_id(message_id) {
        return Err(ApiError::NotFound("Message not found"));
    }
    owner_message_store(state)
        .get(message_id)?
        .ok_or(ApiError::NotFound("Message not found"))
}

/// The page's own requests carry a token minted for the message id, the
/// way doc pages carry one for the doc.
fn token_presented(state: &AppState, headers: &HeaderMap, message_id: &str) -> bool {
    header_text(headers, docs::DOC_TOKEN_HEADER)
        .is_some_and(|token| docs::doc_token_valid(&state.config, message_id, &token))
}

fn ensure_message_read_allowed(
    state: &AppState,
    request: &Request,
    message_id: &str,
) -> Result<(), ApiError> {
    if token_presented(state, request.headers(), message_id) {
        return Ok(());
    }
    ensure_owner_page_read_allowed(state, request)
}

fn ensure_message_write_allowed(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    message_id: &str,
    rest: &str,
) -> Result<(), ApiError> {
    if !token_presented(state, headers, message_id) {
        ensure_session_allowed_from_parts(
            &state.config,
            headers,
            Some(peer_addr),
            &format!("/messages/{message_id}/{rest}"),
        )?;
    }
    ensure_core_writes_enabled(state)
}

/// The page link people open: the browser host when one is configured,
/// else the host this request reached.
fn message_reader_url(config: &AppConfig, headers: &HeaderMap, path: &str) -> String {
    match docs::doc_browser_base_url(config) {
        Some(base) => format!("{base}{path}"),
        None => docs::doc_reader_url(headers, path.to_owned()),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateOwnerMessageRequest {
    #[serde(default)]
    sender_session_id: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    blocking: bool,
}

/// `POST /humans/{identifier}/messages`.
pub(super) async fn create_owner_message(
    State(state): State<Arc<AppState>>,
    Path(identifier): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<CreateOwnerMessageRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/humans/{identifier}/messages"),
    )?;
    ensure_core_writes_enabled(&state)?;
    let Some(human) = email_bridge(&state.config)?
        .lookup_human(&identifier)
        .map_err(email_config_error)?
    else {
        return Err(ApiError::NotFound("Human recipient not configured"));
    };
    let text = payload.text;
    if text.trim().is_empty() {
        return Err(bad_request("text is required"));
    }
    if text.chars().count() > MAX_MESSAGE_CHARS {
        return Err(bad_request(TOO_LONG_DETAIL));
    }
    let title = match payload.title.as_deref() {
        Some(title) => validate_title(title).map_err(|error| bad_request(error.to_string()))?,
        None => derive_title(&text),
    };
    let sender_id = payload
        .sender_session_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| bad_request("sender_session_id is required"))?;
    let Some(sender) = state.session_store.get_session(sender_id)? else {
        return Err(conflict(format!("Sender session {sender_id} not found")));
    };
    let store = owner_message_store(&state);
    let message = match store.create(NewOwnerMessage {
        human: human.name.clone(),
        sender_session_id: sender.id.clone(),
        sender_session_name: session_display_name(sender),
        title,
        body_markdown: text,
        blocking: payload.blocking,
    })? {
        CreateOwnerMessage::Created(message) => *message,
        CreateOwnerMessage::UnreadCapReached => {
            return Err(ApiError::StatusBody {
                status: StatusCode::TOO_MANY_REQUESTS,
                body: json!({"detail": format!(
                    "{} has {UNREAD_CAP} unread messages from you; wait for them to be read",
                    state.config.owner_name
                )}),
            });
        }
    };
    super::follows::notice_new_message(&state, &message);
    if message.blocking {
        super::board::request_recompute(&state);
    }
    let reader_path = message_reader_path(&message.id);
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": message.id,
            "title": message.title,
            "reader_path": reader_path,
            "reader_url": message_reader_url(&state.config, &headers, &reader_path),
            "blocking": message.blocking,
            "owner_name": state.config.owner_name,
        })),
    ))
}

fn reply_json(reply: &OwnerMessageReply) -> Value {
    json!({
        "id": reply.id,
        "body": reply.body,
        "comments": reply.comments,
        "delivered_text": reply.delivered_text,
        "delivered_to_session_id": reply.delivered_to_session_id,
        "created_at": reply.created_at,
    })
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct GetOwnerMessageQuery {
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    bottom: Option<String>,
}

/// `GET /messages/{id}`: the message page, or `?format=json`. Serving the
/// page marks the message viewed; JSON does not.
pub(super) async fn get_owner_message(
    State(state): State<Arc<AppState>>,
    Path(message_id): Path<String>,
    Query(query): Query<GetOwnerMessageQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let message = find_message(&state, &message_id)?;
    let store = owner_message_store(&state);
    let recipient = live_recipient(&state, &message.sender_session_id);
    if query.format.as_deref() == Some("json") {
        let state_value = message_state(&state, &store, &message)?;
        let mut value = serde_json::to_value(&message)?;
        value["state"] = json!(state_value);
        value["reply_to_session_id"] = json!(recipient.as_ref().map(|session| &session.id));
        value["replies"] = json!(store
            .replies(&message.id)?
            .iter()
            .map(reply_json)
            .collect::<Vec<_>>());
        return Ok(Json(value).into_response());
    }
    // The page is the sender's Inbox thread, scrolled to this message
    // (sm#1647). Served here rather than redirected: the app's reader sends
    // paths it does not know to the system browser.
    let at = Some(message.id.clone()).filter(|_| query.bottom.is_none());
    super::inbox::agent_thread_page(
        &state,
        &message.sender_session_id,
        at,
        false,
        web::wants_shell(&state, &request),
    )
}

/// `2m ago`, `3h ago`, `4d ago`; `just now` under a minute.
pub(super) fn relative_time(timestamp: &str, now: OffsetDateTime) -> String {
    let Some(then) = crate::owner_push::parse_ts(timestamp) else {
        return timestamp.to_owned();
    };
    let seconds = (now - then).whole_seconds().max(0);
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// `GET /messages/{id}/drafts`.
pub(super) async fn list_message_drafts(
    State(state): State<Arc<AppState>>,
    Path(message_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_message_read_allowed(&state, &request, &message_id)?;
    let message = find_message(&state, &message_id)?;
    Ok(Json(
        json!({ "drafts": owner_message_store(&state).drafts(&message.id)? }),
    ))
}

#[derive(Debug, Deserialize)]
struct CreateMessageDraftRequest {
    #[serde(default)]
    line: Option<i64>,
    #[serde(default)]
    quote: String,
    body: String,
}

#[derive(Debug, Deserialize)]
struct UpdateMessageDraftRequest {
    body: String,
}

/// `POST /messages/{id}/drafts`.
pub(super) async fn create_message_draft(
    State(state): State<Arc<AppState>>,
    Path(message_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    ensure_message_write_allowed(&state, &headers, peer_addr, &message_id, "drafts")?;
    let message = find_message(&state, &message_id)?;
    let payload: CreateMessageDraftRequest = docs::parse_json_body(&body)?;
    if payload.line.is_some_and(|line| line < 1) {
        return Err(bad_request("line must be positive"));
    }
    let quote = payload.quote.trim();
    if quote.chars().count() > docs::MAX_DRAFT_QUOTE {
        return Err(bad_request(format!(
            "Quotes are limited to {} characters",
            docs::MAX_DRAFT_QUOTE
        )));
    }
    let text = docs::validated_draft_body(&payload.body)?;
    // Drafts don't change under a reply being sent.
    let _guard = state.owner_message_lock.lock().await;
    let store = owner_message_store(&state);
    if store.drafts(&message.id)?.len() >= MAX_DRAFTS_PER_MESSAGE {
        return Err(bad_request(format!(
            "A reply holds at most {MAX_DRAFTS_PER_MESSAGE} comments; send these first"
        )));
    }
    let draft = store.create_draft(&message.id, payload.line, quote, &text)?;
    Ok(Json(serde_json::to_value(draft)?))
}

/// `PATCH /messages/{id}/drafts/{draft_id}`.
pub(super) async fn update_message_draft(
    State(state): State<Arc<AppState>>,
    Path((message_id, draft_id)): Path<(String, String)>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    ensure_message_write_allowed(
        &state,
        &headers,
        peer_addr,
        &message_id,
        &format!("drafts/{draft_id}"),
    )?;
    let message = find_message(&state, &message_id)?;
    let payload: UpdateMessageDraftRequest = docs::parse_json_body(&body)?;
    let text = docs::validated_draft_body(&payload.body)?;
    let _guard = state.owner_message_lock.lock().await;
    let draft = owner_message_store(&state)
        .update_draft(&message.id, &draft_id, &text)?
        .ok_or(ApiError::NotFound("Draft not found"))?;
    Ok(Json(serde_json::to_value(draft)?))
}

/// `DELETE /messages/{id}/drafts/{draft_id}`.
pub(super) async fn delete_message_draft(
    State(state): State<Arc<AppState>>,
    Path((message_id, draft_id)): Path<(String, String)>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    ensure_message_write_allowed(
        &state,
        &headers,
        peer_addr,
        &message_id,
        &format!("drafts/{draft_id}"),
    )?;
    let message = find_message(&state, &message_id)?;
    let _guard = state.owner_message_lock.lock().await;
    if !owner_message_store(&state).delete_draft(&message.id, &draft_id)? {
        return Err(ApiError::NotFound("Draft not found"));
    }
    Ok(Json(json!({ "deleted": true, "id": draft_id })))
}

#[derive(Debug, Deserialize)]
struct ReplyRequest {
    submission_id: String,
    #[serde(default)]
    body: String,
}

pub(super) fn valid_submission_id(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// `POST /messages/{id}/reply` (appendix D3): the overall text and every
/// draft go to the agent as one queued message, exactly once per
/// `submission_id`.
pub(super) async fn reply_to_owner_message(
    State(state): State<Arc<AppState>>,
    Path(message_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    ensure_message_write_allowed(&state, &headers, peer_addr, &message_id, "reply")?;
    let message = find_message(&state, &message_id)?;
    let payload: ReplyRequest = docs::parse_json_body(&body)?;
    let submission_id = payload.submission_id.trim().to_owned();
    if !valid_submission_id(&submission_id) {
        return Err(bad_request(
            "submission_id must be 8-64 letters, digits, '-' or '_'",
        ));
    }
    let _guard = state.owner_message_lock.lock().await;
    let store = owner_message_store(&state);
    if let Some(existing) = store.reply(&submission_id)? {
        if existing.message_id != message.id {
            return Err(conflict("submission_id belongs to another message"));
        }
        return Ok(Json(reply_response(&state, &existing)));
    }
    let Some(recipient) = live_recipient(&state, &message.sender_session_id) else {
        return Err(conflict(NO_RECIPIENT));
    };
    let drafts = store.drafts(&message.id)?;
    let comments = order_reply_comments(&drafts);
    let overall = payload.body.trim().to_owned();
    if overall.is_empty() && comments.is_empty() {
        return Err(bad_request("Nothing to send"));
    }
    let delivered_text =
        render_delivered_text(&state.config.owner_name, &message, &overall, &comments);
    let (reply, inserted) = store.record_reply(&RecordReply {
        submission_id,
        message_id: message.id.clone(),
        body: overall,
        comments,
        delivered_text,
        recipient_session_id: recipient.id.clone(),
        draft_ids: drafts.iter().map(|draft| draft.id.clone()).collect(),
    })?;
    if inserted {
        super::board::request_recompute(&state);
        deliver_now(&state, &recipient.id, &message.id);
    }
    Ok(Json(reply_response(&state, &reply)))
}

/// Delivers what an owner reply just queued without waiting for the next
/// runtime pass. `what` names it in the log.
pub(super) fn deliver_now(state: &AppState, session_id: &str, what: &str) {
    if !state.config.rust_core.runtime_enabled {
        return;
    }
    let runtime = TmuxRuntime::from_app_config(&state.config);
    if let Err(error) = state
        .session_store
        .drain_runtime_pending_messages_for_session(session_id, &runtime)
    {
        eprintln!("Owner message {what}: immediate reply delivery failed: {error:#}");
    }
}

pub(super) fn reply_response(state: &AppState, reply: &OwnerMessageReply) -> Value {
    let name = state
        .session_store
        .get_session(&reply.delivered_to_session_id)
        .ok()
        .flatten()
        .map(session_display_name)
        .unwrap_or_else(|| reply.delivered_to_session_id.clone());
    let mut value = reply_json(reply);
    value["delivered_to_session_name"] = json!(name);
    value
}

/// `POST /messages/{id}/handled`: the owner dealt with a blocking message
/// another way. Nothing goes to the agent.
pub(super) async fn mark_owner_message_handled(
    State(state): State<Arc<AppState>>,
    Path(message_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    ensure_message_write_allowed(&state, &headers, peer_addr, &message_id, "handled")?;
    let message = find_message(&state, &message_id)?;
    if !message.blocking {
        return Err(conflict("Only a blocking message can be marked handled"));
    }
    let _guard = state.owner_message_lock.lock().await;
    owner_message_store(&state).mark_handled(&message.id)?;
    super::board::request_recompute(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// A message as the obligations projection sees it.
pub(super) struct ObligationMessage {
    pub message: OwnerMessage,
    pub state: OwnerMessageState,
}

/// Messages for the obligations projection: stored data and the session
/// registry only.
pub(super) fn obligation_messages(state: &AppState) -> Result<Vec<ObligationMessage>, ApiError> {
    let since = crate::owner_push::format_ts(OffsetDateTime::now_utc() - CARD_MESSAGE_WINDOW);
    let rows = owner_message_store(state).for_obligations(&since)?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let live: BTreeSet<String> = state
        .session_store
        .list_sessions(true)?
        .into_iter()
        .filter(|session| !session_ended(session))
        .map(|session| session.id)
        .collect();
    Ok(rows
        .into_iter()
        .map(|(message, replied)| {
            let sender_ended = !live.contains(&message.sender_session_id);
            ObligationMessage {
                state: derive_message_state(&message, replied, sender_ended),
                message,
            }
        })
        .collect())
}

/// Adds each sender's recent messages and its needs-you waiting entries to
/// the projection entries (appendix G, "Agent card").
pub(super) fn project_messages(
    sessions: &mut BTreeMap<String, Value>,
    messages: &[ObligationMessage],
    owner_name: &str,
) {
    let cutoff = crate::owner_push::format_ts(OffsetDateTime::now_utc() - CARD_MESSAGE_WINDOW);
    for entry in messages {
        let message = &entry.message;
        let id = &message.sender_session_id;
        let session = sessions
            .entry(id.clone())
            .or_insert_with(|| super::new_obligation_entry(id));
        if entry.state == OwnerMessageState::NeedsYou {
            session["waiting_on"].as_array_mut().unwrap().push(json!({
                "kind": "owner_message", "id": message.id,
                "label": format!("{owner_name} · {}", message.title),
                "since": message.created_at,
            }));
        }
        let list = session["messages"].as_array_mut().unwrap();
        if message.created_at >= cutoff && list.len() < CARD_MESSAGE_LIMIT {
            list.push(json!({
                "id": message.id,
                "title": message.title,
                "state": entry.state,
                "created_at": message.created_at,
                "reader_path": message_reader_path(&message.id),
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        let now = OffsetDateTime::parse("2026-09-26T10:00:00Z", &Rfc3339).unwrap();
        assert_eq!(relative_time("2026-09-26T09:59:30Z", now), "just now");
        assert_eq!(relative_time("2026-09-26T09:58:00Z", now), "2m ago");
        assert_eq!(relative_time("2026-09-26T07:00:00Z", now), "3h ago");
        assert_eq!(relative_time("2026-09-22T10:00:00Z", now), "4d ago");
        assert_eq!(relative_time("garbage", now), "garbage");
    }

    #[test]
    fn claude_prompt_distinguishes_owner_typing_from_server_deliveries() {
        assert!(crate::owner_messages::is_owner_typed_prompt(
            "Checked in Chrome, merge it.",
            false
        ));
        assert!(!crate::owner_messages::is_owner_typed_prompt("  ", false));
        assert!(!crate::owner_messages::is_owner_typed_prompt(
            "[Input from: Rajesh via sm app] Re: \"Check\" (msg_3f9a2c1d)\nYes.",
            false
        ));
        assert!(!crate::owner_messages::is_owner_typed_prompt(
            "[sm queue] job finished",
            false
        ));
        assert!(!crate::owner_messages::is_owner_typed_prompt(
            "Started early by Rajesh",
            true
        ));
        assert!(!crate::owner_messages::is_owner_typed_prompt(
            "/rename sm-1782",
            true
        ));
    }
}
