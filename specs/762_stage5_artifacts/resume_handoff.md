# Rust Port Resume Handoff

Status: handoff snapshot from 2026-06-12 after PR #898 and the clean real-state MVP rehearsal.

Use this file to resume the Rust cutover track without reconstructing state from
chat history. Binding scope still lives in [cutover_scope.md](cutover_scope.md),
release gates in [gate_matrix.md](gate_matrix.md), and executable contract
coverage in
[`scripts/rust_migration/contracts_manifest.json`](../../scripts/rust_migration/contracts_manifest.json).

## Current Repository State

- Branch: `main`
- Latest merged commit: `0bc8f5e` (`Merge pull request #898`)
- Open PRs at handoff: none
- Dirty worktree at handoff: only pre-existing untracked `.claude/settings.local.json`
- Stale session-manager review agents from the overnight run were retired.
- Unrelated non-session-manager agents were left alone.

## Completed Work

### Spec And Scope

- The Rust migration spec is converged and merged.
- Stage 5 artifacts define retained scope, removed surfaces, state ownership,
  rollout gates, workstreams, and cutover behavior.
- Owner-approved cutover decisions are incorporated:
  - keep useful stable core and native mobile app;
  - keep email/human fallback and inbound email;
  - remove Telegram, `sm dispatch`, `sm what`, standalone reminders,
    watch-job, policy/CI queue helpers, Termux attach, and public unauthenticated
    operational browser data;
  - require public-edge proof before public operational traffic reaches origin.

### Rust Implementation Track

Merged Rust slices cover:

- contract harness, fixture assertions, baseline runner, shadow mode, and MVP
  rehearsal tooling;
- core health/auth/bootstrap/session list/detail/output/events/SSE reads;
- core runtime slices for session/tmux/spawn/session graph/message queue/task
  complete/input batch/subagents;
- nodes list;
- queue jobs list/detail;
- Codex review requests list/detail;
- tool-call projections for Claude and Codex fork;
- Codex events, pending requests, activity actions;
- mobile bootstrap/session support, attach-ticket proof, terminal WebSocket
  auth/bridge, runtime disable, device revoke/list CLI support;
- public-edge assertion gate and request-target binding;
- email/human fallback and inbound email validation;
- retired-surface fixtures for removed routes and commands.

### Documentation And Manifest

- [mvp_progress.md](mvp_progress.md) records the PR lineage through #898.
- PR #880 added fixture-gated manifest checks for already-implemented detail
  endpoints:
  - `GET /queue-jobs/{queue_job_id}`
  - `GET /codex-review-requests/{codex_review_request_id}`
- PR #882 added validation-only manifest coverage for device-token and
  tmux-client hook error paths.
- PR #884 corrected the retained client-session fixture expectation for
  classic attach support.
- PR #886 added a synthetic Codex app read fixture.
- PR #888 added a synthetic app artifact metadata fixture.
- PR #890 added a synthetic queue-runner job fixture.
- PR #892 added the disposable mutating fixture workspace.
- PR #894 made the MVP rehearsal run both read-only and mutating Rust-core
  contracts in isolated sidecars.
- PR #896 updated this handoff after the first real-state rehearsal exposed a
  single `/events/state` shadow body mismatch.
- PR #898 reclassified `/events/state` shadow comparison as status-only because
  Python carries live tmux-client event state that a fresh Rust sidecar does not
  own.
- Current contract manifest size:
  - `115` checks total
  - `68` `python_and_rust`
  - `47` `rust_only`

## Validation At Handoff

The latest real-state MVP rehearsal is:

```bash
.local/rust-mvp-rehearsals/20260612T013237Z-events-state-shadow/mvp-rehearsal-report.json
```

Observed results from that run:

- overall status: passed with zero blockers;
- Python health, fresh Rust sidecar start/health, and isolated runtime smoke
  passed;
- Rust read-only sidecar contracts: `17` passed, `0` failed, `0` skipped;
- Rust mutating fixture contracts: `30` passed, `0` failed, `0` skipped;
- gap probes: `0` failed;
- Python and Rust baseline measurements completed;
- shadow read summary: `8` passed, `0` failed.

The previous blocker was `GET /events/state` shadow comparison. Python and Rust
both returned status `200`, but the predicted Rust body hash did not match
Python because Python carries live tmux-client event state. PR #898 made that
shadow prediction status-only. The clean run now passes `/health`,
`/health/detailed`, `/auth/session`, `/client/bootstrap`, `/sessions`,
`/client/sessions`, `/nodes`, and `/events/state`.

The same run measured the current Python process at `151.516 MiB` RSS and
`64.3 MiB` physical footprint, while the Rust sidecar measured `17.422 MiB`
RSS and `6.781 MiB` physical footprint.

## Useful Local Fixtures Found

These were valid at handoff and are useful for continuing fixture-backed
contract coverage:

| Fixture | Value | Source |
| --- | --- | --- |
| `queue_job_id` | `job_5a66488c1b6b` | `~/.local/share/claude-sessions/queue-runner/queue_runner.db` |
| `codex_review_request_id` | `255618bcc483` | `~/.local/share/claude-sessions/message_queue.db` |

Historical missing or not-found local fixtures from the earlier #880 handoff:

- no active `provider=codex-app` sessions in `GET /sessions`;
- no app artifact `meta.json` found in the checked default local artifact paths.

Those gaps are now covered by synthetic read-only fixtures from PR #886 and
PR #888 for Rust-side fixture execution. Real live-state fixtures are still
needed for final cutover evidence.

## What Remains

### Near-Term Work

1. Enable Python-authoritative shadow mode for a real observation window and
   triage unexplained mismatches.
2. Run the full retained manifest against Python and Rust with the current
   synthetic fixture set plus any live fixtures needed for mobile/device flows.
3. Audit retained CLI commands against [cutover_scope.md](cutover_scope.md).
   - Retained commands should be native Rust or intentionally routed.
   - Removed commands should be absent or explicitly retired.
4. Implement final state ownership and migration tooling.
   - Freeze or journal write admission before final backup.
   - Prove rollback restores or accounts for every accepted write after the
     restore point.
5. Complete public-edge deployment integration.
   - Edge signer/proxy, device enrollment/list/remove, revoked-device denial,
     and node fallback proof.
6. Finish retained node-control and narrow queue writer fixtures.
   - Include audit, policy, recovery, and rollback semantics.
7. Exercise service packaging and cutover.
   - launchd wrapper, non-destructive port ownership, health checks, rollback.
8. Run final native mobile smoke checks.
   - bootstrap, session list/detail, attach, request-status, analytics, bug
     reports, app artifacts.

### Cutover Stop Conditions

Do not start Rust writer ownership or MVP cutover until:

- retained manifest checks pass for Python and Rust on the selected fixture set;
- shadow mode shows no unexplained retained-core mismatches for the agreed
  observation window;
- final backup happens after write admission is frozen or journaled;
- public traffic has proof-of-possession before origin plus origin
  auth/capability checks;
- mobile attach, request-status, bug reports, and app artifacts pass smoke
  checks;
- rollback restores or explicitly journals every accepted write after the
  restore point.

## Recommended Resume Point

Start with the shadow observation gate:

1. Turn on Python-authoritative Rust shadow mode for retained reads against a
   local Rust sidecar.
2. Let it observe real traffic for the agreed window.
3. Summarize the shadow ledger by route, comparison class, and unexplained
   mismatch.
4. Convert any unexplained retained-surface mismatch into a bounded Rust slice.

This is the highest-signal next step because the latest rehearsal passed both
the read-only sidecar contracts and the mutating Rust-core fixture contracts;
the next risk is sustained live-traffic drift, not single-run rehearsal drift.
