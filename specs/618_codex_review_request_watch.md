# sm#618: durable Codex PR review request/watch workflow

## Scope

Add a first-class Session Manager workflow for requesting a Codex GitHub PR review and waking the requesting agent when the fresh review actually lands.

Primary UX:

```bash
sm request-codex-review 616
```

Immediate response:

```text
Review requested for PR #616, will sm send you when review arrives.
```

Terminal wake to the requesting session:

```text
[sm review] Codex review for PR #616 is here.
```

Optional follow-up commands:

```bash
sm request-codex-review status [<request-id>|--pr 616]
sm request-codex-review cancel <request-id>
sm request-codex-review list
```

## Workflow doc note

The operator request referenced `~/.agent-os/workflows/pr_review_workflow.md`, but the live file in this environment is:

```text
~/.agent-os/workflows/pr_review_process.md
```

This feature should automate that actual process doc:

1. post `@codex review`
2. wait 5 minutes
3. poll for review
4. wait 5 more minutes
5. if still no review, post another `@codex review`
6. repeat until a fresh review lands

## Problem

Today agents implement the review loop themselves with ad-hoc shell polling and local memory:

1. post `@codex review`
2. poll comments/reviews
3. guess whether Codex picked the request up
4. decide when to re-ping

That creates repeated failure modes:

1. the comment posts but Codex never picks it up
2. the agent polls at the wrong moment and decides nothing happened
3. the agent sees an older Codex review/comment and mistakes it for the current request
4. the agent forgets when to re-ping after 10 minutes
5. the agent is interrupted or compacted while waiting, so the review loop disappears

This is the wrong layer. Session Manager already owns durable asynchronous wakeups.

## Goals

1. Make Codex review requesting durable across SM restart and agent interruption.
2. Distinguish the current request from older PR comments/reviews.
3. Re-ping `@codex review` on the workflow cadence without agent involvement.
4. Wake the requesting session only when a fresh review/comment relevant to the request lands.
5. Expose enough state for `sm` and watch surfaces to answer “requested?”, “picked up?”, “review landed?”, and “when do we re-ping?”.

## Non-goals

1. Do not auto-triage or auto-merge the PR.
2. Do not parse review severity in v1.
3. Do not try to infer semantic correctness from arbitrary GitHub conversation.

## Official GitHub doc findings

Relevant GitHub documentation reviewed for this design:

1. `Creating webhooks`
   - https://docs.github.com/en/webhooks/using-webhooks/creating-webhooks
2. `Webhook events and payloads`
   - https://docs.github.com/en/webhooks/webhook-events-and-payloads
3. `Validating webhook deliveries`
   - https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries
4. `Handling failed webhook deliveries`
   - https://docs.github.com/en/webhooks/using-webhooks/handling-failed-webhook-deliveries
5. `Delivering webhooks to private systems`
   - https://docs.github.com/en/enterprise-cloud@latest/webhooks/using-webhooks/delivering-webhooks-to-private-systems
6. `gh api` manual
   - https://cli.github.com/manual/gh_api

Key facts from those docs:

1. GitHub webhooks are configured per repository, organization, or GitHub App.
2. Creating a repository webhook requires repository admin access.
3. Webhook deliveries should be validated with `X-Hub-Signature-256` using an HMAC secret.
4. GitHub does not automatically redeliver failed webhook deliveries.
5. A delivery is recorded as failed if the receiver is down or takes longer than 10 seconds to respond.
6. GitHub documents `pull_request_review` and `issue_comment` webhook events for the review/comment signals we care about.
7. I did not find a first-class webhook event for comment reactions in the official webhook event list, so the eye-reaction "picked up" signal is not a clean webhook-driven primitive.

## Webhook-first vs polling: pros and cons

### Webhook-first pros

1. Best latency for the real terminal signal: SM can wake the agent as soon as GitHub sends the fresh review/comment event.
2. Avoids repeated polling while waiting for review arrival.
3. Reduces the "agent polled at the wrong moment" failure mode.
4. Cleaner source of truth for "review landed" than scraping periodic snapshots.

### Webhook-first cons

1. Requires public ingress or a reverse-proxy path from GitHub into SM.
2. Requires secure secret storage and HMAC verification.
3. Requires per-repo, per-org, or GitHub App webhook configuration; this is operationally heavier than local `gh` polling.
4. Failed deliveries are not automatically retried by GitHub, so SM still needs recovery/reconciliation logic.
5. The eye-reaction pickup signal likely still needs polling because reactions are not exposed as a straightforward webhook event in the reviewed docs.

### Polling pros

1. Works immediately anywhere local `gh` auth can access the PR.
2. No public inbound endpoint required.
3. No webhook secret/configuration overhead.
4. Easier to ship as a repo-local SM feature.

### Polling cons

1. Slower to notice review arrival.
2. More API churn.
3. More state-machine complexity around "old review vs fresh review" because the system is observing snapshots instead of events.

## Design recommendation

Ship v1 as a 30-second polling workflow over `gh`, not as a webhook-first system.

Why this is the right v1 choice:

1. it works immediately against any repo the local `gh` session can access
2. it does not require public ingress, reverse-proxy setup, or webhook secret management
3. it does not require repository admin or organization-owner access just to enable the feature
4. 30-second latency is acceptable for a human-in-the-loop PR review workflow
5. SM already has the durable poller and wakeup patterns needed to implement it cleanly

Why webhooks are not the recommended v1:

1. repository webhooks require repo admin access, which makes the feature non-portable across arbitrary repos
2. webhook delivery to this local/private SM deployment would require extra infrastructure, as GitHub documents via reverse-proxy guidance
3. GitHub does not automatically redeliver failed webhook deliveries, so SM would still need reconciliation logic
4. the eye-reaction "picked up" signal does not appear to have a clean first-class webhook event in the reviewed docs

Conclusion:

1. implement durable 30-second polling in v1
2. keep the request model and API shape compatible with a future webhook-assisted path
3. revisit webhook support later if operators want lower latency and are willing to carry the setup cost

## Existing building blocks

Already present in the repo:

1. `src/github_reviews.py`
   - `post_pr_review_comment(repo, pr_number, steer=None)`
   - `poll_for_codex_review(...)`
   - `fetch_latest_codex_review(...)`
   - `get_pr_repo_from_git(...)`
2. durable background polling patterns in `src/message_queue.py`
   - reminders
   - parent wakes
   - job watches (`#377`)
3. durable session-targeted message delivery via `queue_message(...)`

This feature should reuse those patterns, not add a standalone maintainer script.

## Proposed UX

### 1. Request a review

```bash
sm request-codex-review 616
sm request-codex-review 616 --repo rajeshgoli/session-manager
sm request-codex-review 616 --steer "focus on race conditions in restore path"
sm request-codex-review 616 --notify maintainer
```

Behavior:

1. Resolve repo from `--repo` or infer from the current git checkout.
2. Resolve notify target from `--notify` or default to the caller session.
3. Post `@codex review` immediately.
4. Persist a durable review-request registration.
5. Start background polling.
6. Return an immediate success message without blocking for the review.

### 2. Status/list/cancel

```bash
sm request-codex-review status --pr 616
sm request-codex-review list
sm request-codex-review cancel reviewreq_abc123
```

Agents need visibility into:

1. request id
2. repo / PR number
3. requester session
4. notify target
5. most recent `@codex review` comment id and timestamp
6. whether Codex has acknowledged pickup
7. whether a fresh review/comment landed
8. next scheduled re-ping time
9. active / completed / cancelled / errored state

## What counts as "picked up"

The operator specifically wants the eye reaction on the request comment treated as pickup.

v1 should track two distinct states:

1. `picked_up`
   - best-effort advisory state
   - detected from the Codex eye reaction on the latest request comment
2. `review_landed`
   - terminal signal
   - detected from a fresh Codex-authored PR review or PR comment after the current request timestamp

Important: `picked_up` is not required for correctness. It is only a better progress signal.

Why:

1. reactions can be flaky or absent
2. the actual success criterion is a fresh Codex review/comment after the request
3. the reviewed GitHub webhook docs did not show a first-class reaction event we can rely on for push delivery

## What counts as “review landed”

Only consider Codex activity that is newer than the current request attempt.

Accepted landing events:

1. a PR review by a Codex GitHub identity after `requested_at`
2. a PR issue comment by a Codex GitHub identity after `requested_at`

Matching rules:

1. author login contains `codex`
   - same heuristic already used by `src/github_reviews.py`
2. timestamp is strictly later than the most recent request comment for this registration

This avoids older Codex comments being mistaken for the current request.

## State model

Add a new durable registration, parallel to job watches.

Suggested dataclass:

```python
@dataclass
class CodexReviewRequestRegistration:
    id: str
    repo: str
    pr_number: int
    requester_session_id: str
    notify_session_id: str
    steer: Optional[str]
    requested_at: datetime
    latest_request_comment_id: Optional[int]
    latest_request_comment_url: Optional[str]
    latest_request_posted_at: Optional[datetime]
    pickup_detected_at: Optional[datetime]
    pickup_source: Optional[str]          # "reaction" for v1
    review_landed_at: Optional[datetime]
    review_source: Optional[str]          # "review" or "comment"
    review_comment_id: Optional[int]
    review_url: Optional[str]
    attempt_count: int
    next_retry_at: Optional[datetime]
    last_polled_at: Optional[datetime]
    last_error: Optional[str]
    state: str                            # active|completed|cancelled|errored
    is_active: bool = True
```

Persistence:

1. new SQLite table, similar to `job_watch_registrations`
2. startup recovery that restarts active review-request pollers

## Polling algorithm

Per active request:

1. immediately after posting:
   - save `attempt_count = 1`
   - save `latest_request_comment_id`
   - save `latest_request_posted_at`
   - set `next_retry_at = requested_at + 10 minutes`
2. every poll interval:
   - fetch the latest state of the request comment
   - if Codex eye reaction is present and `pickup_detected_at` is null:
     - set `pickup_detected_at = now`
   - fetch PR reviews newer than `latest_request_posted_at`
   - fetch PR issue comments newer than `latest_request_posted_at`
   - if a fresh Codex review/comment is found:
     - mark request `completed`
     - queue a wake message to `notify_session_id`
     - stop polling
3. if no landing event and `now >= next_retry_at`:
   - post another `@codex review`
   - increment `attempt_count`
   - replace `latest_request_comment_id`
   - replace `latest_request_posted_at`
   - clear `pickup_detected_at` for the new attempt
   - set `next_retry_at = now + 10 minutes`
4. if the notify target session disappears:
   - cancel the request automatically, same pattern as job watches

Suggested cadence:

1. poll every 30 seconds while active
2. no extra 5-minute “do nothing” timers are needed if re-ping is governed by `next_retry_at`
3. the user-facing semantics still match the workflow doc

## GitHub access details

Use `gh` CLI, not browser scraping.

Needed operations:

1. post `gh pr comment`
2. fetch the specific issue comment for reaction state
3. fetch PR reviews
4. fetch PR issue comments

Implementation note:

1. `src/github_reviews.py` should grow helper functions instead of embedding `gh` subprocess calls directly in the poller.
2. Keep them synchronous and wrap with `asyncio.to_thread()` from the server / message queue path, matching existing style.

Suggested additions:

1. `fetch_issue_comment(repo, comment_id) -> dict`
2. `fetch_pr_issue_comments(repo, pr_number, since: datetime) -> list[dict]`
3. `detect_codex_pickup(comment: dict) -> bool`
4. `find_fresh_codex_review_or_comment(...) -> Optional[dict]`

## Future webhook-assisted variant

If operators later want lower-latency review arrival detection and are willing to configure GitHub webhooks, SM can add a webhook-assisted path on top of the same durable request model.

Suggested endpoint:

```text
POST /api/github/webhooks
```

Requirements:

1. raw request body access for signature verification
2. `X-Hub-Signature-256` validation
3. fast acknowledgment path that returns within GitHub's delivery window
4. durable processing after acknowledgment
5. optional delivery-id tracking for dedupe/reconciliation

Webhook events to subscribe to:

1. `pull_request_review`
2. `issue_comment`

Rationale:

1. `pull_request_review` is the primary terminal signal for a real PR review
2. `issue_comment` covers the existing Codex comment-only success path

Open question:

1. whether the eye-reaction pickup signal is worth supporting at all in a webhook-first design, since review/comment arrival is the more important event and reaction push support is unclear from the docs reviewed here

## Notifications

Wake message should be concise and non-prescriptive:

Clean review:

```text
[sm review] Codex review for PR #616 is here.
```

With richer context:

```text
[sm review] Codex review for PR #616 is here: https://github.com/owner/repo/pull/616#pullrequestreview-...
```

If the terminal event is a PR comment rather than a review:

```text
[sm review] Codex comment for PR #616 is here.
```

Do not tell the agent what to do next. The waking signal should be factual.

## API and CLI

### CLI

New command group:

```bash
sm request-codex-review <pr-number> [--repo owner/repo] [--steer "..."] [--notify <session>]
sm request-codex-review list [--all] [--json]
sm request-codex-review status <request-id>|--pr <pr-number> [--json]
sm request-codex-review cancel <request-id>
```

### API

Suggested endpoints:

1. `POST /codex-review-requests`
2. `GET /codex-review-requests`
3. `GET /codex-review-requests/{id}`
4. `DELETE /codex-review-requests/{id}`

Request body:

```json
{
  "repo": "rajeshgoli/session-manager",
  "pr_number": 616,
  "requester_session_id": "9b134c6e",
  "notify_session_id": "9b134c6e",
  "steer": "focus on restart behavior"
}
```

## Failure handling

1. `gh` unavailable / auth failure
   - fail request creation immediately with a concrete error
2. posting comment fails on retry
   - keep request active
   - record `last_error`
   - try again on next poll
3. GitHub polling transiently fails
   - do not cancel the request
   - record `last_error`
   - continue polling
4. notify target missing
   - auto-cancel request
5. duplicate active request for same repo+PR+notify target
   - v1 should reject exact duplicates unless `--force-new` is explicitly added later

## Why not reuse generic job watches directly

The durable behavior is the same, but the state machine is richer than regex polling:

1. a command side effect (`gh pr comment`)
2. multiple external signals (reaction, reviews, comments)
3. retry scheduling
4. “current attempt” versus “older Codex activity” disambiguation

So this should reuse the job-watch architecture, not the exact job-watch schema.

## Acceptance criteria

1. `sm request-codex-review 616` posts `@codex review`, persists a registration, and returns immediately.
2. If the request comment gets an eye reaction, status surfaces show pickup.
3. If no fresh review/comment lands within 10 minutes, SM posts another `@codex review`.
4. Older Codex comments/reviews from before the current request do not satisfy the watch.
5. When a fresh Codex review/comment lands, SM sends a wake message to the notify target and marks the request completed.
6. Active requests survive Session Manager restart.
7. Agents can list/status/cancel active review requests.

## Suggested test coverage

1. creation posts the initial request comment and persists state
2. pickup detection via reaction updates `pickup_detected_at`
3. old Codex review before `latest_request_posted_at` is ignored
4. fresh Codex review completes the registration and queues a wake message
5. fresh Codex issue comment completes the registration and queues a wake message
6. no landing event by `next_retry_at` posts another `@codex review`
7. restart recovery resumes polling active requests
8. notify target missing auto-cancels the registration

## Classification

Single feature ticket.
