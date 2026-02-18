# sm wait: false timeout when agent goes idle after sm send (#180)

## Problem

`sm wait <agent-id> 600` reports "still active after 600s" even though the agent
completed its task, sent a message to the EM via `sm send`, and went idle long
before the timeout expired.

## Observed Behavior

1. EM dispatches agent with `sm send <agent> "task" --urgent`
2. Agent completes work, calls `sm send <em> "done"`, and goes idle
3. EM runs `sm wait <agent> 600`
4. After 600s, EM receives: `[sm wait] Timeout: <agent> still active after 600s`

The agent's tmux pane shows the idle `>` prompt. The session is genuinely idle.
But `_watch_for_idle` never detects it.

## Root Cause Analysis

### Primary: `_watch_for_idle` has no Claude tmux fallback

`_watch_for_idle` (message_queue.py:1178) polls `delivery_states[target].is_idle`
every 2 seconds. This in-memory flag is set only by `mark_session_idle()`, which
is called only when the Stop hook HTTP POST reaches `/hooks/claude`.

For **Codex CLI** sessions (provider="codex"), there is a secondary check —
`_check_codex_prompt()` polls tmux for the `>` prompt and triggers idle after
two consecutive detections (lines 1197-1208). This acts as a safety net when
the in-memory state is wrong.

For **Claude Code** sessions (provider="claude"), **there is no such fallback**.
If the Stop hook fails to set `is_idle = True`, `_watch_for_idle` will poll for
the entire timeout without ever detecting idle.

The Stop hook (`notify_server.sh`) fires `curl` in a backgrounded subshell
(`(curl ...)& disown`). This is fire-and-forget — if the curl fails (server
busy, connection refused, timeout), there is no retry, and `mark_session_idle()`
is never called. The `is_idle` flag remains `False` permanently.

### Secondary: `notify_on_stop` bounce-back creates stuck pending messages

When the agent calls `sm send <em> "done"`, the default `notify_on_stop=True`
causes the following chain:

```
Agent sends "done" to EM (notify_on_stop=True)
  → Delivered to EM → state[EM].stop_notify_sender_id = agent

Agent goes idle → Stop hook → mark_session_idle(agent) → is_idle[agent] = True
  → stop_notify fires "[sm] agent stopped: ..." to EM

EM processes "done" + stop notification → EM's turn ends → Stop hook
  → mark_session_idle(EM) → state[EM].stop_notify_sender_id = agent
  → _send_stop_notification queues "[sm] EM stopped: ..." to agent
    (delivery_mode="important")
```

This bounce-back message is queued for the agent. The `_watch_for_idle`
validation check (line 1211) overrides idle when pending messages exist:

```python
if is_idle and self.get_pending_messages(target_session_id):
    is_idle = False
```

In the normal case, the important-mode message is delivered immediately
(agent is idle), the agent processes it, and goes idle again. The window
where pending messages exist is brief.

**But if delivery fails** (tmux send error, agent session in unexpected state),
the message stays pending permanently. Every `_watch_for_idle` poll sees the
pending message and overrides `is_idle` to `False`, even though the session
status is `IDLE`. The timeout fires after 600s.

### Tertiary: `Session.status` not consulted

The Stop hook handler sets `session.status = SessionStatus.IDLE` (server.py:1381)
as persistent model state. `_watch_for_idle` only checks the in-memory
`delivery_states.is_idle` and never consults `session.status`. Adding a fallback
check on `session.status` would provide resilience against in-memory state
corruption.

## Proposed Solution

### 1. Add Claude tmux prompt fallback to `_watch_for_idle`

Mirror the existing Codex CLI fallback for Claude Code sessions. When
`delivery_states.is_idle` is False, check the tmux pane for the Claude idle
prompt (bare `>` on last line). Require two consecutive detections to avoid
transient false positives (same pattern as Codex CLI).

```python
# message_queue.py – _watch_for_idle(), after the codex fallback block

# Claude Code fallback: detect idle via tmux prompt
if not is_idle:
    session = self.session_manager.get_session(target_session_id)
    if session and getattr(session, "provider", "claude") == "claude" and session.tmux_session:
        prompt_visible = await self._check_claude_prompt(session.tmux_session)
        if prompt_visible:
            claude_prompt_count += 1
            if claude_prompt_count >= 2:
                is_idle = True
        else:
            claude_prompt_count = 0
```

The existing `_check_codex_prompt` method already detects the `>` prompt. It
can be reused directly (Claude Code and Codex CLI use the same `>` prompt).
Rename or alias it to `_check_idle_prompt` for clarity.

### 2. Add `Session.status` fallback check

When `delivery_states.is_idle` is False and tmux prompt is not detected, check
the persistent `session.status`. If `session.status == SessionStatus.IDLE`,
treat the session as idle (subject to the pending-message validation).

```python
# After delivery_states check, before Codex/Claude fallbacks
if not is_idle:
    session = self.session_manager.get_session(target_session_id)
    if session and session.status == SessionStatus.IDLE:
        is_idle = True
```

### 3. Expire stale pending messages in `_watch_for_idle` validation

The pending-message validation is correct in principle (prevents false idle
notifications from #153). But stale undeliverable messages should not block
idle detection forever. Add a staleness check: if the oldest pending message
has been queued for longer than `N` seconds (e.g., 30s) and delivery has been
attempted, treat it as expired.

```python
# Refined validation
if is_idle and self.get_pending_messages(target_session_id):
    # Check if pending messages are stale (delivery failed)
    oldest = self.get_pending_messages(target_session_id)[0]
    age = (datetime.now() - oldest.queued_at).total_seconds()
    if age > 30:  # Message stuck for >30s = delivery failure
        logger.warning(f"Stale pending message {oldest.id} for {target_session_id}, age={age:.0f}s")
        # Don't override idle — delivery is stuck, not in-flight
    else:
        is_idle = False
```

## Implementation Approach

### Files to Change

| File | Change |
|------|--------|
| `src/message_queue.py` | `_watch_for_idle()`: add Claude tmux prompt fallback, `Session.status` fallback, stale message expiry |
| `src/message_queue.py` | Rename `_check_codex_prompt()` → `_check_idle_prompt()` (used by both Codex and Claude fallbacks) |

### What NOT to Change

- The existing `notify_on_stop` mechanism is working as designed. The bounce-back
  notification is a feature (tells the sender their recipient finished).
- The `pending_messages` validation (from #153 fix) stays — it correctly prevents
  the opposite race (false-idle-too-early). This spec only relaxes it for stale
  messages.
- The Stop hook fire-and-forget design stays — it's intentionally non-blocking.
  The fix is defense in depth on the `_watch_for_idle` side.

## Test Plan

### Unit Tests

| Test | Description |
|------|-------------|
| `test_watch_detects_claude_tmux_idle` | Session provider="claude", is_idle=False but tmux shows `>` prompt — assert idle detected after 2 consecutive checks |
| `test_watch_session_status_fallback` | is_idle=False, tmux unavailable, but session.status=IDLE — assert idle detected |
| `test_watch_stale_pending_message` | is_idle=True, pending message queued 60s ago — assert validation does NOT override idle |
| `test_watch_fresh_pending_message` | is_idle=True, pending message queued 2s ago — assert validation DOES override idle (preserves #153 fix) |
| `test_watch_codex_fallback_unchanged` | Verify existing Codex CLI fallback behavior not regressed |

### Manual Verification

1. Dispatch agent: `sm send <agent> "echo hello and report back" --urgent`
2. Agent completes and calls `sm send <em> "done"`
3. `sm wait <agent> 30`
4. Verify: notification arrives within seconds ("idle after Ns"), NOT timeout
5. Verify: normal `notify_on_stop` notification also arrives independently

## Ticket Classification

Single ticket. One file change, localized to `_watch_for_idle()` method.
