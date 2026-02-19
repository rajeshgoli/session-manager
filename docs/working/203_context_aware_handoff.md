# sm#203: Context-Aware Handoff Triggering

## Problem

Long-running agents (especially EM) accumulate context until compaction fires. Compaction is lossy, unpredictable, and the agent can't see its own context usage. The user had to manually intervene during session 3. We need sm to detect approaching context limits and trigger proactive handoff (via sm#196).

## Investigation Summary

Five approaches were tested empirically. Only one is viable.

### Approach 1: Official Hook or API — NOT AVAILABLE

Tested exhaustively:

| Signal | Result |
|--------|--------|
| Hook payload fields | Only: `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `tool_name`, `tool_input`, `tool_response`, `tool_use_id`. **No context/token fields.** |
| Environment variables | Only `CLAUDECODE=1`, `CLAUDE_CODE_ENTRYPOINT=cli`, `CLAUDE_SESSION_MANAGER_ID`. **No context info.** |
| CLI command | `claude --help` exposes no context query command. |
| Compaction hook event | Not available. Only PreToolUse, PostToolUse, Stop, Notification, SubagentStart, SubagentStop. |

**Verdict:** Claude Code exposes no official mechanism for context usage.

### Approach 2: Tmux Status Bar Scraping — FRAGILE, NOT RECOMMENDED

Tested with `tmux capture-pane -p` across multiple active sessions:

- Per-turn token counts appear (e.g., "↓ 1.9k tokens") but this is **output tokens for the current turn**, not total context.
- Context percentage does NOT reliably appear in the TUI. It only shows when Claude Code chooses to display it (inconsistent).
- Requires ANSI escape sequence parsing (`-e` flag).
- Layout changes across Claude Code versions would break parsing.

**Verdict:** Unreliable. Cannot be used as a primary signal.

### Approach 3: Transcript File Monitoring — RECOMMENDED

**Key discovery:** The transcript JSONL file (path available in hook payload as `transcript_path`) contains `assistant` records with full API usage data:

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

**Total context size = `input_tokens` + `cache_creation_input_tokens` + `cache_read_input_tokens`**

This is the full prompt/context sent to the API on each turn.

#### Compaction threshold data (from 30+ real sessions)

| Metric | Value |
|--------|-------|
| Typical compaction threshold | 100K–170K tokens |
| Most common range | 150K–160K tokens |
| Post-compaction size | 55K–110K tokens |
| Session with 6 compactions | Threshold varied between 106K–144K |
| Session with 29 compactions | Max 154K per cycle |

#### Compaction detection

Compaction creates a `summary` type record in the transcript:

```json
{
  "type": "summary",
  "summary": "Debugging double warmup issue with trace script",
  "leafUuid": "685843a1-02c1-4c2c-a11d-14f54c13acd8"
}
```

**Verdict:** Reliable, deterministic, based on actual API token counts. The data already exists — we just need to read it.

### Approach 4: Heuristic (dispatch count / elapsed time) — USEFUL AS FALLBACK

Session data analysis (top sessions by tool call count):

| Session | Tool Calls | Duration (min) | Max Tokens |
|---------|-----------|----------------|------------|
| em-1615 | 69,362 | 14,358 | — |
| scout-gt-nav-status | 54,757 | 28,461 | — |
| architect-1624 | 44,678 | 12,855 | — |

Tool call count varies too widely to set a reliable threshold. But as a fallback (when transcript parsing fails), counting PostToolUse events per session is reasonable.

**Verdict:** Useful as defense-in-depth, not as primary signal.

### Approach 5: Defensive Handoff on Compaction — LAST RESORT

Compaction IS detectable via `summary` records in the transcript. But by the time compaction fires, context is already lost. This approach is reactive, not proactive.

**Verdict:** Keep as last-resort safety net, but prevent compaction rather than detect it.

## Recommended Design: Transcript-Based Context Monitor

### Architecture

```
PostToolUse hook fires
    ↓
Existing log_tool_use.sh forwards to sm server
    ↓
sm server reads transcript_path from hook payload
    ↓
Parse last assistant record for usage.{input_tokens, cache_creation_input_tokens, cache_read_input_tokens}
    ↓
Compute total_context = sum of above
    ↓
Store in Session.tokens_used (field already exists)
    ↓
If total_context > warning_threshold → emit context_warning event
If total_context > critical_threshold → trigger handoff (sm#196)
```

### Thresholds

Based on empirical data:

| Threshold | Tokens | Rationale |
|-----------|--------|-----------|
| Warning | 100K | ~60% of typical compaction point. Agent gets a reminder. |
| Critical | 130K | ~80% of typical compaction point. Trigger handoff. |

These are configurable via sm config. Default values are conservative — better to handoff early than lose context.

### Implementation

#### 1. Extend the PostToolUse handler in `server.py`

After logging the tool use, check context usage:

```python
# In hook_tool_use handler, after logging:
if hook_type == "PostToolUse" and session:
    transcript_path = data.get("transcript_path")
    if transcript_path:
        tokens = await read_transcript_tokens(transcript_path)
        if tokens:
            session.tokens_used = tokens
            # Check thresholds
            config = app.state.config.get("context_monitor", {})
            warning = config.get("warning_threshold", 100_000)
            critical = config.get("critical_threshold", 130_000)

            if tokens >= critical:
                await trigger_context_handoff(session)
            elif tokens >= warning:
                await send_context_warning(session, tokens)
```

#### 2. Transcript reader function

```python
async def read_transcript_tokens(transcript_path: str) -> Optional[int]:
    """Read the last assistant record from transcript and return total context tokens."""
    try:
        # Read file from end to find last assistant record efficiently
        # (JSONL files can be large — 400MB+ for long sessions)
        path = Path(transcript_path)
        if not path.exists():
            return None

        # Read last 100KB (sufficient to find last assistant record)
        size = path.stat().st_size
        read_start = max(0, size - 100_000)

        with open(path, 'r') as f:
            f.seek(read_start)
            if read_start > 0:
                f.readline()  # Skip partial line

            last_assistant = None
            for line in f:
                try:
                    data = json.loads(line.strip())
                    if data.get('type') == 'assistant':
                        usage = data.get('message', {}).get('usage', {})
                        if usage:
                            last_assistant = usage
                except json.JSONDecodeError:
                    continue

        if last_assistant:
            return (
                last_assistant.get('input_tokens', 0)
                + last_assistant.get('cache_creation_input_tokens', 0)
                + last_assistant.get('cache_read_input_tokens', 0)
            )
        return None
    except Exception as e:
        logger.error(f"Failed to read transcript tokens: {e}")
        return None
```

#### 3. Context warning (integrates with sm#188 remind)

When tokens exceed the warning threshold, send a reminder to the agent:

```python
async def send_context_warning(session, tokens):
    """Send context usage warning to agent."""
    pct = int(tokens / 200_000 * 100)  # Approximate % of model context
    msg = f"[sm context] Context at {tokens:,} tokens (~{pct}%). Consider writing a handoff doc and running `sm handoff <path>`."
    # Deliver as sequential (non-interrupting) reminder
    await message_queue.deliver(session.id, msg, mode=DeliveryMode.SEQUENTIAL)
```

#### 4. Critical handoff trigger (uses sm#196)

When tokens exceed the critical threshold:

```python
async def trigger_context_handoff(session):
    """Trigger forced handoff when context is critical."""
    msg = (
        "[sm context] Context critically high. "
        "Write your handoff doc NOW and run `sm handoff <path>`. "
        "Compaction is imminent."
    )
    # Deliver as urgent (interrupting)
    await message_queue.deliver(session.id, msg, mode=DeliveryMode.URGENT)
```

Note: We do NOT force-handoff the agent. We send an urgent message telling it to handoff. The agent controls its own handoff timing and content (per sm#196 design). Force-handoff would risk corrupting in-flight work.

### Fallback: Tool call count heuristic

If transcript parsing fails (file doesn't exist, permission error, etc.), fall back to counting PostToolUse events:

```python
# In Session model, tools_used dict already tracks counts
total_tool_calls = sum(session.tools_used.values())
if total_tool_calls > 500:  # Configurable
    # Send warning via same mechanism
```

### Compaction detection (safety net)

Monitor transcripts for `summary` records. If one appears, it means compaction already fired despite our warnings. Log this as a failure case and alert:

```python
# In transcript reader, also check for summary records
if data.get('type') == 'summary':
    logger.warning(f"Compaction detected for session {session.id} — handoff was too late")
    # Could trigger crash-recovery-style restart with handoff doc
```

### Configuration

```yaml
context_monitor:
  enabled: true
  warning_threshold: 100000    # tokens
  critical_threshold: 130000   # tokens
  fallback_tool_count: 500     # tool calls before warning
  check_frequency: 5           # check every Nth PostToolUse (not every one)
```

`check_frequency` avoids reading the transcript on every single tool call. Every 5th PostToolUse is sufficient — context doesn't grow that fast between tool calls.

## Integration Points

| Feature | Integration |
|---------|-------------|
| sm#196 (sm handoff) | This ticket provides the trigger; sm#196 provides the mechanism. Agent writes handoff doc, calls `sm handoff`. |
| sm#188 (sm remind) | Context warnings use the same delivery mechanism as periodic reminders. |
| Session.tokens_used | Already exists in the model. This ticket populates it with real data. |
| Session.transcript_path | Already stored. Used to locate the JSONL file. |
| PostToolUse hook | Already fires and reaches the sm server. Just need to add transcript reading. |

## What This Does NOT Do

- **Force-handoff agents.** The agent controls when and how to handoff.
- **Prevent compaction.** If the agent ignores warnings, compaction will still fire.
- **Work for Codex sessions.** Codex has a different architecture. This is Claude Code only.
- **Add new hooks to Claude Code.** Uses only existing hook payloads and transcript files.

## Test Plan

1. **Unit test:** Mock transcript JSONL with known token counts. Verify `read_transcript_tokens()` returns correct values.
2. **Unit test:** Verify threshold comparison triggers correct event (warning vs critical vs none).
3. **Integration test:** Create a session, inject a mock transcript path, call PostToolUse handler, verify `Session.tokens_used` is updated.
4. **Manual test:** Run a long session and observe context warnings being delivered at the right time.
5. **Edge cases:**
   - Transcript file doesn't exist (session just started)
   - Transcript file is empty
   - Transcript file is very large (400MB+) — verify tail-reading performance
   - Multiple rapid PostToolUse events — verify check_frequency throttling
   - Session with no assistant records yet (only progress records)

## Ticket Classification

Single ticket. One engineer can implement the transcript reader + threshold checks + message delivery in one session. The sm#196 handoff mechanism is a separate ticket.
