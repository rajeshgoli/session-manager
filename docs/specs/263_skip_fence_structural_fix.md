# sm#263 + sm#240: Skip Fence Structural Fix

## Issues

- **sm#263**: Skip fence absorbs Stop hooks by arrival order, not identity — wrong hook consumed, causing state corruption.
- **sm#240**: `sm children` reports idle agent as RUNNING for 10+ minutes; `sm wait` times out.

Both share the same root: the skip fence in `src/message_queue.py` is count-based with no hook identity tracking.

---

## Architecture: The Skip Fence

`stop_notify_skip_count` (armed by `cmd_clear`) is the gating mechanism that prevents the `/clear` Stop hook from corrupting in-memory idle state. When `sm dispatch` runs:

```
cmd_dispatch
  → cmd_clear
      → client.invalidate_cache()           # arms skip_count += 1, armed_at = now()
      → tmux send-keys /clear               # fires /clear → Claude runs it → Stop hook (async curl)
  → cmd_send (urgent)
      → queue_message() → mark_session_active()  # is_idle = False
      → _deliver_urgent() → register_parent_wake()
```

When the `/clear` Stop hook arrives at the server:
```
mark_session_idle(from_stop_hook=True)
  → cancel_remind()           ← line 356: called BEFORE skip check
  → cancel_parent_wake()      ← line 357: called BEFORE skip check
  → check handoff path
  → skip fence check: skip_count > 0 and armed_at within 8s? → absorb, return
```

If the fence absorbs the hook, `is_idle` stays False. This is correct.

---

## Bug 1: Previous-Task Stop Hook Consumes the Fence (sm#262 Mechanism)

When `sm dispatch` targets an **already-running** agent, the previous task's Stop hook may still be in-flight as curl at arm time.

### Timeline

```
T=0:    cmd_clear: arm skip_count=1, armed_at=T0
T=0:    tmux send /clear
T=0.01: cmd_send: mark_session_active (is_idle=False), register_parent_wake
T=0.4:  Previous task Stop hook curl arrives at server
          → skip_count=1, armed <8s → ABSORBED (skip_count 1→0)
          → cancel_parent_wake() called at line 357 ← BUG (see Bug 2 below)
          → is_idle stays False ✓ (correct so far)
T=1.2:  /clear Stop hook curl arrives
          → skip_count=0 → skip check does NOT trigger
          → mark_session_idle falls through to normal processing:
              cancel_remind() / cancel_parent_wake()  ← line 356-357
              is_idle = True  ← WRONG: agent is running new task
              session.status = IDLE  ← WRONG
              _try_deliver_messages() fires
```

**Result**: Session appears idle while agent is actively running. No pending messages exist, so `_try_deliver_messages` is a no-op. But:
- `cancel_parent_wake()` fires (the registration from T=0.01 is now cancelled)
- `session.status = IDLE`
- Any `sm wait` watcher would fire (won't-fix issue since sm wait removed)

**Impact on sm#240**: Does NOT cause stale-RUNNING state. Causes opposite problem: false IDLE + lost parent wake notification.

---

## Bug 2: cancel_parent_wake Called Before Skip Fence Check

In `mark_session_idle()`, lines 355-357:

```python
if from_stop_hook:
    self.cancel_remind(session_id)      # ← ALWAYS called for any stop hook
    self.cancel_parent_wake(session_id) # ← ALWAYS called for any stop hook
```

These are called before the skip fence check (line 374). This means:

- **Absorbed hooks** (e.g., correct /clear Stop hook absorbed) → `cancel_parent_wake` fires → EM loses parent wake notification for the active task
- **Bug 1 scenario** (previous-task hook absorbed): `cancel_parent_wake` fires inside the absorbed path, then fires AGAIN when /clear falls through

### When cancel_parent_wake Fires for an Absorbed Hook

```
T=0.4: Previous-task Stop hook arrives → skip check absorbs it
         → cancel_parent_wake() fires (line 357) ← WRONG
         → parent wake registration (set at T=0.01) is CANCELLED
         → EM will NOT receive parent wake digest when task finishes
```

The parent_wake was registered at T=0.01 (inside `_deliver_urgent`). The absorbed hook at T=0.4 cancels it even though the agent is actively working.

**Correct behavior**: Only cancel remind/parent_wake when the stop hook is NOT absorbed — i.e., only when the agent genuinely completed a task.

---

## Bug 3: Residual Risk — Skip Fence Absorbs Real Stop Hook (sm#240 Root Cause B)

The existing spec documents this as "residual risk". It is a confirmed additional root cause for sm#240.

### Scenario

```
T=0:   cmd_clear: arm skip_count=1, armed_at=T0
T=0:   /clear Stop hook curl launched (background process)
T=0.5: /clear hook curl KILLED (OS cleanup, network hiccup, SIGTERM)
         → skip_count stays at 1, armed_at=T0
T=0.01-2: Agent receives new task, starts working
T=6:   Agent finishes new task quickly → fires Stop hook curl
         → skip_count=1, now-T0 = 6s < 8s window
         → ABSORBED (skip_count 1→0)
         → is_idle stays False ← PERMANENT
         → session.status stays RUNNING ← PERMANENT
         → sm children shows "running" forever
         → sm wait times out after 600s
```

**This is sm#240.** Whether this specific scenario caused the scout 5399edcb incident is unconfirmed (that agent likely ran for >8s, and the 8s TTL stale-fence reset would have recovered it). However, the mechanism is real and can occur when:
1. The agent is dispatched a quick task (e.g., "check X and respond"), AND
2. The /clear hook curl is killed before reaching the server

The existing sm#240 incident may have been Root Cause A (curl fully dropped + Phase 2 tmux fallback failure). But Root Cause B (residual risk) is structurally identical to the sm#240 symptom and must be addressed.

---

## Connection to sm#262

sm#262 (CLOSED won't-fix) described the same mechanism as Bug 1. It was closed because `sm wait` was removed from the EM workflow. However, the **cancel_parent_wake side effect** (Bug 2) means sm#262's underlying scenario still causes real EM workflow failures:

- EM dispatches an agent that was running
- Previous-task hook consumes the fence
- /clear hook falls through → cancel_parent_wake fires
- EM never receives parent wake digest when the agent finishes the new task

The "won't fix" rationale did not account for the cancel_parent_wake consequence.

---

## Option Analysis

### Option A: Conditional skip_count=2

Arm `skip_count += 2` when the agent is currently RUNNING (`state.is_idle=False`); arm `skip_count += 1` when already idle. This absorbs BOTH the in-flight previous-task hook AND the /clear hook.

**How it works**:
```python
# _invalidate_session_cache(), server.py
if arm_skip:
    state = queue_mgr._get_or_create_state(session_id)
    # If agent is running, absorb 2: in-flight task hook + /clear hook
    slots = 2 if not state.is_idle else 1
    state.stop_notify_skip_count += slots
    state.skip_count_armed_at = datetime.now()
```

**Residual risk with skip_count=2**:
- Any ONE hook lost (previous-task OR /clear) + new task finishes <8s → absorbed → stuck

**Skip_count=2 advantage**:
- Eliminates Bug 1 (previous-task hook consuming fence) in the common case
- New task stop hook only reaches the server AFTER both slots drain

**Skip_count=2 disadvantage**:
- When agent IS idle (no previous task hook), arming 2 means /clear hook (1→0) absorbed correctly, but then next hook from new task also gets absorbed if it arrives within 8s ← WRONG

Wait: if agent is idle at arm time (slots=1), this is not a problem. If agent is running (slots=2), the two slots handle: (1) previous-task hook, (2) /clear hook. New task stop hook falls through. Correct.

**Residual risk scenario with slots=2**:
```
Agent running → slots=2
Previous-task hook arrives → 2→1
/clear hook LOST → fence stays at 1
New task finishes <8s → absorbed → stuck (sm#240 Root Cause B)
```

Same residual risk category as the current code. The 8s stale-fence reset mitigates the >8s cases.

### Option B: Hook Identity Tracking via tmux Environment

Arm the skip fence with a per-clear correlation UUID. Pass the UUID through the tmux session environment so the Stop hook curl includes it. The server matches only hooks carrying the matching UUID.

**Implementation sketch**:
1. `cmd_clear` generates `CLEAR_ID = uuid.uuid4().hex[:8]`
2. `tmux setenv -t <tmux_session> CLAUDE_SM_CLEAR_ID <CLEAR_ID>` (before sending /clear)
3. `notify_server.sh` reads `CLAUDE_SM_CLEAR_ID` and includes it in the POST body
4. Server arms fence with `skip_fence_clear_id = CLEAR_ID`
5. `mark_session_idle`: only absorb if `hook_clear_id == state.skip_fence_clear_id`
6. Previous-task stop hooks (no CLAUDE_SM_CLEAR_ID, or stale value) fall through

**Problem**: If previous-task hook falls through (no matching clear_id), it sets `is_idle=True` again. This re-introduces sm#232 behavior: previous-task hook sets `is_idle=True` → /clear hook arrives (absorbed) → `_try_deliver_messages` fires spuriously.

**Fix for the above**: Combine identity tracking for the /clear hook with a separate count slot for the previous-task hook when agent is running. This is essentially Option A + Option B together.

**Identity tracking also requires**:
- Hook script change (`notify_server.sh`)
- tmux env manipulation in `cmd_clear`
- Server schema change to store `skip_fence_clear_id`
- Handling of the case where `CLAUDE_SM_CLEAR_ID` env var propagates to subsequent turns (must clear it after /clear completes)

**Verdict**: Option B eliminates the identity ambiguity but resurfaces sm#232 for the previous-task hook scenario, and adds significant complexity. It does not reduce residual risk vs. Option A — both still have the "lost hook within 8s window" case.

---

## Recommendation: Option A + Bug 2 Fix

Two-part fix. Both are required.

### Part 1: Conditional skip_count (Option A)

In `_invalidate_session_cache()`, arm additional slot only when the agent is explicitly known to be running.

**Critical constraint**: `_get_or_create_state()` creates a fresh `SessionDeliveryState` with `is_idle=False` (default). Using `not state.is_idle` after `_get_or_create_state()` would treat a **missing delivery state** (first dispatch, post-restart) the same as an explicitly running agent — arming 2 slots when only 1 is needed. This creates the same residual risk that Option A is intended to fix.

**Two-signal guard — delivery state AND session.status must both confirm running**:

`delivery_states.get()` alone is still insufficient after round 1 feedback. Delivery state can exist with `is_idle=False` without the agent genuinely running:
- A prior `_invalidate_session_cache(arm_skip=True)` call creates state via `_get_or_create_state()` (default `is_idle=False`).
- An absorbed /clear hook leaves `is_idle=False` even though the agent is idle waiting for the new task.

In both cases the agent is idle, yet `existing_state is not None and not existing_state.is_idle` is True — still arming 2 slots incorrectly.

**Correct signal**: `session.status == SessionStatus.RUNNING`. This is set by `mark_session_active()` only when a message is actually delivered to the agent, and reset to IDLE when the Stop hook fires. `_invalidate_session_cache` never touches `session.status`. A clear-only path (no dispatch) leaves `session.status` unchanged, so an idle agent with a stale delivery state still shows `session.status = IDLE`.

**`src/server.py`, `_invalidate_session_cache()`**:
```python
if arm_skip:
    # Arm 2 slots only when agent is explicitly known to be running:
    # - existing delivery state (not created by this call) with is_idle=False, AND
    # - session.status == RUNNING (set by mark_session_active on actual delivery)
    # Both conditions required. Either alone is unreliable:
    # - is_idle=False alone: prior clear-only path creates state with default False.
    # - session.status RUNNING alone: persisted status could be stale post-restart.
    # Missing delivery state (first dispatch, post-restart) → 1 slot (sm#263).
    existing_state = queue_mgr.delivery_states.get(session_id)
    session_obj = app.state.session_manager.get_session(session_id) if app.state.session_manager else None
    agent_explicitly_running = (
        existing_state is not None
        and not existing_state.is_idle
        and session_obj is not None
        and session_obj.status == SessionStatus.RUNNING
    )
    slots = 2 if agent_explicitly_running else 1

    state = queue_mgr._get_or_create_state(session_id)
    state.stop_notify_skip_count += slots
    state.skip_count_armed_at = datetime.now()
```

`SessionStatus` is already imported in `server.py`.

**`src/message_queue.py`, `_execute_handoff()`** (line ~1956):
```python
# Arm for handoff /clear hook. Agent just fired a Stop hook (handoff trigger),
# so is_idle is True at this point — only 1 slot needed.
state.stop_notify_skip_count += 1
state.skip_count_armed_at = datetime.now()
```
(Handoff: agent just stopped, so is_idle=True; no previous-task hook in-flight. 1 slot is correct. No change here.)

### Part 2: Move cancel_remind/cancel_parent_wake After Skip Fence Check

In `mark_session_idle()`, move the cancel calls so they only fire when the stop hook is NOT absorbed:

```python
def mark_session_idle(self, session_id, last_output=None, from_stop_hook=False):
    state = self._get_or_create_state(session_id)
    logger.info(f"Session {session_id} marked idle")

    # Check for pending handoff first (sets is_idle=False and returns)
    if from_stop_hook and getattr(state, "pending_handoff_path", None):
        file_path = state.pending_handoff_path
        state.pending_handoff_path = None
        state.is_idle = False
        asyncio.create_task(self._execute_handoff(session_id, file_path))
        return

    # Skip fence check — absorb /clear Stop hooks (sm#232, sm#263)
    if from_stop_hook and state.stop_notify_skip_count > 0:
        armed_at = state.skip_count_armed_at
        if armed_at and (datetime.now() - armed_at).total_seconds() < self.skip_fence_window_seconds:
            state.stop_notify_skip_count -= 1
            if state.stop_notify_skip_count == 0:
                state.skip_count_armed_at = None
            logger.debug(f"Session {session_id}: skip_count decremented to {state.stop_notify_skip_count}")
            asyncio.create_task(self._try_deliver_messages(session_id))
            return  # ← Absorbed: do NOT cancel remind/parent_wake (sm#263)
        else:
            # Stale fence: reset and fall through
            state.stop_notify_skip_count = 0
            state.skip_count_armed_at = None
            logger.warning(f"Session {session_id}: skip fence stale, resetting")

    # NOT absorbed — agent genuinely completed a task.
    # Now safe to cancel remind/parent_wake and mark idle.
    if from_stop_hook:
        self.cancel_remind(session_id)       # ← MOVED: only fires when not absorbed
        self.cancel_parent_wake(session_id)  # ← MOVED: only fires when not absorbed

    state.is_idle = True
    state.last_idle_at = datetime.now()
    # ... rest of method unchanged
```

---

## Case Analysis (Post-Fix)

| Scenario | Delivery state / session.status at arm | Slots | Slots Absorbed | Result |
|----------|----------------------------------------|-------|----------------|--------|
| First dispatch (no prior delivery state) | missing / any | 1 | /clear hook | is_idle=False preserved ✓, parent_wake preserved ✓ |
| Agent idle at dispatch (is_idle=True, status=IDLE) | existing, idle / IDLE | 1 | /clear hook | is_idle=False preserved ✓ |
| Agent idle: is_idle=False but status=IDLE (stale state from prior clear-only path) | existing, is_idle=False / IDLE | 1 | /clear hook | Correctly treats as idle — status=IDLE wins ✓ |
| Agent running at dispatch, prev-task hook arrives first | existing, running / RUNNING | 2 | prev-task (2→1), /clear (1→0) | Both absorbed, task stop hook falls through ✓ |
| Agent running, prev-task hook first, /clear hook lost (>8s) | existing, running / RUNNING | 2 | prev-task (2→1) | Stale fence reset at 8s → task stop falls through ✓ |
| Agent running, prev-task hook first, /clear hook lost (<8s), task stops fast | existing, running / RUNNING | 2 | prev-task (2→1), task stop (1→0) | Residual risk: task stop absorbed ← stuck ⚠ |
| Agent idle at dispatch, /clear hook lost (>8s) | existing, idle / IDLE | 1 | (none — stale reset) | Task stop hook falls through ✓ |
| Agent idle at dispatch, /clear hook lost (<8s), task stops fast | existing, idle / IDLE | 1 | task stop hook | Residual risk: task stop absorbed ← stuck ⚠ |

The residual risk cases remain but are now narrower with Part 1 (conditional skip). Both require: hook lost AND task completes within 8s window. The 8s TTL stale-fence reset handles the common case.

---

## sm#240: Updated Root Cause Assessment

### Root Cause A (Existing, Confirmed)
`/clear` Stop hook curl lost + real task Stop hook curl also lost + Phase 2 tmux fallback fails (reason unconfirmed). Phase 2 should have detected the idle prompt via `tmux capture-pane`, but didn't for the full 600s watch. The sm#240 incident with scout 5399edcb (compacted, ran for extended time) most likely falls here.

Phase 2 failure investigation is still required per the existing spec. Add debug telemetry to `_check_idle_prompt` as previously specified.

### Root Cause B (Structural, Newly Confirmed)
`/clear` Stop hook curl lost + real task Stop hook arrives within 8s window → skip fence absorbs real hook → is_idle stays False permanently. Does not require Phase 2 failure — Phase 2 would eventually detect the idle prompt (agent IS at prompt). However, Phase 2 only runs `if not mem_idle` (line 2087). If is_idle=False (skip fence ate the hook), mem_idle=False → Phase 2 DOES run. Phase 2 SHOULD rescue the situation.

**Wait — if Root Cause B triggers and Phase 2 is active, does Phase 2 rescue?**

Phase 2: checks tmux prompt every 2s, requires 2 consecutive detections (prompt_count >= 2). After 4s, Phase 2 should see the `>` prompt and set mem_idle=True, firing the idle notification.

**This means Root Cause B alone does NOT cause a 10-minute stuck RUNNING state.** Phase 2 would recover within 4-6s. Therefore:

For sm#240 to persist for 10+ minutes, Root Cause B must be combined with Phase 2 failure (same as Root Cause A). The sm#240 incident is still primarily Root Cause A.

**Root Cause B matters for a different scenario**: when no `sm wait` watcher is registered (no Phase 2 running), or when `sm children` is checked ad-hoc without a running watcher. In that case:
- `_watch_for_idle` is not running
- `sm children` reads `session.status` directly
- `session.status` stays RUNNING (since `state.is_idle` never became True)
- Manual `sm wait` started later: Phase 2 detects prompt in 4-6s, rescues correctly

**So Root Cause B causes**: transient stuck state that Phase 2 rescues when a watcher starts. Root Cause A causes: persistent stuck state because Phase 2 also fails. The sm#240 incident was persistent → Root Cause A. But Root Cause B is still a structural gap that should be fixed.

---

## Implementation Approach

| File | Change |
|------|--------|
| `src/server.py` | `_invalidate_session_cache()`: check `delivery_states.get()` and `session.status` before arming; arm `slots=2` only when both confirm running, else `slots=1` |
| `src/message_queue.py` | `mark_session_idle()`: move `cancel_remind`/`cancel_parent_wake` to AFTER skip fence check; only fire when hook falls through as a real task completion |
| `tests/unit/test_message_queue.py` | New tests (see Test Plan) |

---

## Test Plan

| Test | Setup | Assertion |
|------|-------|-----------|
| `test_absorbed_hook_does_not_cancel_parent_wake` | Register parent_wake; arm skip_count=1, is_idle=False, armed <8s. Call `mark_session_idle(from_stop_hook=True)`. | parent_wake still registered (cancel NOT called) |
| `test_absorbed_hook_does_not_cancel_remind` | Register remind; arm skip_count=1, armed <8s. Call `mark_session_idle(from_stop_hook=True)`. | Remind still registered |
| `test_real_stop_hook_cancels_parent_wake` | Register parent_wake; skip_count=0. Call `mark_session_idle(from_stop_hook=True)`. | parent_wake cancelled (cancel IS called) |
| `test_conditional_skip_count_running_agent` | Delivery state exists with `is_idle=False` (explicitly running). Call `_invalidate_session_cache(arm_skip=True)`. | `state.stop_notify_skip_count == 2` |
| `test_conditional_skip_count_idle_agent` | Delivery state exists with `is_idle=True`. Call `_invalidate_session_cache(arm_skip=True)`. | `state.stop_notify_skip_count == 1` |
| `test_conditional_skip_count_no_prior_state` | No delivery state (first dispatch, `delivery_states` dict empty). Call `_invalidate_session_cache(arm_skip=True)`. | `state.stop_notify_skip_count == 1` (missing state defaults to 1, not 2) |
| `test_conditional_skip_count_stale_state_status_idle` | Delivery state exists with `is_idle=False` (stale from prior clear-only path) but `session.status == IDLE`. Call `_invalidate_session_cache(arm_skip=True)`. | `state.stop_notify_skip_count == 1` (status=IDLE overrides is_idle=False; treated as idle) |
| `test_two_slot_fence_absorbs_prev_task_and_clear` | skip_count=2, armed <8s. Call `mark_session_idle` twice. | First call: absorbed, skip_count=1. Second: absorbed, skip_count=0. is_idle remains False throughout. |
| `test_prev_task_hook_absorbed_clear_hook_falls_through` (skip_count=1 regression) | skip_count=1, armed <8s. Call `mark_session_idle` (prev-task hook). Then call again (/clear hook). | First: absorbed (skip_count 1→0). Second: falls through, is_idle=True. Demonstrates old behavior requiring 2-slot fix. |

---

## Ticket Classification

**Single ticket.** Two files modified (server.py, message_queue.py), nine unit tests. No schema changes. Engineer can complete without context compaction.

**Suggested order**:
1. Part 2 (move cancel_remind/cancel_parent_wake) — simplest change, cleanest correctness improvement, can be shipped independently
2. Part 1 (conditional skip_count=2) — depends on understanding Part 2 to write tests correctly

**Does NOT address sm#240 Phase 2 failure** (Root Cause A). That remains an open investigation requiring telemetry as specified in `docs/working/232_sm_wait_false_idle_after_clear.md`. The sm#240 issue should remain open pending that telemetry.
