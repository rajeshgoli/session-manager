# sm#256: Directional notify-on-stop

## Problem

When an agent (e.g. engineer) sends EM a completion notice via `sm send`, the engineer gets enrolled for notify-on-stop. When EM then stops between orchestration steps, the engineer receives an "[sm] em stopped" notification — pure noise they don't need and can't act on.

The current auto-enrollment is symmetric: any `sm send` call sets `notify_on_stop=True` for the sender regardless of direction.

## Root Cause

`notify_on_stop` defaults to `True` in `cmd_send()` (`src/cli/commands.py:873`) and is passed unconditionally through the entire delivery chain:

```
cmd_send()                   [notify_on_stop=True by default]
  → session_manager.send_input()
    → message_queue_manager.queue_message()
      → on delivery: state.stop_notify_sender_id = msg.sender_session_id
        → on Stop hook: sends "[sm] EM stopped" back to sender
```

There is no check for whether the sender is the EM. Any agent sending to any other agent gets enrolled for stop notification of the recipient.

## Current Code Flow

### Where `notify_on_stop` is set
- `src/cli/main.py:433` — `notify_on_stop = not getattr(args, 'no_notify_on_stop', False)` (default `True`)
- `src/cli/commands.py:873` — `cmd_send(... notify_on_stop: bool = True ...)` (default `True`)

### Where sender is enrolled for stop notification
- `src/message_queue.py:981-987` — on sequential/important delivery: if `msg.notify_on_stop and msg.sender_session_id`, sets `state.stop_notify_sender_id` — no sender existence check
- `src/message_queue.py:1100-1102` — codex-app path (same pattern, same gap)
- `src/message_queue.py:1035-1037` — urgent delivery path (same pattern)

### Where stop notification fires
- `src/message_queue.py:1204` — `queue_message(target_session_id=sender_session_id, ...)`: if sender session doesn't exist, delivery at `src/message_queue.py:906` warns and returns; harmless but wasteful

### EM identity gap
`sm em` (#233) sets the session's `friendly_name` to `"em"` or `"em-<suffix>"` but does **not** register any server-side EM flag. There is no `is_em` field on `Session`, no global registry. A reliable EM check is not currently possible.

## Proposed Solution

Add an `is_em: bool` flag to the `Session` model. When `sm em` runs, it sets this flag server-side (via the existing `PATCH /sessions/{id}` endpoint). In `session_manager.send_input()`, check `sender_session.is_em`: if `False` — or if the sender session cannot be found — override `notify_on_stop=False`.

**Rule**: only EM→agent sends (where sender has `is_em=True`) arm stop notification enrollment. All other sends — including unknown senders — do not.

The guard is **fail-closed**: unknown sender = non-EM = suppress. This ensures no unverified session can arm stop notifications via an unknown `sender_session_id`.

## Implementation Approach

### 1. `src/models.py` — Add `is_em` field to `Session`

```python
# After context_monitor_notify field (~line 218):
is_em: bool = False  # Set by sm em; gates directional notify-on-stop (#256)
```

Add to `to_dict()`:
```python
"is_em": self.is_em,
```

Add to `from_dict()` (with backward-compat default):
```python
is_em=data.get("is_em", False),
```

### 2. `src/server.py` — Add `is_em` to `SessionResponse` and `PATCH /sessions/{id}`

**`SessionResponse`** (line 78): add field so it appears in all session API responses:
```python
is_em: bool = False
```

**`update_session()` handler** (~line 895): extend signature and handling:
```python
async def update_session(
    session_id: str,
    friendly_name: Optional[str] = Body(None, embed=True),
    is_em: Optional[bool] = Body(None, embed=True),
):
```

Add handling block after the `friendly_name` block:
```python
if is_em is not None:
    session.is_em = is_em
    app.state.session_manager._save_state()
```

**All `SessionResponse(...)` instantiation sites** — pass through `is_em=session.is_em` (or `is_em=s.is_em` for list responses). Sites: `src/server.py:887`, `src/server.py:924`, `src/server.py:832` (list loop), and any other handler returning `SessionResponse`.

### 3. `src/cli/client.py` — Add `set_em_role()` method

```python
def set_em_role(self, session_id: str) -> tuple[bool, bool]:
    """
    Mark session as EM role (sets is_em=True server-side).

    Returns:
        Tuple of (success, unavailable)
    """
    data, success, unavailable = self._request(
        "PATCH",
        f"/sessions/{session_id}",
        {"is_em": True}
    )
    return success, unavailable
```

### 4. `src/cli/commands.py` — Call `set_em_role()` from `cmd_em()`

In `cmd_em()`, after the name-set step (Step 1), add:

```python
# Step 1b: Register EM role server-side
success, unavailable = client.set_em_role(session_id)
if unavailable:
    print("Error: Session manager unavailable", file=sys.stderr)
    return 2
if success:
    results.append("  EM role: registered")
else:
    results.append("  Warning: Failed to register EM role")
```

Error handling matches Step 1 pattern: unavailable → exit 2, API failure → warn and continue.

### 5. `src/session_manager.py` — Directional guard in `send_input()`

After resolving `sender_name` (~line 636), add:

```python
# Directional notify-on-stop (#256): only EM→agent sends should enroll recipient.
# Fail-closed: unknown sender treated as non-EM.
if notify_on_stop and sender_session_id:
    sender_session = self.sessions.get(sender_session_id)
    if not sender_session or not sender_session.is_em:
        notify_on_stop = False
```

`sender_session` is already fetched above for `sender_name` resolution; the implementation reuses the existing local variable.

The fail-closed logic (`not sender_session or not sender_session.is_em`) ensures that:
- Sender not found → suppress (no way to verify EM status)
- Sender found, `is_em=False` → suppress
- Sender found, `is_em=True` → preserve

### What does NOT change

- `notify_on_stop=True` default in CLI — server-side guard handles suppression transparently
- `--no-notify-on-stop` flag still works (already `False` before the guard; guard only overrides `True→False`, never `False→True`)
- Message queue delivery, stop hook firing, paste-buffered path (#244), skip count (#174), suppression (#182): all untouched
- `sm dispatch`: EM calls dispatch → sender has `is_em=True` → guard preserves `notify_on_stop=True` ✅

## Test Plan

### Unit Tests: `tests/unit/test_directional_notify_on_stop.py`

Test the guard in `session_manager.send_input()` via mocked sessions:

1. **EM sender (`is_em=True`) preserves `notify_on_stop=True`** → `queue_message` called with `notify_on_stop=True`
2. **Non-EM sender (`is_em=False`) suppresses `notify_on_stop`** → `queue_message` called with `notify_on_stop=False`
3. **`is_em` defaults to `False`** — session with no explicit `is_em` is treated as non-EM → suppressed
4. **`notify_on_stop=False` not flipped** — EM sender with explicit `False` → guard does not flip to `True`
5. **No sender session ID** — `sender_session_id=None` → guard skipped, `notify_on_stop=True` passed as-is (no sender to notify anyway; message_queue's own check at 981 requires sender_session_id to arm)
6. **Sender not in sessions dict (fail-closed)** — `sender_session_id` set but `self.sessions.get()` returns `None` → `notify_on_stop` overridden to `False`
7. **Urgent delivery path** — EM sender, urgent mode → `notify_on_stop=True` preserved through guard
8. **Important delivery path** — non-EM sender, important mode → suppressed to `False`

### Unit Tests: `tests/unit/test_em_cmd.py` (extend existing)

9. **`cmd_em` calls `set_em_role()`** — verify `client.set_em_role(session_id)` is called
10. **`set_em_role` unavailable → exit 2** — `set_em_role` returns `(False, True)` → `cmd_em` exits 2
11. **`set_em_role` API failure warns and continues** — `set_em_role` returns `(False, False)` → warning printed, execution continues

### Unit Tests: `tests/unit/test_client_set_em_role.py` (or extend existing client tests)

12. **`set_em_role` sends `PATCH /sessions/{id}` with `{"is_em": True}`**
13. **`set_em_role` returns `(True, False)` on success**
14. **`set_em_role` returns `(False, True)` on unavailable**

### Unit Tests: `tests/unit/test_models.py` (extend)

15. **`is_em` defaults to `False`** — `Session()` without `is_em` → `session.is_em == False`
16. **`is_em` round-trips through `to_dict()`/`from_dict()`** — `True` survives serialization
17. **`from_dict()` backward-compat** — dict without `is_em` key → `session.is_em == False`

### Integration Tests: `tests/integration/test_api_endpoints.py` (extend at line 379+)

18. **`PATCH /sessions/{id}` with `{"is_em": true}` sets flag** — response has `is_em=true`, persisted on session object
19. **`PATCH /sessions/{id}` with mixed payload** — `{"friendly_name": "em-session9", "is_em": true}` — both fields updated, both reflected in response
20. **`PATCH /sessions/{id}` with `is_em=false`** — clears flag if previously set
21. **`GET /sessions/{id}` reflects `is_em`** — after setting via PATCH, GET response includes `is_em=true`

### Regression

22. **Existing stop notification flow**: EM (`is_em=True`) sends to engineer → engineer enrolled → engineer stops → EM notified (no regression)
23. **`--no-notify-on-stop` still works**: EM uses `--no-notify-on-stop` → `notify_on_stop=False` at CLI → guard does not flip → `False` at `queue_message`

### Manual Verification

```bash
# Setup
sm em session9               # sets friendly_name + is_em=True

# Verify EM flag is set and visible in API response
curl -s localhost:8420/sessions/<em-id> | jq '.is_em'
# Expected: true

# Test 1: EM → engineer (should enroll)
sm send <engineer-id> "do this task"
# When engineer stops → EM receives "[sm] engineer stopped" ✅

# Test 2: engineer → EM (should NOT enroll)
# (from engineer's session — is_em=False by default)
sm send <em-id> "task done"
# When EM stops → engineer receives nothing ✅
```

## Ticket Classification

**Single ticket.** Changes span 5 files (`models.py`, `server.py`, `client.py`, `commands.py`, `session_manager.py`) but each change is small and sequential. No schema migrations (JSON state file; `from_dict` handles missing key with default). `SessionResponse` addition requires updating all call sites that return it, but these are mechanical. One agent can complete without compacting context.
