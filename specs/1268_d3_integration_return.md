# 1268 D3 atomic admission integration return

Status: implementation complete; R3 review pending.

## Bounded scope and ancestry

This branch implements the first bootstrap-incarnation spawn-policy canary only.
It does not generalize stable seats, rotation, compound message caps, two-step
briefs, dashboards, or issue #1291 operational actions, and it does not deploy
or run the production canary.

Current `origin/epic/1268-lane-policy` was merged first as `3cb46db`. The retained
D1A code changes were then integrated as individual commits, without claiming
that D1A or PR #1297 merged:

| Retained D1A code | D3 integration commit | Conflict disposition |
|---|---|---|
| `220200f` | `479091c` | Kept the epic's D0 `policy_runtime_attestation` module export while adding the frozen policy store. |
| `8705533` | `43b43b1` | Selected the reviewed D1A frozen-schema `policy_store.rs` resolution over the stale overlapping scaffold. |
| `c40c49d` | `3633c9f` | Applied cleanly on the resolved store. |

The D1A worktree and PR #1297 were not modified, deleted, reopened, or merged.
The merge-only D1A base/docs commit `beaf4e2` was not replayed; the D3 branch
used the newer epic head directly.

## Assembled integration matrix

| Boundary / hostile case | Implemented behavior | Verification |
|---|---|---|
| Retry after active, committed, released, or expired lease | Reuses the one terminal decision and original child binding; creates no decision, child, lease, claim, or capacity duplicate. Terminal retries do not materialize a replacement child. | `ordinary_retries_reuse_terminal_decision_in_every_lease_state` |
| Child launch promotion | Requires exactly one exact reserved/active child-lease binding. Missing, released, expired, already-launched, release-pending, and conflicting pairs fail closed. | `launch_promotion_requires_exactly_one_reserved_active_binding` |
| Equal-rank policy clauses | Conflict identity includes outcome, resolved profile, capacity claims, `overridable`, lease TTL, and all other enforcement-affecting fields. | `equal_rank_conflicts_include_all_enforcement_fields` |
| Requested exception profile | A fully specified frozen request receives that exact requested provider/model/effort/vehicle under override; an omission-caused rewrite receives only the deterministic rewrite target. | `omission_override_uses_rewrite_target_while_full_override_uses_request` |
| Override authority | The server loads request, caller, decision, policy-version, and digest bindings and constructs the override row. Cross-request/cross-caller or caller-built authority is rejected. | `server_constructed_override_rejects_cross_caller_and_cross_request`, `frozen_schema_override_binding_and_capacity_claims_fail_closed_when_tampered` |
| Omitted-tier fixtures | `aa6c1120` and `2260296e` remain deterministically blocked/rewritten to their explicit profiles; inherited defaults do not pass. | `omitted_models_for_aa6c1120_and_2260296e_resolve_to_explicit_profiles`, D0 omitted-tier tests |
| Request ordering | Authenticated bootstrap caller and requested launch fields are frozen and persisted before deterministic classification. A preallocated collision-checked child ID is passed into one immediate admission transaction. | Focused policy-store suite plus HTTP integration path inspection |
| Allow/materialize boundary | No session is created on rewrite/block. Allowed children materialize under their reserved ID without an active parent edge, so they are not routable before attestation. | Crash matrix `after-admission`, `after-session-create`, and hierarchy assertions |
| Provider attestation and commit | Codex-fork schema-v2 provider events attest the actual model/effort and thread binding. Only then does strict `mark_child_launched` commit capacity and publish the exact hierarchy edge. | Runtime-attestation suite (7 tests), crash matrix `after-attestation` |
| Decision, usage, lifecycle evidence | D2 envelopes are appended at admission, materialization, provider attestation, launch, rejection, release, and restart-reconciliation boundaries. Usage is an exact zero delta at the attestation boundary rather than an invented model estimate. | D2 evidence suite (9 tests), crash-matrix event assertions |
| Launch/attestation failure | Persists release intent, stops the provisional runtime, marks and retains the audit row as `launch_rejected`/stopped, removes hierarchy/role/alias/wake/reminder ownership, releases the exact lease, and surfaces every cleanup or release failure. | Crash matrix `after-session-create`, durable cleanup implementation |
| Restart: reserved/no session | Releases the reservation and capacity. | Crash matrix `after-admission` |
| Restart: reserved/session pre-mark | Re-attests and promotes, or rejects/stops/detaches/releases. | Crash matrix `after-session-create`, `after-attestation` |
| Restart: launched/live and launched/dead | Restores the exact intended hierarchy for a live committed child; releases a dead or missing child. Committed leases never TTL-expire. | Crash matrix `after-mark`; terminal retry test's committed case |
| Restart: release-pending / post-release | Completes cleanup and release idempotently; terminal audit rows stay outside the capacity denominator. | Crash matrix `during-release`, `after-release` |
| Restart ambiguity | Reports exact expected/visited/resolved/blocked counts and blocker text; admission remains disabled for the lane. | `ambiguous_policy_restart_state_reports_exact_blocker_and_disables_admission` |

## Enabled APIs and observable state

- `PolicyStore::prepare_admission` reserves capacity and the provisional child
  atomically; `mark_child_launched`, `mark_child_release_pending`, and
  `release_by_child` implement the durable lifecycle.
- `PolicyStore::admission_for_child`, `resolve_child`, and
  `reconciliation_snapshot` expose the D3 recovery view while validating all
  persisted cross-record authority.
- `SessionStore::promote_policy_provisional_session` publishes only the exact
  attested parent edge; `reject_policy_provisional_session` retains a stopped
  audit row while removing active ownership.
- The existing spawn endpoint now returns a policy-governed child only after
  allow, provider attestation, lease commit, and hierarchy publication.
- The existing policy status response adds `admission_enabled`, the exact
  `admission_blocker`, and `restart_reconciliation` counts.
- Existing D0 attestation contracts and D2 evidence/explain/status APIs remain
  the source of runtime proof and inspection; no parallel public contract was
  introduced.

## R3 matched characteristics

R3 applies because this package changes persisted authority and lifecycle
state, concurrent capacity admission, restart recovery and ambiguity handling,
and public spawn behavior. Review must cover at least these scopes:

1. persisted schema/authority/transaction invariants and hostile retry races;
2. public spawn sequencing, provider attestation, cleanup/release, D2 evidence,
   and restart convergence.

At least two and at most five Codex rounds are required. Even if round 1 is
clean, round 2 must use a differently scoped steer on the same review head. A
repeated or structural P0/P1 at or after round 3 returns this package to design
and parks it.

## Review ledger

| Round | Requested head | Reviewed head | Scope / steer | Findings and disposition | Resulting head | Wait | Reviewer usage |
|---|---|---|---|---|---|---|---|
| 1 | pending | pending | Persisted authority, transactionality, retry/concurrency, reconciliation ambiguity | pending | pending | pending | pending |
| 2 | pending | pending | Public spawn order, non-routable provisional runtime, attestation/evidence, cleanup/release, crash convergence | pending | pending | pending | pending |

## Verification ledger

| Gate | Result |
|---|---|
| Policy-store hostile suite | 19 passed |
| Restart crash/ambiguity matrix | 2 passed |
| Provider runtime attestation | 7 passed |
| D2 policy evidence | 9 passed |
| D0 policy contracts | 16 passed |
| `cargo fmt --check` | pending exact review head |
| `git diff --check` | clean during implementation; pending exact review head |
| `cargo test -p sm-server` | pending, to run once at exact review head |

Canonical owner/reviewer token usage and numeric budget variance are recorded
after the review rounds from `message_ledger`; they are not inferred from local
test output.
