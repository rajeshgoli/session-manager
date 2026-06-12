# Rust MVP Progress Snapshot

Status: implementation snapshot after PR #938 merged and the full MVP rehearsal passed.

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
| #878 | progress snapshot for resuming the Rust MVP track |
| #880 | fixture-gated manifest checks for queue job and Codex review request detail endpoints |
| #882 | validation-only manifest coverage for device-token and tmux-client hook error paths |
| #884 | client-session fixture assertion correction for classic attach support |
| #886 | synthetic Codex app read fixture for retained Codex read checks |
| #888 | synthetic app artifact metadata fixture |
| #890 | synthetic queue-runner job fixture for list/detail checks |
| #892 | disposable mutating fixture workspace for Rust-core CLI and HTTP checks |
| #894 | MVP rehearsal gate runs isolated read-only and mutating Rust-core contracts |
| #896 | handoff update after the first real-state rehearsal |
| #898 | `/events/state` shadow comparison moved to status-only, clearing the rehearsal blocker |
| #900 | clean #898-era handoff update |
| #902 | shadow observation report with blocker handling |
| #904 | non-mutating shadow workflow planner |
| #906 | safe `rust_shadow` config activation helper |
| #908 | shadow report `--since` and `--last-minutes` filters |
| #910 | Rust contract CLI checks default to `target/debug/sm` |
| #912 | `--skip-fixture-checks` for broad live-state contract runs |
| #914 | handoff refresh after live shadow activation |
| #916 | shadow report coverage gates |
| #918 | shadow observation planner carries coverage gates |
| #920 | mobile device CLI contract checks |
| #922 | Rust CLI cutover scope audit |
| #924 | state ownership preflight |
| #926 | state backup plan and copy tool |
| #928 | freeze/drain plan ledger scaffold |
| #930 | backup verification and restore rehearsal |
| #932 | MVP rehearsal runs state preflight, backup, restore, and freeze/drain evidence |
| #934 | handoff update after the state-gated rehearsal |
| #936 | live session detail shadow comparison moved to status-only |
| #938 | MVP rehearsal runs synthetic read-only fixture contracts in a dedicated sidecar |

## Implemented Capability Groups

The Rust sidecar now has executable coverage for:

| Group | Current state |
| --- | --- |
| Harness and baselines | Contract manifest, fixture assertions, minimal value baseline runner, shadow comparison, MVP rehearsal, synthetic read-only fixture sidecar, disposable mutating fixtures, Rust CLI build gating, state preflight/backup/restore, and freeze/drain evidence gates exist. |
| Retired-surface checks | Retired public/watch/summary/remind/job-watch/queue-policy checks are represented as Rust-target absence or denial fixtures. |
| Core reads | Health, auth session, bootstrap, session list/detail, client session list/detail, output, events state, SSE hello, nodes list, queue jobs list/detail, Codex review requests list/detail, and tool/audit read projections are implemented. |
| Core runtime | Session/tmux/spawn/session-graph/message-queue/task-complete/input-batch/subagent slices are merged, with shadow and contract fixtures covering the early cutover path. |
| Codex retained reads | Codex event stream, pending request ledger reads, activity actions, review detail/list, and Claude/Codex tool-call projections are implemented and covered by manifest checks. |
| Mobile | Native bootstrap/session support, attach-ticket proofing, terminal WebSocket auth/bridge, runtime disable, device revoke/list CLI support, and public-edge assertion validation are merged. |
| External fallback | Email/human fallback delivery and inbound email validation path are retained in the Rust track. |
| Queue and nodes | Narrow queue list/detail, queue fixture coverage, and registered-node read paths are implemented; retained node and queue writer behavior still needs final cutover verification. |

## Current Live Shadow And Contract State

Live Python-authoritative Rust shadow mode is active in the local config:

- Python origin: `:8420`;
- Rust sidecar: `127.0.0.1:8421`;
- shadow ledger: `~/.local/share/claude-sessions/rust_shadow.jsonl`;
- config backup created by the activation helper:
  `config.yaml.shadow-backup-20260612T023248Z`.

The latest clean short-window shadow report at `2026-06-12T02:59:18Z` used
`--last-minutes 1 --fail-on-blockers` and returned:

| Metric | Result |
| --- | ---: |
| Status | passed |
| Rows | 86 |
| Blockers | 0 |
| `GET /events/state` | 28 status matches |
| `GET /sessions` | 58 status matches |

The fixture-filtered broad live Rust contract run now passes without synthetic
fixture false failures:

| Metric | Result |
| --- | ---: |
| Passed | 71 |
| Skipped | 3 |
| Failed | 0 |

The skipped checks are mutating checks without `--include-mutating`, which is
the expected safety behavior for a live-state read run.

## Latest Real-State Rehearsal

Report:
`.local/rust-mvp-rehearsals/20260612T-full-after-938/mvp-rehearsal-report.json`

Summary:

| Area | Result |
| --- | --- |
| Overall status | Passed with 0 blockers |
| Python health | Passed |
| State ownership gate | Passed: 17 stores checked, 13 existing, 13 copied, 13 verified, 13 restored, freeze/drain ledger written |
| Rust sidecar health | Passed using explicit `--reuse-rust-sidecar` because port 8421 was already healthy |
| Isolated runtime smoke | Passed |
| Rust live core sidecar contracts | 17 passed, 0 failed, 0 skipped |
| Rust synthetic read-only fixture contracts | 10 passed, 0 failed, 0 skipped |
| Rust mutating fixture contracts | 30 passed, 0 failed, 0 skipped |
| Gap probes | 0 failed |
| Python baseline | Passed |
| Rust baseline | Passed |
| Shadow read summary | 8 passed, 0 failed |

The earlier blocker was a body mismatch for `GET /events/state` in shadow
comparison. PR #898 reclassified that route as status-only because Python
carries live tmux-client event state that a fresh Rust sidecar does not own.
After PR #936, live session detail is also status-only for shadow comparison to
avoid lifecycle TOCTOU noise while fixture-backed deterministic session detail
checks remain body/assertion based. The clean run now passes the shadow summary
for `/health`, `/health/detailed`, `/auth/session`, `/client/bootstrap`,
`/sessions`, `/client/sessions`, `/nodes`, and `/events/state`.

Measured baseline snapshot from the same run:

| Metric | Python | Rust |
| --- | ---: | ---: |
| RSS | 154.672 MiB | 19.797 MiB |
| Physical footprint | 66.4 MiB | 6.688 MiB |
| `GET /health` median | 4.167 ms | 0.275 ms |
| `GET /health/detailed` median | 1078.139 ms | 0.294 ms |
| `GET /auth/session` median | 18.825 ms | 0.268 ms |
| `GET /client/bootstrap` median | 6.622 ms | 0.298 ms |
| `GET /events/state` median | 5.017 ms | 0.288 ms |
| `GET /sessions` median | 25.746 ms | 7.972 ms |
| `GET /client/sessions` median | 58.486 ms | 7.946 ms |

## Near-Term Remaining Work

These are the next practical buckets before an MVP cutover trial:

| Bucket | Why it remains |
| --- | --- |
| Shadow observation window | Python-authoritative shadow mode is enabled and a short clean window has been recorded. Continue it for a longer agreed window and triage any unexplained retained-core mismatches before Rust becomes the writer. |
| Full fixture manifest execution | The MVP rehearsal now runs the current synthetic read-only and mutating fixture sets. Remaining work is final live mobile/device fixture evidence and any additional retained fixture rows added by later slices. |
| CLI cutover audit | Verify every retained CLI command in [cutover_scope.md](cutover_scope.md) is native Rust or intentionally routed, and every removed command is absent or explicitly retired. |
| State ownership and migration tooling | Initial preflight, backup, restore, and freeze/drain evidence tools are merged and exercised by the rehearsal. Remaining work is live write-admission freeze/journal ownership and rollback accounting from [state_ownership_and_migration.md](state_ownership_and_migration.md). |
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
