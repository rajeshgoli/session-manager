# Issue #178 — Regressions from #175 fix: Enter never sent + delivery race

**Repo:** rajeshgoli/session-manager
**Issue:** https://github.com/rajeshgoli/session-manager/issues/178
**Introduced by:** PR #176 (fix for #175)

---

## Problem Statement

PR #176 introduced two regressions:

1. **Enter key never sent (persistent):** `sm send` pastes text into the tmux pane but never submits it. Every `sm send` requires manual Enter. This was intermittent before #175 (Bug B) and is now persistent — the fix made it worse.

2. **Urgent delivery race window:** The 3-second prompt-polling window in `_deliver_urgent` allows Stop hooks to fire and deliver queued sequential messages before the urgent message is delivered, causing out-of-order delivery.

---

## Root Cause Analysis

### Regression 1 — Atomic `text + \r` bypasses paste detection settle delay

**What PR #176 changed:**

The old `send_input_async` (`tmux_controller.py`) used two separate tmux send-keys calls with an intentional delay between them:

```python
# Old code (pre-PR #176)
proc = await asyncio.create_subprocess_exec(
    'tmux', 'send-keys', '-t', session_name, '--', text, ...)
await asyncio.sleep(self.send_keys_settle_seconds)  # 0.3s — "to avoid paste detection"
proc = await asyncio.create_subprocess_exec(
    'tmux', 'send-keys', '-t', session_name, 'Enter', ...)
```

PR #176 replaced this with a single atomic call:

```python
# New code (PR #176)
payload = text + "\r"
proc = await asyncio.create_subprocess_exec(
    'tmux', 'send-keys', '-t', session_name, '--', payload, ...)
```

**Why the new approach breaks:**

The 0.3s settle delay was not arbitrary. Both the sync `send_input` (line 313) and async `send_input_async` had explicit comments: *"Small delay between send-keys calls to avoid paste detection"* and *"Brief delay to avoid paste detection (non-blocking)."*

Claude Code is a Node.js TUI application that runs in raw terminal mode. When characters arrive in rapid succession (as they do from a single `tmux send-keys` call), Claude Code's input handler treats the burst as pasted text. During paste processing, control characters like `\r` (0x0D) are treated as literal characters within the paste, not as submit/Enter keystrokes.

The old two-call approach with the 0.3s settle delay worked because:
1. Text characters arrive as a rapid burst → Claude Code enters paste mode → text is buffered
2. 0.3s gap → Claude Code exits paste mode (paste is complete)
3. Enter (0x0D) arrives as a separate keystroke → treated as submit command

The new atomic approach fails because:
1. Text characters + `\r` all arrive as a single rapid burst → Claude Code enters paste mode
2. The `\r` at the end is treated as part of the paste (literal character, not submit)
3. Paste ends when input stops → text is in the input buffer but never submitted

**Empirical verification of the byte-level behavior:**

I verified that `tmux send-keys -- "text\r"` (with Python's actual 0x0D byte) correctly delivers 0x0D to the target process's stdin — the same byte that `tmux send-keys Enter` delivers. I also verified that tmux does NOT send bracketed paste markers for `send-keys` (with or without `-l`), even when the target pane has enabled bracketed paste mode. The issue is therefore not about the byte value but about the **timing**: `\r` arriving as part of a rapid character burst vs. arriving after a pause.

**Scope of impact:**

ALL delivery paths for tmux-based sessions go through `send_input_async`:
- Sequential delivery: `_try_deliver_messages` → `_deliver_direct` → `send_input_async`
- Important delivery: same path
- Urgent delivery: `_deliver_urgent` → `_deliver_direct` → `send_input_async`
- `cmd_clear`: uses atomic `subprocess.run(["tmux", "send-keys", ..., clear_command + "\r"])` — same pattern

The sync `send_input` was NOT changed by PR #176 and still uses the two-call approach with settle delay, but it's only used for spawning sessions (not for message delivery).

### Regression 2 — 3-second prompt polling creates Stop hook race window

**What PR #176 changed:**

The old `_deliver_urgent` used a fixed sleep after Escape:

```python
# Old code
await asyncio.sleep(self.urgent_delay_ms / 1000)  # 0.5s default
```

PR #176 replaced this with prompt polling:

```python
# New code
await self._wait_for_claude_prompt_async(session.tmux_session)  # up to 3s
```

**The race condition:**

When Escape interrupts Claude Code's streaming response, the following sequence occurs:

1. `_deliver_urgent` sends Escape to the target pane
2. `_wait_for_claude_prompt_async` starts polling for `>` prompt (up to 3 seconds)
3. Claude Code stops streaming → Stop hook fires → `mark_session_idle()`
4. `mark_session_idle` calls `asyncio.create_task(_try_deliver_messages(session_id))`
5. `_try_deliver_messages` acquires the delivery lock and delivers queued sequential/important messages
6. `_wait_for_claude_prompt_async` detects the `>` prompt and returns
7. `_deliver_urgent` calls `_deliver_direct` to send the urgent message

**Result:** Sequential messages queued for the target are delivered (step 5) BEFORE the urgent message (step 7). This causes:
- Out-of-order delivery — sequential messages arrive before the urgent one
- If the delivered sequential message triggers Claude to start processing, the urgent message arrives during processing and may not be read until Claude finishes
- If the Stop hook also sends a stop notification to the sender, the sender receives "session stopped" before the urgent message was even processed

**Why this didn't happen before:**

With the old 0.5s `asyncio.sleep`:
- The sleep returns after exactly 0.5s — regardless of Claude Code's state
- In 0.5s, the Stop hook might not have fired yet, or might be in flight
- The delivery happens immediately after the sleep
- The race window is small (0.5s max)

With the new 3s prompt polling:
- Polling actively WAITS for the Stop hook to complete (since `>` prompt appears after Claude finishes stopping)
- This guarantees the Stop hook fires BEFORE delivery proceeds
- Any sequential messages in the queue are delivered during the Stop hook processing
- The race window is 3s max, and the polling mechanism actually makes the race MORE likely (not less), because it waits for the exact state that triggers the race

**Lock analysis:** `_deliver_urgent` does NOT acquire the per-session delivery lock (`_delivery_locks`). Only `_try_deliver_messages` does. This means `_deliver_urgent` and `_try_deliver_messages` can deliver to the same session concurrently, without any mutual exclusion.

---

## Proposed Fix

### Fix 1 — Restore settle delay between text and Enter

Revert `send_input_async` to the two-call approach with the settle delay, but use `proc.communicate()` instead of `proc.wait()` (the one improvement from PR #176 that IS correct):

```python
async def send_input_async(self, session_name: str, text: str) -> bool:
    # ... existing validation ...
    try:
        # Send text
        proc = await asyncio.create_subprocess_exec(
            'tmux', 'send-keys', '-t', session_name, '--', text,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await asyncio.wait_for(
            proc.communicate(), timeout=self.send_keys_timeout_seconds
        )
        if proc.returncode != 0:
            logger.error(f"Failed to send text: {stderr.decode()}")
            return False

        # Settle delay to avoid paste detection (#178)
        await asyncio.sleep(self.send_keys_settle_seconds)

        # Send Enter as separate keystroke
        proc = await asyncio.create_subprocess_exec(
            'tmux', 'send-keys', '-t', session_name, 'Enter',
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await asyncio.wait_for(
            proc.communicate(), timeout=self.send_keys_timeout_seconds
        )
        if proc.returncode != 0:
            logger.error(f"Failed to send Enter: {stderr.decode()}")
            return False

        logger.info(f"Sent input (async) to {session_name}: {text[:50]}...")
        return True
    except ...
```

Also revert the same pattern in `cmd_clear` (back to two separate send-keys calls with a settle delay).

**Why this doesn't reintroduce #175 Bug B:** The original Bug B ("missing Enter") was caused by the Enter subprocess failing silently (timeout or tmux session killed between the two calls). The 0.3s settle delay itself was not the cause — it was the lack of error handling on the second call. The proposed fix keeps `proc.communicate()` and checks `returncode` for both calls, making failures observable. The 0.3s settle is the minimum time needed for Claude Code to exit paste mode.

### Fix 2 — Gate urgent delivery against the delivery lock

Add the per-session delivery lock to `_deliver_urgent` to prevent it from racing with `_try_deliver_messages`:

```python
async def _deliver_urgent(self, session_id: str, msg: QueuedMessage):
    # ... existing paused/session checks ...

    # Acquire delivery lock to prevent racing with _try_deliver_messages (#178)
    lock = self._delivery_locks.setdefault(session_id, asyncio.Lock())
    async with lock:
        # Send Escape
        # Wait for prompt
        # Deliver via _deliver_direct
```

This ensures that if a Stop hook fires during prompt polling and triggers `_try_deliver_messages`, the sequential delivery waits until the urgent delivery completes (or vice versa). This eliminates the out-of-order delivery race.

**Consideration:** The lock is held during the 3-second prompt polling window. This means sequential messages can't be delivered during this time. This is the correct behavior — urgent messages should preempt sequential ones, not race with them.

### Fix 3 — Mark session active before Escape in urgent delivery

To further prevent the race, mark the session as active (`is_idle = False`) before sending Escape:

```python
async def _deliver_urgent(self, session_id: str, msg: QueuedMessage):
    # ... existing checks ...
    state = self._get_or_create_state(session_id)
    state.is_idle = False  # Prevent _try_deliver_messages from delivering during our window
    # ... Escape, poll, deliver ...
```

This prevents `_try_deliver_messages` from even attempting delivery (it checks `state.is_idle` and returns early if False). Combined with Fix 2, this provides defense-in-depth.

---

## Test Plan

### Regression 1 (Paste detection)

1. **Unit test:** Verify `send_input_async` makes TWO subprocess calls (text, then Enter) with the settle delay in between — not a single call with `\r`
2. **Unit test:** Verify `send_input_async` returns False and logs error when EITHER the text call or the Enter call fails
3. **Unit test:** Verify the settle delay is at least `send_keys_settle_seconds` (0.3s)
4. **Integration test:** Send a multi-line payload (simulating `[Input from: ...]\nActual message`) via `send_input_async` to a real tmux session running a test program. Verify the text AND Enter are received (the `\r` byte appears after the settle delay)
5. **Verify `cmd_clear` also uses two-call approach** with settle delay (not atomic `\r`)

### Regression 2 (Delivery race)

1. **Unit test:** Verify `_deliver_urgent` acquires the per-session delivery lock
2. **Unit test:** Simulate concurrent `_deliver_urgent` and `_try_deliver_messages` for the same session — verify only one runs at a time (lock mutual exclusion)
3. **Unit test:** Verify `state.is_idle` is set to False before Escape is sent in `_deliver_urgent`
4. **Integration test:** Send an urgent message to a session that also has a sequential message queued. Verify the urgent message is delivered first (not the sequential one)

---

## Files Changed

| File | Change |
|------|--------|
| `src/tmux_controller.py` | Revert `send_input_async` to two-call approach with settle delay, keep `proc.communicate()` improvement |
| `src/message_queue.py` | Add delivery lock and `is_idle=False` to `_deliver_urgent` |
| `src/cli/commands.py` | Revert `cmd_clear` atomic send-keys back to two-call approach with `_wait_for_claude_prompt` delay |
| `tests/regression/test_issue_175_send_truncation.py` | Update Bug B tests to verify two-call approach instead of atomic |
| `tests/regression/test_issue_178_sm_send_regressions.py` | New test file for both regressions |
| `tests/regression/test_issue_78_clear_completed.py` | Update assertions for two-call send-keys pattern |
| `tests/regression/test_issue_88_urgent_completed.py` | Add delivery lock verification |

---

## Ticket Classification

**Single ticket.** Changes are focused on `send_input_async`, `_deliver_urgent`, and `cmd_clear`. All changes are in the same three files (plus test updates). An engineer can complete this without context compaction.
