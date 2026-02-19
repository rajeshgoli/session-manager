# sm#203: Context-Aware Handoff Triggering

## Problem

Long-running agents (especially EM) accumulate context until compaction fires. Compaction is lossy, unpredictable, and the agent can't see its own context usage. The user had to manually intervene during session 3. We need sm to detect approaching context limits and trigger proactive handoff (via sm#196).

## Investigation Summary

Six approaches were evaluated. Three official Claude Code mechanisms were discovered that, combined, provide a complete solution.

### Approach 1: Status Line API — AVAILABLE, RECOMMENDED PRIMARY

Claude Code supports a `statusLine` setting in `~/.claude/settings.json` that runs a shell command and pipes full context window data via stdin as JSON.

**Configuration format** (object form — the only documented format per [status line docs](https://code.claude.com/docs/en/statusline)):

```json
{
  "statusLine": {
    "type": "command",
    "command": "~/.claude/hooks/context_monitor.sh",
    "padding": 2
  }
}
```

The stdin JSON includes (confirmed fields):

```json
{
  "session_id": "abc123",
  "transcript_path": "/path/to/transcript.jsonl",
  "context_window": {
    "total_input_tokens": 15234,
    "total_output_tokens": 4521,
    "context_window_size": 200000,
    "used_percentage": 8,
    "remaining_percentage": 92,
    "current_usage": {
      "input_tokens": 8500,
      "output_tokens": 1200,
      "cache_creation_input_tokens": 5000,
      "cache_read_input_tokens": 2000
    }
  },
  "exceeds_200k_tokens": false,
  "cost": {
    "total_cost_usd": 0.01234,
    "total_duration_ms": 45000
  },
  "model": { "id": "claude-opus-4-6", "display_name": "Opus" }
}
```

**Key fields:** `used_percentage`, `remaining_percentage`, `context_window_size` — pre-calculated by Claude Code.

**`used_percentage` is input-based only:** calculated as `input_tokens + cache_creation_input_tokens + cache_read_input_tokens` — it does not include `output_tokens`. This means it slightly underestimates true context pressure. Thresholds are calibrated accordingly.

**Update frequency:** After each assistant message, on permission mode change, or vim mode toggle. Debounced at 300ms.

**Caveats:** `used_percentage`, `remaining_percentage`, and `current_usage` may be `null` before the first API call.

**Verdict:** Official, documented, pre-calculated context percentages. Best primary signal.

### Approach 2: PreCompact Hook — AVAILABLE, COMPACTION SAFETY NET

Claude Code exposes a `PreCompact` hook event that fires before compaction:

```json
{
  "hook_event_name": "PreCompact",
  "trigger": "auto",
  "custom_instructions": "",
  "session_id": "abc123",
  "transcript_path": "/path/to/transcript.jsonl"
}
```

Matchers: `auto` (context window full) or `manual` (`/compact` command).

**Cannot block compaction** — exit code 2 only shows stderr to user.

**Verdict:** Direct compaction signal. Use as "last chance" notification — if handoff was too late, at least we know compaction happened.

### Approach 3: SessionStart `compact` matcher — POST-COMPACTION RECOVERY

`SessionStart` fires after compaction with `source: "compact"`:

```json
{
  "hook_event_name": "SessionStart",
  "source": "compact",
  "model": "claude-sonnet-4-6"
}
```

SessionStart hooks can inject `additionalContext` via `hookSpecificOutput` — the documented pattern for "re-inject context after compaction."

**Verdict:** Use as last-resort recovery — inject handoff doc content into post-compaction context.

### Approach 4: Transcript File Monitoring — AVAILABLE, ALTERNATIVE

The transcript JSONL file (path in hook payload as `transcript_path`) contains `assistant` records with full API usage data:

```json
{
  "type": "assistant",
  "message": {
    "usage": {
      "input_tokens": 8,
      "cache_creation_input_tokens": 2281,
      "cache_read_input_tokens": 33640,
      "output_tokens": 1
    }
  }
}
```

**Total context = `input_tokens` + `cache_creation_input_tokens` + `cache_read_input_tokens`**

#### Compaction threshold data (from 30+ real sessions)

| Metric | Value |
|--------|-------|
| Typical compaction threshold | 100K–170K tokens |
| Most common range | 150K–160K tokens |
| Post-compaction size | 55K–110K tokens |
| Session with 6 compactions | Threshold varied between 106K–144K |
| Session with 29 compactions | Max 154K per cycle |

Compaction creates a `summary` type record in the transcript.

**Verdict:** Reliable but requires file I/O on large files. Use as cross-check for status line data.

### Approach 5: Tmux Status Bar Scraping — FRAGILE, NOT RECOMMENDED

Tested with `tmux capture-pane -p` across multiple active sessions. Only shows per-turn tokens, not total context. Requires ANSI parsing. Breaks across versions.

**Verdict:** Rejected.

### Approach 6: Heuristic (dispatch count / elapsed time) — FALLBACK ONLY

Tool call count varies too widely across sessions (25–1500 turns before compaction). Useful only as defense-in-depth if all other signals fail.

## Recommended Design: Three-Layer Context Monitor

### Layer 1: Status Line (Primary — Proactive Warning)

A status line script runs after every assistant message and reports context usage to sm:

```
Claude Code calls statusline script (after each assistant message)
    ↓
Script reads context_window JSON from stdin
    ↓
POSTs {session_id, used_percentage, total_tokens} to sm server
    ↓
sm server stores in Session.tokens_used
    ↓
If used_percentage > warning_pct → send context warning to agent (one-shot)
If used_percentage > critical_pct → send urgent handoff trigger (one-shot)
```

### Layer 2: PreCompact Hook (Safety Net — Last Chance + Flag Reset)

If the agent ignores warnings and compaction is imminent:

```
PreCompact hook fires (trigger=auto)
    ↓
Hook script POSTs {session_id, event: "compaction", trigger: "auto"} to sm server
    ↓
sm resets _context_warning_sent and _context_critical_sent flags
    ↓
sm logs warning: "Compaction triggered — handoff was too late"
    ↓
sm sends urgent notification to parent session / user
```

**Why reset flags here, not in the status line handler:** Post-compaction context can land anywhere between 55K–110K tokens. With a 200K window and warning at 50% (100K), the post-compaction percentage may remain above 50%. Resetting flags in the `used_pct < warning_pct` branch is unreliable — it fails whenever compaction leaves context above the warning threshold. PreCompact is the reliable reset point: it fires on every compaction, always before context is refreshed. Flags reset here re-arm warnings correctly for the next accumulation cycle.

### Layer 3: SessionStart `compact` (Recovery — Post-Compaction)

If compaction fires despite warnings, recover gracefully:

```
SessionStart fires with source=compact
    ↓
Hook script queries sm server: GET /sessions/{SM_SESSION_ID}
    ↓
If session has last_handoff_path set → reads handoff doc content
    ↓
Outputs hookSpecificOutput.additionalContext with handoff doc content
    ↓
Agent resumes with handoff context injected
```

**Dependency:** Recovery requires sm#196's `_execute_handoff` to persist the executed handoff path to `session.last_handoff_path` (a new field on the Session model). See the [sm#196 coordination note](#sm196-coordination) below.

### Thresholds

Based on empirical data (context_window_size=200K):

| Threshold | used_percentage | Tokens (~) | Action |
|-----------|----------------|------------|--------|
| Warning | 50% | 100K | Sequential reminder: consider handoff (one-shot per cycle) |
| Critical | 65% | 130K | Urgent: write handoff doc NOW (one-shot per cycle) |
| Compaction | 80-85% | ~160K | PreCompact fires — resets flags, notifies parent |
| Post-compaction | 55%–110K tokens typical | variable | Flags already reset by PreCompact; new cycle starts |

These are configurable via sm config. Default values are conservative.

### Implementation

#### 1. Status line script (`~/.claude/hooks/context_monitor.sh`)

```bash
#!/bin/bash
# Status line script — receives context JSON from Claude Code via stdin
INPUT=$(cat)

# Extract context data
SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty')
USED_PCT=$(echo "$INPUT" | jq -r '.context_window.used_percentage // 0')
TOTAL_INPUT=$(echo "$INPUT" | jq -r '.context_window.total_input_tokens // 0')
TOTAL_OUTPUT=$(echo "$INPUT" | jq -r '.context_window.total_output_tokens // 0')
CONTEXT_SIZE=$(echo "$INPUT" | jq -r '.context_window.context_window_size // 200000')

# Map Claude session_id to sm session_id via env var
SM_SESSION_ID="${CLAUDE_SESSION_MANAGER_ID}"

if [ -n "$SM_SESSION_ID" ] && [ "$USED_PCT" != "null" ] && [ "$USED_PCT" != "0" ]; then
  # Post to sm server (non-blocking, short timeout)
  # sm server only listens on 127.0.0.1 — this call is localhost-only (trusted)
  curl -s --max-time 0.5 -X POST http://localhost:8420/hooks/context-usage \
    -H "Content-Type: application/json" \
    -d "$(jq -n \
          --arg sid "$SM_SESSION_ID" \
          --argjson pct "$USED_PCT" \
          --argjson tin "$TOTAL_INPUT" \
          --argjson tout "$TOTAL_OUTPUT" \
          --argjson csz "$CONTEXT_SIZE" \
          '{session_id: $sid, used_percentage: $pct, total_input_tokens: $tin, total_output_tokens: $tout, context_window_size: $csz}')" \
    >/dev/null 2>&1 &
fi

# Output status line text (displayed in Claude Code TUI)
echo "${USED_PCT}% ctx"
```

#### 2. Settings.json configuration

**Merge** the following into `~/.claude/settings.json` (do not replace the entire file):

```json
{
  "statusLine": {
    "type": "command",
    "command": "~/.claude/hooks/context_monitor.sh"
  }
}
```

#### 3. PreCompact hook (merge into settings.json hooks)

**Merge** the following into the `hooks` section of `~/.claude/settings.json`:

```json
{
  "hooks": {
    "PreCompact": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "~/.claude/hooks/precompact_notify.sh"
          }
        ]
      }
    ]
  }
}
```

`precompact_notify.sh`:
```bash
#!/bin/bash
INPUT=$(cat)
SM_SESSION_ID="${CLAUDE_SESSION_MANAGER_ID}"
TRIGGER=$(echo "$INPUT" | jq -r '.trigger // "unknown"')

if [ -n "$SM_SESSION_ID" ]; then
  # sm server only listens on 127.0.0.1 — localhost-only trusted call
  curl -s --max-time 0.5 -X POST http://localhost:8420/hooks/context-usage \
    -H "Content-Type: application/json" \
    -d "$(jq -n --arg sid "$SM_SESSION_ID" --arg trig "$TRIGGER" \
          '{session_id: $sid, event: "compaction", trigger: $trig}')" \
    >/dev/null 2>&1
fi
exit 0
```

#### 4. SessionStart compact recovery hook

**Merge** into the `hooks` section of `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "compact",
        "hooks": [
          {
            "type": "command",
            "command": "~/.claude/hooks/post_compact_recovery.sh"
          }
        ]
      }
    ]
  }
}
```

`post_compact_recovery.sh`:
```bash
#!/bin/bash
INPUT=$(cat)
SM_SESSION_ID="${CLAUDE_SESSION_MANAGER_ID}"

if [ -z "$SM_SESSION_ID" ]; then
  exit 0
fi

# Query sm server for the last handoff path (set by sm#196 _execute_handoff)
# sm server only listens on 127.0.0.1 — localhost-only trusted call
HANDOFF_PATH=$(curl -s --max-time 2 http://localhost:8420/sessions/"$SM_SESSION_ID" \
  | jq -r '.last_handoff_path // empty')

# If a handoff doc was previously executed for this session, inject it as additional context
if [ -n "$HANDOFF_PATH" ] && [ -f "$HANDOFF_PATH" ]; then
  jq -n --rawfile ctx "$HANDOFF_PATH" '{
    hookSpecificOutput: {
      hookEventName: "SessionStart",
      additionalContext: $ctx
    }
  }'
fi
```

#### 5. Server endpoint (`/hooks/context-usage`)

```python
@app.post("/hooks/context-usage")
async def hook_context_usage(request: Request):
    data = await request.json()
    session_id = data.get("session_id")
    session = app.state.session_manager.get_session(session_id) if session_id else None
    if not session:
        return {"status": "unknown_session"}

    queue_mgr = app.state.session_manager.message_queue_manager

    # Handle compaction event (from PreCompact hook)
    if data.get("event") == "compaction":
        logger.warning(f"Compaction fired for {session.friendly_name or session_id} (trigger={data.get('trigger')})")
        # Reset one-shot flags here — PreCompact fires before context is refreshed,
        # so this is the reliable reset point for the next accumulation cycle.
        # Cannot rely on used_pct < warning_pct because post-compaction context
        # may land above the warning threshold (documented range: 55K–110K tokens,
        # warning at 50% = 100K — overlap is possible).
        session._context_warning_sent = False
        session._context_critical_sent = False
        # Notify parent session / user
        if session.parent_session_id and queue_mgr:
            msg = f"[sm context] Compaction fired for {session.friendly_name or session_id}. Context was lost."
            queue_mgr.queue_message(
                target_session_id=session.parent_session_id,
                text=msg,
                delivery_mode="sequential",
            )
        return {"status": "compaction_logged"}

    # Handle context usage update
    used_pct = data.get("used_percentage", 0)
    session.tokens_used = data.get("total_input_tokens", 0)

    config = app.state.config.get("context_monitor", {})
    warning_pct = config.get("warning_percentage", 50)
    critical_pct = config.get("critical_percentage", 65)

    if used_pct >= critical_pct:
        if not getattr(session, '_context_critical_sent', False):
            session._context_critical_sent = True
            if queue_mgr:
                msg = (
                    f"[sm context] Context at {used_pct}% — critically high. "
                    "Write your handoff doc NOW and run `sm handoff <path>`. "
                    "Compaction is imminent."
                )
                queue_mgr.queue_message(
                    target_session_id=session.id,
                    text=msg,
                    delivery_mode="urgent",
                )
    elif used_pct >= warning_pct:
        if not getattr(session, '_context_warning_sent', False):
            session._context_warning_sent = True
            if queue_mgr:
                total = data.get("total_input_tokens", 0)
                msg = (
                    f"[sm context] Context at {used_pct}% ({total:,} tokens). "
                    "Consider writing a handoff doc and running `sm handoff <path>`."
                )
                queue_mgr.queue_message(
                    target_session_id=session.id,
                    text=msg,
                    delivery_mode="sequential",
                )

    return {"status": "ok", "used_percentage": used_pct}
```

### Configuration

```yaml
context_monitor:
  enabled: true
  warning_percentage: 50       # used_percentage to send warning
  critical_percentage: 65      # used_percentage to send urgent handoff
```

## sm#196 Coordination

### `last_handoff_path` field

The recovery hook (Layer 3) needs to find the handoff doc after compaction fires. This requires coordination with sm#196 and an explicit server update in sm#203. Three changes are required:

**1. Session model** (`src/models.py`) — sm#196 or sm#203:

Add to the `Session` dataclass/model:
```python
last_handoff_path: Optional[str] = None  # Last successfully executed handoff doc path (#196/#203)
```

**2. `_execute_handoff` success path** (`src/message_queue.py`) — sm#196:

After successful execution, persist the path and reset context flags:
```python
session.last_handoff_path = file_path
self.session_manager._save_state()
# Re-arm context monitor flags for the new cycle
session._context_warning_sent = False
session._context_critical_sent = False
```

**3. `SessionResponse` and `GET /sessions/{id}` mapping** (`src/server.py`) — sm#203:

`GET /sessions/{session_id}` uses an explicit `SessionResponse` Pydantic model with fixed fields (lines 76–89). New model fields are **not** automatically serialized — each must be explicitly added.

Add to `SessionResponse`:
```python
last_handoff_path: Optional[str] = None
```

Add to every `SessionResponse(...)` constructor call in `server.py` (create, get, update, list endpoints):
```python
last_handoff_path=session.last_handoff_path,
```

Without this, the recovery script's `jq -r '.last_handoff_path // empty'` call will always return empty, silently breaking recovery.

## Integration Points

| Feature | Integration |
|---------|-------------|
| sm#196 (sm handoff) | This ticket provides the trigger; sm#196 provides the mechanism. Agent writes handoff doc, calls `sm handoff`. sm#196 must persist `last_handoff_path` on Session. sm#203 must add `last_handoff_path` to `SessionResponse` and all `GET /sessions` mappings. |
| sm#188 (sm remind) | Context warnings use the same `queue_message` delivery mechanism as periodic reminders. |
| Session.tokens_used | Already exists in model. Populated with real data from status line. |
| Session.transcript_path | Already stored. Available in all hooks. |
| PreCompact + SessionStart hooks | New hooks added to settings.json. |

## Security: Localhost Trust Boundary

All hook scripts POST to `http://localhost:8420`. This is safe because:
- sm server binds to `127.0.0.1` by default (not `0.0.0.0`)
- Only local processes can reach the server
- No authentication is required for hook endpoints, consistent with existing hook design

If sm is reconfigured to listen on a non-loopback interface, hook scripts should add appropriate authentication.

## What This Does NOT Do

- **Force-handoff agents.** The agent controls when and how to handoff.
- **Block compaction.** PreCompact hook cannot block — exit code 2 only shows stderr.
- **Work for Codex sessions.** Codex has a different architecture. Claude Code only.
- **Replace sm#196.** This ticket provides the trigger; sm#196 provides the handoff mechanism.

## Test Plan

1. **Unit test:** Mock status line JSON input. Verify `context_monitor.sh` extracts correct fields and POSTs to sm with `jq -n`-assembled JSON.
2. **Unit test:** Verify server endpoint one-shot flag logic:
   - Warning flag fires once at 50%, suppressed on repeat calls at same percentage
   - Critical flag fires once at 65%, suppressed on repeat
   - Both flags reset on `event == "compaction"` — even if post-compaction `used_pct` would be above `warning_pct` (test with simulated 55% post-compaction)
   - Flags reset when `_execute_handoff` runs (sm#196 coordination)
   - **Anti-regression:** Flags NOT reset by `used_pct < warning_pct` in status line updates — verify this path no longer exists
3. **Integration test:** Configure status line, run a session, verify `tokens_used` updates on Session model.
4. **Manual test:** Run a long session and observe:
   - Status line shows context percentage in TUI
   - Warning message delivered at 50% (once, not on every update)
   - Critical message delivered at 65% (once)
   - PreCompact notification logged if compaction fires
5. **Edge cases:**
   - `used_percentage` is null (before first API call)
   - Session not tracked by sm (no `CLAUDE_SESSION_MANAGER_ID`)
   - sm server down (status line script should not block — async curl with 0.5s timeout)
   - Multiple rapid status line updates (one-shot flags prevent duplicate messages)
   - Post-compaction recovery with `last_handoff_path` set (re-injects doc)
   - Post-compaction recovery with no prior handoff (no output, exits cleanly)

## Ticket Classification

Single ticket. One engineer can implement:
1. Status line script + settings.json config
2. PreCompact + SessionStart hook scripts
3. Server `/hooks/context-usage` endpoint
4. Threshold checking and message delivery
5. One-shot flag reset logic

The sm#196 handoff mechanism (what happens when the agent runs `sm handoff`) is a separate ticket. Required coordination work is split:
- sm#196: add `last_handoff_path` to `Session` model, set in `_execute_handoff`, reset context flags
- sm#203: add `last_handoff_path` to `SessionResponse` (explicit Pydantic model) and all `/sessions` response constructors

Both are small additions (1–5 lines each) documented under [sm#196 Coordination](#sm196-coordination) above.
