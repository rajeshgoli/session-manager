# PR Review Process

Use this process whenever a pull request needs Codex review and merge handling.

## Review Loop

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
7. Repeat the cycle until a fresh review is posted.

## Review Triage

When Codex review lands:

1. Collect all review feedback.
2. Categorize each item by severity.
3. Decide whether you agree with each item.

## Exit Criteria

- If there are any important feedback items (`P1`), exit criteria are **not met**.
- If there are no important feedback items (`P2` or lower only), exit criteria are **met**.
- If the review is clean, exit criteria are **met**.

## After Feedback

1. Fix any feedback you choose to address.
2. Commit the changes.

If exit criteria are not met:

1. Push the fixes.
2. Repeat the review loop until exit criteria are met.

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
