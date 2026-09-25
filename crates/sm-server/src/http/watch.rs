//! Web watch (sm#1452, ticket #1489): `GET /` and `GET /watch`, the
//! browser version of `sm watch`, and `GET /watch/state`, the JSON it
//! renders. Read-only and behind the owner page gate. The page is painted
//! on the server, so it works with scripts off; an inline script
//! (`watch_client.js`) refetches the state and re-renders the cards with
//! the same markup as [`render_sessions`].

use super::history::html_response;
use super::*;
use crate::owner_docs::{escape_html, page_shell_with_status, repo_name};
use crate::queue::QueueJobRecord;
use crate::watch_view::{
    array, display_state, filter_sessions, lead_claim, name, repo, s, tree_order,
};
use crate::work_claims::{HolderState, SessionDirectory};

pub const WATCH_SCHEMA_VERSION: i64 = 1;

const CLIENT_JS: &str = include_str!("watch_client.js");

#[derive(Debug, Default, Deserialize)]
pub(super) struct WatchParams {
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    top_level: Option<String>,
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    stopped: Option<String>,
}

fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

fn flag(value: &Option<String>) -> bool {
    matches!(nonempty(value), Some("1" | "true" | "yes"))
}

pub(super) async fn get_watch_page(
    State(state): State<Arc<AppState>>,
    Query(params): Query<WatchParams>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let doc = watch_state(&state, &params)?;
    let path = if request.uri().path() == "/watch" {
        "/watch"
    } else {
        "/"
    };
    let body = format!(
        r#"<style>{STYLE}</style>
{bar}<div id="w" data-refresh="{refresh}">{cards}</div>
<script>{CLIENT_JS}</script>"#,
        bar = filter_bar(path, &params),
        refresh = state.config.web_watch.refresh_seconds(),
        cards = render_sessions(&doc),
    );
    Ok(html_response(
        StatusCode::OK,
        page_shell_with_status(
            "sm · Watch",
            "watch",
            &format!(r#"<span class="m" id="ws">{}</span>"#, summary(&doc)),
            &body,
        ),
    ))
}

pub(super) async fn get_watch_state(
    State(state): State<Arc<AppState>>,
    Query(params): Query<WatchParams>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let doc = watch_state(&state, &params)?;
    Ok(([(CACHE_CONTROL, "private, no-cache".to_owned())], Json(doc)).into_response())
}

// ---- state -----------------------------------------------------------------

/// `now` to the millisecond, so the server's and the script's ages agree.
fn generated_at(now: OffsetDateTime) -> String {
    now.replace_nanosecond(now.nanosecond() / 1_000_000 * 1_000_000)
        .unwrap_or(now)
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// The same sources `sm watch` polls (`/sessions`, `/queue-jobs`,
/// `/session-obligations`), read in-process, in `sm watch`'s tree order.
fn watch_state(state: &AppState, params: &WatchParams) -> Result<Value, ApiError> {
    let include_stopped = flag(&params.stopped);
    let records = state.session_store.list_sessions(true)?;
    let directory = SessionDirectory::new(records.iter().map(claims::session_info));
    let context: BTreeMap<String, Option<f64>> = records
        .iter()
        .map(|record| (record.id.clone(), record.context_used_percentage))
        .collect();
    let sessions: Vec<Value> = records
        .into_iter()
        .filter(|record| include_stopped || !record.is_stopped())
        .map(|record| serde_json::to_value(session_response_with_live_activity(state, record)))
        .collect::<Result<_, _>>()?;

    let feed = session_obligations(state)?;
    let obligations: BTreeMap<String, Value> = array(&feed, "sessions")
        .into_iter()
        .map(|entry| (s(&entry, "session_id").to_owned(), entry))
        .collect();
    let queue_path = expand_home(&state.config.queue_runner_state_dir().to_string_lossy())
        .join("queue_runner.db");
    let jobs =
        RetainedQueueStore::list_queue_jobs_from_path(&queue_path, QueueJobFilters::default())?;
    let colliding = colliding_sessions(state, &directory)?;

    let repo_filter = nonempty(&params.repo).map(|value| {
        if value.starts_with('~') {
            expand_home(value).to_string_lossy().into_owned()
        } else {
            value.to_owned()
        }
    });
    let mut listed = filter_sessions(
        &sessions,
        repo_filter.as_deref(),
        nonempty(&params.role),
        "",
    );
    if let Some(node) = nonempty(&params.node) {
        listed.retain(|v| s(v, "node") == node);
    }
    let top_level = flag(&params.top_level);

    let mut out = Vec::new();
    let mut live = 0;
    let mut waiting_on_owner = 0;
    for entry in tree_order(&listed) {
        if top_level && entry.depth > 0 {
            continue;
        }
        let v = &listed[entry.index];
        let id = s(v, "id");
        let obligation = obligations.get(id);
        let state = match display_state(v, obligation) {
            _ if s(v, "status") == "stopped" => "stopped",
            "working" | "thinking" => "working",
            "waiting" => "waiting",
            "stopped" => "stopped",
            _ => "idle",
        };
        let field = |key: &str| obligation.map_or_else(|| json!([]), |o| o[key].clone());
        let waiting_on = field("waiting_on");
        if state != "stopped" {
            live += 1;
        }
        if waiting_on
            .as_array()
            .is_some_and(|items| items.iter().any(|item| s(item, "kind") == "owner_review"))
        {
            waiting_on_owner += 1;
        }
        let own_jobs: Vec<Value> = jobs
            .iter()
            .filter(|job| owns(job, id))
            .map(|job| {
                json!({"id": job.id, "type": job.job_type, "label": job.label,
                       "state": job.state, "started_at": job.started_at,
                       "queued_at": job.queued_at})
            })
            .collect();
        let optional = |key: &str| {
            let value = s(v, key);
            (!value.is_empty()).then(|| value.to_owned())
        };
        out.push(json!({
            "id": id,
            "name": name(v),
            "provider": s(v, "provider"),
            "role": optional("role"),
            "state": state,
            "activity_state": s(v, "activity_state"),
            "status_text": optional("agent_status_text"),
            "status_at": optional("agent_status_at"),
            "last_activity": optional("last_activity"),
            "parent_session_id": optional("parent_session_id"),
            "depth": entry.depth,
            "group": entry.group,
            "repo": repo(v),
            "node": s(v, "node"),
            "context_percent": context.get(id).copied().flatten(),
            "claims": field("claims"),
            "docs": field("docs"),
            "waiting_on": waiting_on,
            "review_history": field("review_history"),
            "jobs": own_jobs,
            "collision": colliding.contains(id),
            "attach": format!("sm attach {}", name(v)),
        }));
    }
    Ok(json!({
        "schema_version": WATCH_SCHEMA_VERSION,
        "generated_at": generated_at(OffsetDateTime::now_utc()),
        "sessions": out,
        "counts": {"live": live, "waiting_on_owner": waiting_on_owner},
    }))
}

/// `sm watch` lists a job under the agent that asked for it, or under the
/// agent it notifies when nobody is named.
fn owns(job: &QueueJobRecord, id: &str) -> bool {
    match job
        .requester_session_id
        .as_deref()
        .filter(|r| !r.is_empty())
    {
        Some(requester) => requester == id,
        None => job.notify_session_id.as_deref() == Some(id),
    }
}

/// Sessions holding an active claim that a live holder outside their line
/// also holds (the history page's "2 agents").
fn colliding_sessions(
    state: &AppState,
    directory: &SessionDirectory,
) -> Result<BTreeSet<String>, ApiError> {
    let live = |id: &str| {
        directory
            .get(id)
            .is_some_and(|info| matches!(info.state, HolderState::Working | HolderState::Idle))
    };
    let mut holders = BTreeMap::<(String, i64), Vec<String>>::new();
    for view in claims::work_claim_store(state).active_claims()? {
        if live(&view.claim.session_id) {
            holders
                .entry((view.claim.repo.clone(), view.claim.number))
                .or_default()
                .push(view.claim.session_id);
        }
    }
    let mut colliding = BTreeSet::new();
    for ids in holders.values() {
        for a in ids {
            if ids
                .iter()
                .any(|b| a != b && directory.relation(a, b).is_none())
            {
                colliding.insert(a.clone());
            }
        }
    }
    Ok(colliding)
}

// ---- page ------------------------------------------------------------------

const STYLE: &str = "\
details.card>summary{list-style:none;cursor:pointer}\
details.card>summary::-webkit-details-marker{display:none}\
.nm{font-weight:700}.st{display:block;margin-top:2px;color:var(--kt2)}\
.dot.waiting{background:var(--ka)}.amb{color:var(--ka)}\
.grp{font:11px var(--mono);color:var(--kt3);margin:14px 0 6px 2px}\
.cp{cursor:copy;background:var(--k2);border-radius:5px;padding:1px 6px}\
.cp.ok{color:var(--kg)}#ws.stale{color:var(--ka)}";

/// `6 live · 1 waiting on you`.
fn summary(doc: &Value) -> String {
    let live = doc["counts"]["live"].as_u64().unwrap_or(0);
    let waiting = doc["counts"]["waiting_on_owner"].as_u64().unwrap_or(0);
    if waiting > 0 {
        format!("{live} live · {waiting} waiting on you")
    } else {
        format!("{live} live")
    }
}

fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// `path` with the current filters, overridden by `changes`.
fn watch_href(path: &str, params: &WatchParams, changes: &[(&str, Option<&str>)]) -> String {
    let current = [
        ("repo", nonempty(&params.repo)),
        ("role", nonempty(&params.role)),
        ("node", nonempty(&params.node)),
        ("top_level", flag(&params.top_level).then_some("1")),
        ("stopped", flag(&params.stopped).then_some("1")),
    ];
    let query: Vec<String> = current
        .into_iter()
        .filter_map(|(key, value)| {
            let value = changes
                .iter()
                .find(|(k, _)| *k == key)
                .map_or(value, |(_, v)| *v)?;
            Some(format!("{key}={}", encode_component(value)))
        })
        .collect();
    if query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{}", query.join("&"))
    }
}

/// Live / With stopped, and a chip per filter with a clear link.
fn filter_bar(path: &str, params: &WatchParams) -> String {
    let stopped = flag(&params.stopped);
    let mut html = format!(
        r#"<div class="bar"><a class="{}" href="{}">Live</a><a class="{}" href="{}">With stopped</a>"#,
        if stopped { "tab" } else { "tab on" },
        escape_html(&watch_href(path, params, &[("stopped", None)])),
        if stopped { "tab on" } else { "tab" },
        escape_html(&watch_href(path, params, &[("stopped", Some("1"))])),
    );
    for (key, value) in [
        ("repo", nonempty(&params.repo)),
        ("role", nonempty(&params.role)),
        ("node", nonempty(&params.node)),
        (
            "top_level",
            flag(&params.top_level).then_some("top level only"),
        ),
    ] {
        if let Some(value) = value {
            let label = if key == "top_level" {
                value.to_owned()
            } else {
                format!("{key}: {value}")
            };
            html.push_str(&format!(
                r#"<span class="chip">{} <a href="{}" title="clear">✕</a></span>"#,
                escape_html(&label),
                escape_html(&watch_href(path, params, &[(key, None)])),
            ));
        }
    }
    html.push_str("</div>\n");
    html
}

/// `45s`, `12m`, `3h`, `2d` from `at` to `now`, both taken to the
/// millisecond (as the script does).
fn age(at: &str, now: i128) -> String {
    let Some(at) = crate::work_history::parse_time(at) else {
        return "?".to_owned();
    };
    let seconds = ((now - at.unix_timestamp_nanos() / 1_000_000).max(0) / 1000) as i64;
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

fn state_chip(state: &str) -> String {
    let tone = match state {
        "open" => "c",
        "merged" | "closed" => "g",
        _ => "",
    };
    format!(r#"<span class="chip {tone}">{}</span>"#, escape_html(state))
}

/// A link that leaves sm (`https://` only), marked ↗.
fn external(href: &str, label: &str) -> String {
    if href.starts_with("https://") {
        format!(
            r#"<a class="mt lk" href="{}">{} ↗</a>"#,
            escape_html(href),
            escape_html(label)
        )
    } else {
        format!(r#"<span class="mt">{}</span>"#, escape_html(label))
    }
}

fn sections(rows: &[(&str, String)]) -> String {
    let body: String = rows
        .iter()
        .filter(|(_, html)| !html.is_empty())
        .map(|(label, html)| format!(r#"<span class="lbl">{label}</span><span>{html}</span>"#))
        .collect();
    if body.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="sec">{body}</div>"#)
    }
}

/// The cards, with a repo line before each repo's first top-level session.
/// `watch_client.js` renders the same markup from the same JSON.
pub(super) fn render_sessions(doc: &Value) -> String {
    let now = crate::work_history::parse_time(s(doc, "generated_at"))
        .map_or(0, |t| t.unix_timestamp_nanos() / 1_000_000);
    let sessions = array(doc, "sessions");
    if sessions.is_empty() {
        return r#"<p class="dim">No live sessions.</p>"#.to_owned();
    }
    let mut html = String::new();
    for v in &sessions {
        if let Some(group) = v["group"].as_str() {
            html.push_str(&format!(r#"<div class="grp">{}</div>"#, escape_html(group)));
        }
        html.push_str(&card(v, now));
    }
    html
}

fn card(v: &Value, now: i128) -> String {
    let waiting_on = array(v, "waiting_on");
    let owner = waiting_on
        .iter()
        .any(|item| s(item, "kind") == "owner_review");
    let collision = v["collision"].as_bool() == Some(true);
    let edge = if collision {
        "r"
    } else if owner {
        "a"
    } else {
        "c"
    };
    let mut chips = String::new();
    if let Some((lead, more)) = lead_claim(v) {
        let kind = if s(&lead, "kind") == "pr" { "PR " } else { "" };
        let more = if more > 0 {
            format!(" +{more}")
        } else {
            String::new()
        };
        chips.push_str(&format!(
            r#" <span class="chip c">{kind}#{}{more}</span>"#,
            lead["number"].as_i64().unwrap_or_default()
        ));
    }
    let docs = array(v, "docs");
    if !docs.is_empty() {
        let unread = crate::watch_view::unread_doc_count(v);
        let note = if docs.iter().any(|d| s(d, "state") == "review_requested") {
            " · review requested".to_owned()
        } else if unread > 0 {
            format!(" · {unread} new")
        } else {
            String::new()
        };
        chips.push_str(&format!(
            r#" <span class="chip v">docs {}{note}</span>"#,
            docs.len()
        ));
    }
    if owner {
        chips.push_str(r#" <span class="chip a">waiting on you</span>"#);
    }
    if collision {
        chips.push_str(r#" <span class="chip r">2 agents</span>"#);
    }
    let status = match s(v, "status_text") {
        "" => String::new(),
        text => format!(r#"<span class="st">{}</span>"#, escape_html(text)),
    };
    let depth = v["depth"].as_u64().unwrap_or(0).min(6);
    format!(
        r#"<details class="card {edge}" data-id="{id}" style="margin-left:{indent}px"><summary><span class="row"><span class="dot {state}"></span><span class="mt nm">{name}</span><span class="m">{provider} · {state} {age}</span>{chips}</span>{status}</summary>{sections}</details>"#,
        id = escape_html(s(v, "id")),
        indent = depth * 14,
        state = escape_html(s(v, "state")),
        name = escape_html(s(v, "name")),
        provider = escape_html(s(v, "provider")),
        age = age(s(v, "last_activity"), now),
        sections = sections(&[
            ("Work", work_html(v)),
            ("Docs", docs_html(&docs, now)),
            ("Reviews", reviews_html(v, &waiting_on, now)),
            ("Jobs", jobs_html(v, now)),
            ("Attach", attach_html(v)),
        ]),
    )
}

fn work_html(v: &Value) -> String {
    let history = array(v, "review_history");
    let mut parts: Vec<String> = Vec::new();
    let mut worktrees: Vec<String> = Vec::new();
    for claim in array(v, "claims") {
        let number = claim["number"].as_i64().unwrap_or_default();
        if s(&claim, "kind") == "pr" {
            let requested = history
                .iter()
                .find(|h| {
                    s(h, "repo").eq_ignore_ascii_case(s(&claim, "repo"))
                        && h["pr_number"].as_i64() == Some(number)
                })
                .and_then(|h| h["request_count"].as_u64())
                .unwrap_or(0);
            let codex = if requested > 0 {
                format!(r#" <span class="m">{requested} Codex</span>"#)
            } else {
                String::new()
            };
            parts.push(format!(
                "{} {}{codex}",
                external(s(&claim, "url"), &format!("PR #{number}")),
                state_chip(s(&claim, "state"))
            ));
        } else {
            parts.push(format!(
                r#"<a class="mt lk" href="{}">ticket #{number}</a> {}"#,
                escape_html(s(&claim, "history_path")),
                state_chip(s(&claim, "state"))
            ));
        }
        let worktree = s(&claim, "worktree_path");
        if !worktree.is_empty() && !worktrees.iter().any(|w| w == worktree) {
            worktrees.push(worktree.to_owned());
        }
    }
    for worktree in worktrees {
        parts.push(format!(
            r#"<span class="m">worktree {}</span>"#,
            escape_html(&worktree)
        ));
    }
    parts.join(r#" <span class="m">·</span> "#)
}

fn docs_html(docs: &[Value], now: i128) -> String {
    docs.iter()
        .map(|doc| {
            let tone = if s(doc, "state") == "review_requested" {
                "a"
            } else {
                "v"
            };
            let undelivered = if doc["review_undelivered"].as_bool() == Some(true) {
                r#" <span class="chip r">review not delivered</span>"#
            } else {
                ""
            };
            format!(
                r#"<a class="lk" href="{}">{}</a> <span class="chip {tone}">{}</span> <span class="m">{}</span>{undelivered}"#,
                escape_html(s(doc, "reader_path")),
                escape_html(s(doc, "title")),
                escape_html(&s(doc, "state").replace('_', " ")),
                age(s(doc, "published_at"), now),
            )
        })
        .collect::<Vec<_>>()
        .join("<br>")
}

/// Waiting entries (a queue job already listed under Jobs is left out),
/// then the Codex review counts per PR.
fn reviews_html(v: &Value, waiting_on: &[Value], now: i128) -> String {
    let jobs = array(v, "jobs");
    let mut lines: Vec<String> = waiting_on
        .iter()
        .filter(|item| {
            !(s(item, "kind") == "queue_job" && jobs.iter().any(|j| s(j, "id") == s(item, "id")))
        })
        .map(|item| {
            format!(
                r#"<span class="{}">{}</span> <span class="m">waiting {}</span>"#,
                if s(item, "kind") == "owner_review" {
                    "amb"
                } else {
                    ""
                },
                escape_html(s(item, "label")),
                age(s(item, "since"), now),
            )
        })
        .collect();
    for history in array(v, "review_history") {
        lines.push(format!(
            r#"<span class="mt">{}#{}</span> <span class="m">{} landed · {} requested</span>"#,
            escape_html(repo_name(s(&history, "repo"))),
            history["pr_number"].as_i64().unwrap_or_default(),
            history["landed_count"].as_u64().unwrap_or(0),
            history["request_count"].as_u64().unwrap_or(0),
        ));
    }
    lines.join("<br>")
}

fn jobs_html(v: &Value, now: i128) -> String {
    array(v, "jobs")
        .iter()
        .map(|job| {
            let since = if s(job, "state") == "running" {
                s(job, "started_at")
            } else {
                s(job, "queued_at")
            };
            format!(
                r#"<span class="mt">[{}] {}</span> <span class="m">{} {}</span>"#,
                escape_html(s(job, "type")),
                escape_html(s(job, "label")),
                escape_html(s(job, "state")),
                age(since, now),
            )
        })
        .collect::<Vec<_>>()
        .join("<br>")
}

fn attach_html(v: &Value) -> String {
    match s(v, "attach") {
        "" => String::new(),
        command => format!(
            r#"<code class="cp mt" data-cp="{0}" title="Click to copy">$ {0} ⧉</code>"#,
            escape_html(command)
        ),
    }
}
