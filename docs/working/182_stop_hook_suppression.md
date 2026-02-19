# sm#182: Stop Hook — Suppress Redundant Notifications After `sm send`

## Problem

Stop hooks fire every time an agent goes idle. When an agent has already reported its result via `sm send`, the subsequent stop notification is redundant — the EM receives two messages for the same completion:

1. The `sm send` result (the actual payload)
2. `[sm] agent-name stopped: <truncated output>` (noise)

Over a session with 20+ dispatches, this doubles the notification volume and can trigger premature EM intervention.

## Root Cause

The stop notification mechanism and `sm send` are decoupled — neither knows about the other.

**Timeline of the bug:**

1. EM sends task to Agent A via `sm send` (with `notify_on_stop=True`, the default)
2. Message is delivered to Agent A; at delivery time, `state.stop_notify_sender_id` is set to EM's session ID. This happens in three delivery paths:
   - `_try_deliver_messages` (`message_queue.py:789`) — sequential/important batch delivery
   - `_deliver_urgent` codex-app path (`message_queue.py:827`)
   - `_deliver_urgent` tmux path (`message_queue.py:880`)
3. Agent A processes, then calls `sm send` back to EM with its result
4. Agent A goes idle → Stop hook fires → `mark_session_idle` sees `stop_notify_sender_id == EM` → `_send_stop_notification` fires → EM receives `[sm] Agent-A stopped: ...`
5. EM now has **two** messages: the `sm send` result (step 3) and the stop notification (step 4)

The stop notification at step 4 is always redundant when step 3 has already delivered the result to the same recipient.

## Existing Suppression Mechanisms

The codebase already has one suppression mechanism for `sm clear`:

- **`stop_notify_skip_count`** (`_invalidate_session_cache`, `server.py:238-240`): When `sm clear` is called, `skip_count` is incremented by 1. The next Stop hook from the /clear command decrements it and returns early, absorbing the stale hook. `stop_notify_sender_id` is also cleared (`server.py:244-245`), canceling any pending notification from the previous context.

This handles the `sm clear` case. What's missing is suppression for the `sm send` case.

## Proposed Fix

### Approach: Deferred suppression via timestamp check in `mark_session_idle`

Use a two-phase approach: record the outgoing `sm send` after successful enqueue, then check at idle time.

**Phase 1 — Record outgoing `sm send` target after successful enqueue** (`session_manager.send_input`):

The recording is placed AFTER each `queue_message` call returns, ensuring it only executes when the message has been successfully persisted to the queue. This avoids the failure mode where a stale recording causes false suppression after a failed enqueue.

```python
# In send_input, after each queue_message call succeeds:
# (sequential path ~line 679, important/urgent path ~line 695)

# Record outgoing sm send for deferred stop notification suppression (#182)
# Placed after queue_message to ensure message was persisted first.
if from_sm_send and sender_session_id:
    sender_state = self.message_queue_manager._get_or_create_state(sender_session_id)
    sender_state.last_outgoing_sm_send_target = session_id
    sender_state.last_outgoing_sm_send_at = datetime.now()
```

**Phase 2 — Check in `mark_session_idle`** (`message_queue.py`, after skip_count check, before stop notification):

```python
# Suppress redundant stop notification if agent recently sm-sent to the
# same target that would receive the notification (#182)
SUPPRESSION_WINDOW_SECONDS = 30
if state.stop_notify_sender_id and state.last_outgoing_sm_send_target:
    if (state.stop_notify_sender_id == state.last_outgoing_sm_send_target
            and state.last_outgoing_sm_send_at
            and (datetime.now() - state.last_outgoing_sm_send_at).total_seconds()
                < SUPPRESSION_WINDOW_SECONDS):
        logger.info(
            f"Suppressing stop notification for {session_id}: "
            f"agent sm-sent to {state.stop_notify_sender_id} "
            f"{(datetime.now() - state.last_outgoing_sm_send_at).total_seconds():.1f}s ago (#182)"
        )
        state.stop_notify_sender_id = None
        state.stop_notify_sender_name = None
        state.last_outgoing_sm_send_target = None
        state.last_outgoing_sm_send_at = None
```

**New fields on `SessionDeliveryState`** (`models.py`):

```python
last_outgoing_sm_send_target: Optional[str] = None   # Target of last outgoing sm send
last_outgoing_sm_send_at: Optional[datetime] = None   # When last outgoing sm send was recorded
```

### Why deferred instead of eager

1. **Mid-task correctness**: Eagerly clearing `stop_notify_sender_id` in `send_input` would drop the completion signal for any `sm send` — including mid-task status updates where the agent continues working afterward. The deferred approach with a time window limits suppression to `sm send` calls immediately preceding idle (the normal completion pattern).
2. **Failure-mode safety**: Phase 1 records the outgoing target only after `queue_message` returns successfully. If enqueue fails (DB error, exception), the recording is never written, so `mark_session_idle` has nothing to match against and the stop notification fires normally.
3. **No interference with delivery paths**: The suppression check runs in `mark_session_idle`, downstream of all three delivery paths that set `stop_notify_sender_id`. Works correctly regardless of delivery mode.

### Suppression window tradeoff (30s)

The 30-second window is a deliberate design choice that favors **false negatives** (duplicate stop notification preserved) over **false suppression** (completion signal dropped):

- **Within window** (< 30s between `sm send` and Stop hook): Suppressed. This covers the normal completion pattern where the agent sends its result and immediately goes idle.
- **Outside window** (> 30s): Not suppressed. The stop notification fires even though the agent previously sm-sent to the same target. This produces a duplicate notification, but duplicates are noise — losing the completion signal entirely would be a correctness bug.

### Known false-suppression risk

The 30s window does NOT provide a full correctness guarantee against mid-task false suppression. The following scenario can still lose a completion signal:

1. Agent A sends a mid-task status update to EM via `sm send`
2. Agent A continues working but encounters an error or completes quickly
3. Agent A goes idle within 30s of the status update
4. Suppression fires — EM does not receive the stop notification

This is an **accepted risk** for the following reasons:
- Mid-task status updates to the EM do not occur in the current agent workflow (agents always `sm send` as their final action)
- The window is short enough that it only affects very brief post-update processing
- The EM still receives the status update itself (step 1), even if it misses the stop notification
- Eliminating this risk entirely would require an explicit opt-in flag on `sm send` (e.g., `--suppress-stop`), which changes the CLI contract; this can be added as a follow-up if mid-task status updates become a pattern

If the window proves too narrow or too wide in practice, it can be tuned without changing the mechanism.

### What this does NOT suppress

- **Stop hook Telegram notifications**: The Stop hook Telegram path (`server.py:1481-1539`) is unaffected — this notifies the human operator, not the EM agent.
- **Stop notification Telegram mirror**: `_send_stop_notification`'s own Telegram mirror (`message_queue.py:978-982`) IS suppressed as a side effect, since the entire `_send_stop_notification` call is skipped. This is acceptable — if the inter-agent notification is redundant, its Telegram echo is too.
- **Stop notifications to a *different* sender**: If Agent A sm-sends to Agent B but the stop notification target is Agent C, the notification to C is preserved (targets don't match).
- **Stop notifications when agent does NOT sm send**: If Agent A crashes or goes idle without reporting, `last_outgoing_sm_send_target` is unset and the stop notification fires as designed.
- **Stop notifications after window expiry**: If Agent A sm-sent > 30s ago (e.g., mid-task status update), the window has expired and the stop notification fires normally.

## Test Plan

### Unit test: recent `sm send` suppresses stop notification

1. Set up: Agent A has `stop_notify_sender_id = EM_id` in delivery state
2. Record `last_outgoing_sm_send_target = EM_id`, `last_outgoing_sm_send_at = now()`
3. Call `mark_session_idle(Agent_A_id, from_stop_hook=True)`
4. Assert: No stop notification queued for EM
5. Assert: `last_outgoing_sm_send_target` is cleared

### Unit test: expired window does NOT suppress

1. Set up: Agent A has `stop_notify_sender_id = EM_id`
2. Record `last_outgoing_sm_send_target = EM_id`, `last_outgoing_sm_send_at = now() - 60s`
3. Call `mark_session_idle(Agent_A_id, from_stop_hook=True)`
4. Assert: Stop notification IS queued for EM

### Unit test: non-matching target preserves notification

1. Set up: Agent A has `stop_notify_sender_id = EM_id`
2. Record `last_outgoing_sm_send_target = Other_id`, `last_outgoing_sm_send_at = now()`
3. Call `mark_session_idle(Agent_A_id, from_stop_hook=True)`
4. Assert: Stop notification IS queued for EM

### Unit test: `send_input` records outgoing target after enqueue

1. Call `send_input(session_id=EM_id, ..., sender_session_id=Agent_A_id, from_sm_send=True)`
2. Assert: Agent A's `last_outgoing_sm_send_target == EM_id`
3. Assert: Agent A's `last_outgoing_sm_send_at` is recent

### Unit test: failed enqueue does not record target

1. Mock `queue_message` to raise an exception
2. Call `send_input(session_id=EM_id, ..., sender_session_id=Agent_A_id, from_sm_send=True)`
3. Assert: Agent A's `last_outgoing_sm_send_target` is `None`

### Unit test: system messages don't record target

1. Call `send_input(session_id=EM_id, ..., from_sm_send=False)` (system message)
2. Assert: No `last_outgoing_sm_send_target` recorded

### Unit test: mid-task sm send outside window does not suppress

1. Set up: Agent A has `stop_notify_sender_id = EM_id`
2. Record `last_outgoing_sm_send_target = EM_id`, `last_outgoing_sm_send_at = now() - 31s`
3. Call `mark_session_idle(Agent_A_id, from_stop_hook=True)`
4. Assert: Stop notification IS queued for EM (window expired)

### Integration test: end-to-end redundancy elimination

1. EM sends task to Agent A (notify_on_stop=True)
2. Agent A does `sm send` to EM with result
3. Agent A goes idle (Stop hook fires)
4. Assert: EM received exactly one message (the `sm send` result), not two

## Scope

Single ticket. Changes span three files:
- `models.py`: Add 2 fields to `SessionDeliveryState` (~2 lines)
- `session_manager.py`: Record outgoing target in `send_input` after each `queue_message` call (~6 lines, 2 sites)
- `message_queue.py`: Suppression check in `mark_session_idle` (~12 lines)

Plus tests. No schema changes (fields are in-memory only), no new dependencies.
