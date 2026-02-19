#!/bin/bash
# install_context_hooks.sh — Merge sm#203 context monitor hooks into ~/.claude/settings.json
#
# This script merges the required settings into your existing settings.json.
# It does NOT replace the entire file — only adds the new keys.
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

echo "Merging sm#203 context monitor settings into $SETTINGS ..."

# Use jq to deep-merge the required configuration
MERGED=$(jq '
  . * {
    "statusLine": {
      "type": "command",
      "command": "~/.claude/hooks/context_monitor.sh"
    },
    "hooks": (
      (.hooks // {}) * {
        "PreCompact": (
          ((.hooks // {}).PreCompact // []) + [
            {
              "hooks": [
                {
                  "type": "command",
                  "command": "~/.claude/hooks/precompact_notify.sh"
                }
              ]
            }
          ]
        ),
        "SessionStart": (
          ((.hooks // {}).SessionStart // []) + [
            {
              "matcher": "clear",
              "hooks": [
                {
                  "type": "command",
                  "command": "~/.claude/hooks/session_clear_notify.sh"
                }
              ]
            },
            {
              "matcher": "compact",
              "hooks": [
                {
                  "type": "command",
                  "command": "~/.claude/hooks/post_compact_recovery.sh"
                }
              ]
            }
          ]
        )
      }
    )
  }
' "$SETTINGS")

echo "$MERGED" > "$SETTINGS"
echo "Done. Settings written to $SETTINGS"
echo ""
echo "Hook scripts installed at:"
echo "  $HOOKS_DIR/context_monitor.sh"
echo "  $HOOKS_DIR/precompact_notify.sh"
echo "  $HOOKS_DIR/session_clear_notify.sh"
echo "  $HOOKS_DIR/post_compact_recovery.sh"
echo ""
echo "Make sure they are executable:"
echo "  chmod +x $HOOKS_DIR/context_monitor.sh $HOOKS_DIR/precompact_notify.sh $HOOKS_DIR/session_clear_notify.sh $HOOKS_DIR/post_compact_recovery.sh"
