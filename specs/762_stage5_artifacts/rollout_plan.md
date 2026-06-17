# Stage 5 Rollout Plan

Status: converged after three sequential independent reviewer convergence signals; owner security feedback incorporated after convergence.

## Default Release Position

The first Rust release is the cutover port, not a clone of every Python surface. The owner-approved retained and removed surfaces are in [cutover_scope.md](cutover_scope.md). Native `sm` mobile app flows, on-the-go attach, mobile auth/bootstrap, proofed app artifacts, core CLI/session workflows, email/human fallback delivery, Codex review/runtime, narrow queue jobs, and registered nodes remain first-class. Generic public browser/watch data, Telegram control, `sm what`, the redundant `sm kill` CLI alias, dispatch, Termux attach, watch-job, scheduled reminders, queue policy/CI helpers, and public unauthenticated artifacts are not Rust targets.

The initial shipping shape should be:

- Rust server/runtime takes ownership only after contract, migration, and rollback gates pass.
- First release is Rust server/runtime plus retained core CLI by default. Retired CLI commands are absent or return explicit retirement errors; they are not served by Python compatibility shims.
- Retained Python CLI commands may read compatible stores directly only as read-only clients. Any retained CLI command that writes durable/local state must route through the Rust owner, remain fenced as a clearly CLI-owned local compatibility surface with no Rust writer, or carry an explicit retirement gate.
- Python and Rust must not both write the same durable store during normal operation. Read-only shadowing is allowed only against copies, snapshots, or explicitly read-only handles.
- Rust may keep current external URLs, ports, file paths, schemas, and response shapes even when the internal implementation changes.
- The public edge is the internet-facing component and the origin should stay loopback/private where feasible. The edge must forward only allowlisted routes, require phone or node proof-of-possession before forwarding operational traffic, inject an internal controller assertion/token, and fail closed if proof, revocation lookup, route allowlist, or origin assertion validation fails. Human/mobile/browser routes still require origin OAuth/session or SM device-bearer user authorization after edge proof, and shell-capable routes still require attach proof/session authority.
- Registered nodes such as `macbook` remain LAN-first. Cloudflare/public-edge fallback is allowed only after LAN `studio.local` reachability fails and node proof-of-possession succeeds.

## Falsifiable Decision Gate

Before implementation tickets that replace runtime ownership are filed, capture a minimal current-Python baseline for:

- idle and loaded RSS/USS memory measured as median and max over three runs.
- startup and restore time with retained real state.
- p50/p95 latency for `/health`, `/sessions`, `/client/sessions`, `/client/bootstrap`, `/auth/session`, mobile attach-ticket mint, mobile terminal WebSocket auth, hook ingestion, `sm status`, `sm send`, queue wake delivery, node-agent reconnect, and SSE initial snapshot.
- retained 30-day usage for commands/endpoints/surfaces from the Stage 2 telemetry report.
- error and slow-request rates from retained logs.

The baseline artifact must compare the current Python service against a Rust spike/prototype on the same safe retained-state workload. The owner has explicitly waived Python hardening/config variant measurement for this cutover: Python hardening is throwaway work and should not block or consume Rust migration effort. Record the waiver in the baseline artifact instead of measuring variant branches.

The Rust migration should pause or narrow if:

- a Rust spike cannot show at least 25% lower median loaded RSS and USS than the current Python baseline, or at least 100 MiB RSS and 75 MiB USS absolute loaded improvement when that is the smaller threshold. Idle memory should be at least 15% lower or 50 MiB RSS/USS lower when that absolute threshold is smaller. No first-class workload may use more than 5% higher memory than the Python baseline without a documented mitigation.
- critical mobile attach, hook, queue, or session-control paths are slower than Python without a documented mitigation.
- compatibility or state-migration cost exceeds the value of the rewrite for first-class surfaces.
- rollback cannot be rehearsed from copied real state.

The comparison is against current Python as operated for retained workflows. Do not file Python hardening work as part of the Rust value gate unless the owner later reopens that decision.

If the current Python origin cannot remain healthy through sustained
Python-authoritative baseline/shadow, the owner-approved path is not broad
Python hardening. Use the accelerated Rust canary evidence mode instead:
`mvp_rehearsal --rust-canary-cutover` runs short Python canary spot checks, keeps
the Rust/state/fixture gates and Rust baseline blocking, and requires a passed
Cloudflare/mobile smoke report with real Access/public-edge/SM auth proof inputs
before the run can count as cutover-candidate evidence.

## Cutover Sequence

1. Build the contract harness from Stage 2/3/4 artifacts and run it against Python.
2. Run Rust against copied state in rehearsal mode. Rehearsal must not write to the live Python stores.
3. Create a pre-freeze safety backup of must-preserve stores, configs, app artifacts, logs needed for diagnostics, launchd files, and runtime metadata. This backup is for disaster recovery during rehearsal, not the final rollback restore point.
4. Enter write-admission freeze. Freeze coverage is generated from the full Stage 5 state-ownership table and Stage 2 persistence manifest. Python must block or journal new accepted writes for message delivery, reminders, parent wakes, review/job watches, Codex pending requests, queue-runner admission/execution, app artifact uploads, inbound email delivery, outbound email/human delivery, response relay claims, node/Codex cursor advancement, tool audit rows, Telegram telemetry/archive rows, bug-report create/update/prune, codex observability/log-prune rows, request timing/log rows used for telemetry, lock/worktree mutations, and any CLI/shim write path. Read-only diagnostics may continue.
5. Stop or drain risky live writers before ownership transfer: active mobile terminal attaches, queue-runner jobs, active node-agent control streams, in-flight Codex pending requests, review/job watches, message queue delivery, response relay delivery, provider event ingestion, external notifier sends, asynchronous tool audit logging, Telegram telemetry flushing, bug-report maintainer notification/update paths, codex observability pruning, and telemetry/log writers that can affect must-preserve stores. If a drain is impossible, the runbook must require explicit operator override and record the residual risk.
6. Create the final verified production backup after freeze/drain and before Rust ownership. The migration ledger must record freeze start, blocked/journaled write counts, drain status, final backup paths, hashes/sizes, and proof that no live writes were accepted after the restore point unless explicitly journaled for replay.
7. Stop Python through the service wrapper or controlled process ownership check. Do not kill an arbitrary process on the port without proving it is the Session Manager service.
8. Run Rust preflight against the live config and final backed-up state. Preflight must validate schema versions, required config, path roots, port ownership, auth policy, mobile/public host settings, write permissions, write-freeze ledger state, and restore-point hashes.
9. Start Rust on the current service port and paths.
10. Run post-cutover smoke checks: `/health`, `/health/detailed`, `/auth/session`, `/client/bootstrap`, `/client/sessions`, `/sessions`, `/events`, `sm status`, `sm send` to a test session, mobile attach-ticket mint, retained mobile/API session-stop behavior or reviewed app-retarget replacement, app artifact metadata, queue health, node registry, and configured external-channel health checks. If public-edge proof is enabled, smoke checks must also prove unauthenticated internet callers cannot reach operational data, origin rejects forwarded public traffic without a valid edge assertion, revoked phones/nodes are denied, and node fallback does not bypass LAN-first behavior.
11. Keep rollback available until Rust has passed the stabilization window and no incompatible state mutation is pending.

## Rollback Sequence

Rollback must be rehearsed before production cutover.

If Rust has not performed an incompatible migration:

- stop Rust.
- restore the Python launchd/service wrapper.
- start Python against the original stores or pre-cutover backups.
- run the same smoke checks and verify mobile app attach remains usable.

If Rust has written migrated state:

- stop Rust.
- restore backed-up Python-compatible stores atomically.
- restore app artifacts and launchd files if modified.
- mark any Rust-only side effects that cannot be replayed into Python as operator-visible rollback notes.
- require user review before attempting a downgrade that discards accepted state changes.

Rollback is not acceptable if it leaves Python and Rust disagreeing about queue ownership, pending Codex requests, mobile auth state, app artifact metadata, cursors, response relay claims, tool audit rows, Telegram telemetry, bug-report rows/delivery results, codex observability rows, request timing/log-derived telemetry, Telegram/topic state, or session records. The final post-freeze backup is the default rollback restore point; restoring the earlier pre-freeze safety backup is allowed only with explicit operator acknowledgement that accepted writes after that point will be lost, replayed from the write-freeze journal, or explicitly discarded with an operator-visible audit note.

## Kill Switches

The first Rust release must expose documented operator switches for:

- mobile terminal attach.
- queue runner admission/execution.
- public browser/watch exposure.
- public-edge forwarding and edge assertion validation.
- remote nodes/node-agent control.
- remote node public fallback.
- app artifact upload.
- Codex review watchers.
- Codex event ingestion and provider control.
- email/human recipient delivery and inbound email webhook admission.
- retired-surface denial behavior for Telegram, `sm what`, the `sm kill` CLI alias, dispatch, remind/watch-job/policy/Termux/summary-provider routes and commands.
- service/infra sidecar repair.

Kill switches must have observable status and must fail closed for public or command-execution surfaces where compatible.

## User-Review Decision Disposition

Owner-approved Stage 5 disposition for first Rust release:

| Candidate | First Rust Release Disposition |
| --- | --- |
| Disable or localhost/proof-scope generic public browser/watch while preserving native mobile APIs | Accepted. Rust must not return public operational browser/watch data outside auth/proof. Local/operator diagnostics and proofed/authenticated watch views may remain. |
| Require Cloudflare/public-edge proof-of-possession before public traffic reaches origin | Accepted for public remote access. Rollout must include route allowlist, device/node proof, OAuth/session or SM device-bearer user auth after edge proof, route-local capability checks, revocation UX, edge/origin fail-closed tests, downgrade behavior, and observability from `public_edge_proof_model.md`. |
| Allow registered nodes to use Cloudflare/public-edge fallback when LAN `studio.local` is unavailable | Accepted. Fallback must be LAN-first and require node proof, route allowlist, revocation, and audit. |
| Add device inventory/revocation commands such as `sm list-devices` and `sm remove-device <id>` | Accepted and required before proofed public mobile access. |
| Replace flaky two-step mobile attach-ticket flow with direct signed WebSocket attach proof | Accepted as a redesign option for implementation. Either keep current tickets or replace them, but the chosen design must preserve user auth, device proof, session binding, quotas, revocation, audit, and fail-closed behavior. This is not SSH transport auth today. |
| Port deprecated Termux/SSH attach metadata and command generation | Rejected. Rust must not advertise Termux as supported or primary; any reinstatement requires a later owner-approved issue. |
| Gate or change auth-exempt routes, browser OAuth redirects, or auth failure behavior | Accepted. Public operational data must require proof/auth; auth shell/OAuth endpoints may expose only minimal non-operational data. |
| Require route-local secrets for all non-loopback hooks | Accepted. Non-loopback hooks require route-local proof/secret; local tmux hook remains local-only. |
| Add stricter requester/capability checks to legacy session graph APIs | Accepted. Rust should use explicit capability checks for mutating session graph APIs; incompatibilities are allowed when they remove unsafe legacy authority. |
| Disable or gate sensitive read APIs, AI summary generation, and `sm what` | Accepted. Public sensitive reads, the AI summary provider route, and `sm what` are not Rust targets; use `sm tail --raw`, `sm output`, or explicit status prompts. |
| Remove redundant `sm kill` CLI alias | Accepted. `sm retire` is the retained CLI command for ending a session. Retained API/mobile session-stop behavior, including current Android/watch use of `POST /sessions/{session_id}/kill`, remains in scope unless the native app is explicitly retargeted through reviewed fixtures. |
| Rotate browser/device auth signing secrets or expire current sessions/tokens | Accepted when required by the public-edge/device design. Rollout must force re-login/re-enrollment explicitly rather than silently failing. |
| Disallow non-default public inbound email webhook aliases | Accepted only for ambiguous/non-proofed aliases. Email/human delivery and inbound email stay retained; Rust supports the default route and explicitly allowlisted/proofed configured routes. |
| Reject node-agent hook-secret fallback | Accepted. Registered nodes need explicit node credentials. |
| Add queue-runner command allowlists/policy gates | Accepted. Narrow queue jobs stay; policy-run and CI-helper surfaces do not. |
| Disable high-risk Telegram commands by default or deprecate Telegram control in favor of the native app | Accepted. Telegram bot/control/topic cleanup are not ported. |
| Expose `/hooks/tmux-client` remotely or accept new events | Rejected for first Rust release. Preserve local-only behavior. |
| Require app artifact signing or auth/proof before public serving | Accepted. Native app artifacts remain, but public unauthenticated serving is not the Rust target. |
| Tighten local bypass/proxy behavior | Accepted. Local bypass is same-user loopback only; public/proxy paths require edge assertion and route auth. |
| Add repo allowlists or extra approvals for GitHub/Codex review side effects | Accepted. Codex review remains first-class but gets repo/PR allowlists or explicit approval gates. |
| Reset Codex cursors/events or change reducer semantics during migration | Rejected. Preserve cursor/event/reducer semantics; drain/close pending state before cutover instead of resetting. |
| Move infra supervisor repair after port preflight or change install-script process killing | Accepted for cutover tooling. Use controlled process ownership proof; do not use arbitrary port killing as a safety mechanism. |

## Observability During Cutover

The Rust release must provide operator-visible checks for:

- auth failures by class without secrets.
- public-edge deny/forward decisions, edge assertion failures, revoked phone/node attempts, and LAN-vs-public node fallback decisions.
- route-local secret failures.
- mobile attach ticket mint/consume/disable/revoke failures.
- queue depth, queue-runner jobs, review watches, retained parent wakes, and notify-on-stop.
- node-agent connectivity, control failures, and remote inventory recovery.
- Codex event ingestion lag/cursor state and pending request orphaning.
- retired summary-provider/sensitive-read denial attempts.
- app artifact upload/download metadata.
- email/human/inbound accept/reject/delivery outcomes.
- retired Telegram attempts.
- migration progress, backup verification, state ownership, and rollback status.

These checks may start as CLI diagnostics or structured logs; dashboards are not required for the first implementation ticket, but the signals must exist before production cutover.
