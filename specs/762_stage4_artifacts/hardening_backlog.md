# Stage 4 Hardening Backlog

Status: converged after three sequential independent reviewer convergence signals; owner security feedback incorporated after convergence.

## Non-Breaking Requirements

| Area | Requirement | Handoff |
| --- | --- | --- |
| auth policy | Generate public/loopback/auth-exempt tests from the Stage 2 route auth matrix. | Contract harness before Rust route implementation. |
| route-local secrets | Keep hook, node-agent, email worker, device-key, mobile ticket, browser session, and bearer-token checks explicit after middleware. | Use [route_local_secret_matrix.md](route_local_secret_matrix.md); one test per missing/mismatch/reuse/logging/rotation row where current behavior is externally observable. |
| mobile terminal | Preserve TLS/origin/ticket/quota/revoke/disable semantics and audit mint/consume/failure. | Mobile terminal is high priority and cannot be deferred behind generic browser watch. |
| public-edge proof boundary | Design a public-edge/tunnel boundary that can deny callers before origin access and forward only allowlisted mobile/browser/email-worker/node routes with internal controller authentication. | Stage 5 accepts this as the Rust public-access direction rather than broad `sm.rajeshgo.li` exposure. |
| device and node revocation | Provide inventory and revocation semantics for enrolled phones and registered node fallback credentials. | `sm list-devices` and `sm remove-device <id>` or equivalent APIs are required before relying on mobile device proof; node fallback needs token rotation/removal and audit. |
| session-control authority | Implement typed policy helpers for current requester/self/parent/EM checks. Preserve current allow/deny outcomes for retained flows except where Stage 5 intentionally tightens unsafe legacy authority on mutating APIs. | Abuse-case fixtures for every high-risk session graph/control API before Rust route implementation, including subagent start/stop, turn-complete, agent-status, and watch-session coordination-state mutations. |
| sensitive reads and summary provider | Preserve retained local/operator sensitive read auth/response contracts while bounding provider subprocess errors and auditing summary requests. | Use the sensitive-read handoff below; public sensitive reads and AI summary HTTP route are removed by Stage 5. |
| Codex event ingestion | Preserve event/reducer/cursor state semantics with schema validation and transactional cursor advancement. | Use the Codex event handoff below; resetting events/cursors or changing reducer semantics requires Stage 5. |
| payload bounds | Add explicit size/time limits for HTTP bodies, hooks, raw email, WebSocket frames, SSE queues, app uploads, and queue-runner script metadata. | Use the per-surface payload table below; Stage 5 accepts limits for retained surfaces, with defaults chosen from observed maxima or explicit config so implementation tickets do not rediscover bounds. |
| secret handling | Redact secret-bearing config, node tokens, hook secrets, worker secret, device keys, bearer tokens, and raw proxy commands from responses/logs. | Tests should assert absence in `/nodes`, bootstrap, attach descriptor, errors, and logs where feasible. |
| persistence | Backup before migration, fail fast on schema mismatch, and preserve or drain must-preserve stores. | Stage 5 cutover plan owns backup/downgrade mechanics. |
| audit | Keep destructive/sensitive tool audit, app upload actor attribution, mobile attach audit, queue job creation/cancel, inbound email accept/reject, node-agent auth failures, and public auth failures. | Privacy-safe logging only; no raw tokens or full secret-bearing payloads. |
| path safety | Validate artifact, bug-report, node log/control, queue-runner script, backup, and tmux/socket paths against configured roots. | Use the per-surface path table below; Stage 5 accepts managed-root traversal rejection while preserving explicit operator paths where retained behavior depends on them. |
| availability | Keep request timing/slow logs, queue health, mobile attach failure counters, node-agent reconnect counters, and migration progress events. | Use the per-surface observability table below; Stage 5 owns dashboard/rollout wiring. |

## Payload And Timeout Handoff

| Surface | Current Limit / Behavior | Rust Handoff | Compatibility Risk / Fixture |
| --- | --- | --- | --- |
| mobile terminal WebSocket | Input frame max 8192 chars; resize rows 2-120, cols 10-300; auth frame timeout default 3s; max attach default 3600s; history preload default 4000 lines. | Preserve exact bounds and close/error strings from Stage 2 protocol manifest. | Fixtures for oversized input, invalid resize, auth timeout, max attach expiry, and history preload. |
| node-agent WebSocket | First `hello` within 5s; unknown frames ignored; control timeout comes from configured frame. No Stage 2 payload-byte cap recorded. | Preserve hello timeout and unknown-frame behavior. Add configurable high-water frame/body caps only after Stage 5 picks defaults from observed event/control sizes. | Fixtures for missing/wrong hello, bad secret, path violation, malformed event, unknown frame, and oversized-frame rejection once a limit is chosen. |
| public-edge node fallback | No current Rust behavior. Owner-approved future behavior may let registered nodes such as `macbook` use Cloudflare/public edge when `studio.local` over LAN is unavailable. | LAN-first fallback only; require registered node proof-of-possession before forwarding; bound forwarded frame/body sizes; audit fallback use and revocation. | User-reviewed design fixtures for LAN failure fallback, invalid node proof, revoked node proof, route allowlist, controller-token forwarding, and no origin reachability without proof. |
| SSE `/events` | Subscriber queue size 32; keepalive comment every 15s; `text/event-stream`, `Cache-Control: no-cache`, `X-Accel-Buffering: no`. | Preserve queue size, headers, initial `hello`, and keepalive semantics. | Fixture for slow subscriber/backpressure and initial snapshot. |
| app artifact upload | Non-empty APK; 100 MB max; app name and hash regexes; multipart form required. | Preserve limits, error shapes, temp/latest/hash/meta write order, cache headers, and actor attribution. | Fixtures for empty, >100 MB, invalid version, invalid app/hash, metadata read failure, and public immutable serving. |
| hook JSON endpoints | Current source accepts empty/invalid hook bodies as 204 in some hook paths and relies on framework/body parsing; no app-specific byte limit recorded. | Preserve empty/invalid-body compatibility. Add observe-first body-size logging; enforcement default must be Stage 5-approved unless set above observed retained maxima. | Fixtures for malformed JSON, empty body, valid local hook, remote hook secret missing/mismatch, and future oversized-body behavior. |
| tmux client hook | Query-param endpoint, not JSON body. Accepts only local-origin requests with event names `client-attached`, `client-detached`, `client-session-changed`; query values are event/session/client/tty/pid. | Preserve local-only guard and event allowlist. Add length caps for query values only if above observed tmux values or Stage 5-approved. | Fixtures for non-local 403, unsupported event 400, versioned payload, and SSE update. |
| inbound email | Requires `raw_email` or enough parsed content; no explicit raw-email byte limit recorded in Stage 2. | Preserve authorized sender/worker-secret/routing behavior. Add configurable size limit only with Stage 5 default, rejection status, and telemetry. | Fixtures for missing raw content, unauthorized sender, bad secret, trusted header without secret, routing footer, stopped-session restore, and future oversized email. |
| summary generation | HTTP route accepts `lines` for captured tmux output and config timeout default 60s; Telegram summary captures 100 lines and uses 60s timeout; no prompt-char cap recorded. Current responses/replies truncate provider stderr to 200 chars, but logs include full provider stderr. | Preserve current line/default behavior, timeout, and 200-char response/reply stderr truncation. Add log redaction/truncation fixtures as new hardening, and add rate-limit/char-cap only with Stage 5 default if it can reject currently accepted summaries. | Fixtures for no output 404/Telegram message, timeout, provider nonzero exit, empty summary, response/reply stderr truncation, log redaction/truncation, and audit attribution. |
| queue-runner script metadata | CLI/API accept argv/scripts/env/cwd metadata; Stage 2 did not record a byte cap. | Preserve retained narrow queue command/script contracts. Add length limits as policy configuration with rollout metrics and fixtures for oversized metadata. | Fixtures for script-file path, inline script metadata, env/cwd handling, timeout/cancel, and future oversized metadata. |
| generic HTTP JSON bodies | FastAPI/Pydantic validation, route-specific schemas, no Stage 2 global app cap recorded. | Do not silently add a low global cap. Prefer per-route typed validation plus configurable global body cap with Stage 5 rollout metrics. | Negative fixtures for 400/401/403/404/409/410/422/503 route errors before cap enforcement. |

## Path-Safety Handoff

| Surface | Current Path Contract | Rust Handoff | Compatibility Risk / Fixture |
| --- | --- | --- | --- |
| app artifacts | Configured app artifacts dir; app names match `^[a-z0-9][a-z0-9-]*$`; hashed APK names are 8 lowercase hex chars; temp/latest/meta writes are under app dir. | Use root-constrained joins, atomic writes, and reject traversal/symlink escapes under managed artifact roots. | Fixtures for invalid app/hash, immutable file lookup, latest redirect, and metadata unreadable. |
| bug reports and attachments | Configured/source-defined DB and attachment storage; native app reports can include client/server debug state. | Keep report lookup compatibility; constrain attachment writes/reads to configured report root and preserve DB schema. | Fixtures for selected-session metadata, attachment lookup, invalid attachment id, and storage-root traversal rejection. |
| node log/control paths | `nodes.registry.<id>.log_dir`, `control_path`, and restore inventory paths are node-scoped; node-agent register validates paths under node `log_dir`. | Preserve node path confinement and `/nodes` secret/path redaction. Reject register/control paths outside configured node roots. | Fixtures for register path violation, restore inventory path, reconnect, and control timeout. |
| queue-runner scripts and job dirs | Operator-supplied script paths and generated per-job dirs/logs exist; Stage 2 classifies queue runner as command-execution state. | Preserve operator-supplied paths where currently allowed. Constrain generated job files/logs to queue-runner state roots; stricter external script rejection needs user review. | Fixtures for script-file, per-job logs, cancel/timeout, and recovery after restart. |
| tmux sockets, logs, and attach descriptors | Existing socket names, tmux session names, per-session logs/transcripts, and attach descriptor semantics are compatibility contracts. Termux command rendering is deprecated and excluded from the Rust target. | Preserve path naming long enough for current clients and live sessions. Validate generated paths and avoid leaking raw proxy commands in any retained legacy fields. | Fixtures for attach descriptor, Termux-not-supported/non-primary behavior, dead pane, restore, and remote-node unreachable state. |
| Codex fork runtime artifacts | `*.codex-fork.events.jsonl` and `*.control.sock` live under configured log/runtime roots; remote node registration validates paths under node `log_dir`. | Preserve path naming and confinement through cutover. Treat live artifacts as ephemeral IPC that must be quiesced/drained, not blindly migrated. | Fixtures for malformed path, missing artifacts, remote register path violation, runtime restart, and cursor recovery. |
| migration backups | No Rust migration behavior exists yet. | Stage 5 must create backups under a documented root with no traversal from config/session ids and with restore verification. | Dry-run backup/restore fixture before implementation tickets. |

## Availability And Observability Handoff

| Signal | Current Source / Contract | Rust Handoff | Stage 5 Output |
| --- | --- | --- | --- |
| request timing | `RequestTimingMiddleware` emits threshold-biased `Request:` and `SLOW REQUEST:` method/path timing logs used by Stage 2 usage telemetry. | Preserve or replace with equivalent method/path/status/duration logs and mark threshold bias. | Operator command/report for route latency and recent errors. |
| auth failures | Middleware returns redirect, 401 JSON, or 503 JSON depending on path/auth readiness; route-local checks have specific statuses/frames. | Count privacy-safe failures by mechanism without logging secrets: browser OAuth, bearer token, hook secret, node secret, email worker secret, mobile device/ticket. | Rollout gate for unexpected public auth failures. |
| mobile attach | Stage 3 covers ticket mint/consume, active attaches, disable/revoke, and PTY cleanup. | Emit counters/events for mint success/failure reason, consume failure reason, active attach start/stop, disable, and cleanup. | Mobile attach health command/report; native mobile remains first-class. |
| queue and watches | Message queue, review watches, job watches, queue runner, and policy DBs persist state and notifications. | Expose queue depth, active watches, retry/error counts, queue-runner running/queued/failed counts, and recovery counts. | Cutover gate before Rust owns queues. |
| node-agent and remote nodes | Node-agent protocol logs hello/register/control errors; `/nodes` exposes non-secret metadata. | Count node-agent auth failures, disconnects, reconnects, register failures, control timeouts, restore inventory errors. | Remote-node health in rollout/cutover checks. |
| public edge and device/node proof | No current edge-level proof boundary exists in Python; origin middleware currently sees public traffic for exposed hosts. | Count edge denies before origin, forwarded route decisions, device enrollment/removal, node fallback proof success/failure, revoked credential use, and controller-token forwarding failures. | Public-edge rollout gate before public mobile/node fallback exposure. |
| sensitive reads and summaries | Sensitive read APIs expose transcripts/tool/Codex/review state; summary subprocess emits provider exit/stderr/timeout signals. | Count summary invocations, provider failures/timeouts, stderr truncation events, and high-volume sensitive read patterns without logging sensitive payloads. | Operator-visible diagnostic for summary/provider failures and suspicious read volume. |
| Codex event ingestion | Codex event store, provider cursor, reducer, and observability stores track event ingestion and activity projection. | Count malformed events, duplicate/skipped seqs, cursor persistence failures, reducer errors, remote event gaps, and control degradation/restoration. | Cutover gate before Rust owns Codex event ingestion/cursors. |
| service/infra | Launchd wrapper writes `/tmp/session-manager.log`; server infra supervisor currently starts before port preflight. | Preserve log location or document new one; expose service startup phase, port ownership, infra repair outcome, and health-check failures. | Rollback/diagnostic command for failed launchd cutover. |
| migration progress | No Rust migration exists yet. | Stage 5 must define structured migration progress, backup verification, ownership handoff, rollback success/failure, and downgrade status. | Required before implementation tickets that mutate state. |

## Stage 5 Cutover Dispositions

These reduce attack surface but may break current behavior. The owner has now approved the Stage 5 cutover decisions in `../762_stage5_artifacts/cutover_scope.md`; this table records the resulting first-release treatment.

| Candidate | First Rust Release Treatment |
| --- | --- |
| Disable or localhost-scope generic public browser/watch while preserving native mobile APIs. | Accepted. Public operational browser/watch data is removed; local/authenticated/proofed diagnostics may remain. |
| Require Cloudflare/public-edge proof-of-possession before any public traffic reaches the Session Manager origin. | Accepted. Public remote access must fail closed before origin without phone/node proof. |
| Allow registered remote nodes to use Cloudflare/public-edge fallback when LAN `studio.local` is unavailable. | Accepted. Node fallback is LAN-first and requires node proof, route allowlist, revocation, and audit. |
| Add or change device-management commands such as `sm list-devices` and `sm remove-device <id>` as required gates for public-edge mobile access. | Accepted and required. |
| Require route-local hook secret for every non-loopback hook request. | Accepted. |
| Add stricter requester/capability checks to session graph APIs. | Accepted for mutating APIs. |
| Disable or gate sensitive read APIs, AI summary generation, and `sm what`. | Accepted for public/sensitive HTTP summary and `sm what`; use raw tail/output or explicit status prompts instead. |
| Rotate browser/device signing secrets or forcibly expire current browser/device auth. | Accepted if required by public-edge/device cutover; must be explicit re-login/re-enrollment, not silent failure. |
| Reject non-default public email webhook aliases unless configured as explicit auth-exempt worker-secret routes. | Accepted for ambiguous/non-proofed aliases. Retain default inbound email and explicitly allowlisted/proofed configured routes. |
| Reject node-agent hook-secret fallback when `node_token` is absent. | Accepted. |
| Add queue-runner command allowlist or stricter policy gate. | Accepted for retained narrow queue jobs; queue policy/CI helper surfaces are removed. |
| Disable, gate, or deprecate Telegram destructive/control commands in favor of the native app. | Accepted. Telegram bot/control/topic cleanup is not ported. |
| Expose `/hooks/tmux-client` remotely or accept new event names. | Preserve current local-only guard and three-event allowlist. |
| Require signed APK/artifact metadata before public serving. | Accepted: require auth/proof or signing; public unauthenticated serving is not the Rust target. |
| Tighten loopback/local bypass for forwarded/proxied requests. | Accepted. Local bypass is same-user loopback only. |
| Move infra supervisor repair after port preflight or change install-script process killing. | Accepted for cutover tooling: use process ownership proof, not arbitrary port killing. |

## Stage 5 Handoff Gates

Stage 5 must document:

- which Stage 4 cutover candidates are accepted or rejected.
- cutover backup and rehearsal plan for every must-preserve store.
- rollback path when Rust auth policy, persistence migration, queue runner, or mobile terminal fails.
- kill switches for mobile terminal, queue runner, public-edge forwarding, remote node fallback, remote nodes, app artifact upload, review watchers, retired-surface denial behavior, and service/infra sidecars.
- observability dashboards or commands for auth failures, public-edge deny/forward decisions, mobile attach failures, device/node proof failures, route-local secret failures, queue depth, node-agent connectivity, Codex event ingestion, retired-surface attempts, migration progress, and slow requests.
- downgrade behavior for browser/mobile clients, node agents, hooks, queue workers, launchd wrappers, and provider/review watchers that outlive a partial cutover.

## Review Checklist For Stage 4 Reviewers

Reviewers should verify:

- every Stage 2 security-sensitive route/protocol/client/config/persistence surface maps to at least one threat-register row.
- native mobile auth/bootstrap/terminal/app artifact flows are treated as first-class, not collapsed into generic public browser concerns.
- every auth-exempt route has a current-control and residual-risk entry.
- every proposed removal/gating/default change has a Stage 5 accepted/rejected disposition before implementation tickets are filed.
- non-breaking hardening work is specific enough to file implementation tickets without re-reading all Python source.
