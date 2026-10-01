//! Paired reviewers (#1779, spec 1768 D3b, B4, E5–E8): an agent that works
//! in the author's checkout for one ticket, reviews every round of its PR,
//! and returns findings with `sm review submit`. sm posts them as it posts a
//! run's. The request watcher owns every transition, as for runs.
use super::*;
use crate::review::{self, paired};
use anyhow::{Context, Result};
use review_runs::{gh, git, PostTarget};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path as FsPath, PathBuf},
};

const NUDGE_AFTER_SECONDS: i64 = 3600;
const GIVE_UP_AFTER_SECONDS: i64 = 5400;
const RETIRE_SWEEP_SECONDS: u64 = 600;

fn short7(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn seconds_since(at: Option<&str>) -> Option<i64> {
    let at = codex_review_request_requested_at(at?)?;
    Some((OffsetDateTime::now_utc() - at).whole_seconds())
}

/// The reviewer session, following a context handoff to its successor.
fn live_session(state: &AppState, id: &str) -> Result<Option<SessionRecord>> {
    let Some(session) = state.session_store.get_session(id)? else {
        return Ok(None);
    };
    let session = if session.is_stopped() && session.successor_session_id.is_some() {
        state
            .session_store
            .forwarded_session(&session.id)?
            .unwrap_or(session)
    } else {
        session
    };
    Ok((!session.is_stopped() && !session.is_retired()).then_some(session))
}

fn is_idle(state: &AppState, session: &SessionRecord) -> bool {
    let response =
        serde_json::to_value(session_response_with_live_activity(state, session.clone()))
            .unwrap_or_default();
    matches!(
        response["activity_state"].as_str(),
        Some("idle" | "waiting_input")
    )
}

fn deliver(state: &AppState, session_id: &str) {
    if !state.config.rust_core.runtime_enabled {
        return;
    }
    let runtime = TmuxRuntime::from_app_config(&state.config);
    if let Err(error) = state
        .session_store
        .drain_runtime_pending_messages_for_session(session_id, &runtime)
    {
        eprintln!("paired review message to {session_id} not delivered yet: {error:#}");
    }
}

fn send(state: &AppState, session_id: &str, text: &str) -> Result<()> {
    RetainedQueueStore::new(expand_home(&state.config.sm_send.db_path))
        .enqueue_message_with_metadata(
            session_id,
            text,
            "sequential",
            QueueMessageMetadata::default(),
        )?;
    deliver(state, session_id);
    Ok(())
}

/// E7, sent once when a request the reviewer was working ends another way.
/// Only a request whose round reached the reviewer has anything to stop.
pub(super) fn stop(state: &AppState, r: &CodexReviewRequestRegistration, reason: &str) {
    let (Some(reviewer), Some(_)) = (&r.reviewer_session_id, &r.checkout_snapshot) else {
        return;
    };
    let head = r.requested_head_sha.as_deref().unwrap_or("unknown");
    let text = format!(
        "[sm review] Stop reviewing PR #{} @ {}: {reason}. Do not submit. Stay idle.",
        r.pr_number,
        short7(head)
    );
    if let Err(error) = send(state, reviewer, &text) {
        eprintln!("paired review stop for {} failed: {error:#}", r.id);
    }
}

fn step_ticket(step: &Value) -> Option<(String, i64)> {
    Some((
        step["ticket"]["repo"].as_str()?.to_owned(),
        step["ticket"]["number"].as_i64()?,
    ))
}

/// D3b step 2: the worktree on the author's active claim on the PR, else on
/// the ticket.
fn author_checkout(
    db: &FsPath,
    r: &CodexReviewRequestRegistration,
    ticket: &(String, i64),
) -> Result<Option<(PathBuf, String)>> {
    let conn = rusqlite::Connection::open(db)?;
    crate::work_claims::init_work_claims_schema(&conn)?;
    let found = conn
        .query_row(
            "SELECT worktree_path, session_id FROM work_claims WHERE ended_at IS NULL \
             AND worktree_path IS NOT NULL AND worktree_path != '' AND \
             ((kind='pr' AND repo=?1 AND number=?2) OR (kind='ticket' AND repo=?3 AND number=?4)) \
             ORDER BY CASE kind WHEN 'pr' THEN 0 ELSE 1 END, claimed_at DESC LIMIT 1",
            rusqlite::params![r.repo, r.pr_number, ticket.0, ticket.1],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    Ok(found
        .map(|(path, session)| (expand_home(&path), session))
        .filter(|(path, _)| path.is_dir()))
}

fn advance(
    state: &AppState,
    db: &FsPath,
    r: &CodexReviewRequestRegistration,
    reason: &str,
) -> Result<bool, String> {
    finish_github_review_step(state, db, r, reason, &now_rfc3339())?;
    Ok(true)
}

/// One poll of a paired step. True when the request moved on.
pub(super) async fn poll(
    state: &Arc<AppState>,
    db: &FsPath,
    r: &CodexReviewRequestRegistration,
    step: &Value,
) -> Result<bool, String> {
    match r.step_state.as_deref() {
        Some("waiting_reviewer") => send_round(state, db, r).await,
        Some("reviewing" | "nudged") => watch(state, db, r),
        _ => start(state, db, r, step).await,
    }
}

async fn start(
    state: &Arc<AppState>,
    db: &FsPath,
    r: &CodexReviewRequestRegistration,
    step: &Value,
) -> Result<bool, String> {
    if !state.config.rust_core.runtime_enabled {
        return advance(state, db, r, "failed to start: runtime disabled");
    }
    let provider = step["provider"].as_str().unwrap_or("codex");
    if let Some(reason) = review_runs::meter_skip(state, provider).map_err(|e| e.to_string())? {
        return advance(state, db, r, &reason);
    }
    let Some(ticket) = step_ticket(step) else {
        return advance(state, db, r, "failed to start: the policy names no ticket");
    };
    let Some((checkout, _author)) = author_checkout(db, r, &ticket).map_err(|e| e.to_string())?
    else {
        return advance(state, db, r, "no author checkout");
    };
    let model = step["model"].as_str().unwrap_or_default();
    let effort = step["effort"].as_str().unwrap_or_default();
    let checkout_text = checkout.to_string_lossy().into_owned();
    if let Some(existing) = paired::live(db, &ticket.0, ticket.1).map_err(|e| e.to_string())? {
        let session = live_session(state, &existing.session_id).map_err(|e| e.to_string())?;
        let matches = existing.provider == provider
            && existing.model == model
            && existing.effort == effort
            && existing.checkout == checkout_text;
        match session {
            Some(session) if matches => {
                RetainedQueueStore::attach_paired_reviewer(
                    db,
                    &r.id,
                    r.step_index,
                    &session.id,
                    &session_display_name(session.clone()),
                )
                .map_err(|e| e.to_string())?;
                return Ok(false);
            }
            Some(session) => retire_reviewer(state, db, &session.id, &existing.session_id),
            None => {
                paired::retire(db, &existing.session_id, &now_rfc3339())
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    let settings = state
        .session_store
        .owner_settings()
        .map_err(|e| e.to_string())?;
    let repo_name = ticket.0.rsplit('/').next().unwrap_or(&ticket.0);
    let short = settings["new_agent"]["repo_short"][&ticket.0]
        .as_str()
        .unwrap_or(repo_name)
        .to_owned();
    let taken: BTreeSet<String> = state
        .session_store
        .list_sessions(false)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|s| !s.is_retired())
        .map(|s| s.name)
        .collect();
    let name = paired::reviewer_name(&short, ticket.1, &|n| taken.contains(n));
    let created = async {
        let id = state.session_store.allocate_session_id()?;
        create_session_from_request(
            state.clone(),
            CreateCoreSessionRequest {
                id: Some(id),
                name: Some(name.clone()),
                working_dir: Some(checkout_text.clone()),
                provider: Some(
                    if provider == "claude" {
                        "claude"
                    } else {
                        "codex-fork"
                    }
                    .into(),
                ),
                model: Some(model.into()),
                reasoning_effort: Some(effort.into()),
                initial_message: None,
                parent_session_id: None,
                node: None,
                wait: None,
                spawn_prompt_source: None,
                spawn_brief: None,
            },
        )
        .await
    }
    .await;
    let session = match created {
        Ok(session) => session,
        Err(error) => {
            let detail = api_error_detail(&error);
            let first = detail.lines().next().unwrap_or("unknown error");
            return advance(state, db, r, &format!("failed to start: {first}"));
        }
    };
    paired::insert(
        db,
        &paired::PairedReviewer {
            session_id: session.id.clone(),
            repo: ticket.0.clone(),
            ticket: ticket.1,
            pr_number: r.pr_number,
            provider: provider.into(),
            model: model.into(),
            effort: effort.into(),
            checkout: checkout_text,
            created_at: now_rfc3339(),
            retired_at: None,
            rounds: 0,
            last_review_url: None,
        },
    )
    .map_err(|e| e.to_string())?;
    RetainedQueueStore::attach_paired_reviewer(db, &r.id, r.step_index, &session.id, &name)
        .map_err(|e| e.to_string())?;
    Ok(false)
}

fn api_error_detail(error: &ApiError) -> String {
    match error {
        ApiError::Status { detail, .. } => detail.clone(),
        other => format!("{other:?}"),
    }
}

/// What a round needs from GitHub and the checkout, gathered off the async
/// runtime. `None` when the reviewer's checkout disappeared.
struct Round {
    checkout: PathBuf,
    merge_base: String,
    snapshot: String,
    text: String,
}

#[derive(Serialize, Deserialize)]
struct RoundFile {
    checkout: PathBuf,
    merge_base: String,
}

fn round_file(state: &AppState, r: &CodexReviewRequestRegistration) -> PathBuf {
    review_runs::step_dir(state, r).join("paired.json")
}

fn prepare_round(
    state: &AppState,
    r: &CodexReviewRequestRegistration,
    reviewer: &SessionRecord,
    row: &paired::PairedReviewer,
    author: &str,
) -> Result<Option<Round>> {
    let checkout = PathBuf::from(&row.checkout);
    if !checkout.is_dir() {
        return Ok(None);
    }
    let head = r.requested_head_sha.as_deref().context("no head")?;
    let pr = gh(&[
        "pr",
        "view",
        &r.pr_number.to_string(),
        "--repo",
        &r.repo,
        "--json",
        "title,body,baseRefName,headRefOid,closingIssuesReferences",
    ])?;
    let base = pr["baseRefName"].as_str().context("no base")?;
    // Fetch only moves remote-tracking refs: the author's HEAD, index and
    // files stay as they are.
    git(
        &checkout,
        &[
            "fetch",
            "origin",
            &format!("pull/{}/head", r.pr_number),
            &format!("+refs/heads/{base}:refs/remotes/origin/{base}"),
        ],
    )?;
    let merge_base = git(&checkout, &["merge-base", head, &format!("origin/{base}")])?
        .trim()
        .to_owned();
    let at = git(&checkout, &["rev-parse", "HEAD"])?.trim().to_owned();
    let snapshot = paired::snapshot(&checkout)?;
    let checkout_text = checkout.to_string_lossy();
    let text = if row.rounds == 0 {
        let mut text = format!(
            "You are {}, the paired reviewer for ticket #{} in {}. You work in the author's checkout, {checkout_text}, so that you can build the code and run its tests. You must not edit, commit, push, or change branches there, and you must not comment on GitHub. The author, {author}, is idle while you review.\n\n{}\n\n{}",
            session_display_name(reviewer.clone()),
            row.ticket,
            row.repo,
            review::RUBRIC.trim_end(),
            review_runs::prompt(state, r, &pr, base, &merge_base)?.trim_end(),
        );
        if at != head {
            text.push_str(&format!("\nThe checkout is at {}, not the PR head {}. Read the PR's diff with git this round, and do not build or run anything.", short7(&at), short7(head)));
        }
        if at == head {
            text.push_str(&format!("\n\nYou may build and run tests. Run anything longer than a minute with `sm queue run --type tests --cwd {checkout_text}`."));
        }
        if row.provider == "codex" {
            // Codex's sandbox blocks the local socket `sm` uses until the
            // agent asks for escalation, which sm agents are approved for.
            text.push_str("\n\n`sm` talks to the local Session Manager server. If an `sm` command fails with \"Operation not permitted\", the sandbox blocked it: run the same command again with escalated permissions.");
        }
        text.push_str(&format!("\n\nWhen done, write your review as JSON matching this schema, and run `sm review submit --file <path>`:\n{}\nThen stay idle. If the author pushes and asks again, sm sends you the next round.", review::SCHEMA.trim_end()));
        text
    } else {
        let at_head = if at == head {
            ", and the checkout is at it".to_owned()
        } else {
            format!(
                "; the checkout is at {}, so do not build or run anything this round",
                short7(&at)
            )
        };
        let last = row
            .last_review_url
            .clone()
            .unwrap_or_else(|| format!("https://github.com/{}/pull/{}", r.repo, r.pr_number));
        let mut text = format!("[sm review] Round {} of PR #{}: the author pushed and asked again. The head is now {head}{at_head}. Read the author's replies to your last review first ({last}). Then review the whole PR again. Do not raise a finding the author has answered unless you disagree, and then say why.", r.round, r.pr_number);
        if let Some(steer) = &r.steer {
            text.push_str(&format!("\nThe author asks you to focus on: {steer}"));
        }
        text.push_str("\nSubmit with `sm review submit` as before.");
        text
    };
    Ok(Some(Round {
        checkout,
        merge_base,
        snapshot,
        text,
    }))
}

async fn send_round(
    state: &Arc<AppState>,
    db: &FsPath,
    r: &CodexReviewRequestRegistration,
) -> Result<bool, String> {
    let Some(reviewer_id) = r.reviewer_session_id.clone() else {
        return advance(state, db, r, "stopped unexpectedly");
    };
    let Some(reviewer) = live_session(state, &reviewer_id).map_err(|e| e.to_string())? else {
        return advance(state, db, r, "stopped unexpectedly");
    };
    if !is_idle(state, &reviewer) {
        return Ok(false);
    }
    let Some(row) = paired::get(db, &reviewer.id).map_err(|e| e.to_string())? else {
        return advance(state, db, r, "stopped unexpectedly");
    };
    let author_id = r
        .requester_session_id
        .clone()
        .unwrap_or_else(|| r.notify_session_id.clone());
    let author = state
        .session_store
        .get_session(&author_id)
        .ok()
        .flatten()
        .map(session_display_name)
        .unwrap_or(author_id);
    let prepared = {
        let state = state.clone();
        let r = r.clone();
        let reviewer = reviewer.clone();
        let row = row.clone();
        tokio::task::spawn_blocking(move || prepare_round(&state, &r, &reviewer, &row, &author))
            .await
            .map_err(|e| e.to_string())?
    };
    let round = match prepared {
        Ok(Some(round)) => round,
        Ok(None) => return advance(state, db, r, "no author checkout"),
        Err(error) => {
            let error = error.to_string();
            let first = error.lines().next().unwrap_or("unknown error");
            return advance(state, db, r, &format!("failed to start: {first}"));
        }
    };
    let file = round_file(state, r);
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    fs::write(
        &file,
        serde_json::to_vec(&RoundFile {
            checkout: round.checkout.clone(),
            merge_base: round.merge_base.clone(),
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let sent = RetainedQueueStore::send_paired_round_in_path(
        db,
        &r.id,
        r.step_index,
        &round.snapshot,
        &now_rfc3339(),
        &reviewer.id,
        &round.text,
    )
    .map_err(|e| e.to_string())?;
    if sent {
        paired::record_round(db, &reviewer.id, r.pr_number).map_err(|e| e.to_string())?;
        deliver(state, &reviewer.id);
    }
    Ok(false)
}

fn watch(
    state: &AppState,
    db: &FsPath,
    r: &CodexReviewRequestRegistration,
) -> Result<bool, String> {
    let reviewer = match &r.reviewer_session_id {
        Some(id) => live_session(state, id).map_err(|e| e.to_string())?,
        None => None,
    };
    let Some(reviewer) = reviewer else {
        return advance(state, db, r, "stopped unexpectedly");
    };
    let elapsed = seconds_since(r.step_started_at.as_deref()).unwrap_or(0);
    if elapsed >= GIVE_UP_AFTER_SECONDS {
        return advance(state, db, r, "no review after 90 minutes");
    }
    if r.step_state.as_deref() == Some("reviewing")
        && elapsed >= NUDGE_AFTER_SECONDS
        && is_idle(state, &reviewer)
    {
        RetainedQueueStore::mark_github_review_step_in_path(
            db,
            &r.id,
            "nudged",
            r.step_failures,
            &now_rfc3339(),
        )
        .map_err(|e| e.to_string())?;
        send(state, &reviewer.id, &format!("[sm review] You have not submitted your review of PR #{} after 60 minutes. Submit what you have with `sm review submit` within 30 minutes, or sm gives this round to another reviewer.", r.pr_number)).map_err(|e| e.to_string())?;
    }
    Ok(false)
}

#[derive(Deserialize)]
pub(super) struct SubmitReview {
    session_id: String,
    review: Value,
}

fn refuse(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::CONFLICT,
        detail: detail.into(),
    }
}

/// `POST /review-requests/{id}/submit` (B4).
pub(super) async fn submit(
    State(state): State<Arc<AppState>>,
    Path(request_id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<SubmitReview>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer),
        &format!("/review-requests/{request_id}/submit"),
    )?;
    ensure_core_writes_enabled(&state)?;
    if header_text(&headers, handoff::SESSION_HEADER).as_deref() != Some(body.session_id.as_str()) {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Session identity does not match the request".into(),
        });
    }
    let credential = reparent_session_credential(&headers)?;
    if !state
        .session_store
        .session_credential_matches(&body.session_id, &credential)?
    {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Session credential does not match the caller".into(),
        });
    }
    let db = expand_home(&state.config.sm_send.db_path);
    let not_reviewing = || refuse("You are not reviewing an active request.");
    let r = RetainedQueueStore::get_codex_review_request_from_path(&db, &request_id)?
        .filter(|r| r.is_active)
        .filter(|r| r.reviewer_session_id.as_deref() == Some(body.session_id.as_str()))
        .filter(|r| matches!(r.step_state.as_deref(), Some("reviewing" | "nudged")))
        .ok_or_else(not_reviewing)?;
    if review_runs::current_step(&r).map_err(anyhow::Error::msg)?["kind"] != "paired" {
        return Err(not_reviewing());
    }
    if let Some(problem) = review::submission_problem(&body.review) {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: format!("The review is not valid: {problem}. The schema is in the brief."),
        });
    }
    let head = r
        .requested_head_sha
        .clone()
        .context("request has no head")?;
    match github_current_pr_state(state.github_review_poster.clone(), &r.repo, r.pr_number)
        .await
        .map_err(anyhow::Error::msg)?
    {
        GitHubPullRequestState::Open { head_sha } if head_sha != head => {
            supersede_head_change(&state, &db, &r, &head, &head_sha, false)
                .map_err(anyhow::Error::msg)?;
            return Err(refuse(format!(
                "PR #{} moved to {}; your review of {} is not posted. Stay idle.",
                r.pr_number,
                short7(&head_sha),
                short7(&head)
            )));
        }
        GitHubPullRequestState::Closed { state: pr_state } => {
            terminate_codex_review_request_for_closed_pr(
                &db,
                &r.id,
                r.pr_number,
                &pr_state,
                &now_rfc3339(),
            )
            .map_err(anyhow::Error::msg)?;
            return Err(not_reviewing());
        }
        GitHubPullRequestState::Open { .. } => {}
    }
    let saved: RoundFile = serde_json::from_slice(&fs::read(round_file(&state, &r))?)?;
    let author_id = r
        .requester_session_id
        .clone()
        .unwrap_or_else(|| r.notify_session_id.clone());
    let author = state
        .session_store
        .get_session(&author_id)?
        .map(session_display_name)
        .unwrap_or(author_id);
    let now_snapshot = paired::snapshot(&saved.checkout)?;
    if let Some(changed) =
        paired::snapshot_change(r.checkout_snapshot.as_deref().unwrap_or(""), &now_snapshot)
    {
        return Err(refuse(format!(
            "You changed {changed} in {author}'s checkout. Restore it, then submit again."
        )));
    }
    let posted = {
        let state = state.clone();
        let db = db.clone();
        let r = r.clone();
        let review = body.review.clone();
        tokio::task::spawn_blocking(move || {
            review_runs::post_review(
                &state,
                &db,
                &r,
                PostTarget {
                    checkout: &saved.checkout,
                    merge_base: &saved.merge_base,
                    source: "paired_review",
                    cache: false,
                },
                &review,
                || Ok(()),
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))??
    };
    let Some(posted) = posted else {
        return Err(not_reviewing());
    };
    let url = posted["html_url"].as_str().map(str::to_owned);
    paired::record_review(&db, &body.session_id, url.as_deref())?;
    let counts = review::counts_text(&review::counts(&body.review));
    Ok(Json(json!({
        "request_id": r.id, "pr_number": r.pr_number, "head": head, "counts": counts, "url": url,
        "text": format!(
            "Posted review on PR #{} at {}: {counts}\n{}\nStay idle; sm sends you the next round if the author pushes.",
            r.pr_number, short7(&head), url.as_deref().unwrap_or("")
        ),
    })))
}

/// Kills a reviewer as `sm kill` does. Its checkout is the author's and is
/// left alone.
fn retire_reviewer(state: &AppState, db: &FsPath, live_id: &str, row_id: &str) {
    let result = if state.config.rust_core.runtime_enabled {
        let runtime = TmuxRuntime::from_app_config(&state.config);
        state
            .session_store
            .retire_core_session_with_runtime_authorized(
                live_id,
                RetireAuthority::operator("sm review"),
                None,
                &runtime,
            )
    } else {
        state.session_store.retire_core_session_authorized(
            live_id,
            RetireAuthority::operator("sm review"),
            None,
        )
    };
    if let Err(error) = result {
        eprintln!("retiring paired reviewer {live_id} failed: {error:#}");
        return;
    }
    if let Err(error) = paired::retire(db, row_id, &now_rfc3339()) {
        eprintln!("recording paired reviewer {row_id} retired failed: {error:#}");
    }
}

/// D3b step 7: why a live reviewer is no longer needed, if it is not.
fn retire_reason(
    state: &AppState,
    db: &FsPath,
    row: &paired::PairedReviewer,
) -> Result<Option<&'static str>> {
    let conn = rusqlite::Connection::open(db)?;
    crate::work_claims::init_work_claims_schema(&conn)?;
    let pr_repo: String = conn
        .query_row(
            "SELECT repo FROM codex_review_request_registrations WHERE reviewer_session_id=?1 \
             ORDER BY requested_at DESC LIMIT 1",
            [&row.session_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| row.repo.clone());
    // The author is whoever holds a claim in the reviewer's checkout; a
    // handoff successor keeps the worktree, another agent's takeover does not.
    let mut stmt = conn.prepare(
        "SELECT worktree_path FROM work_claims WHERE ended_at IS NULL AND worktree_path IS NOT NULL \
         AND ((kind='ticket' AND repo=?1 AND number=?2) OR (kind='pr' AND repo=?3 AND number=?4))",
    )?;
    let checkout = PathBuf::from(&row.checkout);
    let author_holds = stmt
        .query_map(
            rusqlite::params![row.repo, row.ticket, pr_repo, row.pr_number],
            |r| r.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|path| expand_home(path) == checkout);
    if !author_holds {
        return Ok(Some("the author's claim ended"));
    }
    let still = crate::review::policy::list(db)?.into_iter().any(|p| {
        p["scope"] == "ticket"
            && p["repo"] == row.repo.as_str()
            && p["number"] == row.ticket
            && p["reviewer"]["kind"] == "paired"
            && p["reviewer"]["provider"] == row.provider.as_str()
            && p["reviewer"]["model"] == row.model.as_str()
            && p["reviewer"]["effort"] == row.effort.as_str()
    });
    if !still {
        return Ok(Some("the ticket's policy changed"));
    }
    if let GitHubPullRequestState::Closed { .. } = state
        .github_review_poster
        .current_pr_state(&pr_repo, row.pr_number)
        .map_err(anyhow::Error::msg)?
    {
        return Ok(Some("the PR is merged or closed"));
    }
    Ok(None)
}

pub(super) fn sweep(state: &AppState) -> Result<()> {
    let db = expand_home(&state.config.sm_send.db_path);
    for row in paired::list_live(&db)? {
        let Some(session) = live_session(state, &row.session_id)? else {
            paired::retire(&db, &row.session_id, &now_rfc3339())?;
            continue;
        };
        match retire_reason(state, &db, &row) {
            Ok(Some(_)) => retire_reviewer(state, &db, &session.id, &row.session_id),
            Ok(None) => {}
            Err(error) => eprintln!("paired reviewer {} check failed: {error:#}", row.session_id),
        }
    }
    Ok(())
}

pub(super) fn start_sweeper(state: Arc<AppState>) {
    if !state.config.rust_core.runtime_enabled {
        return;
    }
    tokio::spawn(async move {
        loop {
            let task_state = state.clone();
            if let Ok(Err(error)) = tokio::task::spawn_blocking(move || sweep(&task_state)).await {
                eprintln!("paired reviewer sweep: {error:#}");
            }
            tokio::time::sleep(Duration::from_secs(RETIRE_SWEEP_SECONDS)).await;
        }
    });
}

/// I2: `waiting_on_review` for authors and `paired_reviewer` for reviewers,
/// keyed by session id.
pub(super) fn session_fields(state: &AppState) -> Result<BTreeMap<String, Value>> {
    let db = expand_home(&state.config.sm_send.db_path);
    let mut fields: BTreeMap<String, Value> = BTreeMap::new();
    let active = RetainedQueueStore::list_active_codex_review_requests_from_path(&db)?;
    for r in &active {
        let author = r
            .requester_session_id
            .clone()
            .unwrap_or_else(|| r.notify_session_id.clone());
        fields.entry(author).or_insert_with(|| json!({}))["waiting_on_review"] = json!({
            "pr_number": r.pr_number, "reviewer_label": r.reviewer_label, "since": r.requested_at,
        });
    }
    for row in paired::list_live(&db)? {
        let current = active
            .iter()
            .find(|r| r.reviewer_session_id.as_deref() == Some(row.session_id.as_str()));
        let author_name = current
            .map(|r| {
                r.requester_session_id
                    .clone()
                    .unwrap_or_else(|| r.notify_session_id.clone())
            })
            .and_then(|id| state.session_store.get_session(&id).ok().flatten())
            .map(session_display_name);
        let (round, request_state) = match current {
            Some(r) if review_runs::current_step(r).is_ok_and(|s| s["kind"] == "paired") => {
                (r.round, r.step_state.clone())
            }
            _ => (row.rounds, None),
        };
        fields
            .entry(row.session_id.clone())
            .or_insert_with(|| json!({}))["paired_reviewer"] = json!({
            "repo": row.repo, "ticket": row.ticket, "pr_number": row.pr_number, "round": round,
            "author_name": author_name, "request_state": request_state,
        });
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request},
    };
    use sha2::{Digest, Sha256};
    use std::sync::Mutex as StdMutex;
    use tower::ServiceExt;

    const HEAD: &str = "1111111111111111111111111111111111111111";

    struct FakeGitHub(StdMutex<String>);
    impl GitHubReviewPoster for FakeGitHub {
        fn post_initial_review_request(
            &self,
            _repo: &str,
            _pr_number: i64,
            _steer: Option<&str>,
        ) -> Result<GitHubReviewComment, String> {
            Err("not used".into())
        }
        fn current_open_pr_head(&self, _repo: &str, _pr: i64) -> Result<String, String> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    fn git_in(dir: &FsPath, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{:?}", out);
    }

    #[test]
    fn a_takeover_with_another_checkout_retires_the_reviewer() {
        let dir = std::env::temp_dir().join(format!(
            "sm-paired-retire-{}-{}",
            std::process::id(),
            random_urlsafe_token(8)
        ));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("queue.db");
        crate::review::policy::ensure_schema(&db).unwrap();
        RetainedQueueStore::ensure_codex_review_requests_schema_from_path(&db).unwrap();
        let reviewer =
            json!({"kind":"paired","provider":"codex","model":"gpt-6-luna","effort":"medium"});
        crate::review::policy::set(
            &db,
            crate::review::policy::PolicyChange {
                scope: "ticket",
                repo: "far/repo",
                number: 1848,
                reviewer: Some(&reviewer),
                session_id: None,
                name: "Rajesh",
                now: "now",
            },
        )
        .unwrap();
        let row = paired::PairedReviewer {
            session_id: "reviewer".into(),
            repo: "far/repo".into(),
            ticket: 1848,
            pr_number: 7,
            provider: "codex".into(),
            model: "gpt-6-luna".into(),
            effort: "medium".into(),
            checkout: expand_home("~/worktrees/far-1848")
                .to_string_lossy()
                .into_owned(),
            created_at: "now".into(),
            retired_at: None,
            rounds: 1,
            last_review_url: None,
        };
        let mut config = AppConfig::default();
        config.sm_send.db_path = db.display().to_string();
        let state = AppState::new(config)
            .with_github_review_poster(Arc::new(FakeGitHub(StdMutex::new(HEAD.into()))));
        let conn = rusqlite::Connection::open(&db).unwrap();
        // The author's claim records its worktree with `~`.
        conn.execute_batch("INSERT INTO work_claims(id,repo,number,kind,session_id,source,claimed_at,worktree_path) VALUES
            ('c1','far/repo',1848,'ticket','author','test','now','~/worktrees/far-1848');").unwrap();
        assert_eq!(retire_reason(&state, &db, &row).unwrap(), None);
        conn.execute_batch("UPDATE work_claims SET ended_at='later' WHERE id='c1';
            INSERT INTO work_claims(id,repo,number,kind,session_id,source,claimed_at,worktree_path) VALUES
            ('c2','far/repo',1848,'ticket','other','test','later','~/worktrees/far-1848-other');").unwrap();
        assert_eq!(
            retire_reason(&state, &db, &row).unwrap(),
            Some("the author's claim ended")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn submit_refuses_strangers_bad_json_changed_checkouts_and_moved_heads() {
        let dir = std::env::temp_dir().join(format!(
            "sm-paired-submit-{}-{}",
            std::process::id(),
            random_urlsafe_token(8)
        ));
        let checkout = dir.join("author-checkout");
        fs::create_dir_all(&checkout).unwrap();
        git_in(&checkout, &["init", "-q"]);
        git_in(&checkout, &["config", "user.email", "t@example.com"]);
        git_in(&checkout, &["config", "user.name", "t"]);
        fs::write(checkout.join("a.txt"), "one\n").unwrap();
        git_in(&checkout, &["add", "a.txt"]);
        git_in(&checkout, &["commit", "-qm", "one"]);

        let state_file = dir.join("sessions.json");
        let hash = |s: &[u8]| format!("{:x}", Sha256::digest(s));
        fs::write(&state_file, serde_json::to_vec(&json!({"sessions":[
            {"id":"author","name":"far-1848","working_dir":"/repo","tmux_session":"author","provider":"claude","status":"running","created_at":"2026-09-30T00:00:00","last_activity":"2026-09-30T00:00:00","session_credential_sha256":hash(b"author-secret")},
            {"id":"reviewer","name":"far-1848-reviewer","working_dir":"/repo","tmux_session":"reviewer","provider":"codex-fork","status":"running","created_at":"2026-09-30T00:00:00","last_activity":"2026-09-30T00:00:00","session_credential_sha256":hash(b"reviewer-secret")}
        ]})).unwrap()).unwrap();
        let db = dir.join("queue.db");
        let r = RetainedQueueStore::create_codex_review_request_in_path(
            &db,
            CreateCodexReviewRequest {
                repo: "far/repo".into(),
                pr_number: 7,
                requester_session_id: Some("author".into()),
                notify_session_id: "author".into(),
                steer: None,
                requested_head_sha: HEAD.into(),
                latest_request_comment_id: None,
                latest_request_comment_url: None,
                latest_request_posted_at: now_rfc3339(),
                poll_interval_seconds: 30,
                retry_interval_seconds: 120,
            },
        )
        .unwrap();
        let step = json!({"kind":"paired","provider":"codex","model":"gpt-6-astra","effort":"high",
            "ticket":{"repo":"far/repo","number":1848}});
        let chain = review::chain(&step);
        assert_eq!(chain.len(), 3);
        assert_eq!(
            chain[1],
            json!({"kind":"codex","model":"gpt-6-astra","effort":"high"})
        );
        assert_eq!(chain[2]["model"], "fable");
        RetainedQueueStore::initialize_review_chain(&db, &r.id, &chain, "ticket #1848").unwrap();
        RetainedQueueStore::attach_paired_reviewer(&db, &r.id, 0, "reviewer", "far-1848-reviewer")
            .unwrap();
        paired::insert(
            &db,
            &paired::PairedReviewer {
                session_id: "reviewer".into(),
                repo: "far/repo".into(),
                ticket: 1848,
                pr_number: 7,
                provider: "codex".into(),
                model: "gpt-6-astra".into(),
                effort: "high".into(),
                checkout: checkout.to_string_lossy().into_owned(),
                created_at: now_rfc3339(),
                retired_at: None,
                rounds: 0,
                last_review_url: None,
            },
        )
        .unwrap();
        let snapshot = paired::snapshot(&checkout).unwrap();
        assert!(RetainedQueueStore::send_paired_round_in_path(
            &db,
            &r.id,
            0,
            &snapshot,
            &now_rfc3339(),
            "reviewer",
            "round one"
        )
        .unwrap());
        // A second send for the same round is a no-op.
        assert!(!RetainedQueueStore::send_paired_round_in_path(
            &db,
            &r.id,
            0,
            &snapshot,
            &now_rfc3339(),
            "reviewer",
            "round one"
        )
        .unwrap());
        let round_dir = dir.join("reviews").join(&r.id).join("r1-s0");
        fs::create_dir_all(&round_dir).unwrap();
        fs::write(
            round_dir.join("paired.json"),
            serde_json::to_vec(&RoundFile {
                checkout: checkout.clone(),
                merge_base: HEAD.into(),
            })
            .unwrap(),
        )
        .unwrap();

        let mut config = AppConfig::default();
        config.paths.state_file = state_file.display().to_string();
        config.sm_send.db_path = db.display().to_string();
        config.rust_core.fixture_writes_enabled = true;
        let github = Arc::new(FakeGitHub(StdMutex::new(HEAD.into())));
        let state = AppState::new(config).with_github_review_poster(github.clone());

        let fields = session_fields(&state).unwrap();
        assert_eq!(fields["author"]["waiting_on_review"]["pr_number"], 7);
        assert_eq!(
            fields["author"]["waiting_on_review"]["reviewer_label"],
            "far-1848-reviewer"
        );
        assert_eq!(fields["reviewer"]["paired_reviewer"]["ticket"], 1848);
        assert_eq!(
            fields["reviewer"]["paired_reviewer"]["request_state"],
            "reviewing"
        );
        assert_eq!(
            fields["reviewer"]["paired_reviewer"]["author_name"],
            "far-1848"
        );

        let app = router(state);
        let submit = |session: &str, secret: &str, review: Value| {
            let mut request = Request::builder()
                .method(Method::POST)
                .uri(format!("/review-requests/{}/submit", r.id))
                .header("host", "testserver")
                .header("content-type", "application/json")
                .header("x-sm-session", session)
                .header("x-sm-session-credential", secret)
                .body(Body::from(
                    json!({"session_id":session,"review":review}).to_string(),
                ))
                .unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4200))));
            request
        };
        async fn detail(app: &Router, request: Request<Body>) -> (StatusCode, String) {
            let response = app.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or_default();
            (
                status,
                body["detail"].as_str().unwrap_or_default().to_owned(),
            )
        }
        let review = json!({"findings":[],"overall_correctness":"patch is correct"});
        assert_eq!(
            detail(&app, submit("author", "author-secret", review.clone())).await,
            (
                StatusCode::CONFLICT,
                "You are not reviewing an active request.".into()
            )
        );
        assert_eq!(
            detail(&app, submit("reviewer", "author-secret", review.clone()))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            detail(
                &app,
                submit("reviewer", "reviewer-secret", json!({"findings":"none"}))
            )
            .await,
            (
                StatusCode::BAD_REQUEST,
                "The review is not valid: findings must be an array. The schema is in the brief."
                    .into()
            )
        );
        fs::write(checkout.join("a.txt"), "edited\n").unwrap();
        assert_eq!(
            detail(&app, submit("reviewer", "reviewer-secret", review.clone())).await,
            (
                StatusCode::CONFLICT,
                "You changed a.txt in far-1848's checkout. Restore it, then submit again.".into()
            )
        );
        git_in(&checkout, &["checkout", "--", "a.txt"]);
        *github.0.lock().unwrap() = "2222222222222222222222222222222222222222".into();
        assert_eq!(
            detail(&app, submit("reviewer", "reviewer-secret", review.clone())).await,
            (
                StatusCode::CONFLICT,
                "PR #7 moved to 2222222; your review of 1111111 is not posted. Stay idle.".into()
            )
        );
        let after = RetainedQueueStore::get_codex_review_request_from_path(&db, &r.id)
            .unwrap()
            .unwrap();
        assert!(!after.is_active);

        // A handoff moves the reviewer's registry row and any live request.
        RetainedQueueStore::new(db.clone())
            .hand_off_rows("reviewer", "reviewer-h2")
            .unwrap();
        assert!(paired::get(&db, "reviewer").unwrap().is_none());
        assert_eq!(
            paired::get(&db, "reviewer-h2").unwrap().unwrap().ticket,
            1848
        );
        let _ = fs::remove_dir_all(dir);
    }
}
