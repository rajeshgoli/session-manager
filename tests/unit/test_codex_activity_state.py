"""Unit tests for codex-app activity-state computation."""

from __future__ import annotations

import tempfile
from datetime import datetime, timedelta

from src.models import CompletionStatus, Session, SessionStatus
from src.session_manager import SessionManager


def _make_manager() -> SessionManager:
    tmpdir = tempfile.TemporaryDirectory()
    manager = SessionManager(log_dir=tmpdir.name, state_file=f"{tmpdir.name}/state.json")
    manager._tmpdir = tmpdir  # keep alive for test scope
    return manager


def test_activity_state_for_non_codex_app_follows_status():
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

    assert manager.get_activity_state(session.id) == "working"
    session.status = SessionStatus.IDLE
    assert manager.get_activity_state(session.id) == "idle"


def test_codex_app_activity_thinking_and_working_transitions():
    manager = _make_manager()
    session = Session(
        id="codex1",
        name="codex-app-codex1",
        working_dir="/tmp",
        provider="codex-app",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    manager.codex_turns_in_flight.add(session.id)
    assert manager.get_activity_state(session.id) == "thinking"

    manager.codex_last_delta_at[session.id] = datetime.now()
    assert manager.get_activity_state(session.id) == "working"


def test_codex_app_wait_states_and_waiting_input():
    manager = _make_manager()
    session = Session(
        id="codex2",
        name="codex-app-codex2",
        working_dir="/tmp",
        provider="codex-app",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    manager.codex_wait_states[session.id] = ("waiting_permission", datetime.now())
    assert manager.get_activity_state(session.id) == "waiting_permission"

    manager.codex_wait_states[session.id] = (
        "waiting_input",
        datetime.now() - timedelta(seconds=20),
    )
    assert manager.get_activity_state(session.id) == "idle"

    session.completion_status = CompletionStatus.COMPLETED
    assert manager.get_activity_state(session.id) == "waiting_input"
