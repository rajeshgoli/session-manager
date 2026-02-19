# sm#203: Context-Aware Handoff Triggering

## Problem

Long-running agents (especially EM) accumulate context until compaction fires. Compaction is lossy, unpredictable, and the agent can't see its own context usage. The user had to manually intervene during session 3. We need sm to detect approaching context limits and trigger proactive handoff (via sm#196).

## Investigation Summary

Six approaches were evaluated. Three official Claude Code mechanisms were discovered that, combined, provide a complete solution.

### Approach 1: Status Line API — AVAILABLE, RECOMMENDED PRIMARY

Claude Code supports a `statusLine` setting in `~/.claude/settings.json` that runs a shell command and pipes full context window data via stdin as JSON:

```json
{
  "statusLine": {
    "type": "command",
    "command": "~/.claude/statusline.sh",
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

SessionStart hooks can inject `additionalContext` via JSON output — the documented pattern for "re-inject context after compaction."

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
If used_percentage > warning_pct → send context warning to agent
If used_percentage > critical_pct → send urgent handoff trigger
```

### Layer 2: PreCompact Hook (Safety Net — Last Chance)

If the agent ignores warnings and compaction is imminent:

```
PreCompact hook fires (trigger=auto)
    ↓
Hook script POSTs {session_id, event: "compaction_imminent"} to sm server
    ↓
sm logs warning: "Compaction triggered — handoff was too late"
    ↓
sm sends urgent notification to parent session / user
```

### Layer 3: SessionStart `compact` (Recovery — Post-Compaction)

If compaction fires despite warnings, recover gracefully:

```
SessionStart fires with source=compact
    ↓
Hook script checks if handoff doc exists for this session
    ↓
If yes: outputs additionalContext with handoff doc content
    ↓
Agent resumes with handoff context injected
```

### Thresholds

Based on empirical data (context_window_size=200K):

| Threshold | used_percentage | Tokens (~) | Action |
|-----------|----------------|------------|--------|
| Warning | 50% | 100K | Sequential reminder: consider handoff |
| Critical | 65% | 130K | Urgent: write handoff doc NOW |
| Compaction | 80-85% | ~160K | PreCompact fires — too late for clean handoff |

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
  curl -s --max-time 0.5 -X POST http://localhost:8420/hooks/context-usage \
    -H "Content-Type: application/json" \
    -d "{\"session_id\": \"$SM_SESSION_ID\", \"used_percentage\": $USED_PCT, \"total_input_tokens\": $TOTAL_INPUT, \"total_output_tokens\": $TOTAL_OUTPUT, \"context_window_size\": $CONTEXT_SIZE}" \
    >/dev/null 2>&1 &
fi

# Output status line text (displayed in Claude Code TUI)
echo "${USED_PCT}% ctx"
```

#### 2. Settings.json configuration

Add to `~/.claude/settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "~/.claude/hooks/context_monitor.sh"
  }
}
```

#### 3. PreCompact hook (add to settings.json hooks)

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
  curl -s --max-time 0.5 -X POST http://localhost:8420/hooks/context-usage \
    -H "Content-Type: application/json" \
    -d "{\"session_id\": \"$SM_SESSION_ID\", \"event\": \"compaction\", \"trigger\": \"$TRIGGER\"}" \
    >/dev/null 2>&1
fi
exit 0
```

#### 4. SessionStart compact recovery hook

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
HANDOFF_DOC="/tmp/claude-sessions/${SM_SESSION_ID}_handoff.md"

# If a handoff doc exists for this session, inject it as additional context
if [ -f "$HANDOFF_DOC" ]; then
  CONTENT=$(cat "$HANDOFF_DOC" | jq -Rs .)
  echo "{\"additionalContext\": $CONTENT}"
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

    # Handle compaction event
    if data.get("event") == "compaction":
        logger.warning(f"Compaction fired for {session.friendly_name or session_id} (trigger={data.get('trigger')})")
        # Notify parent session / user
        if session.parent_session_id:
            msg = f"[sm context] Compaction fired for {session.friendly_name or session_id}. Context was lost."
            await message_queue.deliver(session.parent_session_id, msg, mode=DeliveryMode.SEQUENTIAL)
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
            msg = (
                f"[sm context] Context at {used_pct}% — critically high. "
                "Write your handoff doc NOW and run `sm handoff <path>`. "
                "Compaction is imminent."
            )
            await message_queue.deliver(session.id, msg, mode=DeliveryMode.URGENT)
    elif used_pct >= warning_pct:
        if not getattr(session, '_context_warning_sent', False):
            session._context_warning_sent = True
            total = data.get("total_input_tokens", 0)
            msg = (
                f"[sm context] Context at {used_pct}% ({total:,} tokens). "
                "Consider writing a handoff doc and running `sm handoff <path>`."
            )
            await message_queue.deliver(session.id, msg, mode=DeliveryMode.SEQUENTIAL)

    return {"status": "ok", "used_percentage": used_pct}
```

### Configuration

```yaml
context_monitor:
  enabled: true
  warning_percentage: 50       # used_percentage to send warning
  critical_percentage: 65      # used_percentage to send urgent handoff
```

## Integration Points

| Feature | Integration |
|---------|-------------|
| sm#196 (sm handoff) | This ticket provides the trigger; sm#196 provides the mechanism. Agent writes handoff doc, calls `sm handoff`. |
| sm#188 (sm remind) | Context warnings use the same delivery mechanism as periodic reminders. |
| Session.tokens_used | Already exists in model. Populated with real data from status line. |
| Session.transcript_path | Already stored. Available in all hooks. |
| PreCompact + SessionStart hooks | New hooks added to settings.json. |

## What This Does NOT Do

- **Force-handoff agents.** The agent controls when and how to handoff.
- **Block compaction.** PreCompact hook cannot block — exit code 2 only shows stderr.
- **Work for Codex sessions.** Codex has a different architecture. Claude Code only.
- **Replace sm#196.** This ticket provides the trigger; sm#196 provides the handoff mechanism.

## Test Plan

1. **Unit test:** Mock status line JSON input. Verify context_monitor.sh extracts correct fields and POSTs to sm.
2. **Unit test:** Verify server endpoint threshold logic (warning at 50%, critical at 65%, debounce).
3. **Integration test:** Configure status line, run a session, verify tokens_used updates on Session model.
4. **Manual test:** Run a long session and observe:
   - Status line shows context percentage in TUI
   - Warning message delivered at 50%
   - Critical message delivered at 65%
   - PreCompact notification logged if compaction fires
5. **Edge cases:**
   - `used_percentage` is null (before first API call)
   - Session not tracked by sm (no CLAUDE_SESSION_MANAGER_ID)
   - sm server down (status line script should not block)
   - Multiple rapid status line updates (debounce in server)
   - Post-compaction recovery with and without handoff doc

## Ticket Classification

Single ticket. One engineer can implement:
1. Status line script + settings.json config
2. PreCompact + SessionStart hook scripts
3. Server `/hooks/context-usage` endpoint
4. Threshold checking and message delivery

The sm#196 handoff mechanism (what happens when the agent runs `sm handoff`) is a separate ticket.
