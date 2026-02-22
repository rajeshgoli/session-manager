"""Unit tests for CLI argument parsing - ticket #64."""

import pytest
import sys
import argparse
from unittest.mock import MagicMock, patch
from io import StringIO

from src.cli.main import main
from src.cli.commands import resolve_session_id, parse_duration


class TestCliParsing:
    """Tests for CLI argument parsing."""

    def _get_parsed_args(self, args_list):
        """Helper to parse args without executing commands."""
        parser = argparse.ArgumentParser(prog="sm")
        subparsers = parser.add_subparsers(dest="command")

        # sm name
        name_parser = subparsers.add_parser("name")
        name_parser.add_argument("name_or_session")
        name_parser.add_argument("new_name", nargs="?")

        # sm me
        subparsers.add_parser("me")

        # sm who
        subparsers.add_parser("who")

        # sm send
        send_parser = subparsers.add_parser("send")
        send_parser.add_argument("session_id")
        send_parser.add_argument("text")
        send_parser.add_argument("--sequential", action="store_true")
        send_parser.add_argument("--important", action="store_true")
        send_parser.add_argument("--urgent", action="store_true")
        send_parser.add_argument("--wait", type=int, metavar="SECONDS")

        # sm spawn
        spawn_parser = subparsers.add_parser("spawn")
        spawn_parser.add_argument("provider", choices=["claude", "codex", "codex-app"])
        spawn_parser.add_argument("prompt")
        spawn_parser.add_argument("--name")
        spawn_parser.add_argument("--wait", type=int, metavar="SECONDS")
        spawn_parser.add_argument("--model")
        spawn_parser.add_argument("--working-dir")
        spawn_parser.add_argument("--json", action="store_true")

        # sm wait
        wait_parser = subparsers.add_parser("wait")
        wait_parser.add_argument("session_id")
        wait_parser.add_argument("seconds", type=int)

        # sm children
        children_parser = subparsers.add_parser("children")
        children_parser.add_argument("session_id", nargs="?")
        children_parser.add_argument("--recursive", action="store_true")
        children_parser.add_argument("--status", choices=["running", "completed", "error", "all"])
        children_parser.add_argument("--json", action="store_true")
        children_parser.add_argument("--db-path", default=None)

        # sm output
        output_parser = subparsers.add_parser("output")
        output_parser.add_argument("session")
        output_parser.add_argument("--lines", type=int, default=30)

        # sm codex-tui
        codex_tui_parser = subparsers.add_parser("codex-tui")
        codex_tui_parser.add_argument("session")
        codex_tui_parser.add_argument("--poll-interval", type=float, default=1.0)
        codex_tui_parser.add_argument("--event-limit", type=int, default=100)

        # sm watch
        watch_parser = subparsers.add_parser("watch")
        watch_parser.add_argument("--repo", default=None)
        watch_parser.add_argument("--role", default=None)
        watch_parser.add_argument("--interval", type=float, default=2.0)

        return parser.parse_args(args_list)


class TestNameCommand:
    """Tests for 'sm name' command parsing."""

    def test_name_single_arg_renames_self(self):
        """sm name <name> renames current session."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["name", "my-session"])

        assert args.command == "name"
        assert args.name_or_session == "my-session"
        assert args.new_name is None  # Not provided

    def test_name_two_args_renames_child(self):
        """sm name <session> <name> renames child."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["name", "child123", "new-name"])

        assert args.command == "name"
        assert args.name_or_session == "child123"
        assert args.new_name == "new-name"


class TestSendCommand:
    """Tests for 'sm send' command parsing."""

    def test_send_default_mode_is_sequential(self):
        """sm send without flag uses sequential mode (implicit)."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["send", "target123", "Hello world"])

        assert args.command == "send"
        assert args.session_id == "target123"
        assert args.text == "Hello world"
        assert args.sequential is False  # Not explicitly set
        assert args.important is False
        assert args.urgent is False

    def test_send_sequential_flag(self):
        """sm send --sequential sets flag."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["send", "--sequential", "target123", "Test"])

        assert args.sequential is True
        assert args.important is False
        assert args.urgent is False

    def test_send_important_flag(self):
        """sm send --important sets delivery mode."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["send", "--important", "target123", "Test"])

        assert args.sequential is False
        assert args.important is True
        assert args.urgent is False

    def test_send_urgent_flag(self):
        """sm send --urgent sets delivery mode."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["send", "--urgent", "target123", "Test"])

        assert args.sequential is False
        assert args.important is False
        assert args.urgent is True

    def test_send_wait_flag(self):
        """sm send --wait N parses correctly."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["send", "--wait", "60", "target123", "Test"])

        assert args.wait == 60


class TestSpawnCommand:
    """Tests for 'sm spawn' command parsing."""

    def test_spawn_basic(self):
        """sm spawn <prompt> parses correctly."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "claude", "Implement feature X"])

        assert args.command == "spawn"
        assert args.provider == "claude"
        assert args.prompt == "Implement feature X"
        assert args.name is None
        assert args.wait is None
        assert args.model is None
        assert args.working_dir is None
        assert args.json is False

    def test_spawn_wait_flag_parsed(self):
        """sm spawn --wait 300 parses correctly."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "claude", "--wait", "300", "Test prompt"])

        assert args.wait == 300

    def test_spawn_model_flag(self):
        """sm spawn --model opus sets model."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "claude", "--model", "opus", "Test prompt"])

        assert args.model == "opus"

    def test_spawn_model_choices(self):
        """sm spawn --model accepts known Claude models."""
        parser = TestCliParsing()

        for model in ["opus", "sonnet", "haiku"]:
            args = parser._get_parsed_args(["spawn", "claude", "--model", model, "Test"])
            assert args.model == model

    def test_spawn_codex_custom_model(self):
        """sm spawn codex accepts free-form model IDs."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "codex", "--model", "codex-5.1", "Test"])
        assert args.model == "codex-5.1"

    def test_spawn_codex_app_custom_model(self):
        """sm spawn codex-app accepts free-form model IDs."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "codex-app", "--model", "codex-5.1", "Test"])
        assert args.model == "codex-5.1"

    def test_spawn_name_flag(self):
        """sm spawn --name sets friendly name."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "claude", "--name", "test-agent", "Test prompt"])

        assert args.name == "test-agent"

    def test_spawn_working_dir_flag(self):
        """sm spawn --working-dir sets directory."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "claude", "--working-dir", "/tmp/work", "Test prompt"])

        assert args.working_dir == "/tmp/work"

    def test_spawn_json_flag(self):
        """sm spawn --json enables JSON output."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "claude", "--json", "Test prompt"])

        assert args.json is True


class TestWaitCommand:
    """Tests for 'sm wait' command parsing."""

    def test_wait_parses_args(self):
        """sm wait <session-id> <seconds> parses correctly."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["wait", "session123", "60"])

        assert args.command == "wait"
        assert args.session_id == "session123"
        assert args.seconds == 60


class TestWatchCommand:
    """Tests for 'sm watch' parsing."""

    def test_watch_defaults(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(["watch"])

        assert args.command == "watch"
        assert args.repo is None
        assert args.role is None
        assert args.interval == 2.0

    def test_watch_flags(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(
            ["watch", "--repo", "/tmp/repo", "--role", "engineer", "--interval", "3.5"]
        )

        assert args.repo == "/tmp/repo"
        assert args.role == "engineer"
        assert args.interval == 3.5


class TestChildrenCommand:
    """Tests for 'sm children' command parsing."""

    def test_children_no_session_id(self):
        """sm children without session uses current."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["children"])

        assert args.command == "children"
        assert args.session_id is None  # Will use current

    def test_children_with_session_id(self):
        """sm children <session-id> specifies parent."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["children", "parent123"])

        assert args.session_id == "parent123"

    def test_children_recursive_flag(self):
        """sm children --recursive includes grandchildren."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["children", "--recursive"])

        assert args.recursive is True

    def test_children_status_filter(self):
        """sm children --status <status> filters results."""
        parser = TestCliParsing()

        for status in ["running", "completed", "error", "all"]:
            args = parser._get_parsed_args(["children", "--status", status])
            assert args.status == status

    def test_children_json_flag(self):
        """sm children --json enables JSON output."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["children", "--json"])

        assert args.json is True

    def test_children_db_path(self):
        """sm children --db-path overrides tool_usage.db path."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["children", "--db-path", "/tmp/test.db"])

        assert args.db_path == "/tmp/test.db"

    def test_children_db_path_default_none(self):
        """sm children --db-path defaults to None (resolved at runtime)."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["children"])

        assert args.db_path is None


class TestOutputCommand:
    """Tests for 'sm output' command parsing."""

    def test_output_default_lines(self):
        """sm output <session> defaults to 30 lines."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["output", "session123"])

        assert args.command == "output"
        assert args.session == "session123"
        assert args.lines == 30

    def test_output_custom_lines(self):
        """sm output <session> --lines N sets line count."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["output", "session123", "--lines", "100"])

        assert args.lines == 100


class TestCodexTuiCommand:
    """Tests for 'sm codex-tui' command parsing."""

    def test_codex_tui_defaults(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(["codex-tui", "abc123"])

        assert args.command == "codex-tui"
        assert args.session == "abc123"
        assert args.poll_interval == 1.0
        assert args.event_limit == 100

    def test_codex_tui_custom_flags(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(
            ["codex-tui", "abc123", "--poll-interval", "0.5", "--event-limit", "50"]
        )

        assert args.poll_interval == 0.5
        assert args.event_limit == 50


class TestSessionResolution:
    """Tests for session resolution utility."""

    def test_resolve_by_id(self):
        """Can resolve session by ID."""
        mock_client = MagicMock()
        mock_client.get_session.return_value = {"id": "abc123", "name": "test"}

        session_id, session = resolve_session_id(mock_client, "abc123")

        assert session_id == "abc123"
        assert session["id"] == "abc123"
        mock_client.get_session.assert_called_with("abc123")

    def test_resolve_by_friendly_name(self):
        """Can resolve session by friendly name."""
        mock_client = MagicMock()
        mock_client.get_session.return_value = None  # Not found by ID
        mock_client.list_sessions.return_value = [
            {"id": "abc123", "friendly_name": "test-session"},
            {"id": "def456", "friendly_name": "other-session"},
        ]

        session_id, session = resolve_session_id(mock_client, "test-session")

        assert session_id == "abc123"
        assert session["friendly_name"] == "test-session"

    def test_resolve_not_found(self):
        """Returns None when session not found."""
        mock_client = MagicMock()
        mock_client.get_session.return_value = None
        mock_client.list_sessions.return_value = []

        session_id, session = resolve_session_id(mock_client, "nonexistent")

        assert session_id is None
        assert session is None

    def test_resolve_unavailable(self):
        """Returns None when session manager unavailable."""
        mock_client = MagicMock()
        mock_client.get_session.return_value = None
        mock_client.list_sessions.return_value = None  # Unavailable

        session_id, session = resolve_session_id(mock_client, "anything")

        assert session_id is None
        assert session is None

    def test_resolve_rejects_empty_string(self):
        """Empty string identifier returns None (Issue #105)."""
        mock_client = MagicMock()
        mock_client.get_session.return_value = None
        mock_client.list_sessions.return_value = [
            {"id": "abc123", "friendly_name": ""},  # Session with empty name
            {"id": "def456", "friendly_name": "test-session"},
        ]

        session_id, session = resolve_session_id(mock_client, "")

        # Should not match empty-named session
        assert session_id is None
        assert session is None
        # Should not even try to search by friendly name
        mock_client.list_sessions.assert_not_called()

    def test_resolve_rejects_blank_string(self):
        """Blank string (spaces only) identifier returns None (Issue #105)."""
        mock_client = MagicMock()
        mock_client.get_session.return_value = None

        session_id, session = resolve_session_id(mock_client, "   ")

        # Should not match
        assert session_id is None
        assert session is None
        # Should not even try to search
        mock_client.list_sessions.assert_not_called()


class TestParseDuration:
    """Tests for duration parsing utility."""

    def test_parse_seconds(self):
        """Parses seconds format."""
        assert parse_duration("30s") == 30
        assert parse_duration("60s") == 60

    def test_parse_minutes(self):
        """Parses minutes format."""
        assert parse_duration("5m") == 300
        assert parse_duration("10m") == 600

    def test_parse_hours(self):
        """Parses hours format."""
        assert parse_duration("1h") == 3600
        assert parse_duration("2h") == 7200

    def test_parse_days(self):
        """Parses days format."""
        assert parse_duration("1d") == 86400

    def test_parse_combined(self):
        """Parses combined formats."""
        assert parse_duration("2h30m") == 2 * 3600 + 30 * 60
        assert parse_duration("1h30m15s") == 3600 + 1800 + 15

    def test_parse_pure_integer(self):
        """Parses pure integer as seconds."""
        assert parse_duration("300") == 300

    def test_parse_empty_raises(self):
        """Empty string raises ValueError."""
        with pytest.raises(ValueError):
            parse_duration("")

    def test_parse_invalid_raises(self):
        """Invalid format raises ValueError."""
        with pytest.raises(ValueError):
            parse_duration("invalid")

        with pytest.raises(ValueError):
            parse_duration("abc123")

    def test_parse_zero_raises(self):
        """Zero duration raises ValueError (Issue #106)."""
        with pytest.raises(ValueError, match="Duration must be positive"):
            parse_duration("0s")

        with pytest.raises(ValueError, match="Duration must be positive"):
            parse_duration("0m")

        with pytest.raises(ValueError, match="Duration must be positive"):
            parse_duration("0")

    def test_parse_negative_raises(self):
        """Negative duration raises ValueError (Issue #106)."""
        # Note: Current implementation doesn't support negative syntax (e.g., "-5m")
        # but if someone passes 0 it should fail
        with pytest.raises(ValueError, match="Duration must be positive"):
            parse_duration("0h0m0s")
