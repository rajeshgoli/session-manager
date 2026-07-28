#!/bin/bash
# Claude Code statusLine command for the sm context monitor (sm#203).
#
# Two jobs, in this order:
#   1. Post the context-window sample to the session manager. The status line is
#      the only surface Claude gives context usage to, so this is the sole
#      producer of usage data — nothing else can supply it.
#   2. Render the status line itself.
#
# Rendering is delegated to whatever statusLine command was configured before
# this hook was installed (captured by the installer into
# context_monitor_delegate). Installing the monitor must not cost the user the
# status line they already had, which is exactly what the previous installer did
# by overwriting statusLine outright.
#
# Always exits 0 and never blocks the render: a status line that errors or hangs
# is visible in every pane.

set -u

HOOK_BASE_URL="${SM_HOOK_BASE_URL:-http://localhost:8420}"
HOOK_URL="${SM_CONTEXT_HOOK_URL:-${HOOK_BASE_URL%/}/hooks/context-usage}"
DELEGATE_FILE="${SM_STATUSLINE_DELEGATE_FILE:-$HOME/.claude/hooks/context_monitor_delegate}"

INPUT=$(cat)

post_usage() {
  # Reached only as somebody else's delegate; the outer invocation already
  # posted this exact sample.
  [ -z "${SM_STATUSLINE_ACTIVE:-}" ] || return 0
  [ -n "${CLAUDE_SESSION_MANAGER_ID:-}" ] || return 0
  [ -n "$INPUT" ] || return 0
  command -v jq >/dev/null 2>&1 || return 0

  # used_percentage is null until the first API call of a session. Nothing to
  # report yet, and reporting it every render before then is pure noise.
  #
  # sm_hook_emitted_at is stamped here, before the curl detaches, so the server
  # can tell a sample that describes pre-reset context from one that describes
  # the new cycle. A render that races a /clear or a compaction would otherwise
  # land after the lifecycle hook and re-latch the flags it just cleared,
  # silencing the next real warning. Stamping inside the jq run that builds the
  # body keeps this off the hot path — the status line renders constantly.
  local body
  body=$(
    printf '%s' "$INPUT" | jq -c \
      --arg sid "$CLAUDE_SESSION_MANAGER_ID" \
      'def stamp:
         now as $t | ($t | floor) as $s
         | ($s | strftime("%Y-%m-%dT%H:%M:%S"))
           + "." + (("000000" + ((($t - $s) * 1000000) | floor | tostring))[-6:])
           + "Z";
       select(.context_window.used_percentage != null)
       | {session_id: $sid,
          used_percentage: .context_window.used_percentage,
          total_input_tokens: (.context_window.total_input_tokens // 0),
          sm_hook_emitted_at: stamp}' 2>/dev/null
  ) || return 0
  [ -n "$body" ] || return 0

  local headers=(-H "Content-Type: application/json")
  if [ -n "${SM_HOOK_SECRET:-}" ]; then
    headers+=(-H "X-SM-Hook-Secret: $SM_HOOK_SECRET")
  fi

  # Detached with closed FDs so Claude never waits on the render for a request
  # that only feeds a background monitor.
  (
    curl -s --max-time 2 --connect-timeout 1 -X POST "$HOOK_URL" \
      "${headers[@]}" -d "$body" >/dev/null 2>&1
  ) </dev/null >/dev/null 2>&1 &
  disown 2>/dev/null
}

render() {
  local delegate=""
  # A delegate that points back here would fork on every render until the
  # machine gives up. The installer will not create one, but a hand-edited
  # delegate file or a hand-edited statusLine still can, and the failure mode is
  # bad enough to be worth catching at the point of use as well.
  if [ -n "${SM_STATUSLINE_ACTIVE:-}" ]; then
    delegate=""
  elif [ -n "${SM_STATUSLINE_DELEGATE:-}" ]; then
    delegate="$SM_STATUSLINE_DELEGATE"
  elif [ -r "$DELEGATE_FILE" ]; then
    delegate=$(cat "$DELEGATE_FILE" 2>/dev/null)
  fi

  if [ -n "$delegate" ]; then
    # The captured value is a shell command line ("bash ~/.claude/foo.sh"), not
    # necessarily a bare executable, so it has to go through a shell.
    printf '%s' "$INPUT" | SM_STATUSLINE_ACTIVE=1 sh -c "$delegate" 2>/dev/null
    return 0
  fi

  # No prior status line to preserve — emit a minimal usage readout rather than
  # leaving the line blank.
  if command -v jq >/dev/null 2>&1; then
    printf '%s' "$INPUT" | jq -r \
      '(.context_window.used_percentage // empty) | "\(.)% ctx"' 2>/dev/null
  fi
  return 0
}

post_usage
render
exit 0
