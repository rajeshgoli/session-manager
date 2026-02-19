# sm#212: Context monitor — child notifications are ambiguous

**Issue:** https://github.com/rajeshgoli/session-manager/issues/212
**Type:** Single ticket (small, focused fix in one location)

---

## Problem

When an EM registers to monitor a child's context (`sm context-monitor enable <child-id>`), and the child hits a threshold, the notification sent to the EM uses the same text as the EM's own context alert:

```
[sm context] Context at 67% — critically high. Write your handoff doc NOW and run `sm handoff <path>`. Compaction is imminent.
```

This is indistinguishable from a self-alert. The EM cannot tell which agent hit the threshold, and the imperative instruction ("Write your handoff doc NOW") is directed at the wrong recipient — the EM can't write the child's handoff doc.

---

## Root Cause

In `src/server.py`, the `POST /hooks/context-usage` handler builds notification messages at two points:

- **Critical** (line 2432–2435): generic "Context at X% — critically high. Write your handoff doc NOW..."
- **Warning** (line 2447–2449): generic "Context at X% (N tokens). Consider writing a handoff doc..."

Both always send to `session.context_monitor_notify` — which, for child-monitored sessions, is the EM's session ID. The message text is identical regardless of whether the recipient IS the monitored session (self-alert) or a parent observing a child (child-forwarded alert).

The compaction notification at line 2397–2399 already works correctly — it uses `session.friendly_name or session_id` to identify the child in its message. The threshold notifications need the same treatment.

---

## Distinguishing self-alerts from child-forwarded alerts

A self-alert is when `session.context_monitor_notify == session.id`.
A child-forwarded alert is when `session.context_monitor_notify != session.id`.

This is deterministic from existing session state — no new fields or API changes needed.

---

## Proposed Fix

In `src/server.py`, for both threshold branches, compute `is_self_alert` and branch on it.

**Critical threshold** (currently lines 2432–2440):

```python
is_self_alert = (session.context_monitor_notify == session.id)
if is_self_alert:
    msg = (
        f"[sm context] Context at {used_pct}% — critically high. "
        "Write your handoff doc NOW and run `sm handoff <path>`. "
        "Compaction is imminent."
    )
else:
    child_label = session.friendly_name or session.id
    msg = (
        f"[sm context] Child {child_label} ({session.id}) context at {used_pct}% — critically high. "
        "Compaction is imminent."
    )
```

**Warning threshold** (currently lines 2447–2454):

```python
is_self_alert = (session.context_monitor_notify == session.id)
if is_self_alert:
    total = data.get("total_input_tokens", 0)
    msg = (
        f"[sm context] Context at {used_pct}% ({total:,} tokens). "
        "Consider writing a handoff doc and running `sm handoff <path>`."
    )
else:
    child_label = session.friendly_name or session.id
    msg = f"[sm context] Child {child_label} ({session.id}) context at {used_pct}%."
```

No runtime, API, or model files outside `src/server.py` need to change. The only other file touched is the existing test file (`tests/unit/test_context_monitor.py`).

---

## Format rationale

- Child messages use `Child <name> (<id>)` — consistent with the compaction notification format already in use.
- Child critical omits the handoff instruction; the EM should decide what to do (typically `sm send <child-id>` to instruct the child to write its own handoff).
- Child warning is intentionally terse — just a heads-up, no actionable instruction since the EM can't take the action.
- `is_self_alert` is computed inside each `if not session._context_*_sent` block to keep the change localized.

---

## Test Plan

Add a new `TestChildForwardedNotifications` class in `tests/unit/test_context_monitor.py`:

1. **Child critical message contains friendly name when set** — set `session.friendly_name = "engineer-abc"` and `session.context_monitor_notify = "parent-id"`, post at 70%, assert `"Child"` in text and `"engineer-abc"` in text.
2. **Child critical message falls back to session id when friendly_name is None** — leave `session.friendly_name = None`, `session.context_monitor_notify = "parent-id"`, post at 70%, assert `"Child"` in text and `session.id` in text.
3. **Child critical message omits handoff instruction** — `session.friendly_name = None`, `session.context_monitor_notify = "parent-id"`, post at 70%, assert `"Write your handoff doc"` NOT in text.
4. **Child warning message contains session label** — `session.friendly_name = None`, `session.context_monitor_notify = "parent-id"`, post at 55%, assert `"Child"` in text and `session.id` in text.
5. **Child warning message omits handoff suggestion** — same setup, assert `"Consider writing a handoff"` NOT in text.
6. **Self critical message includes handoff instruction** — `session.context_monitor_notify = session.id`, assert `"Write your handoff doc"` in text and `"Child"` NOT in text.
7. **Self warning message includes handoff suggestion** — same, assert `"Consider writing a handoff"` in text and `"Child"` NOT in text.

Existing tests in `TestNotificationRouting` (`test_warning_routes_to_context_monitor_notify`, `test_critical_routes_to_context_monitor_notify`) use `session.context_monitor_notify = "parent-id"` — they only assert routing, not message content, so they continue to pass unchanged.

The existing `TestWarningThreshold` and `TestCriticalThreshold` fixtures use `context_monitor_notify = session_id` (self-alert path) — they assert `"[sm context]"` and `"50%"` / `"critically high"` in text, which both remain true. Those tests continue to pass unchanged.

---

## Ticket classification

**Single ticket.** One location in one file (`src/server.py`), plus new tests in the existing test file. An engineer can complete this without compacting context.
