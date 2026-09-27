//! Owner doc review submission (sm#1447 / #1451): the owner's drafts become
//! one GitHub PR review, and the doc's author is woken with `[sm review]`.
//!
//! GitHub behaviour this relies on is in the spec's "GitHub API findings".
//! A pending review lets each comment fall back to a file-level thread on
//! its own (F1, F2, F3, F5); the event is always `COMMENT` because the owner
//! authors the PRs agents open (F4).
//!
//! Idempotency: the `owner_doc_reviews` row (keyed by the client's
//! `submission_id`) is recorded before any GitHub call, and the review body
//! carries `<!-- sm-review:<submission_id> -->`. A retry or a restart that
//! finds the row `submitting` reconciles against GitHub by that marker
//! instead of starting over. The row turns `posted` in the same transaction
//! that deletes the drafts and queues the wake, so the wake fires once.

use super::*;
use crate::owner_docs::{OwnerDocDraft, OwnerDocReview, OwnerDocVerdict, PostedOwnerDocReview};

pub(super) fn review_marker(submission_id: &str) -> String {
    format!("<!-- sm-review:{submission_id} -->")
}

/// The review body: verdict header, the owner's overall text, the marker.
pub(super) fn review_body(
    verdict: OwnerDocVerdict,
    body: Option<&str>,
    submission_id: &str,
) -> String {
    let mut text = verdict.body_header().to_owned();
    if let Some(body) = body.map(str::trim).filter(|body| !body.is_empty()) {
        text.push_str("\n\n");
        text.push_str(body);
    }
    text.push_str("\n\n");
    text.push_str(&review_marker(submission_id));
    text
}

/// Every comment quotes the selection: `> <quote>\n\n<comment>`.
pub(super) fn comment_body(draft: &OwnerDocDraft) -> String {
    let quote = draft.quote.trim();
    if quote.is_empty() {
        return draft.body.clone();
    }
    format!("> {}\n\n{}", quote.replace('\n', "\n> "), draft.body)
}

fn plural(count: i64, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

pub(super) fn render_owner_review_wake(
    owner_name: &str,
    doc: &OwnerDoc,
    review: &OwnerDocReview,
    review_url: &str,
    line_comments: i64,
    file_comments: i64,
) -> String {
    let verdict = OwnerDocVerdict::parse(&review.verdict)
        .map(OwnerDocVerdict::wake_label)
        .unwrap_or(&review.verdict);
    let mut counts = Vec::new();
    if line_comments > 0 {
        counts.push(plural(line_comments, "line comment"));
    }
    if file_comments > 0 {
        counts.push(plural(file_comments, "file comment"));
    }
    let overall = review
        .body
        .as_deref()
        .map(str::trim)
        .filter(|body| !body.is_empty());
    if counts.is_empty() {
        counts.push(
            if overall.is_some() {
                "no line or file comments"
            } else {
                "no comments"
            }
            .to_owned(),
        );
    }
    let mut wake = format!(
        "[sm review] {owner_name}'s review of \"{}\" (PR #{} @ {}) is here: {review_url}\nVerdict: {verdict} · {}",
        doc.title,
        doc.pr_number.unwrap_or_default(),
        &review.commit_sha[..review.commit_sha.len().min(7)],
        counts.join(" · ")
    );
    // The overall text carries instructions the verdict alone does not
    // (sm#1578), so it rides in the wake instead of only behind the link.
    if let Some(overall) = overall {
        wake.push_str(&format!("\n{owner_name} wrote:\n"));
        wake.push_str(&quote_overall(overall));
    }
    wake
}

/// Longest overall text a wake quotes in full; past it the wake points at
/// the review for the rest.
const WAKE_OVERALL_MAX_CHARS: usize = 4000;

fn quote_overall(overall: &str) -> String {
    let (text, truncated) = match overall.char_indices().nth(WAKE_OVERALL_MAX_CHARS) {
        Some((cut, _)) => (&overall[..cut], true),
        None => (overall, false),
    };
    let mut quoted = text
        .lines()
        .map(|line| format!("> {line}").trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    if truncated {
        quoted.push_str("\n> … (truncated; read the rest at the link above)");
    }
    quoted
}

pub(super) fn is_retired(session: &SessionRecord) -> bool {
    super::super::messages::session_ended(session)
}

/// The author if it still exists (a stopped session gets the queued message
/// on restore), else the retired author's parent, else nobody: the rule
/// message replies use too.
pub(super) fn review_wake_recipient(state: &AppState, doc: &OwnerDoc) -> Option<String> {
    super::super::messages::live_recipient(state, &doc.author_session_id).map(|session| session.id)
}

#[derive(Debug, Deserialize)]
pub(super) struct SubmitReviewRequest {
    submission_id: String,
    sha: String,
    verdict: String,
    #[serde(default)]
    body: Option<String>,
}

fn valid_submission_id(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// A GitHub call failed. 503, not 502: Cloudflare replaces an origin 502 or
/// 504 with its own error, which would hide this detail from the page
/// (sm#1591).
fn github_failure(detail: String) -> ApiError {
    ApiError::Status {
        status: StatusCode::SERVICE_UNAVAILABLE,
        detail,
    }
}

fn conflict(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::CONFLICT,
        detail: detail.into(),
    }
}

/// What a page needs to resume an unfinished submission: its id and the
/// verdict and text it will post.
pub(super) fn unfinished_review_json(review: &OwnerDocReview) -> Value {
    json!({
        "id": review.id,
        "verdict": review.verdict,
        "body": review.body.clone().unwrap_or_default(),
    })
}

/// 409 for a submit that another submission of the revision takes over.
/// `unfinished_review` is the one to resume, or null when the newer one is
/// done and the page should start afresh from the current drafts.
fn superseded(unfinished: Option<OwnerDocReview>) -> ApiError {
    let detail = if unfinished.is_some() {
        "Another device is submitting a review of this revision, with the verdict and text shown here. Submit again to finish it."
    } else {
        "A newer review of this revision was submitted from another device, so this attempt is closed. Check the drafts, then submit."
    };
    ApiError::StatusBody {
        status: StatusCode::CONFLICT,
        body: json!({
            "detail": detail,
            "unfinished_review": unfinished.as_ref().map(unfinished_review_json),
        }),
    }
}

fn review_response(review: &OwnerDocReview) -> Value {
    json!({
        "submission_id": review.id,
        "status": review.status,
        "verdict": review.verdict,
        "commit_sha": review.commit_sha,
        "line_comment_count": review.line_comment_count,
        "file_comment_count": review.file_comment_count,
        "github_review_id": review.github_review_id,
        "github_review_url": review.github_review_url,
        "delivered_to_session_id": review.delivered_to_session_id,
        "submitted_at": review.submitted_at,
    })
}

/// `POST /docs/{id}/review`.
pub(super) async fn submit_owner_doc_review(
    state: &Arc<AppState>,
    doc: &OwnerDoc,
    payload: SubmitReviewRequest,
) -> Result<Value, ApiError> {
    let submission_id = payload.submission_id.trim().to_owned();
    if !valid_submission_id(&submission_id) {
        return Err(bad_request(
            "submission_id must be 8-64 letters, digits, '-' or '_'",
        ));
    }
    let sha = payload.sha.trim().to_ascii_lowercase();
    if !is_full_commit_sha(&sha) {
        return Err(bad_request("sha must be a full 40-character commit SHA"));
    }
    let verdict = OwnerDocVerdict::parse(payload.verdict.trim())
        .ok_or_else(|| bad_request("verdict must be approve, changes_requested or comment"))?;
    let body = payload
        .body
        .as_deref()
        .map(str::trim)
        .filter(|body| !body.is_empty())
        .map(ToOwned::to_owned);

    let _guard = state.owner_doc_review_lock.lock().await;
    let store = owner_doc_store(state);
    if let Some(existing) = store.review(&submission_id)? {
        return match existing.status.as_str() {
            _ if existing.doc_id != doc.id => Err(conflict("submission_id belongs to another doc")),
            "posted" => Ok(review_response(&existing)),
            // A retry of any unfinished submission reconciles against
            // GitHub by its marker before doing anything else, so the
            // client keeps one id until the review is posted.
            _ => {
                let existing = store.reopen_review(&existing.id)?;
                if existing.status == "submitting" {
                    run_blocking(state, existing, None).await
                } else {
                    Err(superseded(
                        store.unfinished_review(&doc.id, &existing.commit_sha)?,
                    ))
                }
            }
        };
    }

    // An unfinished submission of this revision under another id (another
    // device's) is finished first: starting a second one could post the
    // drafts twice. Its verdict and text may differ from what this page
    // shows, so the page is handed them to confirm rather than posting them
    // unseen.
    if let Some(unfinished) = store.unfinished_review(&doc.id, &sha)? {
        return Err(superseded(Some(unfinished)));
    }
    let Some(pr_number) = doc.pr_number else {
        return Err(conflict("This doc has no PR, so it is read-only"));
    };
    let (lookup_state, repo) = (state.clone(), doc.repo.clone());
    let pr = tokio::task::spawn_blocking(move || {
        doc_pull_request(&lookup_state, &repo, pr_number, true)
    })
    .await
    .map_err(|error| anyhow::anyhow!("PR lookup task failed: {error}"))?
    .map_err(github_failure)?;
    if !pr.is_open() {
        return Err(conflict(format!(
            "PR #{pr_number} is {}, so the doc is read-only",
            pr.state
        )));
    }
    let bytes = load_doc_bytes_async(state, doc, &sha).await?;
    let (review, inserted) = store.begin_review(
        &submission_id,
        &doc.id,
        &sha,
        &git_blob_sha(&bytes),
        verdict,
        body.as_deref(),
    )?;
    run_blocking(state, review, inserted.then_some(pr)).await
}

async fn run_blocking(
    state: &Arc<AppState>,
    review: OwnerDocReview,
    fresh: Option<DocPullRequest>,
) -> Result<Value, ApiError> {
    let state = state.clone();
    let result = tokio::task::spawn_blocking(move || run_submission(&state, review, fresh))
        .await
        .map_err(|error| anyhow::anyhow!("review submit task failed: {error}"))?;
    result.map(|review| review_response(&review))
}

/// Carries a `submitting` row to `posted` or `failed`. `fresh` is the open
/// PR when this call just inserted the row, so there is nothing on GitHub
/// yet; otherwise the row is reconciled against GitHub first.
fn run_submission(
    state: &AppState,
    review: OwnerDocReview,
    fresh: Option<DocPullRequest>,
) -> Result<OwnerDocReview, ApiError> {
    let store = owner_doc_store(state);
    let doc = store
        .get(&review.doc_id)?
        .ok_or(ApiError::NotFound("Doc not found"))?;
    let pr_number = doc
        .pr_number
        .ok_or_else(|| conflict("This doc has no PR, so it is read-only"))?;
    let drafts: Vec<OwnerDocDraft> = store
        .drafts(&doc.id)?
        .into_iter()
        .filter(|draft| draft.commit_sha == review.commit_sha)
        .collect();
    let verdict = OwnerDocVerdict::parse(&review.verdict).unwrap_or(OwnerDocVerdict::Comment);
    let body = review_body(verdict, review.body.as_deref(), &review.id);
    let marker = review_marker(&review.id);
    let source = state.owner_doc_source.as_ref();

    let pr = match fresh {
        Some(pr) => pr,
        None => {
            let on_github = source
                .viewer_reviews(&doc.repo, pr_number)
                .map_err(github_failure)?;
            let marked = |pending: bool| {
                on_github
                    .iter()
                    .find(|r| r.body.contains(&marker) && (r.state == "PENDING") == pending)
            };
            if let Some(submitted) = marked(false) {
                return finish_from_github(state, &doc, &review, &drafts, submitted);
            }
            if let Some(pending) = marked(true) {
                let existing: Vec<_> = pending.comments.clone();
                return post_and_submit(
                    state,
                    &doc,
                    &review,
                    &drafts,
                    &body,
                    &pending.node_id,
                    existing,
                );
            }
            if let Some(stale) = review.pending_review_node_id.as_deref() {
                let _ = source.delete_pending_review(stale);
                store.set_pending_review_node_id(&review.id, None)?;
            }
            let pr = doc_pull_request(state, &doc.repo, pr_number, true).map_err(github_failure)?;
            if !pr.is_open() {
                store.fail_review(&review.id)?;
                return Err(conflict(format!(
                    "PR #{pr_number} is {}, so the doc is read-only",
                    pr.state
                )));
            }
            pr
        }
    };

    // GitHub takes comments only on files the PR changes. Say so rather
    // than let every comment fail (sm#1591). Only the head revision is
    // checked: an older one resolves against the diff at that commit, which
    // may still hold the doc (spec F7). A failed lookup leaves the answer to
    // GitHub.
    if !drafts.is_empty()
        && review.commit_sha == pr.head_sha
        && matches!(
            source.pr_changes_path(&doc.repo, pr_number, &doc.path),
            Ok(false)
        )
    {
        store.fail_review(&review.id)?;
        return Err(conflict(format!(
            "{} Ask the doc's author to push a change to it on the PR, then submit again. Your comments are kept.",
            doc_not_in_pr_diff(pr_number, &doc.path)
        )));
    }
    let pending_id = match source.add_pending_review(&pr.node_id, &review.commit_sha, &body) {
        Ok(id) => id,
        // The review may exist even though the response was lost: look for
        // it by marker before calling the attempt failed.
        Err(error) => match source.viewer_reviews(&doc.repo, pr_number) {
            Ok(on_github) => match on_github.iter().find(|r| r.body.contains(&marker)) {
                Some(found) if found.state != "PENDING" => {
                    return finish_from_github(state, &doc, &review, &drafts, found)
                }
                Some(found) => found.node_id.clone(),
                None => {
                    store.fail_review(&review.id)?;
                    return Err(github_failure(format!(
                        "GitHub refused the review: {error}"
                    )));
                }
            },
            // Unknown: leave the row submitting; a retry reconciles it.
            Err(_) => {
                return Err(github_failure(format!(
                    "GitHub refused the review: {error}"
                )))
            }
        },
    };
    store.set_pending_review_node_id(&review.id, Some(&pending_id))?;
    post_and_submit(
        state,
        &doc,
        &review,
        &drafts,
        &body,
        &pending_id,
        Vec::new(),
    )
}

/// Adds each draft's thread to the pending review (skipping ones whose body
/// is already there, when resuming), submits it, and finishes the row.
fn post_and_submit(
    state: &AppState,
    doc: &OwnerDoc,
    review: &OwnerDocReview,
    drafts: &[OwnerDocDraft],
    body: &str,
    pending_id: &str,
    existing: Vec<(String, bool)>,
) -> Result<OwnerDocReview, ApiError> {
    let source = state.owner_doc_source.as_ref();
    let mut existing = existing;
    let (mut line_comments, mut file_comments) = (0i64, 0i64);
    for draft in drafts {
        let text = comment_body(draft);
        if let Some(position) = existing.iter().position(|(posted, _)| *posted == text) {
            let (_, has_line) = existing.remove(position);
            if has_line {
                line_comments += 1;
            } else {
                file_comments += 1;
            }
            continue;
        }
        // A null thread means the line didn't take; the comment then goes
        // on the file, still quoting its selection. An error may be a lost
        // response, so check the pending review before falling back.
        if let Some(line) = draft.line {
            match source.add_review_thread(pending_id, &doc.path, Some(line), &text) {
                Ok(true) => {
                    line_comments += 1;
                    continue;
                }
                Ok(false) => {}
                Err(error) => match pending_thread(state, doc, pending_id, &text) {
                    Ok(Some(true)) => {
                        line_comments += 1;
                        continue;
                    }
                    Ok(Some(false)) => {
                        file_comments += 1;
                        continue;
                    }
                    Ok(None) => {}
                    Err(_) => return abort(state, doc, review, drafts, pending_id, error),
                },
            }
        }
        match source.add_review_thread(pending_id, &doc.path, None, &text) {
            Ok(true) => file_comments += 1,
            Ok(false) => {
                return abort(
                    state,
                    doc,
                    review,
                    drafts,
                    pending_id,
                    "GitHub did not create a file comment".to_owned(),
                )
            }
            Err(error) => return abort(state, doc, review, drafts, pending_id, error),
        }
    }
    match source.submit_pending_review(pending_id, body) {
        Ok(submitted) => finish(
            state,
            doc,
            review,
            drafts,
            submitted.database_id,
            &submitted.url,
            line_comments,
            file_comments,
        ),
        Err(error) => abort(state, doc, review, drafts, pending_id, error),
    }
}

/// Whether the pending review already holds a thread with `text`, and if so
/// whether it is on a line.
fn pending_thread(
    state: &AppState,
    doc: &OwnerDoc,
    pending_id: &str,
    text: &str,
) -> Result<Option<bool>, String> {
    let pr_number = doc.pr_number.ok_or("doc has no PR")?;
    let on_github = state
        .owner_doc_source
        .viewer_reviews(&doc.repo, pr_number)?;
    Ok(on_github
        .iter()
        .find(|review| review.node_id == pending_id)
        .and_then(|review| review.comments.iter().find(|(body, _)| body == text))
        .map(|(_, has_line)| *has_line))
}

/// A GitHub error after the pending review exists. If the review was in
/// fact submitted (the response was lost), finish from it; otherwise delete
/// the pending review so none is left under the owner's account, and mark
/// the row failed with the drafts kept. If the delete fails, the pending
/// review may still be there, so the row stays `submitting` and a retry
/// resumes it.
fn abort(
    state: &AppState,
    doc: &OwnerDoc,
    review: &OwnerDocReview,
    drafts: &[OwnerDocDraft],
    pending_id: &str,
    error: String,
) -> Result<OwnerDocReview, ApiError> {
    let source = state.owner_doc_source.as_ref();
    let marker = review_marker(&review.id);
    if let Some(pr_number) = doc.pr_number {
        if let Ok(on_github) = source.viewer_reviews(&doc.repo, pr_number) {
            if let Some(submitted) = on_github
                .iter()
                .find(|r| r.body.contains(&marker) && r.state != "PENDING")
            {
                return finish_from_github(state, doc, review, drafts, submitted);
            }
        }
    }
    if let Err(delete_error) = source.delete_pending_review(pending_id) {
        eprintln!(
            "Owner doc review {}: could not delete pending review {pending_id}: {delete_error}",
            review.id
        );
        return Err(github_failure(format!(
            "GitHub refused the review: {error}. Submitting again resumes it."
        )));
    }
    owner_doc_store(state).fail_review(&review.id)?;
    Err(github_failure(format!(
        "GitHub refused the review: {error}"
    )))
}

fn finish_from_github(
    state: &AppState,
    doc: &OwnerDoc,
    review: &OwnerDocReview,
    drafts: &[OwnerDocDraft],
    submitted: &DocReviewOnGitHub,
) -> Result<OwnerDocReview, ApiError> {
    let line_comments = submitted.comments.iter().filter(|(_, line)| *line).count() as i64;
    let file_comments = submitted.comments.len() as i64 - line_comments;
    finish(
        state,
        doc,
        review,
        drafts,
        submitted.database_id,
        &submitted.url,
        line_comments,
        file_comments,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish(
    state: &AppState,
    doc: &OwnerDoc,
    review: &OwnerDocReview,
    drafts: &[OwnerDocDraft],
    github_review_id: Option<i64>,
    review_url: &str,
    line_comments: i64,
    file_comments: i64,
) -> Result<OwnerDocReview, ApiError> {
    let wake = review_wake_recipient(state, doc).map(|session_id| {
        (
            session_id,
            render_owner_review_wake(
                &state.config.owner_name,
                doc,
                review,
                review_url,
                line_comments,
                file_comments,
            ),
        )
    });
    let (posted, changed) = owner_doc_store(state).finish_review(
        &review.id,
        &PostedOwnerDocReview {
            github_review_id,
            github_review_url: review_url.to_owned(),
            line_comment_count: line_comments,
            file_comment_count: file_comments,
            draft_ids: drafts.iter().map(|draft| draft.id.clone()).collect(),
            wake,
        },
    )?;
    if changed && state.config.rust_core.runtime_enabled {
        if let Some(target) = posted.delivered_to_session_id.as_deref() {
            let runtime = TmuxRuntime::from_app_config(&state.config);
            if let Err(error) = state
                .session_store
                .drain_runtime_pending_messages_for_session(target, &runtime)
            {
                eprintln!(
                    "Owner doc review {}: immediate wake delivery failed: {error:#}",
                    review.id
                );
            }
        }
    }
    Ok(posted)
}

#[derive(Debug, Deserialize)]
pub(super) struct AssignReviewRequest {
    review_id: String,
}

/// The task a new agent gets for a review nobody was left to take.
pub(super) fn assign_task_text(
    owner_name: &str,
    doc: &OwnerDoc,
    review: &OwnerDocReview,
) -> String {
    let pr = doc.pr_number.unwrap_or_default();
    format!(
        "[sm review] Your task is {owner_name}'s latest review of \"{title}\" on PR #{pr} in {repo}. \
         The agent that wrote the doc has ended, so the review is yours.\n{wake}\n\
         Check out the PR branch in your own worktree, run `sm pr` from it to claim the PR, \
         address the review, push, and republish with `sm doc publish {path} --pr {pr} --review`.",
        title = doc.title,
        repo = doc.repo,
        path = doc.path,
        wake = render_owner_review_wake(
            owner_name,
            doc,
            review,
            review.github_review_url.as_deref().unwrap_or_default(),
            review.line_comment_count,
            review.file_comment_count,
        ),
    )
}

/// `POST /docs/{id}/assign` (sm#1580, appendix D5): starts an agent of the
/// ended author's type and model in the repo's checkout, with the doc's
/// latest undelivered review as its task. Eligibility is checked here, not
/// on the page, so a stale page can never start a second agent.
pub(super) async fn assign_review(
    state: &Arc<AppState>,
    doc: &OwnerDoc,
    payload: AssignReviewRequest,
) -> Result<Value, ApiError> {
    let _guard = state.owner_doc_review_lock.lock().await;
    let store = owner_doc_store(state);
    let review_id = payload.review_id.trim();
    let review = store
        .review(review_id)?
        .filter(|review| review.doc_id == doc.id && review.status == "posted")
        .ok_or_else(|| conflict("Review not found"))?;
    if let Some(session_id) = review.delivered_to_session_id.as_deref() {
        return Err(conflict(format!(
            "Review is already with {}",
            session_name_or_id(state, session_id)
        )));
    }
    let latest = store
        .reviews(&doc.id)?
        .into_iter()
        .rfind(|review| review.status == "posted");
    if latest.as_ref().map(|latest| latest.id.as_str()) != Some(review.id.as_str()) {
        return Err(conflict("A newer review exists; reload"));
    }
    if let Some(session_id) = review_wake_recipient(state, doc) {
        return Err(conflict(format!(
            "The doc has an agent again: {}",
            session_name_or_id(state, &session_id)
        )));
    }
    let working_dir = store
        .publishes(&doc.id)?
        .into_iter()
        .rev()
        .filter_map(|publish| publish.checkout_root)
        .find(|root| StdPath::new(root).is_dir())
        .ok_or_else(|| {
            conflict(format!(
                "No local checkout is known for {}; start the agent yourself.",
                crate::owner_docs::repo_name(&doc.repo)
            ))
        })?;
    let author = state.session_store.get_session(&doc.author_session_id)?;
    let payload = CreateCoreSessionRequest {
        id: None,
        name: None,
        working_dir: Some(working_dir),
        provider: author.as_ref().map(|author| author.provider.clone()),
        parent_session_id: None,
        node: None,
        initial_message: Some(assign_task_text(&state.config.owner_name, doc, &review)),
        model: author.as_ref().and_then(|author| author.model.clone()),
        reasoning_effort: author
            .as_ref()
            .and_then(|author| author.reasoning_effort.clone()),
        wait: None,
        spawn_prompt_source: Some(SpawnBriefSource {
            kind: "positional".to_owned(),
            path: None,
        }),
        spawn_brief: None,
    };
    let session = super::super::create_session_from_request(state.clone(), payload).await?;
    store.assign_review(&review.id, &session.id)?;
    Ok(json!({
        "session_id": session.id,
        "name": session_display_name(session.clone()),
    }))
}

fn session_name_or_id(state: &AppState, session_id: &str) -> String {
    state
        .session_store
        .get_session(session_id)
        .ok()
        .flatten()
        .map(session_display_name)
        .unwrap_or_else(|| session_id.to_owned())
}

/// Startup: finish or restart any submit a crash left `submitting`.
pub(in crate::http) fn recover_owner_doc_reviews(state: Arc<AppState>) {
    if !state.config.rust_core.runtime_enabled {
        return;
    }
    tokio::spawn(async move {
        let _guard = state.owner_doc_review_lock.lock().await;
        let pending = match owner_doc_store(&state).submitting_reviews() {
            Ok(pending) => pending,
            Err(error) => {
                eprintln!("Owner doc review recovery failed to list rows: {error:#}");
                return;
            }
        };
        for review in pending {
            let id = review.id.clone();
            let task_state = state.clone();
            match tokio::task::spawn_blocking(move || run_submission(&task_state, review, None))
                .await
            {
                Ok(Ok(review)) => eprintln!("Owner doc review {id} recovered: {}", review.status),
                Ok(Err(error)) => eprintln!("Owner doc review {id} recovery failed: {error:?}"),
                Err(error) => eprintln!("Owner doc review {id} recovery task failed: {error}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(title: &str) -> OwnerDoc {
        OwnerDoc {
            id: "d0c00001".into(),
            repo: "acme/widgets".into(),
            path: "specs/memo.html".into(),
            pr_number: Some(12),
            author_session_id: "author01".into(),
            author_session_name: None,
            title: title.into(),
            note: None,
            retracted_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn review(verdict: &str) -> OwnerDocReview {
        OwnerDocReview {
            id: "sub-0001".into(),
            status: "submitting".into(),
            pending_review_node_id: None,
            doc_id: "d0c00001".into(),
            commit_sha: "abcdef0123456789abcdef0123456789abcdef01".into(),
            blob_sha: "b".repeat(40),
            verdict: verdict.into(),
            body: None,
            line_comment_count: 0,
            file_comment_count: 0,
            github_review_id: None,
            github_review_url: None,
            submitted_at: String::new(),
            delivered_to_session_id: None,
        }
    }

    #[test]
    fn wake_names_the_doc_pr_version_verdict_and_counts() {
        assert_eq!(
            render_owner_review_wake(
                "Rajesh",
                &doc("Decision memo"),
                &review("changes_requested"),
                "https://github.com/acme/widgets/pull/12#pullrequestreview-1",
                5,
                1
            ),
            "[sm review] Rajesh's review of \"Decision memo\" (PR #12 @ abcdef0) is here: https://github.com/acme/widgets/pull/12#pullrequestreview-1\nVerdict: changes requested · 5 line comments · 1 file comment"
        );
        assert!(
            render_owner_review_wake("Rajesh", &doc("M"), &review("approve"), "u", 0, 0)
                .ends_with("Verdict: approved · no comments")
        );
        assert!(
            render_owner_review_wake("Rajesh", &doc("M"), &review("comment"), "u", 1, 0)
                .ends_with("Verdict: comments · 1 line comment")
        );
    }

    #[test]
    fn wake_quotes_the_overall_text() {
        let mut approved = review("approve");
        approved.body = Some(
            "  Decisions look good.\n\nMerge after 2 Codex rounds. File only tickets that can start now.\n"
                .into(),
        );
        assert_eq!(
            render_owner_review_wake("Rajesh", &doc("M"), &approved, "u", 0, 0),
            "[sm review] Rajesh's review of \"M\" (PR #12 @ abcdef0) is here: u\nVerdict: approved · no line or file comments\nRajesh wrote:\n> Decisions look good.\n>\n> Merge after 2 Codex rounds. File only tickets that can start now."
        );

        let mut blank = review("approve");
        blank.body = Some(" \n ".into());
        assert!(
            render_owner_review_wake("Rajesh", &doc("M"), &blank, "u", 0, 0)
                .ends_with("Verdict: approved · no comments")
        );

        let mut long = review("comment");
        long.body = Some("é".repeat(WAKE_OVERALL_MAX_CHARS + 1));
        let wake = render_owner_review_wake("Rajesh", &doc("M"), &long, "u", 1, 0);
        assert!(wake.contains("Verdict: comments · 1 line comment\nRajesh wrote:\n> é"));
        assert!(wake.ends_with(&format!(
            "{}\n> … (truncated; read the rest at the link above)",
            "é".repeat(WAKE_OVERALL_MAX_CHARS)
        )));
    }

    #[test]
    fn bodies_carry_the_verdict_marker_and_quote() {
        assert_eq!(
            review_body(
                OwnerDocVerdict::ChangesRequested,
                Some("  Tighten §2. "),
                "sub-0001"
            ),
            "**Verdict: Changes requested**\n\nTighten §2.\n\n<!-- sm-review:sub-0001 -->"
        );
        assert_eq!(
            review_body(OwnerDocVerdict::Approve, Some(" "), "sub-0001"),
            "**Verdict: Approved**\n\n<!-- sm-review:sub-0001 -->"
        );
        let draft = OwnerDocDraft {
            id: "x".into(),
            doc_id: "d".into(),
            commit_sha: "c".into(),
            line: Some(3),
            quote: "first line\nsecond".into(),
            body: "Why?".into(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert_eq!(comment_body(&draft), "> first line\n> second\n\nWhy?");
    }

    #[test]
    fn submission_ids_are_bounded_tokens() {
        assert!(valid_submission_id("0b6f3c1e-8f1a-4c7e-9d2b-5a4e3f2c1b0a"));
        for bad in ["short", "has space here", "semi;colon;x", &"a".repeat(65)] {
            assert!(!valid_submission_id(bad), "{bad}");
        }
    }
}
