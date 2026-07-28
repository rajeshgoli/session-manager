#!/bin/bash
# SessionStart(source=clear) hook for the sm context monitor (sm#203).
#
# Covers both the TUI `/clear` and `sm clear`. A manual clear starts a fresh
# accumulation cycle, so the server re-arms the one-shot warning latches and
# drops the queued-but-undelivered alerts that describe the context just thrown
# away (#241).

set -u

HOOK_BASE_URL="${SM_HOOK_BASE_URL:-http://localhost:8420}"
HOOK_URL="${SM_CONTEXT_HOOK_URL:-${HOOK_BASE_URL%/}/hooks/context-usage}"

# Drain stdin so Claude never blocks on an unread pipe, even though the payload
# carries nothing this hook needs.
cat >/dev/null 2>&1

if [ -z "${CLAUDE_SESSION_MANAGER_ID:-}" ] || ! command -v jq >/dev/null 2>&1; then
  exit 0
fi

# The stamp marks where the new accumulation cycle begins. It is taken on this
# host, and the status-line samples it is compared against are too, so a node
# whose clock trails the primary cannot have its fresh samples read as stale.
BODY=$(jq -c -n --arg sid "$CLAUDE_SESSION_MANAGER_ID" \
  'def stamp:
     now as $t | ($t | floor) as $s
     | ($s | strftime("%Y-%m-%dT%H:%M:%S"))
       + "." + (("000000" + ((($t - $s) * 1000000) | floor | tostring))[-6:])
       + "Z";
   {session_id: $sid, event: "context_reset", sm_hook_emitted_at: stamp}' 2>/dev/null) || exit 0

HEADERS=(-H "Content-Type: application/json")
if [ -n "${SM_HOOK_SECRET:-}" ]; then
  HEADERS+=(-H "X-SM-Hook-Secret: $SM_HOOK_SECRET")
fi

curl -s --max-time 3 --connect-timeout 2 -X POST "$HOOK_URL" \
  "${HEADERS[@]}" -d "$BODY" >/dev/null 2>&1

exit 0
