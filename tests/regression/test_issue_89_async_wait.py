"""
Regression tests for issue #89: sm wait blocks synchronously instead of notifying asynchronously

Tests verify that sm wait returns immediately and notifies asynchronously when
the target session goes idle or reaches timeout.
"""

import pytest
import asyncio
from unittest.mock import Mock, AsyncMock, MagicMock, patch
from datetime import datetime

from src.models import Session, SessionStatus
from src.message_queue import MessageQueueManager
from src.cli.commands import cmd_wait
from src.cli.client import SessionManagerClient


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager."""
    manager = Mock()
    manager.get_session = Mock()
    manager.tmux = Mock()
    manager.tmux.send_input_async = AsyncMock(return_value=True)
    manager._save_state = Mock()
    manager._deliver_direct = AsyncMock(return_value=True)
    return manager


@pytest.fixture
def temp_db(tmp_path):
    """Create a temporary database path."""
    return str(tmp_path / "test_queue.db")


@pytest.fixture
def message_queue(mock_session_manager, temp_db):
    """Create a MessageQueueManager instance for testing."""
    config = {
        "urgent_delay_ms": 100,
    }
    queue_mgr = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=temp_db,
        config=config,
    )
    return queue_mgr


@pytest.mark.asyncio
async def test_watch_session_notifies_when_target_goes_idle(
    message_queue, mock_session_manager
):
    """Test that watch_session notifies when target goes idle."""
    # Setup sessions
    target_session = Session(
        id="target-123",
        name="target-session",
        working_dir="/tmp/target",
        tmux_session="claude-target-123",
        friendly_name="target-agent",
    )
    watcher_session = Session(
        id="watcher-456",
        name="watcher-session",
        working_dir="/tmp/watcher",
        tmux_session="claude-watcher-456",
        friendly_name="watcher-agent",
    )

    mock_session_manager.get_session.side_effect = lambda sid: {
        "target-123": target_session,
        "watcher-456": watcher_session,
    }.get(sid)

    # Track message queuing
    queued_messages = []
    original_queue = message_queue.queue_message

    def track_queue(*args, **kwargs):
        msg = original_queue(*args, **kwargs)
        queued_messages.append((args, kwargs))
        return msg

    message_queue.queue_message = track_queue

    # Start watch
    watch_id = await message_queue.watch_session("target-123", "watcher-456", 30)
    assert watch_id is not None

    # Mark target as idle immediately
    message_queue.mark_session_idle("target-123")

    # Wait for watch task to check and queue notification (poll interval is 2s)
    await asyncio.sleep(2.5)

    # Verify notification was queued for watcher
    assert len(queued_messages) > 0

    # Check that watcher-456 received a notification
    watcher_messages = [m for m in queued_messages if m[1].get("target_session_id") == "watcher-456"]
    assert len(watcher_messages) > 0

    # Check notification content
    text = watcher_messages[0][1]["text"]
    assert "target-agent" in text or "target-123" in text
    assert "idle" in text.lower()
    assert watcher_messages[0][1]["delivery_mode"] == "important"


@pytest.mark.asyncio
async def test_watch_session_notifies_on_timeout(
    message_queue, mock_session_manager
):
    """Test that watch_session notifies when timeout is reached."""
    # Setup sessions
    target_session = Session(
        id="target-789",
        name="target-session",
        working_dir="/tmp/target",
        tmux_session="claude-target-789",
        friendly_name="busy-agent",
    )
    watcher_session = Session(
        id="watcher-abc",
        name="watcher-session",
        working_dir="/tmp/watcher",
        tmux_session="claude-watcher-abc",
        friendly_name="watcher-agent",
    )

    mock_session_manager.get_session.side_effect = lambda sid: {
        "target-789": target_session,
        "watcher-abc": watcher_session,
    }.get(sid)

    # Track message queuing
    queued_messages = []
    original_queue = message_queue.queue_message

    def track_queue(*args, **kwargs):
        msg = original_queue(*args, **kwargs)
        queued_messages.append((args, kwargs))
        return msg

    message_queue.queue_message = track_queue

    # Start watch with short timeout
    watch_id = await message_queue.watch_session("target-789", "watcher-abc", 2)
    assert watch_id is not None

    # Target never goes idle - wait for timeout
    await asyncio.sleep(2.5)

    # Verify timeout notification was queued for watcher
    assert len(queued_messages) > 0

    # Check that watcher-abc received a notification
    watcher_messages = [m for m in queued_messages if m[1].get("target_session_id") == "watcher-abc"]
    assert len(watcher_messages) > 0

    # Check notification content
    text = watcher_messages[0][1]["text"]
    assert "busy-agent" in text or "target-789" in text
    assert "timeout" in text.lower() or "still active" in text.lower()
    assert watcher_messages[0][1]["delivery_mode"] == "important"


@pytest.mark.asyncio
async def test_watch_creates_scheduled_task(message_queue, mock_session_manager):
    """Test that watch creates a scheduled task that can be tracked."""
    # Setup sessions
    target_session = Session(
        id="target-999",
        name="target-session",
        working_dir="/tmp/target",
        tmux_session="claude-target-999",
    )
    watcher_session = Session(
        id="watcher-111",
        name="watcher-session",
        working_dir="/tmp/watcher",
        tmux_session="claude-watcher-111",
    )

    mock_session_manager.get_session.side_effect = lambda sid: {
        "target-999": target_session,
        "watcher-111": watcher_session,
    }.get(sid)

    # Start watch
    watch_id = await message_queue.watch_session("target-999", "watcher-111", 30)

    # Verify task was created and added to scheduled tasks
    task = message_queue._scheduled_tasks.get(watch_id)
    assert task is not None
    assert not task.done()

    # Cancel the task to clean up
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass


def test_cmd_wait_returns_immediately():
    """Test that cmd_wait returns immediately instead of blocking."""
    import time

    # Mock client
    mock_client = MagicMock(spec=SessionManagerClient)
    mock_client.session_id = "watcher-123"

    # Mock resolve_session_id
    with patch('src.cli.commands.resolve_session_id') as mock_resolve:
        mock_resolve.return_value = ("target-456", {
            "id": "target-456",
            "friendly_name": "target-session",
            "name": "target-session"
        })

        # Mock watch_session to return success
        mock_client.watch_session.return_value = {
            "status": "watching",
            "watch_id": "watch-789",
            "target_name": "target-session",
            "timeout_seconds": 300,
        }

        # Time the execution
        start = time.time()
        exit_code = cmd_wait(mock_client, "target-456", 300)
        elapsed = time.time() - start

        # Should return immediately (< 1 second, not 300 seconds)
        assert elapsed < 1.0
        assert exit_code == 0

        # Verify watch_session was called
        mock_client.watch_session.assert_called_once_with(
            "target-456",
            "watcher-123",
            300
        )


def test_cmd_wait_without_session_context():
    """Test that cmd_wait fails gracefully without session context."""
    # Mock client without session_id
    mock_client = MagicMock(spec=SessionManagerClient)
    mock_client.session_id = None  # No session context

    with patch('src.cli.commands.resolve_session_id') as mock_resolve:
        mock_resolve.return_value = ("target-456", {
            "id": "target-456",
            "friendly_name": "target-session"
        })

        exit_code = cmd_wait(mock_client, "target-456", 300)

        # Should fail with exit code 1
        assert exit_code == 1


def test_cmd_wait_target_not_found():
    """Test that cmd_wait handles target not found."""
    mock_client = MagicMock(spec=SessionManagerClient)
    mock_client.session_id = "watcher-123"
    mock_client.list_sessions.return_value = []  # No sessions

    with patch('src.cli.commands.resolve_session_id') as mock_resolve:
        mock_resolve.return_value = (None, None)  # Session not found

        exit_code = cmd_wait(mock_client, "nonexistent", 300)

        # Should fail with exit code 2
        assert exit_code == 2


def test_cmd_wait_session_manager_unavailable():
    """Test that cmd_wait handles session manager unavailable."""
    mock_client = MagicMock(spec=SessionManagerClient)
    mock_client.session_id = "watcher-123"

    with patch('src.cli.commands.resolve_session_id') as mock_resolve:
        mock_resolve.return_value = ("target-456", {
            "id": "target-456",
            "friendly_name": "target-session"
        })

        # watch_session returns None (unavailable)
        mock_client.watch_session.return_value = None

        exit_code = cmd_wait(mock_client, "target-456", 300)

        # Should fail with exit code 2
        assert exit_code == 2
