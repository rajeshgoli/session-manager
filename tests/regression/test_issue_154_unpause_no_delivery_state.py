"""
Regression tests for issue #154: unpause_session skips retry when no
delivery state exists.

When a session is paused and an urgent message is queued, _deliver_urgent
returns early (paused) without creating a delivery state entry. On unpause,
delivery_states.get() returns None and _try_deliver_messages is never
scheduled — the message sits in the queue undelivered.

Fix: unpause_session() checks for pending messages directly instead of
relying on delivery state existence.
"""

import asyncio
from unittest.mock import MagicMock, AsyncMock, patch

import pytest

from src.models import Session, SessionStatus
from src.message_queue import MessageQueueManager


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager."""
    manager = MagicMock()
    manager.get_session = MagicMock(return_value=None)
    manager.tmux = MagicMock()
    manager.tmux.send_input_async = AsyncMock(return_value=True)
    manager._save_state = MagicMock()
    manager._deliver_direct = AsyncMock(return_value=True)
    return manager


@pytest.fixture
def temp_db(tmp_path):
    """Create a temporary database path."""
    return str(tmp_path / "test_queue.db")


@pytest.fixture
def message_queue(mock_session_manager, temp_db):
    """Create a MessageQueueManager instance for testing."""
    mq = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=temp_db,
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
                    "watch_poll_interval_seconds": 0.1,
                }
            },
        },
        notifier=None,
    )
    return mq


def noop_create_task(coro):
    """Silently close coroutine without running it."""
    coro.close()
    return MagicMock()


class TestUnpauseNoDeliveryState:
    """Verify unpause triggers delivery even when no delivery state exists."""

    def test_unpause_triggers_delivery_without_state(self, message_queue):
        """
        Reproduce #154: pause → queue urgent → unpause.
        No delivery state entry exists because _deliver_urgent returned early.
        unpause_session must still schedule _try_deliver_messages.
        """
        session_id = "target-154a"

        # 1. Pause session (simulates crash recovery)
        message_queue.pause_session(session_id)
        assert message_queue.is_session_paused(session_id)

        # 2. Queue urgent message while paused — suppress _deliver_urgent task
        with patch("asyncio.create_task", noop_create_task):
            message_queue.queue_message(
                target_session_id=session_id,
                text="urgent while paused",
                delivery_mode="urgent",
            )

        # Verify: no delivery state exists (the bug's precondition)
        assert session_id not in message_queue.delivery_states
        # But message is pending
        assert message_queue.get_queue_length(session_id) == 1

        # 3. Unpause — must schedule delivery
        created_tasks = []

        def track_create_task(coro):
            created_tasks.append(coro)
            coro.close()
            return MagicMock()

        with patch("asyncio.create_task", track_create_task):
            message_queue.unpause_session(session_id)

        assert not message_queue.is_session_paused(session_id)
        # _try_deliver_messages must have been scheduled
        assert len(created_tasks) == 1, (
            "unpause_session must schedule _try_deliver_messages when pending messages exist"
        )

    def test_unpause_no_delivery_when_queue_empty(self, message_queue):
        """
        Unpause with no pending messages should NOT schedule delivery.
        """
        session_id = "target-154b"

        message_queue.pause_session(session_id)

        created_tasks = []

        def track_create_task(coro):
            created_tasks.append(coro)
            coro.close()
            return MagicMock()

        with patch("asyncio.create_task", track_create_task):
            message_queue.unpause_session(session_id)

        assert len(created_tasks) == 0, (
            "unpause_session must NOT schedule delivery when no pending messages"
        )

    def test_unpause_triggers_delivery_with_existing_state(self, message_queue):
        """
        When delivery state exists and has pending messages, unpause
        must still trigger delivery (not just when state is missing).
        """
        session_id = "target-154c"

        # Create state by marking idle, then pause
        with patch("asyncio.create_task", noop_create_task):
            message_queue.mark_session_idle(session_id)
        message_queue.pause_session(session_id)

        # Queue message while paused
        with patch("asyncio.create_task", noop_create_task):
            message_queue.queue_message(
                target_session_id=session_id,
                text="message while paused",
                delivery_mode="sequential",
            )

        assert session_id in message_queue.delivery_states
        assert message_queue.get_queue_length(session_id) == 1

        # Unpause — must schedule delivery
        created_tasks = []

        def track_create_task(coro):
            created_tasks.append(coro)
            coro.close()
            return MagicMock()

        with patch("asyncio.create_task", track_create_task):
            message_queue.unpause_session(session_id)

        assert len(created_tasks) == 1
