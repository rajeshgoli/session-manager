"""Tests for asynchronous Telegram topic title convergence."""

from __future__ import annotations

from datetime import datetime
from unittest.mock import AsyncMock, MagicMock

import pytest

from src.main import SessionManagerApp
from src.models import Session, SessionStatus


def _session() -> Session:
    return Session(
        id="sess-title",
        name="claude-sess-title",
        working_dir="/tmp",
        tmux_session="claude-sess-title",
        status=SessionStatus.RUNNING,
        telegram_chat_id=123,
        telegram_thread_id=456,
        created_at=datetime.now(),
        last_activity=datetime.now(),
    )


@pytest.mark.asyncio
async def test_telegram_topic_title_retry_marks_identity_synced_on_success():
    session = _session()
    app = SessionManagerApp.__new__(SessionManagerApp)
    app.notifier = MagicMock()
    app.notifier.rename_session_topic = AsyncMock(side_effect=[False, True])
    app.session_manager = MagicMock()
    app.session_manager.get_session.return_value = session
    app.session_manager._save_state = MagicMock()

    await app._sync_telegram_topic_title_with_retries(
        session.id,
        "3175-consultant",
        attempts=2,
    )

    assert app.notifier.rename_session_topic.await_count == 2
    assert session.display_identity_synced_name == "3175-consultant"
    assert session.display_identity_synced_chat_id == 123
    assert session.display_identity_synced_thread_id == 456
    app.session_manager._save_state.assert_called_once()


@pytest.mark.asyncio
async def test_restore_monitoring_queues_telegram_topic_title_sync():
    session = _session()
    app = SessionManagerApp.__new__(SessionManagerApp)
    app.session_manager = MagicMock()
    app.session_manager.list_sessions.return_value = [session]
    app.session_manager.get_effective_session_name.return_value = "3175-super-em"
    app.session_manager.tmux.set_status_bar.return_value = True
    app.output_monitor = MagicMock()
    app.output_monitor.start_monitoring = AsyncMock()
    app._queue_telegram_topic_title_sync = MagicMock()

    await app._restore_monitoring()

    app.session_manager.tmux.set_status_bar.assert_called_once_with(
        session.tmux_session,
        "3175-super-em",
    )
    app._queue_telegram_topic_title_sync.assert_called_once_with(
        session.id,
        "3175-super-em",
    )
