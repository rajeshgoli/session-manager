//! Web watch (sm#1452, ticket #1489): `GET /` and `GET /watch`, the
//! browser version of `sm watch`, and `GET /watch/state`, the JSON it
//! renders. Read-only except handoff policy, behind the owner page gate. The page is painted
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
    /// One session by id, stopped or not: the web's agent panel.
    #[serde(default)]
    session: Option<String>,
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
    // The browser hostname gets the web app (spec 1710 D1); `/watch` there
    // moves to `/`, keeping any panel link.
    if super::web::wants_shell(&state, &request) {
        if request.uri().path() == "/watch" {
            let location = request
                .uri()
                .query()
                .map_or_else(|| "/".to_owned(), |query| format!("/?{query}"));
            return Ok((StatusCode::MOVED_PERMANENTLY, [(LOCATION, location)]).into_response());
        }
        if let Some(shell) = super::web::shell_page(&state, &request) {
            return Ok(shell);
        }
    }
    let doc = watch_state(&state, &params)?;
    let path = if request.uri().path() == "/watch" {
        "/watch"
    } else {
        "/"
    };
    let body = format!(
        r#"<style>{STYLE}</style>
{bar}<div id="handoff-defaults" hidden></div><div id="w" data-refresh="{refresh}">{cards}</div>
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
            &format!(
                r#"<button type="button" id="handoff-defaults-open">Handoff defaults</button> <span class="m" id="ws">{}</span>"#,
                summary(&doc)
            ),
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
    // Model, effort, folder and activity times for the web's agent panel,
    // Clone and ball line (spec 1710 D6.1, D6.2).
    let launch: BTreeMap<String, Launch> = records
        .iter()
        .map(|record| {
            (
                record.id.clone(),
                Launch {
                    model: record.model.clone(),
                    reasoning_effort: record.reasoning_effort.clone(),
                    working_dir: record.working_dir.clone(),
                    turn_start: record.activity_turn_start_hook_at.clone(),
                    turn_end: record.activity_hook_at.clone(),
                },
            )
        })
        .collect();
    // Same link and null rules as `/client/sessions`.
    let remote_control: BTreeMap<String, Value> = records
        .iter()
        .map(|record| (record.id.clone(), remote_control_payload(record)))
        .collect();
    let sessions: Vec<Value> = records
        .into_iter()
        .filter(|record| match nonempty(&params.session) {
            Some(id) => record.id == id,
            None => include_stopped || !record.is_stopped(),
        })
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
    let positions: BTreeMap<String, usize> = crate::queue::pending_queue_job_consideration_order(
        &jobs,
        &crate::queue::queue_ticket_ranks(&expand_home(&state.config.sm_send.db_path)),
    )
    .into_iter()
    .enumerate()
    .map(|(index, id)| (id, index + 1))
    .collect();
    let messages: BTreeMap<String, crate::owner_messages::OwnerMessage> =
        super::messages::obligation_messages(state)?
            .into_iter()
            .filter(|entry| entry.state == crate::owner_messages::OwnerMessageState::NeedsYou)
            .map(|entry| (entry.message.id.clone(), entry.message))
            .collect();
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
    let mut counts = BTreeMap::<String, usize>::new();
    let now = OffsetDateTime::now_utc();
    let config = &state.config;
    let fact_context = FactContext {
        messages: &messages,
        config,
        now,
    };
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
        let own_jobs: Vec<Value> = jobs
            .iter()
            .filter(|job| owns(job, id))
            .map(|job| {
                json!({"id": job.id, "type": job.job_type, "label": job.label,
                       "state": job.state, "started_at": job.started_at,
                       "queued_at": job.queued_at, "timeout_seconds": job.timeout_seconds,
                       "quiet_since": job.quiet_alerted_at,
                       "holding_reason": job.holding_reason,
                       "position": positions.get(&job.id)})
            })
            .collect();
        let optional = |key: &str| {
            let value = s(v, key);
            (!value.is_empty()).then(|| value.to_owned())
        };
        let activity_since = launch.get(id).and_then(|l| l.since(state)).or_else(|| {
            (state != "working")
                .then(|| optional("last_activity"))
                .flatten()
        });
        let (facts, attention) = agent_facts(
            v,
            &waiting_on,
            &field("claims"),
            &own_jobs,
            activity_since.as_deref(),
            &fact_context,
        );
        if state != "stopped" {
            *counts
                .entry(s(&attention, "section").to_owned())
                .or_default() += 1;
        }
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
            "model": launch.get(id).and_then(|l| l.model.clone()),
            "reasoning_effort": launch.get(id).and_then(|l| l.reasoning_effort.clone()),
            "working_dir": launch.get(id).map(|l| l.working_dir.clone()),
            // A working agent without a turn-start hook (Codex) has no known
            // start; its last activity is always now, so leave it unset.
            "activity_since": activity_since,
            "facts": facts,
            "attention": attention,
            "handoff": v["handoff"].clone(),
            "remote_control": remote_control.get(id).cloned().unwrap_or(Value::Null),
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
        "generated_at": generated_at(now),
        "sessions": out,
        "counts": {"live": live, "waiting_on_owner": counts.get("you").copied().unwrap_or(0),
                   "needs_you": counts.get("you").copied().unwrap_or(0),
                   "finished": 0, "waiting_long": counts.get("waiting_long").copied().unwrap_or(0),
                   "moving": counts.get("moving").copied().unwrap_or(0),
                   "waiting": counts.get("waiting").copied().unwrap_or(0),
                   "idle": counts.get("idle").copied().unwrap_or(0)},
    }))
}

pub(super) fn session_facts(state: &AppState, session_id: &str) -> Result<Value, ApiError> {
    let doc = watch_state(
        state,
        &WatchParams {
            session: Some(session_id.to_owned()),
            ..WatchParams::default()
        },
    )?;
    Ok(doc["sessions"]
        .as_array()
        .and_then(|items| items.first())
        .map(|item| item["facts"].clone())
        .unwrap_or(Value::Null))
}

struct Launch {
    model: Option<String>,
    reasoning_effort: Option<String>,
    working_dir: String,
    turn_start: Option<String>,
    turn_end: Option<String>,
}

impl Launch {
    /// When the agent's current working or idle stretch began: the turn's
    /// start hook while working, the Stop hook while idle. A start older
    /// than the last Stop belongs to an earlier turn.
    fn since(&self, state: &str) -> Option<String> {
        match state {
            "working" => self
                .turn_start
                .clone()
                .filter(|start| self.turn_end.as_ref().is_none_or(|end| start >= end)),
            "idle" | "waiting" => self.turn_end.clone(),
            _ => None,
        }
    }
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

fn facts_age(since: &str, now: OffsetDateTime) -> String {
    let minutes = OffsetDateTime::parse(since, &Rfc3339)
        .map(|at| ((now - at).whole_minutes()).max(0))
        .unwrap_or(0);
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h {}m", minutes / 60, minutes % 60)
    }
}

fn old_enough(since: &str, now: OffsetDateTime, minutes: i64) -> bool {
    OffsetDateTime::parse(since, &Rfc3339)
        .is_ok_and(|at| now - at >= time::Duration::minutes(minutes))
}

fn inverse_time(since: &str) -> String {
    let seconds = OffsetDateTime::parse(since, &Rfc3339)
        .map(|at| at.unix_timestamp())
        .unwrap_or(0);
    format!("{:010}", (9_999_999_999_i64 - seconds).max(0))
}

fn ordinal(position: u64) -> String {
    let suffix = if (11..=13).contains(&(position % 100)) {
        "th"
    } else {
        match position % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{position}{suffix}")
}

struct FactContext<'a> {
    messages: &'a BTreeMap<String, crate::owner_messages::OwnerMessage>,
    config: &'a AppConfig,
    now: OffsetDateTime,
}

fn agent_facts(
    session: &Value,
    waiting_on: &Value,
    claims: &Value,
    jobs: &[Value],
    activity_since: Option<&str>,
    context: &FactContext<'_>,
) -> (Value, Value) {
    let FactContext {
        messages,
        config,
        now,
    } = context;
    let now = *now;
    let stopped = s(session, "status") == "stopped";
    let working = matches!(s(session, "activity_state"), "working" | "thinking");
    let agent_state = if stopped {
        "stopped"
    } else if working {
        "working"
    } else {
        "idle"
    };
    let running: Vec<&Value> = jobs
        .iter()
        .filter(|job| s(job, "state") == "running")
        .collect();
    let pending: Vec<&Value> = jobs
        .iter()
        .filter(|job| s(job, "state") == "pending")
        .collect();
    let earliest_start = running
        .iter()
        .map(|job| s(job, "started_at"))
        .filter(|at| !at.is_empty())
        .min();
    let oldest_wait = pending
        .iter()
        .map(|job| s(job, "queued_at"))
        .filter(|at| !at.is_empty())
        .min();
    let quiet: Vec<&Value> = running
        .iter()
        .copied()
        .filter(|job| !s(job, "quiet_since").is_empty())
        .collect();
    let review = waiting_on
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| s(item, "kind") == "review")
        .min_by_key(|item| s(item, "since"));
    let review_since = review
        .map(|item| s(item, "since"))
        .filter(|since| !since.is_empty());
    let mut tone = Value::Null;
    let mut job_text = if running.is_empty() && pending.is_empty() {
        "No jobs".to_owned()
    } else if !running.is_empty() && quiet.len() == running.len() && pending.is_empty() {
        tone = json!("red");
        let latest = quiet
            .iter()
            .max_by_key(|job| s(job, "quiet_since"))
            .unwrap();
        format!(
            "Quiet {}: {}",
            facts_age(s(latest, "quiet_since"), now),
            s(latest, "label")
        )
    } else if running.len() == 1 && pending.is_empty() {
        tone = json!("green");
        let job = running[0];
        let kind = s(job, "type");
        let mut chars = kind.chars();
        let title = chars.next().map_or_else(String::new, |first| {
            first.to_uppercase().collect::<String>() + chars.as_str()
        });
        let mut text = format!("{title} running {}", facts_age(s(job, "started_at"), now));
        if let Some(limit) = job["timeout_seconds"].as_i64().filter(|limit| *limit > 0) {
            text.push_str(&format!(
                " of {}",
                facts_age(&generated_at(now - time::Duration::seconds(limit)), now)
            ));
        }
        text
    } else if !running.is_empty() {
        tone = json!("green");
        let mut text = format!(
            "{} running · {}",
            running.len(),
            facts_age(earliest_start.unwrap_or(""), now)
        );
        if !pending.is_empty() {
            text.push_str(&format!(
                " · {} waiting {}",
                pending.len(),
                facts_age(oldest_wait.unwrap_or(""), now)
            ));
        }
        text
    } else if pending.len() == 1 {
        tone = json!("amber");
        let job = pending[0];
        let position = job["position"].as_u64().unwrap_or(1);
        format!(
            "Waiting {} · {} in line",
            facts_age(s(job, "queued_at"), now),
            ordinal(position)
        )
    } else {
        tone = json!("amber");
        let mut text = format!(
            "{} waiting {}",
            pending.len(),
            facts_age(oldest_wait.unwrap_or(""), now)
        );
        if pending
            .iter()
            .all(|job| s(job, "holding_reason") == "concurrency_cap")
        {
            text.push_str(" for a slot");
        }
        text
    };
    if let Some(since) = review_since {
        if running.is_empty() && pending.is_empty() {
            job_text = format!(
                "Codex review on PR #{} · {}",
                review
                    .and_then(|item| item["pr_number"].as_i64())
                    .unwrap_or(0),
                facts_age(since, now)
            );
            tone = json!("amber");
        } else {
            job_text.push_str(&format!(" · review {}", facts_age(since, now)));
        }
    }
    let review_fact =
        review.map(|item| json!({"pr_number": item["pr_number"], "since": item["since"]}));
    let job_facts = json!({"running": running.len(), "waiting": pending.len(),
        "quiet": !quiet.is_empty(), "review": review_fact,
        "earliest_start": earliest_start, "oldest_wait": oldest_wait,
        "tone": tone, "text": job_text});

    let obligations: Vec<&Value> = waiting_on.as_array().into_iter().flatten().collect();
    let open_messages: Vec<&crate::owner_messages::OwnerMessage> = obligations
        .iter()
        .filter(|item| s(item, "kind") == "owner_message")
        .filter_map(|item| messages.get(s(item, "id")))
        .collect();
    let doc_reviews: Vec<&&Value> = obligations
        .iter()
        .filter(|item| s(item, "kind") == "owner_review")
        .collect();
    let doc_review = doc_reviews.first().copied();
    let prompt = s(session, "activity_state") == "waiting_permission";
    let you = if !open_messages.is_empty() {
        let oldest = open_messages
            .iter()
            .min_by_key(|message| &message.created_at)
            .unwrap();
        let newest = open_messages
            .iter()
            .max_by_key(|message| &message.created_at)
            .unwrap();
        let preview: String = newest.body_markdown.chars().take(120).collect();
        json!({"kind": "message", "since": oldest.created_at, "text": preview,
            "more": open_messages.len() - 1 + doc_reviews.len() + usize::from(prompt),
            "message_ids": open_messages.iter().map(|message| &message.id).collect::<Vec<_>>(),
            "dismissible": true})
    } else if let Some(doc) = doc_review {
        json!({"kind": "doc_review", "since": doc["since"],
            "text": format!("Review: {}", s(doc, "label").trim_start_matches("Owner review · ")),
            "more": doc_reviews.len() - 1 + usize::from(prompt), "message_ids": [], "dismissible": false})
    } else if prompt {
        json!({"kind": "prompt", "since": activity_since,
            "text": "Allow prompt in its terminal", "more": 0,
            "message_ids": [], "dismissible": false})
    } else {
        Value::Null
    };
    let facts = json!({"agent": {"state": agent_state, "since": activity_since},
        "jobs": job_facts, "you": you, "finished": null});
    let last = s(session, "last_activity");
    let has_claim = claims
        .as_array()
        .is_some_and(|items| items.iter().any(|item| s(item, "kind") == "ticket"));
    let threshold = i64::from(config.web_watch.waiting_long_minutes);
    let waiting_long = oldest_wait.is_some_and(|at| old_enough(at, now, threshold));
    let review_long = review_since.is_some_and(|at| old_enough(at, now, threshold));
    let stalled = !working
        && running.is_empty()
        && pending.is_empty()
        && review.is_none()
        && has_claim
        && activity_since
            .is_some_and(|at| old_enough(at, now, i64::from(config.board.stall_minutes)));
    let (section, reason, key) = if stopped {
        ("stopped", Value::Null, inverse_time(last))
    } else if !you.is_null() {
        ("you", json!(s(&you, "kind")), s(&you, "since").to_owned())
    } else if !quiet.is_empty() || waiting_long || review_long || stalled {
        let reason = if !quiet.is_empty() {
            "quiet"
        } else if waiting_long {
            "queue_wait"
        } else if review_long {
            "review_wait"
        } else {
            "stalled"
        };
        let earliest = quiet
            .iter()
            .map(|job| s(job, "quiet_since"))
            .chain(oldest_wait)
            .chain(review_since)
            .chain(activity_since.filter(|_| stalled))
            .filter(|at| !at.is_empty())
            .min()
            .unwrap_or("");
        ("waiting_long", json!(reason), earliest.to_owned())
    } else if working || !running.is_empty() {
        let latest = running
            .iter()
            .map(|job| s(job, "started_at"))
            .chain(activity_since)
            .filter(|at| !at.is_empty())
            .max()
            .unwrap_or(last);
        ("moving", Value::Null, inverse_time(latest))
    } else if !pending.is_empty() || review.is_some() {
        (
            "waiting",
            Value::Null,
            oldest_wait
                .into_iter()
                .chain(review_since)
                .min()
                .unwrap_or("")
                .to_owned(),
        )
    } else {
        (
            "idle",
            Value::Null,
            format!("{}{}", if has_claim { 0 } else { 1 }, inverse_time(last)),
        )
    };
    (
        facts,
        json!({"section": section, "reason": reason, "order_key": key}),
    )
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
.cp.ok{color:var(--kg)}#ws.stale{color:var(--ka)}\
.handoff-panel,#handoff-defaults{padding:12px;margin:8px 0;border:1px solid var(--kt3);border-radius:8px}\
.handoff-panel input[type=number],#handoff-defaults input[type=number]{width:6em}\
.handoff-panel button,.handoff-panel label{margin:4px}\
button[data-handoff]{font:inherit;color:inherit;background:none;border:0;padding:3px 0;cursor:pointer;text-align:left}\
[role=status]{color:var(--kt2)}";

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
        r#"<details class="card {edge}" data-id="{id}" style="margin-left:{indent}px"><summary><span class="row"><span class="dot {state}"></span><span class="mt nm">{name}</span><span class="m">{provider} · {state} {age}</span>{chips}</span>{status}{handoff}</summary>{sections}</details>"#,
        handoff = handoff_line(v),
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

fn handoff_line(v: &Value) -> String {
    if !v["handoff"].is_object() {
        return String::new();
    }
    let context = v["context_percent"]
        .as_f64()
        .map(|p| format!("ctx {p:.0}% · "))
        .unwrap_or_default();
    format!(
        r#"<span class="st"><button type="button" data-handoff="{}">{}{}</button></span>"#,
        escape_html(s(v, "id")),
        context,
        escape_html(s(&v["handoff"], "display"))
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
                "{} {}{codex}{}",
                external(s(&claim, "url"), &format!("PR #{number}")),
                state_chip(s(&claim, "state")),
                if claim["merge_hold"].is_object() {
                    " ⏸"
                } else {
                    ""
                }
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

#[cfg(test)]
mod facts_tests {
    use super::*;

    #[test]
    fn attention_examples_keep_agent_jobs_and_owner_question_separate() {
        let now = OffsetDateTime::parse("2026-09-30T19:47:00Z", &Rfc3339).unwrap();
        let config = AppConfig::default();
        let empty = json!([]);
        let messages = BTreeMap::new();
        let context = FactContext {
            messages: &messages,
            config: &config,
            now,
        };
        let session = |activity: &str| {
            json!({"status": "active", "activity_state": activity,
            "last_activity": "2026-09-30T19:42:00Z"})
        };
        let running = json!([{"state": "running", "type": "background", "label": "Run",
            "started_at": "2026-09-30T16:51:00Z", "timeout_seconds": 0}]);
        let pending = json!([{"state": "pending", "type": "tests", "label": "Test",
            "queued_at": "2026-09-30T18:45:00Z", "position": 3}]);
        let recent_pending = json!([{"state": "pending", "type": "tests", "label": "Test",
            "queued_at": "2026-09-30T19:39:00Z", "position": 1}]);
        let cases = [
            (
                session("idle"),
                empty.clone(),
                empty.clone(),
                running.clone(),
                "moving",
                "Background running 2h 56m",
            ),
            (
                session("idle"),
                empty.clone(),
                empty.clone(),
                pending,
                "waiting_long",
                "Waiting 1h 2m · 3rd in line",
            ),
            (
                session("working"),
                empty.clone(),
                empty.clone(),
                recent_pending,
                "moving",
                "Waiting 8m · 1st in line",
            ),
            (
                session("idle"),
                json!([{"kind": "review", "pr_number": 1790,
                "since": "2026-09-30T19:35:00Z"}]),
                empty.clone(),
                empty.clone(),
                "waiting",
                "Codex review on PR #1790 · 12m",
            ),
            (
                session("idle"),
                empty.clone(),
                json!([{"kind": "ticket"}]),
                empty.clone(),
                "waiting_long",
                "No jobs",
            ),
        ];
        for (session, obligations, claims, jobs, section, job_text) in cases {
            let (facts, attention) = agent_facts(
                &session,
                &obligations,
                &claims,
                jobs.as_array().unwrap(),
                Some("2026-09-30T19:13:00Z"),
                &context,
            );
            assert_eq!(attention["section"], section);
            assert_eq!(facts["jobs"]["text"], job_text);
        }
    }

    #[test]
    fn blocking_message_wins_even_while_agent_works_and_doc_review_remains() {
        let now = OffsetDateTime::parse("2026-09-30T19:47:00Z", &Rfc3339).unwrap();
        let mut messages = BTreeMap::new();
        messages.insert(
            "msg_3f9a2c1d".to_owned(),
            crate::owner_messages::OwnerMessage {
                id: "msg_3f9a2c1d".to_owned(),
                human: "rajesh".to_owned(),
                sender_session_id: "agent".to_owned(),
                sender_session_name: "agent".to_owned(),
                title: "Check Chrome".to_owned(),
                body_markdown: "Please check Chrome".to_owned(),
                blocking: true,
                created_at: "2026-09-30T19:40:00Z".to_owned(),
                first_viewed_at: None,
                handled_at: None,
                handled_via: None,
            },
        );
        let session = json!({"status": "active", "activity_state": "working"});
        let config = AppConfig::default();
        let context = FactContext {
            messages: &messages,
            config: &config,
            now,
        };
        let obligations = json!([{"kind": "owner_message", "id": "msg_3f9a2c1d"},
            {"kind": "owner_review", "since": "2026-09-30T19:41:00Z",
             "label": "Owner review · Memo"}]);
        let (facts, attention) = agent_facts(
            &session,
            &obligations,
            &json!([]),
            &[],
            Some("2026-09-30T19:42:00Z"),
            &context,
        );
        assert_eq!(facts["agent"]["state"], "working");
        assert_eq!(facts["you"]["kind"], "message");
        assert_eq!(facts["you"]["more"], 1);
        assert_eq!(attention["section"], "you");
        let (facts, attention) = agent_facts(
            &session,
            &json!([obligations[1].clone()]),
            &json!([]),
            &[],
            None,
            &context,
        );
        assert_eq!(facts["you"]["kind"], "doc_review");
        assert_eq!(facts["you"]["dismissible"], false);
        assert_eq!(attention["section"], "you");
    }
}
