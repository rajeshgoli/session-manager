# Fix Crash Recovery Blocked for RUNNING Sessions

**Created:** 2026-02-15

---

## 1. Problem Statement

Sessions that crash with a `RangeError: Maximum call stack size exceeded` while in `RUNNING` status are never auto-recovered. The output monitor detects the crash pattern but its `_handle_crash` guard rejects recovery because the session's status is still `RUNNING`.

### What Happened

Sessions `3e9a2b67` and `f6dd3349` crashed with JavaScript stack overflows in the Claude Code TUI harness. The output monitor detected the crash patterns but logged:

```
Crash detected in session 3e9a2b67 but status is RUNNING, skipping recovery (agent may still be active)
```

These sessions remained in a crashed state until they happened to crash again while in IDLE status, at which point auto-recovery succeeded.

### Evidence

From `/tmp/claude-session-manager.log`:

| Event | Count | Sessions |
|-------|-------|----------|
| Blocked recoveries (status=RUNNING) | 6 | `3e9a2b67` (4), `f6dd3349` (2) |
| Successful recoveries (status!=RUNNING) | 20+ | `a7587515` (10), `33194522` (8), `3e9a2b67` (10), `f6dd3349` (3) |
| Unrecovered crash (found manually) | 1 | `f9693514` (circumstantial — no blocked-recovery log line found; may have been missed by monitor rather than blocked by RUNNING guard) |

The blocked events span from 2026-02-08 through 2026-02-14. Sessions `3e9a2b67` and `f6dd3349` were eventually recovered (on subsequent crashes when they happened to be IDLE). Session `f9693514` was found crashed manually; its inclusion here is circumstantial as the log evidence may have rotated. The 6 verified blocked recoveries for `3e9a2b67` and `f6dd3349` are the primary evidence.

### Root Cause

The guard in `output_monitor.py:341-347`:

```python
# Only recover sessions in IDLE or STOPPED state (not RUNNING)
if session.status == SessionStatus.RUNNING:
    logger.warning(
        f"Crash detected in session {session.id} but status is RUNNING, "
        "skipping recovery (agent may still be active)"
    )
    return
```

This was written defensively — the intent was to avoid interrupting an actively-working agent. But it's wrong for two reasons:

1. **The crash doesn't always kill the harness.** The JavaScript stack overflow corrupts the terminal output (and sometimes the input fields) but the harness often recovers on its own and the agent continues working. The crash pattern in the output is real — the error happened — but the session may still be functional. The guard assumes any crash detection means the harness is dead, which is not always true.

2. **Status is stale at crash time.** When the harness *does* die, the session is `RUNNING` because it was actively working when it crashed. The Stop hook never fires (the harness died before it could), so nothing transitions the status to `IDLE`. The guard creates a Catch-22: the session can only be recovered when IDLE, but it can only become IDLE through recovery.

### Impact

When the harness dies (session stuck at bash prompt):
- Crashed sessions sit indefinitely until manually discovered and recovered
- Message queue deliveries to the session fail silently (messages go to a dead bash prompt)
- Parent sessions waiting on child completion are never notified
- The agent's in-progress work is lost (no `--resume`)

When the harness survives (agent keeps working):
- Terminal scrollback is polluted with large error dumps
- Input fields may be corrupted, degrading the agent's ability to interact
- No mechanism to clean up the session after the agent finishes its task

---

## 2. Design

### 2.1 Replace the RUNNING Guard with Deferred Recovery

The crash pattern in the output is always real — the JavaScript stack overflow happened. But the harness often survives: it corrupts the terminal output (and sometimes the input fields) but continues working. We must not kill a working agent.

**New behavior:**

- **Session is IDLE or STOPPED** → recover immediately (same as current behavior for non-RUNNING sessions)
- **Session is RUNNING** → mark the session for deferred recovery; when it next transitions to IDLE, trigger recovery then. This gives the agent a clean session (no error dump in the scrollback) without interrupting active work.

Current code (`output_monitor.py:330-358`):
```python
async def _handle_crash(self, session: Session, content: str):
    # Only recover sessions in IDLE or STOPPED state (not RUNNING)
    if session.status == SessionStatus.RUNNING:
        logger.warning(...)
        return

    logger.warning(f"Claude Code harness crash detected in session {session.id}")

    if self._crash_recovery_callback:
        await self._crash_recovery_callback(session)
```

Target code:
```python
async def _handle_crash(self, session: Session, content: str):
    # Only Claude sessions support crash recovery
    if getattr(session, "provider", "claude") != "claude":
        return

    # Debounce (section 2.2) ...

    if session.status == SessionStatus.RUNNING:
        # Agent is still working — defer recovery to when it goes idle
        self._pending_crash_recovery.add(session.id)
        logger.info(
            f"Crash pattern detected in session {session.id} while RUNNING, "
            "deferring recovery until idle"
        )
        return

    logger.warning(
        f"Claude Code harness crash detected in session {session.id} "
        f"(status={session.status}), recovering now"
    )

    if self._crash_recovery_callback:
        await self._crash_recovery_callback(session)
```

### 2.2 Add Debounce to Prevent Double-Recovery

The crash dump can span multiple log chunks (the RangeError output is large). Without debounce, the monitor could detect the pattern in consecutive poll cycles and trigger recovery twice. Add a per-session cooldown with two tiers:

- **Success cooldown (30s):** After a successful recovery, suppress further crash detection for 30 seconds to prevent double-recovery from overlapping crash dump chunks.
- **Failure cooldown (5s):** After a failed recovery attempt, suppress for 5 seconds to prevent tight retry loops when the failure is persistent (e.g., no resume UUID available). The crash pattern may still appear in subsequent poll chunks, and hammering `recover_session` on every 1s poll cycle is wasteful.

```python
CRASH_DEBOUNCE_SUCCESS = timedelta(seconds=30)
CRASH_DEBOUNCE_FAILURE = timedelta(seconds=5)

async def _handle_crash(self, session: Session, content: str):
    # Debounce: skip if we recovered recently (success or failure)
    recovery_state = self._last_crash_recovery.get(session.id)
    if recovery_state:
        last_time, last_succeeded = recovery_state
        cooldown = CRASH_DEBOUNCE_SUCCESS if last_succeeded else CRASH_DEBOUNCE_FAILURE
        if datetime.now() - last_time < cooldown:
            return

    logger.warning(
        f"Claude Code harness crash detected in session {session.id} "
        f"(status={session.status})"
    )

    if self._crash_recovery_callback:
        success = await self._crash_recovery_callback(session)
        self._last_crash_recovery[session.id] = (datetime.now(), bool(success))
```

The `_last_crash_recovery` dict (type: `dict[str, tuple[datetime, bool]]`) should be initialized in `__init__` alongside the existing `_notified_permissions` dict. Each entry stores the timestamp and outcome of the last recovery attempt, so the gate applies the correct cooldown window.

### 2.3 Trigger Deferred Recovery When Safe

Not all IDLE states are safe for recovery:

- **Permission prompt** — NOT safe. Claude is mid-task, asking to perform an action. Killing it here loses the permission request; `--resume` will not re-ask. The agent resumes in IDLE state with the pending action silently dropped. Critically, a session at a permission prompt will eventually trigger `_check_idle` if the user doesn't respond within the idle timeout — so the status guard alone (`session.status == IDLE`) is insufficient to exclude this case.
- **Completion** — Safe. The agent finished its work. Clean restart has no cost.
- **Idle timeout (no pending permission)** — Safe. No activity for 5+ minutes and no permission prompt waiting. Nothing to interrupt.

#### Tracking permission-prompt state

Add a `_awaiting_permission` dict (type: `dict[str, bool]`) initialized in `__init__`:

- **Set to `True`** in `_handle_permission_prompt` when a permission prompt is detected.
- **Cleared** in `_analyze_content` when new activity arrives (any new content means the session moved past the permission state — either the user responded, or the agent continued).

```python
# In _handle_permission_prompt:
self._awaiting_permission[session.id] = True

# In _analyze_content, at the top before pattern checks:
self._awaiting_permission.pop(session.id, None)
```

#### Flush on safe transitions

```python
async def _flush_pending_crash_recovery(self, session: Session):
    """If session has a pending crash recovery, trigger it now (gracefully)."""
    if session.id not in self._pending_crash_recovery:
        return

    # Only flush when session is safely idle — never while RUNNING or at permission prompt
    if session.status not in (SessionStatus.IDLE, SessionStatus.STOPPED):
        return
    if self._awaiting_permission.get(session.id):
        return

    logger.warning(
        f"Session {session.id} is idle with pending crash recovery, "
        "recovering now for a clean session"
    )
    if self._crash_recovery_callback:
        success = await self._crash_recovery_callback(session, graceful=True)
        self._last_crash_recovery[session.id] = (datetime.now(), bool(success))
        if success:
            self._pending_crash_recovery.discard(session.id)
        # On failure, keep session in _pending_crash_recovery for retry
```

Call `_flush_pending_crash_recovery(session)` after the `_status_callback` call in `_handle_completion` and `_check_idle` only. Do **not** call it from `_handle_permission_prompt`.

The `graceful=True` flag tells `recover_session` to use `/exit` + `--resume` instead of Ctrl-C + `--resume`, since the harness is likely still healthy (it survived the crash). This avoids the destructive Ctrl-C path when the session doesn't need it.

#### Retry loop for persistent failures

`_check_idle` has a one-shot gate (`_notified_permissions[notified_key]`, line 372) that prevents it from re-firing after the first idle notification. If the first flush fails and the session stays idle, no new IDLE transition occurs, so recovery never retries.

Add a periodic retry in the monitor loop (`_monitor_loop`) for sessions that are pending and idle:

```python
# In _monitor_loop, after _analyze_content / _check_idle:
if session.id in self._pending_crash_recovery:
    # _flush_pending_crash_recovery has its own status guard (IDLE/STOPPED only),
    # so this is safe to call unconditionally — it no-ops if session is RUNNING.
    recovery_state = self._last_crash_recovery.get(session.id)
    if recovery_state:
        last_time, last_succeeded = recovery_state
        if not last_succeeded and datetime.now() - last_time > CRASH_DEBOUNCE_FAILURE:
            await self._flush_pending_crash_recovery(session)
```

This runs on every poll cycle (~1s) but is gated by: (a) the 5s failure cooldown, and (b) the guards in `_flush_pending_crash_recovery` which only proceeds for IDLE/STOPPED sessions that are not awaiting a permission response. A session that returns to RUNNING or is at a permission prompt is not touched.

#### Init

The `_pending_crash_recovery` set (type: `set[str]`) should be initialized in `__init__`.

---

## 3. Key Files to Modify

| File | Change |
|------|--------|
| `src/output_monitor.py` | `_handle_crash()` — replace RUNNING guard with deferred recovery, add debounce; `_flush_pending_crash_recovery()` — new method; `_handle_completion()`, `_check_idle()` — call flush on safe IDLE transitions; `_monitor_loop()` — add retry for pending + idle sessions |
| `src/session_manager.py` | `recover_session()` — add `graceful` parameter: when `True`, use `/exit` instead of Ctrl-C to cleanly exit a surviving harness |

---

## 4. Edge Cases

### Crash While RUNNING — Harness Survives

The most common case. The harness crashes internally but recovers on its own. The agent continues working with corrupted scrollback. `_handle_crash` adds the session to `_pending_crash_recovery`. When the agent finishes its task and goes idle, `_flush_pending_crash_recovery` triggers a clean restart via `--resume`, giving the session a fresh terminal without the error dump.

### Crash While RUNNING — Harness Dies

Less common. The harness is dead, the session is stuck at a bash prompt, and status never transitions to IDLE. The existing idle timeout in `_check_idle` will eventually fire (default 5 minutes of no output), transition the session to IDLE, and `_flush_pending_crash_recovery` kicks in. If that first attempt fails (e.g., no resume UUID parsed from a dead terminal), the retry loop in `_monitor_loop` retries every 5 seconds. This is slower than immediate recovery but avoids killing a working agent.

### Self-Referential Detection

An agent *discussing* crash patterns (e.g., working on this spec) produces the pattern strings in its own terminal output. The monitor detects a "crash" that never happened. With deferred recovery, the session is RUNNING, so recovery is deferred. The agent keeps working. When it goes idle via completion or idle timeout (not permission prompt), it gets a graceful restart (`/exit` + `--resume`). This is an unnecessary restart but results in a clean scrollback. The graceful path avoids the destructive Ctrl-C used for truly dead harnesses.

### Double-Detection From Large Crash Dumps

The crash stack trace can be 100+ lines. If it straddles two poll intervals, `_analyze_content` fires twice with overlapping patterns. For IDLE sessions, the debounce (section 2.2) prevents double-recovery. For RUNNING sessions, the second detection is a no-op (`session.id` is already in `_pending_crash_recovery`).

### Rapid Successive Crashes

If a session crashes, recovers, then crashes again within 30 seconds, the second crash is debounced. This is acceptable — `recover_session` already increments `recovery_count`, and a session that crashes immediately after recovery likely has a deeper issue that automated recovery won't solve.

### Non-Claude Providers

The output monitor runs for both `claude` and `codex` (CLI) sessions — only `codex-app` sessions are excluded from monitoring (`server.py` lines 700, 744, 1619). The provider gate at the top of `_handle_crash` (section 2.1) rejects non-claude sessions immediately — they are never added to `_pending_crash_recovery` and never reach `recover_session`. This prevents both false recovery attempts and the retry churn that would result from a non-claude session entering the pending set (since `recover_session` always fails for non-claude providers).
