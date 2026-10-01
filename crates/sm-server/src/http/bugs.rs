//! The app's bug button on the server (sm#1859, ticket #1875): filing a
//! report as a public issue that carries only the typed text and links, the
//! private report behind the owner's login and `sm bug show`, and the
//! standing Bugs lane every filed bug joins (spec appendix A).

use super::board::{
    add_lane, bad_request, blocking, board_store, checkout, outside, owner_guard, request_pass,
    send_alerts, start, StartRequest,
};
use super::*;
use crate::board::{model::EdgeKind, model::Key, LinkRequest, Refusal};
use crate::bug_reports::{FiledIssue, StoredBugReport};

const TEXT_MAX_CHARS: usize = 4000;
const PAGE_MAX_CHARS: usize = 40;
const ROUTE_MAX_CHARS: usize = 500;
const CLIENT_VERSION_MAX_CHARS: usize = 200;
const PAGE_DATA_MAX_CHARS: usize = 300_000;
const SERVER_FACTS_MAX_CHARS: usize = 200_000;
const SCREENSHOT_MAX_BYTES: usize = 8 * 1024 * 1024;
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const TITLE_MAX_CHARS: usize = 80;
/// How long a filing with Start waits for the new ticket's board facts.
const START_WAIT: Duration = Duration::from_secs(15);
const START_POLL: Duration = Duration::from_millis(250);

const BUGS_TITLE: &str = "Bugs";
const BUGS_BODY: &str = "Standing lane for bugs filed from the sm app. sm links every open bug to this ticket so the board shows them in the Bugs lane. Leave it open; closing it makes sm start a new one with the next bug.";

#[derive(Debug, Deserialize)]
pub(super) struct BugReportRequest {
    #[serde(default)]
    text: String,
    #[serde(default)]
    client: String,
    #[serde(default)]
    client_version: Option<String>,
    #[serde(default)]
    page: String,
    #[serde(default)]
    route: Option<String>,
    #[serde(default)]
    page_data: Option<Value>,
    #[serde(default)]
    screenshot_png: Option<String>,
    #[serde(default)]
    start: Option<BugStart>,
}

/// "Start an agent": the same choices board Start takes, minus the name
/// and first message, which the server renders from the new ticket.
#[derive(Debug, Deserialize)]
pub(super) struct BugStart {
    provider: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    reviewer: Option<Value>,
}

/// A request that passed validation.
struct ValidReport {
    text: String,
    client: String,
    client_version: Option<String>,
    page: String,
    route: Option<String>,
    page_data: Value,
    screenshot: Option<Vec<u8>>,
}

fn too_large(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::PAYLOAD_TOO_LARGE,
        detail: detail.into(),
    }
}

fn validate(payload: &BugReportRequest) -> Result<ValidReport, ApiError> {
    let text = payload.text.trim();
    if text.is_empty() {
        return Err(bad_request("text is required"));
    }
    if text.chars().count() > TEXT_MAX_CHARS {
        return Err(too_large(format!(
            "text exceeds {TEXT_MAX_CHARS} characters"
        )));
    }
    let client = payload.client.trim();
    if !matches!(client, "web" | "android") {
        return Err(bad_request("client must be web or android"));
    }
    let page = payload.page.trim();
    if page.is_empty() || page.chars().count() > PAGE_MAX_CHARS {
        return Err(bad_request(format!(
            "page is required, at most {PAGE_MAX_CHARS} characters"
        )));
    }
    let route = crate::config::trimmed(&payload.route);
    if route
        .as_ref()
        .is_some_and(|route| route.chars().count() > ROUTE_MAX_CHARS)
    {
        return Err(bad_request(format!(
            "route exceeds {ROUTE_MAX_CHARS} characters"
        )));
    }
    let client_version = crate::config::trimmed(&payload.client_version);
    if client_version
        .as_ref()
        .is_some_and(|version| version.chars().count() > CLIENT_VERSION_MAX_CHARS)
    {
        return Err(bad_request(format!(
            "client_version exceeds {CLIENT_VERSION_MAX_CHARS} characters"
        )));
    }
    let page_data = match &payload.page_data {
        None | Some(Value::Null) => json!({}),
        Some(value @ Value::Object(_)) => value.clone(),
        Some(_) => return Err(too_large("page_data must be an object")),
    };
    if serde_json::to_string(&page_data)?.chars().count() > PAGE_DATA_MAX_CHARS {
        return Err(too_large(format!(
            "page_data exceeds {PAGE_DATA_MAX_CHARS} serialized characters"
        )));
    }
    let screenshot = match payload
        .screenshot_png
        .as_deref()
        .map(str::trim)
        .filter(|png| !png.is_empty())
    {
        None => None,
        Some(encoded) => Some(decode_screenshot(encoded)?),
    };
    Ok(ValidReport {
        text: text.to_owned(),
        client: client.to_owned(),
        client_version,
        page: page.to_owned(),
        route,
        page_data,
        screenshot,
    })
}

fn decode_screenshot(encoded: &str) -> Result<Vec<u8>, ApiError> {
    let encoded = encoded
        .strip_prefix("data:image/png;base64,")
        .unwrap_or(encoded);
    // Refuse before decoding what could only decode over the cap.
    if encoded.len() > SCREENSHOT_MAX_BYTES.div_ceil(3) * 4 + 4 {
        return Err(too_large("screenshot exceeds 8 MiB"));
    }
    let png = STANDARD
        .decode(encoded)
        .map_err(|_| bad_request("screenshot_png is not valid base64"))?;
    if !png.starts_with(&PNG_SIGNATURE) {
        return Err(bad_request("screenshot_png is not a PNG"));
    }
    if png.len() > SCREENSHOT_MAX_BYTES {
        return Err(too_large("screenshot exceeds 8 MiB"));
    }
    Ok(png)
}

/// A4: the first non-blank line, at most 80 characters.
pub(super) fn issue_title(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= TITLE_MAX_CHARS {
        return line.to_owned();
    }
    let cut: String = line.chars().take(TITLE_MAX_CHARS - 1).collect();
    format!("{cut}…")
}

/// A4: the typed text and the links, nothing else.
pub(super) fn issue_body(
    text: &str,
    client: &str,
    page: &str,
    facts_url: Option<&str>,
    bug_id: &str,
) -> String {
    let app = if client == "android" {
        "Android"
    } else {
        "web"
    };
    let mut body = format!("{text}\n\n---\nFiled from the sm {app} app, {page} page.\n");
    if let Some(url) = facts_url {
        body.push_str(&format!(
            "Screenshot and server facts (owner only): {url}\n"
        ));
    }
    body.push_str(&format!("Agents: `sm bug show {bug_id}`\n"));
    body
}

/// `gh issue create` prints the new issue's URL last.
pub(super) fn parse_created_issue(stdout: &str) -> Result<(i64, String), String> {
    let url = stdout
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .ok_or_else(|| "gh issue create printed no issue URL".to_owned())?;
    let number = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|segment| segment.parse::<i64>().ok())
        .filter(|number| *number > 0)
        .ok_or_else(|| format!("gh issue create printed an unexpected URL: {url}"))?;
    Ok((number, url.to_owned()))
}

fn facts_url(config: &AppConfig, bug_id: &str) -> Option<String> {
    docs::doc_browser_base_url(config).map(|base| format!("{base}/bug-reports/{bug_id}"))
}

fn filing_repo(config: &AppConfig) -> Result<String, ApiError> {
    let repo = crate::work_claims::canonical_repo(&config.bug_reports.repo);
    crate::owner_docs::validate_repo_slug(&repo).map_err(|error| ApiError::Status {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        detail: format!("bug_reports.repo: {error}"),
    })?;
    Ok(repo)
}

fn report_store(config: &AppConfig) -> BugReportStore {
    BugReportStore::new(
        expand_home(&config.bug_reports.db_path),
        config.bug_reports.max_reports,
    )
}

/// A3: what the server adds at filing time, from what its own routes
/// already return.
fn server_facts(state: &AppState) -> Result<Value, ApiError> {
    let claims = super::claims::work_claim_store(state).active_claims()?;
    let mut tickets: BTreeMap<String, String> = BTreeMap::new();
    for view in claims.iter().filter(|view| view.claim.kind == "ticket") {
        tickets
            .entry(view.claim.session_id.clone())
            .or_insert_with(|| format!("{}#{}", view.claim.repo, view.claim.number));
    }
    let sessions: Vec<Value> = state
        .session_store
        .list_sessions(true)?
        .into_iter()
        .filter(|record| {
            super::claims::session_info(record).state != crate::work_claims::HolderState::Retired
        })
        .map(|record| {
            let ticket = tickets.get(&record.id).cloned();
            let response = serde_json::to_value(session_response_with_live_activity(state, record))
                .unwrap_or(Value::Null);
            json!({
                "id": response["id"],
                "name": response["friendly_name"].as_str().or(response["name"].as_str()),
                "provider": response["provider"],
                "state": response["activity_state"],
                "ticket": ticket,
                "parent": response["parent_session_id"],
                "activity": response["agent_status_text"],
            })
        })
        .collect();
    let store = board_store(state);
    let syncs = store.repo_syncs()?;
    let board = json!({
        "last_pass_at": syncs.iter().filter_map(|sync| sync.last_ok_at.clone()).max(),
        "stale_repos": syncs.iter().filter(|sync| sync.stale()).map(|sync| sync.repo.clone()).collect::<Vec<_>>(),
        "lanes": store.active_lanes()?.iter().map(|lane| json!({
            "id": lane.id,
            "goal": format!("{}#{}", lane.goal.0, lane.goal.1),
        })).collect::<Vec<_>>(),
    });
    let queue_db = expand_home(&state.config.queue_runner_state_dir().to_string_lossy())
        .join("queue_runner.db");
    let queue: Vec<Value> =
        RetainedQueueStore::list_queue_jobs_from_path(&queue_db, QueueJobFilters::default())
            .unwrap_or_default()
            .into_iter()
            .filter(|job| matches!(job.state.as_str(), "pending" | "running"))
            .map(|job| {
                json!({
                    "id": job.id,
                    "label": job.label,
                    "type": job.job_type,
                    "state": job.state,
                    "since": job.started_at.unwrap_or(job.queued_at),
                })
            })
            .collect();
    let facts = json!({
        "captured_at": now_rfc3339(),
        "server": {
            "build": option_env!("SM_BUILD_SHA").filter(|sha| !sha.is_empty()),
            "started_at": super::web::server_started_at(),
        },
        "sessions": sessions,
        "board": board,
        "queue": queue,
        "truncated": false,
    });
    Ok(cap_facts(facts, SERVER_FACTS_MAX_CHARS))
}

/// Over the cap, drops the sessions' activity first, then sessions.
fn cap_facts(mut facts: Value, cap: usize) -> Value {
    let size = |facts: &Value| facts.to_string().chars().count();
    if size(&facts) <= cap {
        return facts;
    }
    for session in facts["sessions"].as_array_mut().into_iter().flatten() {
        if let Some(session) = session.as_object_mut() {
            session.remove("activity");
        }
    }
    facts["truncated"] = json!(true);
    while size(&facts) > cap {
        match facts["sessions"].as_array_mut() {
            Some(sessions) if !sessions.is_empty() => {
                sessions.pop();
            }
            _ => break,
        }
    }
    facts
}

fn bugs_actor(state: &AppState) -> (String, String) {
    ("sm:owner".to_owned(), state.config.owner_name.clone())
}

/// A5 under the board lock: makes sure the Bugs ticket exists and is open,
/// drops its links to closed bugs, and links the new bug. The Bugs ticket,
/// or `Err` with the board note.
fn link_bug(state: &AppState, repo: &str, bug: &Key) -> Result<Key, String> {
    let _guard = state
        .board_lock
        .lock()
        .map_err(|_| "board lock poisoned".to_owned())?;
    let store = board_store(state);
    let source = state.board_source.as_ref();
    let now = time::OffsetDateTime::now_utc();
    let internal = |error: anyhow::Error| format!("{error:#}");
    store.ensure_schema().map_err(internal)?;
    let stored = store.bugs_goal().map_err(internal)?;
    let current = match stored {
        Some(goal) => match crate::board::check_goal(&store, source, &goal, now)
            .map_err(internal)?
        {
            Ok(()) => Some(goal),
            // Closed or deleted: the next bug starts a fresh Bugs ticket.
            Err(Refusal::NotFound(_) | Refusal::Unprocessable(_)) => None,
            Err(refusal) => return Err(format!("Bugs ticket check failed: {}", refusal.detail())),
        },
        None => None,
    };
    let goal = match current {
        Some(goal) => goal,
        None => {
            let (number, _) = source
                .create_issue(repo, BUGS_TITLE, BUGS_BODY)
                .map_err(|error| format!("Bugs ticket not created: {error}"))?;
            let goal = (repo.to_owned(), number);
            store.set_bugs_goal(&goal).map_err(internal)?;
            if let Err(refusal) =
                crate::board::check_goal(&store, source, &goal, now).map_err(internal)?
            {
                return Err(format!("Bugs ticket check failed: {}", refusal.detail()));
            }
            goal
        }
    };
    let (actor, actor_name) = bugs_actor(state);
    let input = store
        .input(&outside(state).map_err(internal)?)
        .map_err(internal)?;
    let closed: Vec<Key> = input
        .edges
        .iter()
        .filter(|edge| {
            edge.waiter == goal
                && edge.kind == EdgeKind::After
                && input
                    .items
                    .get(&edge.blocker)
                    .is_some_and(|item| !item.is_open())
        })
        .map(|edge| edge.blocker.clone())
        .collect();
    for blocker in closed {
        let request = LinkRequest {
            ticket: goal.clone(),
            target: blocker.clone(),
            kind: EdgeKind::After,
            remove: true,
            actor: actor.clone(),
            actor_name: actor_name.clone(),
        };
        match crate::board::write_link(&store, source, &request, now) {
            Ok(Ok(_)) => {}
            Ok(Err(refusal)) => eprintln!(
                "Bugs lane: dropping the link to closed {}#{} refused: {}",
                blocker.0,
                blocker.1,
                refusal.detail()
            ),
            Err(error) => eprintln!(
                "Bugs lane: dropping the link to closed {}#{} failed: {error:#}",
                blocker.0, blocker.1
            ),
        }
    }
    let request = LinkRequest {
        ticket: goal.clone(),
        target: bug.clone(),
        kind: EdgeKind::After,
        remove: false,
        actor,
        actor_name,
    };
    match crate::board::write_link(&store, source, &request, now).map_err(internal)? {
        Ok(_) => {}
        Err(refusal) => return Err(format!("Bugs lane link refused: {}", refusal.detail())),
    }
    let recomputed = store
        .recompute(&outside(state).map_err(internal)?, now)
        .map_err(internal)?;
    send_alerts(state, &recomputed);
    Ok(goal)
}

/// A5: the Bugs ticket, its lane and the new bug's link. `Err` is the
/// board note; the bug stays filed either way.
fn put_on_board(state: &AppState, repo: &str, bug: &Key) -> Result<(), String> {
    let goal = link_bug(state, repo, bug)?;
    let has_lane = board_store(state)
        .active_lanes()
        .map_err(|error| format!("{error:#}"))?
        .iter()
        .any(|lane| lane.goal == goal);
    if !has_lane {
        // Also what re-adds a Bugs lane the owner ended (decision 5).
        let name = state.config.owner_name.clone();
        match add_lane(state, goal, "owner".to_owned(), name) {
            Ok(_) => {}
            // Added by a concurrent filing.
            Err(ApiError::StatusBody {
                status: StatusCode::CONFLICT,
                ..
            }) => {}
            Err(error) => return Err(format!("Bugs lane not added: {}", api_error_detail(&error))),
        }
    }
    Ok(())
}

/// What steps 1–4 hand to the start step.
struct Filed {
    bug_id: String,
    issue: FiledIssue,
    title: String,
    facts_url: Option<String>,
    board_note: Option<String>,
}

/// `POST /client/bug-reports` (A1).
pub(super) async fn file_bug_report(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(payload): Json<BugReportRequest>,
) -> Result<Json<Value>, ApiError> {
    let owner = owner_guard(&state, &headers, peer_addr, "POST", &uri, true)?;
    ensure_core_writes_enabled(&state)?;
    let report = validate(&payload)?;
    if let Some(start) = &payload.start {
        if !matches!(start.provider.as_str(), "claude" | "codex-fork") {
            return Err(bad_request("provider must be claude or codex-fork"));
        }
        if let Some(reviewer) = &start.reviewer {
            crate::review::policy::validate("ticket", reviewer).map_err(bad_request)?;
        }
    }
    let repo = filing_repo(&state.config)?;
    let reported_by = owner.contains('@').then_some(owner);
    let filed = blocking(&state, move |state| {
        // 1. Stored first: from here the report survives any failure.
        let facts = server_facts(state)?;
        let store = report_store(&state.config);
        let created = store.create_report(CreateBugReport {
            report_text: report.text.clone(),
            reported_by,
            client: report.client.clone(),
            client_version: report.client_version,
            page: report.page.clone(),
            route: report.route,
            page_data: report.page_data,
            server_state: facts,
            screenshot_png: report.screenshot,
        })?;
        // 2. The public issue: the typed text and links only.
        let facts_url = facts_url(&state.config, &created.id);
        let title = issue_title(&report.text);
        let body = issue_body(
            &report.text,
            &report.client,
            &report.page,
            facts_url.as_deref(),
            &created.id,
        );
        let (number, url) = state
            .board_source
            .create_issue(&repo, &title, &body)
            .map_err(|error| ApiError::StatusBody {
                status: StatusCode::BAD_GATEWAY,
                body: json!({
                    "detail": error.lines().next().unwrap_or("GitHub refused the issue"),
                    "bug_id": created.id,
                }),
            })?;
        // 3.
        let issue = FiledIssue {
            repo: repo.clone(),
            number,
            url,
        };
        store.mark_filed(&created.id, &issue)?;
        // 4. A board failure leaves the bug filed.
        let board_note = put_on_board(state, &repo, &(repo.clone(), number)).err();
        request_pass(state);
        Ok(Filed {
            bug_id: created.id,
            issue,
            title,
            facts_url,
            board_note,
        })
    })
    .await?;
    // 5. Start as board Start does, once the ticket has board facts.
    let (started, start_error) = match payload.start {
        None => (None, None),
        Some(choice) => match &filed.board_note {
            Some(note) => (None, Some(format!("not on the board: {note}"))),
            None => match start_on_bug(&state, &filed.issue, choice).await {
                Ok(started) => (Some(started), None),
                Err(error) => (None, Some(error)),
            },
        },
    };
    Ok(Json(json!({
        "bug_id": filed.bug_id,
        "issue": {
            "repo": filed.issue.repo,
            "number": filed.issue.number,
            "url": filed.issue.url,
            "title": filed.title,
        },
        "facts_url": filed.facts_url,
        "on_board": filed.board_note.is_none(),
        "board_note": filed.board_note,
        "started": started,
        "start_error": start_error,
    })))
}

/// Waits (without the board lock) for the new ticket's board facts, then
/// runs board Start on it. `Err` is the start error the dialog shows.
async fn start_on_bug(
    state: &Arc<AppState>,
    issue: &FiledIssue,
    choice: BugStart,
) -> Result<Value, String> {
    let key: Key = (issue.repo.clone(), issue.number);
    let deadline = std::time::Instant::now() + START_WAIT;
    loop {
        let probe = key.clone();
        let present = blocking(state, move |state| {
            let (board, _) =
                board_store(state).board(&outside(state)?, time::OffsetDateTime::now_utc())?;
            Ok(board.facts.contains_key(&probe))
        })
        .await
        .map_err(|error| api_error_detail(&error))?;
        if present {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "#{} did not reach the board within {}s; start it from the board",
                issue.number,
                START_WAIT.as_secs()
            ));
        }
        sleep(START_POLL).await;
    }
    let request = StartRequest {
        repo: key.0,
        number: key.1,
        provider: choice.provider,
        model: choice.model,
        reasoning_effort: choice.reasoning_effort,
        name: None,
        brief: None,
        start_blocked: false,
        reviewer: choice.reviewer,
    };
    start(state.clone(), request, false)
        .await
        .map_err(|error| api_error_detail(&error))
}

/// `GET /client/bug-reports/options`: what the dialog's agent section
/// starts from before the ticket exists.
pub(super) async fn bug_report_options(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner_guard(&state, &headers, peer_addr, "GET", &uri, false)?;
    let repo = filing_repo(&state.config)?;
    let body = blocking(&state, move |state| {
        let working_dir = checkout(&state.config, &repo).ok();
        let default = state.session_store.owner_settings()?["reviews"]["reviewer"].clone();
        let db = expand_home(&state.config.sm_send.db_path);
        let resolved = match board_store(state).bugs_goal()? {
            Some(goal) => {
                crate::review::policy::resolve(&db, &goal.0, None, None, Some(goal.1), &default)?
            }
            None => crate::review::policy::resolve(&db, &repo, None, None, None, &default)?,
        };
        Ok(json!({
            "repo": repo,
            "working_dir": working_dir,
            "review_policy": {
                "resolved": resolved["reviewer"],
                "fallback": resolved["fallback"],
                "source": resolved["source"],
            },
        }))
    })
    .await?;
    Ok(Json(body))
}

fn stored_report(state: &AppState, bug_id: &str) -> Result<StoredBugReport, ApiError> {
    report_store(&state.config)
        .report(bug_id)?
        .ok_or(ApiError::NotFound("Bug report not found"))
}

fn report_json(report: &StoredBugReport) -> Value {
    json!({
        "bug_id": report.id,
        "created_at": report.created_at,
        "client": report.client,
        "client_version": report.client_version,
        "page": report.page,
        "route": report.route,
        "text": report.text,
        "issue": report.issue.as_ref().map(|issue| json!({
            "repo": issue.repo, "number": issue.number, "url": issue.url,
        })),
        "has_screenshot": report.has_screenshot,
        "page_data": report.page_data,
        "server_facts": report.server_facts,
    })
}

fn png_response(png: Vec<u8>) -> Response {
    (
        [
            (CONTENT_TYPE, "image/png"),
            (CACHE_CONTROL, "private, no-store"),
        ],
        png,
    )
        .into_response()
}

fn html_page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
:root {{ --bg: #ffffff; --fg: #1d1f23; --muted: #5d6470; --line: #d9dde3; --code: #f4f5f7; }}
@media (prefers-color-scheme: dark) {{
  :root {{ --bg: #16181c; --fg: #e6e8eb; --muted: #9aa3ae; --line: #2c3036; --code: #1e2126; }}
}}
body {{ background: var(--bg); color: var(--fg); font: 15px/1.5 system-ui, sans-serif; margin: 0 auto; max-width: 960px; padding: 16px; }}
h1 {{ font-size: 1.3rem; }}
.text {{ white-space: pre-wrap; border-left: 3px solid var(--line); padding-left: 12px; }}
table {{ border-collapse: collapse; margin: 16px 0; }}
td {{ border-top: 1px solid var(--line); padding: 4px 12px 4px 0; vertical-align: top; overflow-wrap: anywhere; }}
td:first-child {{ color: var(--muted); white-space: nowrap; }}
img {{ width: 100%; border: 1px solid var(--line); }}
pre {{ background: var(--code); padding: 12px; overflow-x: auto; font-size: 12px; }}
a {{ color: inherit; }}
</style>
</head>
<body>
{body}
</body>
</html>"#,
        title = crate::owner_docs::escape_html(title),
    );
    (
        status,
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CACHE_CONTROL, "private, no-store"),
        ],
        html,
    )
        .into_response()
}

fn report_html(report: &StoredBugReport) -> String {
    let escape = crate::owner_docs::escape_html;
    let cell = |value: Option<&str>| escape(value.unwrap_or("—"));
    let issue = match &report.issue {
        Some(issue) => format!(
            r#"<a href="{url}">{repo}#{number}</a>"#,
            url = escape(&issue.url),
            repo = escape(&issue.repo),
            number = issue.number
        ),
        None => "not filed".to_owned(),
    };
    let screenshot = if report.has_screenshot {
        format!(
            r#"<img src="/bug-reports/{}/screenshot.png" alt="Screenshot">"#,
            escape(&report.id)
        )
    } else {
        "<p>No screenshot.</p>".to_owned()
    };
    let pretty = |value: &Value| escape(&serde_json::to_string_pretty(value).unwrap_or_default());
    format!(
        r#"<h1>{id}</h1>
<div class="text">{text}</div>
<table>
<tr><td>Client</td><td>{client}</td></tr>
<tr><td>Version</td><td>{version}</td></tr>
<tr><td>Page</td><td>{page}</td></tr>
<tr><td>Route</td><td>{route}</td></tr>
<tr><td>Filed</td><td>{created}</td></tr>
<tr><td>Issue</td><td>{issue}</td></tr>
</table>
{screenshot}
<details><summary>Page data</summary><pre>{page_data}</pre></details>
<details><summary>Server facts</summary><pre>{facts}</pre></details>"#,
        id = escape(&report.id),
        text = escape(&report.text),
        client = cell(report.client.as_deref()),
        version = cell(report.client_version.as_deref()),
        page = cell(report.page.as_deref()),
        route = cell(report.route.as_deref()),
        created = escape(&report.created_at),
        page_data = pretty(&report.page_data),
        facts = pretty(&report.server_facts),
    )
}

/// `GET /bug-reports/{id}`: the owner-only page (A7).
pub(super) async fn bug_report_page(
    State(state): State<Arc<AppState>>,
    Path(bug_id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let report = blocking(&state, move |state| {
        report_store(&state.config)
            .report(&bug_id)
            .map_err(ApiError::from)
    })
    .await?;
    Ok(match report {
        Some(report) => html_page(StatusCode::OK, &report.id, &report_html(&report)),
        None => html_page(
            StatusCode::NOT_FOUND,
            "Bug report not found",
            "<h1>Bug report not found</h1><p>It may have been pruned; sm keeps the latest reports only.</p>",
        ),
    })
}

/// `GET /bug-reports/{id}/screenshot.png`.
pub(super) async fn bug_report_screenshot(
    State(state): State<Arc<AppState>>,
    Path(bug_id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    screenshot_response(&state, bug_id, false).await
}

async fn screenshot_response(
    state: &Arc<AppState>,
    bug_id: String,
    base64: bool,
) -> Result<Response, ApiError> {
    let png = blocking(state, move |state| {
        report_store(&state.config)
            .screenshot(&bug_id)?
            .ok_or(ApiError::NotFound("No screenshot"))
    })
    .await?;
    Ok(if base64 {
        // `sm bug show` reads bodies as text.
        ([(CONTENT_TYPE, "text/plain")], STANDARD.encode(png)).into_response()
    } else {
        png_response(png)
    })
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ScreenshotQuery {
    #[serde(default)]
    encoding: Option<String>,
}

/// `GET /bugs/{id}`: the private report for agents (A8).
pub(super) async fn agent_bug(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Path(bug_id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), uri.path())?;
    let report = blocking(&state, move |state| stored_report(state, &bug_id)).await?;
    Ok(Json(report_json(&report)))
}

/// `GET /bugs/{id}/screenshot.png`; `?encoding=base64` for the CLI.
pub(super) async fn agent_bug_screenshot(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Path(bug_id): Path<String>,
    Query(query): Query<ScreenshotQuery>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), uri.path())?;
    screenshot_response(&state, bug_id, query.encoding.as_deref() == Some("base64")).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_is_the_first_line_cut_at_80_characters() {
        assert_eq!(issue_title("\n  Board is wrong  \nmore"), "Board is wrong");
        let long = "é".repeat(100);
        let title = issue_title(&long);
        assert_eq!(title.chars().count(), 80);
        assert!(title.ends_with('…'));
        assert_eq!(issue_title(&"a".repeat(80)), "a".repeat(80));
    }

    #[test]
    fn body_is_the_text_then_the_footer() {
        let mut config = AppConfig::default();
        assert_eq!(facts_url(&config, "BR-1"), None);
        assert_eq!(
            issue_body("Board is wrong\nmore", "web", "Board", None, "BR-1"),
            "Board is wrong\nmore\n\n---\nFiled from the sm web app, Board page.\nAgents: `sm bug show BR-1`\n"
        );
        config.cloudflare_access.browser.enabled = true;
        config.cloudflare_access.browser.hostname = Some("sm.example.com".into());
        let url = facts_url(&config, "BR-1").unwrap();
        assert_eq!(url, "https://sm.example.com/bug-reports/BR-1");
        assert_eq!(
            issue_body("Inbox", "android", "Inbox", Some(&url), "BR-1"),
            "Inbox\n\n---\nFiled from the sm Android app, Inbox page.\n\
             Screenshot and server facts (owner only): https://sm.example.com/bug-reports/BR-1\n\
             Agents: `sm bug show BR-1`\n"
        );
    }

    #[test]
    fn created_issue_url_is_the_last_line() {
        assert_eq!(
            parse_created_issue("Creating issue\n\nhttps://github.com/a/b/issues/1870\n").unwrap(),
            (1870, "https://github.com/a/b/issues/1870".to_owned())
        );
        assert!(parse_created_issue("").is_err());
        assert!(parse_created_issue("https://github.com/a/b/issues/x").is_err());
    }

    #[test]
    fn facts_over_the_cap_drop_activity_then_sessions() {
        let sessions: Vec<Value> = (0..50)
            .map(|n| json!({"id": format!("s{n}"), "activity": "x".repeat(100)}))
            .collect();
        let facts = json!({"sessions": sessions, "truncated": false});
        let size = facts.to_string().len();
        let capped = cap_facts(facts.clone(), size);
        assert_eq!(capped, facts);
        let capped = cap_facts(facts.clone(), size - 100);
        assert_eq!(capped["truncated"], true);
        assert_eq!(capped["sessions"].as_array().unwrap().len(), 50);
        assert!(capped["sessions"][0].get("activity").is_none());
        let capped = cap_facts(facts, 200);
        assert!(capped.to_string().len() <= 200);
        assert!(capped["sessions"].as_array().unwrap().len() < 50);
    }
}
