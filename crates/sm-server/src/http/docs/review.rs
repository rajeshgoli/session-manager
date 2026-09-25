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

/// How wake messages name the owner.
const OWNER_NAME: &str = "Rajesh";

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
    if counts.is_empty() {
        counts.push("no comments".to_owned());
    }
    format!(
        "[sm review] {OWNER_NAME}'s review of \"{}\" (PR #{} @ {}) is here: {review_url}\nVerdict: {verdict} · {}",
        doc.title,
        doc.pr_number.unwrap_or_default(),
        &review.commit_sha[..review.commit_sha.len().min(7)],
        counts.join(" · ")
    )
}

fn is_retired(session: &SessionRecord) -> bool {
    matches!(
        session.completion_status.as_deref(),
        Some("retired" | "killed")
    )
}

/// The author if it still exists (a stopped session gets the queued message
/// on restore), else the retired author's parent, else nobody.
pub(super) fn review_wake_recipient(state: &AppState, doc: &OwnerDoc) -> Option<String> {
    let author = state
        .session_store
        .get_session(&doc.author_session_id)
        .ok()
        .flatten()?;
    if !is_retired(&author) {
        return Some(author.id);
    }
    let parent = author.parent_session_id.as_deref()?;
    state
        .session_store
        .get_session(parent)
        .ok()
        .flatten()
        .filter(|parent| !is_retired(parent))
        .map(|parent| parent.id)
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

fn conflict(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::CONFLICT,
        detail: detail.into(),
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
            "failed" => Err(conflict(
                "This submission failed and its drafts were kept; submit again",
            )),
            _ => run_blocking(state, existing, None).await,
        };
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
    .map_err(|detail| ApiError::Status {
        status: StatusCode::BAD_GATEWAY,
        detail,
    })?;
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
    let github_error = |detail: String| ApiError::Status {
        status: StatusCode::BAD_GATEWAY,
        detail,
    };

    let pr = match fresh {
        Some(pr) => pr,
        None => {
            let on_github = source
                .viewer_reviews(&doc.repo, pr_number)
                .map_err(github_error)?;
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
            let pr = doc_pull_request(state, &doc.repo, pr_number, true).map_err(github_error)?;
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

    let pending_id = match source.add_pending_review(&pr.node_id, &review.commit_sha, &body) {
        Ok(id) => id,
        Err(error) => {
            store.fail_review(&review.id)?;
            return Err(github_error(format!("GitHub refused the review: {error}")));
        }
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
        // A null thread and an error both mean the line didn't take; the
        // comment then goes on the file, still quoting its selection.
        if let Some(line) = draft.line {
            if let Ok(true) = source.add_review_thread(pending_id, &doc.path, Some(line), &text) {
                line_comments += 1;
                continue;
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

/// A GitHub error after the pending review exists. If the review was in
/// fact submitted (the response was lost), finish from it; otherwise delete
/// the pending review so none is left under the owner's account, and mark
/// the row failed with the drafts kept.
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
    }
    owner_doc_store(state).fail_review(&review.id)?;
    Err(ApiError::Status {
        status: StatusCode::BAD_GATEWAY,
        detail: format!("GitHub refused the review: {error}"),
    })
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
            render_owner_review_wake(doc, review, review_url, line_comments, file_comments),
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
                &doc("Decision memo"),
                &review("changes_requested"),
                "https://github.com/acme/widgets/pull/12#pullrequestreview-1",
                5,
                1
            ),
            "[sm review] Rajesh's review of \"Decision memo\" (PR #12 @ abcdef0) is here: https://github.com/acme/widgets/pull/12#pullrequestreview-1\nVerdict: changes requested · 5 line comments · 1 file comment"
        );
        assert!(
            render_owner_review_wake(&doc("M"), &review("approve"), "u", 0, 0)
                .ends_with("Verdict: approved · no comments")
        );
        assert!(
            render_owner_review_wake(&doc("M"), &review("comment"), "u", 1, 0)
                .ends_with("Verdict: comments · 1 line comment")
        );
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
