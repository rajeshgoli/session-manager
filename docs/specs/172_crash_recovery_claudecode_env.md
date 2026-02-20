# Fix Crash Recovery: Unset CLAUDECODE Before Resume

**Issue:** #172
**Created:** 2026-02-17

---

## 1. Problem Statement

When `recover_session()` resumes a crashed Claude Code session, the `CLAUDECODE` environment variable is still set in the tmux shell, causing the resume command to fail with:

```
Error: Claude Code cannot be launched inside another Claude Code session.
Nested sessions share runtime resources and will crash all active sessions.
To bypass this check, unset the CLAUDECODE environment variable.
```

### What Happened

A Claude Code session in `fractal-market-simulator` crashed with `Exception in PromiseRejectCallback`. The session manager's output monitor detected the crash and triggered `recover_session()`. Recovery parsed the resume UUID, sent `stty sane`, then sent `claude --dangerously-skip-permissions --resume <uuid>`. Claude Code checked for the `CLAUDECODE` env var (which was exported by the now-dead process and still lives in the shell), found it, and refused to start.

The session was left at a bare shell prompt. The user had to manually run `stty sane` and try `claude --resume` themselves, only to hit the same error.

### Root Cause

`create_session()` (line 126) and `create_session_with_command()` (line 227) both run `unset CLAUDECODE` before launching Claude:

```python
self._run_tmux("send-keys", "-t", session_name, "unset CLAUDECODE", "Enter")
```

`recover_session()` does not. It goes straight from `stty sane` (line 1662) to building and sending the resume command (line 1682). The `CLAUDECODE` variable, exported by the previous Claude Code process into the shell, persists after the process dies and blocks the resume.

### Impact

- Crash recovery silently fails — the session stays dead at a bash prompt
- Queued messages are delivered to the bare shell instead of Claude
- Parent sessions waiting on child completion are never notified
- The agent's in-progress work is not resumed despite a valid resume UUID being available

---

## 2. Design

### 2.1 Add `unset CLAUDECODE` Before the Resume Command

Insert an `unset CLAUDECODE` send-keys call in `recover_session()` immediately before the resume command, after the `stty sane` block. This must run in **both** recovery paths (graceful and forceful) since either path could have `CLAUDECODE` set.

Current code (`session_manager.py:1660-1688`):

```python
# 5. Reset terminal with stty sane (only needed for forceful Ctrl-C recovery)
if not graceful:
    ...stty sane...

# 6. Build resume command with config args
...
resume_cmd += f" --resume {resume_uuid}"
proc = await asyncio.create_subprocess_exec(
    "tmux", "send-keys", "-t", session.tmux_session, resume_cmd, "Enter",
    ...
)
```

Target code:

```python
# 5. Reset terminal with stty sane (only needed for forceful Ctrl-C recovery)
if not graceful:
    ...stty sane...

# 6. Unset CLAUDECODE to prevent nested-session detection
#    (Claude Code exports this; it persists in the shell after the process dies)
logger.debug(f"Unsetting CLAUDECODE in session {session.id}")
proc = await asyncio.create_subprocess_exec(
    "tmux", "send-keys", "-t", session.tmux_session,
    "unset CLAUDECODE", "Enter",
    stdout=asyncio.subprocess.PIPE,
    stderr=asyncio.subprocess.PIPE,
)
await asyncio.wait_for(proc.communicate(), timeout=5)
await asyncio.sleep(0.3)

# 7. Build resume command with config args
...
```

The 0.3s sleep matches `send_keys_settle_seconds` default and ensures the unset completes before the resume command is sent.

---

## 3. Key Files to Modify

| File | Change |
|------|--------|
| `src/session_manager.py` | `recover_session()` — add `unset CLAUDECODE` send-keys before the resume command |

---

## 4. Edge Cases

### Graceful Recovery (harness survived)

In graceful mode, `recover_session()` sends `/exit` to cleanly shut down the harness. The harness may or may not clear `CLAUDECODE` on exit. The unset is needed regardless — it's a no-op if the var is already cleared, and critical if it isn't.

### CLAUDECODE Not Set

If the crash happened before Claude Code had a chance to export `CLAUDECODE` (unlikely but possible), `unset CLAUDECODE` is a harmless no-op.

### Other Environment Variables

`ENABLE_TOOL_SEARCH=false` is also set during session creation. This persists across recovery since the shell env survives the crash — no action needed, the variable is still correctly set.

`CLAUDE_SESSION_MANAGER_ID` is similarly persistent — no re-export needed.

---

## 5. Testing

### Regression Test

Add a test in `tests/regression/` that verifies `recover_session()` sends `unset CLAUDECODE` before the resume command. Mock the `asyncio.create_subprocess_exec` calls and assert the sequence includes the unset.

### Manual Verification

1. Start a session via `sm spawn`
2. In the tmux pane, confirm `echo $CLAUDECODE` returns a value
3. Kill the Claude Code process (simulate crash)
4. Trigger recovery
5. Verify the resume succeeds (previously failed with nested-session error)

---

## 6. Classification

**Single ticket.** One line-group insertion in one file, plus a regression test. One agent can complete this without compacting context.
