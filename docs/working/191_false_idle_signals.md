# False Idle/Completion Signals from `sm wait` and Stop Hooks (#191)

## Problem

Two interrelated false signal patterns waste EM tokens every dispatch cycle:

1. **`sm wait` Phase 3 false idle**: `sm wait <id> 600` fires "idle (waited 0-2s)" immediately
   after dispatch on the second+ dispatch to any session.
2. **Double notification per completion**: Every agent completion generates two near-simultaneous
   signals to the EM — a stop notification and an `sm wait` idle — both correct but redundant.

**Frequency (from EM report):** Every dispatch. 2-3 false/redundant signals per task cycle = 6-10
per session. Each requires a context read + response to dismiss.

---

## Root Cause 1: Phase 3 False Idle — Stale `session.status` After Urgent Delivery

### What happens

`_watch_for_idle` (message_queue.py) uses 4-phase idle detection, introduced by PR #181:

```
Phase 1: state.is_idle (in-memory SessionDeliveryState)
Phase 2: tmux prompt check (2 consecutive detections required)
Phase 3: session.status == IDLE (Session model field)   ← BUG
Phase 4: pending-message validation with tmux tiebreaker
```

Phase 3 reads `session.status` as a fallback when Phases 1 and 2 both return "not idle." Its
design intent (spec #180) is to catch in-memory corruption where `state.is_idle` is wrong.

### The bug

`session.status` is set to `IDLE` by the Stop hook handler every time Claude finishes a turn:

```python
# server.py:1521-1523
app.state.session_manager.update_session_status(session_manager_id, SessionStatus.IDLE)
```

When the EM dispatches the next task via `sm send --urgent`:
- `mark_session_active(target)` is called → sets `state.is_idle = False` ✓
- `_deliver_urgent` delivers the message → sets `state.is_idle = False` (redundant) ✓
- **`session.status` is NEVER updated to RUNNING** ← gap

Neither `queue_message(urgent)`, `mark_session_active()`, nor `_deliver_urgent()` update
`session.status`. Only `_try_deliver_messages` does (for sequential mode), and the fallback
`_deliver_direct` path for tmux Claude sessions:

```python
# session_manager.py:835-838
success = await self.tmux.send_input_async(session.tmux_session, text)
if success and self.message_queue_manager:
    self.message_queue_manager.mark_session_active(session.id)
return success
# ← session.status NOT updated
```

(codex-app sessions ARE correctly updated: lines 824-826)

### Sequence of events (false idle at 0-2s)

```
t=0    EM: sm send scout "task" --urgent
         → mark_session_active(scout):  state.is_idle = False
         → _deliver_urgent scheduled as asyncio task
         → (session.status = IDLE — stale from previous Stop hook)

t=0+ε  EM: sm wait scout 600
         → _watch_for_idle task scheduled

t=0+2ε _watch_for_idle polls (first iteration):
         Phase 1: state.is_idle = False → mem_idle = False ✓
         Phase 2: _check_idle_prompt(tmux)
                  Claude is processing OR tmux briefly shows > during Escape settling
                  → prompt_count increments but not yet ≥ 2
                  → mem_idle = False ✓
         Phase 3: session.status == IDLE → True → mem_idle = True ← BUG
         Phase 4: No pending messages
         → is_idle = True → FIRES "[sm wait] scout is now idle (waited 0s)"
```

### Why it didn't exist before PR #181

Phase 3 was added by commit `f9f1af6` (PR #181, fix for #180). Before that commit, the
only fallback was Phase 2 (tmux prompt). The logs from before 2026-02-04 show no false idle
detections because Phase 3 didn't exist. The issue was introduced by the #180 fix.

### Condition

Fires only on **second+ dispatch** to any session. First dispatch has `session.status = RUNNING`
(set at session creation: `session_manager.py:384`). After the first Stop hook, `session.status`
becomes `IDLE` and stays stale between dispatches.

---

## Root Cause 2: Double Notification per Completion (Structural Redundancy)

In **non-#182 paths** (when the agent does not `sm send` back to its dispatcher within 30s),
completion of a dispatched agent sends **two** signals to the EM:

```
t=258s  Scout Stop hook fires
          → mark_session_idle(scout)
          → _send_stop_notification: "[sm] scout stopped: {last_output}" → EM (immediate)

t=260s  _watch_for_idle polls (Phase 1 now sees is_idle=True)
          → "[sm wait] scout is now idle (waited 258s)" → EM
```

**Evidence from runtime logs (observed, not checked-in artifacts):**
```
10:04:56,848 - Sent stop notification to 2837e40d (recipient: 76ea0dfc)
10:04:58,093 - Watch 7dc28726df09: 76ea0dfc idle after 258s
```

Both signals fire within 2 seconds of each other. Both are technically correct, but redundant.

**Note on #182 suppression**: When the agent recently `sm send`-ed to the same dispatcher
(within 30s), `mark_session_idle()` lines 343-360 suppress the stop notification. In those
cases EM receives only the `sm wait` idle — one signal. The double notification is limited to
non-#182 paths where the agent completes silently (no `sm send` back). For a scout doing a
silent investigation (no outgoing `sm send` during the task), both signals reach EM.

This double-notification happens independently of RCA #1 (Phase 3). Even with Phase 3 fixed,
the stop notification + `sm wait` idle still both fire in non-#182 completion paths.

---

## Root Cause 3: Stop Hook "Fires While Agent Active" — Multi-Turn Agents

The stop notification fires on the **first** Stop hook after message delivery. For a well-structured
agentic task (single turn: all tool calls + final response), this fires at task completion.
That's correct.

However, if an agent does **multiple turns**, the stop notification fires after turn 1:

```
EM dispatches scout → notify_on_stop=True → stop_notify_sender_id = EM

Scout, Turn 1: reads persona file, reads GitHub issue, says "I'll start investigating"
  Stop hook fires → stop notification fires to EM ← FALSE (scout not done)
  stop_notify_sender_id = None (cleared)

Scout, Turn 2: reads codebase, writes spec, says "Done. Spec at docs/working/191_..."
  Stop hook fires → no notification (stop_notify_sender_id already cleared)
```

The EM receives the stop notification after Turn 1 and thinks the scout is done. `sm what scout`
reads the tmux output — which shows the LAST TOOL OUTPUT (spec content being written in Turn 2)
— and haiku summarizes "agent is writing a spec." This gives the false impression the agent is
still active when it actually already finished Turn 1 and is now running Turn 2 autonomously.

**When multi-turn happens:**
- Agent produces an intermediate conversational response before starting tool work
- Agent hits context limit and is handed off (compaction + new prompt)
- Agent receives an `sm send` message mid-task that triggers a new turn
- Long investigation tasks where Claude pauses to reason out loud before tool use

**Code location:** `message_queue.py:mark_session_idle()` lines 362-371 — fires stop notification
unconditionally on first Stop hook that sees `stop_notify_sender_id` set.

---

## Proposed Solutions

### Fix 1: Update `session.status` on Urgent Delivery (addresses RCA 1)

The targeted fix: wherever `mark_session_active()` is called on urgent dispatch, also update
`session.status = SessionStatus.RUNNING`. The most reliable location is `mark_session_active()`
itself, since it already has access to `self.session_manager`:

```python
# message_queue.py: mark_session_active()
def mark_session_active(self, session_id: str):
    """Mark a session as active (not idle)."""
    from .models import SessionStatus  # import before use — avoids UnboundLocalError
    state = self._get_or_create_state(session_id)
    state.is_idle = False
    # Sync session.status with in-memory state to prevent Phase 3 false positives (#191)
    session = self.session_manager.get_session(session_id)
    if session and session.status != SessionStatus.STOPPED:
        session.status = SessionStatus.RUNNING
    logger.debug(f"Session {session_id} marked active")
```

This keeps `state.is_idle` and `session.status` in sync across ALL callers of `mark_session_active`:
- `queue_message(urgent)` (before `_deliver_urgent`)
- `_deliver_direct` for tmux sessions (after message sent)
- Server PreToolUse hook (`server.py:2269`) — marks active when agent starts a tool

**Alternative**: Narrow fix directly in `_deliver_urgent` — update `session.status = RUNNING` after
successful delivery. Simpler but misses the PreToolUse hook path.

**Note on Phase 3 design**: Phase 3 was designed for "in-memory corruption" but `session.status`
is equally susceptible to staleness. With Fix 1, Phase 3 still provides value when `session.status`
is the ONLY signal (e.g., after server restart with no in-memory state), but no longer causes false
positives because `session.status` will be correct after urgent delivery.

### Fix 2: Suppress Redundant `sm wait` Idle When Stop Notification Already Fired (addresses RCA 2)

Track per-watcher that a stop notification was already sent for this specific (target, watcher)
pair, and suppress the subsequent `sm wait` idle if it fires within a short window (e.g., 10s).

**Scoping requirement**: suppression must be watcher-aware. Multiple callers can watch the same
target simultaneously — a target-level timestamp would incorrectly suppress `sm wait` idle for
watchers that never received the stop notification (which only goes to `stop_notify_sender_id`).

Implementation: record the (target, watcher) pair + timestamp in `_send_stop_notification`,
and check it in `_watch_for_idle` using the task-local `watcher_session_id`:

```python
# message_queue.py: _send_stop_notification()
# Record that this watcher received the stop notification for this target
key = (recipient_session_id, sender_session_id)  # (target, watcher)
self._recent_stop_notifications[key] = datetime.now()
```

```python
# message_queue.py: _watch_for_idle(), before queueing notification
if is_idle:
    key = (target_session_id, watcher_session_id)
    stop_at = self._recent_stop_notifications.get(key)
    if stop_at and (datetime.now() - stop_at).total_seconds() < 10:
        logger.info(f"Watch {watch_id}: suppressing idle — stop already sent to this watcher <10s ago")
        self._recent_stop_notifications.pop(key, None)
        return
    # ... else queue notification
```

`_recent_stop_notifications: Dict[Tuple[str, str], datetime]` is an in-memory dict (no
persistence needed — 10s window is too short for crash recovery to matter).

This prevents the second (redundant) signal from reaching EM on normal completion while
correctly delivering to any additional watchers who did not receive the stop notification.

**Trade-off**: If `sm wait` is being used as a BACKUP (in case stop notification fails), this
suppression would hide the backup signal. Given that stop notification uses the same `important`
delivery mechanism as `sm wait` idle, the backup value is low.

### Fix 3: Multi-Turn Stop Notification — Add Skip Count for Intermediate Turns (addresses RCA 3)

Extend the existing `stop_notify_skip_count` mechanism to absorb intermediate turns. When an agent
is dispatched, allow the EM to specify how many intermediate stop hooks to absorb before firing
the stop notification:

```python
# In queue_message / _deliver_urgent, when notify_on_stop=True:
state.stop_notify_skip_count = 0  # or configurable: notify_on_stop_skip=N
```

**But this requires knowing how many turns the task will take** — not knowable upfront.

**Alternative**: Change the stop notification semantics. Instead of firing on the first Stop hook,
fire on the LAST Stop hook before the session is re-dispatched by another `sm send`. Use a timer:
if another Stop hook fires within 60s, reset the notification window. Fire only when no Stop hook
has fired for 60s (genuine completion).

This is a significant redesign and should be a separate ticket.

---

## Implementation Approach

### Minimal fix (addresses the most disruptive issue: RCA 1)

| File | Change |
|------|--------|
| `src/message_queue.py` | `mark_session_active()`: also update `session.status = SessionStatus.RUNNING` |

### Extended fix (addresses RCAs 1 + 2)

| File | Change |
|------|--------|
| `src/message_queue.py` | `mark_session_active()`: update `session.status = RUNNING` |
| `src/message_queue.py` | `_watch_for_idle()`: suppress idle notification if stop notification sent <10s ago |
| `src/models.py` | `SessionDeliveryState`: add `last_stop_notification_at: Optional[datetime] = None` |
| `src/message_queue.py` | `_send_stop_notification()`: record `state.last_stop_notification_at = datetime.now()` |

### What NOT to change

- Phase 3 itself (keep it for server-restart resilience; Fix 1 makes it non-harmful)
- The `notify_on_stop=True` default (it's a useful feature; redundancy is addressed by Fix 2)
- The Stop hook fire-and-forget design (non-blocking by design; resilience handled on client side)

---

## Test Plan

### Tests for Fix 1 (Phase 3 false idle)

| Test | Description |
|------|-------------|
| `test_watch_no_false_idle_after_urgent_dispatch` | Session has `state.is_idle=False`, `session.status=IDLE` (stale) — verify Phase 3 does NOT fire false idle |
| `test_mark_session_active_updates_session_status` | Call `mark_session_active()` — verify `session.status` becomes `RUNNING` |
| `test_phase3_still_works_after_fix` | Session with `state.is_idle=False`, `session.status=IDLE`, `tmux_session=None` — verify Phase 3 still works for server-restart resilience case |

### Tests for Fix 2 (double notification suppression)

| Test | Description |
|------|-------------|
| `test_sm_wait_suppressed_after_stop_notification` | Stop notification fires → `sm wait` idle fires within 5s → verify EM gets only 1 signal |
| `test_sm_wait_not_suppressed_after_window_expires` | Stop notification fires → `sm wait` fires after 15s → verify EM gets both (different events) |

### Manual Verification

```bash
# Dispatch → watch → verify no false idle
sm send <agent> "task" --urgent
sm wait <agent> 600
# Expected: no "[sm wait] idle (waited 0s)" or "waited 2s" fires immediately
# Expected: ONE notification when agent genuinely finishes (stop OR sm wait, not both)
```

---

## Ticket Classification

**Epic** — two distinct fixes across multiple files:

- **Sub-ticket A** (single): `mark_session_active` updates `session.status` → fixes Phase 3 false idle.
  One method change, 2-3 unit tests. Engineer can complete without context compaction.

- **Sub-ticket B** (single): Suppress redundant `sm wait` idle when stop notification recent.
  Models + queue manager change, 2 unit tests. Engineer can complete without context compaction.

- **Sub-ticket C** (spec only, future): Multi-turn stop notification redesign — defer until
  observed frequently enough to justify architectural change.

File as epic with A and B as sub-tickets. C is a separate investigation ticket.
