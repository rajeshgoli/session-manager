# sm wait Spurious Fires at 0s/2s After Dispatch (sm#240)

**Status**: Investigation complete — spec reviewed, ready for implementation
**GitHub**: #262 (this fix), #263 (structural skip fence follow-up)
**Related**: sm#234 (dispatch --clear), specs/153_sm_wait_idle_race.md, sm#244
**Scope vs GitHub #240**: GitHub issue #240 describes the *inverse* bug — an idle agent shown as RUNNING in `sm children` because the Stop hook never arrived. This spec covers the *opposite* symptom: an actively-running agent whose `sm wait` fires immediately at 0s or 2s due to a skip-fence race introduced by sm#234's auto-clear. Tracked under #262.

**Repro context**: Observed after `sm dispatch <child> --role <role>` (default, no `--no-clear`) followed by `sm wait <child> <timeout>`. The child has a recently-completed or in-progress previous task whose Stop hook is in-flight when dispatch runs.

---

## Problem

`sm wait` fires at 0s or 2s after `sm dispatch`, even though the target agent is actively running its new task. Verified by immediately checking `sm children` and observing the agent as active.

Symptoms:
- `[sm wait] <agent> is now idle (waited 0s)` — fires immediately on first poll
- `[sm wait] <agent> is now idle (waited 2s)` — fires after one poll interval

In both cases, the agent is still working (confirmed by `sm children` showing RUNNING and/or tool activity continuing).

---

## Investigation

### Code Paths Traced

**`sm wait` implementation**: `cmd_wait` → `client.watch_session` → server `watch_session` endpoint → `asyncio.create_task(_watch_for_idle(...))` in `src/message_queue.py:1808`.

**`_watch_for_idle` polling loop** (`src/message_queue.py:2008`):
- Poll interval: 2s (`watch_poll_interval_seconds`)
- Phase 1: Check `state.is_idle` (in-memory flag)
- Phase 2: If not idle, tmux prompt fallback — fires after 2 consecutive prompt-visible polls
- Phase 3: If still not idle, `session.status == IDLE` fallback
- Phase 4: If any phase returned idle AND pending messages exist, use tmux tiebreaker (2 consecutive confirmations required). Comment: "distinguish stuck from in-flight."

**`sm dispatch` flow** (post sm#234):
1. `cmd_dispatch` → `cmd_clear(client, em_id, agent_id)` (new step added by sm#234)
2. `cmd_clear` → `client.invalidate_cache(target_id)` → server arms `skip_count += 1`, `skip_count_armed_at = now`
3. ESC → wait for prompt → `/clear` → wait for prompt
4. Return from `cmd_clear`
5. `cmd_send` → `queue_message(delivery_mode="sequential")` → `_try_deliver_messages` scheduled
6. `_try_deliver_messages` → `_deliver_direct` → `send_input_async` (text + 0.3s settle + Enter) → `mark_session_active` → `is_idle = False`

**Skip fence mechanics** (`src/message_queue.py:367`):
When `mark_session_idle(from_stop_hook=True)` is called and `skip_count > 0` (within 8s window): absorb the hook, decrement `skip_count`, do NOT change `is_idle`. Return early.

If `skip_count = 0` OR fence stale: fall through → `is_idle = True` is set. Server.py hook handler also sets `session.status = IDLE` (line 1553).

---

## Root Cause

### RCA 1: Skip Fence Consumed by the Wrong Stop Hook (Primary)

`cmd_clear` arms `skip_count = 1` to absorb the `/clear` Stop hook. But the target agent's **previous task Stop hook** (Hook A) may still be in-flight at the time `invalidate_cache` is called.

If Hook A arrives at the server **after** `invalidate_cache` (which armed the fence) but **before** the `/clear` Stop hook (Hook B), Hook A is absorbed by the skip fence (`skip_count: 1 → 0`). The fence is now empty.

When Hook B arrives (`skip_count = 0`), it falls through the fence and calls `mark_session_idle`, which sets `is_idle = True` and (via the server.py handler) `session.status = IDLE`.

This is the critical property: **the skip fence absorbs Stop hooks in arrival order, not by identity**. There is no way to distinguish Hook A (previous task) from Hook B (`/clear`) at absorption time.

```
Timeline for 0s spurious fire:

T=0:   Previous task finishes. Hook A curl starts (async background).
T=0.1: EM calls sm dispatch. invalidate_cache: skip_count = 1 armed at T=0.1.
T=0.15: Hook A curl arrives at server.
        mark_session_idle(from_stop_hook=True)
        → skip_count = 1 > 0 → ABSORBED (skip_count → 0)
        → is_idle NOT changed (stays False, agent was active)
T=0.2: /clear sent. Hook B curl starts.
T=0.3: Prompt visible. cmd_clear returns.
T=0.3: cmd_send → queue_message → _try_deliver_messages scheduled.
T=0.6: _try_deliver_messages delivers: mark_session_active → is_idle = False, no pending.
        was_idle = False → paste_buffered_notify_sender_id = em_id.
T=0.7: Hook B arrives. skip_count = 0 → FALLS THROUGH.
        mark_session_idle: is_idle = True, paste_buffered promoted to stop_notify_sender_id.
        server.py: state.is_idle = True → session.status = IDLE.  ← spurious
T=0.8: EM calls sm wait. _watch_for_idle created.
T=0.8: _watch_for_idle first poll:
        Phase 1: is_idle = True ✓
        Phase 4: no pending messages → Phase 4 skipped (existing code)
        → FIRES at elapsed ~= 0s  ← BUG

T=1.0: Agent uses a tool → PreToolUse hook → mark_session_active
        → is_idle = False, session.status = RUNNING.
T=1.5: User checks sm children → sees RUNNING. "Agent still running!"
```

**Why the agent appears "still running"**: After Hook B sets `session.status = IDLE`, the agent's first tool call fires a PreToolUse hook (server.py line 2309) → `mark_session_active` → `session.status = RUNNING`. By the time the user checks `sm children` after the spurious notification, status has been reset.

**`notify_on_stop` behavior in this scenario**: At delivery, `was_idle = False` (agent was running), so `paste_buffered_notify_sender_id = em_id`. When Hook B fires `mark_session_idle`, `paste_buffered` is promoted to `stop_notify_sender_id`. When Hook C (the real task Stop hook) fires, `stop_notify_sender_id = em_id` is present → notification correctly sent. `notify_on_stop` is NOT lost.

### RCA 2: Late /clear Hook Arrives After First Poll (2s case)

A variant where Hook B arrives during the `watch_poll_interval` sleep between polls 1 and 2:

```
Timeline for 2s spurious fire:

T=0:   Dispatch + delivery complete. is_idle = False, no pending.
T=0.1: sm wait called. _watch_for_idle first poll:
        Phase 1: is_idle = False ← safe this poll
        Phase 2: tmux prompt visible (agent just received message, briefly shows '>')
        → prompt_count = 1 (needs 2 consecutive)
        → no fire
        Sleep 2s.
T=1.5: Hook B arrives (skip_count = 0 from RCA 1):
        is_idle = True. session.status = IDLE.
T=2.1: _watch_for_idle second poll:
        Phase 1: is_idle = True ✓
        Phase 4: no pending → Phase 4 skipped
        → FIRES at elapsed ~= 2s  ← BUG
```

### Why sm#234 Introduced the Regression

Before sm#234, `sm dispatch` did not call `cmd_clear`. There was no skip fence interaction and no `/clear` Stop hook in the dispatch critical path. The only idle state change was from the agent's own task Stop hook after completing the dispatched task, which is the correct signal.

After sm#234, `cmd_clear` is called automatically on every dispatch. The skip fence mechanism (designed for `sm clear` + `sm send --urgent` in sm#174) was not designed to handle concurrent in-flight Stop hooks from the agent's previous task. When the timing is tight (previous task finishes just as dispatch begins), the fence is consumed by the wrong hook.

---

## Known Limitations and Out-of-Scope Items

**Transient stale state**: When Hook B falls through, `is_idle = True` and `session.status = IDLE` are set while the agent is actively running. This stale state persists until the next PreToolUse hook corrects it via `mark_session_active` (~seconds until first tool call). During this window, `sm children` may briefly show the agent as IDLE. Phase 4b (see Fix 1 below) addresses the `_watch_for_idle` symptom but does NOT prevent the stale state from being produced.

**Structural fix deferred**: The root cause — skip fence consumed by wrong hook — requires either (a) arming skip_count = 2 when the agent may be running, or (b) some form of hook identity tracking. These are more complex and riskier to the delivery semantics. Deferred to a follow-up ticket (to be filed).

---

## Proposed Solution

### Fix 1: Add Phase 4b in `_watch_for_idle` — tmux validation for no-pending case

The existing Phase 4 validates `is_idle = True` when **pending messages exist** by checking the tmux prompt. This validation is **missing for the no-pending case** — which is where the 0s fire originates (delivery already completed, no pending, but `is_idle` set spuriously by Hook B).

Extend Phase 4 to also verify the tmux prompt when there are NO pending messages:

```python
# Phase 4: Pending-message validation with tmux tiebreaker (EXISTING)
is_idle = mem_idle
if is_idle and self.get_pending_messages(target_session_id):
    # ... existing 2-consecutive tiebreaker logic (unchanged) ...

# Phase 4b: No-pending validation (NEW — sm#240)
# When is_idle=True but no pending messages, verify tmux prompt before firing.
# Handles spurious is_idle=True from /clear Stop hook bypassing skip fence.
elif is_idle and session.tmux_session:
    prompt_visible = await self._check_idle_prompt(session.tmux_session)
    if not prompt_visible:
        # Agent is working OR tmux capture error. Suppress this poll; retry next.
        is_idle = False
# If no tmux_session: can't verify prompt, fall through and fire (existing behavior).
```

**Behavior on tmux capture error**: `_check_idle_prompt` returns `False` on subprocess failures or empty pane output. Phase 4b treats this the same as "agent not at prompt" — sets `is_idle = False` and retries on the next poll. Maximum additional latency: one poll interval (2s). This is preferable to firing spuriously.

**Why no 2-consecutive requirement for Phase 4b**: Unlike Phase 4 (pending, where "prompt visible" could be a brief flash during delivery), the no-pending case only has two possible states: agent is at `>` (genuinely idle) or agent is working (tmux shows non-prompt output). A single check is sufficient. On error, the next poll retries.

**Handles both cases**:
- 0s case: First poll sees `is_idle = True`, no pending, tmux not at prompt → `is_idle = False` → no fire
- 2s case: Second poll sees `is_idle = True` (set by late Hook B during sleep), no pending, tmux not at prompt → `is_idle = False` → no fire

**Legitimate idle still fires**: When the agent genuinely finishes its task, `is_idle = True` from the real Stop hook AND tmux shows `>` prompt → Phase 4b sees `prompt_visible = True` → `is_idle` unchanged → fires on the first poll after genuine idle.

**Latency caveat**: At most one additional 2s delay if `_check_idle_prompt` returns an error on the poll where idle is first detected. If error clears, fires on the next poll. Not indefinitely delayed.

---

## Files to Change

| File | Change |
|------|--------|
| `src/message_queue.py` | `_watch_for_idle()`: add Phase 4b tmux prompt check for no-pending case |
| `tests/regression/test_issue_153_sm_wait_idle_race.py` | `test_watch_fires_idle_when_no_pending_messages`: mock `_check_idle_prompt` to return `True` (session has a `tmux_session` set; without mock Phase 4b runs real subprocess, likely fails → suppresses legitimate idle fire in test) |

---

## Test Plan

### New regression tests (to add in new file `tests/regression/test_issue_240_spurious_sm_wait.py`)

```python
async def test_phase4b_suppresses_spurious_idle_no_pending_agent_working():
    """
    Phase 4b: is_idle=True, no pending messages, _check_idle_prompt returns False
    (agent actively working). _watch_for_idle must NOT fire 'idle'; must fire 'timeout'.
    """
    # is_idle = True (from spurious /clear hook bypass)
    # No pending messages (delivery completed)
    # _check_idle_prompt mocked to return False (agent working)
    # Assert: watch fires 'timeout', NOT 'idle'

async def test_phase4b_fires_idle_when_agent_at_prompt():
    """
    Phase 4b: is_idle=True, no pending, _check_idle_prompt returns True (agent at prompt).
    _watch_for_idle MUST fire the idle notification (legitimate idle).
    """
    # is_idle = True, no pending, _check_idle_prompt returns True
    # Assert: watch fires 'idle' (not timeout, not suppressed)

async def test_phase4b_suppresses_on_tmux_error_then_fires_next_poll():
    """
    Phase 4b: _check_idle_prompt returns False due to error on first poll,
    then returns True on second poll. Watch must NOT fire on poll 1, MUST fire on poll 2.
    """
    # poll 1: is_idle=True, no pending, _check_idle_prompt returns False → is_idle=False
    # poll 2: is_idle=True, no pending, _check_idle_prompt returns True → fires
    # Assert: notification received after poll 2, not poll 1

async def test_phase4b_no_tmux_fires_immediately():
    """
    Phase 4b: session has no tmux_session. is_idle=True, no pending.
    Cannot verify prompt; fall through to fire immediately (existing behavior preserved).
    """
    # session.tmux_session = None
    # is_idle = True, no pending
    # Assert: watch fires 'idle' immediately

async def test_skip_fence_race_0s_suppressed():
    """
    Full RCA 1 race: is_idle=False (delivery), then is_idle=True (Hook B falls through),
    then sm wait polls. _watch_for_idle must NOT fire at 0s.
    """
    # Set is_idle=True after simulating Hook B fallthrough, no pending
    # _check_idle_prompt returns False (agent working)
    # Assert: timeout, not 0s idle fire
```

### Updated existing test

```python
# tests/regression/test_issue_153_sm_wait_idle_race.py
# test_watch_fires_idle_when_no_pending_messages: add mock for _check_idle_prompt
with patch.object(message_queue, '_check_idle_prompt', new=AsyncMock(return_value=True)):
    await message_queue._watch_for_idle("watch-4", target_id, watcher_id, timeout_seconds=5)
```

### Manual Verification

1. Spawn a child agent with a multi-second task
2. Wait for child to start working (confirm with `sm children`)
3. Call `sm dispatch <child> --role <any>` — this auto-clears and dispatches
4. Immediately call `sm wait <child> 600`
5. Verify: `sm wait` does NOT fire at 0s or 2s
6. Verify: `sm wait` fires only when the child actually finishes its dispatched task
7. Verify: `sm wait --no-clear` (future) or manual `sm wait` without prior dispatch also works (regression)

---

## Ticket Classification

**Single ticket.** Fix is one targeted change in `_watch_for_idle` + one test update + new regression tests. An agent can complete this without compacting context.

File a follow-up ticket for the structural skip-fence fix (arming fence for 2 hooks when agent may be running, or hook-identity tracking). That is a separate, higher-risk change.
