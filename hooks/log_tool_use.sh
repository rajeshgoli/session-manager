#!/bin/bash
# Log tool usage to session manager API
# Called by Claude Code PreToolUse/PostToolUse hooks

FALLBACK_DIR="${HOME}/.local/share/claude-sessions"
FALLBACK_FILE="${FALLBACK_DIR}/tool_usage_fallback.jsonl"

INPUT=$(cat)

# Inject session ID if available (use -c for compact single-line JSON)
if [ -n "$CLAUDE_SESSION_MANAGER_ID" ]; then
  INPUT=$(echo "$INPUT" | jq -c --arg sid "$CLAUDE_SESSION_MANAGER_ID" '. + {session_manager_id: $sid}')
fi

# Post to session manager with timeout protection (async - don't block Claude)
# Note: 'timeout' command doesn't exist on macOS, use curl's --max-time and --connect-timeout
# IMPORTANT: Close all inherited FDs so Claude Code doesn't wait for the background process
(
  if ! curl -s --max-time 5 --connect-timeout 2 -X POST http://localhost:8420/hooks/tool-use \
    -H "Content-Type: application/json" \
    -d "$INPUT" &>/dev/null; then
    # Fallback: append to local file if API fails
    mkdir -p "$FALLBACK_DIR"
    echo "$INPUT" >> "$FALLBACK_FILE"
  fi
) </dev/null >/dev/null 2>&1 &
disown 2>/dev/null

exit 0
