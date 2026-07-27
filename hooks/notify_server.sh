#!/bin/bash
# Hook script that posts Claude Code Stop/Notification events to Session Manager.
# Always exits successfully - if the server is down or session not found, log it
# and return 0 so Claude does not surface a hook failure.

HOOK_BASE_URL="${SM_HOOK_BASE_URL:-http://localhost:8420}"
HOOK_URL="${SM_HOOK_URL:-${HOOK_BASE_URL%/}/hooks/claude}"
HOOK_LOG_PATH="${CLAUDE_HOOK_LOG_PATH:-/tmp/claude-hooks.log}"

# Emission timestamp, stamped before the curl is detached. Lifecycle hooks are
# posted from independent background processes, so HTTP arrival order does not
# match the order the events actually happened in; the server orders Stop against
# the newest turn-start using these stamps. Sub-second resolution matters: a Stop
# and the next turn's UserPromptSubmit routinely land in the same second.
hook_emitted_at() {
  local stamp
  stamp=$(date -u +%Y-%m-%dT%H:%M:%S.%N 2>/dev/null || true)
  case "$stamp" in
    *%N*|'')
      # BSD date without %N. Time::HiRes ships with every stock perl.
      stamp=$(perl -MTime::HiRes=time -e '
        my $t = time;
        my @g = gmtime(int($t));
        printf("%04d-%02d-%02dT%02d:%02d:%02d.%06dZ",
          $g[5]+1900, $g[4]+1, $g[3], $g[2], $g[1], $g[0], ($t-int($t))*1000000);
      ' 2>/dev/null || true)
      ;;
    *)
      stamp="${stamp}Z"
      ;;
  esac
  if [ -z "$stamp" ]; then
    stamp=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || true)
  fi
  printf '%s' "$stamp"
}

extract_transcript_metadata() {
  local transcript_path="$1"
  if [ -z "$transcript_path" ] || [ ! -f "$transcript_path" ]; then
    return 1
  fi

  local mtime_seconds
  mtime_seconds=$(stat -f %m "$transcript_path" 2>/dev/null || stat -c %Y "$transcript_path" 2>/dev/null || echo "")
  local mtime_ns=""
  if [ -n "$mtime_seconds" ]; then
    mtime_ns="${mtime_seconds}000000000"
  fi

  local metadata
  metadata=$(
    tail -n 400 "$transcript_path" | jq -cs '
      def trim_text:
        tostring | sub("^[[:space:]]+"; "") | sub("[[:space:]]+$"; "");
      def assistant_text($entry):
        (($entry.message.content // [])
          | map(select(type == "object" and .type == "text") | (.text // ""))
          | join("\n")
          | trim_text);
      reverse | reduce .[] as $entry (
        {sm_last_message: null, sm_native_title: null, assistant_seen: false};
        (if .sm_native_title == null then
          if $entry.type == "custom-title" and (($entry.customTitle // "") | trim_text) != "" then
            .sm_native_title = (($entry.customTitle // "") | trim_text)
          elif $entry.type == "agent-name" and (($entry.agentName // "") | trim_text) != "" then
            .sm_native_title = (($entry.agentName // "") | trim_text)
          else . end
        else . end)
        | (if (.assistant_seen | not) and $entry.type == "assistant" then
          .assistant_seen = true
          | (assistant_text($entry)) as $text
          | if $text != "" then .sm_last_message = $text else . end
        else . end)
      ) | del(.assistant_seen)
    ' 2>/dev/null
  ) || return 1

  if [ -n "$mtime_ns" ]; then
    echo "$metadata" | jq -c --argjson mtime "$mtime_ns" '. + {sm_transcript_mtime_ns: $mtime}' 2>/dev/null
  else
    echo "$metadata"
  fi
}

metadata_message() {
  echo "$1" | jq -r '.sm_last_message // empty' 2>/dev/null
}

# Claude writes a single JSON line for the hook payload, but it may keep stdin
# open until the broader turn finishes (for example while background task
# notifications are still pending). Read just the first line we need instead of
# waiting for EOF, otherwise the Stop hook can appear hung for minutes.
INPUT=""
IFS= read -r INPUT || [ -n "$INPUT" ]

if [ -z "$INPUT" ]; then
  exit 0
fi

# Stamped here, before the transcript retry sleeps below. Those sleeps are
# precisely what lets a Stop land after the next turn's UserPromptSubmit, so the
# stamp has to predate them to describe when the event actually happened.
EMITTED_AT="$(hook_emitted_at)"

# Inline transcript metadata for remote delivery. When the hook is posting to a
# non-local primary, that primary cannot safely read the node-local transcript.
# UserPromptSubmit is a turn-start signal only and carries no transcript payload,
# so skip the tail+jq work there — it runs before every single turn.
if [ -n "$SM_HOOK_BASE_URL" ] || [ -n "$SM_HOOK_URL" ]; then
  TRANSCRIPT_PATH=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)
  HOOK_EVENT=$(echo "$INPUT" | jq -r '.hook_event_name // empty' 2>/dev/null)
  if [ "$HOOK_EVENT" = "UserPromptSubmit" ]; then
    TRANSCRIPT_PATH=""
  fi
  if [ -n "$TRANSCRIPT_PATH" ] && [ -f "$TRANSCRIPT_PATH" ]; then
    METADATA=$(extract_transcript_metadata "$TRANSCRIPT_PATH" || true)
    if [ "$HOOK_EVENT" = "Stop" ]; then
      if [ -z "$(metadata_message "$METADATA")" ]; then
        sleep 0.5
        METADATA=$(extract_transcript_metadata "$TRANSCRIPT_PATH" || true)
      fi
      CURRENT_MESSAGE="$(metadata_message "$METADATA")"
      if [ -n "$CURRENT_MESSAGE" ] && { [ -z "$SM_LAST_CLAUDE_OUTPUT" ] || [ "$CURRENT_MESSAGE" = "$SM_LAST_CLAUDE_OUTPUT" ]; }; then
        sleep 0.3
        METADATA=$(extract_transcript_metadata "$TRANSCRIPT_PATH" || true)
      fi
    fi
    if [ -n "$METADATA" ]; then
      INPUT=$(jq -c --argjson meta "$METADATA" '. + $meta' <<< "$INPUT" 2>/dev/null || echo "$INPUT")
    fi
  fi
fi

# Attach the emission stamp, plus CLAUDE_SESSION_MANAGER_ID when the environment
# provides it. Done in one jq pass to keep this off the per-turn hot path.
INPUT=$(jq -c \
  --arg sid "${CLAUDE_SESSION_MANAGER_ID:-}" \
  --arg emitted "$EMITTED_AT" \
  '. + (if $emitted != "" then {sm_hook_emitted_at: $emitted} else {} end)
     + (if $sid != "" then {session_manager_id: $sid} else {} end)' \
  <<< "$INPUT" 2>/dev/null || echo "$INPUT")

# Post to local server asynchronously (don't block Claude).
# Close inherited FDs so Claude Code does not keep waiting on the background curl.
(
  echo "$(date): Hook called" >> "$HOOK_LOG_PATH"
  echo "$INPUT" >> "$HOOK_LOG_PATH"

  CURL_HEADERS=(-H "Content-Type: application/json")
  if [ -n "$SM_HOOK_SECRET" ]; then
    CURL_HEADERS+=(-H "X-SM-Hook-Secret: $SM_HOOK_SECRET")
  fi

  RESPONSE=$(curl -s --max-time 5 --connect-timeout 2 -X POST "$HOOK_URL" \
    "${CURL_HEADERS[@]}" \
    -d "$INPUT" 2>&1)

  echo "$RESPONSE" >> "$HOOK_LOG_PATH"
  echo "---" >> "$HOOK_LOG_PATH"
) </dev/null >/dev/null 2>&1 &
disown 2>/dev/null

exit 0
