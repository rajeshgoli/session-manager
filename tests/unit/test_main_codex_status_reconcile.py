from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from src.main import SessionManagerApp
from src.models import Session, SessionStatus


def _make_session(session_id: str, *, provider: str) -> Session:
    return Session(
        id=session_id,
        name=f"{provider}-{session_id}",
        working_dir="/tmp",
        tmux_session=f"{provider}-{session_id}",
        log_file=f"/tmp/{session_id}.log",
        provider=provider,
        status=SessionStatus.RUNNING,
    )


@pytest.mark.asyncio
async def test_handle_status_change_marks_plain_codex_idle_in_message_queue():
    app = SessionManagerApp.__new__(SessionManagerApp)
    session = _make_session("codex1", provider="codex")
    app.session_manager = SimpleNamespace(
        update_session_status=Mock(),
        get_session=lambda session_id: session if session_id == session.id else None,
    )
    app.message_queue = SimpleNamespace(
        mark_session_idle=Mock(),
        mark_session_active=Mock(),
    )

    await app._handle_status_change(session.id, SessionStatus.IDLE)

    app.session_manager.update_session_status.assert_called_once_with(session.id, SessionStatus.IDLE)
    app.message_queue.mark_session_idle.assert_called_once_with(
        session.id,
        completion_transition=True,
    )
    app.message_queue.mark_session_active.assert_not_called()


@pytest.mark.asyncio
async def test_handle_status_change_does_not_reconcile_non_codex_providers():
    app = SessionManagerApp.__new__(SessionManagerApp)
    session = _make_session("claude1", provider="claude")
    app.session_manager = SimpleNamespace(
        update_session_status=Mock(),
        get_session=lambda session_id: session if session_id == session.id else None,
    )
    app.message_queue = SimpleNamespace(
        mark_session_idle=Mock(),
        mark_session_active=Mock(),
    )

    await app._handle_status_change(session.id, SessionStatus.IDLE)

    app.session_manager.update_session_status.assert_called_once_with(session.id, SessionStatus.IDLE)
    app.message_queue.mark_session_idle.assert_not_called()
    app.message_queue.mark_session_active.assert_not_called()
