# 168: sm wait times out for Codex CLI sessions (no idle detection)

## Problem

`sm wait <codex-session>` always times out, even after the Codex agent has finished responding. The watcher receives repeated `[sm wait] Timeout: <name> still active after Ns` messages.

### Reproduction

1. Have a Codex CLI session (`provider: "codex"`) running in tmux
2. Send it a task: `sm send <codex-session> "do something"`
3. Watch it: `sm wait <codex-session> 300`
4. Codex agent completes the task
5. **Bug:** `sm wait` reports timeout instead of idle

Observed with `doc-reviewer-167` (`provider: "codex"`, session `c1d607d3`) — repeated 300s timeout notifications even though the agent had finished and responded via `sm send`.

## Root Cause

`sm wait` polls `delivery_states[session_id].is_idle` via `_watch_for_idle()` (message_queue.py:1102-1151). The polling loop checks `state.is_idle` every `watch_poll_interval` seconds until either idle is detected or timeout is reached.

For Claude sessions, `mark_session_idle()` is called when the Stop hook fires (server.py:1321). This sets `state.is_idle = True` and triggers stop notifications.

**Codex CLI sessions (`provider: "codex"`) have no hooks.** No Stop or Notification hooks fire when Codex finishes. The codebase explicitly acknowledges this at message_queue.py:405: `"Codex CLI sessions have no hooks so idle detection never triggers."`

For message delivery, this is worked around by setting `state.is_idle = True` directly when a message is queued for a Codex session (message_queue.py:415-421). But `sm wait`'s `_watch_for_idle` polling loop has no equivalent workaround.

### Why `is_idle` stays False

When a message is delivered to a Codex session, `_deliver_batch` sets `state.is_idle = False` (message_queue.py:746). After Codex processes the message, no hook fires to set it back to `True`. The transient `True` set at line 420 during `queue_message` is consumed by the delivery and then overwritten.

### Why `session.status` is too slow

OutputMonitor's `_check_idle` (output_monitor.py:441) sets `session.status = SessionStatus.IDLE` only after `idle_timeout` seconds (default 300s) of log file silence, using strict `>`. With a 300s `sm wait` timeout, the watch loop exits at the same time or slightly before `_check_idle` fires. The `_handle_completion` heuristic patterns ("Task complete", "Done.") do not reliably match Codex output.

## Proposed Fix

In `_watch_for_idle`, for Codex CLI sessions, poll the tmux pane via `capture-pane` to detect the `> ` input prompt — the same mechanism already used by `_get_pending_user_input_async` (message_queue.py:510-544) for message delivery. The prompt must be visible on **two consecutive polls** to confirm idle — a single detection can false-fire on the transient `> ` prompt visible between delivery and Codex starting to process (after `_deliver_batch` marks messages delivered and clears the pending queue, the pending-message guard no longer blocks).

### Implementation

**In `_watch_for_idle()`** (message_queue.py:1102-1151):

After the existing `state.is_idle` check (line 1117-1118), add Codex prompt detection:

```python
# Existing check
state = self.delivery_states.get(target_session_id)
is_idle = state.is_idle if state else False

# Codex CLI fallback: detect idle via tmux prompt (requires two consecutive detections)
if not is_idle:
    session = self.session_manager.get_session(target_session_id)
    if session and getattr(session, "provider", "claude") == "codex" and session.tmux_session:
        prompt_visible = await self._check_codex_prompt(session.tmux_session)
        if prompt_visible:
            codex_prompt_count += 1  # initialized to 0 before the loop
            if codex_prompt_count >= 2:
                is_idle = True
        else:
            codex_prompt_count = 0

# Existing pending-message guard (applies to both paths)
if is_idle and self.get_pending_messages(target_session_id):
    is_idle = False
```

**New helper `_check_codex_prompt()`:**

Runs `tmux capture-pane -p -t <session>`, checks if the last non-empty line starts with `> ` (empty prompt — no user-typed text). This reuses the same pattern as `_get_pending_user_input_async` but checks for the prompt itself rather than user text after it.

```python
async def _check_codex_prompt(self, tmux_session: str) -> bool:
    """Check if Codex CLI is showing the input prompt (idle)."""
    try:
        proc = await asyncio.create_subprocess_exec(
            "tmux", "capture-pane", "-p", "-t", tmux_session,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)
        if proc.returncode != 0:
            return False
        output = stdout.decode().rstrip()
        if not output:
            return False
        last_line = output.split('\n')[-1]
        # Prompt is "> " with optional trailing whitespace, no user text
        return last_line.rstrip() == '>' or last_line.startswith('> ') and not last_line[2:].strip()
    except Exception:
        return False
```

## Scope

- `src/message_queue.py` — `_watch_for_idle()`: add Codex prompt detection fallback; new `_check_codex_prompt()` helper

## Edge Cases

1. **Pending-message guard**: The existing guard (`is_idle and self.get_pending_messages(...)`, message_queue.py:1120-1122) runs after both the `state.is_idle` check and the Codex prompt fallback. If the prompt is visible but messages are queued, idle is suppressed — same behavior as the Claude path. No special handling needed.

2. **Transient prompt after delivery**: After `_deliver_batch` marks messages delivered (message_queue.py:749) and sets `is_idle = False` (line 746), the pending queue is empty so the pending-message guard won't block. Codex may briefly show the `> ` prompt before it starts processing. The two-consecutive-polls requirement (at `watch_poll_interval` intervals, default 2s) prevents false-firing on this transient state.

3. **tmux session gone**: If the tmux session is destroyed (agent killed), `capture-pane` returns non-zero and `_check_codex_prompt` returns `False`. The watch times out normally. No crash.

4. **Codex-app sessions**: `provider: "codex-app"` sessions use RPC-based turn completion (`_handle_codex_turn_complete`), which already calls `mark_session_idle()` (session_manager.py:860). This fix only targets `provider: "codex"` (tmux-based Codex CLI).

## Ticket Classification

**Single ticket.** The fix adds one helper and a conditional in `_watch_for_idle`, all in one file. One agent can complete this without compacting context.
