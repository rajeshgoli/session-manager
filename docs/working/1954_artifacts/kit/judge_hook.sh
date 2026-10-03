#!/bin/sh
# #1954 PreToolUse hook (memo K.3): forward Claude Code's JSON to the judge service on loopback.
# Never answers "ask". Any failure to reach the service or parse its reply is a deny.
OUT=$(curl -s --max-time 45 -H 'Content-Type: application/json' -H "X-Local-Agent: ${LOCAL_AGENT_ID:-unknown}" \
  --data-binary @- "http://127.0.0.1:${LOCAL_JUDGE_PORT:-8431}/decide" 2>/dev/null)
case "$OUT" in
  *'"permissionDecision"'*) printf '%s' "$OUT" ;;
  *) printf '%s' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"judge unavailable; retry this command in a minute"}}' ;;
esac
