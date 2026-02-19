#!/bin/bash
# install_context_hooks.sh — Install sm#203 context monitor hooks
#
# Writes the 4 hook scripts to ~/.claude/hooks/ and merges the required
# settings into ~/.claude/settings.json (idempotent — safe to run multiple times).
#
# Prerequisites: jq must be installed.
# Usage: bash scripts/install_context_hooks.sh

set -e

SETTINGS="$HOME/.claude/settings.json"
HOOKS_DIR="$HOME/.claude/hooks"

# Ensure hooks directory exists
mkdir -p "$HOOKS_DIR"

# Ensure settings.json exists
if [ ! -f "$SETTINGS" ]; then
  echo '{}' > "$SETTINGS"
fi

# ---------------------------------------------------------------------------
# 1. Write hook scripts
# ---------------------------------------------------------------------------

echo "Writing hook scripts to $HOOKS_DIR ..."

cat > "$HOOKS_DIR/context_monitor.sh" << 'HOOK_EOF'
#!/bin/bash
# Status line script — receives context JSON from Claude Code via stdin
INPUT=$(cat)

# Extract context data
SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty')
USED_PCT=$(echo "$INPUT" | jq -r '.context_window.used_percentage // 0')
TOTAL_INPUT=$(echo "$INPUT" | jq -r '.context_window.total_input_tokens // 0')
TOTAL_OUTPUT=$(echo "$INPUT" | jq -r '.context_window.total_output_tokens // 0')
CONTEXT_SIZE=$(echo "$INPUT" | jq -r '.context_window.context_window_size // 200000')

# Map Claude session_id to sm session_id via env var
SM_SESSION_ID="${CLAUDE_SESSION_MANAGER_ID}"

if [ -n "$SM_SESSION_ID" ] && [ "$USED_PCT" != "null" ] && [ "$USED_PCT" != "0" ]; then
  # Post to sm server (non-blocking, short timeout)
  # sm server only listens on 127.0.0.1 — this call is localhost-only (trusted)
  curl -s --max-time 0.5 -X POST http://localhost:8420/hooks/context-usage \
    -H "Content-Type: application/json" \
    -d "$(jq -n \
          --arg sid "$SM_SESSION_ID" \
          --argjson pct "$USED_PCT" \
          --argjson tin "$TOTAL_INPUT" \
          --argjson tout "$TOTAL_OUTPUT" \
          --argjson csz "$CONTEXT_SIZE" \
          '{session_id: $sid, used_percentage: $pct, total_input_tokens: $tin, total_output_tokens: $tout, context_window_size: $csz}')" \
    >/dev/null 2>&1 &
fi

# Output status line text (displayed in Claude Code TUI)
echo "${USED_PCT}% ctx"
HOOK_EOF

cat > "$HOOKS_DIR/precompact_notify.sh" << 'HOOK_EOF'
#!/bin/bash
INPUT=$(cat)
SM_SESSION_ID="${CLAUDE_SESSION_MANAGER_ID}"
TRIGGER=$(echo "$INPUT" | jq -r '.trigger // "unknown"')

if [ -n "$SM_SESSION_ID" ]; then
  # sm server only listens on 127.0.0.1 — localhost-only trusted call
  curl -s --max-time 0.5 -X POST http://localhost:8420/hooks/context-usage \
    -H "Content-Type: application/json" \
    -d "$(jq -n --arg sid "$SM_SESSION_ID" --arg trig "$TRIGGER" \
          '{session_id: $sid, event: "compaction", trigger: $trig}')" \
    >/dev/null 2>&1
fi
exit 0
HOOK_EOF

cat > "$HOOKS_DIR/session_clear_notify.sh" << 'HOOK_EOF'
#!/bin/bash
SM_SESSION_ID="${CLAUDE_SESSION_MANAGER_ID}"

if [ -n "$SM_SESSION_ID" ]; then
  # Notify sm that context was manually cleared — re-arms one-shot warning flags
  # sm server only listens on 127.0.0.1 — localhost-only trusted call
  curl -s --max-time 0.5 -X POST http://localhost:8420/hooks/context-usage \
    -H "Content-Type: application/json" \
    -d "$(jq -n --arg sid "$SM_SESSION_ID" '{session_id: $sid, event: "context_reset"}')" \
    >/dev/null 2>&1
fi
exit 0
HOOK_EOF

cat > "$HOOKS_DIR/post_compact_recovery.sh" << 'HOOK_EOF'
#!/bin/bash
INPUT=$(cat)
SM_SESSION_ID="${CLAUDE_SESSION_MANAGER_ID}"

if [ -z "$SM_SESSION_ID" ]; then
  exit 0
fi

# Query sm server for the last handoff path (set by sm#196 _execute_handoff)
# sm server only listens on 127.0.0.1 — localhost-only trusted call
HANDOFF_PATH=$(curl -s --max-time 2 http://localhost:8420/sessions/"$SM_SESSION_ID" \
  | jq -r '.last_handoff_path // empty')

# If a handoff doc was previously executed for this session, inject it as additional context
if [ -n "$HANDOFF_PATH" ] && [ -f "$HANDOFF_PATH" ]; then
  jq -n --rawfile ctx "$HANDOFF_PATH" '{
    hookSpecificOutput: {
      hookEventName: "SessionStart",
      additionalContext: $ctx
    }
  }'
fi
HOOK_EOF

chmod +x \
  "$HOOKS_DIR/context_monitor.sh" \
  "$HOOKS_DIR/precompact_notify.sh" \
  "$HOOKS_DIR/session_clear_notify.sh" \
  "$HOOKS_DIR/post_compact_recovery.sh"

echo "  $HOOKS_DIR/context_monitor.sh"
echo "  $HOOKS_DIR/precompact_notify.sh"
echo "  $HOOKS_DIR/session_clear_notify.sh"
echo "  $HOOKS_DIR/post_compact_recovery.sh"

# ---------------------------------------------------------------------------
# 2. Merge settings.json (idempotent — skip entries that already exist)
# ---------------------------------------------------------------------------

echo "Merging sm#203 context monitor settings into $SETTINGS ..."

PRECOMPACT_CMD="~/.claude/hooks/precompact_notify.sh"
SESSION_CLEAR_CMD="~/.claude/hooks/session_clear_notify.sh"
SESSION_COMPACT_CMD="~/.claude/hooks/post_compact_recovery.sh"

MERGED=$(jq \
  --arg precompact "$PRECOMPACT_CMD" \
  --arg session_clear "$SESSION_CLEAR_CMD" \
  --arg session_compact "$SESSION_COMPACT_CMD" '

  # Merge statusLine (idempotent: * overwrites the key)
  . * {
    "statusLine": {
      "type": "command",
      "command": "~/.claude/hooks/context_monitor.sh"
    }
  } |

  # Ensure hooks object exists
  .hooks = (.hooks // {}) |

  # Add PreCompact hook only if the command is not already registered
  .hooks.PreCompact = (
    (.hooks.PreCompact // []) |
    if any(
      .[] | (.hooks // [])[] ;
      .command == $precompact
    )
    then .
    else . + [{"hooks": [{"type": "command", "command": $precompact}]}]
    end
  ) |

  # Add SessionStart clear hook only if not already registered
  .hooks.SessionStart = (
    (.hooks.SessionStart // []) |
    if any(
      .[] | select(.matcher == "clear") | (.hooks // [])[] ;
      .command == $session_clear
    )
    then .
    else . + [{
      "matcher": "clear",
      "hooks": [{"type": "command", "command": $session_clear}]
    }]
    end
  ) |

  # Add SessionStart compact hook only if not already registered
  .hooks.SessionStart = (
    (.hooks.SessionStart // []) |
    if any(
      .[] | select(.matcher == "compact") | (.hooks // [])[] ;
      .command == $session_compact
    )
    then .
    else . + [{
      "matcher": "compact",
      "hooks": [{"type": "command", "command": $session_compact}]
    }]
    end
  )
' "$SETTINGS")

echo "$MERGED" > "$SETTINGS"
echo "Done. Settings written to $SETTINGS"
