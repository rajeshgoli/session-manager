//! Guestbook (sm#1603): signing from `sm task-complete --sign-guestbook`,
//! and `GET /guestbook`, the owner's list of entries, newest first, with
//! `?format=json`. Storage lives in `crate::guestbook`.

use super::history::{age, agent_href, encode_component, html_response, local_time};
use super::*;
use crate::guestbook::{
    render_entry_html, ClaimedWork, GuestbookEntry, GuestbookPage, GuestbookQuery, GuestbookStore,
    NewEntry, DEFAULT_LIMIT,
};
use crate::owner_docs::{escape_html, page_shell, repo_name};
use crate::work_claims::canonical_repo;

const GUESTBOOK_SCHEMA_VERSION: u32 = 1;

fn guestbook_store(state: &AppState) -> GuestbookStore {
    GuestbookStore::new(expand_home(&state.config.sm_send.db_path))
}

/// Stores `text` for `session_id` with the context sm knows: the session's
/// name, provider, model (the record's, else the usage ledger's) and
/// working directory, the tickets and PRs it claimed, and the repos those
/// and its working directory belong to.
pub(super) fn sign(state: &AppState, session_id: &str, text: String) -> anyhow::Result<i64> {
    let session = state
        .session_store
        .get_session(session_id)?
        .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;
    let mut claims: Vec<ClaimedWork> = Vec::new();
    for view in claims::work_claim_store(state).claims_for_session(session_id, false)? {
        let repo = canonical_repo(&view.claim.repo);
        if claims
            .iter()
            .any(|c| c.repo == repo && c.number == view.claim.number)
        {
            continue;
        }
        claims.push(ClaimedWork {
            repo,
            number: view.claim.number,
            kind: view.claim.kind.clone(),
            title: view.title,
        });
    }
    let mut repos: Vec<String> = Vec::new();
    let origin = git_origin_github_repo(&session.working_dir).map(|repo| canonical_repo(&repo));
    for repo in claims.iter().map(|c| c.repo.clone()).chain(origin) {
        if !repos.contains(&repo) {
            repos.push(repo);
        }
    }
    let model = session
        .model
        .clone()
        .filter(|model| !model.trim().is_empty())
        .or_else(|| {
            should_use_configured_usage_db_path(&state.config.usage)
                .then(|| {
                    crate::guestbook::observed_model(
                        &expand_home(&state.config.usage.db_path),
                        session_id,
                    )
                })
                .flatten()
        });
    guestbook_store(state).sign(&NewEntry {
        session_id: session.id.clone(),
        session_name: claims::session_info(&session).name,
        provider: session.provider.clone(),
        model,
        working_dir: session.working_dir.clone(),
        repos,
        claims,
        signed_at: now_rfc3339(),
        text,
    })
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct GuestbookParams {
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    format: Option<String>,
}

fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

pub(super) async fn get_guestbook(
    State(state): State<Arc<AppState>>,
    Query(params): Query<GuestbookParams>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let before = match nonempty(&params.before) {
        Some(cursor) => Some(cursor.parse::<i64>().map_err(|_| ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "Invalid before cursor".to_owned(),
        })?),
        None => None,
    };
    let page = guestbook_store(&state).list(&GuestbookQuery {
        repo: nonempty(&params.repo).map(str::to_owned),
        before,
        limit: params.limit.unwrap_or(DEFAULT_LIMIT),
    })?;
    if params.format.as_deref() == Some("json") {
        let mut body = json!({
            "schema_version": GUESTBOOK_SCHEMA_VERSION,
            "entries": page.entries,
            "next_before": page.next_before,
        });
        if let Some(base) = docs::doc_browser_base_url(&state.config) {
            body["page_url"] = json!(format!("{base}{}", guestbook_href(&params, None)));
        }
        return Ok(Json(body).into_response());
    }
    Ok(html_response(
        StatusCode::OK,
        page_shell("sm · Guestbook", "guestbook", &render(&params, &page)),
    ))
}

/// `/guestbook` keeping the repo filter and limit, at page `before`.
fn guestbook_href(params: &GuestbookParams, before: Option<i64>) -> String {
    let mut pairs = Vec::new();
    if let Some(repo) = nonempty(&params.repo) {
        pairs.push(format!("repo={}", encode_component(repo)));
    }
    if let Some(before) = before {
        pairs.push(format!("before={before}"));
    }
    if let Some(limit) = params.limit {
        pairs.push(format!("limit={limit}"));
    }
    if pairs.is_empty() {
        "/guestbook".to_owned()
    } else {
        format!("/guestbook?{}", pairs.join("&"))
    }
}

fn repo_href(repo: &str) -> String {
    format!("/guestbook?repo={}", encode_component(repo_name(repo)))
}

const ENTRY_STYLE: &str = r#"<style>
.card.v { border-left-color: var(--kv); }
.gb { margin: 8px 0 0; padding: 7px 0 0; border-top: 1px solid var(--k3); font-size: 14.5px; }
.gb > :first-child { margin-top: 0; } .gb > :last-child { margin-bottom: 0; }
.gb p, .gb ul, .gb ol, .gb blockquote, .gb pre, .gb table { margin: 6px 0; }
.gb ul, .gb ol { padding-left: 20px; }
.gb a { text-decoration: underline; text-underline-offset: 3px; color: var(--kc); }
.gb code { font: 12.5px var(--mono); background: var(--k2); border-radius: 4px; padding: 0 3px; }
.gb pre { background: var(--k2); border-radius: 6px; padding: 8px 10px; overflow-x: auto; }
.gb pre code { background: none; padding: 0; }
.gb blockquote { border-left: 2px solid var(--kl); padding-left: 10px; color: var(--kt2); }
.gb table { border-collapse: collapse; } .gb td, .gb th { border: 1px solid var(--kl); padding: 2px 6px; }
.gb h1, .gb h2, .gb h3, .gb h4 { font-size: 15px; margin: 10px 0 4px; }
</style>
"#;

fn render(params: &GuestbookParams, page: &GuestbookPage) -> String {
    let mut html = String::from(ENTRY_STYLE);
    html.push_str(r#"<div class="bar">"#);
    if let Some(repo) = nonempty(&params.repo) {
        html.push_str(&format!(
            r#"<span class="chip">repo: {} <a href="/guestbook" title="clear">✕</a></span>"#,
            escape_html(repo),
        ));
    }
    html.push_str(&format!(
        r#"<span class="sp"></span><form method="get" action="/guestbook"><input name="repo" placeholder="repo" value="{}"><button type="submit">Filter</button></form>"#,
        escape_html(nonempty(&params.repo).unwrap_or_default()),
    ));
    html.push_str("</div>\n");
    if page.entries.is_empty() {
        html.push_str(r#"<p class="dim">No entries yet. Agents sign with <span class="mt">sm task-complete --sign-guestbook</span> as they finish.</p>"#);
    }
    for entry in &page.entries {
        html.push_str(&render_entry(entry));
    }
    let mut pager = Vec::new();
    if nonempty(&params.before).is_some() {
        pager.push(format!(
            r#"<a class="lk" href="{}">← Newest</a>"#,
            escape_html(&guestbook_href(params, None))
        ));
    }
    if let Some(next) = page.next_before {
        pager.push(format!(
            r#"<a class="lk" href="{}">Older →</a>"#,
            escape_html(&guestbook_href(params, Some(next)))
        ));
    }
    if !pager.is_empty() {
        html.push_str(&format!(r#"<div class="pager">{}</div>"#, pager.join("")));
    }
    html
}

fn render_entry(entry: &GuestbookEntry) -> String {
    let short_id: String = entry.session_id.chars().take(8).collect();
    let model = match &entry.model {
        Some(model) => format!("{} · {}", entry.provider, model),
        None => entry.provider.clone(),
    };
    let repos = entry
        .repos
        .iter()
        .map(|repo| {
            format!(
                r#"<a class="chip c" href="{}">{}</a>"#,
                escape_html(&repo_href(repo)),
                escape_html(repo_name(repo))
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let work = entry
        .claims
        .iter()
        .map(|claim| {
            let kind = if claim.kind == "pr" { "PR " } else { "" };
            let title = if claim.title.is_empty() {
                String::new()
            } else {
                format!(" {}", escape_html(&claim.title))
            };
            format!(
                r#"<span><a class="mt lk" href="{}">{kind}#{}</a>{title}</span>"#,
                escape_html(&crate::work_claims::history_path(&claim.repo, claim.number)),
                claim.number,
            )
        })
        .collect::<Vec<_>>()
        .join(r#" <span class="m">·</span> "#);
    let context = [repos, work]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let context = if context.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="row" style="margin-top:4px">{context}</div>"#)
    };
    format!(
        r#"<div class="card v">
<div class="row"><a class="big lk" href="{agent}">{name}</a><span class="m">{short_id}</span><span class="chip v">{model}</span><span class="sp"></span><span class="m" title="{signed_at}">{when} · {age} ago</span></div>
{context}<div class="gb">{body}</div></div>
"#,
        agent = escape_html(&agent_href(&entry.session_id)),
        name = escape_html(&entry.session_name),
        short_id = escape_html(&short_id),
        model = escape_html(&model),
        signed_at = escape_html(&entry.signed_at),
        when = local_time(&entry.signed_at),
        age = age(&entry.signed_at),
        body = render_entry_html(&entry.text),
    )
}
