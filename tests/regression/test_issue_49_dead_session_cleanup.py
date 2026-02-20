"""
Regression tests for issue #49: Dead sessions never cleaned up

Tests verify that:
1. OutputMonitor detects tmux death and cleans up
2. Telegram topics are deleted when sessions die
3. In-memory mappings are cleaned up
4. kill_session() performs full cleanup
"""

import pytest
import asyncio
from pathlib import Path
from unittest.mock import Mock, AsyncMock, patch, MagicMock
from datetime import datetime

from src.output_monitor import OutputMonitor
from src.models import Session, SessionStatus


@pytest.fixture
def mock_session():
    """Create a mock session."""
    session = Session(
        id="test-123",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="claude-test-123",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
        telegram_chat_id=12345,
        telegram_thread_id=67890,
    )
    return session


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager."""
    manager = Mock()
    manager.sessions = {}
    manager.tmux = Mock()
    manager.tmux.session_exists = Mock(return_value=True)
    manager._save_state = Mock()
    manager.app = Mock()
    manager.app.state = Mock()
    manager.app.state.last_claude_output = {}

    # Mock notifier with telegram (not telegram_bot)
    manager.notifier = Mock()
    manager.notifier.telegram = Mock()
    manager.notifier.telegram.bot = AsyncMock()
    manager.notifier.telegram._topic_sessions = {}
    manager.notifier.telegram._session_threads = {}
    # send_with_fallback is async (#200); return msg_id to confirm forum path
    manager.notifier.telegram.send_with_fallback = AsyncMock(return_value=9999)

    return manager


@pytest.fixture
def output_monitor(mock_session_manager):
    """Create OutputMonitor with mocked session manager."""
    monitor = OutputMonitor(poll_interval=0.1)  # Fast polling for tests
    monitor.set_session_manager(mock_session_manager)
    monitor.set_save_state_callback(mock_session_manager._save_state)
    return monitor


@pytest.mark.asyncio
async def test_monitor_detects_tmux_death(output_monitor, mock_session, mock_session_manager, tmp_path):
    """Test that monitor detects when tmux session dies and cleans up."""
    # Setup: Create log file
    log_file = tmp_path / "test.log"
    log_file.touch()
    mock_session.log_file = str(log_file)

    # Add session to manager
    mock_session_manager.sessions[mock_session.id] = mock_session

    # Simulate tmux existing initially
    mock_session_manager.tmux.session_exists = Mock(return_value=True)

    # Start monitoring
    await output_monitor.start_monitoring(mock_session)

    # Wait for a few polls
    await asyncio.sleep(0.3)

    # Simulate tmux death after ~30 polls
    # We need to wait for check_counter to hit 30
    # With 0.1s poll interval, 30 polls = 3 seconds
    # Let's make tmux die and wait
    mock_session_manager.tmux.session_exists = Mock(return_value=False)

    # Wait for detection (30 polls * 0.1s = 3s, add buffer)
    await asyncio.sleep(3.5)

    # Verify cleanup happened
    # Session should be removed from sessions dict
    assert mock_session.id not in mock_session_manager.sessions

    # Status should be STOPPED
    assert mock_session.status == SessionStatus.STOPPED

    # State should be saved
    mock_session_manager._save_state.assert_called()

    # "Session stopped" notification should have been sent (try-and-fallback, #200)
    mock_session_manager.notifier.telegram.send_with_fallback.assert_called_once_with(
        chat_id=12345,
        message=f"Session stopped [{mock_session.id}]",
        thread_id=67890,
    )

    # Monitoring should have stopped
    assert mock_session.id not in output_monitor._tasks


@pytest.mark.asyncio
async def test_cleanup_session_full_workflow(output_monitor, mock_session, mock_session_manager):
    """Test that cleanup_session performs all cleanup steps."""
    # Add session to manager
    mock_session_manager.sessions[mock_session.id] = mock_session

    # Add to telegram mappings
    mock_session_manager.notifier.telegram._topic_sessions[(12345, 67890)] = mock_session.id
    mock_session_manager.notifier.telegram._session_threads[mock_session.id] = (12345, 99999)

    # Add to hook output cache
    mock_session_manager.app.state.last_claude_output[mock_session.id] = "some output"

    # Add to monitoring state
    output_monitor._file_positions[mock_session.id] = 1000
    output_monitor._last_activity[mock_session.id] = datetime.now()
    output_monitor._notified_permissions[mock_session.id] = datetime.now()

    # Call cleanup
    await output_monitor.cleanup_session(mock_session)

    # Verify all cleanup happened
    assert mock_session.status == SessionStatus.STOPPED
    assert mock_session.id not in mock_session_manager.sessions
    assert mock_session_manager._save_state.called

    # Telegram cleanup
    assert (12345, 67890) not in mock_session_manager.notifier.telegram._topic_sessions
    assert mock_session.id not in mock_session_manager.notifier.telegram._session_threads
    # "Session stopped" sent via try-and-fallback (#200); delete_forum_topic no longer used
    mock_session_manager.notifier.telegram.send_with_fallback.assert_called_once_with(
        chat_id=12345,
        message=f"Session stopped [{mock_session.id}]",
        thread_id=67890,
    )

    # Hook output cache cleanup
    assert mock_session.id not in mock_session_manager.app.state.last_claude_output

    # Monitoring state cleanup
    assert mock_session.id not in output_monitor._file_positions
    assert mock_session.id not in output_monitor._last_activity
    assert mock_session.id not in output_monitor._notified_permissions


@pytest.mark.asyncio
async def test_cleanup_handles_telegram_notification_failure(output_monitor, mock_session, mock_session_manager):
    """Test that cleanup continues even if Telegram notification fails (#200: try-and-fallback)."""
    # Add session to manager
    mock_session_manager.sessions[mock_session.id] = mock_session

    # Both forum and fallback sends fail (send_with_fallback returns None)
    mock_session_manager.notifier.telegram.send_with_fallback = AsyncMock(return_value=None)

    # Call cleanup - should not raise
    await output_monitor.cleanup_session(mock_session)

    # Verify cleanup still happened for other things
    assert mock_session.status == SessionStatus.STOPPED
    assert mock_session.id not in mock_session_manager.sessions
    assert mock_session_manager._save_state.called


@pytest.mark.asyncio
async def test_cleanup_without_telegram(output_monitor, mock_session, mock_session_manager):
    """Test that cleanup works when session has no Telegram integration."""
    # Create session without Telegram
    mock_session.telegram_chat_id = None
    mock_session.telegram_thread_id = None

    # Add session to manager
    mock_session_manager.sessions[mock_session.id] = mock_session

    # Call cleanup
    await output_monitor.cleanup_session(mock_session)

    # Verify cleanup happened (without Telegram calls)
    assert mock_session.status == SessionStatus.STOPPED
    assert mock_session.id not in mock_session_manager.sessions

    # Telegram delete should not be called
    mock_session_manager.notifier.telegram.bot.delete_forum_topic.assert_not_called()


@pytest.mark.asyncio
async def test_cleanup_without_notifier(output_monitor, mock_session, mock_session_manager):
    """Test that cleanup works when notifier is not available."""
    # Remove notifier
    mock_session_manager.notifier = None

    # Add session to manager
    mock_session_manager.sessions[mock_session.id] = mock_session

    # Call cleanup - should not crash
    await output_monitor.cleanup_session(mock_session)

    # Verify basic cleanup happened
    assert mock_session.status == SessionStatus.STOPPED
    assert mock_session.id not in mock_session_manager.sessions


@pytest.mark.asyncio
async def test_monitor_checks_tmux_every_30_polls(output_monitor, mock_session, mock_session_manager, tmp_path):
    """Test that tmux existence is checked every 30 polls, not every poll."""
    # Setup: Create log file
    log_file = tmp_path / "test.log"
    log_file.touch()
    mock_session.log_file = str(log_file)

    # Add session to manager
    mock_session_manager.sessions[mock_session.id] = mock_session

    # Track session_exists calls
    call_count = 0
    def mock_session_exists(tmux_session):
        nonlocal call_count
        call_count += 1
        return True

    mock_session_manager.tmux.session_exists = Mock(side_effect=mock_session_exists)

    # Start monitoring
    await output_monitor.start_monitoring(mock_session)

    # Wait for ~35 polls (3.5 seconds with 0.1s interval)
    await asyncio.sleep(3.5)

    # Stop monitoring
    await output_monitor.stop_monitoring(mock_session.id)

    # Should have checked tmux existence ~1 time (at poll 30)
    # With some timing variance, accept 1-2 calls
    assert 1 <= call_count <= 2


@pytest.mark.asyncio
async def test_cleanup_removes_from_all_telegram_mappings(output_monitor, mock_session, mock_session_manager):
    """Test that cleanup removes session from both topic and thread mappings."""
    # Add session to manager
    mock_session_manager.sessions[mock_session.id] = mock_session

    # Add to both topic and thread mappings
    mock_session_manager.notifier.telegram._topic_sessions[(12345, 67890)] = mock_session.id
    mock_session_manager.notifier.telegram._session_threads[mock_session.id] = (12345, 11111)

    # Call cleanup
    await output_monitor.cleanup_session(mock_session)

    # Verify both removed
    assert (12345, 67890) not in mock_session_manager.notifier.telegram._topic_sessions
    assert mock_session.id not in mock_session_manager.notifier.telegram._session_threads
