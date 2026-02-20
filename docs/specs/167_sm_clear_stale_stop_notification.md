# 167: sm clear does not reset stop-hook notification message

## Problem

When an agent is reused via `sm clear` + `sm send`, the stop-hook notification relays the **previous task's** final message instead of the current task's.

### Reproduction (from issue)

1. Agent completes task A -> stop hook fires -> parent receives agent's last message from task A
2. Parent clears agent: `sm clear <agent>`
3. Parent dispatches task B: `sm send <agent> "new task" --urgent`
4. Agent completes task B -> stop hook fires
5. **Bug:** Parent receives stop notification with task A's final message, not task B's

Observed during the #153->#154->#155 sequence with `engineer-1601`.

## Root Cause

`sm clear` resets Claude's context window (sends `/clear` to tmux) but does **not** invalidate the cached output that stop notifications read from.

### Data flow analysis

The stop notification message is sourced from `app.state.last_claude_output`, an in-memory `dict[str, str]` keyed by session ID (server.py:257). Here's how the data flows:

1. **Stop hook fires** -> `POST /hooks/claude` with `hook_event_name="Stop"`
2. Server reads the JSONL transcript file in reverse, extracting the most recent assistant message as `last_message`
3. **If `last_message` is not None**: stored into `last_claude_output[session_manager_id]` (server.py:1310)
4. `mark_session_idle(session_manager_id)` is called (server.py:1321) — this happens **before** the `pending_stop_notifications` check
5. Inside `mark_session_idle`, if `state.stop_notify_sender_id` is set, `_send_stop_notification()` fires immediately (message_queue.py:275-280)
6. `_send_stop_notification()` reads `hook_output_store.get(recipient_session_id)` (message_queue.py:938) — this is a reference to the same `last_claude_output` dict
7. **If `last_message` was None** (transcript not yet flushed — race condition): `last_claude_output` was **not updated** in step 3, and `session_manager_id` is added to `pending_stop_notifications` (server.py:1400-1405) — but this happens **after** step 5 already sent the stale notification

### The bug

When Task 2's Stop hook fires with `last_message = None` (step 3 returns None), `last_claude_output[session_id]` still holds Task 1's message — because `sm clear` never cleared it. The stop notification (step 5-6) fires immediately from `mark_session_idle` and reads the stale Task 1 message from `hook_output_store`. The `pending_stop_notifications` path (step 7) runs after the notification has already been sent.

Even when `last_message` is not None, there is a secondary concern: the transcript file is append-only across `/clear` boundaries within the same Claude process. While the reverse scan typically finds the correct (most recent) assistant message, the stale cache remains as a latent hazard.

### What `sm clear` resets vs. what it doesn't

| State | Reset by `sm clear`? |
|-------|---------------------|
| Claude's context window (via `/clear` in tmux) | Yes |
| `app.state.last_claude_output[session_id]` | **No** |
| `app.state.pending_stop_notifications` (deferred notification set) | **No** |
| `MessageQueueManager.delivery_states[session_id].stop_notify_sender_id` | **No** |
| `OutputMonitor._last_response_sent[session_id]` | **No** |

The CLI path (`cmd_clear` in commands.py:1587-1743) operates directly on tmux via subprocess calls and never touches server-side state at all. The server path (`clear_session` in session_manager.py:969-1002, `_clear_tmux_session` in session_manager.py:1004-1071) only sends tmux keystrokes — no cache invalidation.

## Proposed Fix

When a session is cleared, invalidate the stale cached output and related notification state. All cache invalidation belongs in the server layer, which owns `app.state`. `SessionManager` continues to handle tmux operations only.

1. **In the server's `/sessions/{session_id}/clear` endpoint** (server.py:928-942):
   - After calling `session_manager.clear_session()`, clear server-owned state:
     - `app.state.last_claude_output.pop(session_id, None)` — remove stale cached message
     - `app.state.pending_stop_notifications.discard(session_id)` — cancel any deferred notification from the previous task
   - Clear message-queue state via `message_queue_manager`:
     - `delivery_states[session_id].stop_notify_sender_id = None` — prevent leaking the notification target across tasks
     - `delivery_states[session_id].stop_notify_sender_name = None`

2. **In `cmd_clear` CLI path** (commands.py:1587-1743):
   - After the tmux clear operations succeed, call a lightweight cache-invalidation endpoint (e.g. `POST /sessions/{session_id}/invalidate-cache`) to clear server-side state. This keeps the existing CLI tmux logic intact — the only missing piece is notifying the server that a clear happened. Routing the entire clear through the server endpoint would be a larger refactor (CLI uses synchronous subprocess + sleep, server uses async) with no added benefit.

## Scope

- `src/server.py` — `/sessions/{session_id}/clear` endpoint: add cache invalidation for `last_claude_output`, `pending_stop_notifications`, and `stop_notify_sender_id`
- `src/cli/commands.py` — `cmd_clear()`: add cache-invalidation API call after tmux operations
- `src/session_manager.py` — no changes needed (handles tmux operations only)
- `src/message_queue.py` — no changes needed (reads from `hook_output_store` which will now be correctly invalidated; `stop_notify_sender_id` cleared by server endpoint)

## Edge Cases

1. **Race between clear and in-flight Stop hook**: If a Stop hook is being processed at the exact moment `sm clear` is called, the clear could delete the cache entry right before `_send_stop_notification` reads it. This is acceptable — the notification would fall back to the generic "completed (Stop hook fired)" message (message_queue.py:946), which is better than sending a stale message from the wrong task.

2. **Deferred notifications via `idle_prompt`**: If Task 1's Stop hook was deferred (added to `pending_stop_notifications`) and then `sm clear` is called before the `idle_prompt` hook fires, clearing `pending_stop_notifications` prevents the deferred path from sending Task 1's message during Task 2. This is correct behavior.

3. **CLI path (tmux sessions)**: Currently `cmd_clear` for tmux sessions never calls the server. After tmux operations, `cmd_clear` calls the cache-invalidation endpoint to clear server-side state.

## Ticket Classification

**Single ticket.** The fix touches two files (`src/server.py` endpoint + `src/cli/commands.py` CLI path) with a small, focused change. One agent can complete this without compacting context.
