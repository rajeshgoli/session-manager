"""Unit tests for direct session creation commands."""

from unittest.mock import MagicMock, patch

from src.cli.commands import cmd_codex_2, cmd_new


def test_cmd_new_passes_parent_session_id(tmp_path):
    client = MagicMock()
    client.create_session_result.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "id": "child1234",
            "provider": "codex-fork",
            "tmux_session": "codex-fork-child1234",
        },
    }

    with patch("subprocess.run") as run_mock, patch("time.sleep"):
        rc = cmd_new(
            client,
            working_dir=str(tmp_path),
            provider="codex-fork",
            parent_session_id="parent123",
        )

    assert rc == 0
    client.create_session_result.assert_called_once_with(
        str(tmp_path),
        provider="codex-fork",
        parent_session_id="parent123",
    )
    run_mock.assert_called_once_with(["tmux", "attach", "-t", "codex-fork-child1234"], check=True)


def test_cmd_codex_2_passes_parent_session_id(tmp_path):
    client = MagicMock()
    client.create_session_result.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "id": "child5678",
            "provider": "codex-fork",
        },
    }

    with patch("src.cli.commands.cmd_attach", return_value=0) as attach_mock:
        rc = cmd_codex_2(client, working_dir=str(tmp_path), parent_session_id="parent123")

    assert rc == 0
    client.create_session_result.assert_called_once_with(
        str(tmp_path),
        provider="codex-fork",
        parent_session_id="parent123",
    )
    attach_mock.assert_called_once_with(client, "child5678")


def test_cmd_new_surfaces_create_error_detail(tmp_path, capsys):
    client = MagicMock()
    client.create_session_result.return_value = {
        "ok": False,
        "unavailable": False,
        "detail": "Configured codex-fork runtime unavailable: Launch command does not exist: /missing/codex",
    }

    rc = cmd_new(client, working_dir=str(tmp_path), provider="codex-fork")

    assert rc == 1
    assert "Configured codex-fork runtime unavailable" in capsys.readouterr().err
