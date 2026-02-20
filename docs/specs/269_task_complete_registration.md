# sm#269: task-complete registration — cancel remind noise after agent completion

**Issue:** https://github.com/rajeshgoli/session-manager/issues/269

---

## Problem

After an agent finishes its assigned task, `sm remind` keeps firing at 210s/420s intervals with stale status messages. EM receives noisy remind notifications long after the agent has nothing new to report.

**Root cause (observed, not speculated):** `register_periodic_remind` starts an async loop (`_run_remind_task`) that runs until one of four cancellation events fires: Stop hook, `sm clear`, `sm kill`, or `sm remind --stop`. For Claude sessions, each Stop hook calls `mark_session_idle(from_stop_hook=True)` → `cancel_remind`, so the remind is cancelled automatically after each turn — including the final one. The persistent-loop problem is specific to **codex sessions**, which have no Stop hooks, so `cancel_remind` is never invoked and the loop runs indefinitely until `sm clear` or `sm kill`. The second gap applies to all session types: Stop-hook cancellation is **silent** — the EM receives no notification that the agent finished. `sm task-complete` addresses both: it cancels the loop explicitly for codex (and missed-hook edge cases) and sends the EM a positive completion signal for all session types.

**Observed pattern:** reviewer-240 fired 3–4 times post-completion because it was idle (waiting for EM to clear it) and the remind cycle kept resetting.

---

## Solution: `sm task-complete`

A new self-directed command an agent calls when its task is done. It cancels the remind + parent wake loop and sends a one-time notification to the EM.

### What it does

1. **Cancels remind** for the calling session — no more 210s/420s fires
2. **Cancels parent wake** for the calling session — stops periodic EM digest
3. **Notifies EM** — queues an important message to the dispatching EM so EM knows this agent is free

### Usage

```bash
sm task-complete        # call when your task is fully done
```

No arguments. Self-directed only — caller must be the session itself (enforced by server via `requester_session_id` check, same as `sm handoff`).

---

## EM Lookup

The server needs to find the EM to notify. Precedence:

1. Query `parent_wake_registrations` for `child_session_id=session_id, is_active=1` → use `parent_session_id`
2. Fallback: `session.parent_session_id` (set at spawn time; may differ from dispatch EM in grandchild scenarios)
3. If still `None`: skip EM notification, log warning, still cancel remind/parent-wake

`parent_wake_registrations` is the authoritative source for EM because it's populated by `sm dispatch` (via `parent_session_id` on the queued message), which is the operation that started the remind loop in the first place.

---

## EM Notification Message

Queued to EM via `important` delivery mode:

```
[sm task-complete] agent <session_id>(<friendly_name>) completed its task. Clear context with: sm clear <session_id>
```

This mirrors the phrasing in the issue: *"agent <id> completed its task, clear context and re-use as needed."*

---

## Remind Message Update

The remind message text currently says:

```
[sm remind] Update your status: sm status "your current progress"
[sm remind] Status overdue. Run: sm status "your current progress"
```

Update to include the task-complete hint:

```
[sm remind] Update your status: sm status "message" — or if done: sm task-complete
[sm remind] Status overdue. Run: sm status "message" — or if done: sm task-complete
```

This teaches agents the escape hatch without requiring EM to train each agent separately.

---

## Edge Case: Scout + Reviewer Loop

The scout writes a spec while the reviewer reviews it. The loop may take many remind cycles.

**Resolution:** `sm task-complete` is a **voluntary, explicit action**. The scout must not call it until the spec is converged and all review rounds are complete. During the loop, the scout calls `sm status "reviewing spec with <reviewer-id>"` to reset the remind timer without signalling completion.

The remind message update (above) should not confuse the scout because "if done" is an explicit qualifier. The scout knows it's not done while iterating with the reviewer.

No code change is needed to handle this edge case — it's a protocol concern, not a system concern.

---

## Auto-Cancel Alternative

The issue proposes: *"auto-cancel remind when agent status hasn't changed across 2+ remind fires."*

**Not recommended.** Reasons:

1. **Brittle**: An agent that is working but not calling `sm status` (e.g., running a long test) would have its remind silently cancelled even though it's genuinely still active.
2. **Requires state expansion**: `RemindRegistration` model needs a `last_status_at_at_remind_fire` field; the remind loop needs to compare it across fires — added complexity for weaker semantics.
3. **Silent failure**: EM receives no notification. The pool self-manages only if the agent explicitly calls task-complete, not if the remind silently stops.

The explicit `sm task-complete` approach is unambiguous, teachable, and gives EM a positive signal.

---

## Implementation Plan

### 1. Update remind message text
**File:** `src/message_queue.py`, `_run_remind_task` method

- Soft: `'[sm remind] Update your status: sm status "message" — or if done: sm task-complete'`
- Hard: `'[sm remind] Status overdue. Run: sm status "message" — or if done: sm task-complete'`

Also update the dedup guard string prefix check — it uses `startswith("[sm remind]")` which stays the same.

### 2. Add server endpoint
**File:** `src/server.py`

```
POST /sessions/{session_id}/task-complete
Body: { "requester_session_id": "<self>" }
```

Logic:
1. Verify `requester_session_id == session_id` (self-auth)
2. Verify session exists
3. Get `queue_mgr`
4. **Resolve EM first** (before any cancel): query `parent_wake_registrations WHERE child_session_id=session_id AND is_active=1` → stash `em_id`; fallback to `app.state.session_manager.get_session(session_id).parent_session_id`
5. Call `queue_mgr.cancel_remind(session_id)`
6. Call `queue_mgr.cancel_parent_wake(session_id)`
7. If `em_id` found: `queue_mgr.queue_message(target_session_id=em_id, text="[sm task-complete] ...", delivery_mode="important")`
8. Return `{"status": "completed", "session_id": ..., "em_notified": bool}`

### 3. Add CLI command
**File:** `src/cli/commands.py`

```python
def cmd_task_complete(client: SessionManagerClient, session_id: str) -> int:
```

- Calls `client.task_complete(session_id)` → receives `(success, unavailable, em_notified)`
- Prints on success with EM notified: `Task complete. Remind cancelled. EM notified.`
- Prints on success without EM: `Task complete. Remind cancelled. (No EM registered — no notification sent.)`
- Prints on unavailable: `Error: Session manager unavailable`

**File:** `src/cli/client.py`

```python
def task_complete(self, session_id: str) -> tuple[bool, bool, bool]:
    """Call POST /sessions/{session_id}/task-complete. Returns (success, unavailable, em_notified)."""
```

**File:** `src/cli/main.py`

- Add `task-complete` subcommand (no args)
- Requires `CLAUDE_SESSION_MANAGER_ID` env var
- Routes to `commands.cmd_task_complete(client, session_id)`

### 4. Server-side EM lookup helper
Inside the endpoint handler, a small helper (inline is fine). Must be called **before** `cancel_remind` / `cancel_parent_wake` because `cancel_parent_wake` deactivates the DB row that this query reads:

```python
def _get_em_for_session(queue_mgr, session_manager, session_id: str) -> Optional[str]:
    # 1. Check parent_wake_registrations (most authoritative)
    #    Must be queried before cancel_parent_wake deactivates the row.
    rows = queue_mgr._execute_query(
        "SELECT parent_session_id FROM parent_wake_registrations "
        "WHERE child_session_id = ? AND is_active = 1 LIMIT 1",
        (session_id,)
    )
    if rows:
        return rows[0][0]
    # 2. Fallback: session.parent_session_id (set at spawn time)
    session = session_manager.get_session(session_id)
    return session.parent_session_id if session else None
```

---

## Test Plan

**File:** `tests/unit/test_task_complete.py`

1. `test_task_complete_cancels_remind` — cancel_remind called after task-complete endpoint
2. `test_task_complete_cancels_parent_wake` — cancel_parent_wake called after task-complete endpoint
3. `test_task_complete_notifies_em_via_parent_wake` — EM from parent_wake_registrations receives important message
4. `test_task_complete_falls_back_to_session_parent` — when no parent_wake_registration, uses session.parent_session_id
5. `test_task_complete_no_em_no_error` — when no EM found, endpoint returns success with em_notified=false (no crash)
6. `test_task_complete_self_auth_enforced` — requester_session_id ≠ session_id returns error
7. `test_remind_message_includes_task_complete_hint` — soft and hard remind messages contain "sm task-complete"
8. `test_cli_task_complete_requires_session_id` — error when CLAUDE_SESSION_MANAGER_ID not set

**Manual smoke test:**
1. `sm dispatch engineer-1 engineer --issue 123`
2. Let 210s elapse, observe remind with new text
3. In engineer-1: `sm task-complete`
4. Verify no further reminds fire
5. Verify EM console receives `[sm task-complete]` message

---

## Ticket Classification

**Single ticket.** One agent can implement all four file changes + tests in one session without context overflow. No sub-ticket breakdown needed.
