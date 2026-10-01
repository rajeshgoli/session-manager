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
pub(super) mod work_threads;
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
use crate::turn_messages::{FinishedRow, ThreadReply};
use work_threads::ThreadCatalog;

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
    first_line(text, 120)
}

/// First non-blank line of `text`, cut at `limit` characters.
fn first_line(text: &str, limit: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    match line.char_indices().nth(limit) {
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
    /// `needs_you`, `finished`, `new` or `earlier`.
    pub group: &'static str,
    pub preview: String,
    pub newest_at: String,
    pub message_count: usize,
    pub revision_count: usize,
    pub doc_count: usize,
    pub open_asks: usize,
    pub url: String,
    pub done: bool,
    pub session_id: Option<String>,
    pub doc_id: Option<String>,
    pub folded_by: Option<&'static str>,
    pub agents: Vec<String>,
    /// Items in the thread, for Done.
    #[serde(skip)]
    pub items: usize,
}

fn group_rank(group: &str) -> u8 {
    match group {
        "needs_you" => 0,
        "finished" => 1,
        "new" => 2,
        _ => 3,
    }
}

/// Everything the Inbox reads, gathered once per request.
struct World {
    sessions: BTreeMap<String, SessionRecord>,
    messages: Vec<OwnerMessage>,
    replies: Vec<OwnerMessageReply>,
    notes: Vec<OwnerNote>,
    follows: Vec<Follow>,
    /// `sm task-complete` rows, oldest first (spec 1782 D3).
    finished: Vec<FinishedRow>,
    /// Agents' answers to the owner's sends (sm#1844), oldest first.
    agent_replies: Vec<ThreadReply>,
    marks: BTreeMap<String, ThreadMarks>,
}

impl World {
    fn load(state: &AppState) -> Result<Self, ApiError> {
        let store = owner_message_store(state);
        let inbox = inbox_store(state);
        let marks = inbox.marks()?;
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
            finished: state
                .session_store
                .turn_message_store()
                .map(|store| store.finished())
                .transpose()?
                .unwrap_or_default(),
            agent_replies: state
                .session_store
                .turn_message_store()
                .map(|store| store.thread_replies())
                .transpose()?
                .unwrap_or_default(),
            marks,
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
                    author: author.clone(),
                    group,
                    preview: doc_preview(summary, fact.latest_verdict.as_deref()),
                    done: is_done(&marks, items),
                    newest_at,
                    message_count: 0,
                    revision_count: summary.publish_count,
                    doc_count: 1,
                    open_asks: usize::from(group == "needs_you"),
                    url: doc_reader_path(summary),
                    session_id: None,
                    doc_id: Some(summary.doc.id.clone()),
                    folded_by: None,
                    agents: author.clone().into_iter().collect(),
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
    let rows = ThreadCatalog::load(state)?.rows(state)?;
    let cutoff = format_ts(OffsetDateTime::now_utc() - OPEN_WINDOW);
    // Needs-you and unread Finished threads stay open until dealt with.
    let open = |row: &InboxRow| {
        row.group == "folded"
            || (!row.done
                && (row.doc_id.is_some()
                    || matches!(row.group, "needs_you" | "finished")
                    || row.newest_at >= cutoff))
    };
    let needs_you_count = rows
        .iter()
        .filter(|row| open(row) && row.group == "needs_you")
        .count();
    let has_new = rows.iter().any(|row| open(row) && row.group == "new");
    let newest_first = |a: &InboxRow, b: &InboxRow| b.newest_at.cmp(&a.newest_at);
    let mut rows: Vec<InboxRow> = match filter {
        "done" => rows
            .into_iter()
            .filter(|row| row.done && row.folded_by.is_none())
            .collect(),
        "docs" => rows
            .into_iter()
            .filter(|row| row.doc_id.is_some())
            .collect(),
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
    if let Some(shell) = web::shell_page(&state, &request) {
        return Ok(shell);
    }
    let (rows, needs_you_count, has_new) = inbox_listing(&state, filter)?;
    if query.format.as_deref() == Some("json") || web::wants_json(&request) {
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
.lbl.a { color: var(--ka); } .lbl.nw { color: #F0ABFC; } .lbl.fin { color: var(--kc); }
.card.fin { border-left-color: var(--kc); }
button.done { font: 11px var(--mono); color: var(--kt2); background: var(--k2); border: 1px solid var(--kl);
  border-radius: 999px; padding: 1px 8px; cursor: pointer; }
.empty { color: var(--kt3); margin: 24px 4px; }
.fold { margin: 18px 0; }
.fold > summary { cursor: pointer; font: 12px var(--mono); color: var(--kt2); padding: 8px 4px; }
"#;

fn render_row(row: &InboxRow, now: OffsetDateTime, show_done: bool) -> String {
    let edge = match row.group {
        "needs_you" => "a",
        "finished" => "fin",
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
    sub.extend(row.agents.iter().map(|name| escape_html(name)));
    if row.doc_count > 0 {
        sub.push(format!(
            "{} docs, {} revisions",
            row.doc_count, row.revision_count
        ));
    }
    let done_button = if show_done && !row.done {
        format!(
            r#" <button class="done" data-key="{}">Done</button>"#,
            escape_html(&row.thread_key)
        )
    } else {
        String::new()
    };
    let archive_button = if row.folded_by == Some("archived") {
        format!(
            r#" <button class="unarchive" data-key="{}">Unarchive</button>"#,
            escape_html(&row.thread_key)
        )
    } else {
        format!(
            r#" <button class="archive" data-key="{}">Archive</button>"#,
            escape_html(&row.thread_key)
        )
    };
    format!(
        r#"<div class="card {edge}"><a class="row-card" href="{url}"><div class="row">{tag}<span class="t">{title}</span><span class="sp"></span><span class="m">{when}</span></div><div class="p">{preview}</div></a><div class="row"><span class="m">{sub}</span><span class="sp"></span>{done_button}{archive_button}</div></div>"#,
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
            ("finished", "Finished", "fin"),
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
        let folded: Vec<_> = rows.iter().filter(|row| row.group == "folded").collect();
        if !folded.is_empty() {
            let names = folded
                .iter()
                .take(3)
                .map(|row| escape_html(&row.title))
                .collect::<Vec<_>>()
                .join(", ");
            body.push_str(&format!(r#"<details class="fold" id="folded"><summary>Folded · {} threads ({names}{more})</summary>"#, folded.len(), more = if folded.len() > 3 { ", …" } else { "" }));
            for row in folded {
                body.push_str(&render_row(row, now, false));
            }
            body.push_str("</details>");
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
    Answered(&'a OwnerMessage),
    Reply(&'a OwnerMessageReply),
    Note(&'a OwnerNote),
    Follow(&'a Follow),
    /// A Finished row's turn message.
    Turn(&'a FinishedRow),
    /// The agent's answer to an owner send.
    AgentReply(&'a ThreadReply),
    Doc(
        &'a crate::owner_docs::OwnerDoc,
        &'a crate::owner_docs::OwnerDocPublish,
    ),
}

impl Item<'_> {
    fn at(&self) -> String {
        norm(match self {
            Item::Message(message, _) => &message.created_at,
            Item::Answered(message) => message.handled_at.as_deref().unwrap_or(&message.created_at),
            Item::Reply(reply) => &reply.created_at,
            Item::Note(note) => &note.created_at,
            Item::Follow(follow) => follow.fired_at.as_deref().unwrap_or(&follow.created_at),
            Item::Turn(row) => row.text_at.as_deref().unwrap_or(&row.completed_at),
            Item::AgentReply(reply) => &reply.at,
            Item::Doc(_, publish) => &publish.published_at,
        })
    }

    /// The thread JSON's entry: `html` is the item's bubble, except a turn,
    /// whose `html` is its message alone for the client to label.
    fn json(&self, world: &World, session_id: &str, now: OffsetDateTime) -> Value {
        match self {
            Item::Turn(row) => json!({
                "type": "turn", "at": self.at(), "finished": true,
                "html": crate::owner_doc_render::render_markdown_sanitized(
                    row.text.as_deref().unwrap_or("")),
            }),
            Item::AgentReply(reply) => json!({
                "type": "turn", "at": self.at(), "finished": false,
                "html": crate::owner_doc_render::render_markdown_sanitized(&reply.text),
            }),
            _ => json!({"at": self.at(), "html": render_item(self, world, session_id, now)}),
        }
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
.b.turn { border-left: 3px solid var(--kc); }
.b.turn .lbl { font: 11.5px var(--mono); color: var(--kc); margin-bottom: 4px; }
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
        Item::Answered(message) => {
            let label = match message.handled_via.as_deref() {
                Some("terminal") => "You answered in the terminal",
                Some("claude_prompt") => "You answered in Claude",
                Some("codex_prompt") => "You answered in Codex",
                Some("inbox") => "Answered in the Inbox",
                _ => "Marked answered",
            };
            format!(
                "<div class=\"ev\">{label} · {}</div>",
                when(message.handled_at.as_deref().unwrap_or(&message.created_at))
            )
        }
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
                r#"<div class="b me" id="{id}">{quotes}<div class="body">{body}</div><div class="m">You · {when}{to}</div></div>"#,
                id = escape_html(&reply.id),
                body = escape_html(reply.body.trim()),
                when = when(&reply.created_at),
                to = delivered_to(&reply.delivered_to_session_id),
            )
        }
        Item::Note(note) => format!(
            r#"<div class="b me" id="{id}"><div class="body">{body}</div><div class="m">You · {when}{to}</div></div>"#,
            id = escape_html(&note.id),
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
        Item::Turn(row) => format!(
            r#"<div class="b turn"><div class="lbl">Last turn · {when}</div><div class="md">{body}</div></div>"#,
            when = when(item.at().as_str()),
            body = crate::owner_doc_render::render_markdown_sanitized(
                row.text.as_deref().unwrap_or("")
            ),
        ),
        Item::AgentReply(reply) => format!(
            r#"<div class="b turn"><div class="lbl">Reply · {when}</div><div class="md">{body}</div></div>"#,
            when = when(item.at().as_str()),
            body = crate::owner_doc_render::render_markdown_sanitized(&reply.text),
        ),
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
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let key = work_threads::key_for_agent(&state, &session_id)?;
    let mut path = work_threads::path_for_key(&key);
    if let Some(query) = request.uri().query() {
        path.push('?');
        path.push_str(query);
    }
    Ok(axum::response::Redirect::temporary(&path).into_response())
}

/// Open "PR #n has no reviewer" asks from this agent, for the thread's
/// Retry now, Change policy and Review it myself buttons (1768 G6).
fn review_asks(state: &AppState, session_id: &str) -> Result<Vec<Value>, ApiError> {
    let db = expand_home(&state.config.sm_send.db_path);
    let mut asks = Vec::new();
    for (request_id, message_id) in
        owner_message_store(state).open_keyed(session_id, "review-no-reviewer:")?
    {
        let Some(request) =
            RetainedQueueStore::get_codex_review_request_from_path(&db, &request_id)?
        else {
            continue;
        };
        if request.state == "no_reviewer" {
            asks.push(json!({
                "request_id": request.id, "message_id": message_id,
                "repo": request.repo, "pr_number": request.pr_number,
            }));
        }
    }
    Ok(asks)
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
    if !["agent:", "doc:", "docpath:", "ticket:", "pr:"]
        .iter()
        .any(|prefix| key.starts_with(prefix))
    {
        return Err(bad_request("Invalid thread_key"));
    }
    let canonical = ThreadCatalog::load(&state)?
        .canonical_key(key)
        .ok_or(ApiError::NotFound("Thread not found"))?;
    work_threads::mark_done(&state, &canonical).await?;
    Ok(Json(json!({ "thread_key": canonical, "done": true })))
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
    send_to_agent(&state, &session_id, payload, None).await
}

async fn send_to_agent(
    state: &Arc<AppState>,
    session_id: &str,
    payload: SendRequest,
    thread_key: Option<&str>,
) -> Result<Json<Value>, ApiError> {
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
    let store = owner_message_store(state);
    let inbox = inbox_store(state);
    let world = World::load(state)?;
    let thread_messages: Option<BTreeSet<String>> = thread_key
        .map(|key| work_threads::ThreadCatalog::load(state).map(|catalog| catalog.message_ids(key)))
        .transpose()?;
    let own: Vec<&OwnerMessage> = world
        .messages
        .iter()
        .filter(|m| {
            thread_messages
                .as_ref()
                .map_or(m.sender_session_id == session_id, |ids| ids.contains(&m.id))
        })
        .collect();
    if let Some(existing) = store.reply(&submission_id)? {
        if !own.iter().any(|m| m.id == existing.message_id) {
            return Err(conflict("submission_id belongs to another thread"));
        }
        return Ok(Json(sent_json(state, "reply", Some(&existing), None)));
    }
    if let Some(existing) = inbox.note(&submission_id)? {
        if existing.session_id != session_id
            || thread_key.is_some_and(|key| existing.thread_key.as_deref() != Some(key))
        {
            return Err(conflict("submission_id belongs to another thread"));
        }
        return Ok(Json(sent_json(state, "note", None, Some(&existing))));
    }
    if own.is_empty() && !world.sessions.contains_key(session_id) {
        return Err(ApiError::NotFound("Thread not found"));
    }
    let Some(recipient) = live_recipient(state, session_id) else {
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
    let selected: Vec<_> = own
        .iter()
        .copied()
        .filter(|m| m.sender_session_id == session_id)
        .collect();
    let target = selected
        .iter()
        .rev()
        .find(|m| world.message_state(m, &replied) == OwnerMessageState::NeedsYou)
        .or_else(|| {
            selected
                .iter()
                .rev()
                .find(|m| quoted_ids.contains(m.id.as_str()))
        })
        .or_else(|| selected.last())
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
                deliver_now(state, &recipient.id, &message.id);
            }
            sent_json(state, "reply", Some(&reply), None)
        }
        None => {
            let (note, inserted) = inbox.record_note(&OwnerNote {
                id: submission_id,
                session_id: session_id.to_owned(),
                body: text,
                delivered_text,
                delivered_to_session_id: recipient.id.clone(),
                created_at: String::new(),
                thread_key: thread_key.map(str::to_owned),
            })?;
            if inserted {
                deliver_now(state, &recipient.id, &format!("note to {session_id}"));
            }
            sent_json(state, "note", None, Some(&note))
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
