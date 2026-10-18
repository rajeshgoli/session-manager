//! Agent replies to the owner's doc review (sm#2305). An agent that answers
//! a review comment on GitHub instead of in the doc leaves the answer where
//! the owner does not look. Once the agent the review woke is idle and has
//! published no new revision since the review, its replies reach the owner
//! as one owner message. A new revision, a closed PR, or a week ends the
//! watch: a revised doc carries its own answers.

use super::*;
use crate::owner_docs::OwnerDocReview;

/// How long after posting a review is watched for replies.
const REPLY_WATCH: time::Duration = time::Duration::days(7);
/// Longest reply quoted in full; past it the message points at the thread.
const REPLY_MAX_CHARS: usize = 3000;
/// Longest excerpt of the owner's own comment the message repeats.
const PARENT_MAX_CHARS: usize = 200;

/// One sync pass over watched reviews; a failure on one review is logged and
/// the rest still run.
pub(in crate::http) fn forward_review_replies(state: &AppState) -> anyhow::Result<()> {
    let store = owner_doc_store(state);
    let since = time::OffsetDateTime::now_utc() - REPLY_WATCH;
    let since = since
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    for review in store.reviews_watching_replies(&since)? {
        if let Err(error) = forward_one(state, &store, &review) {
            eprintln!("doc review replies for {} failed: {error:#}", review.id);
        }
    }
    Ok(())
}

fn forward_one(
    state: &AppState,
    store: &OwnerDocStore,
    review: &OwnerDocReview,
) -> anyhow::Result<()> {
    let (Some(doc), Some(github_review_id), Some(posted_at)) = (
        store.get(&review.doc_id)?,
        review.github_review_id,
        review.posted_at.as_deref(),
    ) else {
        return Ok(());
    };
    let Some(pr) = doc.pr_number else {
        return store.close_reply_watch(&review.id);
    };
    if store.revision_after_review(review)? {
        return store.close_reply_watch(&review.id);
    }
    let Some(session_id) = review.delivered_to_session_id.as_deref() else {
        return Ok(());
    };
    // A working agent may still publish; ask again once it stops.
    let agent = super::super::review_paired::live_session(state, session_id)?;
    if agent
        .as_ref()
        .is_some_and(|session| !super::super::review_paired::is_idle(state, session))
    {
        return Ok(());
    }
    if !state
        .owner_doc_source
        .pull_request(&doc.repo, pr)
        .map_err(anyhow::Error::msg)?
        .is_open()
    {
        return store.close_reply_watch(&review.id);
    }
    let forwarded = store.forwarded_reply_ids(&review.id)?;
    let replies: Vec<DocReviewReply> = state
        .owner_doc_source
        .review_replies(&doc.repo, pr, github_review_id, posted_at)
        .map_err(anyhow::Error::msg)?
        .into_iter()
        .filter(|reply| !forwarded.contains(&reply.id))
        .collect();
    if replies.is_empty() {
        return Ok(());
    }
    let sender_name = agent
        .clone()
        .map(session_display_name)
        .unwrap_or_else(|| session_id.to_owned());
    let ids: Vec<i64> = replies.iter().map(|reply| reply.id).collect();
    let key = format!(
        "doc-review-replies:{}:{}",
        review.id,
        ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
    );
    let result =
        crate::owner_messages::OwnerMessageStore::new(expand_home(&state.config.sm_send.db_path))
            .create_once(
            crate::owner_messages::NewOwnerMessage {
                human: state.config.owner_name.clone(),
                sender_session_id: session_id.to_owned(),
                sender_session_name: sender_name.clone(),
                title: format!("Replied on PR #{pr} without a new revision"),
                body_markdown: replies_message(&sender_name, &doc, pr, &replies),
                blocking: false,
            },
            Some(&key),
        )?;
    if let crate::owner_messages::CreateOwnerMessage::Created(message) = result {
        super::super::follows::notice_new_message(state, &message);
    }
    store.record_forwarded_replies(&review.id, &ids)
}

pub(super) fn replies_message(
    sender: &str,
    doc: &OwnerDoc,
    pr: i64,
    replies: &[DocReviewReply],
) -> String {
    let mut body = format!(
        "{sender} replied on PR #{pr} to your review of \"{}\" and has published no new revision since.",
        doc.title
    );
    for reply in replies {
        body.push_str("\n\n---\n\n**Your comment:** ");
        body.push_str(&truncate(
            &owner_comment(&reply.parent_body),
            PARENT_MAX_CHARS,
        ));
        body.push_str("\n\n");
        body.push_str(&truncate(reply.body.trim(), REPLY_MAX_CHARS));
        body.push_str(&format!("\n\n[Open the thread]({})", reply.url));
    }
    body
}

/// The owner's words without the quoted selection `comment_body` prepends.
fn owner_comment(body: &str) -> String {
    let text = body
        .lines()
        .skip_while(|line| line.starts_with('>'))
        .collect::<Vec<_>>()
        .join(" ");
    let text = text.trim();
    if text.is_empty() {
        body.trim().to_owned()
    } else {
        text.to_owned()
    }
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_replies_in_the_owner_review_threads_are_kept() {
        let parents = json!([
            {"id": 10, "body": "> the quote\n\nwhy rate 0.1?"},
            {"id": 11, "body": "> other\n\nand this"}
        ]);
        let comments = json!([
            {"id": 10, "in_reply_to_id": null, "body": "> the quote\n\nwhy rate 0.1?", "html_url": "u10"},
            {"id": 31, "in_reply_to_id": 11, "body": "answered in the doc", "html_url": "u31"},
            {"id": 30, "in_reply_to_id": 10, "body": "Rate 1 lags.", "html_url": "u30"},
            {"id": 40, "in_reply_to_id": 99, "body": "another review's thread", "html_url": "u40"}
        ]);
        let replies = super::super::review_replies_from(&parents, &comments);
        assert_eq!(
            replies.iter().map(|reply| reply.id).collect::<Vec<_>>(),
            vec![30, 31]
        );
        assert_eq!(replies[0].parent_body, "> the quote\n\nwhy rate 0.1?");
        assert_eq!(replies[0].url, "u30");
    }

    #[test]
    fn message_pairs_each_reply_with_the_owner_comment() {
        let doc = OwnerDoc {
            id: "d0c00001".into(),
            repo: "acme/widgets".into(),
            path: "docs/readout.html".into(),
            pr_number: Some(2201),
            author_session_id: "a4782b6e".into(),
            author_session_name: None,
            title: "Readout".into(),
            note: None,
            retracted_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let replies = [DocReviewReply {
            id: 30,
            parent_body: "> small cost, big gain\n> second line\n\nIs this right?".into(),
            body: "  You're right, I overstated it.  ".into(),
            url: "https://github.com/acme/widgets/pull/2201#discussion_r30".into(),
        }];
        assert_eq!(
            replies_message("far-2170-h3", &doc, 2201, &replies),
            "far-2170-h3 replied on PR #2201 to your review of \"Readout\" and has published no new revision since.\n\n---\n\n**Your comment:** Is this right?\n\nYou're right, I overstated it.\n\n[Open the thread](https://github.com/acme/widgets/pull/2201#discussion_r30)"
        );
        assert_eq!(owner_comment("> only a quote"), "> only a quote");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }
}
