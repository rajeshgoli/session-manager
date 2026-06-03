#!/bin/bash
# Log tool usage to session manager API
# Called by Claude Code PreToolUse/PostToolUse hooks

FALLBACK_DIR="${HOME}/.local/share/claude-sessions"
FALLBACK_FILE="${FALLBACK_DIR}/tool_usage_fallback.jsonl"
HOOK_BASE_URL="${SM_HOOK_BASE_URL:-http://localhost:8420}"
HOOK_URL="${SM_TOOL_USE_HOOK_URL:-${HOOK_BASE_URL%/}/hooks/tool-use}"

INPUT=$(cat)

# Inject session ID if available
if [ -n "$CLAUDE_SESSION_MANAGER_ID" ]; then
  INPUT=$(echo "$INPUT" | jq -c --arg sid "$CLAUDE_SESSION_MANAGER_ID" '. + {session_manager_id: $sid}')
fi

# Post to server with short timeout, fallback to file
# Using very short timeouts (0.5s connect, 1s total) to avoid blocking
CURL_HEADERS=(-H "Content-Type: application/json")
if [ -n "$SM_HOOK_SECRET" ]; then
  CURL_HEADERS+=(-H "X-SM-Hook-Secret: $SM_HOOK_SECRET")
fi

if ! curl -s --max-time 1 --connect-timeout 0.5 -X POST "$HOOK_URL" \
    "${CURL_HEADERS[@]}" \
    -d "$INPUT" >/dev/null 2>&1; then
  # Fallback: write to file if server unavailable
  mkdir -p "$FALLBACK_DIR"
  echo "$INPUT" >> "$FALLBACK_FILE"
fi

exit 0
