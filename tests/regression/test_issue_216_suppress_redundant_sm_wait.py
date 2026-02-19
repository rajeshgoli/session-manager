"""
Regression tests for issue #216: Suppress redundant sm wait idle after stop notification.

Every agent completion sends two signals within 2s: stop notification + sm wait idle.
Both correct but redundant. Suppress sm wait idle if stop notification was sent to the
same watcher within 10s.
"""

import pytest
import asyncio
from datetime import datetime, timedelta
from unittest.mock import Mock, AsyncMock

from src.models import Session, SessionStatus
from src.message_queue import MessageQueueManager


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager."""
    manager = Mock()
    manager.get_session = Mock(return_value=None)
    manager.tmux = Mock()
    manager.tmux.send_input_async = AsyncMock(return_value=True)
    manager._save_state = Mock()
    manager._deliver_direct = AsyncMock(return_value=True)
    return manager


@pytest.fixture
def temp_db(tmp_path):
    """Create a temporary database path."""
    return str(tmp_path / "test_216.db")


@pytest.fixture
def message_queue(mock_session_manager, temp_db):
    """Create a MessageQueueManager instance for testing."""
    return MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=temp_db,
        config={
            "timeouts": {
                "message_queue": {
                    "watch_poll_interval_seconds": 0.1,
                    "subprocess_timeout_seconds": 1,
                }
            }
        },
    )


def make_session(session_id: str, name: str) -> Session:
    """Helper to create a minimal Session."""
    return Session(
        id=session_id,
        name=name,
        working_dir="/tmp/test",
        tmux_session=f"tmux-{session_id}",
        friendly_name=name,
    )


@pytest.mark.asyncio
async def test_sm_wait_suppressed_after_stop_notification(
    message_queue, mock_session_manager
):
    """
    Stop notification fires to watcher A → sm wait from watcher A fires within 5s
    → verify watcher A gets only 1 signal (no redundant idle notification).
    """
    target = make_session("target-001", "scout")
    watcher = make_session("watcher-001", "em")

    mock_session_manager.get_session.side_effect = lambda sid: {
        "target-001": target,
        "watcher-001": watcher,
    }.get(sid)

    # Track messages queued for watcher
    queued_for_watcher = []
    original_queue = message_queue.queue_message

    def track_queue(*args, **kwargs):
        msg = original_queue(*args, **kwargs)
        if kwargs.get("target_session_id") == "watcher-001":
            queued_for_watcher.append(kwargs.get("text", ""))
        return msg

    message_queue.queue_message = track_queue

    # Simulate: stop notification was sent to watcher 2s ago (within 10s window)
    key = ("target-001", "watcher-001")
    message_queue._recent_stop_notifications[key] = datetime.now() - timedelta(seconds=2)

    # Mark target idle so _watch_for_idle will detect it
    message_queue.mark_session_idle("target-001")

    # Start watch — should suppress idle notification because stop was recent
    watch_id = await message_queue.watch_session("target-001", "watcher-001", 10)

    # Wait long enough for the watch loop to poll (poll interval = 0.1s)
    await asyncio.sleep(0.5)

    # Watcher should NOT have received any idle notification (it was suppressed)
    idle_notifications = [m for m in queued_for_watcher if "[sm wait]" in m]
    assert len(idle_notifications) == 0, (
        f"Expected 0 sm wait idle notifications (suppressed), got: {idle_notifications}"
    )

    # Suppression key should have been cleaned up
    assert key not in message_queue._recent_stop_notifications


@pytest.mark.asyncio
async def test_sm_wait_not_suppressed_after_window_expires(
    message_queue, mock_session_manager
):
    """
    Stop notification fires → sm wait fires after 15s (window expired)
    → verify watcher gets the idle notification (window has passed).
    """
    target = make_session("target-002", "scout")
    watcher = make_session("watcher-002", "em")

    mock_session_manager.get_session.side_effect = lambda sid: {
        "target-002": target,
        "watcher-002": watcher,
    }.get(sid)

    queued_for_watcher = []
    original_queue = message_queue.queue_message

    def track_queue(*args, **kwargs):
        msg = original_queue(*args, **kwargs)
        if kwargs.get("target_session_id") == "watcher-002":
            queued_for_watcher.append(kwargs.get("text", ""))
        return msg

    message_queue.queue_message = track_queue

    # Simulate: stop notification was sent 15s ago (outside 10s window)
    key = ("target-002", "watcher-002")
    message_queue._recent_stop_notifications[key] = datetime.now() - timedelta(seconds=15)

    # Mark target idle
    message_queue.mark_session_idle("target-002")

    # Start watch — should NOT suppress (window expired)
    watch_id = await message_queue.watch_session("target-002", "watcher-002", 10)

    # Wait for poll
    await asyncio.sleep(0.5)

    # Watcher SHOULD receive idle notification (window expired, no suppression)
    idle_notifications = [m for m in queued_for_watcher if "[sm wait]" in m and "idle" in m]
    assert len(idle_notifications) >= 1, (
        f"Expected sm wait idle notification (window expired), got: {queued_for_watcher}"
    )


@pytest.mark.asyncio
async def test_sm_wait_multi_watcher_scope(
    message_queue, mock_session_manager
):
    """
    Two watchers (A and B) watching same target; stop notification directed at A only.
    Verify A's sm wait is suppressed, B's sm wait is NOT suppressed.
    """
    target = make_session("target-003", "scout")
    watcher_a = make_session("watcher-a", "em")
    watcher_b = make_session("watcher-b", "other-em")

    mock_session_manager.get_session.side_effect = lambda sid: {
        "target-003": target,
        "watcher-a": watcher_a,
        "watcher-b": watcher_b,
    }.get(sid)

    queued_for_a = []
    queued_for_b = []
    original_queue = message_queue.queue_message

    def track_queue(*args, **kwargs):
        msg = original_queue(*args, **kwargs)
        tid = kwargs.get("target_session_id", "")
        text = kwargs.get("text", "")
        if tid == "watcher-a":
            queued_for_a.append(text)
        elif tid == "watcher-b":
            queued_for_b.append(text)
        return msg

    message_queue.queue_message = track_queue

    # Stop notification was sent to watcher-a only (within 10s window)
    key_a = ("target-003", "watcher-a")
    message_queue._recent_stop_notifications[key_a] = datetime.now() - timedelta(seconds=2)
    # No stop notification for watcher-b

    # Mark target idle
    message_queue.mark_session_idle("target-003")

    # Both watchers start watching
    watch_id_a = await message_queue.watch_session("target-003", "watcher-a", 10)
    watch_id_b = await message_queue.watch_session("target-003", "watcher-b", 10)

    # Wait for poll
    await asyncio.sleep(0.5)

    # Watcher A: should be suppressed (stop notification was recent)
    idle_a = [m for m in queued_for_a if "[sm wait]" in m and "idle" in m]
    assert len(idle_a) == 0, (
        f"Watcher A should have suppressed idle notification, got: {idle_a}"
    )

    # Watcher B: should NOT be suppressed (no stop notification was sent to B)
    idle_b = [m for m in queued_for_b if "[sm wait]" in m and "idle" in m]
    assert len(idle_b) >= 1, (
        f"Watcher B should have received idle notification (not suppressed), got: {queued_for_b}"
    )
