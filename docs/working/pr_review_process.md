# PR Review Process

Use this process whenever a pull request needs Codex review and merge handling.

## Requesting A Review

Preferred path:

1. Run `sm request-codex-review <pr-number>`.
2. Treat the command response as registration only. Keep working on the PR or go idle.
3. Wait for Session Manager to wake you with a factual message that the review/comment has landed.
4. When you receive the wake, inspect only Codex activity that was posted after the current request.

Fallback path:

1. If `sm request-codex-review` is unavailable in the current Session Manager deployment, post `@codex review` as a PR comment.
2. Wait about 5 minutes.
3. Poll the PR for a Codex review.
4. If no review was posted, wait 5 more minutes.
5. Poll again.
6. If there is still no review after 10 minutes total, post another `@codex review` comment.
7. Repeat the request-registration cycle until a fresh review is posted. These
   retries obtain one review and do not count as additional review rounds.

## Review Triage

When Codex review lands:

1. Collect all review feedback.
2. Categorize each item by severity.
3. Decide whether you agree with each item.

## Exit Criteria

- If there are any important feedback items (`P1`), the PR is **not mergeable**.
- If there are no important feedback items (`P2` or lower only), exit criteria are **met**.
- If the review is clean, exit criteria are **met**.

Codex review is a sampled gate, not proof obtained by requesting reviews until
one happens to be clean. A PR receives at most three exact-head review rounds:

1. broad current-capability review;
2. root-cause verification after one batched repair, including sibling instances
   in the touched subsystem; and
3. a final enabled-path/rollback/test review only when round 2 retains or
   introduces a reachable `P0`/`P1`.

Record the requested and reviewed head, scope/steer, findings, disposition,
resulting head, review wait, and available token estimate for every round. Do
not request an unchanged head again.

## After Feedback

1. Fix any feedback you choose to address.
2. Commit the changes.

If exit criteria are not met:

1. Push the fixes.
2. Request the next bounded round only if fewer than three rounds have run.

At the three-round cap:

1. A reachable `P0`/`P1` blocks merge. Simplify, revert, split, or mark the PR
   blocked with the residual finding and evidence.
2. A finding that only affects disabled or future capability becomes a linked
   issue and acceptance criterion for that capability.
3. Fix, explicitly accept with rationale, or file each `P2`/`P3`.
4. Do not start a fourth round automatically. The owner may approve one bounded
   extension with an explicit token/time budget. It cannot be extended again;
   another reachable `P0`/`P1` requires a split or redesign.

If exit criteria are met:

1. Merge the PR.
2. Delete the branch.
3. Delete the worktree if one was created for the branch.
4. Clean up local state and return to the appropriate base branch.

## Notes

- Prefer Session Manager ownership of the review loop when available. It handles retries, restart recovery, and "fresh review after current request" disambiguation better than ad-hoc shell polling.
- Do not treat “a review exists” as sufficient by itself.
- When using Session Manager wakeups, still verify that the landed review/comment is tied to the current request cycle before acting on it.
- The blocking threshold is whether unresolved feedback contains any `P1` items.
- `P2` or lower feedback can still be worth fixing before merge, but it does not block exit criteria.
- Keep one deployable capability per PR. A head-expanding refactor discovered
  during review belongs in a separate PR rather than silently increasing the
  current review surface.
- For unusually high-risk transactions, up to two focused reviews may run in
  parallel on the same immutable head and be aggregated as one round. Record
  their costs separately and deduplicate findings before one repair batch.
