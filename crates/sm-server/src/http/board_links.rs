//! Live links attached to each board ticket.

use super::*;
use crate::board::model::{Key, ModelInput};
use crate::owner_docs::{doc_readable_path, OwnerDocStore};
use crate::queue::{CodexReviewRequestFilters, QueueJobFilters, RetainedQueueStore};

pub(super) struct BoardLinks {
    prs: BTreeMap<Key, Vec<Value>>,
    jobs: BTreeMap<Key, Vec<Value>>,
    docs: BTreeMap<String, Vec<Value>>,
    threads: BTreeMap<String, Value>,
}

impl BoardLinks {
    pub fn load(state: &AppState, input: &ModelInput) -> anyhow::Result<Self> {
        let mut prs = BTreeMap::<Key, Vec<Value>>::new();
        let claim_store = claims::work_claim_store(state);
        let claims = claim_store.active_claims()?;
        let mut held = BTreeMap::<String, Vec<Key>>::new();
        for view in &claims {
            let claim = &view.claim;
            if claim.kind == "ticket" {
                let key = (claim.repo.clone(), claim.number);
                held.entry(claim.session_id.clone())
                    .or_default()
                    .push(key.clone());
            }
        }
        let all_claims = claim_store.board_link_claims()?;
        let mut tickets_by_session =
            BTreeMap::<String, Vec<&crate::work_claims::BoardLinkClaim>>::new();
        for claim in &all_claims {
            if claim.kind == "ticket"
                && input
                    .items
                    .contains_key(&(claim.repo.clone(), claim.number))
            {
                tickets_by_session
                    .entry(claim.session_id.clone())
                    .or_default()
                    .push(claim);
            }
        }
        for pr in &all_claims {
            if pr.kind != "pr" {
                continue;
            }
            for ticket in tickets_by_session.get(&pr.session_id).into_iter().flatten() {
                if ticket.repo != pr.repo
                    || pr.claimed_at < ticket.claimed_at
                    || ticket
                        .ended_at
                        .as_ref()
                        .is_some_and(|ended| pr.claimed_at > *ended)
                {
                    continue;
                }
                prs.entry((ticket.repo.clone(), ticket.number))
                    .or_default()
                    .push(json!({
                        "repo": pr.repo, "number": pr.number,
                        "state": pr.state.as_deref().unwrap_or("open").to_ascii_uppercase(),
                        "url": pr.url.as_deref().unwrap_or_default(),
                    }));
            }
        }

        let queue_path = expand_home(&state.config.queue_runner_state_dir().to_string_lossy())
            .join("queue_runner.db");
        let mut jobs = BTreeMap::<Key, Vec<Value>>::new();
        for job in
            RetainedQueueStore::list_queue_jobs_from_path(&queue_path, QueueJobFilters::default())?
        {
            if !matches!(job.state.as_str(), "pending" | "running") || job.job_type == "service" {
                continue;
            }
            let keys: Vec<Key> = job.rank_tickets.clone().unwrap_or_else(|| {
                job.requester_session_id
                    .as_ref()
                    .and_then(|id| held.get(id))
                    .cloned()
                    .unwrap_or_default()
            });
            let quiet = crate::utilization::quiet::status(&queue_path, &job);
            let since = if job.state == "running" {
                job.started_at.as_ref().unwrap_or(&job.queued_at)
            } else {
                &job.queued_at
            };
            for key in keys {
                jobs.entry(key).or_default().push(json!({
                    "id": job.id, "label": job.label,
                    "state": if job.state == "pending" { "waiting" } else { "running" },
                    "type": job.job_type, "since": since, "quiet_since": quiet.quiet_since,
                }));
            }
        }

        let doc_store = OwnerDocStore::new(expand_home(&state.config.sm_send.db_path));
        let mut docs = BTreeMap::<String, Vec<Value>>::new();
        let mut owner_reviews =
            BTreeMap::<(String, i64), (Vec<String>, Vec<(String, String)>)>::new();
        for summary in doc_store.summaries(None, false)? {
            let publishes = doc_store.publishes(&summary.doc.id)?;
            let Some(latest) = publishes.last() else {
                continue;
            };
            for publish in &publishes {
                if let Some(pr) = publish.pr_number.or(summary.doc.pr_number) {
                    if publish.review_requested {
                        owner_reviews
                            .entry((summary.doc.repo.clone(), pr))
                            .or_default()
                            .0
                            .push(publish.published_at.clone());
                    }
                }
            }
            if let Some(pr) = latest.pr_number.or(summary.doc.pr_number) {
                for review in doc_store.reviews(&summary.doc.id)? {
                    if review.status == "posted" {
                        owner_reviews
                            .entry((summary.doc.repo.clone(), pr))
                            .or_default()
                            .1
                            .push((
                                review.posted_at.unwrap_or(review.submitted_at),
                                review.verdict,
                            ));
                    }
                }
            }
            if summary.state.needs_owner() {
                docs.entry(latest.session_id.clone()).or_default().push(json!({
                    "title": summary.doc.title,
                    "reader_path": doc_readable_path(&summary.doc.repo, &summary.doc.path, &latest.commit_sha),
                    "state": summary.state.as_str(),
                }));
            }
        }
        for values in docs.values_mut() {
            values.truncate(3);
        }

        let messages = super::messages::owner_message_store(state);
        let replies = messages.all_replies()?;
        let replied: BTreeSet<String> = replies
            .iter()
            .map(|reply| reply.message_id.clone())
            .collect();
        let mut threads = BTreeMap::<String, Value>::new();
        let all_messages = messages.all()?;
        let sender_of: BTreeMap<String, String> = all_messages
            .iter()
            .map(|message| (message.id.clone(), message.sender_session_id.clone()))
            .collect();
        for message in all_messages {
            let thread = threads.entry(message.sender_session_id.clone()).or_insert_with(|| json!({
                "key": format!("agent:{}", message.sender_session_id), "needs_you": false, "count": 0,
            }));
            thread["count"] = json!(thread["count"].as_u64().unwrap_or(0) + 1);
            if message.blocking && message.handled_at.is_none() && !replied.contains(&message.id) {
                thread["needs_you"] = json!(true);
            }
        }
        for reply in replies {
            if let Some(session) = sender_of.get(&reply.message_id) {
                if let Some(thread) = threads.get_mut(session) {
                    thread["count"] = json!(thread["count"].as_u64().unwrap_or(0) + 1);
                }
            }
        }
        for note in super::inbox::inbox_store(state).notes()? {
            let thread = threads.entry(note.session_id.clone()).or_insert_with(|| {
                json!({
                    "key": format!("agent:{}", note.session_id), "needs_you": false, "count": 0,
                })
            });
            thread["count"] = json!(thread["count"].as_u64().unwrap_or(0) + 1);
        }

        // PR review state comes from sm's durable requests, not GitHub review counts.
        let review_path = expand_home(&state.config.codex_requests.db_path);
        let codex = RetainedQueueStore::list_codex_review_requests_from_path(
            &review_path,
            CodexReviewRequestFilters {
                include_inactive: true,
                ..Default::default()
            },
        )?;
        for (key, refs) in &input.prs {
            let values = prs.entry(key.clone()).or_default();
            for reference in refs {
                if !values.iter().any(|value| {
                    value["repo"] == reference.repo && value["number"] == reference.number
                }) {
                    values.push(json!(reference));
                }
            }
        }
        for values in prs.values_mut() {
            values.sort_by_key(|pr| pr["number"].as_i64().unwrap_or(0));
            values.dedup_by(|a, b| a["repo"] == b["repo"] && a["number"] == b["number"]);
            for pr in values {
                let is_open = pr["state"] == "OPEN";
                let repo = pr["repo"].as_str().unwrap_or_default();
                let number = pr["number"].as_i64().unwrap_or_default();
                let latest = codex
                    .iter()
                    .filter(|request| request.repo == repo && request.pr_number == number)
                    .max_by_key(|request| (request.is_active, &request.requested_at));
                let codex_review = latest.map(|request| json!({
                    "by": "codex", "round": request.round,
                    "verdict": if request.review_landed_at.is_some() { Some("comments") } else { None },
                    "verdict_at": request.review_landed_at,
                    "waiting_since": if is_open && request.is_active { Some(&request.requested_at) } else { None },
                })).unwrap_or(Value::Null);
                let owner_review = owner_reviews.get(&(repo.to_owned(), number)).map(|(requests, verdicts)| {
                    let last_request = requests.iter().max();
                    let last_verdict = verdicts.iter().max_by_key(|(at, _)| at);
                    let waiting = last_request.filter(|at| is_open && last_verdict.is_none_or(|(posted, _)| *at > posted));
                    json!({
                        "by": "you", "round": requests.len(),
                        "verdict": last_verdict.map(|(_, verdict)| match verdict.as_str() {
                            "approve" => "approved", "changes_requested" => "changes_requested", _ => "comments",
                        }),
                        "verdict_at": last_verdict.map(|(at, _)| at),
                        "waiting_since": waiting,
                    })
                }).unwrap_or(Value::Null);
                pr["review"] = choose_review(owner_review, codex_review);
            }
        }
        Ok(Self {
            prs,
            jobs,
            docs,
            threads,
        })
    }

    pub fn attach(&self, ticket: &mut Value) {
        let key = (
            ticket["repo"].as_str().unwrap_or_default().to_owned(),
            ticket["number"].as_i64().unwrap_or_default(),
        );
        if let Some(prs) = self.prs.get(&key) {
            ticket["prs"] = json!(prs);
        }
        ticket["jobs"] = json!(self.jobs.get(&key).cloned().unwrap_or_default());
        let session = ticket["holder"]["session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        ticket["thread"] = self.threads.get(&session).cloned().unwrap_or(Value::Null);
        ticket["docs"] = json!(self.docs.get(&session).cloned().unwrap_or_default());
    }
}

fn choose_review(owner: Value, codex: Value) -> Value {
    if owner.is_null() {
        return codex;
    }
    if codex.is_null() {
        return owner;
    }
    match (
        owner["waiting_since"].as_str(),
        codex["waiting_since"].as_str(),
    ) {
        (Some(_), None) => owner,
        (None, Some(_)) => codex,
        _ => {
            let at = |review: &Value| {
                review["waiting_since"]
                    .as_str()
                    .or_else(|| review["verdict_at"].as_str())
                    .unwrap_or_default()
                    .to_owned()
            };
            if at(&owner) >= at(&codex) {
                owner
            } else {
                codex
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_owner_review_wins_over_landed_codex_review() {
        let owner = json!({"by": "you", "waiting_since": "2026-09-30T11:00:00Z"});
        let codex =
            json!({"by": "codex", "waiting_since": null, "verdict_at": "2026-09-30T12:00:00Z"});
        assert_eq!(choose_review(owner, codex)["by"], "you");
    }
}
