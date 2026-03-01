# sm#316: Thin Codex Fork for First-Class SM Lifecycle, Telemetry, and Control (P0/P1)

## Problem

`session-manager` currently treats the Codex interactive CLI path as a second-class provider in operational workflows:

1. No stable, public machine contract for lifecycle state from interactive Codex sessions.
2. No first-class tool-use telemetry contract equivalent to Claude hook logging.
3. No robust programmatic control plane for this path without tmux/session-env coupling.

We previously attempted to solve this by building a custom TUI path, but that approach creates high maintenance cost and drifts from upstream Codex behavior.

## Decision

Adopt a **thin fork** of `openai/codex` and expose a minimal stable integration layer for SM:

1. Public lifecycle state machine contract.
2. Public tool-use telemetry stream/hooks.
3. Programmatic control channel for live sessions.

Do not fork core product behavior or UI architecture beyond what is needed for these contracts.

## Alternatives Considered

### Option A: Use existing `provider=codex-app` path only (no fork)

Evidence this is already real and integrated:

1. Upstream Codex app-server already exposes documented lifecycle/control/event contracts (`/tmp/codex-investigation/codex/codex-rs/app-server/README.md`).
2. This repo already integrates app-server transport and control:
   - `src/codex_app_server.py`
   - `src/codex_event_store.py`
   - `src/server.py`
   - `src/session_manager.py`

Why this does not fully solve the current request:

1. Current operator workflow is on the Codex interactive CLI path (`provider=codex`) rather than `provider=codex-app`.
2. Product direction for this ticket is to make the Codex path first-class without depending on tmux/session-env coupling.
3. Solving only `codex-app` would leave the active Codex operator path unresolved.

Decision:

1. Deliver first-class lifecycle/telemetry/control for the Codex interactive path via thin fork (`provider=codex-fork`).
2. Deprecate and remove `provider=codex-app` after codex-fork reaches parity and rollout gates pass.
3. Reassess de-forking if upstream Codex ships equivalent native contracts for interactive sessions.

## Context and Findings

Investigation against upstream Codex (`openai/codex`, commit `6a673e733`, 2026-02-28) found:

1. Interactive TUI already has internal session JSONL logging with rich event payloads.
2. Protocol already exposes events sufficient for lifecycle derivation (`TurnStarted`, `TurnComplete`, `ExecApprovalRequest`, `RequestUserInput`, `ShutdownComplete`, tool begin/end events).
3. `after_tool_use` hook payload types and dispatch logic already exist in core tool pipeline.
4. Current user-facing config primarily exposes legacy `notify`; `after_tool_use` is not surfaced as a first-class configurable hook path.
5. A hidden stdio-to-UDS relay utility exists, but no public control API for a running interactive TUI session.
6. Interactive CLI currently does not expose a public `--event-stream` contract surface.
7. Interactive CLI currently does not support detached background runtime + reattach to the same live process.

Conclusion: P0 and part of P1 are mostly productization of existing internals, while detached-runtime/reattach in P1 is explicit net-new architecture with higher implementation risk and dedicated gating.

### Investigation Path

Codex was cloned locally for this investigation at:

`/tmp/codex-investigation/codex`

## Goals

### P0

1. Stable lifecycle contract for `provider=codex-fork` consumable by SM without scraping terminal output.
2. Stable tool-use telemetry contract with turn/call IDs and execution outcome.
3. Robust idle/stopped/waiting state detection for `provider=codex-fork` without tmux heuristics.

### P1

1. Programmatic inbound message/operation delivery into a live `provider=codex-fork` session.
2. Programmatic outbound delivery of events/responses from live `provider=codex-fork` session.
3. Launch: remove tmux transcript/control scraping and `CLAUDE_SESSION_ID` coupling for Codex control paths.
4. Post-launch: remove tmux runtime dependency via detached-runtime/reattach architecture.

## Delivery Tiers

1. MVP (`#1`-`#3`): ship public event contract + lifecycle reducer + tool-use telemetry.
2. MLP (`#4`): ship programmatic control for `sm send`/dispatch/watch/remind equivalents while keeping tmux as runtime container.
3. Post-launch: detached runtime + reattach architecture (no longer launch-blocking).

## Non-Goals

1. Replacing Codex TUI UX with an SM-specific UI.
2. Deep fork divergence from upstream event model or tool runtime.
3. Building a generic remote-control protocol for all future agent providers in this ticket.

## Provider Scope and Migration Contract

Provider behavior in this epic:

1. `provider=codex`: existing tmux-driven behavior remains temporarily for backward compatibility during rollout.
2. `provider=codex-app`: enters deprecation immediately in this epic and is removed after codex-fork cutover criteria are met.
3. `provider=codex-fork`: target provider for Codex workflows, using explicit event/control contracts.

Defaulting and migration:

1. Initial rollout keeps current defaults unchanged.
2. `codex-fork` is opt-in behind a feature flag.
3. Promote `codex-fork` default for Codex CLI users only after rollout gates pass.
4. Disable new `provider=codex-app` session creation after codex-fork reaches parity.
5. Remove `provider=codex-app` execution path and CLI exposure in final cutover phase.

Operational invariant:

1. `sm status`, `sm wait`, `sm send`, and watch surfaces must disclose provider and active capability mode so operators can tell which state/control semantics are in effect.

CLI/API provider selection contract:

| Phase | CLI command | Provider selected | Notes |
|---|---|---|---|
| Pre-cutover | `sm codex` | `provider=codex` | Current behavior. |
| Pre-cutover | `sm codex-app` | `provider=codex-app` | Allowed with deprecation warning once enabled. |
| Pre-cutover | `sm codex --provider codex-fork` (or `sm spawn --provider codex-fork`) | `provider=codex-fork` | Explicit opt-in path. |
| Default-switched migration window | `sm codex` | `provider=codex-fork` | New default path. |
| Default-switched migration window | `sm codex --provider codex` | `provider=codex` | Explicit rollback selection. |
| Default-switched migration window | `sm codex-app` | Rejected | Error text: `provider=codex-app is deprecated; use sm codex (default codex-fork) or --provider codex for rollback.` |
| Post-cutover | `sm codex` | `provider=codex-fork` | Stable default. |
| Post-cutover | `sm codex --provider codex` | `provider=codex` | Temporary emergency path only if rollback window still active. |
| Post-cutover | `sm codex-app` or API `provider=codex-app` | Rejected | Hard error: provider removed. |

If `--provider` selection is not currently available on the relevant command path, this epic must add it before default switch.

Cutover handling for pre-existing `provider=codex-app` sessions:

1. In-place migration from codex-app session runtime to codex-fork is **not supported**.
2. During deprecation window:
   - existing codex-app sessions may be listed/read for observability,
   - new codex-app sessions are blocked,
   - operator-facing warnings are shown on status/watch/attach attempts.
3. At final cutover:
   - any running codex-app sessions are force-stopped with terminal reason `provider_retired_codex_app`,
   - `sm send`/approval/user-input actions against codex-app sessions are rejected with migration guidance,
   - restore/bootstrap must not auto-resume codex-app sessions.
4. State/DB cleanup:
   - clear codex-app pending request ledgers and queued message artifacts with explicit terminal error reason,
   - retain historical codex-app event/tool records read-only until normal retention expiry,
   - mark codex-app sessions in status/watch as `retired` so operators have deterministic visibility.

## Proposed Architecture

## 1) Codex Bridge Contract (new public surface in fork)

Expose a single machine stream from interactive Codex:

1. `--event-stream <path-or-stdout>`: JSONL event output.
2. `--event-schema-version <int>`: explicit contract version pin.

Feasibility framing:

1. This is a **net-new fork surface** for interactive Codex (not an existing upstream interactive CLI flag).

Naming constraint:

1. Bridge flags are intentionally provider-generic (`event-*`, `control-*`), not SM-branded, so the fork remains broadly useful outside Session Manager.

Wire source:

1. Existing internal event feed used by TUI/session logging.
2. Existing outbound op submission points.
3. Existing session start/end metadata.

Contract requirements:

1. Each record includes `schema_version`, `ts`, `session_id`, `event_type`, `payload`.
2. Backward-compatible evolution with additive fields only within a schema version.
3. No reliance on undocumented internal env vars for production SM behavior.

Payload basis and compatibility rules:

1. `codex_event` records must preserve upstream event payload shape parity for protocol events wherever possible.
2. Any fork-added metadata (`seq`, `session_epoch`, bridge annotations) must be additive and namespaced to avoid payload collisions.
3. Schema versioning policy must define:
   - additive-only changes within a version,
   - explicit major bump for breaking field/type changes,
   - fixture-backed compatibility tests across at least previous + current schema versions.

## 2) Lifecycle State Machine Contract (Codex fork + SM reducer)

Canonical states:

1. `running`
2. `idle`
3. `waiting_on_approval`
4. `waiting_on_user_input`
5. `shutdown`
6. `error`

Reducer rules (single active state with deterministic transitions):

1. `TurnStarted` -> `running`
2. `ExecApprovalRequest` or patch approval request -> `waiting_on_approval`
3. `RequestUserInput`/elicitation -> `waiting_on_user_input`
4. Approval/user-input resolution resumes prior active state:
   - if turn still active -> `running`
   - otherwise -> `idle`
5. `TurnComplete`/`TurnAborted` -> `idle`
6. `ShutdownComplete` -> `shutdown`
7. Fatal stream/runtime error -> `error`

State contract must include the transition cause event for audit/debug.

## 3) Tool-Use Telemetry Contract

Expose first-class `after_tool_use` hook configuration and event publication:

1. Config key(s) in Codex fork for tool-use hook command(s), similar UX to existing `notify`.
2. Event payload fields include:
   - `turn_id`
   - `call_id`
   - `tool_name`
   - `tool_kind`
   - `tool_input` (sanitized contract shape)
   - `executed`
   - `success`
   - `duration_ms`
   - `mutating`
   - sandbox metadata
   - output preview (bounded)
3. Hook failure policy remains explicit (`continue` vs `abort`) and observable in telemetry.

SM will ingest this stream into `tool_usage.db` (or provider-specific equivalent) with stable indexing on `session_id`, `turn_id`, `call_id`, `ts`.

Security and privacy requirements:

1. Redact high-risk values before persistence or hook dispatch:
   - bearer/API tokens
   - auth headers/cookies
   - known secret-like env var values
   - access keys in command arguments where pattern-matched
2. `tool_input` must use provider-defined sanitized shape:
   - cap argument lengths
   - truncate oversized blobs with explicit truncation marker
   - never persist raw binary payloads
3. `output_preview` must be bounded and redacted with the same secret filters.
4. Storage policy:
   - tag records with provider and schema version
   - apply existing SM retention controls; if unset, default codex-fork telemetry retention to 30 days
   - ensure deletion/export tooling can target codex-fork telemetry records independently
5. Hook execution receives only sanitized payloads, never unredacted raw payloads.

## 4) Programmatic Control Channel (P1)

Add optional local control plane endpoint for a running session:

1. `--control-socket <uds-path>` to bind per-session UDS.
2. Request types:
   - submit user message/op
   - submit approval decision
   - submit request-user-input response
   - graceful shutdown
3. Response model:
   - ack/nack with error code + message
   - correlation ID for request tracking
4. Security:
   - local-user permissions (`0600`)
   - socket path under provider-managed runtime dir
   - reject requests after shutdown

This is a narrow control API for SM reliability, not a general RPC framework.

Launch runtime assumption:

1. Programmatic control is required at launch while the session runtime may still be hosted inside tmux.
2. This delivers the core operator benefit (no tmux scraping for message/control paths) without requiring detached-runtime architecture in launch scope.

Ordering, replay, and idempotency requirements:

1. Event stream ordering:
   - every emitted event includes a monotonic per-session sequence number (`seq`)
   - consumers treat `seq` as source-of-truth ordering key
2. Control request correlation:
   - every control request requires `request_id`
   - every ack/nack includes `request_id`
3. Idempotency:
   - duplicate `request_id` for the same live session must not double-apply side effects
   - server returns prior result for duplicate request IDs when available
4. Restart/reconnect behavior:
   - on session restart, stream emits a new `session_epoch`
   - SM reducer resets in-flight pending request bookkeeping when epoch changes
   - stale requests from prior epoch are rejected with explicit error code
5. Approval/user-input correctness:
   - approval and request-user-input responses must reference active request IDs
   - mismatched/expired request IDs are rejected deterministically
6. Shutdown semantics:
   - `graceful_shutdown` is acked only when accepted into shutdown flow
   - stream must emit terminal `ShutdownComplete` (or `error`) for completion visibility

Detached interactive runtime contract (post-launch):

Feasibility framing:

1. This is a **net-new fork architecture** (not current upstream interactive behavior).
2. Delivery requires explicit daemon/runtime ownership, attach protocol semantics, and IPC lifecycle guarantees.

Runtime requirements:

1. The Codex fork runtime can be started detached (background process) with event stream + control socket active even without an attached foreground terminal.
2. `sm attach` re-attaches a terminal UI to the existing running interactive session; it does not create a new session.
3. Detach/reattach must not interrupt turn execution, pending approvals, or control socket availability.
4. For SM workflows, this provides codex-app-like operational behavior (programmatic input/output + lifecycle control) while keeping the interactive TUI path.

## 5) Session Manager Integration

Add/extend Codex provider adapter in SM:

1. Spawn forked Codex with:
   - event stream destination
   - schema version pin
   - control socket path (when enabled)
2. Replace tmux output heuristics for lifecycle with event-driven reducer.
3. Persist lifecycle transitions and tool events via existing observability infrastructure.
4. `sm send` for codex-fork routes through control socket when available; tmux fallback only if explicitly configured.
5. `sm wait` and monitoring commands read reducer state instead of transcript timing heuristics.

## 6) Upstream Compatibility Strategy

Keep fork maintenance narrow and predictable:

1. Maintain a patch surface isolated to bridge/config/control files where possible.
2. Rebase/merge from upstream on a fixed cadence (weekly or biweekly).
3. Add CI conformance checks validating:
   - schema output shape
   - lifecycle reducer transition invariants
   - control socket request/response behavior
4. Publish a compatibility matrix:
   - `sm` version -> fork commit/tag -> schema version.

Exit strategy:

1. If upstream Codex ships equivalent native contracts, deprecate fork-specific flags and transition SM to upstream.

Fork distribution and rollback requirements:

1. Publish signed fork artifacts per supported platform (at least macOS + Linux for current operator usage).
2. Pin SM to explicit fork version/commit in provider metadata.
3. Add operator-visible command to report active fork version and schema version.
4. Rollback path:
   - one-command provider fallback to `codex` during migration window
   - preserve session metadata/observability continuity across rollback

## 7) Operator Migration and Cutover Steps

1. Publish codex-app deprecation notice with timeline and migration guidance.
2. Add preflight checks to report sessions/configs still using codex-app.
3. Run codex-fork canary validation for operator workflows (`sm send`, `sm wait`, watch).
4. Switch Codex defaults to `provider=codex-fork` after cutover gates pass.
5. Block new codex-app session creation and return migration guidance in CLI/server responses.
6. Remove codex-app provider selection/execution paths after deprecation window.
7. Keep rollback target as `provider=codex` only during migration window.
8. Remove migration-window rollback override after stabilization.

## Implementation Plan

### Phase A0 (P0 Core): Event stream contract productization (net-new)

1. Codex fork: add `--event-stream` and `--event-schema-version` interactive CLI surfaces.
2. Codex fork: define event record schema and payload-basis rules in docs and fixtures.
3. Tests:
   - schema fixture tests for required fields and type stability
   - compatibility tests for previous/current schema versions
   - parity tests for upstream protocol payload passthrough
4. Gate A0 (must pass before Phase A):
   - stable event stream contract exists and is consumable from interactive codex-fork sessions.

### Phase A (P0 Core): Lifecycle reducer integration

1. SM: add reducer implementation and provider wiring.
2. SM: switch idle/running/waiting/stopped detection to reducer for `provider=codex-fork` sessions.
3. Tests:
   - replay fixtures for deterministic transitions
   - regression tests for false-idle/false-stopped scenarios

### Phase B (P0 Core): Tool-use telemetry contract

1. Codex fork: expose first-class `after_tool_use` config.
2. SM: ingest tool-use telemetry and map to existing observability records.
3. Tests:
   - successful tool call
   - failed tool call
   - hook abort vs continue semantics
   - payload compatibility tests

### Phase C (P1 / MLP): Programmatic control socket on tmux-hosted runtime

1. Codex fork: add `--control-socket` API with request router.
2. SM: add control client and route `sm send`/approval/input responses via socket.
3. SM fallback policy:
   - if socket unavailable: explicit degraded mode with reason
   - optional legacy tmux fallback only when configured
4. Runtime model for launch:
   - keep tmux as runtime container
   - use control socket for programmatic input/output/control instead of tmux transcript/control scraping
5. Tests:
   - submit message happy path
   - approval path
   - timeout/disconnect/restart behavior
   - socket permissions and stale socket cleanup

### Phase D: Rollout + hardening + codex-app retirement (launch path)

1. Feature flag fork provider path in SM (`provider=codex-fork`).
2. Canary on operator workflows.
3. Promote to default Codex provider after stability criteria.
4. Deprecate then remove `provider=codex-app` path and CLI exposure.
5. Document migration and fallback paths.
6. Ship fork distribution, pinning, and rollback playbook.
7. Execute codex-app cutover migration script for runtime/state cleanup.

Cutover gates (launch):

1. Gate A0 complete (event stream contract productized and compatibility-tested).
2. Lifecycle accuracy parity validated against known false-idle/false-stopped regressions.
3. `sm send` reliability on codex-fork control socket meets or exceeds current codex tmux path.
4. Security/redaction checks pass for telemetry payload fixtures.
5. Operational rollback tested in staging before default switch.
6. codex-app retirement checklist completed:
   - no new codex-app session creation
   - codex-app CLI/provider entrypoints removed
   - rollback target documented as `provider=codex` only
   - pre-existing codex-app sessions handled per retirement policy (force-stop + retired status + ledger cleanup)

### Phase E (Post-launch): Detached runtime + reattach (net-new architecture)

1. Codex fork: implement detached runtime mode for interactive sessions with explicit runtime ownership model.
2. Codex fork: implement attach protocol to reconnect a terminal UI to the same live session process.
3. SM: ensure `sm attach` semantics map to reattach (not restart/new session) for codex-fork.
4. Tests:
   - detached launch without foreground terminal
   - attach/reattach under active turn
   - attach during pending approval/user-input
   - no interruption of control socket or event stream during detach/reattach
5. Gate E (post-launch):
   - detached runtime reliability meets post-launch acceptance criteria in staging.

## Acceptance Criteria

### P0 Acceptance Criteria

1. SM lifecycle state for `provider=codex-fork` sessions comes from event contract, not tmux transcript parsing.
2. `sm wait` and status reporting correctly distinguish:
   - running
   - waiting approval
   - waiting user input
   - idle
   - shutdown
3. Tool-use events for `provider=codex-fork` are persisted with stable IDs and timestamps.
4. Observability views show codex-fork tool usage with parity to Claude provider fields where applicable.
5. Integration remains functional across at least one upstream Codex update without schema break.
6. `provider=codex-app` is no longer selectable or executable after final cutover.
7. Pre-existing codex-app sessions are deterministically retired (not migrated), and related pending ledgers/queues are cleaned with explicit operator-visible reasons.
8. `--event-stream` and `--event-schema-version` are delivered as documented net-new interactive fork surfaces with schema compatibility tests.

### P1 Acceptance Criteria

1. SM can deliver user messages programmatically to live `provider=codex-fork` session via control socket.
2. SM can deliver approval and user-input responses programmatically.
3. End-to-end interaction succeeds via control socket in primary mode without tmux transcript/control scraping.
4. Failure modes are explicit (timeout/disconnect/rejected request) and surfaced to operator.
5. Control channel security baseline enforced (socket perms, lifecycle cleanup).
6. Rollback documentation and runbook specify `provider=codex` as the only supported rollback target.
7. CLI/API provider selection behavior matches the migration mapping table across pre-cutover, default-switched, and post-cutover phases.

### Post-launch Acceptance Criteria

1. Detached runtime + reattach behavior is validated end-to-end (background runtime, reattach to same live session, no interruption of turn/control/event streams).

## Test Plan

1. Unit tests for reducer state transitions from captured event fixtures.
2. Contract tests for event schema version, required fields, and version compatibility fixtures.
3. Integration tests:
   - spawn forked codex session
   - process event stream
   - drive message -> tool -> completion loop
4. Integration tests for approval/user-input pause and resume behavior.
5. Integration tests for control socket request/response.
6. Regression tests for pre-existing false idle/stopped edge cases.
7. Manual validation with operator workflows (`sm spawn`, `sm send`, `sm wait`, `sm watch`).
8. Post-launch: manual validation of detached runtime + `sm attach` reattach semantics under active and waiting states.

## Risks and Mitigations

1. Upstream event drift.
   - Mitigation: schema version pin + compatibility tests + small patch surface.
2. Fork maintenance overhead.
   - Mitigation: isolate changes, maintain patch manifest, scheduled upstream sync.
3. Cutover confusion during codex-app retirement.
   - Mitigation: explicit deprecation warnings, migration command guidance, and provider capability reporting.
4. Control channel reliability under process restarts.
   - Mitigation: reconnect strategy, stale socket cleanup, idempotent request IDs where feasible.

## Operational Notes

1. Near-term pragmatic option remains available: consume current internal Codex session log env vars for temporary bootstrap.
2. This ticket formalizes and stabilizes that behavior so SM no longer depends on undocumented internals.

## Proposed Epic Breakdown

1. `A`: Codex fork event stream public contract + schema versioning.
2. `B`: SM lifecycle reducer integration for Codex provider.
3. `C`: Codex fork tool-use hook config productization.
4. `D`: SM telemetry ingestion parity for Codex tool-use events.
5. `E`: Codex fork programmatic control socket.
6. `F`: SM control client integration + control-socket operation on tmux-hosted runtime.
7. `G`: codex-app deprecation/removal plan implementation and operator surfacing.
8. `H`: fork artifact distribution, version pinning, and rollback mechanism.
9. `I`: rollout/cutover gates, hardening, and migration docs.
10. `J`: codex-app existing-session retirement, ledger cleanup, and CLI/API provider mapping enforcement.
11. `K` (Post-launch): Codex fork detached-runtime + reattach architecture implementation (maps to Phase E / Gate E).

## Ticket Classification

**Epic.** Scope crosses forked Codex runtime surfaces plus SM provider/control-path integration and cannot be completed safely as a single implementation ticket without high context compaction risk.
