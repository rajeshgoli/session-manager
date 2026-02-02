"""
Regression tests for issue #34: Race condition allows message double-delivery

Tests verify that concurrent Stop hooks don't cause messages to be delivered twice.
"""

import pytest
import asyncio
import sqlite3
from pathlib import Path
from unittest.mock import Mock, AsyncMock
from datetime import datetime

from src.message_queue import MessageQueueManager
from src.models import Session, SessionStatus


@pytest.fixture
def temp_db(tmp_path):
    """Create a temporary database for testing."""
    db_path = tmp_path / "test_message_queue.db"
    return str(db_path)


@pytest.fixture
def mock_session_manager():
    """Create a mock session manager."""
    manager = Mock()
    manager.sessions = {}
    manager.get_session = lambda sid: manager.sessions.get(sid)
    manager.tmux = Mock()
    # Track how many times send_input_async is called
    manager.tmux.send_input_async = AsyncMock(return_value=True)
    manager._save_state = Mock()
    return manager


@pytest.fixture
async def queue_manager(mock_session_manager, temp_db):
    """Create a message queue manager for testing."""
    mgr = MessageQueueManager(mock_session_manager, db_path=temp_db)
    await mgr.start()
    yield mgr
    await mgr.stop()


@pytest.mark.asyncio
async def test_rapid_mark_idle_no_double_delivery(queue_manager, mock_session_manager):
    """Test that rapid mark_session_idle calls don't cause double-delivery."""
    # Create a session
    session_id = "test-session-123"
    session = Mock(spec=Session)
    session.id = session_id
    session.status = SessionStatus.IDLE
    session.tmux_session = "tmux-session-123"
    session.last_activity = datetime.now()
    mock_session_manager.sessions[session_id] = session

    # Queue a message
    queue_manager.queue_message(
        target_session_id=session_id,
        text="Test message",
        delivery_mode="sequential"
    )

    # Mark session as idle (simulates first Stop hook)
    queue_manager.mark_session_idle(session_id)

    # Immediately mark idle again (simulates rapid second Stop hook)
    queue_manager.mark_session_idle(session_id)

    # And again (simulates third Stop hook)
    queue_manager.mark_session_idle(session_id)

    # Give all tasks time to execute
    await asyncio.sleep(0.5)

    # Verify send_input_async was called exactly once (not 3 times)
    assert mock_session_manager.tmux.send_input_async.call_count == 1

    # Verify message was marked as delivered
    pending = queue_manager.get_pending_messages(session_id)
    assert len(pending) == 0


@pytest.mark.asyncio
async def test_concurrent_delivery_tasks_serialized(queue_manager, mock_session_manager):
    """Test that concurrent delivery tasks are serialized by the lock."""
    session_id = "test-session-456"
    session = Mock(spec=Session)
    session.id = session_id
    session.status = SessionStatus.IDLE
    session.tmux_session = "tmux-session-456"
    session.last_activity = datetime.now()
    mock_session_manager.sessions[session_id] = session

    # Queue multiple messages
    for i in range(3):
        queue_manager.queue_message(
            target_session_id=session_id,
            text=f"Test message {i}",
            delivery_mode="sequential"
        )

    # Create multiple concurrent delivery tasks (simulating rapid Stop hooks)
    tasks = []
    for _ in range(5):
        task = asyncio.create_task(queue_manager._try_deliver_messages(session_id))
        tasks.append(task)

    # Wait for all tasks to complete
    await asyncio.gather(*tasks)

    # The lock should ensure only one delivery happens
    # (up to max_batch_size messages in that delivery)
    assert mock_session_manager.tmux.send_input_async.call_count == 1

    # All messages should be delivered
    pending = queue_manager.get_pending_messages(session_id)
    assert len(pending) == 0


@pytest.mark.asyncio
async def test_different_sessions_not_blocked(queue_manager, mock_session_manager):
    """Test that locks for different sessions don't block each other."""
    # Create two sessions
    session1 = Mock(spec=Session)
    session1.id = "session-1"
    session1.status = SessionStatus.IDLE
    session1.tmux_session = "tmux-1"
    session1.last_activity = datetime.now()
    mock_session_manager.sessions["session-1"] = session1

    session2 = Mock(spec=Session)
    session2.id = "session-2"
    session2.status = SessionStatus.IDLE
    session2.tmux_session = "tmux-2"
    session2.last_activity = datetime.now()
    mock_session_manager.sessions["session-2"] = session2

    # Queue messages for both sessions
    queue_manager.queue_message("session-1", "Message for session 1", "sequential")
    queue_manager.queue_message("session-2", "Message for session 2", "sequential")

    # Trigger delivery for both concurrently
    task1 = asyncio.create_task(queue_manager._try_deliver_messages("session-1"))
    task2 = asyncio.create_task(queue_manager._try_deliver_messages("session-2"))

    await asyncio.gather(task1, task2)

    # Both should have been delivered (locks are per-session)
    assert mock_session_manager.tmux.send_input_async.call_count == 2

    # No pending messages for either session
    assert len(queue_manager.get_pending_messages("session-1")) == 0
    assert len(queue_manager.get_pending_messages("session-2")) == 0


@pytest.mark.asyncio
async def test_lock_released_on_error(queue_manager, mock_session_manager):
    """Test that the lock is released even if delivery fails."""
    session_id = "test-session-error"
    session = Mock(spec=Session)
    session.id = session_id
    session.status = SessionStatus.IDLE
    session.tmux_session = "tmux-error"
    session.last_activity = datetime.now()
    mock_session_manager.sessions[session_id] = session

    # Make send_input_async fail
    mock_session_manager.tmux.send_input_async = AsyncMock(return_value=False)

    # Queue a message
    queue_manager.queue_message(session_id, "Test message", "sequential")

    # First delivery attempt (will fail)
    await queue_manager._try_deliver_messages(session_id)

    # Make send_input_async succeed now
    mock_session_manager.tmux.send_input_async = AsyncMock(return_value=True)

    # Second delivery attempt (should succeed - lock should not be stuck)
    await queue_manager._try_deliver_messages(session_id)

    # Verify the second attempt succeeded
    assert len(queue_manager.get_pending_messages(session_id)) == 0


@pytest.mark.asyncio
async def test_rapid_fire_stress_test(queue_manager, mock_session_manager):
    """Stress test with many rapid mark_idle calls."""
    session_id = "stress-test-session"
    session = Mock(spec=Session)
    session.id = session_id
    session.status = SessionStatus.IDLE
    session.tmux_session = "tmux-stress"
    session.last_activity = datetime.now()
    mock_session_manager.sessions[session_id] = session

    # Queue a single message
    queue_manager.queue_message(session_id, "Stress test message", "sequential")

    # Fire off 20 rapid mark_idle calls (simulating many rapid Stop hooks)
    for _ in range(20):
        queue_manager.mark_session_idle(session_id)

    # Give all tasks time to execute
    await asyncio.sleep(1.0)

    # Should only deliver once despite 20 triggers
    assert mock_session_manager.tmux.send_input_async.call_count == 1

    # Message should be delivered
    assert len(queue_manager.get_pending_messages(session_id)) == 0
