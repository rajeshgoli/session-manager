"""Regression coverage for issue #484: same-tmux forks must be able to cancel inherited recurring reminders."""

from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.message_queue import MessageQueueManager
from src.models import Session, SessionStatus


def noop_create_task(coro):
    """Silently close coroutine without running it."""
    coro.close()
    return MagicMock()


def _close_message_queue(mq: MessageQueueManager) -> None:
    """Release SQLite handles and background tasks for repeatable tests."""
    for task in list(getattr(mq, "_scheduled_tasks", {}).values()):
        cancel = getattr(task, "cancel", None)
        if callable(cancel):
            cancel()
    mq._scheduled_tasks.clear()
    if mq._db_conn is not None:
        mq._db_conn.close()
        mq._db_conn = None


@pytest.mark.asyncio
async def test_forked_session_can_cancel_inherited_recurring_reminder(tmp_path):
    """Recurring reminder delivery includes the reminder ID so a same-tmux fork can cancel it."""
    original = Session(
        id="f72825ce",
        name="claude-f72825ce",
        tmux_session="claude-shared",
        status=SessionStatus.RUNNING,
    )
    forked = Session(
        id="4958edf4",
        name="claude-4958edf4",
        tmux_session="claude-shared",
        status=SessionStatus.RUNNING,
    )

    session_manager = MagicMock()
    session_manager.sessions = {original.id: original, forked.id: forked}
    session_manager.get_session = MagicMock(side_effect=lambda session_id: session_manager.sessions.get(session_id))
    session_manager.tmux = MagicMock()
    session_manager.tmux.send_input_async = AsyncMock(return_value=True)
    session_manager._save_state = MagicMock()
    session_manager._deliver_direct = AsyncMock(return_value=True)

    mq = MessageQueueManager(
        session_manager=session_manager,
        db_path=str(tmp_path / "issue_484.db"),
        config={
            "sm_send": {
                "input_poll_interval": 1,
                "input_stale_timeout": 30,
                "max_batch_size": 10,
                "urgent_delay_ms": 100,
            },
            "timeouts": {
                "message_queue": {
                    "subprocess_timeout_seconds": 1,
                    "async_send_timeout_seconds": 2,
                }
            },
        },
        notifier=None,
    )

    try:
        with patch("asyncio.create_task", noop_create_task):
            reminder_id = await mq.schedule_reminder(
                session_id=original.id,
                delay_seconds=30,
                message="Check pod watchdog",
                recurring_interval_seconds=30,
            )

        fired = []

        def fake_queue_message(target_session_id, text, delivery_mode="sequential", **kwargs):
            fired.append((target_session_id, text, delivery_mode))
            return MagicMock()

        with patch.object(mq, "queue_message", side_effect=fake_queue_message):
            with patch.object(mq, "_schedule_reminder_task"):
                with patch("asyncio.sleep", AsyncMock()):
                    await mq._fire_reminder(
                        reminder_id=reminder_id,
                        session_id=original.id,
                        message="Check pod watchdog",
                        delay_seconds=0,
                        recurring_interval_seconds=30,
                    )

        assert fired == [
            (
                original.id,
                f"[sm] Recurring reminder: ({reminder_id})\nCheck pod watchdog\n[sm] Cancel: sm remind cancel {reminder_id}",
                "urgent",
            )
        ]

        cancelled = mq.cancel_scheduled_reminder(reminder_id)

        assert cancelled is not None
        assert cancelled["target_session_id"] == original.id
        row = mq._execute_query(
            "SELECT is_active FROM scheduled_reminders WHERE id = ?",
            (reminder_id,),
        )[0]
        assert row == (0,)
    finally:
        _close_message_queue(mq)
