# Fix Telegram Topic Duplication on Server Restart

**Created:** 2026-02-15

---

## 1. Problem Statement

Session `c1d607d3` (`codex-doc-reviewer`) has generated hundreds of duplicate `codex-doc-reviewer [c1d607d3]` forum topics on Telegram, flooding the chat and obscuring other threads.

### What Happened

The server has been crash-looping since at least **2026-01-30**, with the most intense period from 2026-02-14 21:40 through 2026-02-15 09:19.

Evidence from `/tmp/claude-session-manager.log` (snapshot taken 2026-02-15 ~09:18 local):

| Metric | Count | Query |
|--------|-------|-------|
| Server starts | 20,382 | `grep -c "Starting Claude Session Manager" /tmp/claude-session-manager.log` |
| Port bind failures | 20,288 | `grep -c "address already in use" /tmp/claude-session-manager.log` |
| Topics created for `c1d607d3` | 552 | `grep -c "Auto-created topic for session c1d607d3" /tmp/claude-session-manager.log` |
| Watchdog kills | 6 | `grep -c "Event loop is frozen" /tmp/claude-session-manager.log` |

**Note:** These counts span the full log file, which covers multiple days (earliest server starts from 2026-01-30). The counts have continued to grow since this snapshot. The ~10s interval between topic creations matches the launchd `ThrottleInterval`.

### Root Cause Chain

Three bugs compound into this outcome:

#### Bug 1: Crash Loop — launchd vs. Running Instance

The server is managed by launchd (`scripts/com.claude.session-manager.plist`) with `KeepAlive: true` and `ThrottleInterval: 10`. At some point, an instance crashed or was killed, and launchd began spawning new instances. But the previous instance (or a manually-started one) was **still alive and holding port 8420**. Each new instance:

1. Starts up, runs full initialization (including Telegram topic creation)
2. Tries to bind port 8420
3. Fails with `[Errno 48] address already in use`
4. Exits — launchd spawns another one 10 seconds later

The startup sequence in `main.py:425-466` runs `_reconcile_telegram_topics()` at line 445, which calls the Telegram API to create forum topics — **before** uvicorn attempts to bind the port at line 459. Side effects (Telegram API calls) fire even when the instance is doomed to fail.

#### Bug 2: State File Race — Running Instance Overwrites Dying Instance

Multiple processes share the same state file (`/tmp/claude-sessions/sessions.json`) with no locking:

```
Timeline:
─────────────────────────────────────────────────────────────
Instance A (running, holding port 8420):
  In-memory: thread_id = null
  Periodically calls _save_state() → writes null to file

Instance B (crash-loop, 10s lifecycle):
  1. _load_state() reads file → thread_id = null
  2. Creates topic on Telegram (API succeeds, topic visible)
  3. _save_state() writes thread_id = 8250 to file
  4. Port bind fails → exits

Instance A calls _save_state() again → overwrites 8250 with null

Instance C (next crash-loop):
  1. _load_state() reads file → thread_id = null (overwritten by A)
  2. Creates ANOTHER topic on Telegram
  ...repeat
─────────────────────────────────────────────────────────────
```

The dying instance's save is always clobbered by the running instance, so the file perpetually shows `telegram_thread_id: null`. This is confirmed by the current state file still showing null despite hundreds of successful topic creations.

#### Bug 3: No Pre-Flight Check Before Side Effects

`_reconcile_telegram_topics()` fires Telegram API calls (creating forum topics) during startup initialization, before the server confirms it can actually bind its port. This means every doomed instance still produces irreversible side effects.

### Current State

```json
{
  "id": "c1d607d3",
  "friendly_name": "codex-doc-reviewer",
  "telegram_chat_id": -1003506774897,
  "telegram_thread_id": null,
  "provider": "codex"
}
```

### Impact

- Hundreds of duplicate forum topics flood the Telegram chat
- Each topic includes a welcome message
- Real session threads are buried
- 190MB+ log file from crash-loop output
- Wasted Telegram API quota

---

## 2. Design

### 2.1 Defer Side Effects Until After Port Bind (Primary Fix)

The startup sequence in `main.py:425-466` must be reordered so the uvicorn server **binds the port first**, before any Telegram API calls. If the port is already in use, the process exits immediately with no side effects.

Current order:
```
__init__()        → _load_state()
start()           → child_monitor.start()
                  → message_queue.start()
                  → telegram_bot.start()
                  → load_session_threads()
                  → _reconcile_telegram_topics()  ← CREATES TOPICS HERE
                  → restore monitoring
                  → uvicorn.Server.serve()        ← BINDS PORT HERE (too late)
```

Target order:
```
__init__()        → _load_state()
start()           → child_monitor.start()
                  → message_queue.start()
                  → telegram_bot.start()
                  → load_session_threads()
                  → uvicorn startup begins
on_startup/       → _reconcile_telegram_topics()  ← MOVED: after bind succeeds
  lifespan hook   → restore monitoring
```

**Primary implementation — ASGI lifespan:** Use Starlette's lifespan context manager (or FastAPI's `@app.on_event("startup")`) to run reconciliation and monitoring restoration after uvicorn has successfully bound the port. This guarantees no side effects fire for doomed instances — no TOCTOU gap.

```python
# In main.py, wrap the post-bind work in the ASGI lifespan:
@asynccontextmanager
async def lifespan(app: FastAPI):
    # Post-bind: safe to run side effects now
    await sm_app._reconcile_telegram_topics()
    await sm_app._restore_monitoring()
    yield
    # Shutdown cleanup
    ...

app = FastAPI(lifespan=lifespan)
```

**Alternative — pre-flight socket probe:** If restructuring the lifespan is too invasive, a simple TCP bind check at the top of `start()` catches the common case. This has a TOCTOU window (port could be released between probe and actual bind) but is pragmatically sufficient for the launchd crash-loop scenario:

```python
async def start(self):
    import socket
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.bind((self.host, self.port))
    except OSError:
        logger.error(f"Port {self.port} already in use, exiting without side effects")
        return
    finally:
        sock.close()
    # Now safe to proceed...
```

### 2.2 Persist `thread_id` Immediately

Add an immediate `_save_state()` call inside the topic creation success path, while preserving the existing outer save for `chat_id`-only changes.

Current code (`session_manager.py:410-444`):
```python
async def _ensure_telegram_topic(self, session, explicit_chat_id=None):
    changed = False

    # 1. Ensure chat_id is set
    if not session.telegram_chat_id:
        chat_id = explicit_chat_id or self.default_forum_chat_id
        if chat_id:
            session.telegram_chat_id = chat_id
            changed = True

    # 2. Create topic if needed
    if session.telegram_chat_id and not session.telegram_thread_id and self._topic_creator:
        topic_name = f"{session.friendly_name or 'session'} [{session.id}]"
        try:
            thread_id = await self._topic_creator(...)
            if thread_id:
                session.telegram_thread_id = thread_id
                changed = True
                logger.info(...)
        except Exception as e:
            logger.warning(...)

    if changed:
        self._save_state()
```

Target code — add an immediate save inside `if thread_id:`, keep the outer save for chat_id-only changes:
```python
async def _ensure_telegram_topic(self, session, explicit_chat_id=None):
    changed = False

    # 1. Ensure chat_id is set
    if not session.telegram_chat_id:
        chat_id = explicit_chat_id or self.default_forum_chat_id
        if chat_id:
            session.telegram_chat_id = chat_id
            changed = True

    # 2. Create topic if needed
    if session.telegram_chat_id and not session.telegram_thread_id and self._topic_creator:
        topic_name = f"{session.friendly_name or 'session'} [{session.id}]"
        try:
            thread_id = await self._topic_creator(...)
            if thread_id:
                session.telegram_thread_id = thread_id
                self._save_state()  # Persist IMMEDIATELY — minimize race window
                changed = False     # Already saved; prevent redundant outer save
                logger.info(...)
        except Exception as e:
            logger.warning(...)

    # Outer save: handles chat_id-only backfill (when topic creation
    # fails or is skipped). Does NOT re-fire after a successful topic
    # creation since changed is reset to False above.
    if changed:
        self._save_state()
```

This doesn't fully solve the state file race (Bug 2 — the running instance can still overwrite), but it minimizes the window. The primary fix (2.1) eliminates the crash-loop that causes the race.

### 2.3 State File Locking (Stretch)

The deeper problem is multiple processes sharing a state file with no coordination. Full fix:

- Use `fcntl.flock()` on the state file before read/write
- Or use a PID file to prevent concurrent instances entirely

This is a broader change. The lifespan fix (2.1) eliminates the crash-loop scenario, which is the only realistic way multiple instances run concurrently. A PID file would be defense-in-depth.

### 2.4 Cleanup: Bulk-Delete Duplicate Topics

A one-off script to delete the duplicate topics from Telegram. The Telegram Bot API doesn't support listing forum topics, so the cleanup approach is:

1. Extract all thread IDs from the log: `grep "Auto-created topic for session c1d607d3" /tmp/claude-session-manager.log | sed 's/.*thread=//;s/[^0-9].*//'`
2. Call `deleteForumTopic(chat_id, thread_id)` for each
3. Optionally keep the most recent one and set it as the session's `telegram_thread_id`

---

## 3. Key Files to Modify

| File | Change |
|------|--------|
| `src/main.py` | `start()` / lifespan — defer `_reconcile_telegram_topics()` and monitoring restoration until after port bind succeeds |
| `src/session_manager.py` | `_ensure_telegram_topic()` — add immediate `_save_state()` inside `if thread_id:`, reset `changed` to avoid redundant outer save |

---

## 4. Edge Cases

### Pre-Flight Check and SO_REUSEADDR

If the pre-flight alternative is used: the socket probe may succeed even if uvicorn later fails, if uvicorn uses `SO_REUSEADDR` differently. **Mitigation:** The lifespan approach (primary recommendation) avoids this entirely. If using the probe, match uvicorn's socket options.

### Race Between Topic Creation and Server Kill

If the server is killed (SIGKILL) between `create_forum_topic()` and `_save_state()`, the topic exists on Telegram but `thread_id` is not persisted. **Mitigation:** One orphan topic may remain on Telegram. The lifespan fix prevents multiplication by ensuring only the port-holding instance runs reconciliation.

### Telegram API Rate Limiting

Hundreds of `createForumTopic` calls over multiple days is within Telegram's rate limits (~30/second). But rapid restarts (every 10s) that each make 2-3 API calls could compound with other bot traffic.

### Multiple Sessions With Missing `thread_id`

Session `d1614fc0` also appears in the log as creating topics during the crash loop. The same fixes apply to all sessions uniformly.

### EventLoopWatchdog and os._exit(1)

The watchdog (`main.py:62-70`) calls `os._exit(1)` when the event loop freezes, which skips all Python cleanup. This contributed 6 of the crashes. While not the primary cause of the crash loop, `os._exit()` guarantees that any in-flight `_save_state()` will not complete. **Mitigation:** The lifespan fix prevents side effects from crash-loop instances, making this a non-issue for the topic duplication bug. The watchdog behavior is a separate concern.

---

## Appendix: Immediate Remediation (Ops)

These are operational steps to stop the bleeding, not code changes:

1. **Stop the crash loop:** Identify and kill the conflicting process holding port 8420, or stop and restart the launchd service cleanly:
   ```bash
   launchctl unload ~/Library/LaunchAgents/com.claude.session-manager.plist
   # Kill any remaining instances
   pkill -f "python.*src.main"
   launchctl load ~/Library/LaunchAgents/com.claude.session-manager.plist
   ```

2. **Delete duplicate topics:** Run the cleanup script (section 2.4) after the crash loop is resolved.

3. **Kill stale session (conditional):** If session `c1d607d3` is no longer needed and is in a terminal state (idle, not actively being used):
   ```bash
   sm kill c1d607d3
   ```
   **Prerequisite:** Verify the session's current status before killing. It may be active (e.g., processing a review request).

4. **Truncate log:** The 190MB+ log file should be rotated or truncated after extracting the thread IDs needed for cleanup.
