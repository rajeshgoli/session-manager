import subprocess
from unittest.mock import MagicMock

from src.tmux_controller import TmuxController


def test_set_status_bar_passes_timeout_to_tmux(monkeypatch):
    controller = TmuxController()
    monkeypatch.setattr(controller, "session_exists", lambda _: True)
    run_tmux = MagicMock(return_value=MagicMock(returncode=0))
    monkeypatch.setattr(controller, "_run_tmux", run_tmux)

    ok = controller.set_status_bar("claude-test123", "friendly", timeout_seconds=1.0)

    assert ok is True
    run_tmux.assert_called_once_with(
        "set-option",
        "-t",
        "claude-test123",
        "status-left",
        "[friendly] ",
        timeout=1.0,
    )


def test_set_status_bar_returns_false_on_timeout(monkeypatch):
    controller = TmuxController()
    monkeypatch.setattr(controller, "session_exists", lambda _: True)

    def _raise_timeout(*args, **kwargs):
        raise subprocess.TimeoutExpired(cmd=["tmux", "set-option"], timeout=1.0)

    monkeypatch.setattr(controller, "_run_tmux", _raise_timeout)

    ok = controller.set_status_bar("claude-test123", "friendly", timeout_seconds=1.0)

    assert ok is False
