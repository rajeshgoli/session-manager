"""
Regression tests for issue #36: spawn_child_session wait parameter accepted but not implemented

Tests verify that when wait parameter is provided to spawn_child_session,
the child_monitor.register_child is called properly.
"""

import pytest
from pathlib import Path
from unittest.mock import Mock, MagicMock
from datetime import datetime

from src.session_manager import SessionManager
from src.models import Session, SessionStatus


@pytest.fixture
def temp_state_file(tmp_path):
    """Create a temporary state file."""
    state_file = tmp_path / "sessions.json"
    import json
    with open(state_file, "w") as f:
        json.dump({"sessions": []}, f)
    return str(state_file)


@pytest.fixture
def mock_child_monitor():
    """Create a mock ChildMonitor."""
    monitor = Mock()
    monitor.register_child = Mock()
    return monitor


@pytest.fixture
def session_manager(temp_state_file, tmp_path, mock_child_monitor):
    """Create a SessionManager for testing."""
    manager = SessionManager(
        log_dir=str(tmp_path),
        state_file=temp_state_file,
    )

    # Replace tmux controller with mock
    mock_tmux = Mock()
    mock_tmux.session_exists = Mock(return_value=True)
    mock_tmux.create_session_with_command = Mock(return_value=True)
    manager.tmux = mock_tmux

    # Set child monitor
    manager.child_monitor = mock_child_monitor

    return manager


@pytest.mark.asyncio
async def test_spawn_child_with_wait_registers_monitor(session_manager, mock_child_monitor):
    """Test that spawn_child_session with wait parameter calls child_monitor.register_child."""
    # Create a parent session first
    parent_session = Session(
        id="parent-123",
        name="parent-session",
        working_dir="/tmp/test",
        tmux_session="tmux-parent",
        status=SessionStatus.RUNNING,
    )
    session_manager.sessions[parent_session.id] = parent_session

    # Spawn a child session with wait parameter
    child_session = await session_manager.spawn_child_session(
        parent_session_id=parent_session.id,
        prompt="Test prompt",
        name="test-child",
        wait=300,  # Wait 300 seconds
    )

    # Verify child session was created
    assert child_session is not None
    assert child_session.parent_session_id == parent_session.id

    # Verify child_monitor.register_child was called
    mock_child_monitor.register_child.assert_called_once()

    # Verify the call had correct parameters
    call_args = mock_child_monitor.register_child.call_args
    assert call_args[1]["child_session_id"] == child_session.id
    assert call_args[1]["parent_session_id"] == parent_session.id
    assert call_args[1]["wait_seconds"] == 300


@pytest.mark.asyncio
async def test_spawn_child_without_wait_skips_monitor(session_manager, mock_child_monitor):
    """Test that spawn_child_session without wait parameter doesn't call child_monitor.register_child."""
    # Create a parent session
    parent_session = Session(
        id="parent-456",
        name="parent-session-2",
        working_dir="/tmp/test2",
        tmux_session="tmux-parent-2",
        status=SessionStatus.RUNNING,
    )
    session_manager.sessions[parent_session.id] = parent_session

    # Spawn a child session WITHOUT wait parameter
    child_session = await session_manager.spawn_child_session(
        parent_session_id=parent_session.id,
        prompt="Test prompt",
        name="test-child-2",
        wait=None,  # No wait
    )

    # Verify child session was created
    assert child_session is not None
    assert child_session.parent_session_id == parent_session.id

    # Verify child_monitor.register_child was NOT called
    mock_child_monitor.register_child.assert_not_called()


@pytest.mark.asyncio
async def test_spawn_child_with_wait_zero_skips_monitor(session_manager, mock_child_monitor):
    """Test that spawn_child_session with wait=0 doesn't call child_monitor.register_child."""
    # Create a parent session
    parent_session = Session(
        id="parent-789",
        name="parent-session-3",
        working_dir="/tmp/test3",
        tmux_session="tmux-parent-3",
        status=SessionStatus.RUNNING,
    )
    session_manager.sessions[parent_session.id] = parent_session

    # Spawn a child session with wait=0 (falsy value)
    child_session = await session_manager.spawn_child_session(
        parent_session_id=parent_session.id,
        prompt="Test prompt",
        name="test-child-3",
        wait=0,  # Falsy value
    )

    # Verify child session was created
    assert child_session is not None

    # Verify child_monitor.register_child was NOT called (0 is falsy)
    mock_child_monitor.register_child.assert_not_called()


@pytest.mark.asyncio
async def test_spawn_child_without_child_monitor_succeeds(session_manager):
    """Test that spawn_child_session works even when child_monitor is not set."""
    # Remove child monitor
    session_manager.child_monitor = None

    # Create a parent session
    parent_session = Session(
        id="parent-abc",
        name="parent-session-4",
        working_dir="/tmp/test4",
        tmux_session="tmux-parent-4",
        status=SessionStatus.RUNNING,
    )
    session_manager.sessions[parent_session.id] = parent_session

    # Spawn a child session with wait parameter (but no child_monitor)
    child_session = await session_manager.spawn_child_session(
        parent_session_id=parent_session.id,
        prompt="Test prompt",
        name="test-child-4",
        wait=300,
    )

    # Verify child session was created (doesn't crash)
    assert child_session is not None
    assert child_session.parent_session_id == parent_session.id


@pytest.mark.asyncio
async def test_spawn_child_with_wait_positive_values(session_manager, mock_child_monitor):
    """Test various positive wait values."""
    parent_session = Session(
        id="parent-def",
        name="parent-session-5",
        working_dir="/tmp/test5",
        tmux_session="tmux-parent-5",
        status=SessionStatus.RUNNING,
    )
    session_manager.sessions[parent_session.id] = parent_session

    test_wait_values = [1, 60, 300, 600, 3600]

    for wait_val in test_wait_values:
        # Reset the mock
        mock_child_monitor.reset_mock()

        # Spawn child with specific wait value
        child_session = await session_manager.spawn_child_session(
            parent_session_id=parent_session.id,
            prompt="Test prompt",
            name=f"test-child-wait-{wait_val}",
            wait=wait_val,
        )

        # Verify register_child was called with correct wait_seconds
        mock_child_monitor.register_child.assert_called_once()
        call_args = mock_child_monitor.register_child.call_args
        assert call_args[1]["wait_seconds"] == wait_val
