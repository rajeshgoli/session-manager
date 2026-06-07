# Stage 5 Cutover Scope

Status: owner-approved cut decisions after staged review convergence.

The Rust port is not a compatibility-preserving clone of every Python surface. It is the cutover port: keep the useful, stable, fast core and leave deprecated or low-value surface area behind. Current Python remains the rollback path for old behavior during the migration window.

## Usage Evidence

Retained evidence is incomplete, but it is good enough to distinguish active core from cruft:

| Evidence | Observed Signal |
| --- | --- |
| Core watch/session API | Access logs show heavy use: `GET /sessions` 120479, `GET /events/state` 21699, `GET /sessions/{id}` 320, `POST /sessions/{id}/input` 159. |
| Native mobile/app attach | Access logs show `GET /client/sessions` 67, `POST /client/sessions/{id}/attach-ticket` 9, `/client/bootstrap` 8, app metadata 13. Owner priority overrides sparse retained mobile logs. |
| Remote nodes | Access logs show `/nodes` 60, restore-candidates 153, node ping 6, node restore 3. |
| Codex review/events | Local DB has 146 Codex review registrations and 125684 Codex session events; access logs show 83 `POST /codex-review-requests`. |
| Simple queue jobs | Queue DB has 19 jobs; access logs show queue job create/list/cancel activity. |
| Telegram | 8920 telemetry rows prove activity, but owner direction is to replace remote control with the native app rather than port Telegram's large external command surface. |
| Email/human fallback | Owner direction keeps email/human recipient delivery and inbound email as the fallback external channel after Telegram removal. It must be ruggedized instead of treated as public operator UI. |
| Unused or near-zero retained state | `job_watch_registrations`, `scheduled_reminders`, `queue_policy_runs`, `codex_pending_requests`, and `tool_usage` have no retained rows in the checked local stores. |
| CLI direct usage | Shell history is incomplete and cannot prove command-level use. CLI scope is decided by product/core workflow, not raw shell history. |

Telemetry is decision support, not the sole authority. The owner has now approved breaking scope reductions for the Rust cutover.

## Retained First-Release Core

These surfaces are in scope for the first Rust release:

| Area | Retained Scope |
| --- | --- |
| Core session lifecycle | create/new, spawn, fork, restore, retire, open where local, attach to tmux, output/tail, clear, handoff, parent/child relationships, registry/role/maintainer identity that current agent workflows depend on. |
| Core CLI | `sm me`, `who`, `status`, `all`, `watch`, `send`, `email`, `wait`, `spawn`, `fork`, `new`, `children`, `retire`, `restore`, `attach`, `output`, `tail`, `clear`, `handoff`, `context-monitor`, `maintainer`, `register`, `unregister`, `lookup`, `roster`, `review`, `request-codex-review`, and current Codex provider commands that are active. |
| Native mobile app | Google/device auth, bootstrap, client session list/detail, mobile terminal attach, request-status, analytics, bug reports, app update/artifact flow, device inventory/revocation. |
| Mobile terminal | Keep or redesign the current attach-ticket/WebSocket path, but preserve auth, device proof, session binding, quotas, revocation, audit, and fail-closed behavior. Termux is not part of this scope. |
| Public remote access | Public edge proof is required before operational public traffic reaches origin. Public callers must prove enrolled phone or approved node possession, then pass OAuth/device-bearer or node authorization at origin. |
| Remote nodes | Keep node registry, node-agent protocol, restore inventory, remote Codex/session control, and LAN-first public-edge fallback for registered nodes. |
| Codex runtime | Keep Codex app/fork event ingestion, reducer/cursor behavior, pending-request ledger semantics needed by current providers, and Codex review request flow. |
| GitHub/Codex review | Keep PR review request/watch flow, but add repo allowlists or explicit approval gates as part of the cutover hardening. |
| Hooks and audit | Keep local hook ingestion, tool-use audit, context usage, tmux-client local-only hook, lock/worktree safety, and no-secret logging. Non-loopback hooks require route-local proof. |
| Message delivery | Keep direct session delivery, queued delivery, parent wakes, notify-on-stop, response relay, and stop-hook delivery semantics used by agent workflows. |
| Email/human fallback delivery | Keep outbound email/human recipient delivery and inbound email webhook delivery as the fallback external channel after Telegram removal. Inbound email must require route-local worker proof before trusting the session header, enforce authorized senders, preserve restore-and-deliver behavior, and log accept/reject reasons without raw email or secrets. |
| Queue runner, narrow mode | Keep basic command queue create/list/status/cancel/run with policy/audit/resource gates. Remove policy-run and CI-helper sprawl unless reintroduced by a later owner-approved ticket. |
| Watch diagnostics | Keep local/operator `sm watch` and authenticated/proofed watch diagnostics where useful. Do not expose public operational browser data. |
| App artifacts | Keep native app update artifacts, but require proof/auth or artifact signing before public serving. The current public unauthenticated artifact behavior is not the Rust target. |
| Service/cutover | Keep launchd/service packaging, migration backup/freeze/cutover/rollback, non-destructive process ownership, and operator diagnostics. |

## Removed Or Not Ported

These surfaces are out of scope for the Rust cutover:

| Surface | Decision |
| --- | --- |
| `sm dispatch` and dispatch templates/config | Do not port. Remove from first-release CLI. Existing dispatch ideas can be rebuilt later as explicit workflow tickets if still useful. |
| `sm what` | Do not port. It depends on low-capability summarization and is often wrong. Use `sm tail --raw`, `sm output`, or an explicit `sm send <id> "what are you doing now?"` status request instead. |
| `sm kill` CLI alias | Do not port. Use `sm retire` as the single CLI lifecycle command for ending a session. This does not retire retained HTTP/mobile session-stop APIs such as `POST /sessions/{session_id}/kill`; preserve them or retarget the native app through an explicit reviewed replacement. |
| Termux/SSH attach metadata and command generation | Do not port. Native mobile terminal is the supported on-the-go attach path. |
| Telegram bot commands, callbacks, topic cleanup, and remote control | Do not port. Telegram telemetry/topic state may be archived or migrated only for rollback/history. Native app replaces this remote-control surface. |
| Generic public browser/watch data | Do not expose public operational data at `sm.rajeshgo.li`. Local/authenticated/proofed diagnostics are allowed; unauthenticated public operational browser views are not. |
| `watch-job` and external job watches | Do not port. Retained state is empty and simple queue jobs cover the useful command-execution path. |
| Scheduled reminders and periodic remind APIs/CLI | Do not port as a standalone feature. Preserve parent wake, notify-on-stop, and direct queue delivery used by agent workflows. |
| Queue policy runs and CI helper subcommands | Do not port. Keep only the narrow queue runner scope above. |
| AI summary provider HTTP route and `sm what` summarizer | Do not port as public/sensitive read APIs or CLI commands. Prefer raw tail/output and explicit status prompts over lossy low-capability summarization. |
| Legacy provider aliases and retired entrypoints | Do not port `codex-server`, `codex-legacy`, or other inactive legacy aliases beyond clear retirement errors where needed. Keep active Codex app/fork/provider commands. |
| Implicit or non-proofed inbound email aliases | Do not carry forward the current ambiguity. Rust supports the default inbound email route and any explicitly configured allowlisted/proofed route only; non-default aliases without worker proof/service identity are rejected. |
| Public unauthenticated APK/artifact serving | Removed. App distribution must be proof/auth-gated or signed. |
| Node-agent hook-secret fallback | Removed. Registered nodes need explicit node credentials. |
| Broad local-bypass/proxy permissiveness | Tighten. Local bypass remains for same-user loopback operator workflows only; public/proxy paths need edge assertion and route auth. |

## Compatibility Rule

Implementation tickets must not file work to port a removed surface unless a later owner-approved issue explicitly reverses this scope decision. Removed Python behavior can remain available only through rollback during the migration window, not as a Rust compatibility target.
