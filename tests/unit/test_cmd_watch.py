"""Unit tests for cmd_watch (#289)."""

from unittest.mock import MagicMock, patch

from src.cli.commands import cmd_watch


def test_cmd_watch_rejects_non_positive_interval():
    client = MagicMock()
    rc = cmd_watch(client, repo=None, role=None, interval=0.0)
    assert rc == 1


def test_cmd_watch_delegates_to_watch_tui():
    client = MagicMock()
    with patch("src.cli.watch_tui.run_watch_tui", return_value=0) as mock_run:
        rc = cmd_watch(client, repo="/tmp/repo", role="engineer", interval=2.5)

    assert rc == 0
    mock_run.assert_called_once_with(
        client=client,
        repo_filter="/tmp/repo",
        role_filter="engineer",
        interval=2.5,
    )
