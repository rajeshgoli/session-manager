# sm#244: Queued Message Delivery via Direct Paste

## Problem

Queued message delivery (`delivery_mode=sequential`) gates on `state.is_idle`, which is set
exclusively by the Stop hook. When the Stop hook fails to fire (sm#232, sm#240), messages get
stuck. PR #242 introduced `_check_stuck_delivery()` as a fallback — it polls the tmux pane for
the `>` prompt. But idle detection via tmux output is inherently unreliable (brittle text
matching, timing sensitivity, rendering races).

## Root Cause

The idle gate exists because the original design assumed delivery must only happen when Claude
is "safe to receive input." The assumption is wrong — it doesn't reflect how tmux and the OS
actually work.

## Experiment: Direct Paste While Busy

**Setup:** I ran `tmux send-keys -t <my-own-pane> -- "[sm-test] direct-paste-experiment" Enter`
while I (Claude Code) was mid-turn, actively generating a response.

**Result:** The keystrokes were buffered in the kernel's tty input queue. They did not interrupt
the running turn. When the turn completed and Claude Code returned to the input loop, it read
the buffered text and delivered it as the next message — correctly and completely.

**Why it works:** Claude Code (Node.js, raw terminal mode) reads stdin only when it's polling
for input — i.e., when it's at the `>` prompt. When busy (mid-API call), the process is not
reading stdin. Any keystrokes sent via `tmux send-keys` accumulate in the kernel tty buffer.
When Claude Code next reads stdin, it drains the buffer in order. The message arrives intact.

**Consequence:** Idle detection is unnecessary for queued delivery. The OS tty buffer is a
reliable FIFO queue. Delivery can happen at any time; timing only affects latency, not
correctness.

## Proposal: Direct Delivery for Queued Messages

Replace the idle-gated delivery path with a direct paste (text + settle delay + Enter), mirroring
the `send_input_async` approach already used by `_deliver_direct`, but **without** the ESC
interrupt used by urgent delivery.

```
urgent  = ESC → wait for > prompt → paste + Enter   (interrupts running turn)
queued  = paste + Enter                              (buffers; runs after current turn)
```

Both paths converge on `_deliver_direct` → `send_input_async`. The only difference is whether
we send ESC first.

## Changes Required

### 1. Remove the idle gate from `_try_deliver_messages`

In `message_queue.py`, `_try_deliver_messages()` currently returns early if `state.is_idle` is
False:

```python
# REMOVE THIS GUARD (lines ~875-878):
if not state.is_idle:
    logger.debug(f"Session {session_id} not idle, skipping sequential delivery")
    return
```

After removal: delivery is attempted immediately. If Claude is busy, the message buffers in the
tty and arrives after the current turn. If Claude is at the prompt, it arrives immediately.

The user-typing guard (lines ~882-887) is still correct and should be kept:
```python
current_input = await self._get_pending_user_input_async(session.tmux_session)
if current_input and not state.saved_user_input:
    return  # User is mid-type at prompt; don't concatenate
```
This only fires when Claude is actually at the `>` prompt with typed-but-not-submitted content.
When Claude is mid-turn, `_get_pending_user_input_async` returns None (no `>` on last line).

### 2. Trigger delivery immediately on queue

In `queue_message()`, the sequential path (lines ~541-554) currently checks `state.is_idle`
before scheduling delivery. Simplify to: always schedule `_try_deliver_messages` when a
sequential message arrives.

```python
# REPLACE the sequential elif block with:
elif delivery_mode == "sequential":
    asyncio.create_task(self._try_deliver_messages(session_id))
```

No idle-state check needed — `_try_deliver_messages` will attempt delivery directly.

### 3. Remove `_check_stuck_delivery`

This method existed solely to detect "Claude is at the prompt but Stop hook hasn't fired." With
direct delivery, it's never needed. Remove:
- `_check_stuck_delivery()` method (~lines 792-832)
- The call in `_monitor_loop()` (~line 716)
- The `_stuck_delivery_count` field from `SessionDeliveryState`

### 4. The `important` mode path

`important` delivery currently defers until idle (same guard). Apply the same change: remove the
idle guard. The only gate remaining is the user-typing check.

### 5. `_monitor_loop` simplification

After removing `_check_stuck_delivery`, the monitor loop only needs to run `_check_stale_input`
(handles user who typed partial input and walked away). Keep that loop.

## Review Findings and Required Additions

Three issues were raised in code review. All confirmed valid (two high, one narrow). Fixes
incorporated below.

### Issue 1 (high): False stop-notify on Task X completion

**Root cause:** `_try_deliver_messages` sets `state.stop_notify_sender_id` at paste time (lines
952-955). `mark_session_idle` fires on the very next Stop hook (lines 389-399). If the paste
happened while Task X was running, Task X's Stop hook fires first — before the buffered message
is consumed — sending a false "done" to the sender.

**Previous fixes were broken:**
- `mark_session_active` promotion: `_deliver_direct` calls `mark_session_active` immediately
  (session_manager.py:837), so promotion fires at paste time, not consumption.
- `stop_notify_skip_count` arming: incompatible with the existing 8s stale-fence
  (message_queue.py:344). If Task X runs > 8s, the fence expires, resets, and falls through →
  false notification fires. In hook-loss scenarios (which this ticket targets), Task X's hook
  never fires; skip remains armed; Task Y's Stop hook (the correct one) consumes the skip and
  returns early — suppressing the notification entirely.

**Correct fix: Dedicated `paste_buffered_notify_*` fields with two-phase promotion.**

The key insight: we want the notification to fire on the Stop hook AFTER the first idle
transition following the paste. This is naturally encoded as a two-step state machine:

1. At paste time, when `is_idle == False` and `msg.notify_on_stop == True`:
   - Set `state.paste_buffered_notify_sender_id = msg.sender_session_id`
   - Set `state.paste_buffered_notify_sender_name = msg.sender_name`
   - Do NOT touch `stop_notify_sender_id` (don't arm it yet)

2. In `mark_session_idle`, AFTER the existing `stop_notify_sender_id` check (lines 389-399),
   add a promotion step:
   ```python
   if state.paste_buffered_notify_sender_id:
       state.stop_notify_sender_id = state.paste_buffered_notify_sender_id
       state.stop_notify_sender_name = state.paste_buffered_notify_sender_name
       state.paste_buffered_notify_sender_id = None
       state.paste_buffered_notify_sender_name = None
   ```
   This runs on Task X's Stop hook (normal) or Task Y's Stop hook (hook-loss, Task X's hook
   failed). It sets `stop_notify_sender_id` for the NEXT Stop hook to fire.

3. At paste time, when `is_idle == True` and `msg.notify_on_stop == True`:
   - Set `stop_notify_sender_id` directly (existing code path, no change). Agent is idle and
     will consume the paste immediately; the next Stop hook IS Task Y's completion.

**Trace: normal mid-turn paste**
1. Paste (is_idle=False): `paste_buffered_notify_sender_id = "Agent A"`
2. Task X Stop hook → `stop_notify_sender_id` is None → no notification fired.
   Promotion: `stop_notify_sender_id = "Agent A"`, clear `paste_buffered`. `is_idle = True`.
3. Claude reads buffered tty → Task Y starts. `mark_session_active`.
4. Task Y Stop hook → `stop_notify_sender_id = "Agent A"` → notification fires. ✓

**Trace: hook-loss (Task X's Stop hook fails)**
1. Paste (is_idle=False): `paste_buffered_notify_sender_id = "Agent A"`
2. Task X Stop hook fails. `is_idle` stays False. Claude reads tty → Task Y starts.
3. Task Y Stop hook → `stop_notify_sender_id` is None → no notification.
   Promotion: `stop_notify_sender_id = "Agent A"`, clear `paste_buffered`. `is_idle = True`.
4. Session may process another task (Task Z). Task Z Stop hook → notification fires.
   Best-effort behavior in hook-loss: notification fires one task late.

No 8s time bound. Does not suppress correct notification in hook-loss.
Two new fields added to `SessionDeliveryState` (see New State section).

### Issue 2 (high): `_pasted_session_ids` — session-level tracking is ambiguous

**Previous fix was broken:** The spec proposed `_pasted_session_ids` (session-level) to delay
`delivered_at` until "consumption." The reviewer correctly identified: if new messages queue
before promotion, the session-level set cannot determine which messages to mark delivered.
Per-message tracking would be needed, adding significant complexity.

**Correct fix: Drop `_pasted_session_ids` entirely.**

Mark `delivered_at` immediately at paste time (existing behavior). The false-idle window that
Phase 4 protected against (T2–T3: between Task X's Stop hook and Claude reading the tty buffer)
is ~milliseconds. The `watch_poll_interval` is 2 seconds. The probability of `_watch_for_idle`
polling in that window is ~0.1% — statistically negligible.

**Phase 4 optional guard:** `paste_buffered_notify_sender_id is not None` signals "a mid-turn
paste was made and Task X's Stop hook has not yet promoted it" — i.e., the buffered message is
in-flight. If desired, `_watch_for_idle` Phase 4 can check this field as an additional
suppression signal when `pending=0`. This is cheap (in-memory check) and correct. Whether to
add this guard is an implementation decision for the engineer; it is not required for correctness.

### Issue 3 (narrow): User-typing + hook-fail stuck path

**Root cause:** `_check_stale_input` returns early if `not state.is_idle` (line 778). With
hook failure (is_idle stays False), stale typed input is never detected and delivery stays
blocked.

This scope is narrower than `_check_stuck_delivery` (which addressed all hook-fail cases).
With direct delivery, the general hook-fail case is solved by the tty buffer mechanism. Only
the specific combination of (user was typing at prompt) + (Stop hook failed) requires a fallback.

**Fix: Remove the `is_idle` guard from `_check_stale_input`.**

`_check_stale_input` should run regardless of idle state. It already checks for the `> <text>`
pattern — it won't fire for mid-turn panes where the last line isn't a prompt. Removing the
guard allows it to catch the user-typing + hook-fail corner.

---

## What Is NOT Changed

- **Urgent delivery**: unchanged. ESC + wait-for-prompt is intentional (it interrupts).
- **User-typing detection** (`_get_pending_user_input_async`): kept. Prevents concatenation
  when user has partial input at prompt.
- **Save/restore of user input**: kept. The stale-input path (`_check_stale_input`) is still
  valid (with its guard removed per Issue 3 fix above).
- **Delivery lock**: kept. Prevents double-delivery when two concurrent delivery tasks fire.
- **Stop hook / `mark_session_idle`**: still fires, still used for stop notifications and
  `sm wait`. No change to those paths.
- **Codex / codex-app paths**: unchanged (these don't use tmux delivery).

## What Goes Away

| Removed | Why |
|---------|-----|
| `_check_stuck_delivery()` | Workaround for unreliable idle detection; direct delivery obsoletes it |
| idle gate in `_try_deliver_messages` | Unnecessary: tty buffer handles ordering |
| idle check in `queue_message` sequential path | Same reason |
| `_stuck_delivery_count` in `SessionDeliveryState` | Used only by `_check_stuck_delivery` |

## New State

Two new fields in `SessionDeliveryState` (models.py):

| Field | Type | Purpose |
|-------|------|---------|
| `paste_buffered_notify_sender_id` | `Optional[str]` | Staged stop-notify sender for mid-turn paste; promoted to `stop_notify_sender_id` on first idle transition |
| `paste_buffered_notify_sender_name` | `Optional[str]` | Sender name for the above |

Issue 2 requires no state changes (`_pasted_session_ids` was dropped; `delivered_at` is set
immediately at paste time).

## Test Plan

1. **Mid-turn delivery**: send a sequential message while the target agent is actively processing.
   Verify: message arrives as the next input after the current turn completes. No interruption,
   no truncation.

2. **Idle delivery**: send a sequential message while the target is at the `>` prompt with no
   typed input. Verify: immediate delivery.

3. **Prompt-with-typing**: send a sequential message while the user has typed something at the
   prompt but not submitted. Verify: delivery is deferred (user-typing gate fires), message stays
   in queue. After user submits, message arrives on next idle.

4. **Rapid burst**: send 3 sequential messages in quick succession. Verify: all 3 arrive (order
   preserved), no messages lost. The delivery lock ensures only one delivery runs at a time.

5. **stop-notify mid-turn**: send message with `notify_on_stop=True` to a busy agent
   (is_idle=False). Verify: `paste_buffered_notify_sender_id` is set at paste time (not
   `stop_notify_sender_id`). Verify: Task X's Stop hook promotes to `stop_notify_sender_id`
   without firing. Verify: Task Y's Stop hook fires the notification.

6. **stop-notify idle path**: send message with `notify_on_stop=True` to an idle agent
   (is_idle=True). Verify: `stop_notify_sender_id` is set directly (no `paste_buffered`).
   Verify: notification fires after Task Y's Stop hook.

7. **stop-notify long Task X (> 8s)**: send message with `notify_on_stop=True` while target
   runs a task that takes > 8s. Verify: notification fires after Task Y, not after Task X
   (no 8s stale-fence issue).

8. **`sm wait` correctness**: register `sm wait` on a busy agent, then send a queued message.
   Verify: `sm wait` fires after the agent finishes processing the buffered message (Task Y idle),
   not during the ~ms Task X idle window.

9. **User-typing + hook-fail**: simulate hook failure (is_idle stays False) while user has
   typed at prompt. Verify: `_check_stale_input` still detects stale input after `input_stale_timeout`.

10. **Regression: urgent delivery**: verify urgent delivery still interrupts an active turn.

## Ticket Classification

Single ticket. One agent can implement, test, and close without compacting context.
