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

## What is still not equivalent to native desktop Codex

This is first-class for session operations, not full desktop UI parity.

- `Approval UX is less structured.`
- Desktop can present richer approval interactions with stronger visual context. TUI will surface approval state and let you respond, but without desktop-level widgets.
- EM impact: you can still keep delegation moving from terminal, but high-risk approvals are easier to review in desktop.
- `User-input requests are less guided.`
- Desktop can provide more guided interaction patterns for input prompts. TUI v0 is text-composer centric and does not aim to replicate every guided input control.
- EM impact: routine steering works in TUI; complex prompt flows may still be cleaner in desktop.
- `Transcript durability differs.`
- TUI event feed is backed by a bounded in-memory event ring in v0, so process restart drops local event history.
- EM impact: live supervision is strong, but post-mortem timeline continuity across restarts is limited until persistence is added.
- `Tool execution visibility is shallower.`
- Desktop can expose richer per-step context for tool interactions. TUI focuses on lifecycle state, deltas, and compact event records rather than full rich tool panels.
- EM impact: enough to know whether a child is progressing or stuck, but less depth for forensic inspection in-pane.
- `Rich media and advanced UX features are out of scope.`
- Desktop supports broader UI capabilities that terminal surfaces do not replicate cleanly.
- EM impact: terminal remains the operations console; desktop remains the deep-inspection interface.

These tradeoffs are acceptable for this ticket because the EM-critical capabilities are preserved: reliable state, fast intervention, non-blocking delegation, and consistent send/control semantics.

## Implementation shape (brief)

- Add computed `activity_state` on session responses for Codex.
- Add bounded Codex event stream endpoint for TUI rendering.
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
- EM-style delegation is practical with Codex sessions at the same operational quality bar as Claude session management.

## Ticket classification

Single ticket.
