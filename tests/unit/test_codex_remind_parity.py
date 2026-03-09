from __future__ import annotations

from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock

import pytest

from src.models import Session, SessionStatus
from src.session_manager import SessionManager


@pytest.mark.asyncio
async def test_codex_app_turn_complete_marks_idle_as_completion_transition(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="appturn1",
        name="codex-app-appturn1",
        working_dir=str(tmp_path),
        provider="codex-app",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.message_queue_manager = SimpleNamespace(mark_session_idle=MagicMock())
    manager.notifier = SimpleNamespace(notify=AsyncMock())

    await manager._handle_codex_turn_complete(
        session_id=session.id,
        text="final answer",
        status="completed",
    )

    manager.message_queue_manager.mark_session_idle.assert_called_once_with(
        session.id,
        completion_transition=True,
    )


@pytest.mark.asyncio
async def test_codex_app_review_complete_marks_idle_as_completion_transition(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="appreview1",
        name="codex-app-appreview1",
        working_dir=str(tmp_path),
        provider="codex-app",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.message_queue_manager = SimpleNamespace(mark_session_idle=MagicMock())

    await manager._handle_codex_review_complete(
        session_id=session.id,
        review_text="review output",
    )

    manager.message_queue_manager.mark_session_idle.assert_called_once_with(
        session.id,
        completion_transition=True,
    )
