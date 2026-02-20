# sm#193: sm wait returns false idle for codex agents

**Status:** Investigation complete — ready for implementation
**Severity:** High — breaks scout→codex review workflows
**Ticket:** https://github.com/rajeshgoli/session-manager/issues/193

---

## Problem Statement

`sm wait <codex-session-id>` returns "idle (waited 0-2s)" immediately after a scout sends a review spec to a codex agent, even when the codex agent is actively processing. This causes scouts to proceed as if the review is complete before it has started.

**Observed sequence:**
```
scout: sm send 0afe1d07 "$(cat spec.md)"
scout: sm wait 0afe1d07
→ [sm wait] knowledge-fold-chunk-4 is now idle (waited 2s)   ← FALSE
```

---

## Investigation

**Repo investigated:** `/Users/rajesh/Desktop/automation/session-manager/`
**Key files:** `src/message_queue.py`, `src/session_manager.py`

---

## Finding 1: Phase 2 (`_check_idle_prompt`) is BROKEN for current Codex CLI (Confirmed empirically)

### Code (message_queue.py:1621)

```python
last_line = output.split('\n')[-1]
return last_line.rstrip() == '>' or last_line.startswith('> ') and not last_line[2:].strip()
```

### What this was designed for

Issue #168 added Phase 2 as a fallback for codex CLI sessions that lack Stop hooks. It checks whether the last visible tmux pane line is `>` (the Codex CLI input prompt), requiring 2 consecutive detections.

### What the current Codex CLI TUI actually shows

Empirical test against live codex sessions `codex-0afe1d07` and `codex-94f9dac5` (Codex CLI v0.104.0+):

```
$ python3 -c "
import subprocess
out = subprocess.run(['tmux','capture-pane','-p','-t','codex-0afe1d07'],
                    capture_output=True).stdout.decode().rstrip()
last = out.split('\n')[-1]
print(repr(last[:80]))
print('_check_idle_prompt result:', last.rstrip() == '>' or (last.startswith('> ') and not last[2:].strip()))
"
→ '  ? for shortcuts                                          100% context left'
→ _check_idle_prompt result: False
```

The idle-state input prompt uses `›` (U+203A, bytes `\xe2\x80\xba`), NOT `>` (U+003E). The last **visible** line in the scrollback is always the "shortcuts/context" status bar, regardless of session state. Additionally, the `›` prompt character is visible during BOTH idle and active states (confirmed on `codex-d638c897`: `› Use /skills...` visible while `(2m 17s • esc to interrupt)` timer was advancing). `_check_idle_prompt` returns `False` for both idle AND active codex sessions — it is permanently broken for the current CLI, and `›` presence is not a valid idle signal.

**Consequence:** Phase 2 can neither detect genuine idle nor cause false idle for codex. This regression was introduced when the Codex CLI TUI changed its layout after issue #168 was fixed. Phase 2 is out of scope for this ticket — the correct replacement signal requires a separate empirical investigation.

---

## Finding 2: The codex `queue_message` path does not reset `session.status` (Code analysis)

### The codex `queue_message` path (message_queue.py:524-530)

```python
elif is_codex:
    state = self._get_or_create_state(target_session_id)
    state.is_idle = True           # ← set synchronously, before delivery
    asyncio.create_task(self._try_deliver_messages(target_session_id))
```

This sets `state.is_idle = True` to gate delivery through `_try_deliver_messages` (which checks `if not state.is_idle: return` on line 827). It is cleared in `_deliver_direct` (session_manager.py:836-837) on successful delivery:

```python
if success and self.message_queue_manager:
    self.message_queue_manager.mark_session_active(session.id)  # sets is_idle=False, status=RUNNING
```

**Key gap:** The codex path sets `state.is_idle = True` but does NOT first call `mark_session_active` to reset `session.status = RUNNING`. If the session has `session.status = IDLE` from a previous idle cycle (set by OutputMonitor), that stale IDLE value persists through the delivery window and is visible to `_watch_for_idle` Phase 3.

---

## Finding 3 (Hypothesis): Phase 3 (`session.status == IDLE`) fires before delivery resets it

### OutputMonitor idle detection (output_monitor.py)

The OutputMonitor watches pipe-pane output for all sessions, including codex CLI. After `idle_timeout = 60` seconds of silence, it sets `session.status = SessionStatus.IDLE`.

### The hypothesized false-idle sequence

This sequence is consistent with the observed "0-2s" timing. It has not been directly instrumented — Fix C (debug logging below) is required to confirm it in production.

```
1. Codex reviewer (0afe1d07) finishes previous task.
   → No output for >60s → OutputMonitor: session.status = IDLE.

2. Scout: sm send 0afe1d07 "$(cat spec.md)"
   → queue_message codex path: state.is_idle = True
   → create_task(_try_deliver_messages)
   [Note: session.status is still IDLE — queue_message does not call mark_session_active]

3. Scout: sm wait 0afe1d07
   → _watch_for_idle starts. First poll (t=0):
      Phase 1: state.is_idle = True → mem_idle = True
      Phase 4: pending messages exist → _check_idle_prompt → False → is_idle = False ✓
      → protected. Sleep 2s.

4. _try_deliver_messages runs async:
   → _deliver_direct → send_input_async → mark_session_active
   → state.is_idle = False, session.status = RUNNING
   → message marked delivered (no longer pending)

5. _watch_for_idle second poll (t=2s):
   Phase 1: state.is_idle = False → mem_idle = False
   Phase 2: _check_idle_prompt → False
   Phase 3: session.status == ???
      IF delivery is slow enough that session.status hasn't been reset yet:
      → session.status == IDLE → mem_idle = True
      Phase 4: no pending messages → is_idle = True → FALSE IDLE FIRES.
```

**Alternative 0s scenario** (sm wait called before sm send):
```
1. session.status = IDLE (from previous OutputMonitor cycle)
2. sm wait 0afe1d07 → _watch_for_idle first poll (t=0):
   Phase 1: state.is_idle = False (no delivery in progress)
   Phase 3: session.status == IDLE → mem_idle = True
   Phase 4: no pending messages → is_idle = True → FIRES IMMEDIATELY (0s)
```

In both cases, the common thread is: `session.status == IDLE` from a prior work cycle is not reset when the new message arrives, because the codex `queue_message` path does not call `mark_session_active` before the delivery task is scheduled.

---

## Root Cause Summary

| Phase | Status | Impact |
|-------|--------|--------|
| Phase 1 | Works transiently (gate for codex delivery), cleared after delivery | Not the false-idle source |
| Phase 2 | **Broken** — `_check_idle_prompt` always False for Codex CLI v0.104.0+ | Cannot detect idle; cannot cause false idle |
| Phase 3 | **Stale IDLE** — `session.status` not reset on message arrival for codex | **Hypothesized false-idle source** |
| Phase 4 | Works when pending messages present; does not engage when none | Fails to protect Phase 3 false idle when delivery completes first |

**Root cause (hypothesis, pending instrumentation):** The codex `queue_message` path does not call `mark_session_active` before scheduling delivery. A stale `session.status = IDLE` from the previous task cycle persists into the `_watch_for_idle` poll window, causing Phase 3 to fire a false idle before delivery resets the status.

---

## Recommended Fix

### Fix A (Primary): Call `mark_session_active` at the start of the codex `queue_message` path

In `queue_message` (message_queue.py:524-530), reset `session.status = RUNNING` synchronously before setting the delivery gate:

```python
elif is_codex:
    # Reset any stale idle status from prior work cycle BEFORE setting is_idle=True.
    # Without this, _watch_for_idle Phase 3 can see session.status=IDLE from
    # OutputMonitor and fire a false idle during the delivery window. (#193)
    self.mark_session_active(target_session_id)          # ← add this line
    state = self._get_or_create_state(target_session_id)
    state.is_idle = True  # re-set: mark_session_active clears is_idle; need True to gate delivery
    asyncio.create_task(self._try_deliver_messages(target_session_id))
```

**Why this works:**
- `mark_session_active` sets `session.status = RUNNING` immediately (sync), eliminating the stale IDLE source for Phase 3.
- `state.is_idle = True` is re-set immediately after (still within the same sync call), so the delivery gate remains intact.
- After delivery: `mark_session_active` is called again inside `_deliver_direct`, redundantly — no harm.
- After codex truly finishes work (60s of silence): OutputMonitor fires, sets `session.status = IDLE`. Phase 3 detects this correctly. `sm wait` returns at the right time.

**What this does NOT change:**
- Phase 2 remains broken but harmless (always returns False).
- Phase 3 remains as the primary codex completion signal via OutputMonitor.
- No behavior change for Claude Code sessions (codex-specific path only).

### Fix C (Instrumentation): Add debug logging to `_watch_for_idle` when idle fires

Add a log entry capturing the exact state at the moment idle is detected. This confirms the root cause in production and provides a regression signal:

```python
if is_idle:
    state = self.delivery_states.get(target_session_id)
    logger.info(
        f"Watch {watch_id}: idle detected at {elapsed:.1f}s — "
        f"state.is_idle={state.is_idle if state else None}, "
        f"session.status={session.status}, "
        f"pending={len(self.get_pending_messages(target_session_id))}, "
        f"prompt_count={prompt_count}, "
        f"pending_idle_count={pending_idle_count}"
    )
```

Deploy Fix C alongside Fix A. If the hypothesis is wrong, Fix C will capture the actual state at fire time.

---

## Out of Scope for This Ticket

**Phase 2 replacement for codex:** Reliable Phase 2 detection would require distinguishing idle from active based on something other than `›` presence (which appears in both states). The active state shows an `esc to interrupt` timer; the idle state shows `? for shortcuts`. A pattern-based detection approach (e.g., checking for the timer string's ABSENCE in a non-last-line scan) requires a dedicated empirical investigation against live sessions in both states. File as a follow-up ticket.

---

## Regression Notes

- Issue #168 introduced `_check_idle_prompt` for codex idle detection. This is now a dead code path for codex. It may still work for Claude Code sessions that use `>` as their last tmux line — verify independently.
- Issue #153 introduced Phase 4 pending-message protection. This correctly protects against the `state.is_idle = True` transient race — but only when messages are in-flight. It does not protect against Phase 3 firing when delivery completes before the poll.

---

## Classification

**Single ticket.** Fix A is ~5 lines in `message_queue.py:524-530`. Fix C is ~8 lines in `_watch_for_idle`. No new subsystems, no separate investigation required, one engineer can complete without compacting context.

Fix B (Phase 2 replacement for codex) is explicitly out of scope and should be filed as a separate ticket after empirical idle-state capture on a live codex session.
