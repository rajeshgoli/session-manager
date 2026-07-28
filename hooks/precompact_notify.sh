#!/bin/bash
# PreCompact hook for the sm context monitor (sm#203).
#
# Compaction is context loss, so the server processes this even for sessions
# that never opted into usage reporting (#210): it re-arms the one-shot warning
# latches for the next accumulation cycle and tells the monitor (or, failing
# that, the parent) that the agent's context was discarded.
#
# PreCompact is the reliable reset point precisely because it fires *before* the
# context is refreshed. Waiting for usage to fall back under the warning
# threshold would not work — post-compaction context can land above it.

set -u

HOOK_BASE_URL="${SM_HOOK_BASE_URL:-http://localhost:8420}"
HOOK_URL="${SM_CONTEXT_HOOK_URL:-${HOOK_BASE_URL%/}/hooks/context-usage}"

INPUT=$(cat)

if [ -z "${CLAUDE_SESSION_MANAGER_ID:-}" ] || ! command -v jq >/dev/null 2>&1; then
  exit 0
fi

TRIGGER=$(printf '%s' "$INPUT" | jq -r '.trigger // "unknown"' 2>/dev/null || echo "unknown")

BODY=$(jq -c -n --arg sid "$CLAUDE_SESSION_MANAGER_ID" --arg trigger "$TRIGGER" \
  '{session_id: $sid, event: "compaction", trigger: $trigger}' 2>/dev/null) || exit 0

HEADERS=(-H "Content-Type: application/json")
if [ -n "${SM_HOOK_SECRET:-}" ]; then
  HEADERS+=(-H "X-SM-Hook-Secret: $SM_HOOK_SECRET")
fi

# Compaction is a one-per-cycle event rather than a per-render one, so this is
# worth waiting on briefly: losing it costs the parent its only notice that the
# child's context was discarded.
curl -s --max-time 3 --connect-timeout 2 -X POST "$HOOK_URL" \
  "${HEADERS[@]}" -d "$BODY" >/dev/null 2>&1

exit 0
