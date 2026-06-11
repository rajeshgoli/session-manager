# Rust MVP Progress Snapshot

Status: implementation snapshot after PR #876 merged on 2026-06-11.

This file is a handoff aid for the Rust cutover implementation track. It does
not change retained or removed scope. Binding scope remains
[cutover_scope.md](cutover_scope.md), release gates remain
[gate_matrix.md](gate_matrix.md), and executable surface coverage remains
[`scripts/rust_migration/contracts_manifest.json`](../../scripts/rust_migration/contracts_manifest.json).

## Current Merge Point

`main` includes the Rust MVP implementation line through:

| PR | Slice |
| --- | --- |
| #773 | read-only session list scaffold |
| #776-#782 | contract fixture expansion and minimal value baseline |
| #784-#788 | read-only session contracts, watch/SSE contracts, and shadow mode |
| #790 | shadow secret redaction |
| #792-#818 | core session/tmux/spawn/session-graph/message-queue/task-complete/input-batch/subagent slices |
| #822-#824 | Codex-fork runtime and control slices |
| #826 | MVP sidecar rehearsal harness |
| #828-#834 | nodes read API, analytics summary, Codex review request list, queue jobs list |
| #836 | live rehearsal and shadow integration |
| #838-#840 | mobile support and email/human fallback |
| #842-#856 | mobile attach tickets, terminal WebSocket auth/bridge, disable/revoke/device CLI, public-edge assertion gate |
| #858-#862 | queue job detail and public-edge request-target follow-up |
| #864-#876 | Codex review detail, tool-call projections, Codex events, pending requests, activity actions, and manifest coverage |

## Implemented Capability Groups

The Rust sidecar now has executable coverage for:

| Group | Current state |
| --- | --- |
| Harness and baselines | Contract manifest, fixture assertions, minimal value baseline runner, shadow comparison, and MVP rehearsal exist. |
| Retired-surface checks | Retired public/watch/summary/remind/job-watch/queue-policy checks are represented as Rust-target absence or denial fixtures. |
| Core reads | Health, auth session, bootstrap, session list/detail, client session list/detail, output, events state, SSE hello, nodes list, queue jobs list/detail, Codex review requests list/detail, and tool/audit read projections are implemented. |
| Core runtime | Session/tmux/spawn/session-graph/message-queue/task-complete/input-batch/subagent slices are merged, with shadow and contract fixtures covering the early cutover path. |
| Codex retained reads | Codex event stream, pending request ledger reads, activity actions, review detail/list, and Claude/Codex tool-call projections are implemented and covered by manifest checks. |
| Mobile | Native bootstrap/session support, attach-ticket proofing, terminal WebSocket auth/bridge, runtime disable, device revoke/list CLI support, and public-edge assertion validation are merged. |
| External fallback | Email/human fallback delivery and inbound email validation path are retained in the Rust track. |
| Queue and nodes | Narrow queue list/detail and registered-node read paths are implemented; retained node and queue writer behavior still needs final cutover verification. |

## Near-Term Remaining Work

These are the next practical buckets before an MVP cutover trial:

| Bucket | Why it remains |
| --- | --- |
| Real-state MVP rehearsal | Run `scripts.rust_migration.mvp_rehearsal` against current local state, record blockers, and keep shrinking the retained gap list. |
| Shadow observation window | Enable Python-authoritative shadow mode for retained reads and triage unexplained mismatches before Rust becomes the writer. |
| CLI cutover audit | Verify every retained CLI command in [cutover_scope.md](cutover_scope.md) is native Rust or intentionally routed, and every removed command is absent or explicitly retired. |
| State ownership and migration tooling | Implement final freeze/drain/backup/restore ledger behavior from [state_ownership_and_migration.md](state_ownership_and_migration.md). |
| Public-edge deployment integration | Pair the Rust origin gate with the actual edge signer/proxy/device enrollment flow, including revoked-device and node fallback tests. |
| Node and queue writer completion | Finish retained node-control and narrow queue writer fixtures, including audit, policy, recovery, and rollback semantics. |
| Service packaging and rollback | Exercise launchd/service cutover, non-destructive port ownership, health checks, rollback, and operator diagnostics. |
| Final native mobile smoke | Run bootstrap/session/attach/request-status/analytics/bug-report/app-artifact smoke checks against Rust using real mobile assumptions. |

## Stop Conditions For MVP Cutover

Do not start an MVP cutover until:

- retained manifest checks pass for Python and Rust on the selected fixture set;
- shadow mode shows no unexplained mismatches on retained core reads for the agreed observation window;
- final backup happens after write admission is frozen or journaled;
- public traffic has proof-of-possession before origin plus origin auth/capability checks;
- mobile attach, request-status, bug reports, and app artifacts pass smoke checks;
- rollback restores or explicitly journals every accepted write after the restore point.
