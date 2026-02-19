"""
Regression tests for issue #153: sm wait race condition — queue_message()
doesn't clear is_idle before async delivery, so sm wait sees stale
is_idle=True and returns immediately with "idle (waited 0s)".
"""

import pytest
import asyncio
from unittest.mock import MagicMock, AsyncMock, patch
from datetime import datetime

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


class TestUrgentSendClearsIdle:
    """Verify that queue_message(urgent) eagerly clears is_idle."""

    def test_urgent_send_clears_idle_before_task(self, message_queue):
        """Mark session idle, queue urgent message — is_idle must be False."""
        session_id = "target-aaa"

        # Simulate Stop hook marking session idle
        with patch("asyncio.create_task", noop_create_task):
            message_queue.mark_session_idle(session_id)
        assert message_queue.is_session_idle(session_id) is True

        # Queue an urgent message (suppress the _deliver_urgent task)
        with patch("asyncio.create_task", noop_create_task):
            message_queue.queue_message(
                target_session_id=session_id,
                text="urgent work",
                delivery_mode="urgent",
            )

        # is_idle must have been cleared eagerly by queue_message
        assert message_queue.is_session_idle(session_id) is False

    def test_urgent_paused_preserves_idle(self, message_queue):
        """Paused session: urgent queue must NOT clear is_idle."""
        session_id = "target-bbb"

        # Session is idle and paused for crash recovery
        with patch("asyncio.create_task", noop_create_task):
            message_queue.mark_session_idle(session_id)
        message_queue.pause_session(session_id)
        assert message_queue.is_session_idle(session_id) is True

        # Queue urgent message
        with patch("asyncio.create_task", noop_create_task):
            message_queue.queue_message(
                target_session_id=session_id,
                text="urgent while paused",
                delivery_mode="urgent",
            )

        # is_idle must still be True — unpause_session needs it to retry
        assert message_queue.is_session_idle(session_id) is True


class TestWatchForIdleRace:
    """Verify _watch_for_idle validates idle state with pending-message check."""

    @pytest.mark.asyncio
    async def test_urgent_send_clears_idle_before_watch(
        self, message_queue, mock_session_manager
    ):
        """
        Mark session idle, queue urgent message, immediately start
        _watch_for_idle — watch must NOT fire "idle (waited 0s)".
        """
        target_id = "target-ccc"
        watcher_id = "watcher-ccc"

        # Setup sessions
        target_session = Session(
            id=target_id,
            name="target",
            working_dir="/tmp",
            tmux_session="claude-target-ccc",
            friendly_name="target-agent",
        )
        watcher_session = Session(
            id=watcher_id,
            name="watcher",
            working_dir="/tmp",
            tmux_session="claude-watcher-ccc",
            friendly_name="watcher-agent",
        )
        mock_session_manager.get_session.side_effect = lambda sid: {
            target_id: target_session,
            watcher_id: watcher_session,
        }.get(sid)

        # Simulate idle → urgent send (delivery suppressed so message stays pending)
        with patch("asyncio.create_task", noop_create_task):
            message_queue.mark_session_idle(target_id)
            message_queue.queue_message(
                target_session_id=target_id,
                text="urgent task",
                delivery_mode="urgent",
            )

        # Even though queue_message cleared is_idle, force it back to True
        # to test the secondary defense in _watch_for_idle
        message_queue._get_or_create_state(target_id).is_idle = True

        # Track notifications
        queued_notifications = []
        original_queue = message_queue.queue_message

        def track_queue(*args, **kwargs):
            msg = original_queue(*args, **kwargs)
            queued_notifications.append(kwargs)
            return msg

        message_queue.queue_message = track_queue

        # Run _watch_for_idle with a short timeout
        await message_queue._watch_for_idle(
            "watch-1", target_id, watcher_id, timeout_seconds=0.5
        )

        # The watch should have timed out, NOT fired an immediate idle notification
        watcher_msgs = [
            n for n in queued_notifications
            if n.get("target_session_id") == watcher_id
        ]
        assert len(watcher_msgs) == 1
        assert "timeout" in watcher_msgs[0]["text"].lower() or "still active" in watcher_msgs[0]["text"].lower()

    @pytest.mark.asyncio
    async def test_sequential_mark_idle_race(
        self, message_queue, mock_session_manager
    ):
        """
        _watch_for_idle Phase 4: even if is_idle=True, pending messages prevent false-idle.

        With sm#244, sequential delivery no longer calls mark_session_idle from queue_message.
        But Phase 4 still guards against any future scenario where is_idle=True coexists
        with stuck pending messages (e.g., delivery lock held, tty buffer lag).
        """
        target_id = "target-ddd"
        watcher_id = "watcher-ddd"

        target_session = Session(
            id=target_id,
            name="target",
            working_dir="/tmp",
            tmux_session="claude-target-ddd",
            friendly_name="target-agent",
            status=SessionStatus.IDLE,
        )
        watcher_session = Session(
            id=watcher_id,
            name="watcher",
            working_dir="/tmp",
            tmux_session="claude-watcher-ddd",
            friendly_name="watcher-agent",
        )
        mock_session_manager.get_session.side_effect = lambda sid: {
            target_id: target_session,
            watcher_id: watcher_session,
        }.get(sid)

        # Queue sequential message (delivery task discarded by noop)
        with patch("asyncio.create_task", noop_create_task):
            message_queue.queue_message(
                target_session_id=target_id,
                text="sequential task",
                delivery_mode="sequential",
            )

        # Manually set is_idle=True to simulate the Phase 4 scenario:
        # is_idle is stale-True while a message is still pending in the queue.
        message_queue._get_or_create_state(target_id).is_idle = True
        assert message_queue.is_session_idle(target_id) is True
        assert message_queue.get_queue_length(target_id) == 1

        # Track notifications
        queued_notifications = []
        original_queue = message_queue.queue_message

        def track_queue(*args, **kwargs):
            msg = original_queue(*args, **kwargs)
            queued_notifications.append(kwargs)
            return msg

        message_queue.queue_message = track_queue

        # Run _watch_for_idle — should NOT immediately fire idle
        await message_queue._watch_for_idle(
            "watch-2", target_id, watcher_id, timeout_seconds=0.5
        )

        watcher_msgs = [
            n for n in queued_notifications
            if n.get("target_session_id") == watcher_id
        ]
        assert len(watcher_msgs) == 1
        assert "timeout" in watcher_msgs[0]["text"].lower() or "still active" in watcher_msgs[0]["text"].lower()

    @pytest.mark.asyncio
    async def test_watch_short_timeout(self, message_queue, mock_session_manager):
        """
        Start _watch_for_idle with timeout < poll_interval.
        Must still fire timeout correctly, not miss the idle check.
        """
        target_id = "target-eee"
        watcher_id = "watcher-eee"

        target_session = Session(
            id=target_id,
            name="target",
            working_dir="/tmp",
            tmux_session="claude-target-eee",
            friendly_name="target-agent",
        )
        watcher_session = Session(
            id=watcher_id,
            name="watcher",
            working_dir="/tmp",
            tmux_session="claude-watcher-eee",
            friendly_name="watcher-agent",
        )
        mock_session_manager.get_session.side_effect = lambda sid: {
            target_id: target_session,
            watcher_id: watcher_session,
        }.get(sid)

        # Target is NOT idle, no pending messages
        queued_notifications = []
        original_queue = message_queue.queue_message

        def track_queue(*args, **kwargs):
            msg = original_queue(*args, **kwargs)
            queued_notifications.append(kwargs)
            return msg

        message_queue.queue_message = track_queue

        # poll_interval is 0.1s, timeout is 0.05s — timeout < poll_interval
        # Override poll interval to make this scenario clear
        message_queue.watch_poll_interval = 2
        await message_queue._watch_for_idle(
            "watch-3", target_id, watcher_id, timeout_seconds=1
        )

        watcher_msgs = [
            n for n in queued_notifications
            if n.get("target_session_id") == watcher_id
        ]
        assert len(watcher_msgs) == 1
        assert "timeout" in watcher_msgs[0]["text"].lower() or "still active" in watcher_msgs[0]["text"].lower()

    @pytest.mark.asyncio
    async def test_watch_fires_idle_when_no_pending_messages(
        self, message_queue, mock_session_manager
    ):
        """
        Genuine idle: is_idle=True and no pending messages.
        _watch_for_idle should fire the idle notification normally.
        """
        target_id = "target-fff"
        watcher_id = "watcher-fff"

        target_session = Session(
            id=target_id,
            name="target",
            working_dir="/tmp",
            tmux_session="claude-target-fff",
            friendly_name="target-agent",
        )
        watcher_session = Session(
            id=watcher_id,
            name="watcher",
            working_dir="/tmp",
            tmux_session="claude-watcher-fff",
            friendly_name="watcher-agent",
        )
        mock_session_manager.get_session.side_effect = lambda sid: {
            target_id: target_session,
            watcher_id: watcher_session,
        }.get(sid)

        # Mark idle with NO pending messages — genuine idle
        with patch("asyncio.create_task", noop_create_task):
            message_queue.mark_session_idle(target_id)
        assert message_queue.get_queue_length(target_id) == 0

        queued_notifications = []
        original_queue = message_queue.queue_message

        def track_queue(*args, **kwargs):
            msg = original_queue(*args, **kwargs)
            queued_notifications.append(kwargs)
            return msg

        message_queue.queue_message = track_queue

        await message_queue._watch_for_idle(
            "watch-4", target_id, watcher_id, timeout_seconds=5
        )

        watcher_msgs = [
            n for n in queued_notifications
            if n.get("target_session_id") == watcher_id
        ]
        assert len(watcher_msgs) == 1
        assert "idle" in watcher_msgs[0]["text"].lower()
        # Should NOT be a timeout
        assert "timeout" not in watcher_msgs[0]["text"].lower()
        assert "still active" not in watcher_msgs[0]["text"].lower()
