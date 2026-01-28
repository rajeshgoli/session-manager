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
