from unittest.mock import MagicMock, patch

from src.cli.commands import cmd_kill


def test_cmd_kill_uses_retire_language(capsys):
    client = MagicMock()
    client.kill_session.return_value = {"status": "killed", "session_id": "child123"}

    with patch(
        "src.cli.commands.resolve_session_id",
        return_value=("child123", {"id": "child123", "friendly_name": "agent-one"}),
    ):
        rc = cmd_kill(client, "parent123", "agent-one")

    assert rc == 0
    assert "Session agent-one (child123) retired" in capsys.readouterr().out
    client.kill_session.assert_called_once_with(
        requester_session_id="parent123",
        target_session_id="child123",
    )


def test_cmd_kill_failure_uses_retire_language(capsys):
    client = MagicMock()
    client.kill_session.return_value = {"status": "error"}

    with patch(
        "src.cli.commands.resolve_session_id",
        return_value=("child123", {"id": "child123", "friendly_name": "agent-one"}),
    ):
        rc = cmd_kill(client, "parent123", "agent-one")

    assert rc == 1
    assert "Failed to retire session" in capsys.readouterr().err
