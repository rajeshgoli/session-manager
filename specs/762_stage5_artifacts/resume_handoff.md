# Rust Port Resume Handoff

Status: handoff snapshot from 2026-06-12 after PR #938 and the full MVP rehearsal passed.

Use this file to resume the Rust cutover track without reconstructing state from
chat history. Binding scope still lives in [cutover_scope.md](cutover_scope.md),
release gates in [gate_matrix.md](gate_matrix.md), and executable contract
coverage in
[`scripts/rust_migration/contracts_manifest.json`](../../scripts/rust_migration/contracts_manifest.json).

## Current Repository State

- Branch: `main`
- Latest merged commit: `43eb722` (`Merge pull request #938`)
- Open PRs at handoff: none
- Dirty worktree at handoff: only pre-existing untracked `.claude/settings.local.json`
  and the local `config.yaml.shadow-backup-20260612T023248Z` created when
  enabling shadow mode.
- Stale session-manager review agents from the overnight run and PR #932 were retired.
- Unrelated non-session-manager agents were left alone.
- Live Python-authoritative Rust shadow mode is enabled in `config.yaml`, with
  Python serving on `:8420`, the Rust sidecar on `127.0.0.1:8421`, and the
  ledger at `~/.local/share/claude-sessions/rust_shadow.jsonl`.

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

- [mvp_progress.md](mvp_progress.md) records the PR lineage through #938.
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
- PR #900 recorded the clean #898-era handoff.
- PR #902 added `shadow_report` observation evidence with blocker handling for
  no-data, malformed status, and invalid row shapes.
- PR #904 added the non-mutating shadow workflow planner.
- PR #906 added the safe `rust_shadow` config activation helper and backup path.
- PR #908 added `--since` and `--last-minutes` filtering to `shadow_report`.
- PR #910 made Rust contract CLI checks default to `target/debug/sm`.
- PR #912 added `--skip-fixture-checks` so broad live-state contract runs can
  skip synthetic fixture checks without suppressing ordinary live coverage.
- PR #914 refreshed this handoff after live shadow activation.
- PR #916 added shadow report coverage gates.
- PR #918 wired shadow observation coverage gates into the planner.
- PR #920 added mobile device CLI contract checks.
- PR #922 added the Rust CLI cutover scope audit.
- PR #924 added state ownership preflight.
- PR #926 added the state backup plan/copy tool.
- PR #928 added the freeze/drain plan ledger scaffold.
- PR #930 added backup verification and restore rehearsal.
- PR #932 made the MVP rehearsal run state preflight, backup, restore
  verification, disposable restore, and freeze/drain evidence by default.
- PR #934 refreshed the handoff after the state-gated rehearsal.
- PR #936 made live session detail shadow prediction status-only while keeping
  fixture-backed deterministic session detail checks body/assertion based.
- PR #938 made the MVP rehearsal run synthetic read-only fixture contracts in a
  dedicated sidecar.
- Current contract manifest size:
  - `117` checks total
  - `68` `python_and_rust`
  - `49` `rust_only`

## Validation At Handoff

The latest real-state MVP rehearsal is:

```bash
.local/rust-mvp-rehearsals/20260612T-full-after-938/mvp-rehearsal-report.json
```

Observed results from that run:

- overall status: passed with zero blockers;
- Python health, explicit Rust sidecar reuse health, and isolated runtime smoke
  passed;
- state ownership gate passed:
  - `17` stores checked;
  - `13` existing stores copied into the backup;
  - `13` backup entries verified;
  - `13` backup entries restored into the disposable restore root;
  - freeze/drain ledger plan written;
- Rust live core sidecar contracts: `17` passed, `0` failed, `0` skipped;
- Rust synthetic read-only fixture contracts: `10` passed, `0` failed, `0`
  skipped;
- Rust mutating fixture contracts: `30` passed, `0` failed, `0` skipped;
- gap probes: `0` failed;
- Python and Rust baseline measurements completed;
- shadow read summary: `8` passed, `0` failed.

The earlier blocker was `GET /events/state` shadow comparison. Python and Rust
both returned status `200`, but the predicted Rust body hash did not match
Python because Python carries live tmux-client event state. PR #898 made that
shadow prediction status-only. PR #936 also made live session detail
status-only for shadow to avoid lifecycle TOCTOU noise. The clean run now
passes `/health`, `/health/detailed`, `/auth/session`, `/client/bootstrap`,
`/sessions`, `/client/sessions`, `/nodes`, and `/events/state`.

The same run measured the current Python process at `154.672 MiB` RSS and
`66.4 MiB` physical footprint, while the Rust sidecar measured `19.797 MiB`
RSS and `6.688 MiB` physical footprint.

## Live Observation

Live shadow mode is currently active. The most recent clean short-window report
used:

```bash
./venv/bin/python -m scripts.rust_migration.shadow_report \
  --ledger ~/.local/share/claude-sessions/rust_shadow.jsonl \
  --last-minutes 1 \
  --fail-on-blockers
```

Observed at `2026-06-12T02:59:18Z`:

- status: passed;
- rows: `86`;
- blockers: `0`;
- routes observed: `GET /events/state` (`28` status matches) and
  `GET /sessions` (`58` status matches).

The broad live Rust contract run now uses the fixture filter:

```bash
./venv/bin/python -m scripts.rust_migration.contracts \
  --target rust \
  --base-url http://127.0.0.1:8421 \
  --session-id 007c6275 \
  --skip-fixture-checks \
  --json
```

Fixture-filtered live Rust contract result:

- `71` passed;
- `3` skipped because they are mutating checks without `--include-mutating`;
- `0` failed.

Transient observation note: while retiring the PR #912 review session, one
`GET /sessions/765b4805` body mismatch was recorded and then the same route
matched on the next observation. Treat this as session-state drift unless it
recurs in a later sustained window.

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

1. Continue Python-authoritative shadow mode for a longer real observation
   window and triage unexplained mismatches.
2. Continue expanding fixture/live evidence as new retained rows land.
   - The MVP rehearsal already runs the current synthetic read-only and
     mutating fixture sets.
   - Final live mobile/device fixture evidence is still needed.
3. Audit retained CLI commands against [cutover_scope.md](cutover_scope.md).
   - Retained commands should be native Rust or intentionally routed.
   - Removed commands should be absent or explicitly retired.
4. Complete final state ownership and migration tooling.
   - Initial preflight, backup, restore, and freeze/drain evidence tools are
     merged and exercised by the MVP rehearsal.
   - Remaining work is the live write-admission freeze/journal gate before
     final backup.
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

Continue with the shadow observation gate:

1. Keep Python-authoritative Rust shadow mode running for retained reads against
   the local Rust sidecar.
2. Let it observe real traffic for the agreed window.
3. Summarize the shadow ledger by route, comparison class, and unexplained
   mismatch.
4. Convert any unexplained retained-surface mismatch into a bounded Rust slice.

This is the highest-signal next step because the latest rehearsal passed both
the read-only sidecar contracts and the mutating Rust-core fixture contracts;
the next risk is sustained live-traffic drift, not single-run rehearsal drift.
