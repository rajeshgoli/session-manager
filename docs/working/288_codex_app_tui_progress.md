# sm#288 — Codex as a first-class session citizen (user narrative)

## The user experience we are building

You are running multiple sessions and delegating work. You should not have to guess whether Codex is busy, blocked, waiting, or done. You should not have to choose between native Codex usability and session-manager observability.

With this change, Codex sessions behave like real managed workers:

- attachable in terminal
- observable in real time
- input-capable from the same pane
- measurable for progress and idle state

This makes Codex usable in the same operational model as Claude sessions, instead of a side path with weaker controls.

## What you get as a user

You get a single workflow where control and visibility live together.

1. You can open `sm codex-tui <session_id>` and immediately see:
- what turn is running
- whether the model is thinking, actively emitting, waiting for input, waiting for permission, idle, or stopped
- event timeline and recent delta output
2. You can type directly in that pane and send input with Enter.
3. You can still use `sm send` from any shell, because both flows hit the same input endpoint and queue lifecycle.
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

## Durable event history (required, not optional)

Process restart dropping event history is not acceptable for EM workflows. This epic includes durable event history so timeline continuity survives server restarts.

- Codex lifecycle events are written to persistent storage (SQLite) as they are emitted.
- TUI and API read from both:
- a hot in-memory ring for low-latency updates
- durable storage for catch-up and restart recovery
- Event schema includes at least:
- `session_id`, monotonic `seq`, timestamp, event type, turn id (if present), compact payload preview
- Retention policy is bounded:
- keep recent window per session (count and age caps) so storage growth is controlled
- API cursor model:
- `GET /sessions/{id}/codex-events?since_seq=<n>&limit=<n>`
- after restart, client resumes from last seen `seq` and backfills missed events from disk
- Failure model:
- if persistence write fails, event is still kept in memory and an explicit `event_persist_error` metric/log entry is emitted
- no silent loss

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

`codex_tool_events` minimum shape:

- `session_id`, `thread_id`, `turn_id`, `item_id`
- `event_type` (`request_approval`, `started`, `output_delta`, `completed`, `request_user_input`)
- `item_type` (`commandExecution`, `fileChange`, `tool`)
- `phase` (`pre`, `running`, `post`)
- `command`, `cwd`, `exit_code`
- `file_path`, `diff_summary`
- `approval_decision`, `latency_ms`
- `raw_payload_json`, `created_at`

App-server mapping:

- `item/commandExecution/requestApproval` -> `event_type=request_approval`, `item_type=commandExecution`, `phase=pre`
- `item/fileChange/requestApproval` -> `event_type=request_approval`, `item_type=fileChange`, `phase=pre`
- `item/started` -> `event_type=started`, `phase=running`
- `item/commandExecution/outputDelta` and `item/fileChange/outputDelta` -> `event_type=output_delta`
- `item/completed` -> `event_type=completed`, `phase=post`
- `item/tool/requestUserInput` -> `event_type=request_user_input`

Compatibility projection for existing commands:

- Add a lightweight read adapter that projects Codex rows into existing summary semantics:
- last activity tool/action for `sm children`
- recent action list for `sm tail`
- The projection is read-only and does not backfill Claude `tool_usage.db`.

Scope boundary:

- Full real-time parity target is for `provider=codex-app`.
- `provider=codex` (tmux CLI) remains a separate effort (rollout file parsing or other ingestion path).

## What is still not equivalent to native desktop Codex

This is first-class for session operations, not full desktop UI parity.

- `Approval UX is less structured.`
- Desktop can present richer approval interactions with stronger visual context. TUI will surface approval state and let you respond, but without desktop-level widgets.
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
- Guarantee restart continuity for turn-level timeline.
2. `#288-B` Codex observability DB + app-server ingestion
- Introduce `codex_observability.db` and logger.
- Ingest `commandExecution` / `fileChange` / approval / delta / completion events from app-server.
- Persist raw payload excerpts and normalized fields for analytics.
3. `#288-C` Compatibility projection into EM surfaces
- Add read adapter for `sm children`, `sm tail`, and parent wake summaries.
- Ensure Codex sessions report recent actionable tool activity with command/file-change distinction.
4. `#288-D` Input-capable `sm codex-tui`
- Add attachable tmux TUI with state panel, event feed, and in-pane composer.
- Submit input through the same session input endpoint used by `sm send`.
5. `#288-E` Docs, rollout guardrails, and operational defaults
- Document flags, retention, failure modes, and recovery behavior.
- Add operator-facing examples for EM workflows and escalation paths.

Dependency order:

- `#288-A` and `#288-B` first (data plane).
- `#288-C` next (CLI/EM integration).
- `#288-D` after data plane is stable.
- `#288-E` final hardening and rollout guidance.

## Implementation shape (epic summary)

- Add computed `activity_state` on session responses for Codex.
- Add Codex event stream with durable storage plus hot in-memory ring for low-latency rendering.
- Add Codex observability logger and `codex_observability.db` with command/file-change lifecycle capture.
- Add projection adapter so `sm children` and `sm tail` work for Codex sessions with no user workflow break.
- Add `sm codex-tui <session_id>` attach flow in tmux with:
- live state panel
- event/delta pane
- direct text composer (Enter to send via existing input API)
- Keep existing `sm send` behavior unchanged.

## Acceptance criteria from user perspective

- I can attach to a Codex session in terminal and see real progress states.
- I can type and send input directly from that pane.
- `sm send` and in-pane send both work and are consistent.
- I can tell whether a child is progressing, waiting, blocked, or done without guessing.
- If session-manager restarts, I can resume and fetch missed Codex events using cursor-based replay from persistent history.
- For `codex-app` sessions, `sm children` and `sm tail` show DB-backed recent tool activity sourced from Codex observability projection.
- I can distinguish command execution vs file change activity in logs, timeline, and summaries.
- EM-style delegation is practical with Codex sessions at the same operational quality bar as Claude session management.

## Ticket classification

Epic.
