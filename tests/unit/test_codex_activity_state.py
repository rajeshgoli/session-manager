"""Unit tests for activity-state computation (#288)."""

from __future__ import annotations

import tempfile
from datetime import datetime, timedelta
from types import SimpleNamespace

from src.models import CompletionStatus, MonitorState, Session, SessionDeliveryState, SessionStatus
from src.session_manager import SessionManager


def _make_manager() -> SessionManager:
    tmpdir = tempfile.TemporaryDirectory()
    manager = SessionManager(log_dir=tmpdir.name, state_file=f"{tmpdir.name}/state.json")
    manager._tmpdir = tmpdir  # keep alive for test scope
    return manager


def test_non_codex_activity_uses_queue_and_monitor_signals():
    manager = _make_manager()
    session = Session(
        id="s1",
        name="claude-s1",
        working_dir="/tmp",
        tmux_session="claude-s1",
        provider="claude",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.message_queue_manager = SimpleNamespace(
        delivery_states={session.id: SessionDeliveryState(session_id=session.id, is_idle=False)}
    )
    manager.output_monitor = SimpleNamespace(
        get_session_state=lambda _sid: MonitorState(is_output_flowing=True)
    )

    assert manager.get_activity_state(session.id) == "working"
    manager.output_monitor = SimpleNamespace(
        get_session_state=lambda _sid: MonitorState(is_output_flowing=False)
    )
    assert manager.get_activity_state(session.id) == "thinking"

    manager.message_queue_manager.delivery_states[session.id].is_idle = True
    assert manager.get_activity_state(session.id) == "idle"


def test_non_codex_waiting_permission_and_waiting_input_precedence():
    manager = _make_manager()
    session = Session(
        id="perm1",
        name="claude-perm1",
        working_dir="/tmp",
        provider="claude",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.message_queue_manager = SimpleNamespace(
        delivery_states={session.id: SessionDeliveryState(session_id=session.id, is_idle=False)}
    )
    manager.output_monitor = SimpleNamespace(
        get_session_state=lambda _sid: MonitorState(is_output_flowing=False, last_pattern="permission")
    )

    assert manager.get_activity_state(session.id) == "waiting_permission"

    session.completion_status = CompletionStatus.COMPLETED
    manager.output_monitor = SimpleNamespace(
        get_session_state=lambda _sid: MonitorState(is_output_flowing=False, last_pattern=None)
    )
    assert manager.get_activity_state(session.id) == "waiting_input"


def test_non_codex_fallback_without_hook_data_uses_last_activity():
    manager = _make_manager()
    session = Session(
        id="fallback1",
        name="claude-fallback1",
        working_dir="/tmp",
        provider="claude",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    session.last_activity = datetime.now() - timedelta(seconds=5)
    assert manager.get_activity_state(session.id) == "thinking"

    session.last_activity = datetime.now() - timedelta(seconds=45)
    assert manager.get_activity_state(session.id) == "idle"


def test_codex_app_uses_queue_tristate_and_completion():
    manager = _make_manager()
    session = Session(
        id="codex2",
        name="codex-app-codex2",
        working_dir="/tmp",
        provider="codex-app",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.message_queue_manager = SimpleNamespace(
        delivery_states={session.id: SessionDeliveryState(session_id=session.id, is_idle=False)}
    )

    assert manager.get_activity_state(session.id) == "working"

    manager.message_queue_manager.delivery_states[session.id].is_idle = True
    assert manager.get_activity_state(session.id) == "idle"

    session.completion_status = CompletionStatus.COMPLETED
    assert manager.get_activity_state(session.id) == "waiting_input"


def test_codex_app_fallback_without_hook_data():
    manager = _make_manager()
    session = Session(
        id="codex3",
        name="codex-app-codex3",
        working_dir="/tmp",
        provider="codex-app",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    session.last_activity = datetime.now() - timedelta(seconds=2)
    assert manager.get_activity_state(session.id) == "thinking"

    session.last_activity = datetime.now() - timedelta(seconds=40)
    assert manager.get_activity_state(session.id) == "idle"
