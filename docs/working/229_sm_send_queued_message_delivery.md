# sm#229 — `sm send` Queued Message Not Delivered When Recipient Becomes Idle

**Ticket:** rajeshgoli/session-manager#229
**Status:** Spec ready for implementation

---

## Observed Behavior

When `sm send` is called while the recipient is active, the message is queued. After the recipient finishes its work and shows the `>` prompt, the message is either not delivered immediately or delivered with a multi-minute delay. The sender observes the recipient as "idle" but no injection occurs in the expected timeframe.

Observed twice in em-session8 (c3bbc6b9):

1. **10:46:18** — architect-pr228 (cc4d95a7) sent `"pr review: blocked — PR #228..."` to EM. EM was actively reading PR comments. Message delivered at **10:49:42** — **3 min 24 sec delay**. EM manually checked GitHub before the message arrived.

2. **11:17:27** — context monitor queued sequential message to EM. EM was dispatching agents. Message delivered at **11:20:40** — **3 min 13 sec delay**.

Both messages were *eventually* delivered. No permanent non-delivery was observed. The delay matched the duration of the recipient's active turn.

---

## Root Cause Analysis

### RCA #1 — Primary: Stop hook is the primary delivery trigger for active Claude/tmux sessions

`mark_session_idle()` has multiple callers, but for the specific bug scenario (Claude/tmux session, recipient active mid-turn), the **Stop hook via `server.py:1507`** is the only path that fires:

```python
# server.py:1507
queue_mgr.mark_session_idle(session_manager_id, last_output=last_message, from_stop_hook=True)
```

Other callers exist but do not apply to this scenario:
- `message_queue.py:551` — only fires if `session.status == IDLE` at queuing time; mid-turn sessions have status `RUNNING`
- `message_queue.py:527` and `session_manager.py:879,959` — codex and codex-app providers only
- `message_queue.py:276` — startup recovery only
- `main.py:327` — only when an explicit interrupt was requested
- `session_manager.py:1338,1235,1253,1012` — error rollback and codex paths only

The Stop hook fires at the **end of a complete response turn** — after all tool calls finish and Claude's text response is complete. It does **not** fire between individual tool calls within a turn.

**Effect:** If the recipient is in the middle of a long multi-tool-call turn (e.g., `gh pr view`, parse, `gh api`, format — all in one response), the sequential message waits the **entire turn duration** before delivery. For complex tasks this is 3–10+ minutes.

**Code path:**
```
queue_message(delivery_mode="sequential")
  → recipient is RUNNING → state.is_idle=False → no delivery trigger queued
  → [long turn: multiple tool calls, minutes pass]
  → Stop hook fires → mark_session_idle(from_stop_hook=True)
  → is_idle=True → asyncio.create_task(_try_deliver_messages())
  → message injected
```

### RCA #2 — Secondary: 2–10 second gap between `>` prompt and message injection

The Stop hook script (`~/.claude/hooks/notify_server.sh`) runs the HTTP POST in a **background subshell** and exits 0 immediately:

```bash
(
  curl -s --max-time 5 --connect-timeout 2 -X POST http://localhost:8420/hooks/claude \
    -H "Content-Type: application/json" -d "$INPUT" 2>&1
) </dev/null >/dev/null 2>&1 &
disown 2>/dev/null
exit 0
```

Claude Code sees exit 0 and shows `>` prompt. The HTTP POST is still in-flight. Full pipeline from `>` appearing to injection:

```
Hook script exits 0 → Claude shows `>` prompt       ← visible; session looks idle
  [background] curl POSTs to server
  → server reads transcript (asyncio.to_thread)       ← up to 300ms for stale retry (#184)
  → mark_session_idle() → asyncio.create_task(T1)
  → event loop runs T1 → _try_deliver_messages()
  → _deliver_direct() → tmux.send_input_async()     ← message injected
```

Window: 2–10+ seconds of apparent idle before injection. For pure autonomous workflows this is harmless (agent cannot type at `>` unprompted), but it confuses human observers and could cause issues if an urgent message arrives and sets `state.is_idle=False` during this window.

### RCA #3 — Tertiary: No fallback delivery for sessions with stuck pending messages

The monitor loop runs every 5 seconds but only calls `_check_stale_input`, which skips sessions that are not already marked idle:

```python
# message_queue.py:749
async def _check_stale_input(self, session_id: str):
    state = self._get_or_create_state(session_id)
    if not state.is_idle:
        return  # ← skips entirely when session is running
    # Only handles stale user-typed input, not stuck pending messages
```

If the Stop hook curl fails silently (transient network error, server busy), there is **no automatic retry**. The message stays stuck until the next natural Stop hook. There is no watchdog that detects "session has pending messages AND is now at idle prompt" and triggers delivery.

Contrast: `sm wait`'s `_watch_for_idle` (Phase 2) uses tmux prompt detection as a fallback for idle detection. No analogous mechanism exists for sequential message delivery.

---

## Evidence

**Database trace** (`~/.local/share/claude-sessions/message_queue.db`):

```
# Incident 1 — all three messages delivered in the same batch at 10:49:42:
10:46:18 → 10:49:42  sequential  architect-pr228  "pr review: blocked — PR #228..."
10:46:30 → 10:49:42  important   system           "[sm wait] architect-pr228 is now idle"
10:49:26 → 10:49:42  important   system           "[sm wait] engineer-sm224 is now idle"
```

Batch delivery at `10:49:42` = Stop hook fired at ~10:49:40. EM was in a **continuous 3m24s turn** from 10:46 to 10:49:40. No Stop hook fired during this window.

**Hook log** (`/tmp/claude-hooks.log`): All Stop hooks show `session_manager_id: c3bbc6b9` and server responds `{"status":"received","hook_event":"Stop"}`. The hook delivery mechanism is functioning correctly; delay is due to the turn duration, not hook failure.

---

## Proposed Solution

### Option A: Targeted delivery fallback in monitor loop (Recommended)

Extend `_monitor_loop` to detect sessions with pending sequential messages where the tmux prompt is visible but `state.is_idle=False`. Rather than calling `mark_session_idle()` (which carries stop-notify side effects), the fallback should **directly set idle state and call `_try_deliver_messages()`**:

```python
# In _monitor_loop (after _check_stale_input):
for session_id in sessions_with_pending:
    await self._check_stale_input(session_id)
    await self._check_stuck_delivery(session_id)   # NEW

async def _check_stuck_delivery(self, session_id: str):
    """
    Fallback: detect sessions at the > prompt with pending messages
    but state.is_idle=False (Stop hook curl not yet processed).

    Skips: codex-app sessions (no tmux), already-idle sessions.
    Requires: 2 consecutive detections to avoid false positives mid-turn.
    """
    state = self.delivery_states.get(session_id)
    if state and state.is_idle:
        return  # Already handled; _try_deliver_messages already triggered

    session = self.session_manager.get_session(session_id)
    if not session:
        return
    # Skip providers with no tmux pane
    provider = getattr(session, "provider", "claude")
    if provider not in ("claude", "codex"):
        return
    if not session.tmux_session:
        return

    if await self._check_idle_prompt(session.tmux_session):
        state = self._get_or_create_state(session_id)
        state._stuck_delivery_count = getattr(state, '_stuck_delivery_count', 0) + 1
        if state._stuck_delivery_count >= 2:
            state._stuck_delivery_count = 0
            # Set idle and trigger delivery directly — do NOT call mark_session_idle()
            # to avoid false stop-notify side effects (no from_stop_hook context).
            state.is_idle = True
            state.last_idle_at = datetime.now()
            logger.info(f"Stuck delivery fallback: {session_id} at prompt with pending messages")
            asyncio.create_task(self._try_deliver_messages(session_id))
    else:
        state = self.delivery_states.get(session_id)
        if state:
            state._stuck_delivery_count = 0
```

**Key design decision:** Call `_try_deliver_messages()` directly instead of `mark_session_idle()`. This avoids:
- Stop notification logic (`message_queue.py:367-377`) — no false stop notifications
- cancel_remind, handoff check, skip_count logic — none applicable to fallback delivery

**Provider scoping:** Skip `codex-app` (no tmux) and any session without `tmux_session`. Only `claude` and `codex` CLI sessions use tmux.

**False positive protection:** Require 2 consecutive poll-interval-apart prompt detections (~10 seconds) before triggering. This prevents mid-turn false positives where the terminal briefly shows `>` during output rendering.

**Polling overhead:** `_check_idle_prompt` does one `tmux capture-pane`. Only called for sessions with *pending* messages (already gated by `_get_sessions_with_pending()`). Overhead bounded by number of sessions with undelivered messages.

### Option B: Retry on delivery failure

If `_deliver_direct` returns False, the delivery failure is already logged. Add a failure timestamp to delivery state. Monitor loop checks for sessions with recent failures and retries. Addresses RCA #3 only; does not close RCA #2.

**Recommendation: Option A.** Closes RCA #2 and RCA #3, reuses existing `_check_idle_prompt()`, bounded overhead.

---

## Implementation Approach

1. **`models.py`:** Add `_stuck_delivery_count: int = 0` to `SessionDeliveryState`

2. **`message_queue.py`:**
   - Add `_check_stuck_delivery(session_id: str)` method
   - Call from `_monitor_loop` inner loop (after `_check_stale_input`)
   - Reuse `_check_idle_prompt()` for tmux detection

3. **No changes to `server.py`, `session_manager.py`, or hook scripts**

---

## Test Plan

### Core delivery tests

1. **Unit test — stuck delivery fires on 2nd prompt detection:**
   Queue sequential message. Set `state.is_idle=False`. Mock `_check_idle_prompt()` to return True for 2 consecutive calls. Run `_check_stuck_delivery()` twice. Verify `_try_deliver_messages()` called and message delivered.

2. **Unit test — first prompt detection does not deliver:**
   Same setup, only one iteration. Verify delivery not triggered (requires 2 consecutive).

3. **Unit test — mid-turn false positive blocked:**
   Queue message. `is_idle=False`. Mock `_check_idle_prompt()`: True then False (prompt appears briefly then disappears). Verify delivery NOT triggered.

### Side-effect tests (reviewer required)

4. **Stop-notify isolation:** Set `state.stop_notify_sender_id` on recipient. Trigger fallback delivery via `_check_stuck_delivery()`. Verify `_send_stop_notification()` is NOT called (fallback does not touch stop-notify logic).

5. **Race test — concurrent Stop hook + fallback:** Simulate Stop hook and fallback both firing in the same window. Stop hook calls `mark_session_idle()` (sets `is_idle=True`, creates delivery task T1). Fallback also fires, sets `is_idle=True`, creates T2. Verify: per-session delivery lock prevents double-delivery; message delivered exactly once.

### Regression tests (#174 invariants)

6. **skip_count regression:** Set `stop_notify_skip_count=1`. Trigger fallback delivery. Verify skip_count is NOT decremented (fallback path does not touch skip_count, unlike `mark_session_idle(from_stop_hook=True)`).

### Provider scope tests

7. **codex-app excluded:** Session with `provider="codex-app"` and pending messages. Mock `_check_idle_prompt()`. Verify `_check_stuck_delivery()` returns early without calling `_check_idle_prompt()`.

8. **claude provider included:** Session with `provider="claude"`, `tmux_session` set. Verify `_check_stuck_delivery()` calls `_check_idle_prompt()` and proceeds normally.

9. **no tmux_session guard:** Session with `provider="claude"` but `tmux_session=None`. Verify `_check_stuck_delivery()` returns early.

---

## Missed Failure Modes (Addressed)

- **False stop notifications from fallback:** Mitigated by calling `_try_deliver_messages()` directly instead of `mark_session_idle()`.
- **Cross-provider prompt mismatch:** Mitigated by provider filter (`provider not in ("claude", "codex")`).
- **Polling overhead with many sessions:** Bounded — only sessions with pending messages are checked (`_get_sessions_with_pending()`). Each check is one tmux subprocess.
- **Double delivery via race:** Mitigated by existing per-session `asyncio.Lock()` in `_try_deliver_messages()`.

---

## Classification

**Single ticket.** Changes confined to `models.py` (one field) and `message_queue.py` (one new method + one line in monitor loop). One engineer can complete without context compaction.
