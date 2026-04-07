#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SOURCE_SCRIPT="$REPO_ROOT/hooks/notify_server.sh"
TARGET_DIR="$HOME/.claude/hooks"
TARGET_SCRIPT="$TARGET_DIR/notify_server.sh"

mkdir -p "$TARGET_DIR"
cp "$SOURCE_SCRIPT" "$TARGET_SCRIPT"
chmod +x "$TARGET_SCRIPT"

echo "Installed notify_server hook to $TARGET_SCRIPT"
