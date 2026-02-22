"""Unit tests for cmd_watch (#289)."""

import os
from unittest.mock import MagicMock, patch

import pytest

from src.cli.commands import cmd_watch
from src.cli.main import main


def test_cmd_watch_rejects_non_positive_interval():
    client = MagicMock()
    with patch.dict(os.environ, {}, clear=False):
        rc = cmd_watch(client, repo=None, role=None, interval=0.0)
    assert rc == 1


def test_cmd_watch_delegates_to_watch_tui():
    client = MagicMock()
    with patch.dict(os.environ, {"CLAUDE_SESSION_MANAGER_ID": ""}, clear=False):
        with patch("src.cli.watch_tui.run_watch_tui", return_value=0) as mock_run:
            rc = cmd_watch(client, repo="/tmp/repo", role="engineer", interval=2.5)

    assert rc == 0
    mock_run.assert_called_once_with(
        client=client,
        repo_filter="/tmp/repo",
        role_filter="engineer",
        interval=2.5,
    )


def test_cmd_watch_rejects_managed_session(capsys):
    client = MagicMock()
    with patch.dict(os.environ, {"CLAUDE_SESSION_MANAGER_ID": "abc12345"}, clear=False):
        rc = cmd_watch(client, repo=None, role=None, interval=2.0)

    assert rc == 1
    assert "operator-only" in capsys.readouterr().err


def test_main_watch_rejects_managed_session_before_dispatch():
    with patch.dict(os.environ, {"CLAUDE_SESSION_MANAGER_ID": "abc12345"}, clear=False):
        with patch("sys.argv", ["sm", "watch"]):
            with patch("src.cli.main.commands.cmd_watch") as mock_cmd_watch:
                with pytest.raises(SystemExit) as exc_info:
                    main()

    assert exc_info.value.code == 1
    mock_cmd_watch.assert_not_called()
