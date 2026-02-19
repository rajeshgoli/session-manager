# sm#206: Context Monitor Opt-In Registration

## Problem

sm#203 (PR #205, merged) fires context monitoring for every Claude Code session with
`CLAUDE_SESSION_MANAGER_ID` set. The hooks (`context_monitor.sh`, `precompact_notify.sh`,
`session_clear_notify.sh`) POST to `/hooks/context-usage` on every assistant message and
compaction event regardless of whether the session is relevant or idle.

**Observed:** A stale scout agent (finished work, not cleared) fired two compaction
notifications to EM before being manually cleared.

**Root cause:** The `/hooks/context-usage` endpoint processes every session that holds a
`CLAUDE_SESSION_MANAGER_ID`. There is no registration gate.

---

## Investigation

### Current endpoint behavior (main branch `src/server.py:2184`)

```
POST /hooks/context-usage
  ├── if event == "compaction"
  │     → reset _context_warning_sent / _context_critical_sent
  │     → notify session.parent_session_id (hardcoded)
  ├── if event == "context_reset"
  │     → reset flags only
  └── else (context usage update)
        → session.tokens_used = total_input_tokens
        → if used_pct >= 65%  → queue urgent msg to session.id
        → if used_pct >= 50%  → queue sequential msg to session.id
```

**Two problems:**
1. No gate — all sessions are processed, including idle/stale ones.
2. Compaction notification hardcoded to `parent_session_id`. There's no way to route
   it to a different target (e.g., EM wanting to monitor a grandchild via a different path).

### Hook scripts (unchanged by this ticket)

All three hook scripts (`context_monitor.sh`, `precompact_notify.sh`,
`session_clear_notify.sh`) are already installed to `~/.claude/hooks/`. They POST to
`/hooks/context-usage` with a 0.5s max timeout (`>/dev/null 2>&1 &`). The POST is cheap;
server-side gating is the right place to suppress processing.

---

## Design

### Core model: two new Session fields

```python
# Context monitor registration (#206)
context_monitor_enabled: bool = False  # Default off; must opt-in
context_monitor_notify: Optional[str] = None  # Session ID to send warnings/crits to
```

`context_monitor_notify` stores the resolved target session ID:
- Self-registration → caller's own session ID
- Parent-registers-child → parent's session ID

This is a stored session ID, not a symbolic "self/parent" token, so no resolution is
needed at notification time.

### Registration paths

**1. Agent self-registers:**
```bash
sm context-monitor enable
# Sets: context_monitor_enabled=True, context_monitor_notify=<caller-session-id>
```
Warnings and compaction alerts are delivered to the agent itself.

**2. Parent registers child:**
```bash
sm context-monitor enable <child-id>
# Sets on child: context_monitor_enabled=True, context_monitor_notify=<caller-session-id>
```
All notifications for the child go to the parent. The child is unaware of monitoring.

**3. Disable:**
```bash
sm context-monitor disable [session-id]
# Sets: context_monitor_enabled=False, context_monitor_notify=None
```

---

## Implementation

### 1. Session model (`src/models.py`)

Add two fields after the crash recovery block:

```python
# Context monitor registration (#206)
context_monitor_enabled: bool = False  # Default off — opt-in only
context_monitor_notify: Optional[str] = None  # Session ID to receive alerts; None = off
```

Update `to_dict()`:
```python
"context_monitor_enabled": self.context_monitor_enabled,
"context_monitor_notify": self.context_monitor_notify,
```

Update `from_dict()`:
```python
context_monitor_enabled=data.get("context_monitor_enabled", False),
context_monitor_notify=data.get("context_monitor_notify"),
```

_Note: `_context_warning_sent` and `_context_critical_sent` remain runtime-only (not persisted). No change needed there._

---

### 2. Server changes (`src/server.py`)

#### 2a. New request model

```python
class ContextMonitorRequest(BaseModel):
    """Request to register/deregister context monitoring for a session (#206)."""
    enabled: bool
    notify_session_id: Optional[str] = None  # Session ID to notify; required when enabled=True
    requester_session_id: str  # Required — caller's session ID for ownership check
```

#### 2b. New registration endpoint

Ownership rules (matching `kill` and `handoff` patterns):
- Self-registration: `requester_session_id == session_id`
- Parent-registers-child: `target.parent_session_id == requester_session_id`

```python
@app.post("/sessions/{session_id}/context-monitor")
async def set_context_monitor(session_id: str, request: ContextMonitorRequest):
    """Enable or disable context monitoring for a session (#206)."""
    if not app.state.session_manager:
        raise HTTPException(status_code=503, detail="Session manager not configured")

    session = app.state.session_manager.get_session(session_id)
    if not session:
        raise HTTPException(status_code=404, detail="Session not found")

    if request.enabled and not request.notify_session_id:
        raise HTTPException(status_code=422, detail="notify_session_id required when enabling")

    # requester_session_id is a required field (str, not Optional).
    # Pydantic rejects requests missing it with 422 before this code runs.
    # Ownership check: requester must be self or the session's parent.
    is_self = (request.requester_session_id == session_id)
    is_parent = (session.parent_session_id == request.requester_session_id)
    if not is_self and not is_parent:
        raise HTTPException(
            status_code=403,
            detail="Cannot configure context monitor — not your session or child session",
        )

    # Validate notify target exists (prevents silent black-holing)
    if request.enabled and request.notify_session_id:
        notify_session = app.state.session_manager.get_session(request.notify_session_id)
        if not notify_session:
            raise HTTPException(
                status_code=422,
                detail=f"notify_session_id {request.notify_session_id!r} not found",
            )

    session.context_monitor_enabled = request.enabled
    session.context_monitor_notify = request.notify_session_id if request.enabled else None

    # Re-arm one-shot flags when enabling so warnings fire fresh in the new cycle.
    # If re-enabled after a period of being disabled (during which compaction may have
    # fired unobserved), stale flag state would suppress the first warning.
    if request.enabled:
        session._context_warning_sent = False
        session._context_critical_sent = False

    app.state.session_manager._save_state()
    return {"status": "ok", "enabled": session.context_monitor_enabled}
```

#### 2c. Gate in `/hooks/context-usage`

Add immediately after session lookup, before any event handling:

```python
# Gate: skip unregistered sessions (#206)
if not session.context_monitor_enabled:
    return {"status": "not_registered"}
```

#### 2d. Route all notifications through `context_monitor_notify`

**Compaction event** — replace hardcoded `parent_session_id`:
```python
# Before (#203):
if session.parent_session_id and queue_mgr:
    queue_mgr.queue_message(target_session_id=session.parent_session_id, ...)

# After (#206):
if session.context_monitor_notify and queue_mgr:
    queue_mgr.queue_message(target_session_id=session.context_monitor_notify, ...)
```

**Warning threshold** — replace hardcoded `session.id`:
```python
# Before:
queue_mgr.queue_message(target_session_id=session.id, ...)

# After:
queue_mgr.queue_message(target_session_id=session.context_monitor_notify, ...)
```

**Critical threshold** — same change as warning.

_Both warning and compaction events route to `context_monitor_notify`. When the agent
self-registers, that's `session.id` — same behavior as before. When a parent registers
a child, that's the parent's ID._

#### 2e. Status query endpoint

Required by `sm context-monitor status`:

```python
@app.get("/sessions/context-monitor")
async def get_context_monitor_status():
    """List sessions with context monitoring enabled (#206)."""
    if not app.state.session_manager:
        return {"monitored": []}
    monitored = [
        {
            "session_id": s.id,
            "friendly_name": s.friendly_name,
            "notify_session_id": s.context_monitor_notify,
        }
        for s in app.state.session_manager.sessions.values()
        if s.context_monitor_enabled
    ]
    return {"monitored": monitored}
```

_Route ordering note: `/sessions/context-monitor` must be registered BEFORE
`/sessions/{session_id}` to prevent "context-monitor" from being treated as a session ID._

---

### 3. CLI changes

#### 3a. Client method (`src/cli/client.py`)

```python
def set_context_monitor(
    self,
    session_id: str,
    enabled: bool,
    notify_session_id: Optional[str] = None,
    requester_session_id: str = "",  # Required by server; CLI always passes session_id
) -> tuple[Optional[dict], bool, bool]:
    """Enable or disable context monitoring for a session."""
    payload = {
        "enabled": enabled,
        "notify_session_id": notify_session_id,
        "requester_session_id": requester_session_id,
    }
    data, success, unavailable = self._request("POST", f"/sessions/{session_id}/context-monitor", data=payload)
    return data, success, unavailable

def get_context_monitor_status(self) -> Optional[list]:
    """Get list of sessions with context monitoring enabled."""
    data, success, _ = self._request("GET", "/sessions/context-monitor")
    if success and data:
        return data.get("monitored", [])
    return None
```

#### 3b. Command handler (`src/cli/commands.py`)

```python
def cmd_context_monitor(
    client: SessionManagerClient,
    session_id: Optional[str],
    action: str,
    target: Optional[str],
) -> int:
    """
    Enable, disable, or show status for context monitoring.

    Args:
        client: API client
        session_id: Caller's session ID (from CLAUDE_SESSION_MANAGER_ID)
        action: "enable", "disable", or "status"
        target: Optional target session ID; defaults to self when action is enable/disable
    """
    if action == "status":
        monitored = client.get_context_monitor_status()
        if monitored is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        if not monitored:
            print("No sessions currently registered for context monitoring.")
            return 0
        print(f"{'Session':<12} {'Name':<24} {'Notify Target'}")
        print("-" * 52)
        for entry in monitored:
            name = entry.get("friendly_name") or ""
            notify = entry.get("notify_session_id") or "(none)"
            print(f"{entry['session_id']:<12} {name:<24} {notify}")
        return 0

    if action in ("enable", "disable"):
        # enable/disable require being inside a managed session (need session_id as requester)
        if not session_id:
            print(
                "Error: sm context-monitor enable/disable requires a managed session "
                "(CLAUDE_SESSION_MANAGER_ID not set)",
                file=sys.stderr,
            )
            return 2

        # Determine target session
        resolved_target = target or session_id
        if not resolved_target:
            print("Error: No session ID — run inside a session or specify a target", file=sys.stderr)
            return 2

        enabled = (action == "enable")
        # notify_session_id: when enabling, notify the CALLER (self), not the target
        notify_session_id = session_id if enabled else None

        data, success, unavailable = client.set_context_monitor(
            resolved_target, enabled, notify_session_id, requester_session_id=session_id
        )
        if unavailable:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        if not success:
            err = (data or {}).get("detail", "Unknown error")
            print(f"Error: {err}", file=sys.stderr)
            return 1

        if enabled:
            if target and target != session_id:
                print(f"Context monitoring enabled for {target} — notifications → {session_id}")
            else:
                print(f"Context monitoring enabled — notifications → self ({session_id})")
        else:
            print(f"Context monitoring disabled for {resolved_target}")
        return 0

    print(f"Error: Unknown action '{action}'. Use: enable, disable, status", file=sys.stderr)
    return 1
```

#### 3c. Argument parser (`src/cli/main.py`)

Add subcommand in the argument parser section:

```python
# sm context-monitor <enable|disable|status> [session-id]
ctx_parser = subparsers.add_parser(
    "context-monitor",
    help="Manage context monitoring registration for a session",
)
ctx_parser.add_argument(
    "action",
    choices=["enable", "disable", "status"],
    help="enable: opt-in, disable: opt-out, status: list monitored sessions",
)
ctx_parser.add_argument(
    "target",
    nargs="?",
    default=None,
    help="Session ID to register/deregister; defaults to self",
)
```

Add `"context-monitor"` to `no_session_needed` (status needs no session; enable/disable
without target also need it, but gracefully error if missing).

Add dispatch:
```python
elif args.command == "context-monitor":
    sys.exit(commands.cmd_context_monitor(client, session_id, args.action, args.target))
```

---

### 4. EM persona update (`~/.agent-os/personas/em.md`)

Add to the Pre-Flight section (after `sm name`):

```markdown
sm context-monitor enable    # Register self for context monitoring
```

This one line ensures EM sessions always self-register. No startup script or hook
needed — it's a deliberate, visible step in EM's session start routine.

When EM spawns a long-running child (scout, engineer for a multi-hour task), EM should
also register the child:

```bash
sm context-monitor enable <child-id>   # EM receives child's compaction alerts
```

This is discretionary (EM's judgment call), so it belongs in the dispatching workflow,
not the pre-flight. Add a note in the Agent Management section.

---

## What Does NOT Change

- **Hook scripts** — no changes to `context_monitor.sh`, `precompact_notify.sh`,
  `session_clear_notify.sh`, or `post_compact_recovery.sh`. They continue to POST for
  every session; the server gate silently returns `{"status": "not_registered"}`.
- **`check_frequency` behavior** — the status line fires on every assistant message.
  For unregistered sessions, the response is a fast early-return with no queue access.
- **One-shot flag semantics** — `_context_warning_sent` / `_context_critical_sent` still
  reset on compaction and context_reset events. Additionally, both flags reset when
  `context_monitor_enabled` is set to `True` (re-arm on registration — see §2b).
- **`sm handoff` re-registration** — `sm handoff` already resets the one-shot flags
  (`_execute_handoff` on main). No need to touch handoff for this ticket.

---

## Test Plan

### Unit tests (`tests/unit/test_context_monitor.py`)

Extend the existing test file with a new test class:

**`TestRegistrationGate`**
1. `test_unregistered_session_returns_not_registered` — POST context-usage for session with `context_monitor_enabled=False`; expect `{"status": "not_registered"}`, no queue calls.
2. `test_registered_session_processes_normally` — POST context-usage for session with `context_monitor_enabled=True, context_monitor_notify=session.id`; expect warning queued.

**`TestNotificationRouting`**
3. `test_compaction_notifies_context_monitor_notify_not_parent` — session with `parent_session_id=X, context_monitor_notify=Y`; POST compaction event; expect queue to Y, not X.
4. `test_warning_routes_to_context_monitor_notify` — session with `context_monitor_notify=parent_id`; POST 55% usage; expect warning queued to `parent_id`, not `session.id`.
5. `test_critical_routes_to_context_monitor_notify` — same as above for critical.

**`TestRegistrationEndpoint`**
6. `test_enable_sets_fields` — POST `/sessions/{id}/context-monitor` with `{enabled: true, notify_session_id: "abc", requester_session_id: session_id}` → fields updated, `_save_state` called.
7. `test_disable_clears_fields` — disable → `context_monitor_enabled=False`, `context_monitor_notify=None`, `_save_state` called.
8. `test_enable_without_notify_session_id_returns_422` — missing `notify_session_id` returns 422.
9. `test_unknown_session_returns_404`.
10. `test_auth_rejects_missing_requester` — omit requester_session_id entirely → 422 (Pydantic required field).
11. `test_auth_rejects_unrelated_requester` — requester is neither self nor parent → 403.
12. `test_auth_allows_self` — requester == session_id → 200.
13. `test_auth_allows_parent` — requester == session.parent_session_id → 200.
14. `test_invalid_notify_session_id_returns_422` — notify_session_id doesn't exist → 422.
15. `test_enable_rearms_flags` — session has `_context_warning_sent=True`; POST enable → both flags reset to False.
16. `test_disable_reenable_reraises_warning` — register, trigger 55% warning (flag set), disable, re-enable, trigger 55% again → warning queued again (flags were re-armed on enable).

**`TestStatusEndpoint`**
17. `test_status_lists_registered_sessions` — two sessions, one registered; GET `/sessions/context-monitor` returns only the registered one.
18. `test_status_empty_when_none_registered`.

### Anti-regression

Verify existing test suite passes: `source venv/bin/activate && PYTHONPATH=. python -m pytest tests/ -v`

**Fixture update required:** The gate introduced in 2c means existing `test_context_monitor.py`
tests will fail out-of-the-box — their fixture sessions have `context_monitor_enabled=False`
(the new default) so the endpoint will return `not_registered` for all of them.

The engineer must update `_make_session()` in `test_context_monitor.py` to set:
```python
context_monitor_enabled=True
context_monitor_notify=session_id  # e.g. "abc12345"
```
After this fixture change, all 32 existing tests should pass without any logic changes.
The new tests added by this ticket cover the gate and registration behavior.

### Manual verification

1. Start an EM session, run `sm context-monitor enable`, then `sm context-monitor status` — verify self appears in list.
2. EM registers a child: `sm context-monitor enable <child-id>`. Trigger compaction in child. Verify EM receives notification, child does not.
3. Unregistered session: run a regular scout for several minutes. Confirm no compaction or context-usage messages reach EM.
4. `sm context-monitor disable` on EM — verify status shows empty. Trigger context update — no messages.

---

## Ticket Classification

**Single ticket.** All changes are in one cohesive layer (model → server → CLI → persona).
An engineer can complete this in one session:
- `src/models.py` — 2 new fields + to_dict/from_dict (5 min)
- `src/server.py` — gate + routing + 2 new endpoints (~60 lines)
- `src/cli/client.py` — 2 new methods (~20 lines)
- `src/cli/commands.py` — new `cmd_context_monitor` function (~50 lines)
- `src/cli/main.py` — new subcommand + dispatch (~15 lines)
- `~/.agent-os/personas/em.md` — 1 line pre-flight + 2 line note
- `tests/unit/test_context_monitor.py` — 18 new tests + fixture update
