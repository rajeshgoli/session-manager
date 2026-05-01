"""Regression tests for issue #598: Codex deferred-send submit verification."""

import asyncio
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.models import Session, SessionStatus
from src.session_manager import SessionManager
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


@pytest.mark.asyncio
async def test_send_input_async_interrupts_codex_deferred_send_banner(tmux_controller):
    deferred_pane = """
• Working (14s • esc to interrupt) · 1 background terminal running · /ps to view

• Messages to be submitted after next tool call (press esc to interrupt and send immediately)
  ↳ [Input from: maintainer (83f53095) via sm send]
    busy repro ping: reply exactly DURING_SLEEP_OK

› Summarize recent commits
"""
    subprocess_calls = []

    async def mock_subprocess(*args, **kwargs):
        subprocess_calls.append(args)
        proc = AsyncMock()
        if args[1] == "capture-pane":
            proc.communicate = AsyncMock(return_value=(deferred_pane.encode(), b""))
        else:
            proc.communicate = AsyncMock(return_value=(b"", b""))
        proc.returncode = 0
        return proc

    with patch.object(tmux_controller, "session_exists", return_value=True), \
         patch.object(tmux_controller, "_exit_copy_mode_if_needed_async", new=AsyncMock(return_value=(0, 0))), \
         patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess), \
         patch("asyncio.sleep", new_callable=AsyncMock):
        result = await tmux_controller.send_input_async(
            "codex-test",
            "[Input from: maintainer (83f53095) via sm send]\nbusy repro ping: reply exactly DURING_SLEEP_OK",
            verify_codex_submit=True,
        )

    assert result is True
    escape_calls = [call for call in subprocess_calls if call[1] == "send-keys" and call[-1] == "Escape"]
    assert len(escape_calls) == 1


@pytest.mark.asyncio
async def test_send_input_async_does_not_interrupt_codex_without_deferred_banner(tmux_controller):
    submitted_pane = """
• DURING_SLEEP_OK

  1 background terminal running · /ps to view · /stop to close
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
            "codex-test",
            "busy repro ping: reply exactly DURING_SLEEP_OK",
            verify_codex_submit=True,
        )

    assert result is True
    escape_calls = [call for call in subprocess_calls if call[1] == "send-keys" and call[-1] == "Escape"]
    assert len(escape_calls) == 0


@pytest.mark.asyncio
async def test_send_input_async_ignores_stale_codex_deferred_banner_history(tmux_controller):
    stale_history_pane = """
• Messages to be submitted after next tool call (press esc to interrupt and send immediately)
  ↳ [Input from: maintainer (83f53095) via sm send]
    busy repro ping: reply exactly DURING_SLEEP_OK

... older output omitted ...

• DURING_SLEEP_OK

  1 background terminal running · /ps to view · /stop to close

› Summarize recent commits
"""
    subprocess_calls = []

    async def mock_subprocess(*args, **kwargs):
        subprocess_calls.append(args)
        proc = AsyncMock()
        if args[1] == "capture-pane":
            proc.communicate = AsyncMock(return_value=(stale_history_pane.encode(), b""))
        else:
            proc.communicate = AsyncMock(return_value=(b"", b""))
        proc.returncode = 0
        return proc

    with patch.object(tmux_controller, "session_exists", return_value=True), \
         patch.object(tmux_controller, "_exit_copy_mode_if_needed_async", new=AsyncMock(return_value=(0, 0))), \
         patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess), \
         patch("asyncio.sleep", new_callable=AsyncMock):
        result = await tmux_controller.send_input_async(
            "codex-test",
            "busy repro ping: reply exactly DURING_SLEEP_OK",
            verify_codex_submit=True,
        )

    assert result is True
    escape_calls = [call for call in subprocess_calls if call[1] == "send-keys" and call[-1] == "Escape"]
    assert len(escape_calls) == 0


@pytest.mark.asyncio
async def test_deliver_direct_enables_codex_submit_verification(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="codex598",
        name="codex-codex598",
        working_dir=str(tmp_path),
        tmux_session="codex-codex598",
        provider="codex",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.message_queue_manager = MagicMock()
    manager.message_queue_manager.mark_session_active = MagicMock()

    with patch.object(manager.tmux, "send_input_async", new=AsyncMock(return_value=True)) as mock_send:
        success = await manager._deliver_direct(session, "hello codex")

    assert success is True
    mock_send.assert_awaited_once_with(
        "codex-codex598",
        "hello codex",
        verify_claude_submit=False,
        verify_codex_submit=True,
    )


@pytest.mark.asyncio
async def test_deliver_direct_marks_missing_tmux_runtime_stopped(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="codex697",
        name="codex-codex697",
        working_dir=str(tmp_path),
        tmux_session="codex-codex697",
        provider="codex",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.tmux.send_input_async = AsyncMock(return_value=False)
    manager.tmux.session_exists = MagicMock(return_value=False)
    manager.tmux.get_session_exit_diagnostics = MagicMock(
        return_value={
            "session_name": "codex-codex697",
            "exists": False,
            "pane_dead": False,
        }
    )

    success = await manager._deliver_direct(session, "hello codex")

    assert success is False
    assert session.status == SessionStatus.STOPPED
    assert session.error_message == "Tmux session codex-codex697 disappeared before delivery"
