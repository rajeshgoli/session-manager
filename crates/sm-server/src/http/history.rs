//! History pages (sm#1452, ticket #1488): `GET /history`, one card per
//! ticket sm tracks, and `GET /t/<repo-name>/<n>`, one ticket's whole story.
//! Both are server-rendered HTML behind the owner page gate, with
//! `?format=json` for `sm history`. Data assembly lives in
//! `crate::work_history`; this module resolves the request and renders.

use super::*;
use crate::owner_docs::{escape_html, page_shell, repo_name};
use crate::work_claims::SessionDirectory;
use crate::work_history::{
    Cursor, Flag, HistoryData, HistoryPage, HistoryQuery, HistoryRow, Timeline, DEFAULT_LIMIT,
    HISTORY_SCHEMA_VERSION,
};

#[derive(Debug, Default, Deserialize)]
pub(super) struct HistoryParams {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    open: Option<String>,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    format: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct TimelineParams {
    #[serde(default)]
    format: Option<String>,
}

fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

fn history_data(state: &AppState) -> Result<HistoryData, ApiError> {
    Ok(HistoryData::load(&expand_home(
        &state.config.sm_send.db_path,
    ))?)
}

/// The session store read once per request: the claim-rule view plus the
/// records the `agent` filter matches against.
fn load_sessions(state: &AppState) -> Result<(Vec<SessionRecord>, SessionDirectory), ApiError> {
    let records = state.session_store.list_sessions(true)?;
    let directory = SessionDirectory::new(records.iter().map(claims::session_info));
    Ok((records, directory))
}

/// `agent` as a session id, id prefix, alias or name, or a registered
/// role; a retired agent is found by its claim snapshots or doc
/// authorship.
fn agent_ids(
    state: &AppState,
    records: &[SessionRecord],
    data: &HistoryData,
    identifier: &str,
) -> Result<BTreeSet<String>, ApiError> {
    let mut ids = data.sessions_named(identifier);
    if data.knows_session(identifier) {
        ids.insert(identifier.to_owned());
    }
    let live = records.iter().find(|record| {
        record.id == identifier
            || record.aliases.iter().any(|alias| alias == identifier)
            || record.friendly_name.as_deref() == Some(identifier)
            || record.name == identifier
    });
    let prefix = || {
        let mut matches = records
            .iter()
            .filter(|record| identifier.len() >= 8 && record.id.starts_with(identifier));
        match (matches.next(), matches.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        }
    };
    if let Some(record) = live.or_else(prefix) {
        ids.insert(record.id.clone());
    } else if ids.is_empty() {
        // Registered roles (`maintainer`, …) are the last resort: the
        // lookup takes the session store's write lock.
        if let Some(session) = resolve_session_or_registry_role(state, identifier)? {
            ids.insert(session.id);
        }
    }
    Ok(ids)
}

pub(super) async fn get_history(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HistoryParams>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let data = history_data(&state)?;
    let (records, sessions) = load_sessions(&state)?;
    let before = match nonempty(&params.before) {
        Some(cursor) => Some(Cursor::decode(cursor).ok_or_else(|| ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "Invalid before cursor".to_owned(),
        })?),
        None => None,
    };
    let query = HistoryQuery {
        agent_ids: match nonempty(&params.agent) {
            Some(agent) => Some(agent_ids(&state, &records, &data, agent)?),
            None => None,
        },
        repo: nonempty(&params.repo).map(str::to_owned),
        open_only: matches!(nonempty(&params.open), Some("1" | "true" | "yes")),
        before,
        limit: params.limit.unwrap_or(DEFAULT_LIMIT),
    };
    let page = data.list(&sessions, &query, OffsetDateTime::now_utc());
    if params.format.as_deref() == Some("json") {
        let mut body = json!({
            "schema_version": HISTORY_SCHEMA_VERSION,
            "rows": page.rows,
            "next_before": page.next_before,
        });
        if let Some(base) = docs::doc_browser_base_url(&state.config) {
            body["page_url"] = json!(format!("{base}{}", list_href(&params, None)));
        }
        return Ok(Json(body).into_response());
    }
    Ok(html_response(
        StatusCode::OK,
        page_shell("sm · History", "history", &render_list(&params, &page)),
    ))
}

pub(super) async fn get_timeline(
    State(state): State<Arc<AppState>>,
    Path((repo, number)): Path<(String, String)>,
    Query(params): Query<TimelineParams>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let json_format = params.format.as_deref() == Some("json");
    let data = history_data(&state)?;
    let (_, sessions) = load_sessions(&state)?;
    let timeline = number.trim().parse::<i64>().ok().and_then(|number| {
        // Repo names are unique among the owner's repos; if two owners
        // share one, the first slug wins.
        let slug = data.repos_for(&repo, number).into_iter().next()?;
        data.timeline(&sessions, &slug, number, OffsetDateTime::now_utc())
    });
    let Some(timeline) = timeline else {
        if json_format {
            return Err(ApiError::NotFound("Not tracked"));
        }
        let body = format!(
            r#"<p class="big">Not tracked</p><p class="dim">sm has no record of {} #{}.</p>
<p><a class="lk" href="/history">All tickets</a></p>"#,
            escape_html(&repo),
            escape_html(&number)
        );
        return Ok(html_response(
            StatusCode::NOT_FOUND,
            page_shell("sm · Not tracked", "", &body),
        ));
    };
    if json_format {
        let mut body = serde_json::to_value(&timeline)?;
        if let Some(base) = docs::doc_browser_base_url(&state.config) {
            body["page_url"] = json!(format!("{base}{}", timeline.item.history_path));
        }
        return Ok(Json(body).into_response());
    }
    let title = format!(
        "sm · #{} {}",
        timeline.item.number,
        if timeline.item.title.is_empty() {
            repo_name(&timeline.item.repo)
        } else {
            &timeline.item.title
        }
    );
    Ok(html_response(
        StatusCode::OK,
        page_shell(&title, "", &render_timeline(&timeline)),
    ))
}

fn html_response(status: StatusCode, html: String) -> Response {
    (
        status,
        [
            (CONTENT_TYPE, "text/html; charset=utf-8".to_owned()),
            (CACHE_CONTROL, "private, no-cache".to_owned()),
        ],
        Body::from(html),
    )
        .into_response()
}

// ---- links -----------------------------------------------------------------

fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// `/history` with the current filters, overridden by `changes`
/// (`(key, None)` drops a filter). Paging never carries over.
fn history_href(params: &HistoryParams, changes: &[(&str, Option<&str>)]) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (key, value) in [
        ("agent", nonempty(&params.agent)),
        ("repo", nonempty(&params.repo)),
        (
            "open",
            matches!(nonempty(&params.open), Some("1" | "true" | "yes")).then_some("1"),
        ),
    ] {
        let value = changes
            .iter()
            .find(|(k, _)| *k == key)
            .map_or(value, |(_, v)| *v);
        if let Some(value) = value {
            pairs.push((key.to_owned(), value.to_owned()));
        }
    }
    for (key, value) in changes {
        if !matches!(*key, "agent" | "repo" | "open") {
            if let Some(value) = value {
                pairs.push(((*key).to_owned(), (*value).to_owned()));
            }
        }
    }
    if let Some(limit) = params.limit {
        pairs.push(("limit".to_owned(), limit.to_string()));
    }
    if pairs.is_empty() {
        return "/history".to_owned();
    }
    let query = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", encode_component(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("/history?{query}")
}

fn list_href(params: &HistoryParams, before: Option<&str>) -> String {
    history_href(params, &[("before", before)])
}

fn agent_href(session_id: &str) -> String {
    format!("/history?agent={}", encode_component(session_id))
}

/// An external link (`https://` only) that leaves sm, marked ↗.
fn external(href: &str, label: &str, class: &str) -> String {
    if href.starts_with("https://") {
        format!(
            r#"<a class="{class}" href="{}">{} ↗</a>"#,
            escape_html(href),
            escape_html(label)
        )
    } else {
        format!(r#"<span class="{class}">{}</span>"#, escape_html(label))
    }
}

// ---- pieces ----------------------------------------------------------------

fn state_chip(state: &str) -> String {
    let tone = match state {
        "open" => "c",
        "merged" | "closed" => "g",
        _ => "",
    };
    format!(r#"<span class="chip {tone}">{}</span>"#, escape_html(state))
}

fn flag_chips(row: &HistoryRow) -> String {
    row.flags
        .iter()
        .filter_map(|id| Flag::parse(id))
        .map(|flag| {
            let (tone, title) = match flag {
                Flag::TwoAgents => ("r", String::new()),
                Flag::Stale => (
                    "",
                    row.sync_error
                        .clone()
                        .unwrap_or_else(|| match &row.synced_at {
                            Some(at) => format!("last fetched from GitHub {} ago", age(at)),
                            None => "not fetched from GitHub yet".to_owned(),
                        }),
                ),
                _ => ("a", String::new()),
            };
            let title = if title.is_empty() {
                String::new()
            } else {
                format!(r#" title="{}""#, escape_html(&title))
            };
            format!(
                r#"<span class="chip {tone}"{title}>{}</span>"#,
                flag.label()
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Card edge: rose for a collision, amber for anything needing attention,
/// green when done, cyan while open.
fn edge(row: &HistoryRow) -> &'static str {
    if row.has_flag(Flag::TwoAgents) {
        "r"
    } else if row.has_flag(Flag::OpenAfterMerge)
        || row.has_flag(Flag::NoLiveHolder)
        || row.has_flag(Flag::WorktreeLeft)
    {
        "a"
    } else if row.state == "open" {
        "c"
    } else {
        "g"
    }
}

fn title_html(row: &HistoryRow) -> String {
    if row.title.is_empty() {
        r#"<span class="dim">(not fetched from GitHub yet)</span>"#.to_owned()
    } else {
        escape_html(&row.title)
    }
}

fn agents_html(row: &HistoryRow) -> String {
    row.agents
        .iter()
        .map(|agent| {
            // A claim that ended because the work closed or merged is the
            // normal end and needs no note; one that was handed off does.
            let ended = match (agent.end_reason.as_deref(), agent.state.as_str()) {
                (_, "retired") => r#" <span class="m">(retired)</span>"#.to_owned(),
                (Some(reason @ ("released" | "taken" | "superseded")), _) => {
                    format!(r#" <span class="m">({reason})</span>"#)
                }
                _ => String::new(),
            };
            format!(
                r#"<span><span class="dot {state}" title="{state}"></span><a class="mt lk" href="{href}">{name}</a>{ended}</span>"#,
                state = escape_html(&agent.state),
                href = escape_html(&agent_href(&agent.session_id)),
                name = escape_html(&agent.name),
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn prs_html(row: &HistoryRow) -> String {
    row.prs
        .iter()
        .map(|pr| {
            let codex = if pr.codex_requested > 0 {
                format!(
                    r#" <span class="m" title="{} landed">{} Codex</span>"#,
                    pr.codex_landed, pr.codex_requested
                )
            } else {
                String::new()
            };
            format!(
                "<span>{} {}{codex}</span>",
                external(&pr.url, &format!("PR #{}", pr.number), "mt lk"),
                state_chip(&pr.state)
            )
        })
        .collect::<Vec<_>>()
        .join(r#" <span class="m">·</span> "#)
}

fn docs_html(row: &HistoryRow) -> String {
    row.docs
        .iter()
        .map(|doc| {
            let tone = if doc.state == "review_requested" { "a" } else { "v" };
            let reviews = match doc.owner_reviews {
                0 => String::new(),
                1 => r#" <span class="m">1 owner review</span>"#.to_owned(),
                n => format!(r#" <span class="m">{n} owner reviews</span>"#),
            };
            format!(
                r#"<span><a class="lk" href="{href}">{title}</a> <span class="chip {tone}">{state}</span>{reviews} <span class="m">{age}</span></span>"#,
                href = escape_html(&doc.reader_path),
                title = escape_html(&doc.title),
                state = escape_html(&doc.state.replace('_', " ")),
                age = age(&doc.published_at),
            )
        })
        .collect::<Vec<_>>()
        .join("<br>")
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

// ---- pages -----------------------------------------------------------------

fn render_list(params: &HistoryParams, page: &HistoryPage) -> String {
    let open_only = matches!(nonempty(&params.open), Some("1" | "true" | "yes"));
    let mut html = String::new();
    html.push_str(r#"<div class="bar">"#);
    html.push_str(&format!(
        r#"<a class="{}" href="{}">All</a><a class="{}" href="{}">Open only</a>"#,
        if open_only { "tab" } else { "tab on" },
        escape_html(&history_href(params, &[("open", None)])),
        if open_only { "tab on" } else { "tab" },
        escape_html(&history_href(params, &[("open", Some("1"))])),
    ));
    for (key, label) in [("agent", "agent"), ("repo", "repo")] {
        let value = if key == "agent" {
            nonempty(&params.agent)
        } else {
            nonempty(&params.repo)
        };
        if let Some(value) = value {
            html.push_str(&format!(
                r#"<span class="chip">{label}: {} <a href="{}" title="clear">✕</a></span>"#,
                escape_html(value),
                escape_html(&history_href(params, &[(key, None)])),
            ));
        }
    }
    html.push_str(&format!(
        r#"<span class="sp"></span><form method="get" action="/history"><input name="agent" placeholder="agent" value="{}"><input name="repo" placeholder="repo" value="{}">{}<button type="submit">Filter</button></form>"#,
        escape_html(nonempty(&params.agent).unwrap_or_default()),
        escape_html(nonempty(&params.repo).unwrap_or_default()),
        if open_only {
            r#"<input type="hidden" name="open" value="1">"#
        } else {
            ""
        },
    ));
    html.push_str("</div>\n");

    if page.rows.is_empty() {
        html.push_str(r#"<p class="dim">Nothing here yet. Tickets appear once an agent claims one, or its PR gets a Codex review or a doc.</p>"#);
    }
    for row in &page.rows {
        let repo = if page.multi_repo {
            format!(
                r#"<span class="m">{}</span>"#,
                escape_html(repo_name(&row.repo))
            )
        } else {
            String::new()
        };
        let last = row
            .last_activity
            .as_deref()
            .map(|at| {
                format!(
                    r#"<span class="m" title="{}">{} ago</span>"#,
                    escape_html(at),
                    age(at)
                )
            })
            .unwrap_or_default();
        let kind = if row.kind == "pr" { "PR " } else { "" };
        html.push_str(&format!(
            r#"<div class="card {edge}">
<div class="row"><a class="mt lk" href="{href}">{kind}#{number}</a>{repo}<a class="big" href="{href}">{title}</a></div>
<div class="row">{state} {flags} <span class="sp"></span>{last}</div>
{sections}</div>
"#,
            edge = edge(row),
            href = escape_html(&row.history_path),
            number = row.number,
            title = title_html(row),
            state = state_chip(&row.state),
            flags = flag_chips(row),
            sections = sections(&[
                ("Agents", agents_html(row)),
                ("PRs", if row.kind == "pr" { String::new() } else { prs_html(row) }),
                ("Docs", docs_html(row)),
            ]),
        ));
    }
    let mut pager = Vec::new();
    if nonempty(&params.before).is_some() {
        pager.push(format!(
            r#"<a class="lk" href="{}">← Newest</a>"#,
            escape_html(&list_href(params, None))
        ));
    }
    if let Some(next) = &page.next_before {
        pager.push(format!(
            r#"<a class="lk" href="{}">Older →</a>"#,
            escape_html(&list_href(params, Some(next)))
        ));
    }
    if !pager.is_empty() {
        html.push_str(&format!(r#"<div class="pager">{}</div>"#, pager.join("")));
    }
    html
}

fn render_timeline(timeline: &Timeline) -> String {
    let row = &timeline.item;
    let kind = if row.kind == "pr" { "PR " } else { "" };
    let mut html = format!(
        r#"<div class="m">t / {repo} / {number}</div>
<div class="row" style="margin-top:6px"><span class="mt">{kind}#{number}</span><span class="big">{title}</span>{github}</div>
"#,
        repo = escape_html(repo_name(&row.repo)),
        number = row.number,
        title = title_html(row),
        github = external(&row.url, "GitHub", "lk m"),
    );
    let linked = row
        .linked_tickets
        .iter()
        .map(|ticket| {
            format!(
                r#"<a class="mt lk" href="{}">ticket #{ticket}</a>"#,
                escape_html(&crate::work_claims::history_path(&row.repo, *ticket))
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let prs = if row.kind == "pr" {
        // The PR's own review count.
        row.prs
            .first()
            .filter(|pr| pr.codex_requested > 0)
            .map(|pr| {
                format!(
                    r#"<span class="m" title="{} landed">{} Codex</span>"#,
                    pr.codex_landed, pr.codex_requested
                )
            })
            .unwrap_or_default()
    } else {
        prs_html(row)
    };
    html.push_str(&format!(
        r#"<div class="strip"><div class="row">{} {} {prs} {linked}</div></div>
"#,
        state_chip(&row.state),
        flag_chips(row),
    ));
    html.push_str(&sections(&[
        ("Agents", agents_html(row)),
        ("Docs", docs_html(row)),
    ]));
    html.push_str(r#"<h2 class="lbl">Timeline</h2>"#);
    if timeline.events.is_empty() {
        html.push_str(r#"<p class="dim">Nothing recorded yet.</p>"#);
        return html;
    }
    html.push_str(r#"<div class="tl">"#);
    for entry in &timeline.events {
        let who = match (&entry.session_id, &entry.name) {
            (Some(id), Some(name)) => format!(
                r#"<a class="mt lk" href="{}">{}</a> "#,
                escape_html(&agent_href(id)),
                escape_html(name)
            ),
            _ => String::new(),
        };
        let link = match entry.link.as_deref() {
            Some(href) if href.starts_with("https://") => {
                format!(r#" <a class="lk m" href="{}">↗</a>"#, escape_html(href))
            }
            Some(href) if href.starts_with('/') => {
                format!(r#" <a class="lk m" href="{}">open</a>"#, escape_html(href))
            }
            _ => String::new(),
        };
        html.push_str(&format!(
            r#"<span class="m" title="{at}">{when}</span><span>{who}{text}{link}</span>"#,
            at = escape_html(&entry.at),
            when = local_time(&entry.at),
            text = escape_html(&entry.text),
        ));
    }
    html.push_str("</div>");
    html.push_str(&format!(
        r#"<p class="m" style="margin-top:14px">Times are this Mac's local time{}.</p>"#,
        local_offset_label()
    ));
    html
}

// ---- time ------------------------------------------------------------------

/// `45s`, `12m`, `3h`, `2d`.
fn age(at: &str) -> String {
    let Some(at) = crate::work_history::parse_time(at) else {
        return "?".to_owned();
    };
    let seconds = (OffsetDateTime::now_utc() - at).whole_seconds().max(0);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `Sep 24 14:02` in the server's local time (the owner's Mac), with the
/// year when it is not this year.
fn local_time(at: &str) -> String {
    let Some(utc) = crate::work_history::parse_time(at) else {
        return escape_html(at);
    };
    let utc = utc.to_offset(time::UtcOffset::UTC);
    let local = crate::queue::local_now_naive(utc)
        .unwrap_or_else(|| time::PrimitiveDateTime::new(utc.date(), utc.time()));
    let this_year = crate::queue::local_now_naive(OffsetDateTime::now_utc())
        .map_or(local.year(), |now| now.year());
    let month = MONTHS[usize::from(u8::from(local.month())) - 1];
    let year = if local.year() == this_year {
        String::new()
    } else {
        format!(" {}", local.year())
    };
    format!(
        "{month} {}{year} {:02}:{:02}",
        local.day(),
        local.hour(),
        local.minute()
    )
}

fn local_offset_label() -> String {
    let now = OffsetDateTime::now_utc();
    let Some(local) = crate::queue::local_now_naive(now) else {
        return " (UTC)".to_owned();
    };
    let offset = (local.assume_utc() - now).whole_minutes();
    let sign = if offset < 0 { '−' } else { '+' };
    let offset = offset.abs();
    format!(" (UTC{sign}{:02}:{:02})", offset / 60, offset % 60)
}
