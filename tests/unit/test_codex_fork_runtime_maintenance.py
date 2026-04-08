from __future__ import annotations

from unittest.mock import AsyncMock, Mock

import pytest

from src.models import Session, SessionStatus
from src.session_manager import SessionManager


def test_prune_codex_fork_runtime_artifacts_removes_dead_files(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    live = Session(
        id="live1234",
        name="codex-fork-live1234",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
        log_file=str(tmp_path / "live1234.log"),
        provider_resume_id="resume-live1234",
    )
    dead = Session(
        id="dead1234",
        name="codex-fork-dead1234",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.STOPPED,
        log_file=str(tmp_path / "dead1234.log"),
        provider_resume_id="resume-dead1234",
    )
    manager.sessions[live.id] = live
    manager.sessions[dead.id] = dead
    manager.tmux.session_exists = lambda name: name == live.tmux_session

    live_event = manager._codex_fork_event_stream_path(live)
    dead_event = manager._codex_fork_event_stream_path(dead)
    orphan_event = tmp_path / "orphan123.codex-fork.events.jsonl"
    live_socket = manager._codex_fork_control_socket_path(live)
    dead_socket = manager._codex_fork_control_socket_path(dead)
    orphan_socket = tmp_path / "orphan123.codex-fork.control.sock"
    for path in (live_event, dead_event, orphan_event, live_socket, dead_socket, orphan_socket):
        path.write_text("x")

    removed = sorted(manager.prune_codex_fork_runtime_artifacts())

    assert removed == sorted(
        [
            dead_event.name,
            dead_socket.name,
            orphan_event.name,
            orphan_socket.name,
        ]
    )
    assert live_event.exists()
    assert live_socket.exists()
    assert not dead_event.exists()
    assert not dead_socket.exists()
    assert not orphan_event.exists()
    assert not orphan_socket.exists()


@pytest.mark.asyncio
async def test_maintain_codex_fork_runtime_artifacts_restarts_live_session_when_all_bridge_files_are_missing(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="heal1234",
        name="codex-fork-heal1234",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
        log_file=str(tmp_path / "heal1234.log"),
        provider_resume_id="resume-heal1234",
    )
    manager.sessions[session.id] = session
    manager.tmux.session_exists = Mock(return_value=True)
    manager.tmux.kill_session = Mock(return_value=True)
    manager.tmux.create_session_with_command = Mock(return_value=True)
    manager._stop_codex_fork_event_monitor = AsyncMock()
    manager._start_codex_fork_event_monitor = Mock()

    result = await manager.maintain_codex_fork_runtime_artifacts()

    assert result["healed"] == [session.id]
    assert result["degraded"] == []
    manager.tmux.kill_session.assert_called_once_with(session.tmux_session)
    manager.tmux.create_session_with_command.assert_called_once()
    _, kwargs = manager.tmux.create_session_with_command.call_args
    assert kwargs["session_id"] == session.id
    assert kwargs["command"] == manager.codex_fork_command
    assert kwargs["args"][:2] == ["resume", "resume-heal1234"]
    assert "--event-stream" in kwargs["args"]
    assert "--control-socket" in kwargs["args"]
    manager._start_codex_fork_event_monitor.assert_called_once_with(session)
    assert session.error_message is None
    assert session.status == SessionStatus.RUNNING


@pytest.mark.asyncio
async def test_maintain_codex_fork_runtime_artifacts_marks_control_only_loss_degraded_without_restart(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="control123",
        name="codex-fork-control123",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
        log_file=str(tmp_path / "control123.log"),
        provider_resume_id="resume-control123",
    )
    manager.sessions[session.id] = session
    manager.tmux.session_exists = Mock(return_value=True)
    manager.tmux.kill_session = Mock(return_value=True)
    manager.tmux.create_session_with_command = Mock(return_value=True)
    manager._stop_codex_fork_event_monitor = AsyncMock()
    manager._start_codex_fork_event_monitor = Mock()
    manager._codex_fork_event_stream_path(session).write_text("{}\n")

    result = await manager.maintain_codex_fork_runtime_artifacts()

    assert result["healed"] == []
    assert result["degraded"] == [session.id]
    manager.tmux.kill_session.assert_not_called()
    manager.tmux.create_session_with_command.assert_not_called()
    manager._start_codex_fork_event_monitor.assert_not_called()
    assert session.error_message is not None
    assert session.error_message.startswith("codex_fork_control_degraded: control socket missing:")


@pytest.mark.asyncio
async def test_maintain_codex_fork_runtime_artifacts_marks_unhealable_session_degraded(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="stuck123",
        name="codex-fork-stuck123",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
        log_file=str(tmp_path / "stuck123.log"),
    )
    manager.sessions[session.id] = session
    manager.tmux.session_exists = Mock(return_value=True)
    manager.tmux.create_session_with_command = Mock(return_value=True)
    manager._stop_codex_fork_event_monitor = AsyncMock()
    manager._start_codex_fork_event_monitor = Mock()

    result = await manager.maintain_codex_fork_runtime_artifacts()

    assert result["healed"] == []
    assert result["degraded"] == [session.id]
    manager.tmux.create_session_with_command.assert_not_called()
    assert session.error_message is not None
    assert session.error_message.startswith("codex_fork_runtime_artifacts_missing: event_stream, control_socket")


def test_kill_session_removes_codex_fork_event_stream(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="kill1234",
        name="codex-fork-kill1234",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
        log_file=str(tmp_path / "kill1234.log"),
        provider_resume_id="resume-kill1234",
    )
    manager.sessions[session.id] = session
    manager.tmux.kill_session = Mock(return_value=True)
    event_stream_path = manager._codex_fork_event_stream_path(session)
    control_socket_path = manager._codex_fork_control_socket_path(session)
    event_stream_path.write_text("payload")
    control_socket_path.write_text("payload")

    assert manager.kill_session(session.id) is True
    assert not event_stream_path.exists()
    assert not control_socket_path.exists()
