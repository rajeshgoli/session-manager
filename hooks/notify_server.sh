#!/bin/bash
# Hook script that posts Claude Code Stop/Notification events to Session Manager.
# Always exits successfully - if the server is down or session not found, log it
# and return 0 so Claude does not surface a hook failure.

HOOK_URL="${SM_HOOK_URL:-http://localhost:8420/hooks/claude}"
HOOK_LOG_PATH="${CLAUDE_HOOK_LOG_PATH:-/tmp/claude-hooks.log}"

# Claude writes a single JSON line for the hook payload, but it may keep stdin
# open until the broader turn finishes (for example while background task
# notifications are still pending). Read just the first line we need instead of
# waiting for EOF, otherwise the Stop hook can appear hung for minutes.
INPUT=""
IFS= read -r INPUT || [ -n "$INPUT" ]

if [ -z "$INPUT" ]; then
  exit 0
fi

# If CLAUDE_SESSION_MANAGER_ID is set (from environment), inject it into the payload.
if [ -n "$CLAUDE_SESSION_MANAGER_ID" ]; then
  INPUT=$(echo "$INPUT" | jq -c --arg sid "$CLAUDE_SESSION_MANAGER_ID" '. + {session_manager_id: $sid}')
fi

# Post to local server asynchronously (don't block Claude).
# Close inherited FDs so Claude Code does not keep waiting on the background curl.
(
  echo "$(date): Hook called" >> "$HOOK_LOG_PATH"
  echo "$INPUT" >> "$HOOK_LOG_PATH"

  RESPONSE=$(curl -s --max-time 5 --connect-timeout 2 -X POST "$HOOK_URL" \
    -H "Content-Type: application/json" \
    -d "$INPUT" 2>&1)

  echo "$RESPONSE" >> "$HOOK_LOG_PATH"
  echo "---" >> "$HOOK_LOG_PATH"
) </dev/null >/dev/null 2>&1 &
disown 2>/dev/null

exit 0
