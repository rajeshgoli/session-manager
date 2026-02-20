# sm#283: `sm status` text not cleared on `sm clear` / `sm dispatch`

## Investigation Trigger

An agent's `sm status "Implementing briefing assembler — writing src/briefing.py, tests, wiring MCP tool"` call appeared as:

```
⏺ Bash(sm status "Implementing briefing assembler — writing src/briefing.py, tests, wiring MCP tool" 2>/dev/null || echo "sm not available")
  ⎿  Error: Sibling tool call errored
```

Two related issues identified:

1. **Sibling tool call errored** — the `sm status` bash call never ran
2. **Stale status after clear** — even if it had run, the status would have persisted past the next `sm clear` (user-reported gap)

---

## Issue 1: "Error: Sibling tool call errored"

### What the error means

`Error: Sibling tool call errored` is **Claude Code's own error message**, not output from the shell command. It appears in the tool result when:

- Claude Code makes multiple tool calls **in parallel** (sibling calls in the same batch)
- One of those sibling calls **fails at the tool execution level** (not a non-zero exit code)
- Claude Code marks the remaining siblings as `Sibling tool call errored` and aborts them

The `sm status "..."` bash call was **never executed**. It was aborted because another tool call in the same parallel batch failed first.

### Why the fallback didn't help

The bash call had `2>/dev/null || echo "sm not available"` — this only catches shell-level non-zero exits. Claude Code's tool framework error bypasses the shell entirely; the command is never handed to the shell.

### Observed behavior / hypothesis

The message `Error: Sibling tool call errored` is not shell output — it is produced by Claude Code's tool execution framework. The `sm status` command was never handed to the shell.

**Hypothesis (not verifiable from this repo):** When Claude Code makes parallel tool calls, a hard failure in one tool call (sandbox error, timeout, permission denial) causes the framework to cancel remaining siblings with this error message. The `sm status` bash call was collateral — cancelled before execution because another sibling failed first.

This behavior is inferred from the observable error message and is plausible, but this codebase has no instrumentation of Claude Code's internal tool batching. The root cause of the sibling failure that triggered the cancellation is unknown.

---

## Issue 2: `agent_status_text` Not Cleared on `sm clear` / `sm dispatch`

### Observed behavior

After `sm clear <agent>` or `sm dispatch <agent> --role ...` (which internally calls `cmd_clear`), the agent's `agent_status_text` field persists. EM's `sm children` display continues showing the old status (e.g., "Implementing briefing assembler...") even after the context is reset for a new task.

### Code trace

**Set path** (`sm status "text"` → `cmd_agent_status`):

```
cli/main.py:423-428
  → commands.cmd_agent_status()
    → client.set_agent_status(session_id, text)
      → POST /sessions/{id}/agent-status
        → session.agent_status_text = request.text    ← set here
        → session.agent_status_at = datetime.now()
        → _save_state()
```

**Clear path 1 — tmux sessions (`sm clear` CLI):**

```
commands.cmd_clear()
  → client.invalidate_cache()                         ← POST /sessions/{id}/invalidate-cache
    → _invalidate_session_cache(arm_skip=True)        ← does NOT touch agent_status_text
  → tmux operations (ESC, /clear, Enter) via subprocess
    → Claude processes /clear → SessionStart hook → context_reset event
      → server.py:2744 session._context_warning_sent = False
      → session._context_critical_sent = False        ← does NOT touch agent_status_text
```

**Clear path 2 — codex-app sessions (`sm clear` CLI):**

```
commands.cmd_clear()
  → client.clear_session()                            ← POST /sessions/{id}/clear
    → session_manager.clear_session()                 ← does NOT touch agent_status_text
    → _invalidate_session_cache()                     ← does NOT touch agent_status_text
    → cancel_remind() / cancel_parent_wake()
```

**`sm dispatch` path:**

```
commands.cmd_dispatch()
  → cmd_clear(client, em_id, agent_id)               ← calls same cmd_clear above
  → cmd_send(...)
```

### Confirmed gap

`agent_status_text` and `agent_status_at` are **never reset** anywhere in the clear pathway. The field is only written by `set_agent_status` and never cleared until the agent sets a new status via `sm status "text"` in its new task context.

### Impact

EM's `sm children` output shows misleading stale status from the previous task while the agent is already running a new task. If the new task agent doesn't call `sm status` early, the old status could persist for the entire duration of the new task.

---

## Proposed Solution

Clear `agent_status_text` and `agent_status_at` at three points in the clear pathways — after a context reset is confirmed, not before.

### Why `_invalidate_session_cache` is the wrong location

For tmux sessions, `cmd_clear` calls `/invalidate-cache` **before** the tmux `/clear` is sent (line 2238 precedes line 2253, intentionally — see `#174` comment). Placing the reset inside `_invalidate_session_cache` would clear status even when the subsequent tmux operations fail, leaving a cleared-but-not-actually-cleared agent with no visible status.

### Correct fix locations

**Location A — `context_reset` event handler (`server.py:2741-2748`)**

Covers: Claude tmux `/clear` from both CLI (`sm clear`) and manual TUI `/clear`. Both trigger the SessionStart hook with `source=clear`, which reaches this handler. This is the confirmed-clear signal for Claude tmux sessions. (Codex tmux uses `/new` which has no equivalent hook — handled by Location C.)

```python
if data.get("event") == "context_reset":
    session._context_warning_sent = False
    session._context_critical_sent = False
    session.agent_status_text = None    # ← add
    session.agent_status_at = None      # ← add
    if queue_mgr:
        queue_mgr.cancel_context_monitor_messages_from(session_id)
    app.state.session_manager._save_state()   # ← add: persist the reset
    return {"status": "flags_reset"}
```

**Location B — `/sessions/{session_id}/clear` endpoint (`server.py:1272-1310`)**

Covers: codex-app sessions. The codex-app clear calls this endpoint (no tmux, no SessionStart hook, so Location A doesn't fire). Fix alongside the existing `cancel_remind` / `cancel_parent_wake` calls:

```python
# After cancel_remind / cancel_parent_wake (line 1307-1308):
session.agent_status_text = None
session.agent_status_at = None
app.state.session_manager._save_state()
```

**Location C — `cmd_clear` CLI, post-tmux-success path (`commands.py:2316-2320`)**

Covers: codex tmux sessions (provider=`codex`, uses `/new`). The codex tmux path has no SessionStart hook — `context_reset` is emitted by `session_clear_notify.sh` which is a Claude Code hook and doesn't fire for Codex CLI. After the tmux `/new` succeeds, `cmd_clear` prints and returns with no server call.

Fix: after the tmux block succeeds, make a best-effort server call to clear agent status. To keep the diff minimal, extend `POST /sessions/{id}/agent-status` to accept `{"text": null}` as a clear operation (rather than adding a new endpoint):

```python
# AgentStatusRequest: make text Optional[str]
# In set_agent_status handler:
session.agent_status_text = request.text   # None clears the field
session.agent_status_at = datetime.now() if request.text else None
# Only reset remind timer when setting a non-null status — a null/clear call
# must not disturb an active remind registration on the new task.
if request.text is not None and queue_mgr:
    queue_mgr.reset_remind(session_id)
app.state.session_manager._save_state()
```

Client: add `clear_agent_status(session_id)` → `POST /sessions/{id}/agent-status` with `{"text": null}`.

In `cmd_clear` (commands.py), after the success print at line ~2316:
```python
# Best-effort clear of stale agent status (codex tmux has no context_reset hook)
client.clear_agent_status(target_session_id)
```

Non-critical — no early return on failure; the `/new` already succeeded.

---

## Implementation Approach

Changes span `src/server.py`, `src/cli/client.py`, `src/cli/commands.py`:

1. `server.py` — `context_reset` handler (~line 2744): add status reset + `_save_state()`
2. `server.py` — `/sessions/{id}/clear` endpoint (~line 1308): add status reset + `_save_state()`
3. `server.py` — `AgentStatusRequest` model + `set_agent_status` handler: make `text` optional, treat `null` as clear
4. `client.py` — add `clear_agent_status(session_id)` method
5. `commands.py` — `cmd_clear` success path (~line 2316): call `client.clear_agent_status()` after tmux `/new` succeeds

---

## Test Plan

1. Set agent status: `sm status "doing task A"`
2. Verify status is visible in `sm children` output
3. Call `sm clear <agent-id>` (or `sm dispatch`)
4. Immediately call `sm children` / `sm status`
5. Verify `agent_status_text` is no longer shown (not "doing task A")

For codex-app path: same steps but target a codex-app session.

---

## Ticket Classification

**Single ticket.** Three-file change (`server.py`, `client.py`, `commands.py`) but entirely self-contained — no architectural decisions, one agent can complete without compacting context.
