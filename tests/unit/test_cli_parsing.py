"""Unit tests for CLI argument parsing - ticket #64."""

import pytest
import sys
import argparse
import os
from unittest.mock import MagicMock, patch
from io import StringIO

from src.cli.main import main, _normalize_optional_track_args
from src.cli.commands import (
    parse_duration,
    resolve_session_id,
    resolve_session_id_with_status,
    validate_friendly_name,
)


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
        send_parser.add_argument("--track", action="store_const", const=300, default=None)
        send_parser.add_argument("--track-seconds", dest="track", type=int, metavar="SECONDS")

        # sm email
        email_parser = subparsers.add_parser("email")
        email_parser.add_argument("recipients")
        email_parser.add_argument("--subject", required=True)
        email_parser.add_argument("--body")
        email_parser.add_argument("--text")
        email_parser.add_argument("--html")
        email_parser.add_argument("--cc")

        # sm spawn
        spawn_parser = subparsers.add_parser("spawn")
        spawn_parser.add_argument("provider", choices=["claude", "codex", "codex-fork", "codex-app"])
        spawn_parser.add_argument("prompt")
        spawn_parser.add_argument("--name")
        spawn_parser.add_argument("--wait", type=int, metavar="SECONDS")
        spawn_parser.add_argument("--model")
        spawn_parser.add_argument("--working-dir")
        spawn_parser.add_argument("--json", action="store_true")
        spawn_parser.add_argument("--track", action="store_const", const=300, default=None)
        spawn_parser.add_argument("--track-seconds", dest="track", type=int, metavar="SECONDS")

        # sm wait
        wait_parser = subparsers.add_parser("wait")
        wait_parser.add_argument("session_id")
        wait_parser.add_argument("seconds", type=int)

        # sm remind
        remind_parser = subparsers.add_parser("remind")
        remind_parser.add_argument("first_arg", nargs="?")
        remind_parser.add_argument("message", nargs="*", default=[])
        remind_parser.add_argument("--recurring", action="store_true")
        remind_parser.add_argument("--stop", action="store_true")

        # sm watch-job
        watch_job_parser = subparsers.add_parser("watch-job")
        watch_job_subparsers = watch_job_parser.add_subparsers(dest="watch_job_command")

        watch_job_add = watch_job_subparsers.add_parser("add")
        watch_job_add.add_argument("--target")
        watch_job_add.add_argument("--label")
        watch_job_add.add_argument("--pid", type=int)
        watch_job_add.add_argument("--file", dest="file_path")
        watch_job_add.add_argument("--progress-regex")
        watch_job_add.add_argument("--done-regex")
        watch_job_add.add_argument("--error-regex")
        watch_job_add.add_argument("--exit-code-file")
        watch_job_add.add_argument("--interval", dest="interval_seconds", type=int, default=300)
        watch_job_add.add_argument("--tail-lines", type=int, default=200)
        watch_job_add.add_argument("--tail-on-error", type=int, default=10)
        watch_job_add.add_argument("--notify-every-poll", action="store_true")

        watch_job_list = watch_job_subparsers.add_parser("list")
        watch_job_list.add_argument("--target")
        watch_job_list.add_argument("--all", action="store_true")
        watch_job_list.add_argument("--json", action="store_true")
        watch_job_list.add_argument("--include-inactive", action="store_true")

        watch_job_cancel = watch_job_subparsers.add_parser("cancel")
        watch_job_cancel.add_argument("watch_id")

        # sm request-codex-review
        request_codex_review_parser = subparsers.add_parser("request-codex-review")
        request_codex_review_parser.add_argument("action_or_pr")
        request_codex_review_parser.add_argument("request_id", nargs="?")
        request_codex_review_parser.add_argument("--repo")
        request_codex_review_parser.add_argument("--notify")
        request_codex_review_parser.add_argument("--steer")
        request_codex_review_parser.add_argument("--all", action="store_true")
        request_codex_review_parser.add_argument("--inactive", action="store_true")
        request_codex_review_parser.add_argument("--json", action="store_true")
        request_codex_review_parser.add_argument("--pr", dest="status_pr", type=int)
        request_codex_review_parser.add_argument("--poll-interval", dest="poll_interval_seconds", type=int, default=30)
        request_codex_review_parser.add_argument("--retry-interval", dest="retry_interval_seconds", type=int, default=600)

        # sm children
        children_parser = subparsers.add_parser("children")
        children_parser.add_argument("session_id", nargs="?")
        children_parser.add_argument("--recursive", action="store_true")
        children_parser.add_argument("--terminated", action="store_true")
        children_parser.add_argument("--status", choices=["running", "completed", "error", "all"])
        children_parser.add_argument("--json", action="store_true")
        children_parser.add_argument("--db-path", default=None)

        # sm restore
        restore_parser = subparsers.add_parser("restore")
        restore_parser.add_argument("session")

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

        return parser.parse_args(_normalize_optional_track_args(args_list))


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

    def test_send_track_default_flag(self):
        """sm send --track defaults to 300 seconds."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["send", "target123", "Test", "--track"])

        assert args.track == 300

    def test_send_track_custom_flag(self):
        """sm send --track N parses correctly."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["send", "target123", "Test", "--track", "420"])

        assert args.track == 420

    def test_send_track_before_session_id_uses_default(self):
        """sm send --track <session> <text> keeps positionals intact."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["send", "--track", "target123", "Test"])

        assert args.track == 300
        assert args.session_id == "target123"
        assert args.text == "Test"

    def test_send_track_before_text_uses_default(self):
        """sm send <session> --track <text> keeps the message positional intact."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["send", "target123", "--track", "Test"])

        assert args.track == 300
        assert args.session_id == "target123"
        assert args.text == "Test"

    def test_send_track_equals_custom_flag(self):
        """sm send --track=420 parses correctly."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["send", "target123", "Test", "--track=420"])

        assert args.track == 420

    def test_send_track_sentinel_preserves_literal_text(self):
        """sm send target -- --track=420 keeps the payload literal."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["send", "target123", "--", "--track=420"])

        assert args.track is None
        assert args.session_id == "target123"
        assert args.text == "--track=420"


class TestEmailCommand:
    """Tests for 'sm email' command parsing."""

    def test_email_command_parses_basic_flags(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(
            ["email", "rajesh,architect", "--subject", "Spec ready", "--body", "See attached"]
        )

        assert args.command == "email"
        assert args.recipients == "rajesh,architect"
        assert args.subject == "Spec ready"
        assert args.body == "See attached"
        assert args.text is None
        assert args.html is None

    def test_email_command_parses_file_flags(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(
            ["email", "rajesh", "--subject", "Weekly", "--text", "docs/summary.md", "--cc", "architect"]
        )

        assert args.text == "docs/summary.md"
        assert args.cc == "architect"


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

    def test_spawn_track_default_flag(self):
        """sm spawn --track defaults to 300 seconds."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "claude", "Test prompt", "--track"])

        assert args.track == 300

    def test_spawn_track_custom_flag(self):
        """sm spawn --track N parses correctly."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "claude", "Test prompt", "--track", "420"])

        assert args.track == 420

    def test_spawn_track_before_prompt_uses_default(self):
        """sm spawn claude --track <prompt> keeps the prompt positional intact."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "claude", "--track", "Test prompt"])

        assert args.track == 300
        assert args.provider == "claude"
        assert args.prompt == "Test prompt"

    def test_spawn_track_sentinel_preserves_literal_prompt(self):
        """sm spawn claude -- --track=420 keeps the prompt literal."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "claude", "--", "--track=420"])

        assert args.track is None
        assert args.provider == "claude"
        assert args.prompt == "--track=420"

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

    def test_spawn_codex_fork_custom_model(self):
        """sm spawn codex-fork accepts free-form model IDs."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["spawn", "codex-fork", "--model", "codex-5.1", "Test"])
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


class TestFriendlyNameValidation:
    """Tests for friendly-name validation shared by CLI and server paths."""

    def test_rejects_trailing_newline(self):
        valid, error = validate_friendly_name("agent\n")

        assert valid is False
        assert "alphanumeric" in error

    def test_rejects_embedded_newline_command(self):
        valid, error = validate_friendly_name("agent\n/clear")

        assert valid is False
        assert "alphanumeric" in error

    def test_accepts_safe_name(self):
        valid, error = validate_friendly_name("agent-123_ok")

        assert valid is True
        assert error == ""


class TestWaitCommand:
    """Tests for 'sm wait' command parsing."""

    def test_wait_parses_args(self):
        """sm wait <session-id> <seconds> parses correctly."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["wait", "session123", "60"])

        assert args.command == "wait"
        assert args.session_id == "session123"
        assert args.seconds == 60


class TestRemindCommand:
    """Tests for `sm remind` command parsing."""

    def test_remind_one_shot_parses(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(["remind", "300", "check", "run"])

        assert args.command == "remind"
        assert args.first_arg == "300"
        assert args.message == ["check", "run"]
        assert args.recurring is False
        assert args.stop is False

    def test_remind_recurring_flag_parses(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(["remind", "--recurring", "900", "check", "run"])

        assert args.first_arg == "900"
        assert args.message == ["check", "run"]
        assert args.recurring is True

    def test_remind_cancel_parses(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(["remind", "cancel", "abc123"])

        assert args.first_arg == "cancel"
        assert args.message == ["abc123"]


class TestCodexCommandRouting:
    """Tests for user-facing Codex command routing."""

    def test_main_codex_routes_to_codex_fork(self):
        with patch("sys.argv", ["sm", "codex", "/tmp/repo"]):
            with patch("src.cli.main.commands.cmd_new", return_value=0) as mock_cmd_new:
                with pytest.raises(SystemExit) as exc_info:
                    main()

        assert exc_info.value.code == 0
        mock_cmd_new.assert_called_once()
        assert mock_cmd_new.call_args.kwargs["provider"] == "codex-fork"

    def test_main_codex_legacy_routes_to_legacy_provider(self):
        with patch("sys.argv", ["sm", "codex-legacy", "/tmp/repo"]):
            with patch("src.cli.main.commands.cmd_new", return_value=0) as mock_cmd_new:
                with pytest.raises(SystemExit) as exc_info:
                    main()

        assert exc_info.value.code == 0
        mock_cmd_new.assert_called_once()
        assert mock_cmd_new.call_args.kwargs["provider"] == "codex"


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
        """sm children <session> specifies parent."""
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

    def test_children_terminated_flag(self):
        """sm children --terminated includes killed children."""
        parser = TestCliParsing()
        args = parser._get_parsed_args(["children", "--terminated"])

        assert args.terminated is True

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


class TestChildrenCommandDispatch:
    """Tests for 'sm children' runtime resolution and dispatch."""

    def test_children_resolves_parent_identifier_before_dispatch(self):
        mock_client = MagicMock()

        with patch.object(sys, "argv", ["sm", "children", "chief-scientist"]):
            with patch("src.cli.main.SessionManagerClient", return_value=mock_client):
                with patch("src.cli.main.commands.resolve_session_id", return_value=("abc12345", {"id": "abc12345"})):
                    with patch("src.cli.main.commands.cmd_children", return_value=0) as mock_cmd_children:
                        with pytest.raises(SystemExit) as exc_info:
                            main()

        assert exc_info.value.code == 0
        mock_cmd_children.assert_called_once_with(mock_client, "abc12345", False, None, False, False, None)

    def test_children_reports_missing_named_parent(self, capsys):
        mock_client = MagicMock()
        mock_client.list_sessions.return_value = []

        with patch.object(sys, "argv", ["sm", "children", "chief-scientist"]):
            with patch("src.cli.main.SessionManagerClient", return_value=mock_client):
                with patch("src.cli.main.commands.resolve_session_id", return_value=(None, None)):
                    with pytest.raises(SystemExit) as exc_info:
                        main()

        assert exc_info.value.code == 1
        assert "Error: Session 'chief-scientist' not found" in capsys.readouterr().err

    def test_children_reports_unavailable_when_resolution_cannot_query_sessions(self, capsys):
        mock_client = MagicMock()
        mock_client.list_sessions.return_value = None

        with patch.object(sys, "argv", ["sm", "children", "chief-scientist"]):
            with patch("src.cli.main.SessionManagerClient", return_value=mock_client):
                with patch("src.cli.main.commands.resolve_session_id", return_value=(None, None)):
                    with pytest.raises(SystemExit) as exc_info:
                        main()

        assert exc_info.value.code == 2
        assert "Session manager unavailable or request timed out" in capsys.readouterr().err


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


class TestRequestCodexReviewCommand:
    """Tests for 'sm request-codex-review' command parsing and dispatch."""

    def test_request_codex_review_create_parses_pr_and_options(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(
            [
                "request-codex-review",
                "618",
                "--repo",
                "owner/repo",
                "--notify",
                "maintainer",
                "--steer",
                "focus on races",
            ]
        )

        assert args.command == "request-codex-review"
        assert args.action_or_pr == "618"
        assert args.repo == "owner/repo"
        assert args.notify == "maintainer"
        assert args.steer == "focus on races"
        assert args.poll_interval_seconds == 30
        assert args.retry_interval_seconds == 600

    def test_main_request_codex_review_dispatches_create(self):
        mock_client = MagicMock()

        with patch.dict(os.environ, {"CLAUDE_SESSION_MANAGER_ID": "agent618"}, clear=True):
            with patch.object(sys, "argv", ["sm", "request-codex-review", "618"]):
                with patch("src.cli.main.SessionManagerClient", return_value=mock_client):
                    with patch("src.cli.main.commands.cmd_request_codex_review_create", return_value=0) as mock_cmd:
                        with pytest.raises(SystemExit) as exc_info:
                            main()

        assert exc_info.value.code == 0
        mock_cmd.assert_called_once()


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


class TestRestoreCommand:
    """Tests for restore command parsing."""

    def test_restore_command_parses_target(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(["restore", "engineer-ticket2508"])

        assert args.command == "restore"
        assert args.session == "engineer-ticket2508"

    def test_main_restore_allowed_without_managed_session(self):
        mock_client = MagicMock()

        with patch.dict(os.environ, {}, clear=True):
            with patch.object(sys, "argv", ["sm", "restore", "dead123"]):
                with patch("src.cli.main.SessionManagerClient", return_value=mock_client):
                    with patch("src.cli.main.commands.cmd_restore", return_value=0) as mock_cmd_restore:
                        with pytest.raises(SystemExit) as exc_info:
                            main()

        assert exc_info.value.code == 0
        mock_cmd_restore.assert_called_once_with(mock_client, "dead123")


class TestSessionResolution:
    """Tests for session resolution utility."""

    def test_resolve_by_id(self):
        """Can resolve session by ID."""
        mock_client = MagicMock()
        mock_client.get_session.return_value = {"id": "abc123", "name": "test"}

        session_id, session = resolve_session_id(mock_client, "abc123")

        assert session_id == "abc123"
        assert session["id"] == "abc123"
        mock_client.get_session.assert_called_with("abc123", timeout=None)

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

    def test_resolve_by_friendly_name_including_stopped(self):
        """Stopped sessions can be resolved when explicitly requested."""
        mock_client = MagicMock()
        mock_client.get_session.return_value = None
        mock_client.list_sessions.return_value = [
            {"id": "dead123", "friendly_name": "engineer-ticket2508", "status": "stopped"},
        ]

        session_id, session = resolve_session_id(
            mock_client,
            "engineer-ticket2508",
            include_stopped=True,
        )

        assert session_id == "dead123"
        assert session["status"] == "stopped"
        mock_client.list_sessions.assert_called_once_with(include_stopped=True, timeout=None)

    def test_resolve_with_status_reports_unavailable(self):
        """Status-aware resolution preserves transport unavailability."""
        mock_client = MagicMock()
        mock_client.get_session.return_value = None
        mock_client.list_sessions.return_value = None

        session_id, session, unavailable = resolve_session_id_with_status(mock_client, "anything")

        assert session_id is None
        assert session is None
        assert unavailable is True

    def test_resolve_with_status_falls_back_for_legacy_get_session_signature(self):
        """Legacy clients without a timeout kwarg still resolve by ID."""
        mock_client = MagicMock()

        def _legacy_get_session(session_id):
            return {"id": session_id, "name": "legacy"}

        mock_client.get_session.side_effect = _legacy_get_session

        session_id, session, unavailable = resolve_session_id_with_status(
            mock_client,
            "abc123",
            timeout=15.0,
        )

        assert session_id == "abc123"
        assert session["name"] == "legacy"
        assert unavailable is False
        mock_client.get_session.assert_called_with("abc123")

    def test_resolve_with_status_preserves_include_stopped_for_legacy_list_sessions(self):
        """Legacy list_sessions fallbacks still honor include_stopped."""
        mock_client = MagicMock()
        mock_client.get_session.return_value = None

        def _legacy_list_sessions(*args, **kwargs):
            if "timeout" in kwargs:
                raise TypeError("legacy client does not accept timeout")
            if kwargs.get("include_stopped") is True:
                return [{"id": "dead123", "friendly_name": "stopped-worker", "status": "stopped"}]
            return []

        mock_client.list_sessions.side_effect = _legacy_list_sessions

        session_id, session, unavailable = resolve_session_id_with_status(
            mock_client,
            "stopped-worker",
            include_stopped=True,
            timeout=15.0,
        )

        assert session_id == "dead123"
        assert session["status"] == "stopped"
        assert unavailable is False


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


class TestWatchJobCommand:
    """Tests for 'sm watch-job' command parsing."""

    def test_watch_job_add(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args([
            "watch-job",
            "add",
            "--target", "maintainer",
            "--label", "checkpoint-build",
            "--pid", "12345",
            "--file", "/tmp/job.log",
            "--progress-regex", "bars processed",
            "--interval", "120",
            "--notify-every-poll",
        ])

        assert args.command == "watch-job"
        assert args.watch_job_command == "add"
        assert args.target == "maintainer"
        assert args.label == "checkpoint-build"
        assert args.pid == 12345
        assert args.file_path == "/tmp/job.log"
        assert args.progress_regex == "bars processed"
        assert args.interval_seconds == 120
        assert args.notify_every_poll is True

    def test_watch_job_list(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(["watch-job", "list", "--all", "--json", "--include-inactive"])

        assert args.command == "watch-job"
        assert args.watch_job_command == "list"
        assert args.all is True
        assert args.json is True
        assert args.include_inactive is True

    def test_watch_job_cancel(self):
        parser = TestCliParsing()
        args = parser._get_parsed_args(["watch-job", "cancel", "watch123"])

        assert args.command == "watch-job"
        assert args.watch_job_command == "cancel"
        assert args.watch_id == "watch123"
