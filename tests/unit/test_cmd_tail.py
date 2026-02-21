"""Unit tests for sm#189: sm tail command."""

import sqlite3
import sys
from datetime import datetime, timedelta
from io import StringIO
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from src.cli.commands import cmd_tail


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def _make_client(session_id: str = "abc12345678", friendly_name: str = "test-agent"):
    """Build a mock SessionManagerClient that resolves to a single session."""
    session = {
        "id": session_id,
        "name": f"claude-{session_id}",
        "friendly_name": friendly_name,
        "provider": "claude",
        "tmux_session": f"claude-{session_id}",
        "working_dir": "/tmp/test",
        "status": "running",
    }
    client = MagicMock()
    client.get_session = MagicMock(return_value=session)
    client.list_sessions = MagicMock(return_value=[session])
    return client, session


def _make_db(tmp_path: Path, rows: list[dict]) -> Path:
    """Create a minimal tool_usage.db at tmp_path with the given rows."""
    db_path = tmp_path / "tool_usage.db"
    conn = sqlite3.connect(db_path)
    conn.execute("""
        CREATE TABLE tool_usage (
            session_id TEXT,
            session_name TEXT,
            hook_type TEXT,
            tool_name TEXT,
            target_file TEXT,
            bash_command TEXT,
            timestamp DATETIME
        )
    """)
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
                row.get("timestamp", datetime.utcnow().isoformat(sep=" ")),
            ),
        )
    conn.commit()
    conn.close()
    return db_path


# ---------------------------------------------------------------------------
# _rel() timestamp formatting (tested indirectly via cmd_tail output)
# ---------------------------------------------------------------------------


class TestRelativeTime:
    """Test relative time formatting via structured output."""

    def test_seconds_ago(self, tmp_path, capsys):
        """Row 30s old → shows '30s ago'."""
        client, session = _make_client()
        ts = (datetime.utcnow() - timedelta(seconds=30)).strftime("%Y-%m-%d %H:%M:%S")
        db = _make_db(tmp_path, [{"tool_name": "Read", "target_file": "foo.py", "timestamp": ts}])

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override=str(db))
        assert rc == 0
        captured = capsys.readouterr()
        assert "30s ago" in captured.out

    def test_minutes_and_seconds_ago(self, tmp_path, capsys):
        """Row 90s old → shows '1m30s ago'."""
        client, session = _make_client()
        ts = (datetime.utcnow() - timedelta(seconds=90)).strftime("%Y-%m-%d %H:%M:%S")
        db = _make_db(tmp_path, [{"tool_name": "Read", "target_file": "bar.py", "timestamp": ts}])

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override=str(db))
        assert rc == 0
        captured = capsys.readouterr()
        assert "1m30s ago" in captured.out

    def test_malformed_timestamp_no_crash(self, tmp_path, capsys):
        """Malformed timestamp → shows '? ago', no exception."""
        client, session = _make_client()
        db = _make_db(tmp_path, [{"tool_name": "Read", "target_file": "x.py", "timestamp": "not-a-date"}])

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override=str(db))
        assert rc == 0
        captured = capsys.readouterr()
        assert "? ago" in captured.out


# ---------------------------------------------------------------------------
# -n validation
# ---------------------------------------------------------------------------


class TestNValidation:
    """Test -n flag validation."""

    def test_n_zero_exits_1(self, tmp_path, capsys):
        """-n 0 → exit code 1, error on stderr."""
        client, _ = _make_client()
        db = _make_db(tmp_path, [])

        rc = cmd_tail(client, "abc12345678", n=0, db_path_override=str(db))
        assert rc == 1
        captured = capsys.readouterr()
        assert "at least 1" in captured.err

    def test_n_negative_exits_1(self, tmp_path, capsys):
        """-n -1 → exit code 1, error on stderr."""
        client, _ = _make_client()
        db = _make_db(tmp_path, [])

        rc = cmd_tail(client, "abc12345678", n=-1, db_path_override=str(db))
        assert rc == 1
        captured = capsys.readouterr()
        assert "at least 1" in captured.err


# ---------------------------------------------------------------------------
# Structured mode — DB not found
# ---------------------------------------------------------------------------


class TestStructuredModeNoDB:
    """Test structured mode when DB file is missing."""

    def test_missing_db_exits_1(self, tmp_path, capsys):
        """Nonexistent DB path → exit code 1, informative message on stderr."""
        client, _ = _make_client()

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override="/nonexistent/path.db")
        assert rc == 1
        captured = capsys.readouterr()
        assert "not found" in captured.err.lower() or "No tool usage" in captured.err


# ---------------------------------------------------------------------------
# Structured mode — no rows for session
# ---------------------------------------------------------------------------


class TestStructuredModeNoRows:
    """Test structured mode when DB has no rows for this session."""

    def test_no_rows_exits_0_with_message(self, tmp_path, capsys):
        """Session exists but no DB rows → exit 0, 'no data' message on stdout."""
        client, _ = _make_client()
        db = _make_db(tmp_path, [])  # Empty DB

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override=str(db))
        assert rc == 0
        captured = capsys.readouterr()
        assert "No tool usage data" in captured.out

    def test_other_session_rows_not_shown(self, tmp_path, capsys):
        """Rows for a different session_id are not included."""
        client, _ = _make_client()
        # Only rows for a different session
        db = _make_db(tmp_path, [{"session_id": "different_session", "tool_name": "Read", "target_file": "x.py"}])

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override=str(db))
        assert rc == 0
        captured = capsys.readouterr()
        assert "No tool usage data" in captured.out


# ---------------------------------------------------------------------------
# Session not found / unavailable
# ---------------------------------------------------------------------------


class TestSessionResolution:
    """Test session resolution errors."""

    def test_session_not_found_exits_1(self, capsys):
        """Unresolvable identifier → exit code 1."""
        client = MagicMock()
        client.get_session = MagicMock(return_value=None)
        client.list_sessions = MagicMock(return_value=[])  # Empty list, SM available

        rc = cmd_tail(client, "nonexistent-session", n=10, db_path_override="/nonexistent/path.db")
        assert rc == 1
        captured = capsys.readouterr()
        assert "not found" in captured.err.lower()

    def test_session_manager_unavailable_exits_2(self, capsys):
        """Server unreachable → exit code 2."""
        client = MagicMock()
        client.get_session = MagicMock(return_value=None)
        client.list_sessions = MagicMock(return_value=None)  # None = unavailable

        rc = cmd_tail(client, "some-session", n=10, db_path_override="/nonexistent/path.db")
        assert rc == 2
        captured = capsys.readouterr()
        assert "unavailable" in captured.err.lower()


# ---------------------------------------------------------------------------
# Structured mode — normal output
# ---------------------------------------------------------------------------


class TestStructuredModeOutput:
    """Test structured mode output formatting."""

    def test_shows_read_with_filename(self, tmp_path, capsys):
        """Read tool with target_file shows 'Read: <file>'."""
        client, _ = _make_client()
        db = _make_db(tmp_path, [{"tool_name": "Read", "target_file": "src/foo.py"}])

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override=str(db))
        assert rc == 0
        captured = capsys.readouterr()
        assert "Read: src/foo.py" in captured.out

    def test_shows_bash_with_command(self, tmp_path, capsys):
        """Bash tool with bash_command shows 'Bash: <command>'."""
        client, _ = _make_client()
        db = _make_db(tmp_path, [{"tool_name": "Bash", "bash_command": "git status"}])

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override=str(db))
        assert rc == 0
        captured = capsys.readouterr()
        assert "Bash: git status" in captured.out

    def test_deduplication_pretooluse_only(self, tmp_path, capsys):
        """PostToolUse rows are excluded — only PreToolUse rows shown."""
        client, _ = _make_client()
        ts = datetime.utcnow().strftime("%Y-%m-%d %H:%M:%S")
        db = _make_db(tmp_path, [
            {"hook_type": "PreToolUse", "tool_name": "Read", "target_file": "a.py", "timestamp": ts},
            {"hook_type": "PostToolUse", "tool_name": "Read", "target_file": "a.py", "timestamp": ts},
        ])

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override=str(db))
        assert rc == 0
        captured = capsys.readouterr()
        # Should only show one entry, not two
        assert captured.out.count("Read: a.py") == 1

    def test_n_limits_results(self, tmp_path, capsys):
        """-n 3 returns at most 3 entries even when more exist."""
        client, _ = _make_client()
        rows = [
            {"tool_name": "Read", "target_file": f"file{i}.py",
             "timestamp": (datetime.utcnow() - timedelta(seconds=i*10)).strftime("%Y-%m-%d %H:%M:%S")}
            for i in range(10)
        ]
        db = _make_db(tmp_path, rows)

        rc = cmd_tail(client, "abc12345678", n=3, db_path_override=str(db))
        assert rc == 0
        captured = capsys.readouterr()
        # Count "ago]" occurrences to count rows
        assert captured.out.count(" ago]") == 3

    def test_most_recent_last(self, tmp_path, capsys):
        """Most recent entry appears last (chronological order)."""
        client, _ = _make_client()
        older_ts = (datetime.utcnow() - timedelta(seconds=60)).strftime("%Y-%m-%d %H:%M:%S")
        newer_ts = (datetime.utcnow() - timedelta(seconds=10)).strftime("%Y-%m-%d %H:%M:%S")
        db = _make_db(tmp_path, [
            {"tool_name": "Read", "target_file": "older.py", "timestamp": older_ts},
            {"tool_name": "Read", "target_file": "newer.py", "timestamp": newer_ts},
        ])

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override=str(db))
        assert rc == 0
        captured = capsys.readouterr()
        lines = captured.out.strip().split("\n")
        # Find the lines with the filenames
        older_pos = next(i for i, l in enumerate(lines) if "older.py" in l)
        newer_pos = next(i for i, l in enumerate(lines) if "newer.py" in l)
        assert older_pos < newer_pos, "Older entry should appear before newer entry"

    def test_grep_shows_search_label(self, tmp_path, capsys):
        """Grep tool shows 'Grep: (search)'."""
        client, _ = _make_client()
        db = _make_db(tmp_path, [{"tool_name": "Grep"}])

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override=str(db))
        assert rc == 0
        assert "Grep: (search)" in capsys.readouterr().out

    def test_task_shows_subagent_label(self, tmp_path, capsys):
        """Task tool shows 'Task: (subagent)'."""
        client, _ = _make_client()
        db = _make_db(tmp_path, [{"tool_name": "Task"}])

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override=str(db))
        assert rc == 0
        assert "Task: (subagent)" in capsys.readouterr().out

    def test_header_shows_session_name_and_id_prefix(self, tmp_path, capsys):
        """Header line includes friendly name and 8-char session ID prefix."""
        client, _ = _make_client(session_id="abc12345678", friendly_name="test-agent")
        db = _make_db(tmp_path, [{"tool_name": "Read", "target_file": "x.py"}])

        rc = cmd_tail(client, "abc12345678", n=10, db_path_override=str(db))
        assert rc == 0
        captured = capsys.readouterr()
        assert "test-agent" in captured.out
        assert "abc12345" in captured.out  # 8-char prefix of session_id


class TestCodexAppProjection:
    def test_codex_app_structured_mode_uses_activity_projection(self, capsys):
        client, session = _make_client(session_id="codexapp1", friendly_name="codex-worker")
        session["provider"] = "codex-app"
        client.get_session.return_value = session
        client.list_sessions.return_value = [session]
        client.get_activity_actions.return_value = {
            "actions": [
                {
                    "summary_text": "Started: pytest -q",
                    "status": "running",
                    "started_at": datetime.utcnow().isoformat(),
                    "ended_at": None,
                }
            ]
        }

        rc = cmd_tail(client, "codexapp1", n=5)
        assert rc == 0
        out = capsys.readouterr().out
        assert "Started: pytest -q" in out
        assert "[running]" in out

    def test_codex_app_structured_mode_no_actions(self, capsys):
        client, session = _make_client(session_id="codexapp2", friendly_name="codex-worker")
        session["provider"] = "codex-app"
        client.get_session.return_value = session
        client.list_sessions.return_value = [session]
        client.get_activity_actions.return_value = {"actions": []}

        rc = cmd_tail(client, "codexapp2", n=5)
        assert rc == 0
        out = capsys.readouterr().out
        assert "No activity data" in out
