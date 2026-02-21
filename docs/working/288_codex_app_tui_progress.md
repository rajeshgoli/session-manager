# sm#288 — Codex app-server as a first-class session citizen (user narrative)

## The user experience we are building

Scope note: in this doc, "Codex sessions" means `provider=codex-app` unless explicitly stated otherwise.

You are running multiple sessions and delegating work. You should not have to guess whether Codex is busy, blocked, waiting, or done. You should not have to choose between native Codex usability and session-manager observability.

With this change, Codex app-server sessions behave like real managed workers:

- attachable in terminal
- observable in real time
- input-capable from the same pane
- measurable for progress and idle state

This makes `provider=codex-app` usable in the same operational model as Claude sessions, instead of a side path with weaker controls.

## What you get as a user

You get a single workflow where control and visibility live together.

1. You can open `sm codex-tui <session_id>` and immediately see:
- what turn is running
- whether the model is thinking, actively emitting, waiting for input, waiting for permission, idle, or stopped
- event timeline and recent delta output
2. You can type directly in that pane and either send a normal follow-up turn or respond to a pending approval/input request.
3. You can still use `sm send` from any shell for normal turns; TUI and `sm send` share the same turn-input endpoint and queue lifecycle.
4. You can trust idle/working transitions because they are computed from Codex app events (`turn/started`, deltas, `turn/completed`) instead of tmux heuristics.

Result: less babysitting, fewer blind spots, faster intervention when a session stalls or needs a nudge.

## Why this matters in the EM pattern

In the EM pattern, the parent agent delegates to children and avoids burning tokens while waiting. The manager role is about throughput and control, not watching terminals all day.

This feature improves that pattern directly:

- You can delegate to Codex children without losing visibility.
- You can check progress by state, not by polling noisy output.
- You can detect blocked children quickly (`waiting_permission`, long `thinking`, no fresh deltas).
- You can send corrective instructions immediately from the same attached pane.
- You can correlate completion/idle notifications with turn timeline and tool activity context.

Operationally, this turns Codex children from "black boxes that sometimes answer" into managed workers with explicit lifecycle signals.

## End-to-end EM workflow (how it feels)

1. Spawn a child for implementation or review work.
2. Continue your own work or go idle.
3. Open/attach Codex TUI only when needed.
4. Read clear state (`working`, `thinking`, `waiting_input`, `waiting_permission`, `idle`, `stopped`) and event feed.
5. If needed, send a targeted follow-up from the TUI input box or with `sm send`.
6. Receive completion/idle signal with enough context to decide next routing action.

This preserves the non-blocking EM model while restoring confidence in child progress tracking.

## What session management gains

- Reliable turn tracking for Codex sessions
- Real idle detection based on lifecycle events
- Better SLA-style supervision of child sessions
- Cleaner handoffs because current state is explicit
- Less context waste from manual polling and guesswork

## Evidence baseline from current implementation

- `src/codex_app_server.py` currently auto-responds to `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, and `item/tool/requestUserInput` immediately, including an explicit "avoid blocking" path for user input.
- `src/session_manager.py` treats `provider=codex-app` through a dedicated JSON-RPC path; tmux-based `provider=codex` uses a different delivery/control surface.
- `src/tool_logger.py` currently has no retention/prune job for its SQLite event table, which is evidence that retention policy must be explicit in the new Codex observability DB design.

## Interactive control model (required, in scope)

Approval and request-user-input response are in scope for this epic. The control plane is intentionally split into two explicit channels:

Required migration from legacy behavior:

- `provider=codex-app` currently auto-responds to structured server requests inside `CodexAppServerSession._handle_server_request`.
- This epic requires removing that direct auto-response behavior for:
- `item/commandExecution/requestApproval`
- `item/fileChange/requestApproval`
- `item/tool/requestUserInput`
- New behavior: server requests are first persisted into `codex_pending_requests`, then resolved only through:
- `POST /sessions/{id}/codex-requests/{request_id}/respond` (user/API path), or
- explicit policy resolution (timeout/restart path) recorded in the same ledger.
- This cutover is mandatory before enabling TUI/API structured-response UX.

- Turn input channel (existing semantics):
- `POST /sessions/{id}/input`
- Used by `sm send` and TUI chat composer mode.
- Enqueues normal user turns and respects current queue lifecycle.
- Hard gate: if unresolved structured requests exist for the session, this endpoint returns `409` with explicit error code `pending_structured_request`.
- `409` payload includes at least the oldest pending item summary: `request_id`, `request_type`, `requested_at`.
- Structured request-response channel (new, required):
- `GET /sessions/{id}/codex-pending-requests`
- `POST /sessions/{id}/codex-requests/{request_id}/respond`
- Used for `requestApproval` and `requestUserInput` items only.
- Payload supports:
- approval: `{ "decision": "accept" | "acceptForSession" | "decline" | "cancel" }`
- user input: `{ "answers": { ... } }`

Correlation/idempotency requirements:

- Every pending request carries `request_id`, `session_id`, `thread_id`, `turn_id`, `item_id`, `request_type`, `requested_at`, `expires_at` (optional).
- `POST .../respond` is idempotent on `request_id`:
- first successful response resolves the request
- repeated submissions return the stored resolved result
- unknown/expired request returns 404 with explicit error code

Durability/restart requirements:

- Pending structured requests are persisted in SQLite (`codex_pending_requests`) before they are exposed to API/TUI.
- Minimum stored fields:
- `request_id` (unique), `session_id`, `thread_id`, `turn_id`, `item_id`, `request_type`
- `requested_at`, `expires_at` (optional), `status` (`pending|resolved|expired|orphaned`)
- `request_payload_json`, `resolved_payload_json` (nullable), `resolved_at` (nullable), `resolution_source` (`tui|api|policy`)
- `error_code` (nullable), `error_message` (nullable) for explicit expiry/orphaned/restart outcomes
- Resolver path is transactional:
- `POST .../respond` atomically transitions `pending -> resolved` and stores the response payload used for idempotent replay.
- App-server response dispatch is driven from that stored resolution (never from unstored transient state).
- Restart reconciliation:
- On process start, unresolved rows from prior app-server process generations are marked `orphaned` with explicit `error_code=server_restarted`.
- `GET .../codex-pending-requests` excludes `orphaned` by default and supports `include_orphaned=true` for audit/debug.
- If Codex re-emits the same logical request after resume, it is recorded as a new pending row with a new `request_id`.
- No unresolved request may remain silently pending across restart.

Timeout/unblock rules (required):

- `POST /sessions/{id}/input` gating checks `status=pending` only. `resolved|expired|orphaned` rows never block normal chat input.
- On expiry while the app-server process generation is alive, resolver transitions:
- `pending -> expired` with `error_code=request_expired`
- then immediately applies policy response from persisted state:
- approval expiry default payload: `{ "decision": "decline" }`
- request-user-input expiry payload: `{ "answers": {} }`
- on successful dispatch, transition `expired -> resolved` with `resolution_source=policy`.
- If dispatch cannot be completed because the originating app-server process generation is gone, transition `expired -> orphaned` with explicit restart/unavailable error code.
- End-state contract: no request remains `pending` after expiry/restart reconciliation.

TUI behavior:

- Displays pending structured requests in a dedicated queue with focus shortcuts.
- Composer has explicit mode (`chat`, `approval`, `input`) so Enter semantics are unambiguous.
- Normal chat text never attempts to satisfy structured requests automatically.
- When pending structured requests exist, chat mode send is disabled and the UI shows an explicit action hint to switch to `approval` or `input` mode.
- This structured-request queue and send-gating behavior is for `provider=codex-app`.

## Durable event history (required, not optional)

Process restart dropping event history is not acceptable for EM workflows. This epic includes durable event history so timeline continuity survives server restarts.

- Codex lifecycle events are written to persistent storage (SQLite) as they are emitted.
- TUI and API read from both:
- a hot in-memory ring for low-latency updates
- durable storage for catch-up and restart recovery
- Event schema includes at least:
- `session_id`, per-session strictly increasing `seq`, timestamp, event type, turn id (if present), compact payload preview
- `seq` contract:
- `seq` is unique for `(session_id, seq)` and starts at `1` for each session
- assignment is transactional at write time (no duplicate or out-of-order persisted sequence after restart)
- `seq` exists only for persisted events (`persisted=true`)
- in-memory-only fallback events use `persisted=false` and `seq=null`
- Retention policy is bounded:
- keep recent window per session (count and age caps) so storage growth is controlled
- API cursor model:
- `GET /sessions/{id}/codex-events?since_seq=<n>&limit=<n>`
- response includes `{ events, earliest_seq, latest_seq, next_seq, history_gap, gap_reason }` (`gap_reason` nullable)
- after restart, client resumes from last seen `seq` and backfills missed events from disk
- if `since_seq < earliest_seq - 1`, server returns oldest retained page and `history_gap=true` (explicit gap; never silent truncation)
- Failure model:
- if persistence write fails, event is still emitted to the hot ring with `persisted=false`, `seq=null`, and an explicit `event_persist_error` metric/log entry is emitted
- cursor API (`since_seq`) returns persisted events only; it never fabricates sequence numbers for unpersisted events
- while any unpersisted event exists after client cursor, response sets `history_gap=true` with `gap_reason=persistence_error`
- once persistence recovers, server writes a synthetic persisted marker event (`event_persist_recovered`) so clients can anchor the degraded window in timeline audit
- no silent loss: degraded windows are explicitly detectable via `history_gap`, `gap_reason`, and telemetry

EM benefit:

- parent agent can audit exactly what happened before/after restart
- idle/completion supervision remains trustworthy during long-running delegations
- handoff quality improves because timeline continuity is preserved

## Codex observability model (separate and richer, required)

Right now, Claude has strong tool observability and Codex does not. This epic closes that gap for `provider=codex-app`, but without forcing Codex into Claude’s hook schema.

Design decision:

- Use a separate Codex observability store with a richer event taxonomy.
- Keep Claude `tool_usage.db` unchanged.
- Provide a compatibility projection so existing EM workflows (`sm children`, `sm tail`) still work.

Storage:

- New DB: `codex_observability.db` (SQLite, WAL), managed by a dedicated logger.
- Core tables:
- `codex_tool_events`: one row per lifecycle event.
- `codex_turn_events`: turn state and timing events.
- `codex_event_checkpoints`: optional replay/checkpoint metadata for resumable consumers.

Retention/size controls (required):

- `codex_observability.db` retention is mandatory in this epic (not deferred).
- Default retention envelope (operator-configurable):
- age cap: 14 days for tool/turn events
- per-session row cap: 20,000 rows in `codex_tool_events`, 5,000 rows in `codex_turn_events`
- payload cap: `raw_payload_json` is stored as a compact excerpt (bounded length), not unbounded raw blobs
- Pruning runs on startup and periodically in-process (hourly), with explicit metrics/logging for deleted rows and prune duration.

`codex_tool_events` minimum shape:

- `session_id`, `thread_id`, `turn_id`, `item_id`
- `request_id` (for server-request lifecycle correlation)
- `event_type` (`request_approval`, `approval_decision`, `request_user_input`, `user_input_submitted`, `started`, `output_delta`, `completed`, `failed`, `interrupted`, `cancelled`, `timeout`)
- `item_type` (`commandExecution`, `fileChange`, `tool`)
- `phase` (`pre`, `running`, `post`)
- `command`, `cwd`, `exit_code`
- `file_path`, `diff_summary`
- `approval_decision`, `latency_ms`
- `final_status`, `error_code`, `error_message`
- `raw_payload_json`, `created_at`

App-server mapping:

- `item/commandExecution/requestApproval` -> `event_type=request_approval`, `item_type=commandExecution`, `phase=pre`
- `item/fileChange/requestApproval` -> `event_type=request_approval`, `item_type=fileChange`, `phase=pre`
- `POST /sessions/{id}/codex-requests/{request_id}/respond` (approval payload) -> `event_type=approval_decision`, `phase=post`
- `item/started` -> `event_type=started`, `phase=running`
- `item/commandExecution/outputDelta` and `item/fileChange/outputDelta` -> `event_type=output_delta`
- `item/completed` -> one of `completed|failed|interrupted|cancelled|timeout` based on item status, `phase=post`
- `item/tool/requestUserInput` -> `event_type=request_user_input`
- `POST /sessions/{id}/codex-requests/{request_id}/respond` (answers payload) -> `event_type=user_input_submitted`, `phase=post`
- app-server stream/process failure -> synthetic terminal event with explicit `final_status` + `error_code`

Compatibility projection for existing commands:

- Add a server-side read adapter that projects Codex rows into provider-neutral summary semantics used by existing EM surfaces:
- last activity tool/action for `sm children`
- recent action list for `sm tail`
- projection output shape includes at least: `source_provider`, `action_kind`, `summary_text`, `status`, `started_at`, `ended_at`, `session_id`, `turn_id`, `item_id`
- `sm children` and `sm tail` consume this adapter for `provider=codex-app` and keep current `tool_usage.db` path for Claude sessions
- The projection is read-only and does not backfill Claude `tool_usage.db`.

Scope boundary:

- Full real-time parity target is for `provider=codex-app`.
- `provider=codex` (tmux CLI) remains a separate effort (rollout file parsing or other ingestion path).
- All endpoint contracts in this doc (`codex-events`, `codex-pending-requests`, `codex-requests/*/respond`, `pending_structured_request`) apply to `provider=codex-app`.

## What is still not equivalent to native desktop Codex

This is first-class for session operations, not full desktop UI parity.

- `Approval and request-input UX are supported but less structured.`
- Desktop can present richer approval/input interactions with stronger visual context. TUI supports full response semantics, but without desktop-level widgets.
- EM impact: you can still keep delegation moving from terminal, but high-risk approvals are easier to review in desktop.
- `User-input requests are less guided.`
- Desktop can provide more guided interaction patterns for input prompts. TUI v0 is text-composer centric and does not aim to replicate every guided input control.
- EM impact: routine steering works in TUI; complex prompt flows may still be cleaner in desktop.
- `Desktop still has richer presentation for tool details.`
- Session-manager will capture rich Codex tool telemetry in its own DB, but desktop may still present some per-step context with better visual ergonomics.
- EM impact: operational supervision and audit are equivalent or better in manager data; deep visual inspection ergonomics can still be better in desktop.
- `Rich media and advanced UX features are out of scope.`
- Desktop supports broader UI capabilities that terminal surfaces do not replicate cleanly.
- EM impact: terminal remains the operations console; desktop remains the deep-inspection interface.

These tradeoffs are acceptable for this epic because the EM-critical capabilities are preserved: reliable state, fast intervention, non-blocking delegation, and consistent send/control semantics.

## Epic breakdown (implementation tickets)

This should be delivered as an epic with sequenced tickets.

1. `#288-A` Codex activity state + durable lifecycle event stream
- Add `activity_state` computation for `codex-app` sessions.
- Add durable event persistence and cursor replay API (`since_seq` model).
- Guarantee restart continuity for turn-level timeline with explicit `history_gap` contract.
2. `#288-B` Structured request ledger + response APIs
- Add persistent `codex_pending_requests` store and request lifecycle state machine.
- Add `GET /sessions/{id}/codex-pending-requests` and `POST /sessions/{id}/codex-requests/{request_id}/respond`.
- Implement idempotent response replay and restart orphaning policy.
- Gate `POST /sessions/{id}/input` with explicit `409 pending_structured_request` when unresolved structured requests exist.
- Remove legacy direct auto-response path in `codex_app_server.py` and route all structured responses through the ledger resolver.
3. `#288-C` Codex observability DB + app-server ingestion
- Introduce `codex_observability.db` and logger.
- Ingest `commandExecution` / `fileChange` / approval / delta / completion events from app-server.
- Persist raw payload excerpts and normalized fields for analytics, including failure/interruption outcomes.
- Implement retention/pruning and payload bounding as part of the initial schema rollout.
4. `#288-D` Compatibility projection into EM surfaces
- Add read adapter for `sm children`, `sm tail`, and parent wake summaries.
- Ensure Codex sessions report recent actionable tool activity with command/file-change distinction.
5. `#288-E` Input-capable `sm codex-tui`
- Add attachable tmux TUI with state panel, event feed, and in-pane composer.
- Add pending-request queue and structured response actions for approval/user-input requests.
- Route chat input through existing input endpoint and structured replies through codex-request response endpoint.
6. `#288-F` Docs, rollout guardrails, and operational defaults
- Document flags, retention, failure modes, and recovery behavior.
- Add operator-facing examples for EM workflows and escalation paths.

Dependency order:

- `#288-A` and `#288-B` first (state/control plane correctness).
- `#288-C` next (observability ingestion).
- `#288-D` after observability schema is stable.
- `#288-E` after control + data planes are stable.
- `#288-F` final hardening and rollout guidance.

## Implementation shape (epic summary)

- Add computed `activity_state` on session responses for Codex.
- Add Codex event stream with durable storage plus hot in-memory ring for low-latency rendering.
- Add durable structured-request ledger with idempotent response API and restart orphan reconciliation.
- Add Codex observability logger and `codex_observability.db` with command/file-change lifecycle capture.
- Add projection adapter so `sm children` and `sm tail` work for Codex sessions with no user workflow break.
- Add `sm codex-tui <session_id>` attach flow in tmux with:
- live state panel
- event/delta pane
- direct composer mode for chat turns (Enter sends via existing input API)
- pending request panel for approval/user-input requests (respond via structured request endpoint)
- Keep existing `sm send` behavior unchanged.

## Acceptance criteria from user perspective (provider=codex-app)

- I can attach to a Codex app-server session in terminal and see real progress states.
- I can type and send input directly from that pane.
- `sm send` and in-pane send both work and are consistent.
- I can approve/decline tool/file-change requests and submit request-user-input answers from TUI or API with deterministic request IDs.
- If unresolved approval/input requests exist, normal chat send is rejected with explicit `pending_structured_request` guidance instead of being silently queued.
- Legacy automatic approval/input responses are disabled for `provider=codex-app`; structured responses flow through the pending-request ledger.
- I can tell whether a child is progressing, waiting, blocked, or done without guessing.
- If session-manager restarts, I can resume and fetch missed Codex events using cursor-based replay from persistent history.
- If I resume with an old cursor beyond retention, I get an explicit `history_gap` signal (not silent truncation).
- If session-manager restarts while structured requests are open, I get explicit orphaned-request visibility (no silent hanging pending state).
- For `codex-app` sessions, `sm children` and `sm tail` show DB-backed recent tool activity sourced from Codex observability projection.
- I can distinguish command execution vs file change activity in logs, timeline, and summaries.
- I can distinguish success vs failed/interrupted/cancelled/timeout outcomes for tool actions and turns.
- EM-style delegation is practical with `provider=codex-app` sessions at the same operational quality bar as Claude session management.

## Ticket classification

Epic.
