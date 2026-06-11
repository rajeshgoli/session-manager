# Rust Port Resume Handoff

Status: handoff snapshot from 2026-06-11 after PR #880.

Use this file to resume the Rust cutover track without reconstructing state from
chat history. Binding scope still lives in [cutover_scope.md](cutover_scope.md),
release gates in [gate_matrix.md](gate_matrix.md), and executable contract
coverage in
[`scripts/rust_migration/contracts_manifest.json`](../../scripts/rust_migration/contracts_manifest.json).

## Current Repository State

- Branch: `main`
- Latest merged commit: `9854e0c` (`Merge pull request #880`)
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

- [mvp_progress.md](mvp_progress.md) records the PR lineage through #876.
- PR #880 added fixture-gated manifest checks for already-implemented detail
  endpoints:
  - `GET /queue-jobs/{queue_job_id}`
  - `GET /codex-review-requests/{codex_review_request_id}`
- Current contract manifest size:
  - `115` checks total
  - `68` `python_and_rust`
  - `47` `rust_only`

## Validation At Handoff

All commands below passed at handoff:

```bash
cargo test -p sm-server
./venv/bin/python -m pytest tests/unit/test_rust_migration_contracts.py
```

Observed results:

- Rust tests: `78` lib tests, `20` CLI tests, `142` read-only HTTP tests, and
  doctests passed.
- Rust migration contract tests: `34` passed.
- Latest MVP rehearsal passed with zero blockers:
  - `.local/rust-mvp-rehearsals/20260611T142302Z/mvp-rehearsal-report.json`

The two new detail checks passed against live Python and a temporary Rust
sidecar using real local fixture IDs:

```bash
./venv/bin/python -m scripts.rust_migration.contracts \
  --target python \
  --base-url http://127.0.0.1:8420 \
  --check-id http.queue_job_detail \
  --check-id http.codex_review_request_detail \
  --fixture queue_job_id=job_5a66488c1b6b \
  --fixture codex_review_request_id=255618bcc483 \
  --json

cargo run -p sm-server --bin sm-server -- --port 8421 --config config.yaml

./venv/bin/python -m scripts.rust_migration.contracts \
  --target rust \
  --base-url http://127.0.0.1:8421 \
  --check-id http.queue_job_detail \
  --check-id http.codex_review_request_detail \
  --fixture queue_job_id=job_5a66488c1b6b \
  --fixture codex_review_request_id=255618bcc483 \
  --json
```

## Useful Local Fixtures Found

These were valid at handoff and are useful for continuing fixture-backed
contract coverage:

| Fixture | Value | Source |
| --- | --- | --- |
| `queue_job_id` | `job_5a66488c1b6b` | `~/.local/share/claude-sessions/queue-runner/queue_runner.db` |
| `codex_review_request_id` | `255618bcc483` | `~/.local/share/claude-sessions/message_queue.db` |

Missing or not found at handoff:

- no active `provider=codex-app` sessions in `GET /sessions`;
- no app artifact `meta.json` found in the checked default local artifact paths.

## What Remains

### Near-Term Work

1. Build a complete fixture set for the retained manifest.
   - Need real or synthetic fixtures for `codex_app_session_id`, app artifact
     metadata, mobile/device flows, disposable mutating sessions, and any
     retained CLI fixture inputs.
2. Run the full contract manifest against Python and Rust with those fixtures.
3. Enable Python-authoritative shadow mode for a real observation window and
   triage unexplained mismatches.
4. Audit retained CLI commands against [cutover_scope.md](cutover_scope.md).
   - Retained commands should be native Rust or intentionally routed.
   - Removed commands should be absent or explicitly retired.
5. Implement final state ownership and migration tooling.
   - Freeze or journal write admission before final backup.
   - Prove rollback restores or accounts for every accepted write after the
     restore point.
6. Complete public-edge deployment integration.
   - Edge signer/proxy, device enrollment/list/remove, revoked-device denial,
     and node fallback proof.
7. Finish retained node-control and narrow queue writer fixtures.
   - Include audit, policy, recovery, and rollback semantics.
8. Exercise service packaging and cutover.
   - launchd wrapper, non-destructive port ownership, health checks, rollback.
9. Run final native mobile smoke checks.
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

Start with fixture-backed manifest execution:

1. Create or identify a disposable retained session fixture.
2. Create or identify a real `codex_app_session_id`.
3. Publish or synthesize an app artifact metadata fixture.
4. Run the manifest against Python and Rust with the fixture set.
5. Convert any failures into the next bounded Rust implementation slice.

This is the highest-signal next step because the MVP rehearsal currently has
zero blockers only for the core sidecar subset. The full manifest with fixtures
will expose the next real cutover gaps.
