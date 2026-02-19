# sm#188: sm remind — Periodic Status Update Reminders

**Issue:** https://github.com/rajeshgoli/session-manager/issues/188
**Status:** Draft (v5 — post-review round 4)
**Classification:** Single ticket

---

## Problem

EM agents orchestrate multiple child agents across long sessions. Between dispatch and completion, they have no visibility into whether a dispatched agent is making progress, stuck, or deeply lost in a wrong direction. `sm what` costs tokens and is imprecise; `sm children` shows only `running`/`idle` — no progress signal. The EM ends up blind.

The fix: sm periodically nudges dispatched agents to self-report their status. If they don't respond, sm escalates to an urgent interrupt.

---

## Scope of Investigation

This spec covers the full design of the feature described in #188. The issue is well-specified; investigation focused on:

1. What mechanisms already exist (don't rebuild)
2. Where exactly each change goes
3. What's architecturally clean vs. what would accumulate tech debt

### Key Existing Infrastructure

**`sm remind` (one-shot, partially exists):**
`cmd_remind()` in `commands.py`, `schedule_reminder()` on server (`POST /scheduler/remind`), `_fire_reminder()` in `message_queue.py`. Fires a single urgent message after a fixed delay. **Not wired in main.py** — no subparser registered, so `sm remind` currently cannot be invoked. This ticket wires it and adds the periodic variant. `client.schedule_reminder()` also does not exist — must be added to `client.py`.

**`sm status` (existing, different purpose):**
`cmd_status()` shows system-wide session status (all sessions + lock). No args. The proposed `sm status "<text>"` (with positional arg) is unambiguous and can coexist.

**`Session.current_task`:**
Already persisted. Set by `sm task "<description>"`. Not wired to the remind timer. New fields `agent_status_text` / `agent_status_at` are added separately — `task` is what you were assigned; `status` is what you're actively doing now.

**`sm send` delivery path:**
`session_manager.py:667` — sequential send returns `QUEUED` immediately without waiting for actual delivery. Starting remind timer at send time would misfire before the agent has received the dispatched prompt. Timer must start only after confirmed delivery.

**`sm children` display:**
Currently shows name, status, last_activity. No progress signal. Enhancement: show self-reported status text.

**`scheduled_reminders` SQLite table:**
One-shot reminders (`task_type = 'reminder'`). Periodic remind state is more complex. Add a separate `remind_registrations` table.

---

## Design

### 1. Registration: `sm send --remind <seconds>`

```bash
sm send <id> "As engineer, implement #1668..." --urgent --remind 180
```

The `--remind 180` flag starts a periodic remind registration targeting `<id>`:

- **Soft threshold:** 180s from last status update → important reminder (non-interrupting, delivers at next turn boundary)
- **Hard threshold:** 300s from last status update → urgent reminder (interrupts)
- Gap is fixed: `hard = soft + hard_gap` where `hard_gap` defaults to 120s (configurable)

**Timer starts on delivery, not on send.** The message may be queued before delivery (agent busy). The remind config is passed directly into `queue_message` via a new `remind_on_delivery=(soft_threshold, hard_threshold)` parameter, stored on the `QueuedMessage` object. When the queue manager marks the message delivered (inside `_try_deliver_messages` or `_deliver_urgent`), it calls `self.register_periodic_remind(target_session_id, soft, hard)` directly. No server callback or message ID propagation is needed — delivery happens inside `MessageQueueManager`, which is the right owner of this logic.

**One-active-per-target policy.** If a target session already has an active remind registration and a new `sm send --remind` arrives, the old registration is cancelled and replaced. Prevents duplicate loops.

### 2. Reminder delivery

**Soft (important mode):**
```
[sm remind] Update your status: sm status "your current progress"
```
Delivered as `important` — waits for next idle (non-interrupting). Avoids disrupting deep work. Does not accumulate unboundedly: important messages deliver when agent finishes a turn; next cycle only fires after `soft_threshold` from last reset.

**Hard (urgent, fires if soft was ignored after `hard_gap` seconds):**
```
[sm remind] Status overdue. Run: sm status "your current progress"
```
Delivered as `urgent` — interrupts the agent via Escape.

Timer semantics:
- Both thresholds measure from `last_reset_at` (the time the agent last called `sm status`, or the time the dispatch was delivered if no status set yet)
- After the hard reminder fires, `last_reset_at` is reset to now — cycle restarts
- Soft uses `important` mode — no sequential backlog risk across cycles

### 3. Agent response: `sm status "<text>"`

Agent self-reports status with a text argument:

```bash
sm status "investigating root cause — found 2 call sites, testing fix"
```

**What this does:**
- Sets `session.agent_status_text` and `session.agent_status_at` on the session
- Resets `last_reset_at` on any active remind registration for this session (both soft and hard timers restart)
- Persists `agent_status_text` and `agent_status_at` to `sessions.json`

**Coexistence with existing `sm status`:**
- `sm status` (no args) → existing behavior (system status display)
- `sm status "<text>"` (positional arg) → self-report status

No conflict: argparse handles via `nargs='?'` on the text argument.

### 4. `sm children` display

Enhanced to show self-reported status:

```
sm-engineer (66a8c9ee) | running | "investigating root cause — found 2 call sites" (2m ago)
app-scout   (340d0709) | running | "writing spec, sending to reviewer next" (45s ago)
app-codex   (d638c897) | idle    | (no status)
```

Format: `<name> (<id>) | <status> | "<agent_status_text>" (<age>)` or `(no status)` if none set.

### 5. Stopping reminders

| Trigger | Effect |
|---------|--------|
| Agent calls `sm status "..."` | Resets timer — reminders continue |
| Agent goes idle (Stop hook) | Cancels registration for that session |
| `sm remind <id> --stop` | EM manually cancels remind for target |
| `sm clear <id>` | Cancels registration (context reset) |
| `sm kill <id>` | Cancels registration (both kill endpoints) |

### 6. `sm remind` command syntax

The one-shot `sm remind` and the new stop command are disambiguated by flag:

```bash
# One-shot self-reminder (existing behavior, now wired)
sm remind 300 "check on task progress"

# Stop periodic remind for a target session (new)
sm remind <session-id> --stop
```

Disambiguation rule: if `--stop` flag is present, first arg is always a session ID. If absent, first arg is always a delay in seconds. This is unambiguous even for all-digit hex session IDs (e.g. `12345678`).

### 7. Configuration

```yaml
remind:
  soft_threshold_seconds: 180   # Default soft threshold
  hard_gap_seconds: 120          # Gap from soft to hard (hard = soft + hard_gap)
```

Per-dispatch override: `--remind 120` sets soft=120s, hard=240s (120 + 120).

Formula for any soft value: `hard = soft + config.remind.hard_gap_seconds`

This matches the issue examples exactly:
- Default: soft=180, hard=300 (180+120)
- Override: `--remind 120` → soft=120, hard=240 (120+120)

---

## Implementation Approach

### Files changed

**`src/models.py`**
- Add `agent_status_text: Optional[str] = None` to `Session`
- Add `agent_status_at: Optional[datetime] = None` to `Session`
- Update `to_dict()` / `from_dict()` with None defaults for backward compat
- Add `remind_soft_threshold: Optional[int] = None` and `remind_hard_threshold: Optional[int] = None` to `QueuedMessage` (persisted to DB — survives crash+recovery like `notify_on_delivery` and `notify_on_stop`)
- Add `RemindRegistration` dataclass:
  ```python
  @dataclass
  class RemindRegistration:
      id: str
      target_session_id: str
      soft_threshold_seconds: int
      hard_threshold_seconds: int   # soft + hard_gap
      registered_at: datetime
      last_reset_at: datetime       # updated by sm status; initialized on delivery
      soft_fired: bool = False
      hard_fired: bool = False
      is_active: bool = True
  ```

**`src/message_queue.py`**
- New SQLite table `remind_registrations` (id, target_session_id, soft_threshold_seconds, hard_threshold_seconds, registered_at, last_reset_at, soft_fired, hard_fired, is_active)
- Add two nullable columns to `message_queue` table: `remind_soft_threshold INTEGER` and `remind_hard_threshold INTEGER`; add migration block (same pattern as `notify_on_stop` migration at lines 134-139)
- Update `queue_message` INSERT to write `remind_soft_threshold` and `remind_hard_threshold`
- Update `get_pending_messages` SELECT and `QueuedMessage` reconstruction to read these columns — ensures crash-recovered messages carry remind intent through to delivery
- Add `_remind_registrations: Dict[str, RemindRegistration]` keyed by target_session_id (one-active-per-target)
- Add `register_periodic_remind(target_session_id, soft_threshold, hard_threshold) -> str` — persists to DB, starts asyncio task, returns registration id; cancels any existing registration for that target first
- Add `reset_remind(target_session_id)` — updates `last_reset_at` + clears soft_fired/hard_fired in DB and in-memory
- Add `cancel_remind(target_session_id)` — sets `is_active=False`, cancels asyncio task
- Add `_run_remind_task(target_session_id)` async task — polls every 5s, fires important/urgent reminders when thresholds exceeded; resets cycle on hard fire; includes pending-remind dedup guard (see algorithm detail below)
- Modify `_try_deliver_messages` and `_deliver_urgent` — after marking a message delivered, check if `msg.remind_soft_threshold` is set and call `self.register_periodic_remind(target_session_id, soft, hard)` if so
- Modify `mark_session_idle(session_id, from_stop_hook)` — when `from_stop_hook=True`, call `cancel_remind(session_id)` (agent completed their task)
- Add `_recover_remind_registrations()` — on startup, reload active registrations from DB, restart tasks with adjusted remaining time

**`src/server.py`**
- New endpoint: `POST /sessions/{session_id}/remind` — start periodic remind (body: `soft_threshold`, `hard_threshold`)
- New endpoint: `DELETE /sessions/{session_id}/remind` — cancel remind
- New endpoint: `POST /sessions/{session_id}/agent-status` — agent self-reports status (body: `text`); updates session fields + calls `reset_remind`
- Modify `sm send` handler — accept `remind_soft_threshold: Optional[int]` and `remind_hard_threshold: Optional[int]` in request body; pass through to `session_manager.send_input` → `queue_message`; the queue manager persists them to DB and handles registration on delivery internally
- Update `clear_session` handler (server.py:1045) — after successful clear, call `queue_mgr.cancel_remind(session_id)`
- Update `kill_session` handler (server.py:1084, `DELETE /sessions/{id}`) — before kill, call `queue_mgr.cancel_remind(session_id)`
- Update `kill_session_with_check` handler (server.py:1993, `POST /sessions/{id}/kill`) — before kill, call `queue_mgr.cancel_remind(target_session_id)` (this is the endpoint `sm kill` uses)
- Update `list_children_sessions` to include `agent_status_text`, `agent_status_at` in response
- Update `SessionResponse` pydantic model to include `agent_status_text`, `agent_status_at`

**`src/cli/client.py`**
- Add `schedule_reminder(session_id, delay_seconds, message)` — calls `POST /scheduler/remind` (enables `cmd_remind` to function)
- Add `set_agent_status(session_id, text)` — calls `POST /sessions/{id}/agent-status`
- Add `register_remind(target_session_id, soft_threshold, hard_threshold)` — calls `POST /sessions/{id}/remind`
- Add `cancel_remind(target_session_id)` — calls `DELETE /sessions/{id}/remind`
- Modify `send_input(...)` — accept `remind_soft_threshold: Optional[int]` and `remind_hard_threshold: Optional[int]`, pass to server in request body

**`src/cli/commands.py`**
- Add `cmd_agent_status(client, session_id, text)` — calls `client.set_agent_status`
- Modify `cmd_send` — accept `remind_seconds: Optional[int]`, pass to API
- Add `cmd_remind_stop(client, session_id, target_id)` — calls `client.cancel_remind`

**`src/cli/main.py`**
- Add `--remind <seconds>` flag to `sm send` parser
- Extend `sm status` to accept optional positional `text` arg; dispatch to `cmd_status` (no args) or `cmd_agent_status` (with text)
- Wire `sm remind` properly: add `remind_parser` subparser with two modes (one-shot: `delay message`; stop: `session_id --stop`); dispatch to `cmd_remind` or `cmd_remind_stop`
- Add new commands to `no_session_needed` as appropriate

**`config.yaml`**
- Add `remind:` section with `soft_threshold_seconds: 180` and `hard_gap_seconds: 120`

### Key algorithmic detail: `_run_remind_task`

```python
async def _run_remind_task(self, target_session_id: str):
    CHECK_INTERVAL = 5  # seconds
    REMIND_PREFIX = "[sm remind]"
    while True:
        await asyncio.sleep(CHECK_INTERVAL)
        reg = self._remind_registrations.get(target_session_id)
        if not reg or not reg.is_active:
            return
        elapsed = (datetime.now() - reg.last_reset_at).total_seconds()
        if not reg.soft_fired and elapsed >= reg.soft_threshold_seconds:
            # Dedup guard: skip if a soft remind is already pending delivery
            pending = self.get_pending_messages(target_session_id)
            has_pending_remind = any(m.text.startswith(REMIND_PREFIX) for m in pending)
            if not has_pending_remind:
                self.queue_message(
                    target_session_id=reg.target_session_id,
                    text='[sm remind] Update your status: sm status "your current progress"',
                    delivery_mode="important",
                )
            reg.soft_fired = True
            self._update_remind_db(target_session_id, soft_fired=True)
        if not reg.hard_fired and elapsed >= reg.hard_threshold_seconds:
            self.queue_message(
                target_session_id=reg.target_session_id,
                text='[sm remind] Status overdue. Run: sm status "your current progress"',
                delivery_mode="urgent",
            )
            # Reset cycle — write final state atomically; never persist hard_fired=True
            reg.last_reset_at = datetime.now()
            reg.soft_fired = False
            reg.hard_fired = False
            self._update_remind_db(target_session_id,
                hard_fired=False, last_reset_at=reg.last_reset_at,
                soft_fired=False)
```

The dedup guard at soft-fire time ensures at most one pending soft remind in the queue across cycles. Hard reminders are urgent and deliver immediately, so no dedup is needed for them.

### Delivery-triggered registration (queue-internal, crash-safe)

When `sm send --remind 180` is called:

1. CLI passes `remind_soft_threshold=180, remind_hard_threshold=300` in the send request body
2. Server passes through to `session_manager.send_input` → `queue_message`
3. `queue_message` persists both values as columns on the `message_queue` row (same as `notify_on_delivery`, `notify_on_stop`)
4. `get_pending_messages` reads these columns on every recovery path — crash-recovered messages carry the remind intent
5. When `_try_deliver_messages` or `_deliver_urgent` marks the message delivered, it checks `msg.remind_soft_threshold` and calls `self.register_periodic_remind(target_session_id, soft, hard)` if set

The timer-start logic is entirely within `MessageQueueManager`. No server callback or message-ID propagation needed. Remind intent survives server restart.

---

## Crash Recovery

On server restart, `_recover_remind_registrations()` reads `remind_registrations` table where `is_active = 1`. For each:
- Reconstruct `RemindRegistration` from DB
- Compute remaining time before soft/hard fires (relative to `last_reset_at`)
- Restart `_run_remind_task` asyncio task

This mirrors how `_recover_scheduled_reminders()` works for one-shot reminders.

---

## Test Plan

1. **Delivery-triggered start:**
   - Agent is busy. `sm send <id> "prompt" --remind 10`. Assert no remind fires during delivery delay. Once delivered, assert soft fires ~10s later.

2. **Immediate-start when idle:**
   - Agent is idle. `sm send <id> "prompt" --remind 10`. Assert soft fires ~10s after send.

3. **Basic remind lifecycle:**
   - `sm send <id> "prompt" --remind 10` (hard_gap=10). Wait 10s without `sm status` → assert important reminder received. Wait 20s → assert urgent received and cycle resets.

4. **Status reset:**
   - `sm send <id> "prompt" --remind 10`. At 5s, agent calls `sm status "working on it"`. Assert no reminder at 10s (timer reset). Assert reminder fires at 5+10=15s.

5. **Idle cancels remind:**
   - `sm send <id> "prompt" --remind 10`. Stop hook fires → assert no reminder at 10s.

6. **Clear cancels remind:**
   - `sm send <id> "prompt" --remind 10`. `sm clear <id>` → assert no reminder fires.

7. **Kill cancels remind:**
   - `sm send <id> "prompt" --remind 10`. `sm kill <id>` → assert no reminder fires.

8. **Manual stop:**
   - `sm send <id> "prompt" --remind 10`. `sm remind <id> --stop` at 5s → assert no reminder at 10s.

9. **Replacement policy:**
   - `sm send <id> "prompt" --remind 10`. After 5s, `sm send <id> "prompt2" --remind 60`. Assert timer reset (no reminder at 10s), remind fires at 5+60=65s.

10. **`sm children` shows status:**
    - Agent calls `sm status "making progress"`. `sm children` output contains `"making progress"` with age.

11. **`sm status` no-arg unchanged:**
    - `sm status` with no text arg → system status display unchanged.

12. **Config override:**
    - `sm send <id> "..." --remind 120`. Assert soft fires at 120s, hard at 240s (120+120).

13. **`sm remind` disambiguation:**
    - `sm remind 60 "check in"` → one-shot scheduled (numeric delay)
    - `sm remind abc1def2 --stop` → cancel periodic remind for that session

14. **Crash recovery — active registration:**
    - Register remind (soft=30s), advance time 15s, restart server, assert soft fires ~15s after restart.

15. **Crash recovery — queued message with remind intent:**
    - Agent is busy. `sm send <id> "prompt" --remind 10` while agent mid-turn (message queued, not yet delivered). Restart server. Assert: after restart, message is delivered, and remind fires ~10s after delivery.

16. **One-shot `sm remind` now works:**
    - `sm remind 5 "hello"` → urgent message received in ~5s.

---

## Non-goals / Exclusions

- **`--remind` on `sm spawn`:** Not in scope. Spawn has `--wait` for completion. Remind is for long-running dispatches where completion is not predictable.
- **Telegram mirroring of reminders:** Not required for MVP.

---

## Ticket Classification

**Single ticket.** One engineer can implement this in one session without context overflow. The changes are mechanical and touch well-understood code paths. The spec is complete enough that no architectural decisions remain.

The asyncio task pattern is already established by `_fire_reminder` and `_watch_for_idle`. The delivery-triggered registration requires one new hook but builds on the existing `notify_on_delivery` flow.

**Prerequisite:** sm#183 (CLOSED ✓). Sequential vs urgent delivery now works correctly.
