"""Regression tests for issue #410: Claude multiline submit verification."""

import asyncio
from unittest.mock import AsyncMock, patch

import pytest

from src.tmux_controller import TmuxController


@pytest.fixture
def tmux_controller():
    config = {
        "timeouts": {
            "tmux": {
                "send_keys_timeout_seconds": 2,
                "send_keys_settle_seconds": 0.01,
                "submit_verify_seconds": 0.01,
                "submit_retry_seconds": 0.01,
            }
        }
    }
    return TmuxController(log_dir="/tmp/test-sessions", config=config)


def test_extract_active_claude_composer_text_detects_pending_payload(tmux_controller):
    pane = """
────────────────────────────────────────────────────────────────────────────────
❯ alpha beta gamma delta epsilon
  zeta eta theta
────────────────────────────────────────────────────────────────────────────────
  0% ctx
"""
    assert (
        tmux_controller._extract_active_claude_composer_text(pane)
        == "alpha beta gamma delta epsilon zeta eta theta"
    )


def test_extract_active_claude_composer_text_ignores_empty_prompt(tmux_controller):
    pane = """
✳ Orbiting…
  ⎿  Tip: Run /install-slack-app to use Claude in Slack

────────────────────────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────────────────────────
  0% ctx
"""
    assert tmux_controller._extract_active_claude_composer_text(pane) is None


@pytest.mark.asyncio
async def test_send_input_async_retries_enter_when_composer_stays_populated(tmux_controller):
    stuck_pane = """
────────────────────────────────────────────────────────────────────────────────
❯ alpha beta gamma delta epsilon zeta eta theta
  iota kappa lambda mu
────────────────────────────────────────────────────────────────────────────────
  0% ctx
"""
    subprocess_calls = []

    async def mock_subprocess(*args, **kwargs):
        subprocess_calls.append(args)
        proc = AsyncMock()
        if args[1] == "capture-pane":
            proc.communicate = AsyncMock(return_value=(stuck_pane.encode(), b""))
        else:
            proc.communicate = AsyncMock(return_value=(b"", b""))
        proc.returncode = 0
        return proc

    with patch.object(tmux_controller, "session_exists", return_value=True), \
         patch.object(tmux_controller, "_exit_copy_mode_if_needed_async", new=AsyncMock(return_value=(0, 0))), \
         patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess), \
         patch("asyncio.sleep", new_callable=AsyncMock):
        result = await tmux_controller.send_input_async(
            "claude-test",
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu",
            verify_claude_submit=True,
        )

    assert result is True
    enter_calls = [call for call in subprocess_calls if call[1] == "send-keys" and call[-1] == "Enter"]
    assert len(enter_calls) == 2


@pytest.mark.asyncio
async def test_capture_pane_async_uses_history_and_join_flags(tmux_controller):
    """Verification should inspect pane history and join wrapped lines."""
    subprocess_calls = []

    async def mock_subprocess(*args, **kwargs):
        subprocess_calls.append(args)
        proc = AsyncMock()
        proc.communicate = AsyncMock(return_value=(b"pane output", b""))
        proc.returncode = 0
        return proc

    with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
        result = await tmux_controller._capture_pane_async("claude-test")

    assert result == "pane output"
    assert subprocess_calls == [
        (
            "tmux",
            "capture-pane",
            "-p",
            "-J",
            "-S",
            "-200",
            "-t",
            "claude-test",
        )
    ]


@pytest.mark.asyncio
async def test_send_input_async_does_not_retry_when_composer_is_empty(tmux_controller):
    submitted_pane = """
✳ Orbiting…
  ⎿  Tip: Run /install-slack-app to use Claude in Slack

────────────────────────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────────────────────────
  0% ctx
"""
    subprocess_calls = []

    async def mock_subprocess(*args, **kwargs):
        subprocess_calls.append(args)
        proc = AsyncMock()
        if args[1] == "capture-pane":
            proc.communicate = AsyncMock(return_value=(submitted_pane.encode(), b""))
        else:
            proc.communicate = AsyncMock(return_value=(b"", b""))
        proc.returncode = 0
        return proc

    with patch.object(tmux_controller, "session_exists", return_value=True), \
         patch.object(tmux_controller, "_exit_copy_mode_if_needed_async", new=AsyncMock(return_value=(0, 0))), \
         patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess), \
         patch("asyncio.sleep", new_callable=AsyncMock):
        result = await tmux_controller.send_input_async(
            "claude-test",
            "alpha beta gamma",
            verify_claude_submit=True,
        )

    assert result is True
    enter_calls = [call for call in subprocess_calls if call[1] == "send-keys" and call[-1] == "Enter"]
    assert len(enter_calls) == 1


@pytest.mark.asyncio
async def test_send_input_async_does_not_retry_for_queued_message_placeholder(tmux_controller):
    queued_pane = """
────────────────────────────────────────────────────────────────────────────────
❯ Press up to edit queued messages
────────────────────────────────────────────────────────────────────────────────
  3% ctx
"""
    subprocess_calls = []

    async def mock_subprocess(*args, **kwargs):
        subprocess_calls.append(args)
        proc = AsyncMock()
        if args[1] == "capture-pane":
            proc.communicate = AsyncMock(return_value=(queued_pane.encode(), b""))
        else:
            proc.communicate = AsyncMock(return_value=(b"", b""))
        proc.returncode = 0
        return proc

    with patch.object(tmux_controller, "session_exists", return_value=True), \
         patch.object(tmux_controller, "_exit_copy_mode_if_needed_async", new=AsyncMock(return_value=(0, 0))), \
         patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess), \
         patch("asyncio.sleep", new_callable=AsyncMock):
        result = await tmux_controller.send_input_async(
            "claude-test",
            "alpha beta gamma",
            verify_claude_submit=True,
        )

    assert result is True
    enter_calls = [call for call in subprocess_calls if call[1] == "send-keys" and call[-1] == "Enter"]
    assert len(enter_calls) == 1
