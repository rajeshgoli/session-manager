//! The owner Inbox (sm#1647): everything agents address to the owner, one
//! row per thread. An agent thread holds one agent's messages, the owner's
//! replies and notes, and follow results; a doc thread is one doc across its
//! revisions. `GET /inbox` lists them (HTML, or `?format=json` for the app),
//! `GET /inbox/agent/{session_id}` is an agent thread's page, and Done
//! clears a thread without messaging anyone. Spec:
//! `specs/1647_owner_inbox.html`.

use super::docs::{doc_reader_path, owner_doc_store, DOC_TOKEN_HEADER};
use super::messages::{
    deliver_now, live_recipient, owner_message_store, relative_time, reply_response, session_ended,
    valid_submission_id, NO_RECIPIENT,
};
use super::*;
use crate::owner_doc_render::{inline_json, render_markdown_with_lines};
use crate::owner_docs::{
    doc_readable_path, escape_html, page_shell_with_status, OwnerDocState, OwnerDocSummary,
    OwnerDocVerdict,
};
use crate::owner_inbox::{
    agent_thread_key, doc_thread_key, OwnerInboxStore, OwnerNote, ThreadMarks,
};
use crate::owner_messages::{
    derive_message_state, render_thread_text, OwnerMessage, OwnerMessageReply, OwnerMessageState,
    RecordReply, ReplyComment,
};
use crate::owner_push::{format_ts, notification_for, parse_ts, Follow, REASON_JOB_FINISHED};

/// The Open list holds threads with activity this recent, plus every thread
/// that needs the owner.
const OPEN_WINDOW: time::Duration = time::Duration::days(30);
/// Rows the Docs and Done lists return, newest first.
const LIST_LIMIT: usize = 200;
/// Quoted passages one send may carry.
const MAX_QUOTES: usize = 100;
/// The signed page token's subject: one token covers every Inbox write.
const TOKEN_SUBJECT: &str = "inbox";
const INBOX_CLIENT_JS: &str = include_str!("inbox_client.js");

pub(super) fn inbox_store(state: &AppState) -> OwnerInboxStore {
    OwnerInboxStore::new(expand_home(&state.config.sm_send.db_path))
}

pub(super) fn agent_thread_path(session_id: &str) -> String {
    format!("/inbox/agent/{session_id}")
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

/// Stored times in one text form, so they compare as strings.
fn norm(timestamp: &str) -> String {
    parse_ts(timestamp).map_or_else(|| timestamp.to_owned(), format_ts)
}

/// First line of `text`, cut at 120 characters.
fn snippet(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    match line.char_indices().nth(120) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

/// One Inbox row (appendix B, "Row").
#[derive(Debug, Clone, Serialize)]
pub(super) struct InboxRow {
    pub thread_key: String,
    /// `agent` or `doc`.
    pub kind: &'static str,
    pub title: String,
    pub repo: String,
    /// Agent: `live` or `ended`. Doc: its derived state.
    pub status: String,
    /// A doc's latest posted verdict: `approve`, `changes_requested`, `comment`.
    pub verdict: Option<String>,
    pub pr_number: Option<i64>,
    /// A doc's latest revision's publisher.
    pub author: Option<String>,
    /// `needs_you`, `new` or `earlier`.
    pub group: &'static str,
    pub preview: String,
    pub newest_at: String,
    pub message_count: usize,
    pub revision_count: usize,
    pub open_asks: usize,
    pub url: String,
    pub done: bool,
    pub session_id: Option<String>,
    pub doc_id: Option<String>,
    /// Items in the thread, for Done.
    #[serde(skip)]
    pub items: usize,
}

fn group_rank(group: &str) -> u8 {
    match group {
        "needs_you" => 0,
        "new" => 1,
        _ => 2,
    }
}

/// Everything the Inbox reads, gathered once per request.
struct World {
    sessions: BTreeMap<String, SessionRecord>,
    messages: Vec<OwnerMessage>,
    replies: Vec<OwnerMessageReply>,
    notes: Vec<OwnerNote>,
    follows: Vec<Follow>,
    marks: BTreeMap<String, ThreadMarks>,
}

impl World {
    fn load(state: &AppState) -> Result<Self, ApiError> {
        let store = owner_message_store(state);
        let inbox = inbox_store(state);
        Ok(Self {
            sessions: state
                .session_store
                .list_sessions(true)?
                .into_iter()
                .map(|session| (session.id.clone(), session))
                .collect(),
            messages: store.all()?,
            replies: store.all_replies()?,
            notes: inbox.notes()?,
            follows: super::follows::push_store(state).fired()?,
            marks: inbox.marks()?,
        })
    }

    fn live(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .is_some_and(|session| !session_ended(session))
    }

    fn replied(&self) -> BTreeSet<&str> {
        self.replies
            .iter()
            .map(|reply| reply.message_id.as_str())
            .collect()
    }

    fn message_state(&self, message: &OwnerMessage, replied: &BTreeSet<&str>) -> OwnerMessageState {
        derive_message_state(
            message,
            replied.contains(message.id.as_str()),
            !self.live(&message.sender_session_id),
        )
    }

    fn marks(&self, key: &str) -> ThreadMarks {
        self.marks.get(key).cloned().unwrap_or_default()
    }

    /// The agent's name: the registry's, else the newest one stored with
    /// its messages or follows, else its id.
    fn agent_name(&self, session_id: &str) -> String {
        if let Some(session) = self.sessions.get(session_id) {
            return session_display_name(session.clone());
        }
        self.messages
            .iter()
            .rev()
            .find(|message| message.sender_session_id == session_id)
            .map(|message| message.sender_session_name.clone())
            .or_else(|| {
                self.follows
                    .iter()
                    .rev()
                    .find(|follow| follow.session_id == session_id)
                    .map(|follow| follow.session_name.clone())
            })
            .unwrap_or_else(|| session_id.to_owned())
    }

    fn agent_repo(&self, session_id: &str) -> String {
        self.sessions
            .get(session_id)
            .and_then(|session| {
                std::path::Path::new(&session.working_dir)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default()
    }

    /// Agent threads: one per session with a message, a note, or a fired
    /// follow. Doc publishes alone make no agent thread; the doc has its own.
    fn agent_rows(&self) -> Vec<InboxRow> {
        let replied = self.replied();
        let sender_of: BTreeMap<&str, &str> = self
            .messages
            .iter()
            .map(|message| (message.id.as_str(), message.sender_session_id.as_str()))
            .collect();
        let mut sessions = BTreeSet::new();
        sessions.extend(self.messages.iter().map(|m| m.sender_session_id.as_str()));
        sessions.extend(self.notes.iter().map(|note| note.session_id.as_str()));
        sessions.extend(self.follows.iter().map(|follow| follow.session_id.as_str()));
        sessions
            .into_iter()
            .map(|session_id| {
                let messages: Vec<(&OwnerMessage, OwnerMessageState)> = self
                    .messages
                    .iter()
                    .filter(|message| message.sender_session_id == session_id)
                    .map(|message| (message, self.message_state(message, &replied)))
                    .collect();
                // Newest item and its preview.
                let mut newest: Option<(String, String)> = None;
                let mut consider = |at: &str, preview: String| {
                    let at = norm(at);
                    if newest.as_ref().is_none_or(|(best, _)| at >= *best) {
                        newest = Some((at, preview));
                    }
                };
                let mut items = messages.len();
                for (message, _) in &messages {
                    consider(&message.created_at, message.title.clone());
                }
                for reply in self
                    .replies
                    .iter()
                    .filter(|reply| sender_of.get(reply.message_id.as_str()) == Some(&session_id))
                {
                    let text = if reply.body.trim().is_empty() {
                        reply
                            .comments
                            .first()
                            .map(|c| c.quote.as_str())
                            .unwrap_or("")
                    } else {
                        reply.body.as_str()
                    };
                    consider(&reply.created_at, format!("You: {}", snippet(text)));
                    items += 1;
                }
                for note in self.notes.iter().filter(|n| n.session_id == session_id) {
                    consider(&note.created_at, format!("You: {}", snippet(&note.body)));
                    items += 1;
                }
                let marks = self.marks(&agent_thread_key(session_id));
                let mut follows = 0;
                for follow in self.follows.iter().filter(|f| f.session_id == session_id) {
                    let fired_at = follow.fired_at.as_deref().unwrap_or(&follow.created_at);
                    consider(fired_at, follow_line(follow).0);
                    items += 1;
                    follows += 1;
                }
                // Fired follows are listed oldest first, so any beyond the
                // count seen at the last read are new.
                let unread_follow = follows
                    > marks
                        .read_follows
                        .and_then(|seen| usize::try_from(seen).ok())
                        .unwrap_or(0);
                let (newest_at, mut preview) = newest.unwrap_or_default();
                let open_asks = messages
                    .iter()
                    .filter(|(_, state)| *state == OwnerMessageState::NeedsYou)
                    .count();
                let group = if open_asks > 0 {
                    if let Some((message, _)) = messages
                        .iter()
                        .rev()
                        .find(|(_, state)| *state == OwnerMessageState::NeedsYou)
                    {
                        preview = message.title.clone();
                    }
                    "needs_you"
                } else if unread_follow
                    || messages
                        .iter()
                        .any(|(_, state)| *state == OwnerMessageState::New)
                {
                    "new"
                } else {
                    "earlier"
                };
                InboxRow {
                    thread_key: agent_thread_key(session_id),
                    kind: "agent",
                    title: self.agent_name(session_id),
                    repo: self.agent_repo(session_id),
                    status: if self.live(session_id) {
                        "live"
                    } else {
                        "ended"
                    }
                    .to_owned(),
                    verdict: None,
                    pr_number: None,
                    author: None,
                    group,
                    preview,
                    done: is_done(&marks, items),
                    newest_at,
                    message_count: messages.len(),
                    revision_count: 0,
                    open_asks,
                    url: agent_thread_path(session_id),
                    session_id: Some(session_id.to_owned()),
                    doc_id: None,
                    items,
                }
            })
            .collect()
    }

    fn doc_rows(&self, state: &AppState) -> Result<Vec<InboxRow>, ApiError> {
        let store = owner_doc_store(state);
        let facts = store.inbox_facts()?;
        Ok(store
            .summaries(None, false)?
            .iter()
            .map(|summary| {
                let mut fact = facts.get(&summary.doc.id).cloned().unwrap_or_default();
                // A verdict describes the revision it was given on; a newer
                // revision the owner only read has none.
                if summary.state != OwnerDocState::Reviewed {
                    fact.latest_verdict = None;
                }
                let mut newest_at = norm(&summary.published_at);
                if let Some(at) = fact.latest_review_at.as_deref().map(norm) {
                    newest_at = newest_at.max(at);
                }
                let author_live = self.live(&fact.latest_session_id);
                let group = match summary.state {
                    OwnerDocState::ReviewRequested if author_live => "needs_you",
                    OwnerDocState::ReviewRequested
                    | OwnerDocState::New
                    | OwnerDocState::Updated => "new",
                    OwnerDocState::Reviewed | OwnerDocState::Read => "earlier",
                };
                let author = if fact.latest_session_id.is_empty() {
                    summary.doc.author_session_name.clone()
                } else if self.sessions.contains_key(&fact.latest_session_id) {
                    Some(self.agent_name(&fact.latest_session_id))
                } else if fact.latest_session_id == summary.doc.author_session_id {
                    summary.doc.author_session_name.clone()
                } else {
                    Some(fact.latest_session_id.clone())
                };
                let marks = self.marks(&doc_thread_key(&summary.doc.id));
                let items = summary.publish_count + fact.review_count;
                InboxRow {
                    thread_key: doc_thread_key(&summary.doc.id),
                    kind: "doc",
                    title: summary.doc.title.clone(),
                    repo: crate::owner_docs::repo_name(&summary.doc.repo).to_owned(),
                    status: summary.state.as_str().to_owned(),
                    verdict: fact.latest_verdict.clone(),
                    pr_number: summary.doc.pr_number,
                    author,
                    group,
                    preview: doc_preview(summary, fact.latest_verdict.as_deref()),
                    done: is_done(&marks, items),
                    newest_at,
                    message_count: 0,
                    revision_count: summary.publish_count,
                    open_asks: usize::from(group == "needs_you"),
                    url: doc_reader_path(summary),
                    session_id: None,
                    doc_id: Some(summary.doc.id.clone()),
                    items,
                }
            })
            .collect())
    }
}

/// Done while the thread holds no more items than when it was marked.
fn is_done(marks: &ThreadMarks, items: usize) -> bool {
    marks
        .done_items
        .is_some_and(|done| usize::try_from(done).is_ok_and(|done| items <= done))
}

fn verdict_label(verdict: &str) -> &'static str {
    match OwnerDocVerdict::parse(verdict) {
        Some(OwnerDocVerdict::Approve) => "Approved",
        Some(OwnerDocVerdict::ChangesRequested) => "Changes requested",
        Some(OwnerDocVerdict::Comment) | None => "Commented",
    }
}

fn doc_preview(summary: &OwnerDocSummary, verdict: Option<&str>) -> String {
    match summary.state {
        OwnerDocState::ReviewRequested => {
            format!("Review requested · revision {}", summary.publish_count)
        }
        OwnerDocState::New if summary.publish_count <= 1 => {
            "Published · for your information".to_owned()
        }
        OwnerDocState::New | OwnerDocState::Updated => {
            format!("Revision {} published", summary.publish_count)
        }
        OwnerDocState::Reviewed | OwnerDocState::Read => match verdict {
            Some(verdict) => format!("You reviewed · {}", verdict_label(verdict)),
            None => "Read".to_owned(),
        },
    }
}

/// A follow result as one line, and the report it links, if any.
fn follow_line(follow: &Follow) -> (String, Option<String>) {
    let notification = notification_for(follow);
    let text = if follow.fire_reason.as_deref() == Some(REASON_JOB_FINISHED) {
        format!("Queue job {} · {}", notification.title, notification.body)
    } else {
        format!("{} · {}", notification.title, notification.body)
    };
    (text, follow.report_reader_path.clone())
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct InboxQuery {
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    format: Option<String>,
}

/// The rows a filter shows, in order, plus the badge values (which always
/// count the Open list).
fn inbox_listing(state: &AppState, filter: &str) -> Result<(Vec<InboxRow>, usize, bool), ApiError> {
    let world = World::load(state)?;
    let mut rows = world.agent_rows();
    rows.extend(world.doc_rows(state)?);
    let cutoff = format_ts(OffsetDateTime::now_utc() - OPEN_WINDOW);
    let open = |row: &InboxRow| !row.done && (row.group == "needs_you" || row.newest_at >= cutoff);
    let needs_you_count = rows
        .iter()
        .filter(|row| open(row) && row.group == "needs_you")
        .count();
    let has_new = rows.iter().any(|row| open(row) && row.group == "new");
    let newest_first = |a: &InboxRow, b: &InboxRow| b.newest_at.cmp(&a.newest_at);
    let mut rows: Vec<InboxRow> = match filter {
        "done" => rows.into_iter().filter(|row| row.done).collect(),
        "docs" => rows.into_iter().filter(|row| row.kind == "doc").collect(),
        _ => rows.into_iter().filter(|row| open(row)).collect(),
    };
    if filter == "open" {
        rows.sort_by(|a, b| {
            group_rank(a.group)
                .cmp(&group_rank(b.group))
                .then_with(|| newest_first(a, b))
        });
    } else {
        rows.sort_by(newest_first);
        rows.truncate(LIST_LIMIT);
    }
    Ok((rows, needs_you_count, has_new))
}

fn html(body: String) -> Response {
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, "text/html; charset=utf-8".to_owned()),
            (CACHE_CONTROL, "private, no-cache".to_owned()),
        ],
        Body::from(body),
    )
        .into_response()
}

/// `GET /inbox`: the list, as a page or `?format=json`. `?filter=` is
/// `open` (default), `docs` or `done`.
pub(super) async fn get_inbox(
    State(state): State<Arc<AppState>>,
    Query(query): Query<InboxQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let filter = match query.filter.as_deref() {
        None | Some("" | "open") => "open",
        Some("docs") => "docs",
        Some("done") => "done",
        Some(_) => return Err(bad_request("filter must be open, docs or done")),
    };
    let (rows, needs_you_count, has_new) = inbox_listing(&state, filter)?;
    if query.format.as_deref() == Some("json") {
        return Ok(Json(json!({
            "filter": filter,
            "needs_you_count": needs_you_count,
            "has_new": has_new,
            "rows": rows,
        }))
        .into_response());
    }
    Ok(html(render_inbox_page(
        &state,
        filter,
        &rows,
        needs_you_count,
    )))
}

const INBOX_STYLE: &str = r#"
.filters { display: flex; gap: 6px; margin: 0 0 10px; }
.filters a { font: 12px var(--mono); padding: 2px 10px; border-radius: 999px; border: 1px solid var(--kl); color: var(--kt2); }
.filters a.on { color: var(--kc); border-color: #143744; background: #0f2530; }
.row-card { display: block; }
.row-card .row { flex-wrap: nowrap; }
.row-card .t { font-weight: 600; min-width: 0; flex: 0 1 auto; }
.row-card .m { flex-shrink: 0; }
.row-card .p { margin: 2px 0; }
.card.nw { border-left-color: #F0ABFC; }
.lbl.a { color: var(--ka); } .lbl.nw { color: #F0ABFC; }
button.done { font: 11px var(--mono); color: var(--kt2); background: var(--k2); border: 1px solid var(--kl);
  border-radius: 999px; padding: 1px 8px; cursor: pointer; }
.empty { color: var(--kt3); margin: 24px 4px; }
"#;

fn render_row(row: &InboxRow, now: OffsetDateTime, show_done: bool) -> String {
    let edge = match row.group {
        "needs_you" => "a",
        "new" => "nw",
        _ => "",
    };
    let tag = if row.kind == "doc" {
        r#"<span class="chip v">DOC</span> "#
    } else {
        ""
    };
    let mut sub = Vec::new();
    if !row.repo.is_empty() {
        sub.push(escape_html(&row.repo));
    }
    if row.kind == "doc" {
        if let Some(pr) = row.pr_number {
            sub.push(format!("PR #{pr}"));
        }
        if let Some(author) = &row.author {
            sub.push(escape_html(author));
        }
    } else {
        if row.message_count > 0 {
            sub.push(format!(
                "{} message{}",
                row.message_count,
                if row.message_count == 1 { "" } else { "s" }
            ));
        }
        sub.push(
            match (row.group, row.status.as_str()) {
                ("needs_you", _) => "asks you",
                (_, "ended") => "agent ended",
                _ if row.preview.starts_with("You: ") => "you replied",
                _ => "for your information",
            }
            .to_owned(),
        );
    }
    let done_button = if show_done && !row.done {
        format!(
            r#" <button class="done" data-key="{}">Done</button>"#,
            escape_html(&row.thread_key)
        )
    } else {
        String::new()
    };
    format!(
        r#"<div class="card {edge}"><a class="row-card" href="{url}"><div class="row">{tag}<span class="t">{title}</span><span class="sp"></span><span class="m">{when}</span></div><div class="p">{preview}</div></a><div class="row"><span class="m">{sub}</span><span class="sp"></span>{done_button}</div></div>"#,
        url = escape_html(&row.url),
        title = escape_html(&row.title),
        when = escape_html(&relative_time(&row.newest_at, now)),
        preview = escape_html(&row.preview),
        sub = sub.join(" · "),
    )
}

fn render_inbox_page(
    state: &AppState,
    filter: &str,
    rows: &[InboxRow],
    needs_you_count: usize,
) -> String {
    let now = OffsetDateTime::now_utc();
    let chip = |name: &str, label: &str| {
        let class = if name == filter { " class=\"on\"" } else { "" };
        format!(r#"<a{class} href="/inbox?filter={name}">{label}</a>"#)
    };
    let mut body = format!(
        r#"<style>{INBOX_STYLE}</style><div class="filters">{}{}{}</div>"#,
        chip("open", "Open"),
        chip("docs", "Docs"),
        chip("done", "Done")
    );
    if rows.is_empty() {
        body.push_str(r#"<p class="empty">Nothing here.</p>"#);
    } else if filter == "open" {
        for (group, label, class) in [
            ("needs_you", "Needs you", "a"),
            ("new", "New", "nw"),
            ("earlier", "Earlier", ""),
        ] {
            let group_rows: Vec<&InboxRow> = rows.iter().filter(|r| r.group == group).collect();
            if group_rows.is_empty() {
                continue;
            }
            body.push_str(&format!(
                r#"<h2 class="lbl {class}">{label} · {}</h2>"#,
                group_rows.len()
            ));
            for row in group_rows {
                body.push_str(&render_row(row, now, true));
            }
        }
    } else {
        for row in rows {
            body.push_str(&render_row(row, now, filter != "done"));
        }
    }
    let config = json!({
        "token": docs::issue_doc_token(&state.config, TOKEN_SUBJECT),
        "page": "list",
    });
    body.push_str(&format!(
        "<script>{INBOX_CLIENT_JS}({});</script>",
        inline_json(&config)
    ));
    let status = if needs_you_count > 0 {
        format!(r#"<span class="chip a">{needs_you_count} need you</span>"#)
    } else {
        String::new()
    };
    page_shell_with_status("Inbox", "inbox", &status, &body)
}

/// One entry on an agent thread's page.
enum Item<'a> {
    Message(&'a OwnerMessage, OwnerMessageState),
    Reply(&'a OwnerMessageReply),
    Note(&'a OwnerNote),
    Follow(&'a Follow),
    Doc(
        &'a crate::owner_docs::OwnerDoc,
        &'a crate::owner_docs::OwnerDocPublish,
    ),
}

impl Item<'_> {
    fn at(&self) -> String {
        norm(match self {
            Item::Message(message, _) => &message.created_at,
            Item::Reply(reply) => &reply.created_at,
            Item::Note(note) => &note.created_at,
            Item::Follow(follow) => follow.fired_at.as_deref().unwrap_or(&follow.created_at),
            Item::Doc(_, publish) => &publish.published_at,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ThreadQuery {
    #[serde(default)]
    at: Option<String>,
    /// Open at the newest item even on a message's own URL (after a send).
    #[serde(default)]
    bottom: Option<String>,
}

const THREAD_STYLE: &str = r#"
.th-head { display: flex; align-items: baseline; gap: 8px; flex-wrap: wrap; margin: 0 0 12px; }
.th-head .big { font-size: 17px; }
.items { display: flex; flex-direction: column; gap: 8px; padding-bottom: 16px; }
.b { max-width: 88%; border-radius: 12px; padding: 8px 11px; background: var(--k2); overflow-wrap: anywhere; }
.b.me { align-self: flex-end; background: #143744; }
.b.ask { border: 1px solid var(--ka); }
.b.hl { box-shadow: 0 0 0 2px var(--kc); }
.b h3 { margin: 2px 0 4px; font-size: 14.5px; }
.b .m { margin-top: 4px; }
.b.me .body { white-space: pre-wrap; }
.b blockquote, .md blockquote { margin: 4px 0; padding-left: 10px; border-left: 3px solid #F0ABFC; color: var(--kt2); white-space: pre-wrap; }
.ev { align-self: center; font: 12px var(--mono); color: var(--kt3); text-align: center; max-width: 92%; }
.ev a { text-decoration: underline dotted var(--kt3); }
.md { font-size: 14px; line-height: 1.5; }
.md > :first-child { margin-top: 0; } .md > :last-child { margin-bottom: 0; }
.md a { color: var(--kc); text-decoration: underline; }
.md pre { overflow-x: auto; background: var(--k0); padding: 8px; border-radius: 6px; }
.md code { font-family: var(--mono); font-size: .92em; }
.md table { border-collapse: collapse; display: block; overflow-x: auto; }
.md th, .md td { border: 1px solid var(--kl); padding: 2px 6px; }
.md img { max-width: 100%; }
.can-quote .md [data-sm-line] { cursor: pointer; }
.md .quoted { background: rgba(240,171,252,.14); border-radius: 4px; }
.compose { position: sticky; bottom: 0; background: var(--k1); border-top: 1px solid var(--kl);
  padding: 8px; display: flex; flex-direction: column; gap: 6px; border-radius: 10px 10px 0 0; }
.compose textarea { width: 100%; min-height: 3.2em; max-height: 40vh; resize: vertical; font: 15px/1.4 var(--sans);
  color: var(--kt); background: var(--k3); border: 1px solid var(--kl); border-radius: 8px; padding: 7px 9px; }
.qs { display: flex; flex-direction: column; gap: 4px; }
.qc { display: flex; gap: 6px; align-items: flex-start; background: var(--k3); border-left: 3px solid #F0ABFC;
  padding: 4px 8px; border-radius: 6px; color: var(--kt2); font-size: 13px; }
.qc span { flex: 1; max-height: 4.2em; overflow: hidden; }
.qc button { background: none; border: 0; color: var(--kt3); cursor: pointer; font-size: 15px; }
.btns { display: flex; gap: 8px; align-items: center; }
.btns button { font: 13px var(--sans); border-radius: 999px; padding: 5px 16px; cursor: pointer;
  border: 1px solid var(--kl); background: none; color: var(--kt2); }
.btns button.send { background: var(--kc); color: #09090D; border-color: var(--kc); font-weight: 700; }
.btns button:disabled { opacity: .5; cursor: default; }
.btns .msg { flex: 1; font-size: 12.5px; color: var(--kt3); }
.btns .msg.err { color: var(--kr); }
.hint { font: 11.5px var(--mono); color: var(--kt3); }
"#;

fn message_state_label(state: OwnerMessageState) -> &'static str {
    match state {
        OwnerMessageState::New => "new",
        OwnerMessageState::Read => "read",
        OwnerMessageState::NeedsYou => "needs you",
        OwnerMessageState::Replied => "replied",
        OwnerMessageState::Handled => "handled",
    }
}

fn render_item(item: &Item<'_>, world: &World, session_id: &str, now: OffsetDateTime) -> String {
    let when = |at: &str| escape_html(&relative_time(at, now));
    let delivered_to = |to: &str| {
        if to == session_id {
            String::new()
        } else {
            format!(" · to {}", escape_html(&world.agent_name(to)))
        }
    };
    match item {
        Item::Message(message, state) => {
            let ask = *state == OwnerMessageState::NeedsYou;
            format!(
                r#"<div class="b{ask_class}" id="{id}">{chip}<h3>{title}</h3><div class="md" data-msg="{id}">{body}</div><div class="m">{when} · {state}</div></div>"#,
                ask_class = if ask { " ask" } else { "" },
                id = escape_html(&message.id),
                chip = if ask {
                    r#"<span class="chip a">NEEDS YOU</span>"#
                } else {
                    ""
                },
                title = escape_html(&message.title),
                body = render_markdown_with_lines(&message.body_markdown),
                when = when(&message.created_at),
                state = message_state_label(*state),
            )
        }
        Item::Reply(reply) => {
            let quotes: String = reply
                .comments
                .iter()
                .map(|comment| {
                    let mut part = String::new();
                    if !comment.quote.trim().is_empty() {
                        part.push_str(&format!(
                            "<blockquote>{}</blockquote>",
                            escape_html(comment.quote.trim())
                        ));
                    }
                    if !comment.body.trim().is_empty() {
                        part.push_str(&format!(
                            r#"<div class="body">{}</div>"#,
                            escape_html(comment.body.trim())
                        ));
                    }
                    part
                })
                .collect();
            format!(
                r#"<div class="b me">{quotes}<div class="body">{body}</div><div class="m">You · {when}{to}</div></div>"#,
                body = escape_html(reply.body.trim()),
                when = when(&reply.created_at),
                to = delivered_to(&reply.delivered_to_session_id),
            )
        }
        Item::Note(note) => format!(
            r#"<div class="b me"><div class="body">{body}</div><div class="m">You · {when}{to}</div></div>"#,
            body = escape_html(note.body.trim()),
            when = when(&note.created_at),
            to = delivered_to(&note.delivered_to_session_id),
        ),
        Item::Follow(follow) => {
            let (text, report) = follow_line(follow);
            let at = follow.fired_at.as_deref().unwrap_or(&follow.created_at);
            match report {
                Some(path) => format!(
                    r#"<div class="ev"><a href="{}">{}</a> · {}</div>"#,
                    escape_html(&path),
                    escape_html(&text),
                    when(at)
                ),
                None => format!(
                    r#"<div class="ev">{} · {}</div>"#,
                    escape_html(&text),
                    when(at)
                ),
            }
        }
        Item::Doc(doc, publish) => format!(
            r#"<div class="ev">{what} <a href="{url}">{title}</a> · {when}</div>"#,
            what = if publish.review_requested {
                "Asked for review:"
            } else {
                "Published:"
            },
            url = escape_html(&doc_readable_path(
                &doc.repo,
                &doc.path,
                &publish.commit_sha
            )),
            title = escape_html(&doc.title),
            when = when(&publish.published_at),
        ),
    }
}

/// `GET /inbox/agent/{session_id}`: the agent thread's page. Opening it
/// marks the agent's messages viewed and the thread read.
pub(super) async fn get_agent_thread(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Query(query): Query<ThreadQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let at = query.at.filter(|_| query.bottom.is_none());
    agent_thread_page(&state, &session_id, at)
}

/// The thread page for `session_id`, scrolled to message `at`. Also what
/// `/messages/{id}` serves, so apps that only know that path keep the page
/// in their reader.
pub(super) fn agent_thread_page(
    state: &AppState,
    session_id: &str,
    at: Option<String>,
) -> Result<Response, ApiError> {
    let session_id = session_id.to_owned();
    let world = World::load(state)?;
    let has_items = world
        .messages
        .iter()
        .any(|m| m.sender_session_id == session_id)
        || world.notes.iter().any(|n| n.session_id == session_id)
        || world.follows.iter().any(|f| f.session_id == session_id);
    if !has_items && !world.sessions.contains_key(&session_id) {
        return Err(ApiError::NotFound("Thread not found"));
    }
    let follows_seen = world
        .follows
        .iter()
        .filter(|f| f.session_id == session_id)
        .count();
    owner_message_store(state).mark_sender_viewed(&session_id)?;
    inbox_store(state).mark_read(&agent_thread_key(&session_id), follows_seen)?;
    let world = World::load(state)?;
    let replied = world.replied();
    let message_ids: BTreeSet<&str> = world
        .messages
        .iter()
        .filter(|m| m.sender_session_id == session_id)
        .map(|m| m.id.as_str())
        .collect();
    let mut items: Vec<Item<'_>> = Vec::new();
    for message in world
        .messages
        .iter()
        .filter(|m| m.sender_session_id == session_id)
    {
        items.push(Item::Message(
            message,
            world.message_state(message, &replied),
        ));
    }
    items.extend(
        world
            .replies
            .iter()
            .filter(|reply| message_ids.contains(reply.message_id.as_str()))
            .map(Item::Reply),
    );
    items.extend(
        world
            .notes
            .iter()
            .filter(|note| note.session_id == session_id)
            .map(Item::Note),
    );
    items.extend(
        world
            .follows
            .iter()
            .filter(|follow| follow.session_id == session_id)
            .map(Item::Follow),
    );
    let publishes = owner_doc_store(state).publishes_by_session(&session_id, 100)?;
    items.extend(
        publishes
            .iter()
            .map(|(doc, publish)| Item::Doc(doc, publish)),
    );
    // Stable: equal times keep the order above (messages before replies).
    items.sort_by_key(Item::at);
    let now = OffsetDateTime::now_utc();
    let name = world.agent_name(&session_id);
    let live = world.live(&session_id);
    let recipient = live_recipient(state, &session_id);
    let repo = world.agent_repo(&session_id);
    let mut body = format!(
        r#"<style>{THREAD_STYLE}</style><div class="th-head"><a class="dim" href="/inbox">‹ Inbox</a><span class="big">{name}</span><span class="m"><span class="dot {dot}"></span>{status}{repo}</span></div><div class="items{quote_class}">"#,
        name = escape_html(&name),
        dot = if live { "working" } else { "retired" },
        status = if live { "live" } else { "ended" },
        repo = if repo.is_empty() {
            String::new()
        } else {
            format!(" · {}", escape_html(&repo))
        },
        quote_class = if recipient.is_some() {
            " can-quote"
        } else {
            ""
        },
    );
    if items.is_empty() {
        body.push_str(r#"<div class="ev">Nothing yet. Write below to start.</div>"#);
    }
    for item in &items {
        body.push_str(&render_item(item, &world, &session_id, now));
    }
    body.push_str("</div>");
    let reply_to = recipient.map(session_display_name);
    body.push_str(r#"<div class="compose">"#);
    match &reply_to {
        Some(to) => {
            let hint = if live {
                "Tap a paragraph to quote it.".to_owned()
            } else {
                format!("This agent has ended; a reply goes to {}.", escape_html(to))
            };
            body.push_str(&format!(
                r#"<div class="qs" id="qs"></div><textarea id="box" rows="2" placeholder="Write to {to}"></textarea><div class="btns"><button id="done">Done</button><span class="msg" id="msg">{hint}</span><button class="send" id="send">Send</button></div>"#,
                to = escape_html(to),
            ));
        }
        None => body.push_str(
            r#"<div class="btns"><button id="done">Done</button><span class="msg" id="msg">No agent is left to reply to.</span></div>"#,
        ),
    }
    body.push_str("</div>");
    let config = json!({
        "page": "thread",
        "token": docs::issue_doc_token(&state.config, TOKEN_SUBJECT),
        "sessionId": session_id,
        "threadKey": agent_thread_key(&session_id),
        "canSend": reply_to.is_some(),
        "at": at,
    });
    body.push_str(&format!(
        "<script>{INBOX_CLIENT_JS}({});</script>",
        inline_json(&config)
    ));
    Ok(html(page_shell_with_status(&name, "inbox", "", &body)))
}

/// Inbox writes: the page's signed token, or the ordinary session guard
/// (the app).
fn ensure_inbox_write_allowed(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    path: &str,
) -> Result<(), ApiError> {
    let token_ok = header_text(headers, DOC_TOKEN_HEADER)
        .is_some_and(|token| docs::doc_token_valid(&state.config, TOKEN_SUBJECT, &token));
    if !token_ok {
        ensure_session_allowed_from_parts(&state.config, headers, Some(peer_addr), path)?;
    }
    ensure_core_writes_enabled(state)
}

#[derive(Debug, Deserialize)]
struct DoneRequest {
    thread_key: String,
}

/// `POST /inbox/done`: the owner has dealt with a thread. Every open blocking
/// message in an agent thread becomes Handled and its unread messages read;
/// an open review request on a doc becomes No review needed. Nothing goes
/// to any agent. Anything newer in the thread brings it back.
pub(super) async fn post_done(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    ensure_inbox_write_allowed(&state, &headers, peer_addr, "/inbox/done")?;
    let payload: DoneRequest = docs::parse_json_body(&body)?;
    let key = payload.thread_key.trim();
    let items;
    if let Some(session_id) = key.strip_prefix("agent:") {
        let _guard = state.owner_message_lock.lock().await;
        let world = World::load(&state)?;
        let row = world
            .agent_rows()
            .into_iter()
            .find(|row| row.thread_key == key);
        if row.is_none() && !world.sessions.contains_key(session_id) {
            return Err(ApiError::NotFound("Thread not found"));
        }
        items = row.map_or(0, |row| row.items);
        let replied = world.replied();
        let store = owner_message_store(&state);
        for message in world.messages.iter().filter(|m| {
            m.sender_session_id == session_id
                && m.blocking
                && m.handled_at.is_none()
                && !replied.contains(m.id.as_str())
        }) {
            store.mark_handled(&message.id)?;
        }
        store.mark_sender_viewed(session_id)?;
    } else if let Some(doc_id) = key.strip_prefix("doc:") {
        let _guard = state.owner_doc_review_lock.lock().await;
        let store = owner_doc_store(&state);
        let summary = store
            .summary(doc_id)?
            .ok_or(ApiError::NotFound("Thread not found"))?;
        if summary.state == OwnerDocState::ReviewRequested {
            store.dismiss_review(doc_id)?;
        }
        items = summary.publish_count
            + store
                .inbox_facts()?
                .get(doc_id)
                .map_or(0, |fact| fact.review_count);
    } else {
        return Err(bad_request("thread_key must be agent:<id> or doc:<id>"));
    }
    inbox_store(&state).mark_done(key, items)?;
    Ok(Json(json!({ "thread_key": key, "done": true })))
}

#[derive(Debug, Deserialize)]
struct QuoteRequest {
    message_id: String,
    quote: String,
}

#[derive(Debug, Deserialize)]
struct SendRequest {
    submission_id: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    quotes: Vec<QuoteRequest>,
}

/// `POST /inbox/agent/{session_id}/send`: the text and quotes go to the
/// agent as one queued message, exactly once per `submission_id`. The send
/// is recorded as a reply to the newest open blocking message, else the
/// newest quoted message, else the agent's newest message; an agent that
/// has sent nothing gets a note.
pub(super) async fn post_agent_send(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    ensure_inbox_write_allowed(
        &state,
        &headers,
        peer_addr,
        &format!("{}/send", agent_thread_path(&session_id)),
    )?;
    let payload: SendRequest = docs::parse_json_body(&body)?;
    let submission_id = payload.submission_id.trim().to_owned();
    if !valid_submission_id(&submission_id) {
        return Err(bad_request(
            "submission_id must be 8-64 letters, digits, '-' or '_'",
        ));
    }
    if payload.quotes.len() > MAX_QUOTES {
        return Err(bad_request(format!(
            "A send carries at most {MAX_QUOTES} quotes"
        )));
    }
    let text = payload.body.trim().to_owned();
    if !text.is_empty() {
        docs::validated_draft_body(&text)?;
    }
    let _guard = state.owner_message_lock.lock().await;
    let store = owner_message_store(&state);
    let inbox = inbox_store(&state);
    let world = World::load(&state)?;
    let own: Vec<&OwnerMessage> = world
        .messages
        .iter()
        .filter(|m| m.sender_session_id == session_id)
        .collect();
    if let Some(existing) = store.reply(&submission_id)? {
        if !own.iter().any(|m| m.id == existing.message_id) {
            return Err(conflict("submission_id belongs to another thread"));
        }
        return Ok(Json(sent_json(&state, "reply", Some(&existing), None)));
    }
    if let Some(existing) = inbox.note(&submission_id)? {
        if existing.session_id != session_id {
            return Err(conflict("submission_id belongs to another thread"));
        }
        return Ok(Json(sent_json(&state, "note", None, Some(&existing))));
    }
    if own.is_empty() && !world.sessions.contains_key(&session_id) {
        return Err(ApiError::NotFound("Thread not found"));
    }
    let Some(recipient) = live_recipient(&state, &session_id) else {
        return Err(conflict(NO_RECIPIENT));
    };
    let mut quotes = Vec::new();
    let mut quoted_ids = BTreeSet::new();
    for quote in &payload.quotes {
        let text = quote.quote.trim();
        if text.is_empty() {
            continue;
        }
        if text.chars().count() > docs::MAX_DRAFT_QUOTE {
            return Err(bad_request(format!(
                "Quotes are limited to {} characters",
                docs::MAX_DRAFT_QUOTE
            )));
        }
        if !own.iter().any(|m| m.id == quote.message_id) {
            return Err(bad_request("A quote must come from this thread's messages"));
        }
        quoted_ids.insert(quote.message_id.as_str());
        quotes.push(text.to_owned());
    }
    if text.is_empty() && quotes.is_empty() {
        return Err(bad_request("Nothing to send"));
    }
    let replied = world.replied();
    let target = own
        .iter()
        .rev()
        .find(|m| world.message_state(m, &replied) == OwnerMessageState::NeedsYou)
        .or_else(|| {
            own.iter()
                .rev()
                .find(|m| quoted_ids.contains(m.id.as_str()))
        })
        .or_else(|| own.last())
        .copied();
    let delivered_text = render_thread_text(&state.config.owner_name, target, &text, &quotes);
    let response = match target {
        Some(message) => {
            let (reply, inserted) = store.record_reply(&RecordReply {
                submission_id,
                message_id: message.id.clone(),
                body: text,
                comments: quotes
                    .into_iter()
                    .map(|quote| ReplyComment {
                        line: None,
                        quote,
                        body: String::new(),
                    })
                    .collect(),
                delivered_text,
                recipient_session_id: recipient.id.clone(),
                draft_ids: Vec::new(),
            })?;
            if inserted {
                deliver_now(&state, &recipient.id, &message.id);
            }
            sent_json(&state, "reply", Some(&reply), None)
        }
        None => {
            let (note, inserted) = inbox.record_note(&OwnerNote {
                id: submission_id,
                session_id: session_id.clone(),
                body: text,
                delivered_text,
                delivered_to_session_id: recipient.id.clone(),
                created_at: String::new(),
            })?;
            if inserted {
                deliver_now(&state, &recipient.id, &format!("note to {session_id}"));
            }
            sent_json(&state, "note", None, Some(&note))
        }
    };
    Ok(Json(response))
}

fn sent_json(
    state: &AppState,
    kind: &str,
    reply: Option<&OwnerMessageReply>,
    note: Option<&OwnerNote>,
) -> Value {
    let mut value = match (reply, note) {
        (Some(reply), _) => {
            let mut value = reply_response(state, reply);
            value["message_id"] = json!(reply.message_id);
            value
        }
        (None, Some(note)) => {
            let name = state
                .session_store
                .get_session(&note.delivered_to_session_id)
                .ok()
                .flatten()
                .map(session_display_name)
                .unwrap_or_else(|| note.delivered_to_session_id.clone());
            json!({
                "id": note.id,
                "body": note.body,
                "delivered_text": note.delivered_text,
                "delivered_to_session_id": note.delivered_to_session_id,
                "delivered_to_session_name": name,
                "created_at": note.created_at,
                "message_id": null,
            })
        }
        (None, None) => json!({}),
    };
    value["kind"] = json!(kind);
    value
}
