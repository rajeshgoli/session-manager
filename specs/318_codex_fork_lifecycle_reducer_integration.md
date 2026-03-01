# Ticket #318: codex-fork lifecycle reducer integration

## Scope

Implements `sm#316-B` for `provider=codex-fork`:

1. Adds event-stream-driven lifecycle reducer in `SessionManager`.
2. Wires `codex-fork` provider creation with `--event-stream` and schema pin.
3. Routes `sm status`/API activity state and `sm wait` behavior through reducer state.

## Implementation summary

1. Added `codex-fork` provider support across API/CLI/session model surfaces.
2. Added codex-fork lifecycle reducer states:
   - `running`
   - `idle`
   - `waiting_on_approval`
   - `waiting_on_user_input`
   - `shutdown`
   - `error`
3. Added deterministic transition logic with transition-cause tracking.
4. Added event-stream monitor to tail codex-fork JSONL events and feed reducer.
5. Added wait/status integration:
   - `get_activity_state()` maps reducer state to public activity state.
   - `MessageQueueManager._watch_for_idle()` uses reducer state for codex-fork sessions.
6. Added regression-focused tests for false-idle/false-stopped and waiting transitions.

## Notes

1. Reducer transition changes are persisted to codex event store as `lifecycle_transition` events for audit/debug.
2. Existing `codex` and `codex-app` behavior remains intact.
