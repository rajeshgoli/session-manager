"""
Regression tests for issue #88: sm send --urgent fails to deliver to completed sessions

Tests verify that urgent message delivery wakes up completed sessions before
sending the message, following the same pattern as cmd_clear (issue #78).
"""

import pytest
import asyncio
from unittest.mock import Mock, AsyncMock, patch, MagicMock
from datetime import datetime
from pathlib import Path

from src.models import Session, SessionStatus, QueuedMessage
from src.message_queue import MessageQueueManager


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
        "urgent_delay_ms": 100,  # Shorter for tests
    }
    queue_mgr = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=temp_db,
        config=config,
    )
    return queue_mgr


@pytest.mark.asyncio
async def test_urgent_delivery_to_completed_session_wakes_up_first(
    message_queue, mock_session_manager
):
    """Test that urgent delivery to a completed session sends Enter first to wake it up."""
    from src.models import CompletionStatus

    # Mock session with completion_status=COMPLETED
    session = Session(
        id="test-123",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="claude-test-123",
        completion_status=CompletionStatus.COMPLETED,
        friendly_name="completed-agent",
    )

    mock_session_manager.get_session.return_value = session

    # Create a test message
    msg = QueuedMessage(
        id="msg-001",
        target_session_id="test-123",
        text="urgent task",
        delivery_mode="urgent",
    )

    # Mock asyncio.create_subprocess_exec to track calls
    subprocess_calls = []

    async def mock_subprocess(*args, **kwargs):
        """Track subprocess calls."""
        subprocess_calls.append(args)
        proc = AsyncMock()
        proc.communicate = AsyncMock(return_value=(b"", b""))
        proc.returncode = 0
        return proc

    with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
        # Mock prompt polling so capture-pane calls don't appear in subprocess_calls (#175)
        message_queue._wait_for_claude_prompt_async = AsyncMock(return_value=True)
        # Deliver urgent message
        await message_queue._deliver_urgent("test-123", msg)

    # Verify subprocess calls were made in correct order
    assert len(subprocess_calls) >= 2

    # First call should be Enter to wake up the completed session
    first_call = subprocess_calls[0]
    assert first_call[0] == "tmux"
    assert first_call[1] == "send-keys"
    assert first_call[2] == "-t"
    assert first_call[3] == "claude-test-123"
    assert first_call[4] == "Enter"

    # Second call should be Escape (to interrupt)
    second_call = subprocess_calls[1]
    assert second_call[0] == "tmux"
    assert second_call[1] == "send-keys"
    assert second_call[2] == "-t"
    assert second_call[3] == "claude-test-123"
    assert second_call[4] == "Escape"


@pytest.mark.asyncio
async def test_urgent_delivery_to_running_session_no_wake_up(
    message_queue, mock_session_manager
):
    """Test that urgent delivery to a running session doesn't send wake-up Enter."""
    # Mock session without completion_status (or with None)
    session = Session(
        id="test-456",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="claude-test-456",
        completion_status=None,  # Not completed
        friendly_name="running-agent",
    )

    mock_session_manager.get_session.return_value = session

    # Create a test message
    msg = QueuedMessage(
        id="msg-002",
        target_session_id="test-456",
        text="urgent task",
        delivery_mode="urgent",
    )

    # Mock asyncio.create_subprocess_exec to track calls
    subprocess_calls = []

    async def mock_subprocess(*args, **kwargs):
        """Track subprocess calls."""
        subprocess_calls.append(args)
        proc = AsyncMock()
        proc.communicate = AsyncMock(return_value=(b"", b""))
        proc.returncode = 0
        return proc

    with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
        message_queue._wait_for_claude_prompt_async = AsyncMock(return_value=True)
        # Deliver urgent message
        await message_queue._deliver_urgent("test-456", msg)

    # Verify subprocess calls - should NOT start with wake-up Enter
    assert len(subprocess_calls) >= 1

    # First call should be Escape (NOT Enter)
    first_call = subprocess_calls[0]
    assert first_call[0] == "tmux"
    assert first_call[1] == "send-keys"
    assert first_call[2] == "-t"
    assert first_call[3] == "claude-test-456"
    assert first_call[4] == "Escape"


@pytest.mark.asyncio
async def test_urgent_delivery_to_error_session_no_wake_up(
    message_queue, mock_session_manager
):
    """Test that urgent delivery to an error session doesn't send wake-up Enter."""
    # Mock session with completion_status="error"
    session = Session(
        id="test-error",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="claude-test-error",
        completion_status="error",  # Error, not completed
        friendly_name="error-agent",
    )

    mock_session_manager.get_session.return_value = session

    # Create a test message
    msg = QueuedMessage(
        id="msg-003",
        target_session_id="test-error",
        text="urgent task",
        delivery_mode="urgent",
    )

    # Mock asyncio.create_subprocess_exec to track calls
    subprocess_calls = []

    async def mock_subprocess(*args, **kwargs):
        """Track subprocess calls."""
        subprocess_calls.append(args)
        proc = AsyncMock()
        proc.communicate = AsyncMock(return_value=(b"", b""))
        proc.returncode = 0
        return proc

    with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
        message_queue._wait_for_claude_prompt_async = AsyncMock(return_value=True)
        # Deliver urgent message
        await message_queue._deliver_urgent("test-error", msg)

    # Verify subprocess calls - should NOT start with wake-up Enter
    assert len(subprocess_calls) >= 1

    # First call should be Escape (NOT Enter)
    first_call = subprocess_calls[0]
    assert first_call[4] == "Escape"


@pytest.mark.asyncio
async def test_urgent_delivery_to_abandoned_session_no_wake_up(
    message_queue, mock_session_manager
):
    """Test that urgent delivery to an abandoned session doesn't send wake-up Enter."""
    # Mock session with completion_status="abandoned"
    session = Session(
        id="test-abandoned",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="claude-test-abandoned",
        completion_status="abandoned",  # Abandoned, not completed
        friendly_name="abandoned-agent",
    )

    mock_session_manager.get_session.return_value = session

    # Create a test message
    msg = QueuedMessage(
        id="msg-004",
        target_session_id="test-abandoned",
        text="urgent task",
        delivery_mode="urgent",
    )

    # Mock asyncio.create_subprocess_exec to track calls
    subprocess_calls = []

    async def mock_subprocess(*args, **kwargs):
        """Track subprocess calls."""
        subprocess_calls.append(args)
        proc = AsyncMock()
        proc.communicate = AsyncMock(return_value=(b"", b""))
        proc.returncode = 0
        return proc

    with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
        message_queue._wait_for_claude_prompt_async = AsyncMock(return_value=True)
        # Deliver urgent message
        await message_queue._deliver_urgent("test-abandoned", msg)

    # Verify subprocess calls - should NOT start with wake-up Enter
    assert len(subprocess_calls) >= 1

    # First call should be Escape (NOT Enter)
    first_call = subprocess_calls[0]
    assert first_call[4] == "Escape"


@pytest.mark.asyncio
async def test_urgent_delivery_acquires_delivery_lock(
    message_queue, mock_session_manager
):
    """Test that _deliver_urgent acquires the per-session delivery lock (#178)."""
    session = Session(
        id="test-lock",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="claude-test-lock",
        completion_status=None,
        friendly_name="lock-agent",
    )
    mock_session_manager.get_session.return_value = session

    msg = QueuedMessage(
        id="msg-lock",
        target_session_id="test-lock",
        text="urgent task",
        delivery_mode="urgent",
    )

    lock_acquired = []

    async def mock_subprocess(*args, **kwargs):
        proc = AsyncMock()
        proc.communicate = AsyncMock(return_value=(b"", b""))
        proc.returncode = 0
        return proc

    # Pre-create the lock and wrap it to track acquisition
    original_lock = asyncio.Lock()
    message_queue._delivery_locks["test-lock"] = original_lock
    original_acquire = original_lock.acquire

    async def tracking_acquire():
        lock_acquired.append(True)
        return await original_acquire()

    original_lock.acquire = tracking_acquire

    with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
        message_queue._wait_for_claude_prompt_async = AsyncMock(return_value=True)
        await message_queue._deliver_urgent("test-lock", msg)

    assert len(lock_acquired) == 1, "Delivery lock must be acquired exactly once"


@pytest.mark.asyncio
async def test_urgent_delivery_lock_prevents_concurrent_try_deliver(
    message_queue, mock_session_manager
):
    """Test that _deliver_urgent and _try_deliver_messages cannot run concurrently (#178)."""
    session = Session(
        id="test-mutex",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="claude-test-mutex",
        completion_status=None,
        friendly_name="mutex-agent",
    )
    mock_session_manager.get_session.return_value = session

    # Track which function holds the lock
    concurrent_overlap = []
    currently_running = []

    original_deliver_direct = mock_session_manager._deliver_direct

    async def slow_deliver_direct(*args, **kwargs):
        currently_running.append("urgent")
        await asyncio.sleep(0.05)  # Simulate work
        if "try_deliver" in currently_running:
            concurrent_overlap.append(True)
        currently_running.remove("urgent")
        return True

    mock_session_manager._deliver_direct = AsyncMock(side_effect=slow_deliver_direct)

    msg = QueuedMessage(
        id="msg-mutex",
        target_session_id="test-mutex",
        text="urgent task",
        delivery_mode="urgent",
    )

    async def mock_subprocess(*args, **kwargs):
        proc = AsyncMock()
        proc.communicate = AsyncMock(return_value=(b"", b""))
        proc.returncode = 0
        return proc

    with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
        message_queue._wait_for_claude_prompt_async = AsyncMock(return_value=True)
        # Run urgent delivery; while it holds the lock, _try_deliver_messages
        # must wait (not overlap)
        urgent_task = asyncio.create_task(
            message_queue._deliver_urgent("test-mutex", msg)
        )
        # Let urgent delivery start and acquire lock
        await asyncio.sleep(0.01)
        # Try to trigger sequential delivery concurrently
        state = message_queue._get_or_create_state("test-mutex")
        state.is_idle = True
        try_task = asyncio.create_task(
            message_queue._try_deliver_messages("test-mutex")
        )
        await asyncio.gather(urgent_task, try_task)

    # The two functions must not overlap while holding the lock
    assert len(concurrent_overlap) == 0, (
        "_deliver_urgent and _try_deliver_messages ran concurrently — lock not working"
    )


@pytest.mark.asyncio
async def test_urgent_delivery_marks_message_as_delivered(
    message_queue, mock_session_manager
):
    """Test that urgent delivery marks the message as delivered on success."""
    # Mock session
    session = Session(
        id="test-deliver",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="claude-test-deliver",
        completion_status=None,
    )

    mock_session_manager.get_session.return_value = session
    mock_session_manager.sessions = {"test-deliver": session}

    async def mock_subprocess(*args, **kwargs):
        """Return immediately so _deliver_urgent doesn't block on real tmux."""
        proc = AsyncMock()
        proc.communicate = AsyncMock(return_value=(b"", b""))
        proc.returncode = 0
        return proc

    # Start the message queue (required for background delivery)
    await message_queue.start()

    try:
        with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            # Mock prompt polling so urgent delivery completes immediately (#178 lock)
            message_queue._wait_for_claude_prompt_async = AsyncMock(return_value=True)

            # Create and queue a test message
            msg = message_queue.queue_message(
                target_session_id="test-deliver",
                text="urgent task",
                delivery_mode="urgent",
            )

            # Wait for delivery to complete (urgent path: _deliver_urgent acquires lock
            # then delivers; with mocked subprocess it completes quickly)
            await asyncio.sleep(0.3)

        # Verify message was marked as delivered
        pending = message_queue.get_pending_messages("test-deliver")
        assert len(pending) == 0  # Message should be delivered and removed from queue

        # Verify _deliver_direct was called
        mock_session_manager._deliver_direct.assert_called()
    finally:
        await message_queue.stop()
