#!/bin/bash
# SessionStart(source=compact) hook for the sm context monitor (sm#249).
#
# Runs once the compacted session is back up. Two jobs:
#   1. Acknowledge the compaction to the server, closing the cycle opened by
#      precompact_notify.sh.
#   2. Re-inject the session's last handoff doc as additional context, so the
#      work that was written down precisely because compaction was coming is
#      what the agent wakes up holding (sm#196).
#
# The handoff injection is the half that has to emit on stdout: Claude reads
# this hook's stdout as JSON, so nothing else may be printed.

set -u

HOOK_BASE_URL="${SM_HOOK_BASE_URL:-http://localhost:8420}"
HOOK_URL="${SM_CONTEXT_HOOK_URL:-${HOOK_BASE_URL%/}/hooks/context-usage}"
SESSION_URL_BASE="${SM_HOOK_BASE_URL:-http://localhost:8420}"

cat >/dev/null 2>&1

if [ -z "${CLAUDE_SESSION_MANAGER_ID:-}" ] || ! command -v jq >/dev/null 2>&1; then
  exit 0
fi

HEADERS=(-H "Content-Type: application/json")
if [ -n "${SM_HOOK_SECRET:-}" ]; then
  HEADERS+=(-H "X-SM-Hook-Secret: $SM_HOOK_SECRET")
fi

BODY=$(jq -c -n --arg sid "$CLAUDE_SESSION_MANAGER_ID" \
  '{session_id: $sid, event: "compaction_complete"}' 2>/dev/null) || BODY=""
if [ -n "$BODY" ]; then
  curl -s --max-time 3 --connect-timeout 2 -X POST "$HOOK_URL" \
    "${HEADERS[@]}" -d "$BODY" >/dev/null 2>&1
fi

HANDOFF_PATH=$(
  curl -s --max-time 3 --connect-timeout 2 \
    "${SESSION_URL_BASE%/}/sessions/$CLAUDE_SESSION_MANAGER_ID" 2>/dev/null |
    jq -r '.last_handoff_path // empty' 2>/dev/null
) || HANDOFF_PATH=""

if [ -n "$HANDOFF_PATH" ] && [ -f "$HANDOFF_PATH" ]; then
  jq -n --rawfile ctx "$HANDOFF_PATH" '{
    hookSpecificOutput: {
      hookEventName: "SessionStart",
      additionalContext: $ctx
    }
  }' 2>/dev/null
fi

exit 0
