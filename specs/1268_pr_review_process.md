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

- If there are any reachable critical/important feedback items (`P0`/`P1`), the
  PR is **not mergeable**.
- If there are no important feedback items (`P2` or lower only), exit criteria are **met**.
- If the review is clean, exit criteria are **met**.
- Meeting the severity gate does not waive the selected tier's minimum review
  count. In particular, an R3 PR is not mergeable until at least two recorded
  rounds have completed, even when its first round is clean.

Codex review is a sampled gate, not proof obtained by requesting reviews until
one happens to be clean. Assign the review tier before the first request and
record it in the PR body:

- **R0 - non-operative scratch:** no review cycle for temporary notes that do
  not authorize, gate, or direct production, migration, security, recovery, or
  other durable action. Any R3 characteristic takes precedence over a file's
  temporary location or intended lifetime.
- **R1 - low risk:** one broad round. A reachable `P0`/`P1` promotes the PR to R2;
  clean or P2-only results meet the gate.
- **R2 - medium risk:** at most three exact-head rounds. Use a broad review, one
  batched root-cause repair and sibling search, then a current-capability review
  only if a reachable `P0`/`P1` remains.
- **R3 - high risk:** persisted shapes, authority, recovery, concurrency,
  security, ambiguity semantics, hot paths, and public contracts. Run at least
  two rounds and no more than three. Round 3 is a hard stop for that PR: any
  finding that requires another code change triggers split, revert, redesign,
  or park rather than a fourth review.

Tier assignment uses the highest matching row; lower-risk labels never override
a higher-risk characteristic:

| Tier | Objective match |
|---|---|
| R0 | Non-operative scratch only; the artifact cannot authorize, gate, mutate, migrate, deploy, recover, or define durable behavior. |
| R1 | Local, reversible implementation or documentation with no persisted shape, public contract, authority, concurrency, recovery, security, or production-control effect. |
| R2 | Durable or user-visible behavior spanning a bounded component, provided no R3 characteristic applies. |
| R3 | Any persisted schema or lifecycle, authority/authentication, atomicity/concurrency, restart/recovery, security boundary, ambiguity that could authorize action, production hot path/control, migration, or public compatibility contract. |

The PR author records the selected tier and matched characteristics in the PR
body before requesting review. Until Session Manager enforces this field, the
maintainer treats a missing tier as blocked and does not request or accept the
review gate. Any reviewer or maintainer may raise an under-classified PR to the
highest matching tier; lowering a tier requires a recorded rationale showing
that the higher-risk characteristic is absent or removed from the current head.
Tier changes preserve prior rounds and must satisfy the new tier's minimum and
maximum from that point. The owner is not a code-classification gate.

Record the requested and reviewed head, scope/steer, findings, disposition,
resulting head, review wait, and available token estimate for every round. Do
not request an unchanged head again except for R3's mandatory second round: a
differently scoped confirmation may review the same immutable head once when
round 1 produced no change.

## After Feedback

1. Fix any feedback you choose to address.
2. Commit the changes.

If exit criteria are not met:

1. Push the fixes.
2. Request the next bounded round only when the selected tier permits it and
   the new head stays inside the frozen capability.

At an R2 third-round P0/P1 or an R3 design-return trigger:

1. A reachable `P0`/`P1` still blocks merge. Never merge a repair for the final
   blocking finding without review of that repaired head.
2. Classify the finding as localized, partitionable, or structural using its
   enabled-path reachability, blast radius, recurrence across rounds, touched
   files/interfaces, rollback quality, and hostile-test coverage.
3. Neither R2 nor R3 grows a fourth round. A localized repair that requires
   confirmation becomes a fresh, smaller package with an independently useful
   capability and its own three-round ceiling. Cosmetic repartition, an
   unchanged resubmission, or moving the same coupled diff to another PR does
   not reset the cap.
4. A partitionable finding produces a split proposal keyed by the findings'
   file/interface list. Land no partition until its own bounded review passes.
5. A structural or repeated finding enters design-return: a fresh design seat
   receives the ticket, current code, and accumulated findings; it must produce
   pieces that each have an independent gate. Execute those pieces as a
   subepic, rerun the original full gate at the subepic head, and perform one
   integration-focused review before landing.
6. A finding that only affects disabled or future capability becomes a linked
   issue and an activation criterion for that capability.
7. Fix, explicitly accept with rationale, or file every `P2`/`P3`.

At the three-round ceiling, the maintainer stops review activity and chooses
revert, split, redesign, or park from the recorded risk and cost evidence.
Risk-free slices may merge only when they are independently testable, useful,
and already satisfy their own bounded review gate; the unresolved slice does
not ride along. Do not ask the owner to read or adjudicate code findings. Owner
input is required only to accept a named residual risk, change product scope,
or override policy. If owner input is not available, the fail-closed default is
split/park, not an indefinite review loop and not a merge with unresolved
P0/P1 findings.

If exit criteria are met:

1. Merge the PR.
2. Delete the branch.
3. Delete the worktree if one was created for the branch.
4. Clean up local state and return to the appropriate base branch.

## Notes

- Prefer Session Manager ownership of the review loop when available. It handles retries, restart recovery, and "fresh review after current request" disambiguation better than ad-hoc shell polling.
- Do not treat “a review exists” as sufficient by itself.
- When using Session Manager wakeups, still verify that the landed review/comment is tied to the current request cycle before acting on it.
- The blocking threshold is whether unresolved feedback contains any reachable
  `P0`/`P1` items.
- `P2` or lower feedback can still be worth fixing before merge, but it does not block exit criteria.
- Keep one deployable capability per PR. A head-expanding refactor discovered
  during review belongs in a separate PR rather than silently increasing the
  current review surface.
- A review dominated by defects in verification infrastructure terminates the
  product-code loop and files an apparatus issue, unless the PR itself changes
  that apparatus.
- A new gate, probe, fixture, or monitor does not count as protection until a
  test or recorded trial demonstrates that it fires red.
- For unusually high-risk transactions, up to two focused reviews may run in
  parallel on the same immutable head and be aggregated as one round. Record
  their costs separately and deduplicate findings before one repair batch.
