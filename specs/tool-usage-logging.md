# Spec: Tool Usage Logging for Security Audit

**Status:** Draft
**Issue:** #26
**Author:** sessionmgr
**Created:** 2026-01-27

---

## Overview

Log all Claude Code tool usage to a local SQLite database for security auditing, analytics, and informing permission policies.

## Goals

1. **Security Audit:** Track what agents are doing, especially destructive operations
2. **Analytics:** Understand tool usage patterns across sessions
3. **Permission Policies:** Data-driven decisions on what requires approval vs auto-allow
4. **Debugging:** Trace agent behavior when things go wrong

## Non-Goals

- Real-time blocking of dangerous operations (future enhancement)
- Remote/cloud logging (local only for privacy)
- Token/cost tracking (separate concern)

---

## Architecture

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│  Claude Code    │────▶│  Hook Script     │────▶│ Session Manager │
│  (PreToolUse/   │     │  (log_tool.sh)   │     │ API Endpoint    │
│   PostToolUse)  │     │                  │     │                 │
└─────────────────┘     └──────────────────┘     └────────┬────────┘
                                                          │
                                                          ▼
                                                 ┌─────────────────┐
                                                 │  SQLite DB      │
                                                 │  tool_usage.db  │
                                                 └─────────────────┘
```

---

## Implementation

### Phase 1: Hook Script

**File:** `hooks/log_tool_use.sh`

```bash
#!/bin/bash
# Log tool usage to session manager API
# Called by Claude Code PreToolUse/PostToolUse hooks

FALLBACK_DIR="${HOME}/.local/share/claude-sessions"
FALLBACK_FILE="${FALLBACK_DIR}/tool_usage_fallback.jsonl"

INPUT=$(cat)

# Inject session ID if available
if [ -n "$CLAUDE_SESSION_MANAGER_ID" ]; then
  INPUT=$(echo "$INPUT" | jq --arg sid "$CLAUDE_SESSION_MANAGER_ID" '. + {session_manager_id: $sid}')
fi

# Post to session manager with timeout protection (async - don't block Claude)
# timeout 5s for process, --max-time 3s for curl response
(
  if ! timeout 5 curl -s --max-time 3 -X POST http://localhost:8420/hooks/tool-use \
    -H "Content-Type: application/json" \
    -d "$INPUT" &>/dev/null; then
    # Fallback: append to local file if API fails
    mkdir -p "$FALLBACK_DIR"
    echo "$INPUT" >> "$FALLBACK_FILE"
  fi
) &

exit 0
```

**Key decisions:**
- Fire and forget (async) - don't slow down Claude
- Always exit 0 - logging failure shouldn't break Claude
- Use existing `CLAUDE_SESSION_MANAGER_ID` env var
- Timeout protection: 5s process timeout + 3s curl max-time prevents zombie processes
- Fallback file: On API failure, logs to `~/.local/share/claude-sessions/tool_usage_fallback.jsonl`

---

### Phase 2: Claude Code Hook Configuration

**File:** `~/.claude/settings.json` (additions)

```json
{
  "hooks": {
    "PreToolUse": [{
      "hooks": [{
        "type": "command",
        "command": "/Users/rajesh/Desktop/automation/claude-session-manager/hooks/log_tool_use.sh"
      }]
    }],
    "PostToolUse": [{
      "hooks": [{
        "type": "command",
        "command": "/Users/rajesh/Desktop/automation/claude-session-manager/hooks/log_tool_use.sh"
      }]
    }],
    "SubagentStart": [{
      "hooks": [{
        "type": "command",
        "command": "/Users/rajesh/Desktop/automation/claude-session-manager/hooks/log_tool_use.sh"
      }]
    }],
    "SubagentStop": [{
      "hooks": [{
        "type": "command",
        "command": "/Users/rajesh/Desktop/automation/claude-session-manager/hooks/log_tool_use.sh"
      }]
    }]
  }
}
```

**Hook payload structure (from Claude Code official docs):**

PreToolUse:
```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "default",
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": {
    "command": "git push origin main",
    "description": "Push changes to remote",
    "timeout": 120000
  },
  "tool_use_id": "toolu_01ABC123..."
}
```

PostToolUse:
```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "default",
  "hook_event_name": "PostToolUse",
  "tool_name": "Bash",
  "tool_input": {
    "command": "git push origin main"
  },
  "tool_response": {
    "stdout": "...",
    "stderr": "...",
    "exitCode": 0
  },
  "tool_use_id": "toolu_01ABC123..."
}
```

**Key fields:**
- `tool_use_id`: Unique ID for correlating PreToolUse and PostToolUse events
- `session_id`: Claude Code's internal session ID (different from our `CLAUDE_SESSION_MANAGER_ID`)
- `hook_event_name`: The hook type (PreToolUse, PostToolUse, etc.)
- `tool_response`: Only present in PostToolUse (note: camelCase `exitCode` not `exit_code`)

**SubagentStart payload (for tracking subagent context):**
```json
{
  "session_id": "abc123",
  "hook_event_name": "SubagentStart",
  "agent_id": "agent-def456",
  "agent_type": "Explore"
}
```

Tool calls made by subagents can be attributed using the `agent_id` field.

---

### Phase 3: API Endpoint

**File:** `src/server.py`

```python
@router.post("/hooks/tool-use")
async def hook_tool_use(request: Request):
    """
    Receive tool usage events from Claude Code hooks.
    """
    data = await request.json()

    # Our session ID (injected by hook script)
    session_manager_id = data.get("session_manager_id")

    # Claude Code's native fields
    claude_session_id = data.get("session_id")  # Claude's internal ID
    hook_type = data.get("hook_event_name")  # PreToolUse or PostToolUse
    tool_name = data.get("tool_name")
    tool_input = data.get("tool_input", {})
    tool_response = data.get("tool_response")  # Only for PostToolUse
    tool_use_id = data.get("tool_use_id")  # For Pre/Post correlation
    cwd = data.get("cwd")  # Working directory

    # Subagent context (if present)
    agent_id = data.get("agent_id")  # From SubagentStart context

    # Get session info if available
    session = None
    if session_manager_id:
        session_manager = request.app.state.session_manager
        session = session_manager.get_session(session_manager_id)

    # Log to database
    tool_logger = request.app.state.tool_logger
    await tool_logger.log(
        session_id=session_manager_id,
        claude_session_id=claude_session_id,
        session_name=session.friendly_name if session else None,
        parent_session_id=session.parent_session_id if session else None,
        hook_type=hook_type,
        tool_name=tool_name,
        tool_input=tool_input,
        tool_response=tool_response,
        tool_use_id=tool_use_id,
        cwd=cwd,
        agent_id=agent_id,
    )

    return {"status": "logged"}
```

---

### Phase 4: Database Schema

**File:** `src/tool_logger.py`

```python
import sqlite3
import json
import re
from datetime import datetime
from pathlib import Path
from typing import Optional
import logging

logger = logging.getLogger(__name__)

# Patterns for sensitive/destructive operations
DESTRUCTIVE_PATTERNS = [
    # Git operations
    (r"git\s+push.*(?:main|master)", "git_push_main"),
    (r"git\s+push\s+--force", "git_push_force"),
    (r"git\s+reset\s+--hard", "git_reset_hard"),
    (r"git\s+branch\s+-[dD]", "git_branch_delete"),

    # File operations
    (r"rm\s+-rf?\s+/", "rm_root"),
    (r"rm\s+-rf", "rm_recursive"),
    (r"chmod\s+777", "chmod_777"),
    (r"chown", "chown"),

    # Database operations
    (r"DROP\s+TABLE", "drop_table"),
    (r"DROP\s+DATABASE", "drop_database"),
    (r"DELETE\s+FROM.*WHERE\s+1\s*=\s*1", "delete_all"),
    (r"TRUNCATE", "truncate"),

    # Package management
    (r"npm\s+install\s+-g", "npm_global_install"),
    (r"pip\s+install", "pip_install"),
    (r"brew\s+install", "brew_install"),

    # System operations
    (r"sudo\s+", "sudo"),
    (r"systemctl\s+(?:stop|disable|restart)", "systemctl"),

    # Sensitive files
    (r"\.env", "env_file"),
    (r"credentials", "credentials_file"),
    (r"\.ssh", "ssh_file"),
    (r"id_rsa", "ssh_key"),
]

SENSITIVE_FILE_PATTERNS = [
    r"\.env",
    r"\.env\.\w+",
    r"credentials",
    r"secrets",
    r"\.ssh/",
    r"id_rsa",
    r"\.aws/",
    r"\.npmrc",
    r"\.pypirc",
]


class ToolLogger:
    """Logs tool usage to SQLite database."""

    def __init__(self, db_path: str = "~/.local/share/claude-sessions/tool_usage.db"):
        self.db_path = Path(db_path)
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._init_db()

    def _init_db(self):
        """Initialize database schema."""
        conn = sqlite3.connect(self.db_path)
        cursor = conn.cursor()

        cursor.execute("""
            CREATE TABLE IF NOT EXISTS tool_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,

                -- Session info (ours)
                session_id TEXT,              -- Our CLAUDE_SESSION_MANAGER_ID
                session_name TEXT,
                parent_session_id TEXT,

                -- Session info (Claude's native)
                claude_session_id TEXT,       -- Claude Code's internal session ID
                tool_use_id TEXT,             -- For correlating PreToolUse/PostToolUse
                cwd TEXT,                     -- Working directory at time of call
                project_name TEXT,            -- Derived from cwd (last path component)
                agent_id TEXT,                -- Subagent ID if this is a subagent call

                -- Hook info
                hook_type TEXT NOT NULL,      -- PreToolUse or PostToolUse

                -- Tool info
                tool_name TEXT NOT NULL,
                tool_input TEXT,              -- JSON
                tool_response TEXT,           -- JSON (PostToolUse only)

                -- Derived fields
                is_destructive BOOLEAN DEFAULT 0,
                destructive_type TEXT,        -- e.g., "git_push_main", "rm_recursive"
                is_sensitive_file BOOLEAN DEFAULT 0,
                target_file TEXT,             -- For file operations
                bash_command TEXT,            -- For Bash tool
                exit_code INTEGER             -- For Bash PostToolUse
            )
        """)

        # Indexes for common queries
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_session ON tool_usage(session_id)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_tool ON tool_usage(tool_name)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_destructive ON tool_usage(is_destructive)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_timestamp ON tool_usage(timestamp)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_hook_type ON tool_usage(hook_type)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_tool_use_id ON tool_usage(tool_use_id)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_agent_id ON tool_usage(agent_id)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_project_name ON tool_usage(project_name)")

        conn.commit()
        conn.close()

    def _detect_destructive(self, tool_name: str, tool_input: dict) -> tuple[bool, Optional[str]]:
        """Detect if operation is destructive."""
        text_to_check = ""

        if tool_name == "Bash":
            text_to_check = tool_input.get("command", "")
        elif tool_name in ("Write", "Edit", "Read"):
            text_to_check = tool_input.get("file_path", "")

        for pattern, dtype in DESTRUCTIVE_PATTERNS:
            if re.search(pattern, text_to_check, re.IGNORECASE):
                return True, dtype

        return False, None

    def _detect_sensitive_file(self, tool_name: str, tool_input: dict) -> tuple[bool, Optional[str]]:
        """Detect if operation involves sensitive files."""
        file_path = None

        if tool_name in ("Write", "Edit", "Read"):
            file_path = tool_input.get("file_path", "")
        elif tool_name == "Bash":
            # Try to extract file paths from command
            command = tool_input.get("command", "")
            file_path = command  # Check whole command

        if file_path:
            for pattern in SENSITIVE_FILE_PATTERNS:
                if re.search(pattern, file_path, re.IGNORECASE):
                    return True, file_path

        return False, None

    async def log(
        self,
        session_id: Optional[str],
        claude_session_id: Optional[str],
        session_name: Optional[str],
        parent_session_id: Optional[str],
        hook_type: str,
        tool_name: str,
        tool_input: dict,
        tool_response: Optional[dict] = None,
        tool_use_id: Optional[str] = None,
        cwd: Optional[str] = None,
        agent_id: Optional[str] = None,
    ):
        """Log a tool usage event."""
        try:
            # Detect destructive operations
            is_destructive, destructive_type = self._detect_destructive(tool_name, tool_input)

            # Detect sensitive file access
            is_sensitive, target_file = self._detect_sensitive_file(tool_name, tool_input)

            # Extract bash command
            bash_command = None
            if tool_name == "Bash":
                bash_command = tool_input.get("command")

            # Extract exit code (note: Claude uses camelCase "exitCode")
            exit_code = None
            if tool_response and tool_name == "Bash":
                exit_code = tool_response.get("exitCode")

            # Extract target file for file operations
            if tool_name in ("Write", "Edit", "Read") and not target_file:
                target_file = tool_input.get("file_path")

            # Derive project name from cwd (last path component)
            project_name = None
            if cwd:
                project_name = Path(cwd).name

            conn = sqlite3.connect(self.db_path)
            cursor = conn.cursor()

            cursor.execute("""
                INSERT INTO tool_usage (
                    session_id, claude_session_id, session_name, parent_session_id,
                    tool_use_id, cwd, project_name, agent_id,
                    hook_type, tool_name, tool_input, tool_response,
                    is_destructive, destructive_type, is_sensitive_file,
                    target_file, bash_command, exit_code
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """, (
                session_id, claude_session_id, session_name, parent_session_id,
                tool_use_id, cwd, project_name, agent_id,
                hook_type, tool_name,
                json.dumps(tool_input) if tool_input else None,
                json.dumps(tool_response) if tool_response else None,
                is_destructive, destructive_type, is_sensitive,
                target_file, bash_command, exit_code,
            ))

            conn.commit()
            conn.close()

            # Log warning for destructive operations
            if is_destructive:
                logger.warning(
                    f"Destructive operation detected: {destructive_type} "
                    f"by session {session_name or session_id}"
                )

        except Exception as e:
            logger.error(f"Failed to log tool usage: {e}")
```

---

### Phase 5: CLI Commands

**File:** `src/cli/commands.py` (additions)

```python
def cmd_tools(client: SessionManagerClient, session_id: Optional[str] = None) -> int:
    """
    Show tool usage statistics.

    Args:
        client: API client
        session_id: Optional session to filter by

    Exit codes:
        0: Success
        1: Error
    """
    # Query stats from API
    stats = client.get_tool_stats(session_id)

    if stats is None:
        print("Error: Could not fetch tool stats", file=sys.stderr)
        return 1

    print("Tool Usage Statistics")
    print("=" * 50)
    print()

    # Most used tools
    print("Most Used Tools:")
    for tool, count in stats.get("by_tool", [])[:10]:
        print(f"  {tool}: {count}")

    print()

    # Destructive operations
    destructive = stats.get("destructive", [])
    if destructive:
        print("Destructive Operations:")
        for op in destructive[:10]:
            print(f"  [{op['timestamp']}] {op['session_name']}: {op['destructive_type']}")
            if op.get('bash_command'):
                print(f"    Command: {op['bash_command'][:80]}...")
    else:
        print("No destructive operations recorded.")

    return 0


def cmd_tools_destructive(client: SessionManagerClient) -> int:
    """Show only destructive operations."""
    # ...


def cmd_tools_session(client: SessionManagerClient, session_id: str) -> int:
    """Show tool usage for a specific session."""
    # ...
```

**CLI additions:**
```bash
sm tools                    # Overall stats
sm tools --destructive      # Only destructive ops
sm tools --session abc123   # Filter by session
sm tools --since 1h         # Last hour
sm tools --export csv       # Export to CSV
```

---

## Edge Cases & Considerations

### 1. High Volume Logging

**Problem:** Busy agents may generate thousands of tool calls.

**Solution:** Log 100% - database is cheap, filter post-hoc.
- Async writes (fire-and-forget from hook)
- 30-day retention with auto-cleanup
- Add batching later if performance becomes an issue

### 2. Hook Failures

**Problem:** Hook script fails (curl timeout, API down, etc.)

**Solution:** ✅ Implemented in hook script:
- Always exit 0 - don't block Claude
- Timeout protection (5s process + 3s curl) prevents zombie processes
- Fallback file: `~/.local/share/claude-sessions/tool_usage_fallback.jsonl`
- On API failure, logs are appended to fallback file for later processing

### 3. Missing Session ID

**Problem:** `CLAUDE_SESSION_MANAGER_ID` not set (manual Claude session).

**Solutions:**
- Log with `session_id = NULL`
- Try to infer from tmux session name
- Skip logging entirely

**Recommendation:** Log with NULL. Still valuable for security audit.

### 4. Sensitive Data in Logs

**Problem:** Tool inputs may contain secrets, passwords, API keys.

**Solutions:**
- Redact known patterns (API_KEY=xxx → API_KEY=****)
- Don't log tool_input for certain tools
- Encrypt database
- Configurable redaction patterns

**Recommendation:** Basic redaction for common patterns. Document risk.

### 5. Database Size Growth

**Problem:** Unbounded growth over time.

**Solutions:**
- Auto-cleanup: DELETE WHERE timestamp < NOW() - 30 days
- Separate tables by month
- Archive to compressed files
- Configurable retention period

**Recommendation:** 30-day default retention, configurable.

### 6. PreToolUse vs PostToolUse Correlation

**Problem:** Matching Pre and Post events for same tool call.

**Solution:** ✅ Claude Code provides `tool_use_id` in both hooks (verified from official docs).

**Implementation:** Store `tool_use_id` in database. Query to correlate:
```sql
SELECT pre.*, post.tool_response, post.exit_code
FROM tool_usage pre
JOIN tool_usage post ON pre.tool_use_id = post.tool_use_id
WHERE pre.hook_type = 'PreToolUse' AND post.hook_type = 'PostToolUse';
```

### 7. Subagent Tool Usage

**Problem:** Child agents' tool usage should be attributable.

**Solutions:**
- Use `parent_session_id` field
- Query with recursive CTEs for full hierarchy
- Aggregate stats at parent level

**Recommendation:** Already handled via `parent_session_id`.

### 8. Performance Impact

**Problem:** Logging adds latency to every tool call.

**Mitigation:**
- Async HTTP call (don't wait for response)
- Local Unix socket instead of HTTP (faster)
- In-memory buffer with async flush

**Recommendation:** Async HTTP is fine for MVP. Optimize if needed.

---

## Database Queries (Reference)

```sql
-- Tool usage summary
SELECT tool_name, COUNT(*) as count,
       SUM(CASE WHEN is_destructive THEN 1 ELSE 0 END) as destructive_count
FROM tool_usage
GROUP BY tool_name
ORDER BY count DESC;

-- Destructive operations in last 24h
SELECT timestamp, session_name, tool_name, destructive_type, bash_command
FROM tool_usage
WHERE is_destructive = 1
  AND timestamp > datetime('now', '-1 day')
ORDER BY timestamp DESC;

-- Sessions with most destructive operations
SELECT session_name, COUNT(*) as destructive_count
FROM tool_usage
WHERE is_destructive = 1
GROUP BY session_id
ORDER BY destructive_count DESC
LIMIT 10;

-- Git operations
SELECT timestamp, session_name, bash_command
FROM tool_usage
WHERE tool_name = 'Bash'
  AND bash_command LIKE '%git %'
ORDER BY timestamp DESC;

-- File modifications by path
SELECT target_file, COUNT(*) as edits
FROM tool_usage
WHERE tool_name IN ('Write', 'Edit')
GROUP BY target_file
ORDER BY edits DESC
LIMIT 20;

-- Sensitive file access
SELECT timestamp, session_name, tool_name, target_file
FROM tool_usage
WHERE is_sensitive_file = 1
ORDER BY timestamp DESC;

-- Tool usage timeline (hourly buckets)
SELECT strftime('%Y-%m-%d %H:00', timestamp) as hour,
       COUNT(*) as tool_calls
FROM tool_usage
GROUP BY hour
ORDER BY hour DESC
LIMIT 24;
```

---

## Configuration

**File:** `config.yaml` (additions)

```yaml
tool_logging:
  enabled: true
  db_path: "~/.local/share/claude-sessions/tool_usage.db"

  # Retention
  retention_days: 30
  cleanup_interval: 86400  # Daily cleanup

  # Redaction
  redact_secrets: true
  redaction_patterns:
    - "API_KEY=\\S+"
    - "PASSWORD=\\S+"
    - "SECRET=\\S+"
    - "TOKEN=\\S+"

  # Note: No sampling - log 100% of tool calls. Database is cheap, filter post-hoc.
```

---

## Implementation Phases

### Phase 1: Basic Logging (MVP)
- [ ] Hook script (`hooks/log_tool_use.sh`)
- [ ] API endpoint (`POST /hooks/tool-use`)
- [ ] ToolLogger class with SQLite
- [ ] Basic destructive detection
- [ ] Wire up in main.py

### Phase 2: CLI & Queries
- [ ] `sm tools` command
- [ ] `sm tools --destructive`
- [ ] `sm tools --session X`
- [ ] API endpoint for stats

### Phase 3: Configuration & Polish
- [ ] Config file options
- [ ] Secret redaction
- [ ] Log retention/cleanup
- [ ] Documentation

### Phase 4: Future Enhancements
- [ ] Real-time alerts for destructive ops
- [ ] Permission policies based on patterns
- [ ] Web dashboard for analytics
- [ ] Export to external systems

---

## Resolved Questions

1. **Hook payload format:** ✅ Verified from official docs. `tool_use_id` IS included for correlation. Full payload structure documented above.

2. **Permissions:** ✅ Always opt-in. Logging is always enabled, no consent needed.

3. **Subagent attribution:** ✅ Attribute to subagents via `agent_id` field. SubagentStart hook provides `agent_id` and `agent_type`. Tool calls within subagent context inherit this.

4. **Real-time blocking:** ✅ No. This is strictly a data collection exercise. No blocking functionality.

5. **Multi-machine:** ✅ Only one machine for now. Same machine assumption (localhost API).

---

## Testing Plan

1. **Unit tests:**
   - Destructive pattern detection
   - Sensitive file detection
   - Database operations

2. **Integration tests:**
   - Hook script receives and posts data
   - API endpoint stores correctly
   - Queries return expected results

3. **Manual testing:**
   - Run various tools, verify logging
   - Check destructive operations flagged
   - Verify no performance impact

---

## References

- [Claude Code Hooks Reference](https://code.claude.com/docs/en/hooks) - Official documentation
- [Hooks Guide](https://claude.com/blog/how-to-configure-hooks) - Anthropic blog
- SQLite best practices for append-heavy workloads
- Issue #26: Tool usage logging for security audit
