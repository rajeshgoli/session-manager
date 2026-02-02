"""
Regression tests for issue #76: Pending messages not delivered, accumulating in database

Tests cover:
1. Messages for non-existent sessions are cleaned up during recovery
2. Messages for sessions with status='error' are delivered immediately
3. Recovery runs correctly on startup
"""

import pytest
import asyncio
import sqlite3
from pathlib import Path
from datetime import datetime
from unittest.mock import Mock, MagicMock, AsyncMock

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
    manager.tmux.send_input_async = AsyncMock(return_value=True)
    return manager


@pytest.fixture
async def queue_manager(mock_session_manager, temp_db):
    """Create a message queue manager for testing."""
    mgr = MessageQueueManager(mock_session_manager, db_path=temp_db)
    await mgr.start()
    yield mgr
    await mgr.stop()


@pytest.mark.asyncio
async def test_recovery_cleans_up_nonexistent_sessions(queue_manager, mock_session_manager, temp_db):
    """Test that recovery cleans up messages for sessions that no longer exist."""
    # Queue a message for a non-existent session
    session_id = "nonexistent-session-123"
    queue_manager.queue_message(
        target_session_id=session_id,
        text="Test message",
        delivery_mode="sequential"
    )

    # Verify message was queued
    pending = queue_manager.get_pending_messages(session_id)
    assert len(pending) == 1

    # Run recovery (simulating server restart)
    await queue_manager._recover_pending_messages()

    # Verify message was cleaned up
    pending_after = queue_manager.get_pending_messages(session_id)
    assert len(pending_after) == 0

    # Verify database cleanup
    conn = sqlite3.connect(temp_db)
    cursor = conn.cursor()
    cursor.execute("SELECT COUNT(*) FROM message_queue WHERE target_session_id = ? AND delivered_at IS NULL", (session_id,))
    count = cursor.fetchone()[0]
    conn.close()
    assert count == 0


@pytest.mark.asyncio
async def test_delivery_to_error_status_session(queue_manager, mock_session_manager):
    """Test that messages are delivered to sessions with status='error'."""
    # Create a session with ERROR status
    session_id = "test-session-error"
    session = Mock(spec=Session)
    session.id = session_id
    session.status = SessionStatus.ERROR
    session.tmux_session = "tmux-session-123"
    mock_session_manager.sessions[session_id] = session

    # Queue a message - this should trigger immediate delivery
    queue_manager.queue_message(
        target_session_id=session_id,
        text="Test message for error session",
        delivery_mode="sequential"
    )

    # Give async tasks time to execute
    await asyncio.sleep(0.2)

    # Verify session was marked idle at some point (last_idle_at should be set)
    state = queue_manager.delivery_states.get(session_id)
    assert state is not None
    assert state.last_idle_at is not None, "Session should have been marked idle"

    # Verify message was delivered (queue should be empty)
    pending = queue_manager.get_pending_messages(session_id)
    assert len(pending) == 0, "Message should have been delivered"

    # Verify send_input_async was called
    mock_session_manager.tmux.send_input_async.assert_called()


@pytest.mark.asyncio
async def test_delivery_to_idle_status_session(queue_manager, mock_session_manager):
    """Test that messages are delivered to sessions with status='idle'."""
    # Create a session with IDLE status
    session_id = "test-session-idle"
    session = Mock(spec=Session)
    session.id = session_id
    session.status = SessionStatus.IDLE
    session.tmux_session = "tmux-session-456"
    mock_session_manager.sessions[session_id] = session

    # Queue a message - this should trigger immediate delivery
    queue_manager.queue_message(
        target_session_id=session_id,
        text="Test message for idle session",
        delivery_mode="sequential"
    )

    # Give async tasks time to execute
    await asyncio.sleep(0.2)

    # Verify session was marked idle at some point (last_idle_at should be set)
    state = queue_manager.delivery_states.get(session_id)
    assert state is not None
    assert state.last_idle_at is not None, "Session should have been marked idle"

    # Verify message was delivered (queue should be empty)
    pending = queue_manager.get_pending_messages(session_id)
    assert len(pending) == 0, "Message should have been delivered"

    # Verify send_input_async was called
    mock_session_manager.tmux.send_input_async.assert_called()


@pytest.mark.asyncio
async def test_recovery_marks_existing_sessions_idle(queue_manager, mock_session_manager):
    """Test that recovery marks existing sessions with pending messages as idle."""
    # Create a session
    session_id = "test-session-recovery"
    session = Mock(spec=Session)
    session.id = session_id
    session.status = SessionStatus.RUNNING
    session.tmux_session = "tmux-session-789"
    mock_session_manager.sessions[session_id] = session

    # Queue a message
    queue_manager.queue_message(
        target_session_id=session_id,
        text="Test message",
        delivery_mode="sequential"
    )

    # Clear the in-memory state (simulating server restart)
    queue_manager.delivery_states.clear()

    # Run recovery
    await queue_manager._recover_pending_messages()

    # Verify session was marked idle during recovery
    state = queue_manager.delivery_states.get(session_id)
    assert state is not None
    assert state.is_idle is True


@pytest.mark.asyncio
async def test_mixed_recovery_scenario(queue_manager, mock_session_manager, temp_db):
    """Test recovery with a mix of existing and non-existent sessions."""
    # Create one existing session and queue messages for it and a non-existent session
    existing_id = "existing-session"
    nonexistent_id = "nonexistent-session"

    session = Mock(spec=Session)
    session.id = existing_id
    session.status = SessionStatus.IDLE
    session.tmux_session = "tmux-existing"
    mock_session_manager.sessions[existing_id] = session

    # Queue messages
    queue_manager.queue_message(existing_id, "Message for existing", "sequential")
    queue_manager.queue_message(nonexistent_id, "Message for nonexistent", "sequential")

    # Verify both messages were queued
    assert len(queue_manager.get_pending_messages(existing_id)) == 1
    assert len(queue_manager.get_pending_messages(nonexistent_id)) == 1

    # Clear in-memory state
    queue_manager.delivery_states.clear()

    # Run recovery
    await queue_manager._recover_pending_messages()

    # Verify existing session was marked idle
    assert queue_manager.delivery_states.get(existing_id) is not None

    # Verify nonexistent session's messages were cleaned up
    assert len(queue_manager.get_pending_messages(nonexistent_id)) == 0

    # Verify existing session's messages remain
    assert len(queue_manager.get_pending_messages(existing_id)) == 1
