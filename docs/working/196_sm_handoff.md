# sm handoff — Self-Directed Context Rotation

**Issue:** #196
**Status:** Draft (rev 4 — post-re-review)
**Author:** Scout

## Problem

Long-running agents accumulate context until compaction fires. Compaction is expensive, lossy, and unpredictable — the agent doesn't control when it happens or what gets dropped. Agents need a way to proactively rotate their own context by writing a handoff doc and restarting with it.

## Design: Fire-and-Forget with Deferred Execution

`sm handoff` is a **fire-and-forget CLI command** that an agent calls on itself. The actual clear+restart happens asynchronously after the agent's current turn completes (Stop hook fires).

### Why deferred?

The agent is **running** when it calls `sm handoff` (it's executing a Bash tool call). You can't send `/clear` to a tmux pane while its Claude process is mid-turn — `/clear` is only accepted at the idle `>` prompt. So the CLI registers the intent, and the server executes it when the Stop hook fires.

## Usage

```bash
sm handoff /path/to/handoff_doc.md
```

The agent should:
1. Write its state to the handoff doc (persona, queue status, lessons learned, next actions)
2. Call `sm handoff <file-path>` as its **last action** — anything after this in the same turn is lost
3. The agent's turn ends naturally; Stop hook fires; server executes the handoff

## Sequence

```
Agent                     CLI (sm handoff)            Server                      tmux/Claude
  |                          |                           |                           |
  |-- bash: sm handoff f.md->|                           |                           |
  |                          |-- verify file exists       |                           |
  |                          |-- POST /handoff ---------->|                           |
  |                          |   (session_id + file_path) |                           |
  |                          |                           |-- verify self-auth         |
  |                          |                           |-- reject if codex-app      |
  |                          |                           |-- store pending_handoff    |
  |                          |<-- 200 OK ----------------|                           |
  |<-- "Handoff scheduled" --|                           |                           |
  |                          |                           |                           |
  | (agent turn completes)   |                           |                           |
  |                          |                           |<-- Stop hook fires         |
  |                          |                           |-- detect pending_handoff   |
  |                          |                           |-- invalidate_cache(full)   |
  |                          |                           |-- skip _restore_user_input |
  |                          |                           |-- acquire delivery lock    |
  |                          |                           |-- Escape ----------------->|
  |                          |                           |-- wait for > prompt        |
  |                          |                           |-- /clear + Enter --------->|
  |                          |                           |-- wait for > prompt        |
  |                          |                           |-- handoff prompt --------->|
  |                          |                           |-- release delivery lock    |
  |                          |                           |-- clear pending_handoff    |
  |                          |                           |                           |
  |<========= NEW CONTEXT: "Read f.md and continue." =========================>|
```

## Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Self-directed only | Agent can only handoff itself | Matches the use case; avoids parent-child auth complexity |
| File path, not content | CLI verifies file exists; new context reads it | Handoff docs can be large (10-50KB). Avoids CLI arg length limits and tmux send-keys payload issues |
| Preserves identity | Same session ID, name, tmux session | Children and parent continue to see the same agent |
| Fire-and-forget | CLI returns immediately | Avoids chicken-and-egg: agent must finish turn before clear can execute |
| Handoff prompt is simple | `Read <file-path> and continue from where you left off.` | The handoff doc itself contains all instructions (persona, state, next actions). sm stays dumb |
| Server-side execution | Server performs clear+send in Stop hook | Server already has TmuxController and async primitives; CLI can't wait for Stop hook |

## Authorization

- `CLAUDE_SESSION_MANAGER_ID` must be set (agent is calling from within a session)
- CLI passes `requester_session_id` in the request body (same pattern as `/sessions/{id}/kill`)
- Server verifies `requester_session_id == session_id` (self-handoff only)
- No parent-child relationship required

## Implementation Approach

### 1. CLI: New `sm handoff` Subcommand

**File:** `src/cli/main.py`

Add argparse subcommand:
```python
handoff_parser = subparsers.add_parser("handoff", help="Self-directed context rotation via handoff doc")
handoff_parser.add_argument("file_path", help="Path to handoff document")
```

Route to handler:
```python
elif args.command == "handoff":
    sys.exit(commands.cmd_handoff(client, session_id, args.file_path))
```

Note: `handoff` must NOT be in `no_session_needed` — it requires `CLAUDE_SESSION_MANAGER_ID`.

**File:** `src/cli/commands.py`

New `cmd_handoff` function:
```python
def cmd_handoff(client, session_id, file_path):
    # 1. Verify session_id is set
    if not session_id:
        print("Error: CLAUDE_SESSION_MANAGER_ID not set. sm handoff can only be called from within a session.", file=sys.stderr)
        return 2
    # 2. Resolve file_path to absolute path
    abs_path = os.path.abspath(file_path)
    # 3. Verify file exists
    if not os.path.isfile(abs_path):
        print(f"Error: File not found: {abs_path}", file=sys.stderr)
        return 1
    # 4. Call server API
    result = client.schedule_handoff(session_id, abs_path)
    if result is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2
    if result.get("error") or result.get("detail"):
        print(f"Error: {result.get('error') or result.get('detail')}", file=sys.stderr)
        return 1
    print(f"Handoff scheduled — will execute after current turn completes")
    return 0
```

**File:** `src/cli/client.py`

New client method (follows existing `_request` pattern):
```python
def schedule_handoff(self, session_id: str, file_path: str) -> Optional[dict]:
    """Schedule a self-directed handoff for a session."""
    data, success, unavailable = self._request(
        "POST",
        f"/sessions/{session_id}/handoff",
        {"requester_session_id": session_id, "file_path": file_path},
    )
    if unavailable:
        return None
    return data if data else {"error": "Unknown error"}
```

### 2. Server: New API Endpoint

**File:** `src/server.py`

Request model:
```python
class HandoffRequest(BaseModel):
    requester_session_id: str
    file_path: str
```

New endpoint `POST /sessions/{session_id}/handoff`:
```python
@app.post("/sessions/{session_id}/handoff")
async def schedule_handoff(session_id: str, request: HandoffRequest):
    if not app.state.session_manager:
        raise HTTPException(status_code=503, detail="Session manager not configured")

    # 1. Verify self-auth: requester must be the session itself
    if request.requester_session_id != session_id:
        return {"error": "sm handoff is self-directed only — requester must equal target session"}

    # 2. Verify session exists
    session = app.state.session_manager.get_session(session_id)
    if not session:
        return {"error": f"Session {session_id} not found"}

    # 3. Reject codex-app sessions (no tmux, different clear mechanism)
    if session.provider == "codex-app":
        return {"error": "sm handoff is not supported for codex-app sessions"}

    # 4. Store pending handoff on delivery state
    queue_mgr = app.state.session_manager.message_queue_manager
    if not queue_mgr:
        return {"error": "Message queue manager not available"}

    state = queue_mgr._get_or_create_state(session_id)
    state.pending_handoff_path = request.file_path
    return {"status": "scheduled"}
```

### 3. Server: Gate Stop Hook Side Effects on Pending Handoff

**File:** `src/server.py` — in `claude_hook()`, Stop hook handling block

The `_restore_user_input_after_response` call and other post-idle tasks in the Stop hook handler must be gated when a handoff is pending. The handoff check in `mark_session_idle` (section 4) returns early, but the server-side code after that call runs unconditionally. Fix:

```python
# Handle Stop hook - Claude finished responding
if hook_event == "Stop" and session_manager_id:
    queue_mgr = app.state.session_manager.message_queue_manager if app.state.session_manager else None
    if queue_mgr:
        queue_mgr.mark_session_idle(session_manager_id, last_output=last_message, from_stop_hook=True)

        # === NEW: Skip _restore_user_input if handoff was triggered ===
        # mark_session_idle sets is_idle=False synchronously when a handoff is pending.
        # Check that flag to avoid restoring saved input during handoff execution.
        state = queue_mgr.delivery_states.get(session_manager_id)
        handoff_in_progress = state and not state.is_idle
        if not handoff_in_progress:
            asyncio.create_task(queue_mgr._restore_user_input_after_response(session_manager_id))
        # === END NEW ===

    # Keep session.status in sync (existing code, unchanged)
    ...
```

**Why check `not state.is_idle`:** `mark_session_idle` sets `is_idle = True` at the top, but when it detects a pending handoff it sets `is_idle = False` synchronously (in `mark_session_idle` itself, section 4 line 228) before scheduling `_execute_handoff` and returning. So `is_idle == False` after `mark_session_idle` returns is a reliable signal that a handoff was triggered. The `is_idle = False` is set by the synchronous caller, NOT by the async `_execute_handoff` task — this ensures the flag is visible immediately when the server-side code checks it.

### 4. Server: Deferred Execution in Stop Hook

**File:** `src/message_queue.py` — `mark_session_idle()`

Insert handoff check **before** the skip_count absorption and stop notification logic. When a handoff is pending, it takes priority — no queued message delivery, no stop notifications:

```python
def mark_session_idle(self, session_id, last_output=None, from_stop_hook=False):
    state = self._get_or_create_state(session_id)
    state.is_idle = True
    state.last_idle_at = datetime.now()

    # === NEW: Check for pending handoff ===
    if from_stop_hook and getattr(state, 'pending_handoff_path', None):
        file_path = state.pending_handoff_path
        state.pending_handoff_path = None  # Clear before execution
        state.is_idle = False  # Signal to server.py that handoff is in progress
        asyncio.create_task(self._execute_handoff(session_id, file_path))
        return  # Skip stop notification, skip _try_deliver_messages
    # === END NEW ===

    # ... existing skip_count, stop notification, delivery logic ...
```

### 5. Server: Handoff Execution Method

**File:** `src/message_queue.py`

New async method on `MessageQueueManager`. Acquires `_delivery_locks[session_id]` to prevent concurrent interleaving with queued message delivery:

**IMPORTANT — Failure recovery:** `mark_session_idle` sets `is_idle = False` synchronously before scheduling this task. Every abort/failure path in `_execute_handoff` MUST restore `is_idle = True` and trigger `_try_deliver_messages` so the session doesn't stall permanently.

```python
async def _execute_handoff(self, session_id: str, file_path: str):
    """Execute a deferred handoff: clear context and send handoff prompt.

    Caller (mark_session_idle) has already set is_idle=False. On ANY failure,
    this method MUST restore idle state to prevent permanent stall.

    Acquires the per-session delivery lock to prevent interleaving with
    queued message delivery from _try_deliver_messages.
    """
    def _restore_idle():
        """Restore idle state and trigger queued delivery on failure."""
        state = self._get_or_create_state(session_id)
        state.is_idle = True
        state.last_idle_at = datetime.now()
        logger.warning(f"Handoff failed for {session_id}, restoring idle state")
        asyncio.create_task(self._try_deliver_messages(session_id))

    session = self.session_manager.sessions.get(session_id)
    if not session:
        logger.error(f"Handoff: session {session_id} not found")
        _restore_idle()
        return

    # Verify file still exists
    from pathlib import Path
    if not Path(file_path).exists():
        logger.error(f"Handoff: file {file_path} no longer exists, aborting")
        _restore_idle()
        return

    tmux_session = session.tmux_session
    if not tmux_session:
        logger.error(f"Handoff: session {session_id} has no tmux session")
        _restore_idle()
        return

    logger.info(f"Executing handoff for {session_id}: {file_path}")

    # Acquire delivery lock to prevent _try_deliver_messages from interleaving
    lock = self._delivery_locks.setdefault(session_id, asyncio.Lock())
    async with lock:
        try:
            # 1. Full cache invalidation (matches _invalidate_session_cache behavior):
            #    - Arm skip fence for /clear Stop hook
            #    - Clear stale notification state
            state = self._get_or_create_state(session_id)
            state.stop_notify_skip_count += 1
            state.stop_notify_sender_id = None
            state.stop_notify_sender_name = None
            state.last_outgoing_sm_send_target = None
            state.last_outgoing_sm_send_at = None
            # Also clear server-side caches if session_manager has app reference
            if hasattr(self.session_manager, '_app') and self.session_manager._app:
                app = self.session_manager._app
                app.state.last_claude_output.pop(session_id, None)
                app.state.pending_stop_notifications.discard(session_id)

            # 2. Send Escape to ensure idle (with subprocess timeout)
            proc = await asyncio.create_subprocess_exec(
                "tmux", "send-keys", "-t", tmux_session, "Escape",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)

            # 3. Wait for > prompt (reuse existing method)
            await self._wait_for_claude_prompt_async(tmux_session)

            # 4. Send /clear (with subprocess timeout + settle delay)
            clear_command = "/new" if session.provider == "codex" else "/clear"
            proc = await asyncio.create_subprocess_exec(
                "tmux", "send-keys", "-t", tmux_session, "--", clear_command,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)
            await asyncio.sleep(0.3)  # settle delay
            proc = await asyncio.create_subprocess_exec(
                "tmux", "send-keys", "-t", tmux_session, "Enter",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)

            # 5. Wait for clear to complete (reuse existing method)
            await self._wait_for_claude_prompt_async(tmux_session, timeout=5.0)

            # 6. Send handoff prompt (with subprocess timeout + settle delay)
            handoff_prompt = f"Read {file_path} and continue from where you left off."
            proc = await asyncio.create_subprocess_exec(
                "tmux", "send-keys", "-t", tmux_session, "--", handoff_prompt,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)
            await asyncio.sleep(0.3)  # settle delay
            proc = await asyncio.create_subprocess_exec(
                "tmux", "send-keys", "-t", tmux_session, "Enter",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)

            # 7. Mark session as active
            self.mark_session_active(session_id)

            logger.info(f"Handoff complete for {session_id}")

        except Exception as e:
            logger.error(f"Handoff execution failed for {session_id}: {e}")
            _restore_idle()
```

**No new helper methods needed.** All tmux subprocess calls use `asyncio.wait_for(..., timeout=self.subprocess_timeout)` matching the existing pattern in `_try_deliver_messages` (line 867-878). Prompt polling reuses the existing `_wait_for_claude_prompt_async` method (line 1140+).

### 6. Model: New Field on SessionDeliveryState

**File:** `src/models.py`

Add to `SessionDeliveryState`:
```python
pending_handoff_path: Optional[str] = None  # File path for pending handoff (#196)
```

### 7. Server-Side Cache Access

**File:** `src/session_manager.py`

The `_execute_handoff` method needs access to `app.state` for full cache invalidation. The cleanest approach: store an `_app` reference on `SessionManager` during startup (in `server.py`'s `create_app`), similar to how `notifier` is already attached. Alternatively, pass a cache-invalidation callback to `MessageQueueManager` at construction time. The implementer should choose whichever pattern is more consistent with the existing code.

## Edge Cases

| Scenario | Behavior |
|----------|----------|
| Agent continues after `sm handoff` | Fine — handoff executes after Stop hook, not immediately. But any work after the call is lost on clear |
| File deleted between schedule and execution | `_execute_handoff` checks file exists; logs error, calls `_restore_idle()` to set `is_idle=True` and trigger queued delivery. Session recovers to normal idle state |
| Multiple `sm handoff` calls in same turn | Last one wins (overwrites `pending_handoff_path`) |
| Session killed before handoff executes | Pending handoff is lost (in-memory only). No harm |
| Server restart with pending handoff | Lost (in-memory). Agent would need to re-handoff. Acceptable for v1 |
| Codex-app sessions | Rejected at schedule time with clear error message |
| `stop_notify_skip_count` interaction | The `/clear` during handoff generates a Stop hook. `skip_count += 1` ensures it's absorbed. The handoff prompt then runs, and its eventual Stop hook flows normally |
| Queued messages for this session | The `return` in `mark_session_idle` skips `_try_deliver_messages`. Delivery lock in `_execute_handoff` prevents interleaving. After handoff completes and the agent processes the doc, queued messages deliver on the next Stop hook |
| `_restore_user_input` race | Server-side Stop hook handler checks `state.is_idle` after `mark_session_idle` returns — if `False` (handoff in progress), skips the restore task |

## Test Plan

1. **Happy path:** Agent writes doc → calls `sm handoff` → turn ends → context clears → new context reads doc
2. **File validation:** `sm handoff /nonexistent/file.md` returns error at CLI
3. **Self-only auth:** Craft request where `requester_session_id != session_id` → server rejects
4. **Codex-app rejection:** Schedule handoff for codex-app session → server rejects at schedule time
5. **No session:** `sm handoff` from bare shell (no `CLAUDE_SESSION_MANAGER_ID`) returns error
6. **Skip fence:** Verify the `/clear` Stop hook is absorbed and doesn't trigger spurious notifications
7. **Queued messages:** If messages are queued for the session, verify they're delivered after handoff completes (on the next Stop hook), not interleaved during clear
8. **Delivery lock:** Verify `_execute_handoff` acquires `_delivery_locks[session_id]` and `_try_deliver_messages` cannot interleave
9. **Failure recovery:** Delete handoff file after scheduling but before Stop hook fires. Verify session restores to idle and queued messages are delivered

## Ticket Classification

**Single ticket.** One agent can implement this without compacting context. The changes are localized:
- ~20 lines in `main.py` (argparse + routing)
- ~25 lines in `commands.py` (cmd_handoff)
- ~10 lines in `client.py` (schedule_handoff)
- ~25 lines in `server.py` (endpoint + Stop hook gate)
- ~80 lines in `message_queue.py` (mark_session_idle hook + _execute_handoff + helpers)
- ~1 line in `models.py` (new field)
- Tests
