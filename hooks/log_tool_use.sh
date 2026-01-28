#!/bin/bash
# Log tool usage to session manager API
# Called by Claude Code PreToolUse/PostToolUse hooks

FALLBACK_DIR="${HOME}/.local/share/claude-sessions"
FALLBACK_FILE="${FALLBACK_DIR}/tool_usage_fallback.jsonl"

INPUT=$(cat)

# Inject session ID if available
if [ -n "$CLAUDE_SESSION_MANAGER_ID" ]; then
  INPUT=$(echo "$INPUT" | jq -c --arg sid "$CLAUDE_SESSION_MANAGER_ID" '. + {session_manager_id: $sid}')
fi

# Post to server with short timeout, fallback to file
# Using very short timeouts (0.5s connect, 1s total) to avoid blocking
if ! curl -s --max-time 1 --connect-timeout 0.5 -X POST http://localhost:8420/hooks/tool-use \
    -H "Content-Type: application/json" \
    -d "$INPUT" >/dev/null 2>&1; then
  # Fallback: write to file if server unavailable
  mkdir -p "$FALLBACK_DIR"
  echo "$INPUT" >> "$FALLBACK_FILE"
fi

exit 0
