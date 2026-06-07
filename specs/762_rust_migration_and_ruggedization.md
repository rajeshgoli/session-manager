# Rust Migration and Ruggedization Spec

Issue: #762
Status: Ready for user review after staged independent reviews converged and owner security feedback was incorporated
Owner: spec owner / sm-maintainer

## Stage 1: What And Why

Session Manager should be evaluated for migration from the current Python/FastAPI implementation to Rust, and migrated if the decision gate proves value, to improve runtime resource usage, responsiveness under multi-agent load, and operational robustness. Any migration must preserve the behaviors external actors already depend on while using the port as a deliberate opportunity to reduce attack surface and make trust boundaries explicit.

This is not a cosmetic rewrite. Session Manager is now core operator infrastructure: it owns agent lifecycle, tmux orchestration, message delivery, Telegram/mobile surfaces, durable queues, Codex review requests, hook ingestion, and local state. A Rust migration is justified only if it makes those responsibilities cheaper, faster, safer, and easier to reason about in production.

The spec must remain falsifiable. If the current Python implementation can meet the same memory, responsiveness, and security goals through targeted hardening at substantially lower risk, the final recommendation should narrow, pause, or reject a full Rust migration rather than treating the rewrite as inevitable.

## Motivation

### Memory Use

The Python server keeps long-lived process state around many active sessions, event monitors, queue workers, Telegram integrations, SQLite connections, and HTTP/WebSocket consumers. Rust should reduce baseline memory use and give tighter control over allocation patterns, background task ownership, and long-lived buffers.

Success requires measurement, not assumption. The migration spec must define current memory baselines and post-port acceptance thresholds before implementation work begins.

### Responsiveness

Session Manager is latency-sensitive in places that are easy to underestimate:

- `sm` CLI commands should return quickly because agents use them inside active work loops.
- `sm watch` should reflect activity changes without stale reducer state masking live work.
- queue delivery, reminders, review watches, and Telegram notifications should not be delayed by unrelated slow paths.
- restart/recovery should restore useful service quickly without losing durable intent.

Rust should improve responsiveness by making concurrency, cancellation, backpressure, and I/O ownership explicit. The spec must identify which operations need hard latency targets and which can be eventually consistent.

### Ruggedization

The current system has a large trusted surface: localhost HTTP, mobile HTTPS, tmux command execution, shelling out to CLIs, Telegram web APIs, SQLite state, hook payloads, control sockets, event streams, and files under user-writable directories. The Rust migration should include a threat model rather than merely porting the same trust assumptions into a faster binary.

The migration should reduce attack surface where possible. Where surfaces must remain, the spec must document the threats, mitigations, residual risk, and compatibility impact.

Security-sensitive surfaces include, at minimum, localhost/public auth split, mobile terminal WebSockets, Google/device auth, hook ingestion, tool-use logging, remote node transport, queue runner command execution, app artifact deployment/APK serving, email and human-recipient channels, Telegram bot callbacks, tmux shell control, Unix control sockets, event streams, config files, launchd/service installation, and writable local state.

Owner security direction after the staged reviews: the Rust migration is now the cutover port, not a full compatibility clone. Generic public `sm.rajeshgo.li` browser/watch data exposure, Telegram remote control, Termux attach, dispatch, watch-job, standalone reminders, queue policy/CI helper sprawl, public unauthenticated artifacts, and other low-value cruft are out of the first Rust release. The native `sm` mobile app and on-the-go attach flow remain the high-value remote surface. Email/human recipient delivery and inbound email remain the fallback external channel after Telegram removal, but must be ruggedized with worker proof, sender allowlists, explicit route allowlisting, and privacy-safe audit. Public remote access should use an edge or tunnel boundary that denies callers before they reach the Session Manager origin unless the phone can prove possession of a device credential issued from a trusted local enrollment path. Registered remote nodes, for example `macbook`, may use the same model as a fallback when they cannot reach `studio.local` over LAN, but only after proving possession of a node credential before the public edge forwards traffic. Device and node inventory/revocation, for example `sm list-devices`, `sm remove-device <id>`, and equivalent node-token rotation/removal, are required mitigations for that boundary.

## Falsifiable Decision Gate

Before implementation tickets are filed, the final spec must define:

- representative baseline workloads against the current Python system.
- memory, latency, startup, and recovery metrics to capture.
- target thresholds for the Rust implementation.
- a comparison against feasible Python hardening, configuration, or decomposition alternatives.
- a go/no-go rule for full migration, partial migration, or no migration.

The Rust migration should proceed only if the expected value survives this gate. If Stage 2 through Stage 5 reveal that compatibility, rollout, or security risk dominates the benefit, the spec should explicitly recommend a narrower path.

## Migration Shape

The preferred migration is staged and rollback-safe, but Stage 5 now defines a cutover scope rather than preserving every Python surface.

The Rust implementation should initially behave as a drop-in replacement for the externally observable behavior of all current entrypoints. The `sm` CLI is the primary operator interface, but it is not the only compatibility anchor.

The migration must preserve, or explicitly propose and get approval to break:

- CLI contract for existing `sm` commands.
- HTTP API contract used by CLI, watch UI, mobile clients, and integrations.
- WebSocket and long-poll/event-stream behavior.
- Telegram, email, human-recipient, Android/artifact, and bug-report behavior where enabled.
- hook script behavior, environment variables, config files, launchd/install scripts, logs, and operational commands.
- tmux session naming and attach behavior.
- Unix sockets, control sockets, event streams, and file artifacts.
- persisted state compatibility or a deliberate migration path.
- durable queue and review-watch behavior.
- human-readable status and error strings where agents, scripts, or users depend on them.
- audit logging semantics.

The spec should allow internal implementation to change aggressively as long as externally observable behavior remains compatible or an explicitly approved breaking change is documented.

## State Migration Safety Bar

The default expectation is that existing state is readable or safely migratable without destructive conversion.

Destructive or one-way migrations require:

- explicit user review.
- backups before migration.
- migration rehearsal on copied real state.
- rollback and downgrade behavior.
- observability for migration progress and failures.
- clear recovery instructions.

State inventory must include more than `sessions.json`: message queue DB, tool usage DB, response relay DB, Codex events DB, Codex request ledger, queue runner and policy state, bug reports DB, Telegram topics, config/local environment files, artifact metadata, logs, lock files, runtime sockets, event streams, and any state files under `~/.local/share/claude-sessions`, repo-local `.local`, or configured paths.

## Staged Spec Plan

### Stage 1: Scaffold

Document the what and why. Define the staged review process, migration goals, non-goals, quality bars, and acceptance themes. User review is required after independent spec review.

### Stage 2: Outward-Facing Surface

Inventory every externally visible surface:

- CLI commands and flags.
- HTTP endpoints and WebSocket endpoints.
- mobile/browser surfaces.
- Telegram commands, callbacks, and notifications.
- tmux sessions, panes, environment variables, hooks, control sockets, event streams, and file artifacts.
- SQLite/state files that external tools or older versions depend on.
- process behavior, ports, config files, logs, and operational commands.

For each surface, document inputs, outputs, errors, side effects, authentication/trust assumptions, compatibility expectations, and lightweight trust-boundary classification. The full Stage 4 threat model will expand these classifications, but Stage 2 must identify security-sensitive surfaces early enough that compatibility assumptions do not harden around unsafe behavior.

This inventory must be source-derived before it is hand-curated. Stage 2 must mechanically extract or enumerate:

- FastAPI route decorators and WebSocket routes from server code.
- CLI parser definitions, subcommands, flags, and aliases.
- Pydantic/request/response models and ad hoc response payloads.
- config keys and environment variables.
- persisted paths, SQLite schemas, JSON files, sockets, and event streams.
- hooks, scripts, install/launchd assets, and external commands.
- tests that encode observable behavior.

The source-derived inventory must then be reconciled with manual notes and reviewer findings.

Stage 2 must also identify which surfaces need executable contract fixtures against the Python implementation.

This stage does not need user review unless a proposed breaking change appears. It requires independent bar-raising review to convergence.

### Stage 3: Internal Behavior

Document the internal behavior that must be faithfully ported, including at minimum the list below plus any internal behavior discovered from Stage 2 inventory, background services, tests, and existing specs:

- session lifecycle and parent-child rules.
- provider-specific behavior for Claude, codex-fork, codex-app retirement paths, and remote nodes.
- message queue semantics, delivery modes, idle detection, waits, reminders, and wakeups.
- tmux orchestration, attach/restore behavior, and activity projection.
- Telegram topic mapping and notification rules.
- Codex event ingestion, reducer state, review request watching, and replay/recovery.
- persistence, startup reconciliation, schema compatibility, and failure recovery.

For each input, document the expected outputs and side effects external actors depend on. This stage requires independent bar-raising review to convergence.

### Stage 4: Ruggedization And Threat Model

Write the threat model:

- assets.
- actors.
- trust boundaries.
- entry points.
- abuse cases.
- attack-surface reduction opportunities.
- mitigations for surfaces that remain.
- residual risks.
- operational hardening requirements.

Any proposed breaking changes require user review. Otherwise this stage requires independent bar-raising review to convergence.

### Stage 5: Migration Execution And Rollout

Document the execution plan before implementation tickets are filed:

- Python/Rust coexistence boundary.
- server-only versus server-plus-CLI migration sequencing.
- compatibility shims, if any.
- dual-read/write, read-only migration, or one-way state migration decision.
- backup, rehearsal, downgrade, and rollback plan.
- launchd/service packaging and install/update behavior.
- operator rollout gates.
- kill switch or fast revert mechanism.
- cutover observability, health checks, and alerting.
- criteria for retiring Python components.

This stage requires independent bar-raising review to convergence and user review for any breaking change, destructive migration, or operational risk that cannot be fully mitigated.

## Contract Baseline Requirement

The final spec must require executable contract capture before port implementation begins. Contract fixtures should be captured against the existing Python implementation and used to verify Rust compatibility.

Required contract areas:

- CLI stdout/stderr, exit codes, option parsing, aliases, and error text where depended upon.
- HTTP status codes, response schemas, auth behavior, redirects, and error payloads.
- WebSocket frame sequencing, close behavior, and auth behavior.
- hook payload effects and audit logging.
- tmux session naming, environment variables, attach descriptors, and attach/restore semantics.
- durable queue delivery modes, wait behavior, wakeups, reminders, and review request watches.
- persisted schema compatibility and migration behavior.
- Telegram/email/mobile side effects where feasible to fixture or simulate.
- startup reconciliation and recovery behavior.

The contract harness is a first-class workstream, not a nice-to-have after the port.

## Review Process

Each stage gets independent spec review from independent Codex reviewers with explicit instructions to raise the bar, find missing surfaces, challenge assumptions, and reject shallow completeness.

Stage review exit criteria:

- Stage 1: one independent review pass, then user review.
- Stage 2: two to three independent review rounds, stopping when three reviewers give convergence signals or feedback degrades to nits only.
- Stage 3: two to three independent review rounds, same convergence rule.
- Stage 4: two to three independent review rounds, same convergence rule; user review is required for any breaking change.
- Stage 5: two to three independent review rounds, same convergence rule; user review is required for any breaking change, destructive migration, or operational risk that cannot be fully mitigated.

Reviewer feedback is not applied blindly. The spec owner must classify each substantive item as valid, invalid, or partially valid, then update the spec only after convergence.

Each review cycle must leave a durable review artifact in the working doc or a linked review log:

- reviewer identity.
- stage reviewed.
- findings by severity.
- spec-owner classification for each substantive finding.
- disposition after edits.
- unresolved dissent, if any.

After edits, reviewers must re-read the full updated stage, not just the diff. Repeated findings must be resolved by either changing the spec or explicitly documenting why the owner rejects them. Convergence means no unresolved P1/P2 findings remain and remaining P3 feedback is cosmetic or genuinely optional. If reviewers disagree on a substantive point, the spec owner records the disagreement and escalates to the user only when the decision affects product intent, breaking compatibility, or rollout risk.

## Success Criteria

The final spec is successful when implementation agents can use it to port Session Manager without rediscovering the system by archaeology.

Acceptance themes:

- External compatibility is clear enough to run contract tests captured from Python behavior.
- Internal behavior is clear enough to build state/recovery tests before rewriting the subsystem.
- Security posture is explicit enough to decide what to preserve, remove, gate, or harden.
- Migration risks are sequenced into implementable tickets.
- Breaking changes, if any, are explicit and user-reviewed.

## Non-Goals

- No Rust implementation work in this spec phase.
- No change to current production behavior while writing the spec.
- No broad refactor of the Python implementation as part of spec writing.
- No breaking-change decision without explicit user review.
- No assumption that Rust alone improves safety; safety improvements must come from reduced surface, explicit validation, tighter privileges, and tested failure behavior.
- No Rust framework, runtime, or library-stack decision during Stage 1. Framework choices must wait until outward surfaces, concurrency needs, persistence needs, and threat model constraints are documented.

## Current Assumptions

- Externally observable behavior of all current entrypoints remains the compatibility anchor; `sm` CLI behavior is the primary operator-facing subset of that anchor.
- Existing persisted state should be readable or migratable.
- The Rust service may coexist temporarily with Python components during migration if that reduces risk.
- The threat model should cover local single-user assumptions and remote/mobile exposure separately.
- The final work should be filed as an epic with sub-tickets rather than one implementation ticket.

## Stage-Gated Decisions

These are not blockers for Stage 1, but each must be resolved at the listed gate before implementation tickets are filed.

| Decision | Resolution Gate | Blocks Implementation Tickets? |
|----------|-----------------|-------------------------------|
| Should the first Rust release replace only the server, or server plus CLI? | Stage 5 | Yes |
| Which Python compatibility shims are acceptable during migration? | Stage 5 | Yes |
| What memory and latency baselines become acceptance criteria? | Falsifiable decision gate before Stage 5 completion | Yes |
| Which mobile, Telegram, email, artifact, and human-recipient surfaces are mandatory in the first Rust release? | Stage 5 cutover scope resolves this: native `sm` mobile app, on-the-go attach, email/human fallback delivery, inbound email webhook, proof/auth-gated app artifacts, Codex review/runtime, and core CLI/session surfaces are mandatory; Telegram, public browser data, dispatch, watch-job, standalone reminders, queue policy/CI helpers, and Termux are removed from the Rust target. | No |
| Which files and SQLite schemas are compatibility contracts versus private implementation details? | Stage 2 for external state, Stage 3 for internal state, finalized in Stage 5 | Yes |
| Should risky surfaces be disabled by default in Rust and enabled explicitly by config? | Stage 4; user review if breaking | Yes if breaking |
| What rollback/downgrade behavior is required after state migration? | Stage 5 | Yes |
| Which Python hardening alternatives are good enough to narrow or pause the Rust port? | Falsifiable decision gate | Yes |

## Stage 2: Outward-Facing Surface

Status: converged after three sequential independent reviewer convergence signals. Source-derived artifacts are linked and reconciled for Stage 2 handoff.

Stage 2 inventory was seeded from these sources:

- `src/server.py` FastAPI route decorators: 123 decorated HTTP/WebSocket route entries, plus dynamic `app.add_api_route` registrations and mounted static routes that must be counted separately.
- `src/cli/main.py` parser definitions: 73 parser entries including nested subcommands and aliases.
- `src/server.py` and `src/models.py` Pydantic/model classes: request, response, persistent model, and enum surfaces.
- `config.yaml.example`, `config/client.yaml.example`, `config/email_send.yaml.example`.
- Android client models and Retrofit interface under `android-app/`.
- watch UI TypeScript models and API consumers under `web/sm-watch/`.
- Cloudflare email worker example under `examples/cloudflare/`.
- Telegram bot command/callback registration in `src/telegram_bot.py`.
- `hooks/`, `scripts/`, `src/node_agent.py`, tmux controller/session-manager code, SQLite schema creators, and existing tests/specs.

This section inventories outward-facing behavior. It does not yet decide whether each surface should survive unchanged in Rust; that decision belongs to Stage 4 and Stage 5 when security and rollout costs are visible.

Stage 2 generated artifact bundle: [index](762_stage2_artifacts/index.md).

Stage 2 convergence depends on these generated artifacts being produced, linked from this spec or its review log, and reconciled against the prose inventory:

| Artifact | Required Coverage |
|----------|-------------------|
| [route manifest](762_stage2_artifacts/route_manifest.md) | Decorator routes, WebSocket routes, dynamic `app.add_api_route` registrations, mounted static routes, configured route aliases, response models, path/query/body params, and unmatched/reconciled rows. |
| [route auth matrix](762_stage2_artifacts/route_auth_matrix.md) | One row per route/path pattern with local bypass, public-host behavior, Google cookie/session auth, bearer device token support, hook/node/worker/device-key secret requirements, auth-exempt status, redirect-vs-JSON behavior, and auth-disabled/auth-incomplete outcomes. |
| [CLI manifest](762_stage2_artifacts/cli_manifest.md) | Command/subcommand tree, aliases, positional arity, flags, defaults, choices, suppressed/hidden flags, `argparse.REMAINDER`, stdin behavior, env requirements, stdout/stderr modes, and exit-code fixture plan. |
| [protocol manifest](762_stage2_artifacts/protocol_manifest.md) | Mobile terminal WebSocket, node-agent WebSocket, SSE/event stream, and watch static SPA behavior, including frame schemas, ordering, timeouts, close codes, and error frames. |
| [external-client manifest](762_stage2_artifacts/external_client_manifest.md) | Android app endpoint/header/model expectations, watch UI models and ad hoc fields, Cloudflare email worker payload/headers, Telegram commands/callbacks, and direct operator script entrypoints. |
| [config manifest](762_stage2_artifacts/config_manifest.md) | All example and code-read config keys, defaults, validation/coercion, env/local-env override precedence, secret/header names, and whether each key is compatibility contract or internal. |
| [persistence manifest](762_stage2_artifacts/persistence_manifest.md) | SQLite DDL, indexes, ALTER history, JSON fields/defaults, file paths, runtime sockets, log/artifact paths, and compatibility classification for every store Rust must read/write or migrate. |
| [schema manifest](762_stage2_artifacts/schema_manifest.md) | Pydantic/request/response/persistent models, ad hoc JSON payloads, enum values, permissive fields such as hook `extra = "allow"`, and client-consumed fields not represented by Pydantic models. |
| [usage telemetry report](762_stage2_artifacts/usage_telemetry_report.md) | Last-30-days and all-time usage observations, instrumentation gaps, and port-priority recommendations for commands/endpoints/surfaces. |

The generated manifests are Stage 2 artifacts, not implementation tickets. If any artifact cannot be generated mechanically, Stage 2 must document the manual extraction method and the residual risk.

### Usage Telemetry And Port Priority

Stage 2 must supplement the source-derived inventory with usage evidence. Telemetry is decision support, not automatic removal authority.

The usage report must cover at least the last 30 days and all retained history where available. Sources include `tool_usage.db`, `telegram_telemetry`, message queue registrations, Codex events and observability DBs, Codex review request registrations, queue runner DBs, server/access logs if available, app/client analytics, and git/test references when runtime telemetry is sparse.

For each command, endpoint, script, client flow, protocol, and persistent surface, Stage 2 should record:

- observed count in the last 30 days and all-time retained data.
- last observed use.
- number of distinct sessions/users/actors when available.
- whether telemetry is absent because the surface is uninstrumented, data is not retained, the local DB is empty, or the surface appears genuinely unused.
- equivalent command/API paths, if any.
- security risk and implementation cost.
- recommended port priority: first-class Rust port, compatibility shim, defer from first Rust release, or candidate remove/consolidate.

Sparse or missing telemetry must be recorded as `unknown` or `insufficient instrumentation` unless there is explicit evidence of non-use. Low or missing usage cannot filter the source-derived inventory or silently downgrade compatibility obligations. Any removal, consolidation, disabled-by-default change, or other compatibility break still requires the Stage 1 user-review path.

Owner priority overrides telemetry. The native `sm` mobile app and on-the-go attach flow are high-priority, first-class compatibility targets for the Rust migration even if some individual mobile endpoints show low observed volume. The generic public `sm.rajeshgo.li` browser surface is lower product value except where it supports auth, app distribution, watch diagnostics, or the mobile attach flow; returning operational data outside an auth/device-proof boundary is an owner-flagged reduction candidate, not a strategic compatibility goal.

### Surface Classification Dimensions

Stage 2 must classify each surface across separate dimensions rather than mixing intended actor with actual enforcement.

| Dimension | Values / Meaning |
|-----------|------------------|
| Actor | Local operator, managed agent, local process, authenticated remote, public remote, external service, native mobile app, watch UI, Telegram user, email worker, remote node. |
| Boundary | Loopback HTTP, public HTTP(S), WebSocket, CLI/stdin/stdout, tmux/process, SQLite/file state, provider CLI/API, external SaaS API, launchd/service, shell script. |
| Auth/enforcement | Local bypass, Google cookie/session, bearer device token, device-key signature, hook secret, node secret, email worker secret, Telegram allowlist, operator shell only, unauthenticated/exempt, disabled when auth incomplete. |
| Sensitivity | Read-only metadata, sensitive content read, state mutation, command execution, terminal I/O, credential-bearing, public artifact, destructive/external side effect. |
| Compatibility | Must preserve, first-class mobile app path, compatibility shim acceptable, candidate defer, candidate breaking change requiring user review. |

### HTTP And WebSocket Surface

All JSON request/response shapes must be captured as executable OpenAPI/Pydantic-derived fixtures before port implementation. The tables below name the path-level surface and summarize input/output contracts. Exact schemas are represented by request/response model names where present and by source-derived fixtures where responses are ad hoc dictionaries.

#### Route Auth Matrix Summary

The route auth matrix artifact must be route-by-route. This summary records current behavior that the Rust port must preserve or explicitly change through Stage 4/5 review.

| Current Behavior | Applies To | External Outcome |
|------------------|------------|------------------|
| Auth disabled or not requested | all HTTP routes | request proceeds without Google middleware auth checks; route-local checks still apply. |
| True local loopback bypass | all HTTP routes when request is from trusted local client/host and public-host denial of bypass does not apply | request proceeds even when Google auth is configured. |
| Auth-exempt HTTP paths | `/`, `/logged-out`, `/health`, `/health/detailed`, default `/api/email-inbound`, `/auth/google/login`, `/auth/google/callback`, `/auth/device/google`, `/auth/logout`, `/auth/session`, `/client/bootstrap`, `/apk`, `/apps/*` | request proceeds before Google cookie/bearer auth; route-local checks may still reject. |
| Google auth requested but incomplete | non-exempt external HTTP routes | JSON `503` with detail `Google auth is enabled but incomplete`. |
| Valid bearer device token | non-exempt HTTP routes | request proceeds with `request.state.device_auth`. |
| Valid browser session cookie | non-exempt HTTP routes | request proceeds as authenticated browser user. |
| Public `/watch` without auth | `/watch*` when Google auth is ready and request is not local/authenticated | `302` redirect to Google login. |
| Other public unauthenticated HTTP | non-exempt, non-watch paths | JSON `401` with `detail` and `login_url`. |
| Default inbound email webhook | `/api/email-inbound` | Google-auth exempt, then email bridge worker-secret and authorized-sender checks apply when configured. |
| Configured inbound email webhook alias | configured `email_bridge.webhook_path` if not `/api/email-inbound` | current behavior is verified in the artifacts: non-default aliases are dynamically registered but are not added to `GoogleAuthMiddleware.exempt_paths`, so public-host callers hit normal Google auth unless already authenticated or local. Treat this as a Stage 4/5 compatibility/security finding candidate, not intended behavior. |
| Hook routes | `/hooks/*` | Google auth follows the matrix above; hook-local secret/session behavior is separate and must be fixture-tested. |
| Node-agent WebSocket | `/nodes/agent` | first WebSocket frame must be `hello` with valid node secret; failures send an error frame and close. |
| Mobile terminal WebSocket | `/client/terminal` | WebSocket is accepted, then first frame must be mobile terminal `auth` with ticket/device proof; failures send an error frame and close with policy code. |

#### Health, Watch, And Event Stream

| Surface | Inputs | Outputs | Side Effects | Trust |
|---------|--------|---------|--------------|-------|
| `GET /` | request host/path | health JSON or watch redirect depending request context | none | Local operator / public remote when exposed |
| `GET /health` | none | `{status}` | none | Public remote readiness probe |
| `GET /health/detailed` | none | `HealthCheckResponse` | checks state/log/db resources | Auth-exempt; public details exposure if externally reachable |
| `GET /events/state` | none | event-stream state snapshot | none | Local operator/watch UI |
| `GET /events` | SSE client connection | server-sent event stream | long-lived connection | Local operator/watch UI, security-sensitive for metadata leakage |
| `GET /watch`, `GET /watch/{_path:path}` | browser path | static watch frontend or unavailable placeholder | none | Local operator/browser |

#### Auth, Mobile, Client, And App Artifacts

| Surface | Inputs | Outputs | Side Effects | Trust |
|---------|--------|---------|--------------|-------|
| `GET /auth/session` | session cookie | auth/session summary | none | Public remote with cookie handling |
| `GET /client/bootstrap` | none | `ClientBootstrapResponse` | exposes client config defaults | Auth-exempt bootstrap for mobile/native clients |
| `GET /client/analytics/summary` | none | mobile/client analytics summary | reads analytics state | Authenticated remote |
| `POST /auth/device/google` | `DeviceGoogleAuthRequest` | `DeviceGoogleAuthResponse` bearer token | validates Google ID token, mints device token | Public remote, security-sensitive |
| `GET /auth/google/login` | `next` query | OAuth redirect | writes session OAuth state | Public remote |
| `GET /auth/google/callback` | `state`, `code`, `error` | redirect/login result | validates OAuth, writes session cookie | Public remote, security-sensitive |
| `GET /auth/logout` | `next` query | redirect/logout | clears session auth | Authenticated remote |
| `GET /logged-out` | none | HTML landing | none | Public remote |
| `POST /deploy/{app_name}` | multipart APK plus `version_code`, `version_name` | `AppArtifactDeployResponse` | stores artifact, metadata, hashed copy | Authenticated remote/local, security-sensitive upload |
| `GET /apps/{app_name}/latest.apk` | app name | APK file | reads artifact | Public remote by design if exposed |
| `GET /apps/{app_name}/{artifact_hash}.apk` | app name/hash | APK file | reads artifact | Public remote by design if exposed |
| `GET /apps/{app_name}/meta.json` | app name | `AppArtifactMetadataResponse` | reads metadata | Public remote by design if exposed |
| `GET /apk` | none | legacy APK file redirect/download | reads artifact | Public remote by design if exposed |
| `GET /client/sessions` | auth/session context | client-filtered session list | none | Authenticated remote |
| `GET /client/sessions/{session_id}` | auth/session context, session id | client-safe session detail | none | Authenticated remote |
| `POST /client/request-status` | none | `ClientRequestStatusResponse` | asks live sessions to refresh status | Authenticated remote, mutating |
| `POST /client/bug-reports` | `ClientBugReportRequest` | `ClientBugReportResponse` | stores report, may notify maintainer | Authenticated remote, security-sensitive for data volume/content |
| `POST /client/sessions/{session_id}/attach-ticket` | session id, auth/device context | `MobileAttachTicketResponse` | creates short-lived attach ticket | Authenticated remote, security-sensitive |
| `GET /client/terminal` | browser request | upgrade-required/diagnostic response | none | Authenticated remote |
| `WEBSOCKET /client/terminal` | attach ticket, auth frame, terminal frames | bidirectional terminal frames | attaches to tmux session, streams terminal I/O | Authenticated remote, highly security-sensitive |
| `POST /client/mobile-terminal/disable` | auth/session context | `MobileTerminalDisableResponse` | disables/terminates mobile terminal attaches | Authenticated owner, security-sensitive |
| `GET /sessions/{session_id}/attach-descriptor` | session id | `{"attach": descriptor}` | exposes provider/tmux/mobile attach metadata; current Python may include deprecated Termux fields | Local/browser/mobile client, sensitive metadata |

#### Core Session Lifecycle

| Surface | Inputs | Outputs | Side Effects | Trust |
|---------|--------|---------|--------------|-------|
| `POST /sessions` | `CreateSessionRequest` | `SessionResponse` | creates tmux/provider runtime, persists session, starts monitor | Local operator/API client, security-sensitive |
| `POST /sessions/create` | form/query `working_dir`, `provider`, `parent_session_id`, `node` | session dict | legacy create path | Local operator/API client |
| `GET /sessions` | `include_stopped` | list of `SessionResponse` | may sync display name depending server internals | Local operator/watch/client |
| `GET /sessions/{session_id}` | session id | `SessionResponse` | none | Local operator/client |
| `PATCH /sessions/{session_id}` | friendly name, EM flag | `SessionResponse` | updates persisted session/display state | Local operator/managed agent |
| `DELETE /sessions/{session_id}` | session id | deletion/kill result | stops runtime, updates state, may notify | Local operator, security-sensitive |
| `POST /sessions/{session_id}/restore` | session id | `SessionResponse` | restarts/restores runtime and monitors | Local operator, security-sensitive |
| `POST /sessions/{session_id}/open` | session id | terminal open result | invokes terminal/open behavior | Local operator/local process |
| `POST /sessions/{session_id}/fork` | `ForkSessionRequest` | fork result/session | creates forked provider/runtime session | Local operator/managed agent, security-sensitive |
| `POST /sessions/spawn` | `SpawnChildRequest` | child session result | creates child session, optional wait/track | Managed agent/local operator, security-sensitive |
| `POST /sessions/{target_session_id}/kill` | `KillSessionRequest` | kill result | permission-checked kill/retire | Managed agent/local operator |

#### Session Metadata, Registry, Roles, And Context

| Surface | Inputs | Outputs | Side Effects | Trust |
|---------|--------|---------|--------------|-------|
| `PUT /sessions/{session_id}/role` | `SetRoleRequest` | `SessionResponse` | sets role | Managed agent/local operator |
| `DELETE /sessions/{session_id}/role` | session id | `SessionResponse` | clears role | Managed agent/local operator |
| `PUT /sessions/{session_id}/maintainer` | `SetMaintainerRequest` | `SessionResponse` | marks maintainer | Managed agent/local operator |
| `DELETE /sessions/{session_id}/maintainer` | `SetMaintainerRequest` | `SessionResponse` | clears maintainer | Managed agent/local operator |
| `POST /maintainer/ensure` | `EnsureMaintainerRequest` | `EnsureMaintainerResponse` | ensures maintainer alias/session | Managed agent/local operator |
| `POST /registry/{role}/ensure` | `EnsureRoleRequest` | `EnsureMaintainerResponse` | ensures role registration | Managed agent/local operator |
| `GET /registry` | none | role registry list | none | Local operator |
| `GET /registry/{role}` | role | `AgentRegistrationResponse` | none | Local operator |
| `POST /sessions/{session_id}/registry` | `RoleRegistrationRequest` | `AgentRegistrationResponse` | registers durable role | Managed agent/local operator |
| `DELETE /sessions/{session_id}/registry` | `RoleRegistrationRequest` | `AgentRegistrationResponse` | unregisters durable role | Managed agent/local operator |
| `POST /sessions/{session_id}/context-monitor` | `ContextMonitorRequest` | status result | enables/disables context monitor | Managed agent/local operator |
| `GET /sessions/context-monitor` | none | context monitor status | none | Local operator |
| `POST /sessions/{session_id}/notify-on-stop` | `ArmStopNotifyRequest` | arm result | arms EM/parent stop notification without queued message | Managed EM/local operator, mutating notification behavior |
| `PUT /sessions/{session_id}/task` | task text | result | updates current task | Managed agent/local operator |
| `POST /sessions/{session_id}/agent-status` | `AgentStatusRequest` | result | updates self-reported status | Managed agent |

#### Session Input, Output, And Control

| Surface | Inputs | Outputs | Side Effects | Trust |
|---------|--------|---------|--------------|-------|
| `POST /sessions/{session_id}/input` | `SendInputRequest` | delivery result | queues/sends text to provider/tmux/control API | Managed agent/local operator, highly security-sensitive |
| `POST /sessions/input-batch` | batch request | `SendInputBatchResponse` | resolves multiple recipients, may send to sessions/humans/email | Managed agent/local operator, security-sensitive |
| `POST /sessions/{session_id}/key` | key string | result | sends tmux key | Local operator, security-sensitive |
| `POST /sessions/{session_id}/clear` | `ClearSessionRequest` | result | sends `/clear`, arms skip fences | Managed agent/local operator |
| `POST /sessions/{session_id}/invalidate-cache` | `arm_skip` | result | invalidates cached session/provider state | Local operator |
| `GET /sessions/{session_id}/output` | `lines` | captured pane output | reads terminal scrollback | Local operator, sensitive data exposure |
| `GET /sessions/{session_id}/tool-calls` | `limit` | tool call rows | reads audit DB | Local operator, sensitive metadata |
| `GET /sessions/{session_id}/last-message` | session id | last assistant output | reads relay/hook output | Local operator, sensitive content |
| `GET /sessions/{session_id}/summary` | `lines` | generated summary | may call summarizer/provider | Local operator, potentially slow |
| `POST /sessions/{target_session_id}/watch` | watcher id, timeout | watch registration | wakes watcher when target idle | Managed agent |
| `GET /sessions/{session_id}/send-queue` | session id | queue state | none | Local operator/managed agent |

#### Codex, Reviews, And Provider-Specific State

| Surface | Inputs | Outputs | Side Effects | Trust |
|---------|--------|---------|--------------|-------|
| `GET /sessions/{session_id}/codex-events` | `since_seq`, `limit` | event list | reads codex event store | Local operator/client, sensitive content |
| `GET /sessions/{session_id}/activity-actions` | `limit` | activity/action list | reads action projection | Local operator/client |
| `GET /sessions/{session_id}/codex-pending-requests` | `include_orphaned` | pending request list | reads request ledger | Local operator/client |
| `POST /sessions/{session_id}/codex-requests/{request_id}/respond` | `CodexRequestRespondRequest` | response result | answers Codex request | Local operator, security-sensitive |
| `GET /sessions/{session_id}/review-results` | session id | parsed review result | reads provider output/GitHub review | Local operator |
| `POST /sessions/{session_id}/review` | `StartReviewRequest` | review start result | starts review in existing session | Local operator/managed agent |
| `POST /sessions/review` | `SpawnReviewRequest` | spawned review result | creates review session | Local operator/managed agent |
| `POST /reviews/pr` | `PRReviewRequest` | PR review result | starts/request PR review | Local operator/managed agent |
| `POST /codex-review-requests` | `CodexReviewRequestCreateRequest` | `CodexReviewRequestResponse` | comments on GitHub and tracks review | Managed agent/local operator, external service |
| `GET /codex-review-requests` | filters | review request list | none | Managed agent/local operator |
| `GET /codex-review-requests/{request_id}` | request id | `CodexReviewRequestResponse` | none | Managed agent/local operator |
| `DELETE /codex-review-requests/{request_id}` | request id | `CodexReviewRequestResponse` | cancels durable request | Managed agent/local operator |
| `GET /admin/rollout-flags` | none | rollout flag state | none | Local operator |
| `GET /admin/codex-fork-runtime` | none | runtime metadata | none | Local operator |
| `GET /admin/codex-launch-gates` | none | gate status | none | Local operator |

#### Subagents, Children, Handoff, Adoption, Completion

| Surface | Inputs | Outputs | Side Effects | Trust |
|---------|--------|---------|--------------|-------|
| `POST /sessions/{session_id}/subagents` | `SubagentStartRequest` | `SubagentResponse` | records subagent start | Hook/managed agent |
| `POST /sessions/{session_id}/subagents/{agent_id}/stop` | `SubagentStopRequest` | result | records subagent stop | Hook/managed agent |
| `GET /sessions/{session_id}/subagents` | session id | subagent list | none | Local operator/managed agent |
| `GET /sessions/{parent_session_id}/children` | `recursive`, `status`, `include_terminated` | child list | none | Local operator/managed agent |
| `POST /sessions/{session_id}/handoff` | `HandoffRequest` | result | schedules handoff/clear/wakeup | Managed agent |
| `POST /sessions/{target_session_id}/adoption-proposals` | `CreateAdoptionProposalRequest` | adoption proposal | creates proposal visible in watch UI | Managed agent/local operator |
| `POST /adoption-proposals/{proposal_id}/accept` | proposal id | result | mutates ownership/parenting | Local operator |
| `POST /adoption-proposals/{proposal_id}/reject` | proposal id | result | marks proposal rejected | Local operator |
| `POST /sessions/{session_id}/task-complete` | `TaskCompleteRequest` | result | cancels reminders, notifies EM/parent | Managed agent |
| `POST /sessions/{session_id}/turn-complete` | `TaskCompleteRequest` | result | cancels periodic remind only | Managed agent |

#### Scheduler, Queue Runner, Job Watches

| Surface | Inputs | Outputs | Side Effects | Trust |
|---------|--------|---------|--------------|-------|
| `POST /scheduler/remind` | session id, message, delay, recurrence | reminder result | schedules reminder | Managed agent/local operator |
| `DELETE /scheduler/remind/{reminder_id}` | reminder id | cancel result | cancels reminder | Managed agent/local operator |
| `POST /sessions/{session_id}/remind` | `PeriodicRemindRequest` | result | registers periodic remind | Managed agent/local operator |
| `DELETE /sessions/{session_id}/remind` | session id | result | cancels remind | Managed agent/local operator |
| `POST /job-watches` | `JobWatchCreateRequest` | `JobWatchResponse` | registers file/PID watch and notifications | Managed agent/local operator |
| `GET /job-watches` | filters | job watch list | none | Managed agent/local operator |
| `DELETE /job-watches/{watch_id}` | watch id | `JobWatchResponse` | cancels job watch | Managed agent/local operator |
| `POST /queue-jobs` | `QueueJobCreateRequest` | `QueueJobResponse` | writes script, starts/queues local command | Managed agent/local operator, highly security-sensitive |
| `GET /queue-jobs` | filters | queue job list | none | Managed agent/local operator |
| `GET /queue-jobs/{job_id}` | job id | `QueueJobResponse` | none | Managed agent/local operator |
| `DELETE /queue-jobs/{job_id}` | job id | `QueueJobResponse` | cancels process/job | Managed agent/local operator |
| `POST /queue-policy-runs` | `QueuePolicyRunCreateRequest` | `QueuePolicyRunResponse` | admission decision plus optional queue job | Managed agent/local operator, security-sensitive |
| `GET /queue-policy-runs` | policy, limit, include suppressed | policy run list | none | Managed agent/local operator |
| `GET /queue-policy-runs/status` | policy/dedupe/id | `QueuePolicyRunResponse` | none | Managed agent/local operator |
| `GET /queue-policy-runs/{run_id}` | run id | `QueuePolicyRunResponse` | none | Managed agent/local operator |

#### Nodes, Hooks, Notifications, Humans, Email

| Surface | Inputs | Outputs | Side Effects | Trust |
|---------|--------|---------|--------------|-------|
| `GET /nodes` | none | node registry/config list | none | Local operator |
| `POST /nodes/{node_id}/ping` | node id | ping result | remote node check | Local operator/remote node |
| `GET /nodes/{node_id}/restore-candidates` | node id, refresh | restore candidate list | may refresh remote inventory | Local operator |
| `POST /nodes/{node_id}/restore-candidates/{session_id}/restore` | node/session ids | `SessionResponse` | restores remote candidate | Local operator, security-sensitive |
| `WEBSOCKET /nodes/agent` | node secret/auth frames | node agent protocol frames | remote node control/data path | Authenticated remote, highly security-sensitive |
| `POST /notify` | `NotifyRequest` | delivery result | sends notification to configured channels | Managed agent/local operator |
| `GET /humans` | none | human recipient list | none | Managed agent/local operator |
| `GET /humans/{identifier}` | identifier | `HumanRecipientResponse` | none | Managed agent/local operator |
| `POST /humans/{identifier}/telegram` | `HumanDeliveryRequest` | delivery result | sends Telegram to human | Managed agent/local operator, external service, sensitive content mutation |
| `POST /humans/{identifier}/email` | `HumanDeliveryRequest` | delivery result | sends email to human | Managed agent/local operator, external service, sensitive content mutation |
| `POST /email/send` | `SendEmailRequest` | delivery result | sends registered email | Managed agent/local operator, external service, sensitive content mutation |
| `POST /api/email-inbound` and configured email webhook alias | `InboundEmailRequest`, worker-secret/header, authorized sender, optional trusted session-id header | sent/ignored/error result | may restore stopped session and deliver sequential input from email reply | External email worker, auth-sensitive mutating ingress |
| `POST /hooks/claude` | Claude hook JSON | hook result | updates session completion/idle/cache/output | Managed agent hook, security-sensitive |
| `POST /hooks/tool-use` | tool hook JSON | hook result | logs tool use, may evaluate lock/worktree policy | Managed agent hook, security-sensitive |
| `POST /hooks/context-usage` | context hook JSON | hook result | records context warnings/reset events | Managed agent hook |
| `POST /hooks/tmux-client` | query params event/session/client/tty/pid | hook result | records tmux client attach/switch events | Local process hook |
| `POST /admin/cleanup-idle-topics` | request auth/context | cleanup result | deletes/archives Telegram topics | Local operator/admin, external service |

### CLI Surface

The CLI is both an operator interface and an agent scripting interface. Rust compatibility must preserve command names, aliases, positional argument order, option names, exit codes, stdout/stderr shape where depended upon, and network timeout behavior.

#### Top-Level Commands

Source-derived top-level parser entries:

`dispatch`, `name`, `role`, `me`, `who`, `nodes`, `node`, `what`, `others`, `all`, `alone`, `task`, `lock`, `unlock`, `status`, `subagent-start`, `subagent-stop`, `subagents`, `send`, `telegram` alias `tg`, `email`, `remind`, `wait`, `watch-job`, `queue`, `request-codex-review`, `spawn`, `children`, `kill`, `retire`, `restore` alias `unkill`, `fork`, `clean`, `claude`, `codex`, `codex-legacy`, `codex-fork` alias `codex_fork`, `codex-2`, `codex-app`, `codex-server`, `new`, `attach`, `output`, `codex-tui`, `codex-fork-info`, `codex-rollout-gates`, `watch`, `tail`, `clear`, `handoff`, `task-complete`, `turn-complete`, `context-monitor`, `em`, `maintainer`, `register`, `unregister`, `lookup`, `roster`, `adopt`, `setup`, `review`.

#### CLI Families

| Family | Commands | Inputs | Outputs | Side Effects / Trust |
|--------|----------|--------|---------|----------------------|
| Identity/status | `me`, `who`, `what`, `others`, `all`, `alone`, `status`, `task`, `name`, `role`, `maintainer`, `register`, `unregister`, `lookup`, `roster`, `em` | session ids, names, role strings, status text, `--json` where present | human-readable tables/status, JSON for selected commands, exit codes | local/managed agent; many commands require `SESSION_MANAGER_ID` in Rust, with `CLAUDE_SESSION_MANAGER_ID` as a legacy compatibility alias during migration |
| Session lifecycle | `claude`, `codex`, `codex-legacy`, `codex-fork`, `codex-2`, `codex-app`, `codex-server`, `new`, `spawn`, `attach`, `restore`, `fork`, `kill`, `retire`, `clean` | working dirs, provider, node, model, name, wait/track, attach/json flags | session creation/attach output, errors | starts/kills tmux/provider processes; security-sensitive |
| Messaging/control | `send`, `telegram`/`tg`, `email`, `wait`, `clear`, `output`, `tail`, `codex-tui`, `codex-fork-info`, `codex-rollout-gates`, `watch` | target ids, text, delivery mode flags, line counts, raw/db path, filters | delivery results, terminal attach/TUI, tables, watch UI | sends input, reads output/logs, attaches terminal; security-sensitive |
| Coordination | `children`, `subagents`, `subagent-start`, `subagent-stop`, `handoff`, `task-complete`, `turn-complete`, `context-monitor`, `adopt`, `dispatch`, `setup` | session ids, files, action names, templates | status/tables/errors | mutates state, schedules clears/wakes, installs templates |
| Queues and watches | `remind`, `watch-job add/list/cancel`, `queue run/list/status/cancel/ci-run/ci-status/ci-history`, `request-codex-review` | job argv/scripts/env/cwd, policy, dedupe tokens, regexes, PR repo/number | job ids, statuses, JSON, notifications | executes commands, watches files/PIDs, posts GitHub comments; highly security-sensitive |
| Locks | `lock`, `unlock` | description | lock result/errors | local worktree lock files/state |
| Reviews | `review` | session, base, commit, PR, repo, new/model/wait/steer | review session/request output | starts Codex review flows |

#### CLI Manifest Requirements

The CLI family table is only a summary. Stage 2 must include or link a generated CLI manifest before convergence.

The manifest must cover:

- nested commands such as `node ping`, `watch-job add/list/cancel`, `queue run/list/status/cancel/ci-run/ci-status/ci-history`, and `request-codex-review list/status/cancel/<pr>`.
- aliases such as `telegram`/`tg`, `restore`/`unkill`, and `codex-fork`/`codex_fork`.
- positional argument arity and ordering.
- defaults, choices, repeatable flags, hidden/suppressed flags, and output mode flags.
- special parsing behavior: `sm send` option normalization, `sm send -` stdin read/error cases, `sm email` stdin/body/file exclusivity, queue `argparse.REMAINDER`, script-file `-`, and operator-only `sm watch`.
- environment-required commands and exact missing-env errors.
- stdout/stderr and exit-code fixtures for success, validation errors, server unavailable, auth/session missing, and removed/retired provider paths.

Known high-risk parser contracts include `send --urgent/--important/--steer/--wait/--track/--track-seconds/--no-notify-on-stop`, `spawn --track/--track-seconds`, `children --db-path`, `tail --raw --db-path`, queue `--script-file -`, and all JSON output modes.

#### CLI Environment Inputs

| Variable | Used By | Compatibility Meaning |
|----------|---------|-----------------------|
| `SESSION_MANAGER_ID` | CLI, hooks, tmux-launched sessions | Canonical Rust-era managed session identity; required for self/agent-scoped mutations. Rust should export and prefer this name. |
| `CLAUDE_SESSION_MANAGER_ID` | CLI, hooks, tmux-launched sessions | Legacy Python/current-source compatibility alias for the same managed session id. Rust should accept it during migration and should define a retirement gate before removing it. |
| `SM_API_URL` | CLI/node agent/hooks | base URL override. Must reject non-http(s) where current client does. |
| `SM_CLIENT_CONFIG` | CLI | client config path override. |
| `SM_DEFAULT_NODE`, `SM_LOCAL_NODE` | CLI | default execution node and local node attach routing. |
| `SM_API_TIMEOUT`, `SM_SEND_API_TIMEOUT`, `SM_MUTATION_API_TIMEOUT` | CLI | request timeout behavior. |
| `SM_HOOK_BASE_URL`, `SM_HOOK_URL`, `SM_TOOL_USE_HOOK_URL`, `SM_HOOK_SECRET` | hook scripts | hook destination and shared secret header. |
| `SM_NODE_TOKEN`, `SM_NODE_LOG_DIR`, `SM_NODE_STATE_FILE`, `SM_NODE_CONTROL_TIMEOUT` | node agent | remote node agent auth/state/control behavior. |
| `SM_TMUX_SESSION`, `SM_TMUX_SOCKET` | deprecated Termux attach command | current Python shell-snippet targets; Rust must not port Termux attach. |
| terminal color vars (`TERM_PROGRAM`, `COLORTERM`, `CLICOLOR`, `FORCE_COLOR`) | tmux/session launch | inherited display/color behavior. |
| Google/cloudflared env (`PUBLIC_HTTP_HOST`, `PUBLIC_SSH_HOST`, `HTTP_ORIGIN_URL`, `SSH_USERNAME`, `SSH_PROXY_COMMAND`, `GOOGLE_*`, `ALLOWLIST_EMAIL`, `SESSION_COOKIE_SECRET`) | config local-env loader | external access/auth defaults. |
| email/human recipient env such as configured `address_env` | human recipient email delivery | resolves private email addresses outside repo config. |

### Config Surface

Config files are outward-facing because operators edit them and Rust must preserve defaults, validation, and local-env override behavior.

| File | Keys / Purpose |
|------|----------------|
| `config.yaml` / `config.yaml.example` | `server`, `paths`, `monitor`, `worktree_cleanup`, `tmux`, `timeouts`, `telegram`, `email`, `services`, `external_access`, `mobile_terminal`, `auth.google`, `claude`, `codex`, `codex_app_server`, `codex_rollout`, `codex_fork`, `codex_events`, `codex_requests`, `codex_observability`, `response_relay`, `sm_send`, `tool_logging`, `watchdog`, `queue_runner`, `nodes`, `service_roles`, `maintainer_agent`, `service_role_maintenance`, `codex_fork_runtime_maintenance`, `infra_supervisor`, `mobile_analytics`, `child_agents`, `remind`, `dispatch`, and `sessions`. |
| `config/client.yaml` | client `api_url`, `default_node`, `local_node`. |
| `config/email_send.yaml` | Resend API/domain, human recipients, users, email bridge authorized senders, worker secret/header, session-id header, webhook path. |
| local env file loaded by `src/main.py` | external access, Google auth, allowlist, cookie secret. |
| `.sm/dispatch_templates.yaml` / `src/cli/default_dispatch_templates.yaml` | dispatch template names and prompts. |

Contract tests must include config parsing defaults, invalid URL behavior, missing-secret behavior, path expansion, enum/coercion behavior, secret/header-name defaults, and env/local-env override precedence. The generated config manifest must classify `nodes` and remote placement settings as a security-sensitive trust-boundary contract.

### Hook, Script, And Install Surface

| Surface | Inputs | Outputs / Side Effects | Trust |
|---------|--------|------------------------|-------|
| `hooks/notify_server.sh` | Claude hook JSON on stdin plus env | POSTs `/hooks/claude`, may include `SESSION_MANAGER_ID` or legacy `CLAUDE_SESSION_MANAGER_ID`, short timeouts | Managed agent hook |
| `hooks/log_tool_use.sh` | tool-use hook JSON on stdin plus env | POSTs `/hooks/tool-use`, adds session id, optional secret header | Managed agent hook |
| `scripts/install_notify_server_hook.sh` | local paths/config | installs hook into provider settings | Local operator |
| `scripts/install_context_hooks.sh` | context hook events/env | installs or emits context usage/reset/compaction hook POSTs | Local operator/managed hook |
| `scripts/install-service.sh`, `scripts/com.claude.session-manager.plist`, `scripts/session-manager-wrapper.sh`, `run.sh`, `setup.sh` | local filesystem/env | install/start launchd/service wrapper | Local process, operationally sensitive |
| `scripts/deploy_android_app.sh` | APK path/config | uploads artifact to deploy endpoint | Local operator/authenticated remote |
| `scripts/cleanup_orphan_forum_topics_mtproto.py`, `scripts/cleanup_duplicate_topics.py` | Telegram config/session state | deletes or reconciles Telegram topics | External service, destructive |
| `scripts/codex_fork/release_artifacts.sh` | Codex repo path/ref | builds/publishes codex-fork artifacts | Local operator/external GitHub |
| `claude-session-manager` / `python -m src.main` | config/env/current directory | starts FastAPI server with lifespan services | Local operator/service |
| `sm-node-agent` / `src.node_agent` | `--node-id`, `--primary-url`, `--secret`, `--log-dir`, `--state-file`, `--poll-interval`, `--control-timeout`, `--log-level`, `SM_NODE_*` env | long-running remote node WebSocket bridge; stderr/exit code on failure | Remote node/local process, security-sensitive |

Rust migration can replace script internals only if command-line shape, environment variables, timeout behavior, and failure messages remain compatible or are explicitly broken with user approval.

### Tmux, Process, And Terminal Surface

| Surface | Inputs | Outputs / Side Effects | Trust |
|---------|--------|------------------------|-------|
| tmux socket name | `tmux.socket_name`, attach descriptors, `SM_TMUX_SOCKET` | sessions run under configured socket, currently often `session-manager` | Local process |
| tmux session names | provider plus 8-char session id (`claude-*`, `codex-fork-*`) | attach/restore scripts and users depend on stable names | Local process |
| tmux panes/titles/status bar | provider TUI title, friendly names, activity spinner | watch/activity projection and human attach UX | Local process |
| tmux hooks/client events | `/hooks/tmux-client` event/session/client/tty/pid | activity/revival/client state | Local process |
| provider launch commands | `claude.command`, `claude.args`, `codex.command`, `codex.args`, model/node/working dir | starts external CLIs with env exports | Highly security-sensitive |
| mobile terminal attach | WebSocket to tmux attach bridge | bidirectional terminal frames | Authenticated remote, highly security-sensitive |
| terminal open command | local terminal integration | opens attach session | Local operator |

### Persistence And File Artifact Surface

The Rust implementation must either read/write these stores compatibly or provide a user-approved migration path.

| Store | Default / Source | External Contract |
|-------|------------------|-------------------|
| session state JSON | `~/.local/share/claude-sessions/sessions.json`, legacy `/tmp/claude-sessions/sessions.json` | session records, provider ids, tmux names, Telegram threads, role/maintainer/context state. |
| logs | `/tmp/claude-sessions`, `logs/`, configured `paths.log_dir` | operator debugging and audit context. |
| message queue DB | `~/.local/share/claude-sessions/message_queue.db` | `message_queue`, `scheduled_reminders`, `remind_registrations`, `parent_wake_registrations`, `job_watch_registrations`, `codex_review_request_registrations`. |
| tool usage DB | `~/.local/share/claude-sessions/tool_usage.db` | `tool_usage`, `telegram_telemetry` audit rows and indexes. |
| response relay DB | `~/.local/share/claude-sessions/response_relay.db` | `inbound_turns`, `assistant_outputs`. |
| codex events DB | `~/.local/share/claude-sessions/codex_events.db` | `codex_session_events`, `codex_assistant_relays`, `codex_fork_provider_cursors`, `codex_fork_provider_event_positions`. |
| codex observability DB | `~/.local/share/claude-sessions/codex_observability.db` | `codex_tool_events`, `codex_turn_events`. |
| codex requests DB | `~/.local/share/claude-sessions/codex_requests.db` | `codex_pending_requests`; externally visible through pending-request routes and response APIs, so DDL/index/history extraction belongs in Stage 2. |
| queue runner state | `~/.local/share/claude-sessions/queue-runner`, `queue_runner.db`, `policy_runs.db`, per-job directories, `submitted.zsh`, logs | `queue_jobs`, `queue_resource_samples`, `queue_policy_runs`, `queue_policy_results`, job scripts/logs. |
| bug reports DB | `data/bug_reports.db` by default unless configured | `bug_reports`, `bug_report_attachments`. |
| Telegram topics | `~/.local/share/claude-sessions/telegram_topics.json` | durable mapping for forum topics and cleanup. |
| app artifacts | configured artifacts root or default data path | latest APK, hashed APKs, metadata JSON. |
| codex-fork runtime artifacts | `/tmp/claude-sessions/*.codex-fork.events.jsonl`, `*.control.sock` | event stream/control socket paths used by runtime maintenance, attach descriptors, and recovery. |
| lock/worktree state | lock manager files and git worktrees | operator safety and dirty-worktree warnings. |

### External Service Surface

| Service | Inputs | Outputs / Side Effects | Trust |
|---------|--------|------------------------|-------|
| Telegram Bot API | bot token, chat/thread ids, forum topic state | messages, topic creation/rename/delete, bot commands/menu | External service, security-sensitive content |
| Google OAuth / ID token verification | client ids/secrets, redirect URI, allowlist | session cookie or device access token | External service auth |
| GitHub via `gh`/API | repo, PR number, review steer | PR comments, review request tracking, issue/PR metadata | External service, mutating |
| Resend/email/IMAP bridge | API key/domain/users/authorized senders | outbound email, inbound worker delivery | External service, sensitive content |
| cloudflared / external SSH | public hosts, proxy command | legacy Termux attach metadata only | External service/network boundary; deprecated and excluded from the Rust target unless a later owner-reviewed ticket reinstates it. |
| provider CLIs (`claude`, `codex`) | command args/env/stdin/tmux | live agent sessions, event streams, control sockets | External process, highly security-sensitive |
| remote node agents | node secret, WebSocket protocol, SSH/local node config | remote session lifecycle/restore/control | Authenticated remote, highly security-sensitive |

### External Client And Protocol Surface

Server routes are not the whole outward surface. Stage 2 must capture the contracts enforced by external clients and long-lived protocols.

| Client / Protocol | Source Inputs | Contract To Preserve |
|-------------------|---------------|----------------------|
| native Android `sm` app | `android-app/.../ApiService.kt`, `ApiModels.kt`, `WatchViewModel.kt` | endpoints under `client/*`, `auth/*`, `apps/*`, `sessions/*`; bearer auth; `X-SM-Device-Key-Id`, `X-SM-Device-Timestamp`, `X-SM-Device-Nonce`, `X-SM-Device-Signature`; attach-ticket creation; mobile terminal WebSocket auth and frame handling; fields including `attach_descriptor`, `mobile_terminal`, and `primary_action`. This is a high-priority first-class Rust target. The current `termux_attach` field is legacy compatibility evidence only and must not be ported as an attach path. |
| watch UI | `web/sm-watch/src/types.ts`, `App.tsx`, `watchModel.ts`, component code | session/activity fields, adoption proposals, attach descriptors, activity/action rows, tool calls, tail/output payloads, `/events`/state behavior, and `/watch` static SPA behavior. Generic public browser use is lower product value than mobile app attach, but existing watch diagnostics remain compatibility surfaces. |
| mobile terminal WebSocket | `src/server.py`, Android terminal view model/assets | first frame must be `auth`; auth timeout; origin checks; ticket/device proof fields; client frames `resize`, `input`, `key`, `ping`, `detach`; server frames `output`, `status`, `error`, `exit`; base64/history modes; input size limits; resize bounds; max attach timeout; close codes and error strings. |
| node-agent WebSocket | `src/server.py`, `src/node_agent.py`, `src/codex_fork_remote.py` | first frame `hello` with node id/secret; frames `hello_ok`, `register`, `registered`, `register_failed`, `unregister`, `event`, `event_gap`, `control`, `control_result`, `restore_inventory`, `restore_inventory_result`, `error`; timeout/reconnect/disconnect behavior; unknown-frame handling. |
| SSE/event stream | `GET /events`, `GET /events/state`, watch UI consumers | initial state behavior, event names, event ordering, reconnect behavior, long-lived connection errors, and metadata leakage expectations. |
| static watch and app artifacts | `WatchStaticFiles`, `/watch` mount/fallback, `/apps/*`, `/apk` | SPA history fallback, missing-build `503` JSON, root redirect, HTML cache behavior, APK file names, content types, hash validation, metadata JSON, and public-artifact auth exemptions. |
| Cloudflare email worker | `examples/cloudflare/email_worker_id_routing.js`, email bridge config | POST payload `raw_email` and `from_address`; headers `x-email-worker-secret` and `x-email-session-id`; worker-secret failures; authorized sender failures; missing routing footer behavior; stopped-session restore and sequential delivery side effects. |
| Telegram bot | `src/telegram_bot.py` | commands `/start`, `/help`, `/new`, `/session`, `/list`, `/status`, `/subagents`, `/message`, `/summary`, `/kill`, `/stop`, `/force`, `/open`, `/name`, `/password`, `/follow`; callback prefixes `new_project:`, `follow:`, `perm:`; regular-message routing; allowlists; topic creation/rename/delete; telemetry and notification side effects. |

Protocol appendices or linked generated fixtures are required before Stage 2 convergence for mobile terminal, node-agent, SSE/events, and static file behavior.

### Request/Response Schema Surface

The following model classes are externally visible through HTTP responses, CLI JSON, persisted state, or hook payload semantics and require schema fixtures or migration tests:

- API request/response models: `CreateSessionRequest`, `ForkSessionRequest`, `SessionResponse`, `AgentRegistrationResponse`, `EnsureMaintainerResponse`, `ClientBootstrapResponse`, `DeviceGoogleAuthRequest`, `DeviceGoogleAuthResponse`, `MobileAttachTicketResponse`, `MobileTerminalDisableResponse`, `AppArtifactMetadataResponse`, `AppArtifactDeployResponse`, `ClientRequestStatusResponse`, `ClientBugReportRequest`, `ClientBugReportResponse`, `SendInputRequest`, `SendInputBatchRequest`, `SendInputBatchResponse`, `SendInputBatchResult`, `CodexRequestRespondRequest`, `PeriodicRemindRequest`, `ArmStopNotifyRequest`, `InboundEmailRequest`, `JobWatchCreateRequest`, `JobWatchResponse`, `QueueJobCreateRequest`, `QueueJobResponse`, `QueuePolicyRunCreateRequest`, `QueuePolicyRunResponse`, `CodexReviewRequestCreateRequest`, `CodexReviewRequestResponse`, `AdoptionProposalResponse`, `AgentStatusRequest`, role/maintainer/registry requests, `NotifyRequest`, `SendEmailRequest`, `HumanRecipientResponse`, `HumanDeliveryRequest`, `HookPayload`, hook/subagent/spawn/review/context request models, and health models including `HealthCheckResult`.
- Persistent/session models: `Session`, `Subagent`, `ReviewConfig`, `ReviewResult`, `ReviewFinding`, `AgentRegistration`, `TelegramTopicRecord`, `AdoptionProposal`, `QueuedMessage`, `RemindRegistration`, `ParentWakeRegistration`, `JobWatchRegistration`, `CodexReviewRequestRegistration`, `SessionDeliveryState`, `MonitorState`.
- Enums: `SessionStatus`, `DeliveryMode`, `DeliveryResult`, `NotificationChannel`, `SubagentStatus`, `CompletionStatus`, `ActivityState`, `AdoptionProposalStatus`.

Stage 2 reviewers should decide whether field-level schema tables belong directly in this spec or in generated linked artifacts. The compatibility requirement is non-negotiable: Rust must either match these fields and defaults or document an approved breaking change. `HookPayload` permissive extra-field handling is part of the hook compatibility contract.

### Human-Readable Output And Error Surface

Human-readable strings are outward-facing because agents parse and react to them. Contract capture must include common success/error text for:

- `SESSION_MANAGER_ID` missing errors, including legacy `CLAUDE_SESSION_MANAGER_ID` alias compatibility during migration.
- `sm send`, `sm wait`, `sm request-codex-review`, `sm watch-job`, `sm queue`, and lifecycle command outputs.
- provider retirement/cutover errors such as removed `codex-server` entrypoint behavior.
- auth failures and mobile terminal rejection reasons.
- hook failures that intentionally return success to avoid blocking providers.
- queue/job/review request status text.

### Candidate Breaking-Change Review Triggers

Stage 2 should flag safer-default candidates early even though Stage 4/5 decide them. Any accepted break requires user review.

| Surface | Why It May Need Change | Current Constraint |
|---------|------------------------|--------------------|
| auth-exempt `/health/detailed` | may expose resource/log/db health details on public host | current middleware exempts it. |
| auth-exempt `/client/bootstrap` | exposes client auth/access configuration before login | native mobile app depends on bootstrap being reachable. |
| generic public `sm.rajeshgo.li` data exposure | public browser/watch access can reveal operational state outside the remote surface that is actually high value | owner preference is to remove or reduce public data exposure unless it is inside an auth/device-proof boundary. |
| public tunnel/origin boundary | letting Cloudflare or another tunnel forward unauthenticated callers to the origin keeps the whole origin middleware exposed to internet traffic | owner preference is to move proof-of-possession before the origin where feasible, with trusted-LAN mobile enrollment, node credential proof for Cloudflare fallback when LAN `studio.local` is unavailable, and device/node revocation. |
| public APK/app artifact downloads | public files can leak internal app builds if host is exposed | Android update/install flow may depend on current public behavior. |
| inbound email webhook | public-before-Google-auth mutating ingress; default path and configured alias may differ | Cloudflare worker and email bridge depend on worker-secret/sender checks. |
| hook routes | local hooks may be unauthenticated under local bypass or rely on hook secret behavior | provider hooks must not block managed sessions unexpectedly. |
| mobile terminal attach | remote terminal I/O is high value and high risk | owner has marked native mobile app and on-the-go attach as high-priority. |
| Termux/cloudflared attach fallback | crosses public network/SSH/proxy boundary | owner has deprecated it; Rust should not port this path, and native mobile attach should depend on the mobile terminal path or an owner-approved replacement. |
| queue runner arbitrary command execution | managed agents can request local command execution | existing queue workflows and policy runs depend on it. |
| node-agent shared-secret WebSocket | remote node control depends on shared secret framing | remote codex-fork/session restore behavior depends on it. |
| Telegram destructive commands/topic cleanup | external bot commands can mutate sessions/topics through Telegram servers, outside the native app device-proof model | existing workflows may depend on command vocabulary, but owner preference is to consider deprecation or scoping in favor of the native app. |

### Stage 2 Contract Capture Requirements

Before Rust implementation:

- Generate a route manifest from decorators and compare it to this section.
- Generate a route auth matrix from middleware plus route-local auth/secret checks and compare it to this section.
- Generate a CLI manifest from parser definitions and compare it to this section.
- Generate config, persistence, schema, external-client, and protocol manifests described above.
- Generate a telemetry/usage report and port-priority matrix; mark missing instrumentation separately from observed non-use.
- Snapshot request/response schemas for all Pydantic models and representative ad hoc JSON responses.
- Capture route-level HTTP fixtures for normal and negative cases: path/query/body validation, status codes, content types, headers, redirects, auth errors, 400/401/403/404/409/410/422/503 behavior, codex rollout-gate errors, codex-app retirement payloads, and upload/download headers.
- Capture golden CLI outputs and exit codes for common success, validation error, auth/session missing, and unavailable-server cases.
- Capture WebSocket/SSE/static protocol fixtures for node agent, mobile terminal, event stream, and watch static behavior, including happy path, auth failure, timeout, unknown frame, close/error, reconnect, and missing-build cases.
- Capture hook POST payload behavior with and without `SESSION_MANAGER_ID`, legacy `CLAUDE_SESSION_MANAGER_ID`, and hook secret.
- Capture persisted DB schemas and JSON field defaults from a copied real state directory.

### Stage 2 Completion Checklist For Reviewers

Reviewers should block convergence if any of these remain unresolved:

- every decorator route, dynamic route, static mount, and configured route alias is present in the route manifest with reconciliation status.
- route auth matrix distinguishes current behavior from intended safer behavior, especially the default versus configured email webhook path.
- mobile terminal, node-agent, SSE, and static watch/app artifact protocols have appendices or generated fixtures.
- Android app, watch UI, Cloudflare email worker, Telegram bot, `sm-node-agent`, and direct scripts are represented as external actors.
- config and persistence manifests include exact keys, defaults, override precedence, DDL, indexes, ALTER history, file paths, and compatibility/private classifications.
- schema manifest includes all request/response/persistent models and ad hoc client-consumed fields.
- telemetry/usage report exists and does not filter the source-derived inventory.
- tests/specs are searched for additional output strings or side effects not listed here.

## Stage 3: Internal Behavior To Port Faithfully

Status: converged after three sequential independent reviewer convergence signals.

Stage 3 inventories internal behavior that outward actors depend on indirectly. Rust may replace the internal architecture, but it must preserve these observable state transitions, side effects, recovery semantics, timing assumptions, and failure modes unless a later Stage 4/5 breaking-change decision is explicitly user-reviewed.

Stage 3 includes current Python behavior for surfaces that Stage 5 later removes from the Rust cutover, such as Telegram, Termux attach, standalone reminders, external job watches, policy-run helpers, and generic public browser data. Those rows remain source-traceability and rollback evidence only. Email/human recipient delivery and inbound email are retained fallback surfaces, not removed surfaces.

Stage 3 source pass used:

- `src/main.py` startup/shutdown orchestration, lifespan hooks, Telegram reconciliation, event-loop watchdog, and local-env config loading.
- `src/session_manager.py` state hydration, session lifecycle, provider launch/restore/kill, role registry, service-role maintenance, Codex/codex-fork reducers, activity projection, response relay, and attach descriptors.
- `src/tmux_controller.py` tmux launch, socket/hook setup, shell preparation, input injection, submit verification, provider-native rename, capture, and kill/open behavior.
- `src/output_monitor.py` log tailing, output pattern detection, activity state, crash recovery trigger, dead-pane cleanup, and output-throughput state.
- `src/message_queue.py` durable delivery, idle/active state, sequential/important/urgent modes, reminders, parent wakeups, job watches, Codex review watches, response relay, Telegram mirroring, and recovery.
- `src/response_relay.py` provider-agnostic inbound-turn ledger, assistant-output claim/dedupe, release/retry, and relayed-marker semantics.
- `src/tool_logger.py` and `src/lock_manager.py` audit logging, destructive/sensitive classification, repository locks, and worktree safety side effects.
- `src/codex_event_store.py`, `src/codex_request_ledger.py`, and `src/codex_observability_logger.py` durable event/request/observability semantics.
- `src/node_runner.py` and `src/node_agent.py` remote execution, remote restore inventory, codex-fork event/control bridging, and node reachability.
- `src/queue_runner.py` queued command execution, admission policy, recovery, cancellation, resource sampling, and notifications.
- `src/mobile_analytics.py`, `src/telegram_bot.py`, `src/email_handler.py`, `src/bug_report_store.py`, and `src/server.py` integration side effects that are produced by internal flows.

### Stage 3 Completion Artifacts

Before Stage 3 converges, the spec or linked artifacts must contain:

- source-derived behavior matrix for the subsystems below, including source references and reconciliation notes.
- startup/recovery sequence diagrams or ordered tables for server restart, session restore, queue recovery, codex-fork recovery, and queue-runner recovery.
- input-to-side-effect tables for `send_input`, Stop hook idle transitions, tmux death, Codex request approval, inbound email delivery, queue runner job lifecycle, and mobile attach.
- state-transition tables for `SessionStatus`, `ActivityState`, delivery state, codex-fork lifecycle state, Codex pending request state, queue jobs, reminders, parent wakeups, Telegram topic records, and bug-report status.
- contract-test candidates for each behavior where an external actor depends on a timing window, output string, persisted row, Telegram/email side effect, or activity projection.

Stage 3 traceability artifacts:

| Artifact | Purpose |
|----------|---------|
| [index](762_stage3_artifacts/index.md) | Stage 3 artifact bundle entry point and reviewer handoff. |
| [source traceability](762_stage3_artifacts/source_traceability.md) | Source/test references and reconciliation notes for the high-risk Stage 3 behavior matrices. |
| [ordered recovery](762_stage3_artifacts/ordered_recovery.md) | Ordered server restart, session restore, queue recovery, codex-fork recovery, and queue-runner recovery handoff tables. |
| [state transitions](762_stage3_artifacts/state_transitions.md) | Source-anchored state-transition tables for the required Stage 3 state machines. |

### Startup, Shutdown, And Background Service Ownership

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| process start | Load config, apply local-env overlay, build managers, then run `infra_supervisor.start()` before the current Python port preflight. The preflight then probes the server port; if it fails, Python returns before starting child monitor, message queue, SessionManager background tasks, Telegram bot, lifespan restoration/reconciliation, tmux hooks, or uvicorn. | Current compatibility: mobile/Android sidecar repair may run even when another server already owns the port, but Telegram, queue, monitor, restore, and tmux-hook side effects are withheld on doomed instances. | Preserve this current behavior unless Stage 4/5 explicitly approves moving infra repair after port preflight as a hardening change. |
| ASGI lifespan startup | Reconcile Telegram topics and restore output monitoring after the web app starts. Tmux client hooks are installed only after uvicorn has actually bound the listener. | Prevents tmux hooks from pointing at an unavailable server; live sessions regain monitoring and watch/activity state after restart. | Preserve post-bind tmux-hook installation and lifespan restoration ordering. |
| server shutdown | Stop infra supervisor, Telegram bot, queue manager, monitors, background maintenance tasks, codex sessions, and uvicorn-owned tasks. | Prevents duplicate queue tasks, stuck Telegram polling, leaked WebSockets, and stale codex-fork monitors after restart. | Shutdown must cancel tasks idempotently and leave durable state readable. |
| event-loop watchdog | Background thread monitors loop health using configured cadence/timeout. | Operator diagnostics and crash-loop debugging depend on watchdog logs, not request failure semantics. | Preserve diagnostic behavior or explicitly replace it with equivalent Rust health instrumentation. |

### Persistent State Hydration And Atomic Saves

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| state load | Load configured `sessions.json`; if default path is missing or unreadable and legacy `/tmp/claude-sessions/sessions.json` exists, fall back to legacy path. | Existing installs keep sessions after path migration. | Preserve fallback and clear critical logging/error behavior for unreadable state. |
| state hydrate | Drop legacy codex app sessions with app-thread-only state, hydrate `Session` objects, cache node mapping, backfill Telegram topic registry, restore stopped sessions as records, and preserve remote-node sessions when nodes are unreachable. | CLI/watch can list stopped sessions; remote-node outages do not destroy state; Telegram topics stay mapped. | Preserve hydrate decisions before any Rust schema migration. |
| live tmux check during hydrate | Non-stopped local tmux-backed sessions are retained only if tmux exists; dead sessions become orphaned-topic cleanup candidates. Remote-node failures preserve state and mark node unreachable. Codex-fork stopped sessions may be healed to idle when detached runtime artifacts remain reachable. | `sm watch`, restore, Telegram cleanup, and node status reflect runtime reality without deleting unreachable remote work. | Preserve local/remote distinction and codex-fork runtime healing semantics. |
| atomic state save | Build full state snapshot, write to per-thread temp file, and atomically replace target. Async saves snapshot on the event loop then write off-loop. | Prevents partial JSON state under concurrent async tasks. | Rust writes must be atomic, serialized, and backward-compatible with old fields/defaults. |
| registry and adoption hydrate | Restore EM topic, maintainer id, agent registrations, role last-session ids, and adoption proposals; recover missing maintainer registration and prune dead registrations. | `sm maintainer`, role lookup, adoption APIs, and agents relying on role aliases remain stable after restart. | Preserve role normalization, maintainer backfill, pruning, and last-session fallback rules. |

### Session Lifecycle And Provider Launch

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| create session | Resolve target node from explicit arg, parent inheritance, and node defaults. Reject unknown nodes, unsupported provider/node combinations, unavailable providers, or remote codex-fork without required bridge capability. | CLI/API errors prevent partially-created sessions and preserve remote placement expectations. | Preserve rejection ordering enough that errors remain actionable and no state is written after rejection. |
| tmux-backed provider create | Create `Session`, derive name/tmux session/log path, detect git remote, build provider command/args/model, create tmux session, set history/socket/env, pipe logs, start monitor later, persist state, and maybe create Telegram topic. | `sm all`, attach, logs, Telegram topic creation, role/parent metadata, and provider runtime all become visible together. | Rust may own orchestration, but must preserve session naming, log paths, runtime env, and persistence-before-side-effect boundaries. |
| `claude` provider | Uses configured command/args/default model; optional initial prompt is passed through tmux launch path; initial prompt is recorded in response relay for direct response tracking. | Claude sessions support resume, response relay, Stop hooks, and crash recovery. | Preserve response relay inbound recording for initial prompts and Claude-specific launch behavior. |
| `codex` provider | Uses configured CLI command/args/default model; Codex has no Stop hook, so queue delivery and idle reconcile use prompt polling. | Plain Codex sessions still receive queued messages and return to idle in watch/UI. | Preserve prompt-based idle reconcile or replace with equivalent contract-tested activity tracking. |
| `codex-fork` provider | Build managed args, prepare event/control artifacts, register remote bridge when needed, fall back to legacy codex only for allowed local non-fork creates, start event monitor, initialize lifecycle state, and own runtime artifacts. | Codex-fork sessions show correct active/idle/wait states, can be controlled remotely, and do not silently degrade on remote nodes. | Preserve fallback restrictions, artifact naming, monitor startup, and lifecycle initialization. |
| `codex-app` provider | Start app-server session without tmux, retain thread id, register callbacks for turn/request/item/stream events, mark no-prompt sessions idle, and use app-server restore paths. | Retired app-server paths remain represented in state/API; no tmux assumptions leak into app-server sessions. | Preserve app-server retirement semantics until Stage 4/5 decides whether this surface remains. |
| Telegram topic ensure | Session creation can create Telegram forum topic synchronously or defer it for spawn paths; topic name uses effective display name plus short id and saves immediately after creation. | Telegram topic routing works even when HTTP response returns before Telegram API completes. | Preserve lock-per-session, immediate persistence after topic creation, and deferred path behavior. |

### Parent-Child, Roles, Maintainer, And Adoption

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| spawn child | Parent id, prompt, model/provider/node, friendly name, tracking options, and optional wait behavior create a child session with parent metadata and durable spawn prompt. | Parent-child hierarchy, `sm children`, `sm dispatch`, Telegram topics, track/remind, and parent wakeups depend on persisted lineage. | Preserve parent inheritance, node inheritance, spawn timestamps, and tracking enrollment. |
| role registration | Normalize roles, synchronize maintainer alias, update role last-session map, reparent live children when maintainer changes, and persist registrations. | `sm maintainer`, `sm role`, service-role routing, and adoption flows depend on role lookup behavior. | Preserve role normalization, maintainer special case, live-registration pruning, and last-session fallback. |
| service role maintenance | Background loop periodically ensures auto-bootstrapped service roles using configured provider priority, working dir, prompt templates, and task-complete TTL. | Maintainer/chief-scientist service agents revive without manual action and expire task-complete state correctly. | Preserve provider fallback ordering, bootstrap prompt rendering, locks, and reap behavior. |
| adoption proposal | Creating a proposal rejects duplicate live conflicts; deciding a proposal marks it accepted/rejected and rejects competing pending proposals for same target. | EM/adoption UI and CLI see one coherent pending ownership decision. | Preserve proposal statuses, conflict handling, and persistence order. |

### Message Delivery, Idle State, Reminders, And Wakeups

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| `send_input` common path | Validate session exists/not stopped, optionally detect role from prompt, clear completed-task state when external sender wakes agent, format sender metadata, and route by delivery mode. | Agents see `[Input from: name (id) via sm send]`; CLI sees delivered/queued/failed; task-complete state clears on new work. | Preserve sender formatting, completion-state clearing, and result semantics. |
| sequential/important delivery | Persist message first, optionally record outgoing target, then try immediate delivery for a bounded control wait. Return `DELIVERED` only if DB row was marked delivered; otherwise leave queued for background delivery. | CLI/API users can trust "queued" versus "delivered"; restart does not lose intent. | Preserve persist-before-inject and bounded wait behavior. |
| urgent delivery | Persist urgent message, mark active unless paused, interrupt provider. Claude backgrounds live task, waits for prompt, sends Escape, then payload; non-Claude tmux providers send Escape; codex-app uses app-server urgent path. | Urgent `sm send` interrupts active work without message reordering. | Preserve provider-specific wake/interrupt ordering and per-session delivery lock. |
| steer delivery | Only Codex CLI/codex-fork support steer mode; it bypasses queue and drives provider-specific Enter-based text. | PR-review steering and codex workflows depend on immediate direct behavior. | Preserve provider gating and failure behavior. |
| queued message delivery | Use per-session lock, skip paused sessions, drop expired messages, final-gate on user-typed input unless saved, batch up to max size, isolate native rename control messages, inject, mark rows delivered, record response relay, mirror to Telegram, and start remind/parent-wake. | Prevents double delivery, preserves user input, keeps Telegram/requester side effects in sync with actual injection. | Preserve locking, final input gate, batching rules, native-rename isolation, and side-effect order after successful injection. |
| stale user input | Monitor pending tmux composer input; if unchanged beyond timeout, save it, clear input, deliver queued messages, and restore saved input after response. | Human/operator typed text is not overwritten by agent messages. | Preserve stale-input detection and restore behavior for tmux-backed providers. |
| Stop hook / idle transition | Stop hooks can execute pending handoff first, absorb `/clear` skip fences within configured time window, cancel remind/parent wake only on genuine completion, mark idle, handle stop notifications, promote paste-buffered notify state, and trigger queued delivery. | Parent wakeups, `notify-on-stop`, handoff, and queued messages fire at the right turn boundary. | Preserve skip-fence timing, paste-buffered two-phase notification, and handoff precedence. |
| active transition | Mark delivery state active, cancel delayed stop notification, set non-stopped session status to running, and cancel Codex idle reconcile. | `sm watch` and parent notifications stop treating active agents as idle. | Preserve active-state repair behavior. |
| reminders | Scheduled reminders persist, fire urgent messages, support recurring reschedule, wait through compaction up to configured cap, and deactivate when target is not runnable. | `sm remind` survives restart and wakes the correct agent. | Preserve reminder ids, active/fired state, compaction wait, and recurring semantics. |
| tracked remind | Soft threshold sends important nudge; hard threshold sends urgent overdue message and resets cycle; tracked sessions can nudge target and notify owners; replies cancel or refresh tracking depending persistent mode. | Dispatch tracking and EM workflows depend on cadence and exact target/owner routing. | Preserve soft/hard cadence, target-facing status nudge, owner-facing reminders, and cancel-on-reply behavior. |
| parent wake | Durable parent wake sends periodic digest to parent with duration, status age, no-progress detection, and last tool events; escalates cadence when status does not advance. | EM sees long-running child progress without polling. | Preserve digest content structure, escalation, cancellation on child completion/death, and recovery. |
| startup recovery | Queue manager recovers scheduled reminders, remind registrations, parent wake registrations, job watches, Codex review watches, then marks sessions with pending messages idle to trigger delivery. | Restarted server resumes durable intent instead of losing queued work. | Preserve recovery order enough that registrations exist before pending delivery begins. |

### Durable Review Watches And External Job Watches

Message queue also owns long-running watches that wake agents later. These are externally visible through CLI/API commands and through queued notification messages.

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| Codex review request create | Resolve repo from explicit arg or requester working dir, validate notify/requester sessions, serialize duplicate creation with `(repo, pr, notify_session)` lock, reject active duplicates, validate open PR, post request comment, refresh latest comment metadata, persist registration with requested state, `attempt_count`, `next_retry_at`, requester/notify ids, and comment fields, then start poll task. | `sm request-codex-review` creates at most one active watch per PR/notify target and records comment URL/id for status/retry. | Preserve duplicate lock, validation order, persisted fields, and active duplicate error. |
| review pickup polling | Poll on configured interval; if latest request comment exists and pickup not detected, detect Codex pickup by reaction and persist `pickup_detected_at/source`; pickup failures set `last_error` but do not skip review polling. | Status surfaces show pickup separately from completion, and transient pickup errors do not hide landed reviews. | Preserve independent pickup/review checks and `last_error` behavior. |
| review landed polling | Find fresh Codex review/comment after request timestamp; when found, update the in-memory registration with landed/completed/inactive fields, queue `[sm review] Codex comment for PR #... is here. <url>` or `[sm review] Codex review for PR #... is here. <url>`, then persist the terminal review-watch update and stop task. | Notify session wakes exactly once with factual review URL, and current Python avoids making the durable watch disappear before the wake message is queued. | Preserve source-specific noun, URL inclusion, completed/inactive transition, single notification, and queue-before-terminal-watch-persist ordering. A stronger transactional redesign belongs in Stage 4/5 ruggedization. |
| review retry | When no pickup/review has landed and `next_retry_at` passes, repost/refresh request comment, increment `attempt_count`, update latest comment fields, and set next retry. Failures persist `last_error` and continue watching. | Long-running watches keep nudging Codex and survive transient GitHub failures. | Preserve retry cadence, attempt count, latest comment metadata, and continued polling after failure. |
| review cancel/recovery | Cancel marks inactive with state/last_error and cancels task; missing notify session auto-cancels; inactive history remains listable with include-inactive; startup recovers active registrations only. | Operators can inspect past requests and do not get watches for gone agents. | Preserve inactive-history persistence, auto-cancel, and active-only recovery. |
| job watch create | Validate target session, require pid/file/exit-code signal, require pid or regex/exit-code rule, validate positive intervals/tail values, compile regexes, expand paths, capture initial file offset, persist active registration, and start poll task. | `sm watch-job` does not alert on stale pre-existing log lines and rejects unusable watches. | Preserve validation, regex errors, initial offset, and path expansion. |
| job watch evaluation | On each poll read appended lines since last offset, check pid, read exit code when pid ended/missing, then choose event precedence: error regex, exit-code complete/error, done regex, progress regex, pid-exited/no-signal, none. | Notification text and deactivation match current behavior. | Preserve event precedence and delivery modes. |
| job watch progress | Persist last file offset, last progress text, last event/notified timestamp. Progress notifies on changed text, and current `notify_on_change=False` behavior still returns progress notifications even when text repeats. | Progress watches avoid stale-line spam but preserve current notify-on-change compatibility. | Preserve last-progress dedupe and current `notify_on_change` semantics unless user-approved. |
| job watch notify/deactivate/recovery | Queue `[sm job-watch] ...` notification text before persisting last-event/last-notified/progress-offset updates or terminal deactivation, using important delivery for terminal/error states and sequential delivery for progress. Error/completed/exited events deactivate only after queuing notification; cancellation marks inactive; startup restores active watches only if target session exists, otherwise marks inactive. | Restart does not lose active watches or wake missing targets, and agents receive stable job-watch message strings. Current Python avoids making a terminal watch inactive before the wake message is queued. | Preserve notification prefix/content, delivery mode, queue-before-watch-persist ordering, deactivate/update ordering after queueing, and skipped recovery behavior. A stronger transactional redesign belongs in Stage 4/5 ruggedization. |

Review/job-watch Stage 3 contract tests must include review duplicate serialization, pickup failure still checking reviews, retry persistence when comment refresh fails, completion notification text, queue failure/crash-window ordering before terminal completion persistence, missing notify-session auto-cancel, inactive-history list, startup active-only recovery, job-watch stale-line suppression, regex validation, event precedence, progress dedupe, `notify_on_change` compatibility, exit-code completion/error, PID-exited fallback, queue-before-deactivate ordering, and target-missing recovery skip.

### Tmux, Process, And Output Monitoring

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| tmux session create | Ensure server anchor/options, create bootstrap then main window, set history limit, enable exit diagnostics, initialize pane title, pipe pane to log, export managed shell env, wait settle time, and send provider command. | Attach, logs, history, hook environment, and dead-pane diagnostics work consistently. | Preserve tmux socket/session/window naming, history, pipe-pane, env exports, and exit diagnostics. |
| input injection | Exit copy mode, split large text into chunks, send literal text, wait adaptive settle delay, send Enter separately, optionally verify Claude/Codex submit, clear partial input on error. | Prevents paste-mode failures and partial prompt corruption. | Preserve settle/chunk/Enter separation and failure cleanup semantics. |
| provider-native rename | Queue/drive Claude or Codex native rename only for safe friendly names; failed native rename messages are marked delivered to avoid infinite retries. | Display names converge without injecting unsafe slash commands repeatedly. | Preserve safety checks, category isolation, and drop-on-failed-rename behavior. |
| output monitor start | Start from current log end, keep restored-session notification grace, mark remote node unreachable on read failures, and maintain per-session monitor state. | Restart does not spam old output and remote outages surface as node-unreachable. | Preserve initial offset and remote failure handling. |
| output pattern analysis | Detect crash first, then permission prompts, errors, and completions. Permission marks status idle and may notify; completion marks idle and flushes deferred crash recovery; errors do not mark failed. | Watch/Telegram/status reflect waiting-permission/idle without treating arbitrary error text as session failure. | Preserve pattern priority and status side effects. |
| dead runtime detection | Every monitor cycle group checks tmux existence and exit diagnostics; dead panes produce diagnostic error text and cleanup with stopped record preserved. | Dead sessions remain visible/restorable with useful error message instead of disappearing. | Preserve stopped-record preservation and diagnostic formatting. |
| crash recovery | Claude-only. Pause queue, kill or gracefully exit harness, parse resume id, reset terminal if needed, resume with `--resume`, then unpause queue. Running sessions defer recovery until idle. | Queued messages do not go to a shell during harness restart; agents recover without losing conversation. | Preserve queue pause/unpause, graceful versus forced path, resume parsing, and debounce behavior. |

### Mobile Terminal Attach Internals

The native mobile app and on-the-go attach flow are first-class Rust targets. The Stage 2 WebSocket protocol is not enough; Rust must also preserve the internal ticket, quota, bridge, and kill-switch state machine.

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| server startup | Initialize in-memory mobile terminal ticket map, active attach map, async lock, secret, runtime-disabled flag, and revoked device-key set. | Existing tickets are process-local and vanish on restart; active attach count starts empty. | Preserve process-local ticket semantics unless Stage 5 approves durable tickets. |
| attach-ticket request | Require mobile terminal enabled, non-stopped attachable session, visible allowed user, `interactive_shell_access`, registered device key, valid timestamp/nonce/signature over the ticket-request message, supported tmux metadata, and valid tmux target. | Mobile app gets a short-lived ticket only after both user and device prove authorization. | Preserve auth order, error statuses/text, and audit-denied reasons. |
| pending ticket replacement | While holding the mobile terminal lock, cleanup expired tickets and remove any unconsumed ticket for the same user/device before minting a new one. | Retrying attach-ticket does not strand old pending tickets that consume quota. | Preserve pending-ticket replacement and single-use ticket ids/secrets. |
| quota checks at mint | Count active attaches plus pending tickets against global, per-user, and per-session limits before minting. | Mobile app receives `429` before it can create excess attach attempts. | Preserve quota dimensions and inclusive active+pending accounting. |
| ticket consume | First WebSocket frame must be `auth`; consume validates ticket id/secret hash, device id match, expiry/consumed state, active-only quotas, current user/device authorization, fresh device signature over the WebSocket auth message, current session attachability, and metadata support. Ticket is removed and active attach record is inserted atomically under the lock. | Tickets are single-use and revalidated at consume time, so user/device revocation and session stop take effect before terminal access. | Preserve consume-time reauthorization, active-only quota checks, and atomic remove/active-insert behavior. |
| WebSocket auth failure | Timeout, missing/invalid auth frame, invalid origin, disabled runtime, invalid ticket, revoked device, or unsupported session sends an error frame and closes with policy code. | Android/WebView can distinguish auth failure from terminal exit. | Preserve first-frame timeout, error frame strings, and close behavior. |
| PTY/tmux bridge start | After auth, create bridge to the validated tmux target, preload configured history lines, apply initial resize grace, register stop event/websocket in active attach record, and audit `attach_started`. | Mobile app receives existing scrollback and live terminal I/O for the intended session. | Preserve history preload limits, initial resize behavior, active attach metadata, and audit event ordering. |
| terminal frames | Client resize/input/key/ping/detach frames drive tmux/PTY bridge; server emits output/status/error/exit frames, enforces input/resize bounds, and propagates termination. | Mobile terminal UX remains interactive, bounded, and debuggable. | Preserve resize/input/key semantics, ping/status handling, output framing, error strings, and max attach lifetime. |
| bridge cleanup | On disconnect, exit, error, timeout, or stop event, remove active attach, close WebSocket if needed, cleanup PTY/subprocess resources, and audit attach termination. | Quotas free promptly and future attach attempts are not blocked by leaked active records. | Preserve cleanup in `finally` paths and resource teardown even on client disconnect. |
| runtime disable | Authorized owner can set runtime-disabled, clear all tickets, clear active attach records, set stop events, send exit frame with `mobile_terminal_disabled`, close sockets, and return terminated count. | Emergency kill switch immediately terminates mobile terminal access without restarting SM. | Preserve disable authorization, ticket clearing, active attach teardown, response count, and audit event. |

Mobile terminal Stage 3 contract tests must include ticket mint denial reasons, pending-ticket replacement, ticket expiry/consume once, quota enforcement at mint and consume, user/device revocation between mint and consume, disabled runtime before mint and during active attach, origin rejection, auth timeout, history preload, resize/input/output, detach/exit, PTY cleanup, and active-record cleanup on disconnect.

### Native Mobile Client Auth, Bootstrap, Status, And Analytics

The native mobile app is a first-class Rust target beyond the terminal bridge. Rust must preserve the internal auth-token, bootstrap, status-refresh, analytics, and action-projection behavior that the app depends on.

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| device bearer issue/verify | Device tokens are `smat_` values containing a URL-safe base64 JSON payload signed with HMAC-SHA256 from `auth.google.session_cookie_secret`. Payload includes `v`, `type=device_access`, lowercase email, name, `iat`, and `exp`; verification requires prefix, signature match, `type=device_access`, unexpired `exp`, and non-empty email. | Native app can authenticate future API calls without browser cookies. | Preserve token prefix, payload fields, signature input, expiry behavior, and failure-as-unauthenticated semantics. |
| `/auth/session` | Return auth-disabled local-bypass success when Google auth is not requested; return local-bypass success for loopback; return misconfigured when Google auth is requested but incomplete; accept device bearer before browser session; otherwise reflect browser cookie session state. | Android can distinguish auth disabled, local development, misconfigured public deployment, device bearer auth, browser session auth, and unauthenticated state. | Preserve mode ordering, response fields, `auth_type` values, and misconfigured error shape. |
| `/auth/device/google` | Require Google auth ready, verify Google ID token, require audience in allowed web/android client ids, require verified allowlisted email, then issue bearer token or return the current `401`/`403`/`503` errors. | Native app login rejects wrong audience/unverified/non-allowlisted accounts and receives a bearer token for allowed accounts. | Preserve verification order, status codes, detail strings, email normalization, and token response fields. |
| `/client/bootstrap` | Build cold-start config from `auth.google`, `external_access`, `mobile_terminal`, and infra health: expose auth endpoints, public host/user metadata, `mobile_terminal_supported`, `mobile_terminal_ws_url`, preferred action, and no-secret mobile metadata. Current Python also exposes Termux fallback fields. | App cold start can choose login, mobile terminal attach, or details without leaking local SSH/proxy internals. | Preserve public/no-secret fields, path-prefix-aware endpoints, TLS/public-host gating, and mobile terminal preferred-action behavior. Do not port Termux attach support; if legacy fields remain during transition they must be unsupported/non-primary. |
| client session mobile metadata | Session list/detail computes `mobile_terminal`, `attach_descriptor`, and `primary_action` from provider/runtime, public path prefix, TLS policy, and attach descriptor support. Ticket endpoint and signature input use the advertised prefixed path. Current Python also computes deprecated `termux_attach`. | The native app opens the correct attach mode and signs the exact path it was told to call. | Preserve path-prefix propagation, TLS rejection behavior, `requires_device_key`, and details fallback for headless providers. Do not port Termux command generation or make Termux a primary action. |
| Termux attach command metadata | Current Python can build a local `sh -lc` wrapper around SSH with `ProxyCommand`, `StrictHostKeyChecking=accept-new`, `SM_TMUX_SESSION`/`SM_TMUX_SOCKET`, tmux fallback order, attach log cleanup, retry-on-255, Cloudflare bad-handshake detection, and LAN fallback. | This is legacy behavior; owner has deprecated it because it is flakier than the native app terminal path and should not shape the Rust port. | Do not port this path. Keep this row only as historical source-traceability so implementation tickets do not rediscover and accidentally preserve it. |
| `/client/request-status` | Iterate all sessions, enforce input gates, send important prompt `[sm] user requests status, please update now using sm status`, and return targeted/delivered/queued/failed counts plus targeted ids. | Mobile operator can ask every live agent for a fresh status and see how many were reached. | Preserve prompt text, delivery mode, gate failures, count semantics, and id list. |
| `/client/analytics/summary` | Build summary from live session state, message queue DB, track registrations, server log spawn/restart/self-heal lines, configured health checks, and attach availability. | Mobile dashboard KPIs, reliability cards, provider/state distribution, throughput, and attach health remain meaningful. | Preserve source precedence, bucket/window behavior, configured paths, health-check labels, and absent-data defaults. |
| `/sessions/{session_id}/activity-actions` | Only codex-app sessions with observability projection enabled can return projected actions; missing session, wrong provider, disabled rollout, or missing getter have current errors. | Watch/mobile action rows do not appear for unsupported providers or disabled projections. | Preserve gating, errors, limit bounds, and action ordering from the projection getter. |

Native mobile Stage 3 contract tests must include device bearer issue/verify/expiry/tamper, `/auth/session` disabled/local/misconfigured/device/browser modes, Google device auth wrong-audience/unverified/not-allowlisted/signing-missing paths, bootstrap secret redaction and preferred-action fallback, path-prefix-aware ticket endpoints/signatures, TLS-required behavior, Termux-not-primary/unsupported migration behavior, request-status prompt/counts/gate failures, analytics KPI inputs/defaults, and activity-actions provider/rollout gating.

### Activity Projection And Watch State

Activity state is an external contract for `sm watch`, mobile/watch UIs, and operator decisions. Rust must preserve the precedence below or replace it with fixtures that demonstrate equivalent behavior.

| Provider / Condition | Activity Result | Source Behavior To Preserve |
|----------------------|-----------------|-----------------------------|
| stopped session or killed completion | `stopped` | Stopped status wins over other evidence. |
| node unreachable | `node-unreachable` | Remote failures surface before provider-specific activity. |
| Claude/plain tmux providers with permission pattern | `waiting_permission` | Monitor `last_pattern == permission` wins over idle/running heuristics. |
| non-Codex provider with completion status | `waiting_input` | Completion status means waiting for next user/agent input unless killed. |
| delivery state idle true/false | `idle` or `thinking`/`working` | Delivery state overrides stale session status; output-flowing upgrades active to `working`. |
| plain Codex recently idle | `thinking` for short window | Codex prompt/idle reconciliation uses a recent-activity grace window to avoid false idle. |
| codex-fork lifecycle wait states | `waiting_permission` or `waiting_input` | Reducer wait state wins over pane-title fallback. |
| codex-fork running or spinner pane title | `working` | Reducer running state and Braille spinner pane-title correction both indicate active work. |
| codex-app delivery state | `idle` or `working`; otherwise recent activity is `thinking` | No tmux/output monitor exists, so activity depends on app-server callbacks and queue state. |

### Codex, Codex-Fork, And Structured Request Behavior

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| codex-app turn started/delta/complete | Append event rows, log observability, update turn-in-flight maps, mark session running/idle, update hook output store, notify Telegram, and mark queue active/idle. | Watch/activity, pending messages, Telegram responses, and observability APIs stay aligned with app-server stream. | Preserve event order and queue-state transitions around app-server callbacks. |
| codex-app server request | Approval/user-input requests create durable pending request with timeout policy, set wait state, log tool event, append event, wait for resolution, and return resolved payload or policy timeout. | CLI/API can list/respond to pending requests; provider receives default decline/empty answers on timeout. | Preserve request ids, pending/resolved/expired/orphaned states, idempotent response behavior, and timeout policy. |
| Codex request ledger startup | On ledger construction, create DDL/indexes, then mark unresolved `pending` or `expired` rows from a different `process_generation` as `orphaned` with `resolution_source=policy`, `error_code=server_restarted`, and `error_message=server restarted before request resolution`. Same-generation rows remain pending. | `/sessions/{session_id}/codex-pending-requests?include_orphaned=true` and provider waiters see previous-process unresolved requests as closed instead of hanging forever. | Preserve process-generation orphaning exactly, or require a Stage 5 cutover plan that drains/closes pending requests before migration. |
| codex-app item notifications | Tool/file/command item start/delta/complete map into observability rows with normalized status, latency, command/cwd/file/diff/error fields. | Tool activity timelines and usage telemetry remain useful. | Preserve status normalization and best-effort logging without failing provider flow. |
| codex-fork event ingestion | Normalize event type, dedupe provider-native `(session_epoch, seq)` before processing, persist SM event, reduce lifecycle, then advance durable cursor only after successful persistence/reduction. Roll back SM event if cursor advancement path fails. | Restart/reconnect does not duplicate or skip provider events; activity state is deterministic. | Preserve persist-before-cursor-advance and duplicate suppression semantics. |
| codex-fork lifecycle reducer | Tracks `turns_in_flight`, wait resume state, wait kind, last seq, session epoch, and lifecycle state. Interrupted aborts do not imply idle unless they end a real turn; shutdown-complete is transport churn, not a stopped session. | Avoids false "interrupted"/idle/stop notifications while codex-fork is still working or restarting. | Preserve reducer state machine and special cases for interrupted aborts and shutdown-complete. |
| codex-fork assistant relay | Relays completed assistant messages once per `(thread, turn, item)`, buffers deltas as fallback, updates hook output store, sends Telegram response, and records relay marker. | Telegram/mobile last-output views do not duplicate responses after reconnect/replay. | Preserve dedupe keys and delta fallback behavior. |
| codex-fork runtime maintenance | Prune stale runtime artifacts, restart missing runtime when possible, maintain event/control sockets, and record degraded control state. | Detached runtime attach/control remains available and stale artifacts do not confuse restore. | Preserve artifact ownership, cleanup safety, and control-degraded reporting. |
| codex native title sync | Reads Codex session index and provider events to update `native_title`, display sync timestamps, and provider resume ids. | `sm watch`, Telegram topics, and attach descriptors show useful names. | Preserve title normalization, source mtime checks, and persistence behavior. |

### Remote Node And Node-Agent Behavior

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| node config load | Always create `primary`; normalize node ids; skip empty ids; expose non-secret metadata through `/nodes`; keep hook/node secrets private. | CLI/watch/mobile can list nodes without leaking credentials. | Preserve secret/non-secret response boundary. |
| remote command | Build local argv for primary or SSH command with configured `ProxyCommand`, `ControlPath`, `ControlMaster=auto`, `ControlPersist=600`, `ConnectTimeout=5`, and optional `-tt` for attach. | Remote session create/attach behaves like local commands routed over SSH. | Preserve command quoting, cwd handling, SSH options, and error behavior. |
| remote restore inventory | Node agent loads remote state file, falling back to legacy default path when appropriate, and returns restorable sessions. Server caches restore candidates with configured TTL. | `sm restore --node` can discover remote stopped/live sessions. | Preserve state-file fallback and cache freshness behavior. |
| node-agent registration | First frame authenticates node; server registers event/control paths. Node agent validates paths are absolute or `~`-relative under node log dir before tailing or control. | Remote codex-fork cannot make server read/control arbitrary paths outside node log dir. | Preserve path confinement and error frames. |
| node-agent event/control | Tail registrations stream event lines with cursor filtering; control frames open Unix control socket with timeout and return `control_result`; unavailable sockets return structured errors. | Remote codex-fork control and activity work across node disconnect/reconnect. | Preserve reconnect loop, cursor handoff, timeout, and `not_registered`/`not_ready`/`control_failed` errors. |
| node reachability | Session manager and output monitor mark remote sessions unreachable on SSH/log/bridge failures and reachable after successful operations. | Activity state shows `node-unreachable` without losing session state. | Preserve reachability as state overlay rather than destructive failure. |

### Queue Runner And External Job Behavior

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| create job | Validate enabled/type/cwd/notify target, require exactly one argv or script, create per-job directory, persist wrapper/log/exit paths, insert pending row, try admission, and send queued notification if still pending. | CLI/API receives stable job ids and log paths; notify sessions get queued/started/completed messages. | Preserve validation, filesystem layout, and notification side effects. |
| admission | Enforce global max running, per-type concurrency, memory gate, perf cooldown, tests-after-perf ordering, and perf displacement of background jobs when eligible. | Queue scheduling remains predictable for tests/perf/background workflows. | Preserve priority order and holding reasons. |
| run job | Execute `/bin/zsh` wrapper in a new process group, capture stdout/stderr to log, enforce timeout, record exit code/state, and schedule further admissions. | Cancellation/timeout can terminate process groups and logs remain inspectable. | Preserve process-group ownership, timeout/cancel grace, and state transitions. |
| restart recovery | Running jobs finish from exit-code file, timeout if expired, poll live pid if still alive, or fail if pid vanished; pending jobs clear holding reason and re-enter scheduler. | Server restart does not orphan queue state or double-run completed jobs. | Preserve recovery classification and notify-once behavior. |
| policy runs | Dedupe/retention suppresses or runs policy jobs, stores queue job link/result, and reconciles results after job completion. | Policy APIs return consistent run/result status. | Preserve dedupe tokens, suppression, and retention cleanup. |
| resource sampling | Periodically records queue/running counts and CPU/memory-ish samples when enabled. | Usage telemetry and queue diagnostics remain available. | Preserve sampling or explicitly replace with equivalent instrumentation. |

### App Artifact Deployment And Public Serving

Android app artifact publishing is a mobile distribution contract. The Rust port must preserve write ordering, immutable artifact behavior, public serving semantics, and actor attribution unless Stage 4/5 approves a breaking deployment change.

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| deploy actor resolution | Upload actor comes from device bearer email, browser session email, or loopback local bypass; external requests without an actor fail with `401 Authentication required`. | Local deploy scripts and authenticated mobile/device publishers work, while unauthenticated public uploads fail. | Preserve actor precedence, local-bypass attribution as `local_bypass`, and auth failure shape. |
| multipart validation | Validate app name, multipart form parsing, required `file` field, optional integer `version_code`, optional `version_name`, non-empty upload, and 100 MB maximum while streaming chunks. | Deploy scripts get stable validation errors and cannot publish empty/oversized artifacts. | Preserve validation order, limits, status codes, and temp-file cleanup on failure. |
| artifact write | Stream to temp file in app artifact dir while hashing; atomically replace `latest.apk`; derive 8-character lowercase SHA-256 hash; atomically copy immutable `{hash}.apk` only if absent; then atomically write `meta.json`. | `latest.apk`, immutable APK, and metadata converge together enough for Android update clients and rollback inspection. | Preserve temp-write/replace/copy/metadata ordering, hash length, and immutable copy-if-absent behavior. |
| metadata/response | Metadata includes `artifact_hash`, `uploaded_at`, `size_bytes`, `uploaded_by`, and optional `version_code`/`version_name`; deploy response returns `ok`, `app`, `size_bytes`, `download_url=/apps/{app}/latest.apk`, and `artifact_hash`. | Operators and mobile updater can inspect artifact provenance and fetch the latest URL. | Preserve metadata schema, response fields, timestamp format, and actor attribution. |
| public latest serving | `/apps/{app}/latest.apk` reads `meta.json`, validates `artifact_hash`, and returns `302` to `/apps/{app}/{hash}.apk` with `Cache-Control: no-cache`. | Clients always resolve latest through metadata and do not cache stale latest redirects. | Preserve public auth-exempt redirect semantics and cache header. |
| immutable serving | `/apps/{app}/{hash}.apk` validates app/hash, serves the hashed APK with media type `application/vnd.android.package-archive`, filename `{app}.apk`, and `Cache-Control: public, max-age=31536000, immutable`. | Android downloads and caches content-addressed APKs safely. | Preserve status/error behavior, content type, filename, and immutable cache header. |
| metadata and legacy alias | `/apps/{app}/meta.json` returns the latest metadata model; `/apk` redirects to `/apps/session-manager-android/latest.apk`. | Existing mobile install/update links keep working. | Preserve metadata endpoint and legacy redirect until Stage 5 approves removal. |

App artifact Stage 3 contract tests must include local-bypass upload, external unauthenticated rejection, device-bearer upload attribution, empty/oversized/version-code validation, latest plus hashed file creation, `meta.json` schema, latest redirect/cache header, immutable APK cache/content type, metadata endpoint, and `/apk` legacy redirect.

### Telegram, Email, Bug Reports, And Human Delivery Side Effects

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| Telegram startup reconciliation | Delete orphaned topics, backfill default chat ids, reuse durable topic registry, create missing topics asynchronously, and start optional stale-topic cleanup loop. | Forum topics do not drift from session state after restarts. | Preserve durable registry, absent-topic handling, and nonblocking topic creation. |
| Telegram title sync | Queue best-effort rename retries with backoff; skip stale requested names; treat "topic not modified" as success and absent topic as mapping cleanup. | Topic names converge without blocking API requests. | Preserve retry/backoff and stale-name guard. |
| Telegram notifications | Output monitor, message queue, queue runner, Codex response/review flows, and direct bot commands all route through notifier/bot handlers with allowlist checks and chunking. | Operators receive responses, delivery confirmations, queue updates, and review results in the expected topics. | Preserve routing by session/thread, chunking, and telemetry logging. |
| inbound email webhook | Validate bridge availability, optional worker secret, authorized sender, trusted session-id header after secret check, routing footer fallback, stopped-session restore, input gates, sequential delivery with `response_relay_source=email`, and activity update. | Email replies can revive and message sessions safely. | Preserve secret-before-trusted-header ordering, restore-on-stopped behavior, and ignored versus error responses. |
| outbound email | Resolve human/user aliases, require managed sender and body, render markdown/html, append routing footer, and call provider API. | Human recipients can reply back into the original session. | Preserve routing footer and configured alias resolution. |
| bug reports | Insert report with optional client/server debug state, prune spool to max reports, return id/created fields, and update maintainer delivery result after notification attempt. | Native app bug reports survive and can be delivered to maintainer. | Preserve spool schema, pruning, and delivery-result update semantics. |

### Hook Audit, Locks, Worktrees, And Context Monitor

Hook behavior is internal plumbing, but providers, agents, and operators depend on its side effects. Rust must preserve hook latency posture: safety decisions that must block a tool use are synchronous, while audit logging remains fire-and-forget and must not slow provider hooks in normal operation.

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| tool-use DB init | Create `tool_usage` and `telegram_telemetry` with WAL, busy timeout, persistent connection, serialized lock, indexes, and derived fields for destructive/sensitive/file/command/exit-code queries. | `/tool-calls`, telemetry, parent wake digests, audits, and usage reports can query tool history reliably. | Preserve schema, WAL/busy-timeout posture, indexes, and derived-field semantics. |
| tool audit classification | Classify destructive Bash/file operations and sensitive files, extract target file, bash command, project name, agent id, native Claude ids, tool-use id, hook type, and exit code. | Security audit and reviewers can filter destructive/sensitive activity. | Preserve classification behavior or explicitly version any changed classifier in Stage 4. |
| `PreToolUse` active repair | On PreToolUse with a session id, mark queue delivery state active, update session last tool call/name, and then continue hook processing. | `sm watch` does not incorrectly show idle while a tool is running. | Preserve active-state repair before fire-and-forget logging. |
| file-write auto-lock | PreToolUse for Edit/Write/NotebookEdit resolves absolute path, finds git root, tries to acquire repo lock for session, records touched repo, and saves state. | Sessions automatically protect shared worktrees when editing. | Preserve lock acquisition before allowing the tool. |
| lock conflict | If another session owns the repo lock, return hook error with existing friendly-name/session and worktree instructions; do not rely on asynchronous audit logging to block the tool. | Provider receives a blocking message and avoids unsafe concurrent edits. | Preserve conflict response shape, synchronous decision, and no state mutation as the new owner. |
| worktree creation tracking | PreToolUse Bash containing `git worktree add` records resolved worktree path in session state. | Stop hook cleanup can later warn about dirty generated worktrees. | Preserve command detection, path resolution, and state persistence. |
| Stop hook lock release | On Stop hook, release all session-held repo locks, evaluate touched worktrees, clear clean prompt state, and optionally send dirty worktree cleanup prompt via important delivery. | Locks do not remain held forever; agents get one cleanup prompt per dirty status hash. | Preserve release, dirty-hash dedupe, `notify_dirty` mute behavior, and prompt text contract. |
| audit logging | Tool-use hook schedules logger task and returns promptly; malformed JSON returns 204; slow-hook debug logs use configured threshold. | Provider hook path remains low latency and resilient to logging failures. | Preserve nonblocking audit path and failure isolation. |
| context monitor compaction | Compaction event bypasses registration gate, sets `_is_compacting`, resets warning/critical one-shot flags, and queues sequential context message to `context_monitor_notify` or parent. | Parents/EMs learn context was compacted even for unregistered children. | Preserve bypass, compacting flag, one-shot reset, fallback target, and message category. |
| context monitor complete | `compaction_complete` clears `_is_compacting` and force-resets tracked remind timer. | Reminders do not fire during compaction and resume after agent wakes. | Preserve compaction gate interaction with reminders. |
| context reset | `context_reset` bypasses registration gate, re-arms warning/critical flags, clears agent status/task-complete state, saves state, and cancels queued context-monitor messages from this sender. | `/clear` and `sm clear` do not deliver stale context warnings to parent/EM. | Preserve stale-message cancellation and status clearing. |
| context usage warning/critical | Usage events are ignored unless context monitor is enabled; null usage is accepted; warning/critical thresholds are one-shot per cycle; critical is urgent, warning is sequential; self-alert text differs from child-alert text. | Agents and parents receive bounded, correctly routed context pressure alerts. | Preserve registration gate, one-shot flags, threshold routing, delivery mode, and message strings. |

Hook/context Stage 3 contract tests must include malformed hook JSON, fast audit return under logger latency, destructive/sensitive classification, PreToolUse active repair, lock acquire/conflict/release, dirty worktree prompt dedupe and mute mode, worktree-add tracking, compaction bypass, compaction-complete remind reset, context-reset stale-message cancellation, warning/critical one-shot routing, null usage ignored, and disabled monitor ignored.

### Response Relay And Turn-Bound Notifications

The response relay ledger prevents Telegram/mobile/human notification flows from relaying stale or duplicate assistant output after queued input, direct input, email input, or restart. It is provider-agnostic for inbound boundaries and has Claude-specific transcript behavior.

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| inbound turn record | Delivered input with an explicit source records `inbound_turns` row, hashes text, captures provider/transcript boundary when available, and supersedes older active turns for the same session. Internal uncategorized prompts without explicit source do not move the response boundary. | Telegram/email/mobile response relay follows the latest externally-originated turn, not stale earlier work. | Preserve source filtering, supersede semantics, text hash, and boundary fields. |
| direct input relay | Direct sends and bypass sends record inbound turns only when a relay source is present or `from_sm_send` implies `sm-send`. | Permission responses and internal maintenance prompts do not incorrectly claim the next assistant response. | Preserve explicit source requirement. |
| Claude Stop relay | On Claude Stop/idle, locate active inbound turn, backfill spawn boundary if needed, resolve or replace transcript path/offset, retry transcript read after Stop, collect assistant outputs after boundary, and defer if no transcript/output exists. | Telegram receives only response text produced after the relevant inbound turn, and delayed transcript writes do not cause data loss. | Preserve boundary replacement, retry delay, deferral, and pending-stop-notification behavior. |
| assistant output claim | Claim `(session, inbound, provider, assistant_message_id)` before notifying; reject already relayed or recently claimed outputs; allow stale claims to be retried after timeout. | Concurrent hooks/restarts do not duplicate response notifications. | Preserve claim-before-notify and stale-claim timeout behavior. |
| notifier result | Mark output relayed only after notifier accepts; release claim when notifier rejects or raises; update session activity/save only after accepted notification. | Failed Telegram delivery can be retried and successful delivery is durable. | Preserve accepted-only marking and release-on-rejection semantics. |
| restart dedupe | `assistant_outputs` and relay markers survive restart; `inbound_turns` keep active/superseded boundaries. | Restart does not resend old assistant output or lose latest inbound source. | Preserve schema and idempotent dedupe across process restart. |
| Codex/codex-fork relay | Codex app/fork response paths update hook output store and use provider-specific dedupe markers; completed assistant messages are relayed once per turn/item. | Provider-specific streams still feed the same user-facing response channels without duplicates. | Preserve provider-specific relay keys and integration with response relay source semantics. |

Response-relay Stage 3 contract tests must include stale-output suppression, no-boundary suppression, spawn-boundary backfill, transcript path replacement, empty transcript deferral/retry, superseding older turns, explicit source filtering, claim/dedupe after restart, notifier rejection release/retry, accepted-only mark-relayed, direct-send relay source, email relay source, and duplicate Stop hook handling.

### Restore, Kill, Clear, And Failure Recovery

| Input / Trigger | Internal Behavior | Externally Depended Output / Side Effect | Rust Contract |
|-----------------|-------------------|------------------------------------------|---------------|
| retire/current Python session-stop paths | Provider-specific cleanup: close codex-app, orphan pending requests, cancel codex-fork monitors and unregister remote bridge, unlink runtime artifacts, kill tmux if applicable, mark stopped/killed/completed timestamps, unregister roles, and save state. | Retiring a session stops runtime, pending requests unblock, queues/roles do not point at dead sessions. Current Python also exposes `sm kill` as a CLI alias and HTTP/mobile session-stop APIs such as `POST /sessions/{target_session_id}/kill`. | Preserve provider cleanup and stopped record semantics through retained `retire` and API/mobile stop behavior; do not port the redundant `sm kill` CLI alias. |
| restore | Validate stopped/runtime-missing, resolve resume id, rebuild provider runtime with resume args/thread id, register remote bridge, reset completion/error/stopped fields, restart codex-fork monitor, mark codex-app idle, save, and repair Telegram topic asynchronously. | Stopped sessions can be restored in place without changing id/name/topic. | Preserve in-place restore, provider-specific resume requirements, and failure messages. |
| clear | Provider/session clear stops current runtime or input, handles skip fences for subsequent Stop hook, may create new prompt, and prevents stale context-monitor alerts from parent delivery. | Agents can clear/reset without triggering false stop notifications. | Preserve clear/skip-fence interaction with message queue. |
| tmux death | Monitor cleanup marks stopped with diagnostic but preserves record, so restore and postmortem remain possible. | `sm watch`/CLI do not lose dead sessions silently. | Preserve record-preserving cleanup and diagnostics. |
| codex request orphaning | Killing/retiring sessions marks pending requests orphaned and unblocks waiters with session-closed error. | Providers/clients do not hang on dead pending approvals. | Preserve orphaning and idempotent response behavior. |

### Internal Behavior Contract Tests To Capture

The Rust migration should not begin implementation until these Python-baseline tests or fixtures exist:

- startup with current state file, legacy state fallback, corrupt state fallback, unreachable remote node, dead local tmux session, live codex-fork runtime artifact, and existing Telegram topic registry.
- session create for Claude, Codex, codex-fork local fallback, codex-fork remote rejection/bridge success, codex-app retirement/restore, parent-child spawn, friendly-name native rename, and Telegram topic defer.
- `send_input` sequential/important/urgent/steer, queued versus delivered return, typed-user-input preservation, delivery timeout, self-send notify suppression, EM-only notify-on-stop, and codex-fork notify-on-stop disablement.
- Stop hook idle transition with pending handoff, fresh and stale skip fence, paste-buffered stop notify, delayed stop notify, reminder cancellation, parent-wake cancellation, and queued delivery.
- scheduled reminder, recurring reminder, tracked soft/hard remind, persistent tracked remind refresh, cancel-on-reply, parent wake digest/escalation/recovery, and startup recovery.
- tmux input chunking/settle/Enter separation, submit verification, copy-mode exit, partial-injection cleanup, dead-pane diagnostics, and crash recovery pause/unpause.
- activity projection for stopped, node-unreachable, permission, completion waiting-input, delivery idle/active, output-flowing working, plain Codex prompt return, codex-fork lifecycle wait/running/idle/error, pane-title spinner fallback, and codex-app no-tmux states.
- codex-app request lifecycle: pending, response, timeout policy response, idempotent response, orphaning on kill, event/observability rows, and wait-state projection.
- Codex request ledger startup reconciliation: different-generation unresolved pending/expired rows become orphaned with `server_restarted`, same-generation rows remain pending, and include-orphaned listing exposes the state.
- codex-fork event ingestion: duplicate provider event suppression, persistence failure fallback, cursor advancement only after reducer success, interrupted abort semantics, shutdown-complete semantics, assistant relay dedupe, delta fallback, remote bridge reconnect, and control error frames.
- mobile terminal attach ticket mint/replace/expire/consume, quota checks at mint and consume, device/user revocation between mint and consume, disabled runtime behavior, origin/auth timeout failures, PTY bridge start/history/resize/input/output, detach/exit, cleanup, and active-attach teardown.
- native mobile auth/bootstrap/status/analytics: device bearer issue/verify, auth-session modes, Google ID-token exchange failures, bootstrap secret redaction/preferred action, path-prefix/TLS behavior, Termux-not-primary/unsupported migration behavior, request-status prompt/counts, analytics KPI sources, and activity-actions gating.
- app artifact deployment and serving: actor attribution, multipart validation, temp/latest/immutable/meta write ordering, public latest redirect/cache headers, immutable APK serving, metadata endpoint, and `/apk` alias.
- tool-use hook audit/lock behavior: malformed JSON, fast return under logger latency, destructive/sensitive classification, PreToolUse active repair, auto-lock acquire/conflict/release, dirty worktree prompt dedupe and mute mode, worktree-add tracking, and `/tool-calls` audit visibility.
- response relay ledger behavior: stale-output suppression, empty transcript deferral/retry, transcript path replacement, superseding older turns, source filtering, claim/dedupe after restart, notifier rejection release/retry, accepted-only mark-relayed, direct-send/email relay source, and duplicate Stop hook handling.
- durable Codex review watch behavior: duplicate serialization, pickup failure still checking reviews, retry persistence/comment refresh, completion notification text, missing notify-session auto-cancel, inactive-history listing, cancellation state, and startup active-only recovery.
- external job watch behavior: stale-line suppression, regex validation, event precedence, progress dedupe, `notify_on_change` compatibility, exit-code completion/error, PID-exited fallback, notification text/delivery mode, deactivate ordering, and target-missing recovery skip.
- context monitor hook behavior: compaction bypass and one-shot reset, compaction-complete remind reset, context-reset stale-message cancellation and task-complete clear, warning/critical one-shot routing, null usage ignored, disabled monitor ignored, and self-vs-child message text.
- queue runner create/admit/run/timeout/cancel/recover/displace/notify/resource-sample behavior.
- inbound email restore-and-deliver, trusted session header only with worker secret, missing routing footer ignored, unauthorized sender rejected, and empty reply ignored.
- Telegram topic reconciliation, title sync retry/stale-name guard, absent-topic cleanup, and notification routing by session thread.

## Stage 4: Ruggedization And Threat Model

Status: converged after three sequential independent reviewer convergence signals.

Stage 4 turns the Stage 2 surface inventory and Stage 3 behavior contracts into security requirements for the Rust migration. Its current-control rows describe Python source behavior. Stage 5 now supersedes broad preservation for removed surfaces: Rust preserves retained core behavior and asserts absence or retirement errors for cutover removals.

Stage 4 itself did not approve breaking changes. The owner has since approved the Stage 5 cutover scope. Implementation tickets must now follow Stage 5 for retained versus removed surfaces, while using Stage 4 for threat/control detail, abuse-case fixtures, and security requirements.

Native mobile attach and the sm mobile app are first-class, high-priority surfaces. Generic public browser access through `sm.rajeshgo.li` is not a Rust public data target. Local/authenticated/proofed diagnostics may remain; unauthenticated public operational data must not.

### Stage 4 Threat Model Artifacts

| Artifact | Purpose |
| --- | --- |
| [index](762_stage4_artifacts/index.md) | Stage 4 artifact bundle entry point and reviewer handoff. |
| [threat register](762_stage4_artifacts/threat_register.md) | Source-tied threat scenarios, current controls, required Rust mitigations, and residual risks. |
| [route-local secret matrix](762_stage4_artifacts/route_local_secret_matrix.md) | Per-secret missing/mismatch/reuse/logging/rotation behavior and Stage 5 design gates. |
| [hardening backlog](762_stage4_artifacts/hardening_backlog.md) | Hardening work, accepted cutover reductions, observability requirements, and Stage 5 handoff gates. |

### Security Objectives

The Rust port should make Session Manager safer without hiding compatibility breaks inside the rewrite. Security objectives:

- prevent unauthorized shell/session control, especially through public HTTP, mobile terminal WebSockets, hooks, remote nodes, email, Telegram, and queue-runner entrypoints.
- protect secrets: Google OAuth credentials, session-cookie secret, device bearer signing material, mobile device keys, hook secrets, node tokens, email worker secret, Telegram/email tokens, provider credentials, tmux socket paths where they grant control, and app artifact write credentials.
- preserve operator intent and durable delivery state across restart, crash, and cutover: queued messages, reminders, parent wakes, review watches, job watches, codex pending requests, response relay state, queue jobs, audit rows, and app artifacts.
- keep mobile attach usable while making ticket, device, origin, TLS, quota, and disable/revocation checks explicit and testable.
- keep audit evidence reliable for destructive/sensitive tool use, Telegram telemetry, request timing, queue/policy runs, and usage evidence.
- reduce denial-of-service blast radius from untrusted network inputs, malformed hook payloads, WebSocket clients, queue jobs, filesystem churn, and large app uploads.
- make trust-boundary failures fail closed with clear operator diagnostics rather than silently downgrading auth.

### Scope And Assumptions

Session Manager is operator infrastructure, not a general multi-tenant service. The local OS user account, local config files, and loopback caller are currently high-trust. That is a compatibility fact, not a security ideal: a process running as the operator can already read local state, talk to tmux, and invoke `sm`.

Threat modeling therefore focuses on boundaries where trust changes:

- public HTTP versus loopback.
- browser session versus device bearer token versus route-local secret.
- native mobile device versus public browser.
- managed hook payload versus untrusted network request.
- local session versus remote node.
- session graph authority versus ordinary session metadata reads.
- queue-runner command execution versus ordinary status/read APIs.
- inbound email/Telegram/GitHub/provider events versus operator-authored input.
- launchd/service install and infra repair versus ordinary server runtime.
- durable state owned by Session Manager versus user-writable files and runtime IPC artifacts.

Out of scope for Stage 4 as a primary defense boundary: a fully compromised operator account, a malicious Rust compiler/toolchain, root compromise, and provider-side compromise after valid credentials are stolen. The spec should still reduce blast radius and improve detection where feasible, but it cannot claim to solve those conditions.

### Assets

| Asset | Why It Matters |
| --- | --- |
| shell and tmux control | Grants direct command execution and access to projects, credentials, and local files. |
| session graph and durable delivery state | Encodes user intent, parent/child authority, queued inputs, reminders, review/job watches, and wakeups. |
| mobile terminal tickets and device auth | Protects on-the-go shell attach, the highest-priority remote control path. |
| Google/browser auth state | Protects public host access, watch UI, Android device exchange, and bootstrap metadata. |
| hook, node, worker, and app upload secrets | Separates managed automation from unauthenticated public traffic. |
| audit and telemetry stores | Supports forensic review, usage evidence, and destructive-tool accountability. |
| app artifacts and metadata | Feeds native app distribution and update trust. |
| logs, transcripts, and response relay state | May contain sensitive output and drive externally visible notifications. |
| config and local-env overlays | Can enable public auth, mobile access, node routing, and service credentials. |
| queue runner jobs and policy runs | Can execute commands and mutate project state asynchronously. |

### Actors

| Actor | Trust Level | Notes |
| --- | --- | --- |
| local operator | trusted | Owns configuration and intentional `sm` control. |
| local managed agent/hook | limited trusted | Runs with operator privileges but may be confused or compromised by prompt/tool input. |
| native mobile app/device | authenticated remote | High-priority user workflow; must keep strong device, ticket, TLS, and revocation controls. |
| browser/watch client | authenticated or loopback remote | Lower priority than native mobile; local/auth/proofed diagnostics may remain, but public operational data is removed from the Rust target. |
| remote node/node-agent | delegated trusted | Can execute sessions and relay provider events; secrets and path confinement matter. |
| email workers and human-recipient delivery | external services with route-local trust | Retained fallback channel after Telegram removal; worker proof, sender allowlists, trusted-header ordering, and privacy-safe audit are required. |
| Telegram bot/users | external service with route-local trust in current Python | Source-traceability and rollback evidence only for the Rust cutover; Telegram bot/control/topic cleanup is removed from the first Rust release. |
| provider/GitHub/Codex integrations | external dependency | Events can drive reducers, review watches, and notifications; persistence must avoid replay/skips. |
| unauthenticated network client | untrusted | Must not gain shell, queue, artifact upload, or state mutation access. |
| compromised agent/session | adversarial insider | May try to forge hooks, steal tokens from output/config, bypass locks, or spam external channels. |
| stolen mobile/browser token holder | adversarial authenticated principal | Must be constrained by expiry, revocation, audience, and least privilege. |

### Trust Boundaries And Required Controls

| Boundary | Current Contract To Preserve | Rust Hardening Requirement |
| --- | --- | --- |
| loopback/public HTTP middleware | Local bypass; Google/session/device bearer on public host; explicit exemptions from Stage 2 auth matrix. | Implement deny-by-default public routing, exact exempt-path tests, public-host detection tests, redirect-versus-JSON behavior tests, and route-local auth after middleware. |
| browser OAuth/session auth | `/auth/google/login`, `/auth/google/callback`, `/auth/logout`, `/logged-out`, `/auth/session`, and `/auth/device/google` are auth-exempt by current middleware, with route-local OAuth/device checks. | Preserve retained OAuth state, safe local redirects, allowlist, signed cookie creation/destruction, device exchange, and failure redirects for local/auth/proofed flows; Rust may force explicit re-login/re-enrollment during cutover when required by proofed public access. |
| session graph and agent authority | Core APIs create, fork, spawn, restore, kill, open, send input, update roles/maintainers, arm stop/context notifications, accept adoption, and mutate task/status state. | Preserve retained core workflows and error shapes where compatible; Rust should add explicit capability checks for mutating session graph APIs and may break unsafe legacy authority patterns under the approved Stage 5 cutover. |
| sensitive read and summarizer surfaces | Output, last-message, tool-call, Codex event, pending request, activity-action, review-result, analytics, and summary endpoints expose sensitive state; summary routes send captured output to an external provider subprocess. | Keep retained local/operator diagnostics with redaction/error-bound fixtures, provider subprocess timeout/rate limits, and audit attribution. Public sensitive reads and the AI summary provider HTTP route are removed from the Rust target. |
| mobile terminal | Device auth, signed attach tickets, first-frame WebSocket auth, origin/TLS checks, quotas, disable/revoke kill switch. | Keep mobile as first-class; use constant-time secret comparison, single-use ticket atomicity, bounded frame sizes, explicit close/error fixtures, and audit events for mint/consume/disable. |
| hooks | Loopback/public middleware plus route-local hook secret for remote-session hooks; tool hook may affect locks/audit. | Preserve fast hook response while bounding payloads, validating session/node identity, requiring remote hook secrets, and isolating audit failures from hook decisions. |
| tmux client hook | `/hooks/tmux-client` is a local-only route-local control that records attach/detach/session-change events and broadcasts event-stream state. | Preserve local-origin guard, event allowlist, query-param contract, versioned broadcast payload, and 403/400 failure behavior; non-local exposure is user-review-only. |
| remote nodes | `nodes.*` config, SSH/proxy routing, node token or hook-secret fallback, node-agent hello secret, path confinement. | Require explicit node credentials, reject hook-secret fallback, validate node ids/paths, never expose secrets through `/nodes`, and fixture LAN-first public-edge fallback with node proof/revocation. |
| queue runner | Authenticated command-execution API plus persistent job/policy state. | Treat retained narrow queue creation as command execution; require policy/admission checks, resource limits, durable cancellation, and audit/notification preservation. Policy-run and CI-helper surfaces are removed. |
| app artifacts | Upload requires authenticated actor; public latest/hash/meta download routes are auth-exempt in current Python. | Preserve native update flow while requiring auth/proof or signing before public serving; validate app/hash/version/size, atomic temp/latest/meta writes, immutable hash serving, content type/cache headers, and upload attribution. |
| inbound email | Default `/api/email-inbound` is auth-exempt in current Python; worker secret/sender checks and trusted session header rules are route-local. Configured non-default alias is currently not middleware-exempt on public host. | Retain as a fallback ingress with stricter proof: worker secret/service identity required before trusting session header or delivering, sender allowlist enforced, route fixed/default or explicitly allowlisted, and accept/reject events audited without raw email/secrets. |
| Telegram bot control | Telegram can wake sessions, execute bot commands, create/rename/delete topics, and relay assistant/user output in current Python. | Removed from the first Rust release. Native app plus email/human fallback replace this external command surface. |
| human/email delivery | Human-recipient and email delivery can wake sessions and relay output through external services. | Retain as a fallback external channel with response-relay attribution, fake-notifier fixtures, secret redaction, and delivery-result observability. |
| GitHub/Codex review side effects | Review routes and durable watches can post GitHub comments, retry polling, and notify sessions. | Preserve repo/PR validation, duplicate locks, retry/comment refresh, cancellation/inactive history, queue-before-terminal-persist notification ordering, and token redaction. |
| Codex event ingestion and reducer persistence | Codex app/fork events, provider cursors, reducer state, assistant relay state, and `/tmp` event/control artifacts drive activity, recovery, SSE, and pending-request behavior. | Validate event schemas, preserve dedupe/replay/skip behavior, persist before cursor advance, bound/redact payload previews, and fixture interrupted/shutdown/delta/remote-bridge error semantics. |
| service install and infra supervision | Launchd install/wrapper scripts, port ownership, health checks, tmux hook install timing, and infra sidecar repair are operational entrypoints. | Document non-destructive process handling, path/permission validation, logs, rollback, and whether moving infra repair after port preflight is a Stage 5 hardening change. |
| persistent files/SQLite | Stage 2 classifies must-preserve/migrate/private stores. | Use safe open/create modes, backups for migrations, WAL/busy-timeout equivalents where needed, schema-version checks, and explicit archival for diagnostics stores. |
| tmux/process control | Session Manager creates tmux sessions, injects input, reads logs, and exposes attach descriptors. | Preserve attach semantics while bounding generated commands, avoiding shell interpolation for internal commands where possible, and retaining record-preserving failure cleanup. |
| SSE/watch/static | Events, watch static fallback, and public/browser routes exist today. | Keep local/auth/proofed diagnostics, bound subscriber queues, preserve retained no-cache/SSE semantics, avoid leaking secrets, and remove unauthenticated public operational browser data. |

### Owner-Approved Attack-Surface Reductions

Stage 5 now makes the cut. These reductions are approved for the first Rust release and should be implemented as retained-core fixtures or retirement/absence fixtures.

| Reduction | Security Value | First Rust Release Decision |
| --- | --- | --- |
| Generic public browser/watch operational data is removed. | Shrinks public HTTP read/status surface and Google/browser session exposure. | Accepted. Local/auth/proofed diagnostics may remain. |
| Public remote access requires Cloudflare/public-edge proof before origin. | Blocks unauthenticated internet traffic before the SM process sees it. | Accepted. Origin still requires OAuth/device-bearer or node auth plus route-local checks. |
| Non-loopback hooks require route-local proof/secret. | Reduces forged public hook mutation risk if middleware/auth is misconfigured. | Accepted. No-secret remote configs fail closed or become local-only. |
| Mutating session graph APIs get explicit capability checks. | Reduces compromised-agent abuse of kill/input/spawn/role/control APIs. | Accepted for retained mutating APIs, even when it removes unsafe legacy authority. |
| Public sensitive read APIs, AI summary provider HTTP route, and `sm what` are removed. | Reduces transcript/tool/provider-output leakage and avoids low-capability summarization errors. | Accepted. Use `sm tail --raw`, `sm output`, or explicit `sm send` status prompts instead. |
| Browser/device auth signing rotation may require re-login/re-enrollment. | Reduces stale token risk during proofed cutover. | Accepted when recorded as an explicit cutover gate. |
| Inbound email is retained but proofed and narrowed. | Keeps a fallback external channel without preserving ambiguous public ingress. | Accepted. Default or explicitly allowlisted webhook routes only; worker proof required; trusted session header ignored unless proof is valid; authorized sender checks preserved. |
| Node-agent hook-secret fallback is rejected. | Separates remote hook and node-agent credentials. | Accepted. |
| Queue runner is narrowed and policy/CI helper sprawl is removed. | Reduces arbitrary asynchronous command-execution surface. | Accepted. |
| Telegram bot/control/topic cleanup is removed. | Removes a large external command surface that bypasses phone/node proof. | Accepted in favor of the native app. |
| `/hooks/tmux-client` stays local-only with the current event allowlist. | Prevents remote spoofing of tmux client state. | Accepted. |
| App artifacts require auth/proof or signing before public serving. | Improves native update trust. | Accepted. |
| Local bypass is tightened to same-user loopback only. | Prevents accidental public/proxy trust expansion. | Accepted. |
| Service install/cutover uses process ownership proof instead of arbitrary port killing. | Reduces destructive operations during cutover. | Accepted. |

### Non-Breaking Hardening Requirements

Rust implementation tickets should include these by default:

- centralize auth decisions in a small policy layer fed by the Stage 2 route auth matrix, then keep route-local secret checks explicit.
- make public route defaults deny-by-default; adding an exemption requires a threat-register row and contract tests.
- define typed request/response/frame structs with size limits for HTTP bodies, hook payloads, WebSocket frames, SSE queues, app uploads, and raw email.
- use constant-time comparison for ticket secrets, device signatures, hook secrets, node tokens, worker secret, and session tokens where practical.
- make mobile terminal ticket consume/update atomic across expiry, quota, revocation, and active-attach creation.
- preserve kill switches for mobile terminal, codex rollout/app retirement, queue runner cancellation, node-agent disconnect, and service shutdown.
- maintain audit records for destructive/sensitive tools, app uploads, mobile attach ticket mint/consume/disable, inbound email acceptance/rejection, node-agent auth failures, queue job creation/cancel, and public auth failures where privacy-safe.
- keep secret-bearing fields out of `/nodes`, bootstrap, attach descriptors, app metadata, watch payloads, logs, and errors.
- validate filesystem paths against configured roots for artifacts, bug-report attachments, node log/control paths, tmux sockets, queue-runner scripts, and migration backups.
- treat durable persistence migrations as fail-fast and backup-first; partial migration must not leave Python and Rust disagreeing about ownership.
- include abuse-case tests in the contract harness, not only successful compatibility fixtures.

### Residual Risks

Some risks remain even after a careful Rust port:

- A compromised operator account or local process can still control tmux, read local state, or invoke `sm`; Rust cannot make same-user local execution a hard boundary.
- Public mobile and browser access depend on correct Google/device configuration, TLS/proxy setup, and secret hygiene outside the binary.
- Remote nodes intentionally execute work on other machines; a compromised node can lie about local execution state or leak data available on that node.
- Email, GitHub, and provider events can carry malicious content that influences agents after attribution; hardening can preserve boundaries but not make agent interpretation safe. Telegram is removed from the Rust target and remains rollback-only.
- Queue-runner jobs intentionally execute commands; policy gates and audit reduce risk but do not remove operator-approved command execution.
- App artifact serving remains a native update surface, but Stage 5 requires auth/proof or signing before public serving.

### Stage 4 Completion Criteria

Before Stage 4 converges:

- the threat register must cover every Stage 2 security-sensitive surface and every Stage 3 behavior that mutates shell/session/durable/external-channel state.
- every public or auth-exempt route must have an explicit abuse case, current control, Rust mitigation, and residual-risk classification.
- every route-local secret mechanism must state what happens when the secret is missing, mismatched, reused, logged, or rotated.
- every attack-surface reduction must receive a Stage 5 accepted/rejected disposition with rationale before implementation tickets are filed.
- reviewer findings must explicitly check native mobile attach/auth/bootstrap and not collapse it into lower-priority browser watch behavior.
- Stage 5 must receive a list of user-review decisions, migration backups, rollback gates, observability requirements, and cutover kill switches.

## Stage 5: Migration Execution And Rollout

Status: converged after three sequential independent reviewer convergence signals; owner security feedback incorporated after convergence.

Stage 5 turns the compatibility inventory, behavior handoff, and threat model into an execution plan. The goal is to make the Rust migration measurable and reversible before any implementation ticket claims runtime ownership.

Owner security feedback after the staged reviews changes the migration from broad compatibility preservation to an explicit cutover port. Generic public `sm.rajeshgo.li` should not return operational data outside an auth/proof boundary, and the high-value remote path is the native mobile app through a device-proofed public-edge/tunnel boundary. Registered nodes may use the same proof-of-possession public edge as a fallback when LAN `studio.local` is unavailable. Email/human recipient delivery and inbound email remain the fallback external channel after Telegram removal. Public browser data, Telegram control, `sm what`, the redundant `sm kill` CLI alias, dispatch, Termux, watch-job, standalone reminders, queue policy/CI helper sprawl, public unauthenticated artifacts, and similar cruft are no longer Rust targets.

The owner-approved Stage 5 position is a deliberate cutover scope:

- Rust must preserve current outward behavior only for retained core surfaces.
- Removed surfaces in the cutover scope are not Rust compatibility targets; implementation tickets must assert absence or explicit retirement errors.
- native `sm` mobile app flows and on-the-go attach remain first-class release blockers.
- email/human recipient delivery and inbound email remain the fallback external channel after Telegram removal.
- generic public browser/watch operational data is removed; local/authenticated/proofed diagnostics may remain.
- Telegram, `sm what`, the redundant `sm kill` CLI alias, dispatch, Termux attach, watch-job, standalone reminders, queue policy/CI helpers, and public unauthenticated artifacts are not ported.
- retiring the `sm kill` CLI alias does not retire retained HTTP/mobile session-stop APIs; preserve `POST /sessions/{target_session_id}/kill` behavior or retarget the native app through a reviewed replacement with fixtures.
- public remote access requires public-edge/device or node proof before origin, followed by origin auth/capability checks.
- Python and Rust must not both write the same durable stores during normal operation.
- rollout must be backup-first, rehearsal-first, and rollback-tested before live cutover.

### Stage 5 Execution Artifacts

| Artifact | Purpose |
| --- | --- |
| [index](762_stage5_artifacts/index.md) | Stage 5 artifact bundle entry point and reviewer handoff. |
| [cutover scope](762_stage5_artifacts/cutover_scope.md) | Owner-approved retained core and removed surfaces for the Rust cutover. |
| [rollout plan](762_stage5_artifacts/rollout_plan.md) | Cutover sequence, coexistence boundary, rollback, kill switches, and user-review disposition. |
| [state ownership and migration](762_stage5_artifacts/state_ownership_and_migration.md) | Store-by-store ownership, backup, migration, rollback, and downgrade rules. |
| [gate matrix](762_stage5_artifacts/gate_matrix.md) | Falsifiable value gate, compatibility/security/ops gates, and observability requirements. |
| [implementation workstreams](762_stage5_artifacts/implementation_workstreams.md) | Epic split and sequencing for implementation tickets after this spec converges. |

### Release Shape

The first Rust release should be server/runtime-first plus the retained core CLI. It should not provide broad compatibility shims for removed surfaces.

- Rust may replace the server/runtime only after the route, protocol, persistence, behavior, security, migration, and rollback gates pass.
- The `sm` CLI may move command-by-command for retained commands. Removed commands must be absent or return clear retirement errors; they do not continue through Python shims.
- Retained Python CLI commands may remain only when they are HTTP-only, read-only against compatible stores, explicitly CLI-owned local writers with no Rust writer, or routed through Rust for writes. Direct local writes such as lock/worktree commands need an ownership and retirement gate.
- Existing ports, URLs, config paths, state paths, launchd identity, mobile app headers, and externally visible status/error strings remain contracts only for retained surfaces.
- Public remote-access architecture must keep the Session Manager origin private or loopback-scoped where feasible, expose only an allowlisted public edge, require mobile/browser callers or remote-node fallback traffic to authenticate or prove possession before forwarding to the origin, and require worker proof/service identity for inbound email ingress.
- A compatibility shim is acceptable only for retained surfaces, when it does not create ambiguous writer ownership and has an explicit retirement gate.

### Falsifiable Value Gate

The migration should pause, narrow, or recommend Python hardening instead of a full rewrite if the evidence does not support the Rust thesis.

Before runtime replacement tickets are filed, Stage 5 requires current Python baselines for memory, startup/restore, retained route latency, retained CLI latency, hook latency, queue wake delivery, mobile attach-ticket mint or its replacement attach proof, mobile terminal WebSocket auth, node reconnect, Codex event ingestion, SSE initial snapshot, and retained usage. Rust must be compared against current Python plus feasible Python hardening/config changes under the same retained-state workloads. Removed surfaces require retirement tests, not latency parity.

Acceptance targets are in the gate matrix. In short: Rust must clear a quantitative memory gate against the best measured Python-compatible baseline, must not regress first-class mobile/hook/queue/session-control p95 latency by more than 10% without an approved mitigation, must preserve recovery ordering, and must pass zero-regression compatibility fixtures for first-class release surfaces.

### Coexistence Boundary

Python owns live state until Rust cutover begins. Rust rehearsal runs against copied real state. Normal operation is single-writer:

- no concurrent Python/Rust writes to `sessions.json`, message queue, response relay, Codex events/requests, queue runner stores, Telegram topics, app artifacts, bug reports, locks, or config.
- no concurrent Python/Rust writes to audit/telemetry stores, including `tool_usage.db`, `telegram_telemetry`, codex observability, request timing inputs, or migration logs.
- no dual-write design unless it receives a separate crash-window/state-consistency review.
- live cutover requires a write-admission freeze, active-writer drain, and final verified backup after the freeze. A pre-freeze backup is only a safety snapshot, not the default rollback restore point.
- process-local state such as mobile terminal tickets can be lost on restart because current Python behaves that way.
- browser sessions and native device bearer tokens may be invalidated during the proofed cutover only through an explicit re-login/re-enrollment gate recorded in the migration ledger.

### Cutover And Rollback Gates

Live cutover is blocked until all of these are true:

- Python contract fixtures pass and are recorded as the baseline.
- Rust contract fixtures pass against copied real state.
- write admission is frozen or journaled for live Python writers.
- write-freeze coverage is generated from the full Stage 5 state-ownership table and Stage 2 persistence manifest, not a hand-maintained subset.
- active risky writers are drained or explicitly risk-accepted: mobile terminal attaches, queue-runner jobs, active node-agent control streams, in-flight Codex pending requests, review/job watches, queue delivery, response relay delivery, provider event ingestion, inbound email admission, outbound email/human delivery, other external notifier sends, tool audit writes, Telegram telemetry archive writes, bug-report create/update/prune writes, codex observability/log-prune writers, request timing/log writers used for telemetry, and CLI/shim write paths.
- final post-freeze backups are created and verified for every must-preserve store and operational artifact, with a ledger proving no accepted writes landed after the restore point unless journaled.
- rollback to Python is rehearsed and smoke-tested.
- launchd/service handoff proves process ownership before stopping anything on the service port.
- observability exists for auth failures, route-local secret failures, mobile attach, queue depth, node agents, Codex cursors, retired-surface denials, migration progress, and slow requests.

Rollback must restore Python service ownership and Python-compatible state from the final post-freeze backup by default. If Rust has written state that Python cannot read, rollback requires restoring backups or explicit owner approval to discard Rust-only changes. Rollback is unacceptable if it silently loses accepted audit, telemetry, bug-report, observability, queue, cursor, artifact, or session writes after the restore point without a ledgered replay/discard decision.

### Cutover Decisions

For the first Rust release, Stage 5 now accepts the breaking reductions recorded in [cutover scope](762_stage5_artifacts/cutover_scope.md) and [rollout plan](762_stage5_artifacts/rollout_plan.md). Public browser/watch operational data, Telegram control, `sm what`, the redundant `sm kill` CLI alias, dispatch, Termux attach, watch-job, standalone reminders, queue policy/CI helpers, public unauthenticated app artifacts, non-loopback hook permissiveness, node hook-secret fallback, and unsafe legacy authority assumptions are not compatibility targets. Email/human recipient delivery and inbound email stay retained as the fallback external channel, with stricter worker proof and route allowlisting. Rust should keep the stable core and make removed surfaces fail clearly.

Hardening remains required for retained surfaces: typed auth policies, secret redaction, abuse-case fixtures, route-local secret tests, mobile attach atomicity or replacement proof fixtures, payload/path validation, audit reliability, and operator-visible diagnostics.

### Stage 5 Completion Criteria

Before Stage 5 converges:

- every Stage 4 cutover candidate must be accepted or rejected for the first Rust release; no deprecation candidate remains deferred by default.
- the single-writer coexistence boundary must be explicit for every must-preserve store.
- the migration tool requirements must include preflight, rehearse, safety backup, freeze, drain, final backup, cutover, rollback, and status.
- launchd/service cutover must avoid arbitrary process killing without ownership proof.
- backup and rollback must be specific enough to rehearse against copied real state, including a final post-freeze restore point.
- baseline workloads and target thresholds must be measurable.
- native mobile app and on-the-go attach must remain first-class in cutover and smoke gates.
- CLI/shim sequencing must classify direct state access and ownership.
- implementation workstreams must be split into tickets with dependencies and exit gates.
- reviewers must verify that Stage 5's approved cuts match the owner-approved cutover scope and that no extra breaking change was introduced silently.

## Ticket Classification

Epic. This cannot be completed by one implementation agent without context compaction. The final spec should graduate into multiple implementation tickets, likely split by compatibility surface, persistence/migration, runtime core, integrations, and ruggedization hardening.

Expected workstreams include:

- contract and baseline test harness.
- outward surface compatibility.
- persistence and state migration.
- runtime core and lifecycle management.
- tmux/process orchestration.
- message queue, parent-wake, notify-on-stop, response-relay, and Codex-review behavior.
- integrations: native mobile, email/human fallback delivery, inbound email, proofed artifacts, GitHub/Codex reviews, and remote nodes.
- retired-surface absence fixtures for Telegram, `sm what`, the `sm kill` CLI alias, dispatch, Termux, watch-job, standalone reminders, queue policy/CI helpers, public browser operational data, and public unauthenticated artifacts.
- ruggedization and threat-model mitigations.
- migration execution, cutover, rollback, and operations.
