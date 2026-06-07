# Stage 5 Gate Matrix

Status: converged after three sequential independent reviewer convergence signals; owner security feedback incorporated after convergence.

## Gate Summary

| Gate | Required Evidence | Blocks |
| --- | --- | --- |
| value/falsifiability | Python baseline, Rust spike or prototype comparison, and comparison against feasible Python hardening. | Filing runtime replacement tickets. |
| contract capture | Retained Stage 2 route/CLI/protocol/schema/client/config/persistence artifacts converted into executable tests against Python; removed surfaces from `cutover_scope.md` converted into retirement/absence tests. | Any Rust route/CLI/protocol implementation claiming compatibility. |
| internal behavior | Stage 3 ordered recovery and state-transition fixtures pass against Python. | Rust ownership of queue, sessions, tmux, mobile attach, provider events, and external delivery. |
| threat controls | Stage 4 threat register and secret matrix have tests for auth, route-local secrets, abuse cases, no-secret logging, retained email/GitHub integrations, and Telegram retirement/absence. | Public exposure, mobile terminal, hooks, node agents, queue runner, app upload, retained email/GitHub integrations, and retired Telegram denial/absence fixtures. |
| public-edge proof | Public-edge proof model fixtures show default-deny forwarding, valid/revoked phone proof, valid/revoked node fallback proof, origin edge-assertion rejection, route allowlist, and no public operational data outside auth/proof. | Public remote access. |
| state rehearsal | Migration runs on copied real state and produces matched compatibility reports. | Live cutover. |
| write freeze and final backup | Python write admission is frozen, active writers are drained or risk-accepted, final backup is created after the freeze, and the ledger proves no accepted writes landed after the restore point except journaled writes. | Live cutover. |
| backup/restore | Pre-freeze safety backup and post-freeze final backup are created, verified, and restored in rehearsal. | Live cutover. |
| rollback | Python service can be restored and smoke checks pass after Rust rehearsal writes copied state. | Live cutover. |
| observability | Operator-visible health/status exists for auth, queue, mobile, nodes, Codex, summary, sensitive reads, migration, and slow requests. | Live cutover. |
| user review | Every accepted breaking change, destructive migration, or operational risk is explicitly approved. | Implementing that change. |

## Baseline Workloads

Baseline measurements must use retained real state where safe plus fixture workloads:

- idle server with current config.
- session list/status with current retained sessions.
- native mobile bootstrap/session list/attach-ticket/WebSocket auth.
- `sm status`, `sm send`, `sm wait`, narrow `sm queue`, and `sm request-codex-review` representative commands.
- hook ingestion for tool-use, context-usage, tmux-client, and remote hook secret paths.
- queue recovery with parent wakes, notify-on-stop, Codex review watches, and pending messages.
- Codex event ingestion/reducer/cursor replay.
- response relay, email/human delivery, inbound email webhook, and GitHub/Codex review notification paths with fake notifiers where needed; Telegram surfaces are retirement tests.
- node-agent hello/register/control/event frames.
- public-edge proof and node fallback fixtures.
- app artifact metadata/latest/hash serving and upload validation.
- local/auth/proofed watch diagnostics, SSE `/events`, `/events/state`, and denial of public unauthenticated operational watch data.

Feasible Python hardening/config comparisons must include measured or explicitly ruled-out variants for:

- disabling integrations that are already disabled or unused in the current config.
- reducing retained event/log scan windows where compatible.
- deferring startup background work that is not needed for first response where compatible.
- removing retired Telegram, `sm what`, the `sm kill` CLI alias, dispatch, remind/watch-job/policy/Termux surfaces per cutover scope, and isolating retained email/node/queue-runner/mobile-terminal work when disabled by config.
- reducing logging verbosity or request timing thresholds where compatible.

## Target Thresholds

These are acceptance targets for the first replacement release:

- Memory: Rust loaded median RSS and USS must be at least 25% lower than the best measured Python-compatible baseline, or at least 100 MiB RSS and 75 MiB USS lower when those absolute thresholds are smaller. Rust idle median RSS and USS must be at least 15% lower, or at least 50 MiB lower when that absolute threshold is smaller. No first-class workload may use more than 5% higher RSS/USS than the Python-compatible baseline without an approved mitigation.
- Latency: p95 latency for first-class mobile attach/auth/bootstrap, hook ingestion, queue wake delivery, and session-control APIs must be no worse than Python by more than 10% unless an explicit mitigation is approved.
- Startup/recovery: Rust restore/recovery must be no slower than Python by more than 10% for retained state, and must not change recovery ordering contracts.
- Reliability: contract fixtures must pass with zero known compatibility regressions for first-class mobile, CLI/operator, queue, hooks, session lifecycle, and persistence surfaces.
- Security: no route-local secret, public auth, mobile terminal, node-agent, queue-runner, app artifact, or retired-surface denial fixture may be skipped before public or command-execution exposure.

If thresholds are missed, the spec should recommend one of: optimize Rust, narrow the first Rust release, keep a Python compatibility shim, or stop the migration and file Python hardening tickets instead.

## Smoke Checks

Minimum post-cutover smoke checks:

- `GET /health` and `GET /health/detailed`.
- `GET /auth/session` for local, browser-auth, and device-bearer modes where configured.
- `GET /client/bootstrap` and `GET /client/sessions` from the native app perspective.
- mobile attach-ticket mint and WebSocket auth failure/success paths.
- retained Android/watch session-stop action through `POST /sessions/{session_id}/kill`, or a reviewed app-retarget replacement.
- public-edge deny/allow behavior, origin edge-assertion rejection, device list/remove, revoked-device denial, and node public fallback LAN-first behavior.
- `POST /maintainer/ensure`, `POST /client/request-status`, `GET /client/analytics/summary`, and representative bug-report create/list/detail flows.
- `GET /sessions`, `sm status`, and `sm send` to a controlled test session.
- `sm tail --raw` and `sm output` cover raw inspection; `sm what` is absent or returns a retirement error.
- local/auth/proofed watch behavior, SSE `/events` initial hello/keepalive/backpressure behavior, `/events/state`, public unauthenticated operational-data denial, and no `/api/sessions` shim.
- queue depth and retained parent-wake/Codex-review registrations; retired reminder/watch-job APIs deny or are absent.
- node registry and node-agent reconnect status.
- app artifact `meta.json`, latest APK, hashed APK, and `/apk` redirect.
- email/human recipient delivery and inbound email webhook smoke checks pass for worker-secret, authorized-sender, trusted-session-header, ignored routing, and delivery paths.
- Telegram routes and CLI commands, `sm what`, and the `sm kill` CLI alias are absent or return explicit retirement errors.
- Codex pending request listing and event cursor status.
- migration ledger `status` and rollback availability.

## Review Checklist For Stage 5 Reviewers

Reviewers should verify:

- Stage 5 cutover scope matches the owner-approved retained/removed surface list.
- native mobile app and on-the-go attach remain first-class in rollout gates.
- public browser/watch operational data is removed from unauthenticated public access; public-edge proof is tested as fail-closed.
- Python/Rust coexistence has a single-writer model for must-preserve stores.
- backups and rollback are concrete enough to execute against copied real state.
- contract and baseline gates are measurable, not aspirational.
- launchd/service cutover avoids arbitrary process killing without ownership checks.
- active sessions, queues, node agents, provider requests, and external notifiers are drained or explicitly risk-accepted before cutover.
- observability covers failures an operator would need to diagnose rollback.
