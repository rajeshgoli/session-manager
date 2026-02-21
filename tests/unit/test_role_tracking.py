"""Unit tests for role tracking behavior (#287)."""

from __future__ import annotations

import tempfile
from unittest.mock import Mock

import pytest

from src.models import DeliveryResult, Session, SessionStatus
from src.session_manager import SessionManager


def _make_manager() -> SessionManager:
    tmpdir = tempfile.TemporaryDirectory()
    manager = SessionManager(log_dir=tmpdir.name, state_file=f"{tmpdir.name}/state.json")
    manager._tmpdir = tmpdir  # keep tempdir alive for test scope
    return manager


def _make_session(session_id: str = "role1234") -> Session:
    return Session(
        id=session_id,
        name=f"claude-{session_id}",
        working_dir="/tmp",
        tmux_session=f"claude-{session_id}",
        provider="claude",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
    )


def test_set_role_and_clear_role_roundtrip():
    manager = _make_manager()
    session = _make_session()
    manager.sessions[session.id] = session

    assert manager.set_role(session.id, "engineer") is True
    assert session.role == "engineer"

    assert manager.clear_role(session.id) is True
    assert session.role is None


def test_detect_role_from_prompt_case_insensitive():
    assert SessionManager.detect_role_from_prompt("As engineer, handle this task.") == "engineer"
    assert SessionManager.detect_role_from_prompt("please act As Architect and review") == "architect"
    assert SessionManager.detect_role_from_prompt("no role here") is None


@pytest.mark.asyncio
async def test_send_input_detects_role_when_unset():
    manager = _make_manager()
    session = _make_session("det00001")
    manager.sessions[session.id] = session

    queue_mgr = Mock()
    queue_mgr.delivery_states = {}
    queue_mgr.queue_message = Mock()
    manager.message_queue_manager = queue_mgr

    result = await manager.send_input(session.id, "As engineer, implement ticket 287.")

    assert result == DeliveryResult.DELIVERED
    assert session.role == "engineer"
    queue_mgr.queue_message.assert_called_once()


@pytest.mark.asyncio
async def test_send_input_does_not_override_existing_role():
    manager = _make_manager()
    session = _make_session("det00002")
    session.role = "architect"
    manager.sessions[session.id] = session

    queue_mgr = Mock()
    queue_mgr.delivery_states = {}
    queue_mgr.queue_message = Mock()
    manager.message_queue_manager = queue_mgr

    result = await manager.send_input(session.id, "As engineer, implement ticket 287.")

    assert result == DeliveryResult.DELIVERED
    assert session.role == "architect"
    queue_mgr.queue_message.assert_called_once()
