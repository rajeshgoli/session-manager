import subprocess
from unittest.mock import AsyncMock, MagicMock

import pytest

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


def test_codex_rename_prompt_detection():
    controller = TmuxController()

    assert controller._looks_like_codex_rename_prompt("Name thread\nPress enter to confirm or esc to go back")
    assert controller._looks_like_codex_rename_prompt("Rename thread\nPress enter to confirm or esc to go back")
    assert not controller._looks_like_codex_rename_prompt("› /rename worker")


def test_codex_rename_prompt_detection_uses_active_region_only():
    controller = TmuxController()
    pane_text = """Name thread
old-name
Press enter to confirm or esc to go back

› normal prompt text

  gpt-5.5 xhigh · ~/repo
"""

    active_region = controller._extract_active_codex_region(pane_text)

    assert active_region is not None
    assert "normal prompt text" in active_region
    assert not controller._looks_like_codex_rename_prompt(active_region)


@pytest.mark.asyncio
async def test_rename_codex_thread_uses_interactive_dialog(monkeypatch):
    controller = TmuxController()
    monkeypatch.setattr(controller, "session_exists", lambda _: True)
    exit_copy = AsyncMock(return_value=(0, 0))
    send_key = AsyncMock(return_value=True)
    send_input = AsyncMock(return_value=True)
    capture = AsyncMock(return_value="Rename thread\nworker-old\nPress enter to confirm or esc to go back")
    monkeypatch.setattr(controller, "_exit_copy_mode_if_needed_async", exit_copy)
    monkeypatch.setattr(controller, "send_key_async", send_key)
    monkeypatch.setattr(controller, "send_input_async", send_input)
    monkeypatch.setattr(controller, "_capture_pane_async", capture)

    ok = await controller.rename_codex_thread_async("codex-test", "worker-new")

    assert ok is True
    exit_copy.assert_awaited_once_with("codex-test")
    assert [call.args for call in send_key.await_args_list] == [("codex-test", "C-u"), ("codex-test", "C-u")]
    assert [call.args for call in send_input.await_args_list] == [("codex-test", "/rename"), ("codex-test", "worker-new")]
