"""Tests for scripts/install_notify_server_hook.sh - sm#1131, sm#1132.

The installer merges into a settings.json that users and other tools also write
to, so what it must not do matters as much as what it does: no clobbering, no
duplicates, no widening of the file mode.
"""

import json
import os
import subprocess
import time
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
INSTALLER = REPO_ROOT / "scripts" / "install_notify_server_hook.sh"

HOOK_SCRIPTS = (
    "notify_server.sh",
    "context_monitor.sh",
    "precompact_notify.sh",
    "session_clear_notify.sh",
    "post_compact_recovery.sh",
)


def run_installer(home: Path) -> str:
    result = subprocess.run(
        ["bash", str(INSTALLER)],
        env={**os.environ, "HOME": str(home)},
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout


def write_settings(home: Path, settings: dict, mode: int = 0o600) -> Path:
    path = home / ".claude" / "settings.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(settings, indent=2))
    path.chmod(mode)
    return path


def read_settings(home: Path) -> dict:
    return json.loads((home / ".claude" / "settings.json").read_text())


def commands_for(settings: dict, event: str, matcher=None) -> list:
    entries = settings.get("hooks", {}).get(event, [])
    return [
        inner["command"]
        for entry in entries
        if entry.get("matcher") == matcher
        for inner in entry.get("hooks", [])
    ]


def test_installs_every_hook_script(tmp_path):
    run_installer(tmp_path)

    for script in HOOK_SCRIPTS:
        installed = tmp_path / ".claude" / "hooks" / script
        assert installed.is_file()
        assert os.access(installed, os.X_OK)


def test_context_monitor_forwards_rate_limit_windows(tmp_path):
    capture = tmp_path / "posted.json"
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    fake_curl = fake_bin / "curl"
    fake_curl.write_text(
        """#!/bin/bash
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-d" ]; then
    shift
    printf '%s' "$1" > "$CAPTURE"
    exit 0
  fi
  shift
done
exit 1
"""
    )
    fake_curl.chmod(0o755)
    payload = {
        "context_window": {"used_percentage": 41.5, "total_input_tokens": 83000},
        "rate_limits": {
            "five_hour": {"used_percentage": 12, "resets_at": "2026-08-10T20:00:00Z"},
            "seven_day": {"used_percentage": 34, "resets_at": "2026-08-16T16:00:00Z"},
        },
    }

    subprocess.run(
        ["bash", str(REPO_ROOT / "hooks" / "context_monitor.sh")],
        input=json.dumps(payload),
        env={
            **os.environ,
            "HOME": str(tmp_path),
            "PATH": f"{fake_bin}:{os.environ['PATH']}",
            "CAPTURE": str(capture),
            "CLAUDE_SESSION_MANAGER_ID": "seat-123",
        },
        capture_output=True,
        text=True,
        check=True,
    )
    for _ in range(50):
        if capture.exists():
            break
        time.sleep(0.02)

    posted = json.loads(capture.read_text())
    assert posted["session_id"] == "seat-123"
    assert posted["rate_limits"]["five_hour"] == payload["rate_limits"]["five_hour"]
    assert posted["rate_limits"]["seven_day"] == payload["rate_limits"]["seven_day"]


def test_registers_all_events_and_takes_over_status_line(tmp_path):
    write_settings(
        tmp_path,
        {"statusLine": {"type": "command", "command": "bash ~/.claude/statusline.sh"}},
    )
    run_installer(tmp_path)

    hooks_dir = tmp_path / ".claude" / "hooks"
    settings = read_settings(tmp_path)

    assert commands_for(settings, "UserPromptSubmit") == [str(hooks_dir / "notify_server.sh")]
    assert commands_for(settings, "Stop") == [str(hooks_dir / "notify_server.sh")]
    assert commands_for(settings, "PreCompact") == [str(hooks_dir / "precompact_notify.sh")]
    assert commands_for(settings, "SessionStart", "clear") == [
        str(hooks_dir / "session_clear_notify.sh")
    ]
    assert commands_for(settings, "SessionStart", "compact") == [
        str(hooks_dir / "post_compact_recovery.sh")
    ]

    # Claude exposes context usage only to the status line, so the monitor has to
    # take it over — but the command already there is preserved as a delegate.
    assert settings["statusLine"]["command"] == str(hooks_dir / "context_monitor.sh")
    delegate = (hooks_dir / "context_monitor_delegate").read_text().strip()
    assert delegate == "bash ~/.claude/statusline.sh"


def test_is_idempotent(tmp_path):
    write_settings(tmp_path, {})
    run_installer(tmp_path)
    first = read_settings(tmp_path)

    output = run_installer(tmp_path)
    assert "already registers every sm hook" in output
    assert read_settings(tmp_path) == first


def test_preserves_unrelated_settings_and_file_mode(tmp_path):
    settings_path = write_settings(
        tmp_path,
        {
            "env": {"SM_HOOK_SECRET": "keepme"},
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/user/own.sh"}]}]},
        },
        mode=0o600,
    )
    run_installer(tmp_path)

    settings = read_settings(tmp_path)
    assert settings["env"] == {"SM_HOOK_SECRET": "keepme"}
    assert "/user/own.sh" in commands_for(settings, "Stop")
    # Settings can hold hook credentials; a rewrite must not widen the mode.
    assert settings_path.stat().st_mode & 0o777 == 0o600


def test_removing_the_status_line_clears_a_captured_delegate(tmp_path):
    # Capture a delegate, then let the user drop statusLine entirely. Leaving the
    # delegate behind would keep rendering the status line they removed.
    write_settings(
        tmp_path,
        {"statusLine": {"type": "command", "command": "bash ~/.claude/statusline.sh"}},
    )
    run_installer(tmp_path)
    delegate_file = tmp_path / ".claude" / "hooks" / "context_monitor_delegate"
    assert delegate_file.exists()

    settings = read_settings(tmp_path)
    del settings["statusLine"]
    write_settings(tmp_path, settings)
    run_installer(tmp_path)

    assert not delegate_file.exists()
    hooks_dir = tmp_path / ".claude" / "hooks"
    assert read_settings(tmp_path)["statusLine"]["command"] == str(
        hooks_dir / "context_monitor.sh"
    )


def legacy_settings(home: Path) -> dict:
    """What the deleted install_context_hooks.sh left behind: the same scripts,
    registered by their `~/.claude/...` spelling rather than an absolute path."""
    return {
        "statusLine": {"type": "command", "command": "~/.claude/hooks/context_monitor.sh"},
        "hooks": {
            "PreCompact": [
                {"hooks": [{"type": "command", "command": "~/.claude/hooks/precompact_notify.sh"}]}
            ],
            "SessionStart": [
                {
                    "matcher": "clear",
                    "hooks": [
                        {
                            "type": "command",
                            "command": "~/.claude/hooks/session_clear_notify.sh",
                        }
                    ],
                },
                {
                    "matcher": "compact",
                    "hooks": [
                        {
                            "type": "command",
                            "command": "~/.claude/hooks/post_compact_recovery.sh",
                        }
                    ],
                },
            ],
        },
    }


def test_legacy_install_does_not_become_its_own_status_line_delegate(tmp_path):
    # The legacy statusLine names the very script this installer overwrites.
    # Capturing it as a delegate would make context_monitor.sh invoke itself on
    # every render, forever.
    write_settings(tmp_path, legacy_settings(tmp_path))
    run_installer(tmp_path)

    assert not (tmp_path / ".claude" / "hooks" / "context_monitor_delegate").exists()
    assert read_settings(tmp_path)["statusLine"]["command"] == (
        "~/.claude/hooks/context_monitor.sh"
    )


@pytest.mark.parametrize(
    "event,matcher",
    [("PreCompact", None), ("SessionStart", "clear"), ("SessionStart", "compact")],
)
def test_legacy_install_is_not_registered_twice(tmp_path, event, matcher):
    # Both spellings resolve to the same file, so a duplicate entry would post
    # every compaction twice and notify the parent twice.
    write_settings(tmp_path, legacy_settings(tmp_path))
    run_installer(tmp_path)

    assert len(commands_for(read_settings(tmp_path), event, matcher)) == 1
