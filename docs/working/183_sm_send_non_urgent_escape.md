# sm#183: sm send non-urgent delivery — investigation

## Issue

Report: `sm send` (non-urgent/sequential) sends an Escape keystroke to interrupt the target agent before injecting the message.

## Root Cause Analysis

### Finding: Sequential delivery does NOT send Escape

Traced the full code path for `sm send <id> "msg"` (default sequential delivery):

```
cmd_send()  [cli/main.py:282]
  → client.send_input(delivery_mode="sequential")  [cli/client.py:193]
    → POST /sessions/{id}/input  [server.py:901]
      → session_manager.send_input(delivery_mode="sequential")  [session_manager.py:668]
        → message_queue_manager.queue_message(delivery_mode="sequential")  [message_queue.py:445]
          → _try_deliver_messages()  [message_queue.py:691]
            → session_manager._deliver_direct()  [session_manager.py:816]
              → tmux.send_input_async()  [tmux_controller.py:359]
                → tmux send-keys text + Enter  (NO Escape)
```

Escape is sent **only** in `_deliver_urgent()` (message_queue.py:854-860), which is called exclusively for `delivery_mode="urgent"`.

Verified empirically: `inspect.getsource()` on `_try_deliver_messages`, `_deliver_direct`, and `send_input_async` — none reference Escape.

### What the user is actually observing

The behavior described as "Escape interrupting active work" is actually **sequential messages being delivered between agent turns**, disrupting multi-turn logical tasks. This became visible after PR #179 fixed the Enter key regression (#178).

**Before PR #179:** Enter was never sent (paste-detection bug). Messages were pasted into the terminal but never submitted. Agents were never actually disrupted by `sm send` because the message just sat in the terminal buffer.

**After PR #179:** Enter works correctly. Sequential messages are now delivered and submitted when `is_idle=True` (Stop hook fires). If the agent was between turns of a multi-step task (e.g., sent a message via `sm send` in turn 1, waiting for response in turn 2), the queued sequential message arrives and starts a new turn, disrupting the logical flow.

### Other Escape sources investigated and ruled out

| Source | Delivery mode | Sends Escape? | Notes |
|--------|---------------|---------------|-------|
| `_deliver_urgent()` | urgent | Yes | Only for `--urgent` flag |
| Watch idle notification | important | No | Changed from urgent→important in PR #123 |
| Watch timeout notification | important | No | Same PR #123 |
| Stop notification | important | No | Never sent Escape |
| Scheduled reminders | urgent | Yes | But only for explicit `sm remind` |
| `cmd_clear()` | N/A | Yes | Only for `/clear` command |
| Crash recovery | N/A | Yes | Only for harness restart |

### Log evidence

Examined `logs/log-20260203-175042.log` (87K lines). All sequential deliveries used `_try_deliver_messages` → `_deliver_direct` → `send_input_async`. No Escape events during sequential delivery. The only Escape events were from explicit urgent deliveries and watch notifications (on older code that used `delivery_mode="urgent"`).

## The real problem

Sequential delivery works as designed: it waits for `is_idle=True` then delivers. But "idle at the `>` prompt" doesn't mean "agent has no more work." An agent in a multi-turn coordination task (e.g., dispatching work to children, waiting for reviews) gets its flow interrupted by incoming sequential messages arriving between turns.

This is a **design gap**, not a code bug. The delivery system has no concept of "logically busy across multiple turns."

## Proposed solution

### Option A: No-op (document current behavior)

The behavior is working as designed. Agents that need uninterrupted multi-turn flows should use `sm wait` to hold messages until they're ready, or senders should use `--important` mode.

### Option B: Add agent-side "busy" signal

Add `sm busy` / `sm ready` commands. While busy, sequential messages queue but don't deliver. Important and urgent still deliver normally.

```python
# Agent marks itself busy before multi-turn work
sm busy  # Sets busy=True in delivery state

# Sequential messages queue but don't deliver
# Important/urgent still deliver

sm ready  # Clears busy, triggers queued delivery
```

This is the cleanest solution: agents opt-in to deferred delivery when they know they're in a multi-turn flow.

### Option C: Batch sequential messages per-sender

Don't deliver sequential messages from the same sender while the agent is actively processing a previous message from that sender. Wait until the stop notification fires before delivering the next message.

### Recommendation

**Option A** (no-op) is the right call for now. The issue is a misdiagnosis — no Escape is being sent. The observed behavior is sequential delivery working correctly for the first time. If agents need uninterrupted flow, that's a separate feature request (Option B), not a bug fix.

## Test plan

1. Verify sequential delivery with logging: add temporary debug log at `_try_deliver_messages` entry/exit and confirm no Escape subprocess calls
2. Verify urgent delivery: confirm Escape IS sent only in `_deliver_urgent`
3. Manual test: `sm send <idle-agent> "test"` — observe tmux output, confirm text + Enter only

## Classification

This is **not a bug** — it's a misdiagnosis. The issue should be closed as "works as designed" with a note explaining what was actually observed. If the user wants agent-side flow control (Option B), that should be filed as a separate feature request.
