#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SOURCE_SCRIPT="$REPO_ROOT/hooks/notify_server.sh"
TARGET_DIR="$HOME/.claude/hooks"
TARGET_SCRIPT="$TARGET_DIR/notify_server.sh"
SETTINGS="$HOME/.claude/settings.json"

mkdir -p "$TARGET_DIR"
cp "$SOURCE_SCRIPT" "$TARGET_SCRIPT"
chmod +x "$TARGET_SCRIPT"

echo "Installed notify_server hook to $TARGET_SCRIPT"

# Register both ends of the turn. Stop alone is not enough: the server treats a
# stored idle as conclusive only when turn-start is observable too, and sessions
# run in arbitrary working directories that never load this repo's
# .claude/settings.json.
python3 - "$SETTINGS" "$TARGET_SCRIPT" <<'PY'
import json
import os
import sys

settings_path, hook_script = sys.argv[1], sys.argv[2]

try:
    with open(settings_path) as handle:
        settings = json.load(handle)
except FileNotFoundError:
    settings = {}
except json.JSONDecodeError:
    sys.exit(f"{settings_path} is not valid JSON; register the hooks manually")

if not isinstance(settings, dict):
    sys.exit(f"{settings_path} is not a JSON object; register the hooks manually")

hooks = settings.setdefault("hooks", {})
added = []
for event in ("UserPromptSubmit", "Stop"):
    matchers = hooks.setdefault(event, [])
    already = any(
        entry.get("command") == hook_script
        for matcher in matchers
        if isinstance(matcher, dict)
        for entry in matcher.get("hooks", [])
        if isinstance(entry, dict)
    )
    if already:
        continue
    matchers.append({"hooks": [{"type": "command", "command": hook_script}]})
    added.append(event)

if not added:
    print(f"{settings_path} already registers notify_server for UserPromptSubmit and Stop")
    sys.exit(0)

os.makedirs(os.path.dirname(settings_path), exist_ok=True)
tmp_path = f"{settings_path}.tmp"
with open(tmp_path, "w") as handle:
    json.dump(settings, handle, indent=2)
    handle.write("\n")
os.replace(tmp_path, settings_path)
print(f"Registered notify_server for {', '.join(added)} in {settings_path}")
PY
