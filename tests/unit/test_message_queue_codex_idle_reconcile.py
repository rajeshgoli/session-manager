from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock, Mock, patch

import pytest

from src.message_queue import MessageQueueManager
from src.models import Session, SessionStatus


def _make_session(session_id: str) -> Session:
    return Session(
        id=session_id,
        name=f"codex-{session_id}",
        working_dir="/tmp",
        tmux_session=f"codex-{session_id}",
        log_file=f"/tmp/{session_id}.log",
        provider="codex",
        status=SessionStatus.RUNNING,
    )


def _noop_create_task(coro):
    coro.close()
    return MagicMock()


@pytest.mark.asyncio
async def test_reconcile_codex_idle_after_delivery_marks_queue_idle(tmp_path: Path):
    session = _make_session("codex-idle-1")
    sessions = {session.id: session}
    session_manager = SimpleNamespace(
        get_session=lambda session_id: sessions.get(session_id),
        _save_state=Mock(),
    )
    mq = MessageQueueManager(session_manager, db_path=str(tmp_path / "mq.db"))
    mq.watch_poll_interval = 0
    mq._get_or_create_state(session.id).is_idle = False

    prompt_results = iter([True, True])

    async def fake_prompt(_tmux_session: str) -> bool:
        return next(prompt_results)

    mq._check_idle_prompt = fake_prompt  # type: ignore[method-assign]

    with patch("asyncio.create_task", side_effect=_noop_create_task):
        await mq._reconcile_codex_idle_after_delivery(session.id)

    assert mq.delivery_states[session.id].is_idle is True
    assert session.status == SessionStatus.IDLE
    session_manager._save_state.assert_called_once()


@pytest.mark.asyncio
async def test_successful_plain_codex_delivery_schedules_idle_reconcile(tmp_path: Path):
    session = _make_session("codex-idle-2")
    sessions = {session.id: session}
    session_manager = SimpleNamespace(
        get_session=lambda session_id: sessions.get(session_id),
        _deliver_direct=AsyncMock(return_value=True),
        _save_state=Mock(),
    )
    mq = MessageQueueManager(session_manager, db_path=str(tmp_path / "mq.db"))
    mq._schedule_codex_idle_reconcile = Mock()
    mq._get_pending_user_input_async = AsyncMock(return_value=None)

    mq.queue_message(
        target_session_id=session.id,
        text="Reply exactly Done.",
        delivery_mode="sequential",
        trigger_delivery=False,
    )
    mq._prepare_nonurgent_delivery(session.id)

    await mq._try_deliver_messages(session.id)

    mq._schedule_codex_idle_reconcile.assert_called_once_with(session.id)
