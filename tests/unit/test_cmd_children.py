"""Unit tests for sm#190: sm children thinking duration + last tool use."""

import sqlite3
import sys
import time
from datetime import datetime, timedelta
from io import StringIO
from pathlib import Path
from unittest.mock import MagicMock, call, patch

import pytest

from src.cli.commands import (
    _DB_ERROR,
    _format_thinking_duration,
    _get_tmux_session_activity,
    _query_last_tool,
    cmd_children,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_db(tmp_path: Path, rows: list[dict]) -> Path:
    """Create a minimal tool_usage.db with given rows."""
    db_path = tmp_path / "tool_usage.db"
    conn = sqlite3.connect(db_path)
    conn.execute(
        """
        CREATE TABLE tool_usage (
            session_id TEXT,
            session_name TEXT,
            hook_type TEXT,
            tool_name TEXT,
            target_file TEXT,
            bash_command TEXT,
            timestamp DATETIME
        )
        """
    )
    for row in rows:
        conn.execute(
            "INSERT INTO tool_usage VALUES (?, ?, ?, ?, ?, ?, ?)",
            (
                row.get("session_id", "abc12345678"),
                row.get("session_name", "test-agent"),
                row.get("hook_type", "PreToolUse"),
                row.get("tool_name", "Read"),
                row.get("target_file"),
                row.get("bash_command"),
                row.get("timestamp", datetime.utcnow().strftime("%Y-%m-%d %H:%M:%S")),
            ),
        )
    conn.commit()
    conn.close()
    return db_path


def _make_client(children: list[dict]) -> MagicMock:
    """Build a mock client returning given children."""
    client = MagicMock()
    client.list_children = MagicMock(return_value={"children": children})
    return client


def _child(
    child_id: str = "aaa00000001",
    name: str = "sm-agent",
    friendly_name: str = "sm-agent",
    status: str = "running",
    completion_status: str = None,
    last_activity: str = None,
    provider: str = "claude",
    agent_status_text: str = None,
    agent_status_at: str = None,
    completion_message: str = None,
    activity_projection: dict = None,
) -> dict:
    if last_activity is None:
        last_activity = datetime.utcnow().isoformat()
    return {
        "id": child_id,
        "name": f"claude-{child_id}",
        "friendly_name": friendly_name,
        "status": status,
        "completion_status": completion_status,
        "last_activity": last_activity,
        "provider": provider,
        "agent_status_text": agent_status_text,
        "agent_status_at": agent_status_at,
        "completion_message": completion_message,
        "activity_projection": activity_projection,
    }


# ---------------------------------------------------------------------------
# _query_last_tool
# ---------------------------------------------------------------------------


class TestQueryLastTool:
    def test_returns_correct_fields(self, tmp_path):
        ts = "2026-02-19 05:00:00"
        db = _make_db(
            tmp_path,
            [
                {
                    "session_id": "sess1",
                    "tool_name": "Edit",
                    "target_file": "src/main.py",
                    "bash_command": None,
                    "timestamp": ts,
                }
            ],
        )
        result = _query_last_tool("sess1", str(db))
        assert result is not None
        assert result["tool_name"] == "Edit"
        assert result["target_file"] == "src/main.py"
        assert result["bash_command"] is None
        assert result["timestamp_str"] == ts

    def test_returns_none_when_no_entries(self, tmp_path):
        db = _make_db(tmp_path, [])
        result = _query_last_tool("nonexistent-session", str(db))
        assert result is None

    def test_returns_sentinel_when_db_missing(self, tmp_path):
        # Missing DB → sqlite3 creates empty file → no table → OperationalError → sentinel
        result = _query_last_tool("sess1", str(tmp_path / "no_such.db"))
        assert result is _DB_ERROR

    def test_only_pretooluse_events(self, tmp_path):
        ts_pre = "2026-02-19 05:00:00"
        ts_post = "2026-02-19 05:01:00"
        db = _make_db(
            tmp_path,
            [
                {"session_id": "s1", "hook_type": "PreToolUse", "tool_name": "Read", "timestamp": ts_pre},
                {"session_id": "s1", "hook_type": "PostToolUse", "tool_name": "Bash", "timestamp": ts_post},
            ],
        )
        result = _query_last_tool("s1", str(db))
        assert result is not None
        assert result["tool_name"] == "Read"

    def test_returns_most_recent(self, tmp_path):
        db = _make_db(
            tmp_path,
            [
                {"session_id": "s1", "tool_name": "Read", "timestamp": "2026-02-19 05:00:00"},
                {"session_id": "s1", "tool_name": "Bash", "timestamp": "2026-02-19 05:05:00"},
                {"session_id": "s1", "tool_name": "Edit", "timestamp": "2026-02-19 05:03:00"},
            ],
        )
        result = _query_last_tool("s1", str(db))
        assert result["tool_name"] == "Bash"


# ---------------------------------------------------------------------------
# _get_tmux_session_activity
# ---------------------------------------------------------------------------


class TestGetTmuxSessionActivity:
    def test_returns_int_epoch_on_valid_output(self):
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(stdout="1771480032\n", returncode=0)
            result = _get_tmux_session_activity("codex-abc123")
        assert result == 1771480032
        mock_run.assert_called_once_with(
            ["tmux", "display-message", "-p", "-t", "codex-abc123", "#{session_activity}"],
            capture_output=True,
            text=True,
            timeout=3,
        )

    def test_returns_none_on_empty_output(self):
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(stdout="", returncode=1)
            result = _get_tmux_session_activity("codex-abc123")
        assert result is None

    def test_returns_none_on_non_integer_output(self):
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(stdout="not-a-number\n", returncode=0)
            result = _get_tmux_session_activity("codex-abc123")
        assert result is None

    def test_returns_none_when_tmux_unavailable(self):
        with patch("subprocess.run", side_effect=OSError("tmux not found")):
            result = _get_tmux_session_activity("codex-abc123")
        assert result is None


# ---------------------------------------------------------------------------
# _format_thinking_duration
# ---------------------------------------------------------------------------


class TestFormatThinkingDuration:
    def test_sub_minute(self):
        assert _format_thinking_duration(0) == "0s"
        assert _format_thinking_duration(30) == "30s"
        assert _format_thinking_duration(59) == "59s"

    def test_one_minute_boundary(self):
        assert _format_thinking_duration(60) == "1m00s"

    def test_multi_minute(self):
        assert _format_thinking_duration(272) == "4m32s"
        assert _format_thinking_duration(501) == "8m21s"

    def test_zero_pad_seconds(self):
        assert _format_thinking_duration(65) == "1m05s"


# ---------------------------------------------------------------------------
# cmd_children output
# ---------------------------------------------------------------------------


class TestCmdChildrenOutput:
    def test_running_claude_session_shows_thinking_and_last_tool(self, tmp_path, capsys):
        ts = (datetime.utcnow() - timedelta(seconds=30)).strftime("%Y-%m-%d %H:%M:%S")
        db = _make_db(
            tmp_path,
            [
                {
                    "session_id": "aaa00000001",
                    "tool_name": "Edit",
                    "target_file": "src/cli/commands.py",
                    "bash_command": None,
                    "timestamp": ts,
                }
            ],
        )
        child = _child(child_id="aaa00000001", provider="claude", status="running")
        client = _make_client([child])

        rc = cmd_children(client, "parent1", db_path=str(db))
        out = capsys.readouterr().out

        assert rc == 0
        assert "thinking" in out
        assert "last tool: Edit src/cli/commands.py" in out

    def test_running_codex_session_shows_no_hooks(self, tmp_path, capsys):
        child = _child(child_id="bbb00000002", provider="codex", status="running")
        client = _make_client([child])

        epoch = int(time.time()) - 120
        with patch("src.cli.commands._get_tmux_session_activity", return_value=epoch):
            rc = cmd_children(client, "parent1", db_path=str(tmp_path / "tool_usage.db"))

        out = capsys.readouterr().out
        assert rc == 0
        assert "last tool: n/a (no hooks)" in out
        assert "thinking" in out

    def test_codex_app_session_uses_activity_projection(self, tmp_path, capsys):
        child = _child(
            child_id="ccc00000003",
            provider="codex-app",
            status="running",
            activity_projection={
                "summary_text": "Started: git status",
                "started_at": datetime.utcnow().isoformat(),
            },
        )
        client = _make_client([child])

        rc = cmd_children(client, "parent1", db_path=str(tmp_path / "tool_usage.db"))
        out = capsys.readouterr().out

        assert rc == 0
        assert "thinking" in out
        assert "last action: Started: git status" in out

    def test_codex_app_without_projection_skips_signals(self, tmp_path, capsys):
        child = _child(child_id="ccc00000004", provider="codex-app", status="running", activity_projection=None)
        client = _make_client([child])

        rc = cmd_children(client, "parent1", db_path=str(tmp_path / "tool_usage.db"))
        out = capsys.readouterr().out

        assert rc == 0
        assert "thinking" not in out
        assert "last action" not in out

    def test_idle_session_no_thinking_columns(self, tmp_path, capsys):
        child = _child(child_id="ddd00000004", provider="claude", status="idle")
        client = _make_client([child])

        rc = cmd_children(client, "parent1", db_path=str(tmp_path / "tool_usage.db"))
        out = capsys.readouterr().out

        assert rc == 0
        assert "thinking" not in out
        assert "last tool" not in out

    def test_completed_session_no_thinking_columns(self, tmp_path, capsys):
        child = _child(
            child_id="eee00000005",
            provider="claude",
            status="stopped",
            completion_status="completed",
            completion_message="Done.",
        )
        client = _make_client([child])

        rc = cmd_children(client, "parent1", db_path=str(tmp_path / "tool_usage.db"))
        out = capsys.readouterr().out

        assert rc == 0
        assert "thinking" not in out
        assert "Done." in out

    def test_db_unavailable_single_warning_no_crash(self, tmp_path, capsys):
        children = [
            _child(child_id=f"fff0000000{i}", provider="claude", status="running")
            for i in range(3)
        ]
        client = _make_client(children)

        rc = cmd_children(client, "parent1", db_path=str(tmp_path / "nonexistent.db"))
        out = capsys.readouterr()

        assert rc == 0
        # One warning emitted, not three
        assert out.err.count("tool_usage.db not found") == 1

    def test_db_locked_single_warning_no_crash(self, tmp_path, capsys):
        """DB file exists but is locked — one warning, skip signals for all sessions."""
        db = _make_db(tmp_path, [])  # file exists so db_ok passes
        children = [
            _child(child_id=f"jjj0000000{i}", provider="claude", status="running")
            for i in range(3)
        ]
        client = _make_client(children)

        with patch("src.cli.commands._query_last_tool", return_value=_DB_ERROR):
            rc = cmd_children(client, "parent1", db_path=str(db))
            out = capsys.readouterr()

        assert rc == 0
        # One warning emitted, not three
        assert out.err.count("locked or unreadable") == 1
        # No thinking/last-tool columns in output
        assert "thinking" not in out.out
        assert "last tool" not in out.out

    def test_agent_status_text_preserved_after_new_fields(self, tmp_path, capsys):
        """#188 agent_status_text still appears after thinking/last-tool fields."""
        ts = (datetime.utcnow() - timedelta(seconds=10)).strftime("%Y-%m-%d %H:%M:%S")
        db = _make_db(
            tmp_path,
            [
                {
                    "session_id": "ggg00000007",
                    "tool_name": "Bash",
                    "bash_command": "pytest",
                    "timestamp": ts,
                }
            ],
        )
        agent_status_at = datetime.utcnow().isoformat()
        child = _child(
            child_id="ggg00000007",
            provider="claude",
            status="running",
            agent_status_text="Running tests",
            agent_status_at=agent_status_at,
        )
        client = _make_client([child])

        rc = cmd_children(client, "parent1", db_path=str(db))
        out = capsys.readouterr().out

        assert rc == 0
        line = out.strip()
        # thinking and last tool come before agent_status_text
        thinking_pos = line.find("thinking")
        status_pos = line.find('"Running tests"')
        assert thinking_pos != -1
        assert status_pos != -1
        assert thinking_pos < status_pos

    def test_format_relative_time_fix_no_unknown(self, tmp_path, capsys):
        """elapsed should not be 'unknown' when last_activity is a valid ISO string."""
        ts = (datetime.utcnow() - timedelta(minutes=5)).isoformat()
        child = _child(child_id="hhh00000008", status="idle", last_activity=ts)
        client = _make_client([child])

        rc = cmd_children(client, "parent1", db_path=str(tmp_path / "tool_usage.db"))
        out = capsys.readouterr().out

        assert rc == 0
        assert "unknown" not in out

    def test_no_children_returns_zero(self, tmp_path, capsys):
        client = _make_client([])
        rc = cmd_children(client, "parent1", db_path=str(tmp_path / "tool_usage.db"))
        out = capsys.readouterr().out
        assert rc == 0
        assert "No child sessions" in out

    def test_server_unavailable_returns_2(self, capsys):
        client = MagicMock()
        client.list_children = MagicMock(return_value=None)
        rc = cmd_children(client, "parent1")
        assert rc == 2
