#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="$HOME/.claude/hooks"
SETTINGS="$HOME/.claude/settings.json"

# Every hook this repo installs. Keeping one installer means a single place
# decides how settings.json is merged, and no second script can quietly undo the
# first one's registrations.
HOOK_SCRIPTS=(
  notify_server.sh
  context_monitor.sh
  precompact_notify.sh
  session_clear_notify.sh
  post_compact_recovery.sh
)

mkdir -p "$TARGET_DIR"
for hook in "${HOOK_SCRIPTS[@]}"; do
  cp "$REPO_ROOT/hooks/$hook" "$TARGET_DIR/$hook"
  chmod +x "$TARGET_DIR/$hook"
  echo "Installed $hook to $TARGET_DIR/$hook"
done

# Register both ends of the turn. Stop alone is not enough: the server treats a
# stored idle as conclusive only when turn-start is observable too, and sessions
# run in arbitrary working directories that never load this repo's
# .claude/settings.json.
#
# The context monitor hooks (sm#203) ride the same merge. Claude exposes context
# usage only to the status line, so the monitor has to take statusLine over — but
# whatever command is configured there is captured first and re-run by
# context_monitor.sh, so taking it over costs the user nothing.
python3 - "$SETTINGS" "$TARGET_DIR" <<'PY'
import json
import os
import sys

settings_path, hooks_dir = sys.argv[1], sys.argv[2]


def hook_path(name):
    return os.path.join(hooks_dir, name)


try:
    with open(settings_path) as handle:
        settings = json.load(handle)
except FileNotFoundError:
    settings = {}
except json.JSONDecodeError:
    sys.exit(f"{settings_path} is not valid JSON; register the hooks manually")

if not isinstance(settings, dict):
    sys.exit(f"{settings_path} is not a JSON object; register the hooks manually")

changes = []
hooks = settings.setdefault("hooks", {})


def register(event, command, matcher=None):
    """Append a hook entry unless this exact command is already registered under
    the same matcher. Other entries on the event are left untouched — users and
    other tools register hooks here too."""
    matchers = hooks.setdefault(event, [])
    for entry in matchers:
        if not isinstance(entry, dict) or entry.get("matcher") != matcher:
            continue
        for inner in entry.get("hooks", []):
            if isinstance(inner, dict) and inner.get("command") == command:
                return
    new_entry = {"hooks": [{"type": "command", "command": command}]}
    if matcher is not None:
        new_entry["matcher"] = matcher
    matchers.append(new_entry)
    changes.append(event if matcher is None else f"{event}({matcher})")


notify_server = hook_path("notify_server.sh")
register("UserPromptSubmit", notify_server)
register("Stop", notify_server)
register("PreCompact", hook_path("precompact_notify.sh"))
register("SessionStart", hook_path("session_clear_notify.sh"), matcher="clear")
register("SessionStart", hook_path("post_compact_recovery.sh"), matcher="compact")

# statusLine: capture whatever is configured now so context_monitor.sh can re-run
# it, then point statusLine at context_monitor.sh. Re-running the installer must
# not capture our own script as its own delegate — that would recurse forever.
context_monitor = hook_path("context_monitor.sh")
delegate_file = hook_path("context_monitor_delegate")
status_line = settings.get("statusLine")
existing_command = ""
if isinstance(status_line, dict) and status_line.get("type") == "command":
    existing_command = (status_line.get("command") or "").strip()

if existing_command and existing_command != context_monitor:
    with open(delegate_file, "w") as handle:
        handle.write(existing_command + "\n")
    os.chmod(delegate_file, 0o600)
    changes.append(f"statusLine delegate ({existing_command})")

if existing_command != context_monitor:
    settings["statusLine"] = {"type": "command", "command": context_monitor}
    changes.append("statusLine")

if not changes:
    print(f"{settings_path} already registers every sm hook")
    sys.exit(0)

os.makedirs(os.path.dirname(settings_path), exist_ok=True)

# Settings can hold hook credentials and environment values. Writing a fresh
# temp file and replacing would hand the result whatever the umask says
# (commonly 0644), silently widening a 0600 file, so carry the original mode
# across — and default a brand-new file to owner-only.
try:
    mode = os.stat(settings_path).st_mode & 0o777
except FileNotFoundError:
    mode = 0o600

tmp_path = f"{settings_path}.tmp"
with open(tmp_path, "w") as handle:
    json.dump(settings, handle, indent=2)
    handle.write("\n")
os.chmod(tmp_path, mode)
os.replace(tmp_path, settings_path)
print(f"Registered {', '.join(changes)} in {settings_path}")
PY
