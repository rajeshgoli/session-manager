"""
Regression tests for issue #37: Blocking I/O in async context degrades performance

Tests verify that async operations don't use blocking subprocess calls.
"""

import pytest
import asyncio
from pathlib import Path
from unittest.mock import Mock, AsyncMock, patch, call
from datetime import datetime

from src.session_manager import SessionManager
from src.message_queue import MessageQueueManager
from src.models import Session, SessionStatus, DeliveryResult


@pytest.fixture
def temp_state_file(tmp_path):
    """Create a temporary state file."""
    state_file = tmp_path / "sessions.json"
    import json
    with open(state_file, "w") as f:
        json.dump({"sessions": []}, f)
    return str(state_file)


@pytest.fixture
def mock_tmux():
    """Create a mock TmuxController."""
    tmux = Mock()
    tmux.session_exists = Mock(return_value=True)
    tmux.create_session = Mock(return_value=True)
    tmux.send_input_async = AsyncMock(return_value=True)
    return tmux


@pytest.fixture
def session_manager(temp_state_file, tmp_path, mock_tmux):
    """Create a SessionManager for testing."""
    manager = SessionManager(
        log_dir=str(tmp_path),
        state_file=temp_state_file,
    )
    manager.tmux = mock_tmux
    return manager


@pytest.mark.asyncio
async def test_get_git_remote_url_async_no_blocking():
    """Test that _get_git_remote_url_async doesn't block event loop."""
    from src.session_manager import SessionManager

    manager = SessionManager(log_dir="/tmp", state_file="/tmp/test.json")

    # Mock asyncio.create_subprocess_exec to verify it's called (not subprocess.run)
    with patch('asyncio.create_subprocess_exec') as mock_subprocess:
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"https://github.com/test/repo.git\n", b""))
        mock_proc.returncode = 0
        mock_subprocess.return_value = mock_proc

        # Call async version
        result = await manager._get_git_remote_url_async("/tmp/test")

        # Verify asyncio.create_subprocess_exec was used, not subprocess.run
        mock_subprocess.assert_called_once()
        assert result == "https://github.com/test/repo.git"


@pytest.mark.asyncio
async def test_create_session_async_no_blocking(session_manager, mock_tmux):
    """Test that create_session is async and doesn't block."""
    # Mock _get_git_remote_url_async
    with patch.object(session_manager, '_get_git_remote_url_async', return_value="https://github.com/test/repo.git") as mock_git:
        # Create session (should be awaitable)
        session = await session_manager.create_session(
            working_dir="/tmp/test",
            name="test-session",
        )

        # Verify async git remote was called
        mock_git.assert_called_once_with("/tmp/test")
        assert session is not None
        assert session.git_remote_url == "https://github.com/test/repo.git"


@pytest.mark.asyncio
async def test_spawn_child_session_async_no_blocking(session_manager, mock_tmux):
    """Test that spawn_child_session is async and doesn't block."""
    # Create parent session first
    parent = Session(
        id="parent-123",
        name="parent",
        working_dir="/tmp/test",
        tmux_session="tmux-parent",
        status=SessionStatus.RUNNING,
    )
    session_manager.sessions[parent.id] = parent

    # Mock _get_git_remote_url_async
    with patch.object(session_manager, '_get_git_remote_url_async', return_value="https://github.com/test/repo.git") as mock_git:
        with patch.object(session_manager.tmux, 'create_session_with_command', return_value=True):
            # Spawn child (should be awaitable)
            child = await session_manager.spawn_child_session(
                parent_session_id=parent.id,
                prompt="Test prompt",
                name="child-session",
            )

            # Verify async git remote was called
            mock_git.assert_called_once()
            assert child is not None
            assert child.parent_session_id == parent.id


@pytest.mark.asyncio
async def test_send_input_async_uses_send_input_async(session_manager, mock_tmux):
    """Test that send_input uses tmux.send_input_async (not blocking send_input)."""
    # Create session
    session = Session(
        id="test-123",
        name="test",
        working_dir="/tmp/test",
        tmux_session="tmux-test",
        status=SessionStatus.RUNNING,
    )
    session_manager.sessions[session.id] = session

    # Call send_input (with bypass_queue to trigger direct send)
    success = await session_manager.send_input(
        session_id=session.id,
        text="test input",
        bypass_queue=True,
    )

    # Verify send_input_async was called (not send_input)
    mock_tmux.send_input_async.assert_called_once_with("tmux-test", "test input")
    assert success == DeliveryResult.DELIVERED


@pytest.mark.asyncio
async def test_get_pending_user_input_async_no_blocking():
    """Test that _get_pending_user_input_async doesn't block event loop."""
    from src.message_queue import MessageQueueManager

    manager = MessageQueueManager(
        session_manager=Mock(),
        db_path=":memory:",
    )

    # Mock asyncio.create_subprocess_exec
    with patch('asyncio.create_subprocess_exec') as mock_subprocess:
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"> test input\n", b""))
        mock_proc.returncode = 0
        mock_subprocess.return_value = mock_proc

        # Call async version
        result = await manager._get_pending_user_input_async("tmux-session")

        # Verify asyncio.create_subprocess_exec was used
        mock_subprocess.assert_called_once()
        assert result == "test input"


@pytest.mark.asyncio
async def test_async_functions_complete_fast():
    """Test that async operations complete quickly (not blocked by subprocess)."""
    from src.session_manager import SessionManager

    manager = SessionManager(log_dir="/tmp", state_file="/tmp/test.json")

    # Mock subprocess to simulate fast async execution
    with patch('asyncio.create_subprocess_exec') as mock_subprocess:
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"https://github.com/test/repo.git\n", b""))
        mock_proc.returncode = 0
        mock_subprocess.return_value = mock_proc

        # Time the async operation
        start = asyncio.get_event_loop().time()
        result = await manager._get_git_remote_url_async("/tmp/test")
        elapsed = asyncio.get_event_loop().time() - start

        # Should complete very fast (< 100ms) since it's not blocking
        assert elapsed < 0.1
        assert result == "https://github.com/test/repo.git"


@pytest.mark.asyncio
async def test_no_blocking_subprocess_run_in_async_path():
    """Verify that blocking subprocess.run is not used in async code paths."""
    from src.session_manager import SessionManager
    from src.message_queue import MessageQueueManager

    # This test ensures we don't accidentally use subprocess.run in async contexts
    # by patching it and verifying it's NOT called

    manager = SessionManager(log_dir="/tmp", state_file="/tmp/test.json")

    with patch('subprocess.run') as mock_blocking_run:
        with patch('asyncio.create_subprocess_exec') as mock_async_exec:
            mock_proc = AsyncMock()
            mock_proc.communicate = AsyncMock(return_value=(b"result\n", b""))
            mock_proc.returncode = 0
            mock_async_exec.return_value = mock_proc

            # Call async git remote
            await manager._get_git_remote_url_async("/tmp/test")

            # Verify blocking subprocess.run was NOT called
            mock_blocking_run.assert_not_called()

            # Verify async version was called
            mock_async_exec.assert_called_once()


@pytest.mark.asyncio
async def test_concurrent_async_operations_dont_block():
    """Test that multiple concurrent async operations don't block each other."""
    from src.session_manager import SessionManager

    manager = SessionManager(log_dir="/tmp", state_file="/tmp/test.json")

    # Mock subprocess with delays
    call_count = 0
    async def mock_create_subprocess(*args, **kwargs):
        nonlocal call_count
        call_count += 1
        mock_proc = AsyncMock()
        # Simulate some async delay
        async def delayed_communicate():
            await asyncio.sleep(0.01)  # 10ms delay
            return (b"result\n", b"")
        mock_proc.communicate = delayed_communicate
        mock_proc.returncode = 0
        return mock_proc

    with patch('asyncio.create_subprocess_exec', side_effect=mock_create_subprocess):
        # Run 10 concurrent operations
        tasks = [
            manager._get_git_remote_url_async(f"/tmp/test{i}")
            for i in range(10)
        ]

        start = asyncio.get_event_loop().time()
        results = await asyncio.gather(*tasks)
        elapsed = asyncio.get_event_loop().time() - start

        # All 10 should complete in roughly the same time as 1 (concurrent, not sequential)
        # With blocking calls, this would take ~100ms (10 * 10ms)
        # With async, should take ~10-20ms (concurrent)
        assert elapsed < 0.1  # Less than 100ms
        assert call_count == 10
        assert all(r == "result" for r in results)


@pytest.mark.asyncio
async def test_clear_user_input_async_no_blocking():
    """Test that _clear_user_input_async doesn't block event loop."""
    from src.message_queue import MessageQueueManager

    manager = MessageQueueManager(
        session_manager=Mock(),
        db_path=":memory:",
    )

    # Mock asyncio.create_subprocess_exec
    with patch('asyncio.create_subprocess_exec') as mock_subprocess:
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))
        mock_proc.returncode = 0
        mock_subprocess.return_value = mock_proc

        # Call async version
        result = await manager._clear_user_input_async("tmux-session")

        # Verify asyncio.create_subprocess_exec was used
        mock_subprocess.assert_called_once()
        # Verify correct command (Ctrl+U to clear input)
        args = mock_subprocess.call_args[0]
        assert args == ("tmux", "send-keys", "-t", "tmux-session", "C-u")
        assert result is True


@pytest.mark.asyncio
async def test_restore_user_input_async_no_blocking():
    """Test that _restore_user_input_async doesn't block event loop."""
    from src.message_queue import MessageQueueManager

    manager = MessageQueueManager(
        session_manager=Mock(),
        db_path=":memory:",
    )

    # Mock asyncio.create_subprocess_exec
    with patch('asyncio.create_subprocess_exec') as mock_subprocess:
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))
        mock_proc.returncode = 0
        mock_subprocess.return_value = mock_proc

        # Call async version
        await manager._restore_user_input_async("tmux-session", "test input")

        # Verify asyncio.create_subprocess_exec was used
        mock_subprocess.assert_called_once()
        # Verify correct command (restore text without Enter)
        args = mock_subprocess.call_args[0]
        assert args == ("tmux", "send-keys", "-t", "tmux-session", "--", "test input")


@pytest.mark.asyncio
async def test_deliver_urgent_uses_async_escape():
    """Test that _deliver_urgent uses async subprocess for Escape key."""
    from src.message_queue import MessageQueueManager

    mock_session_manager = Mock()
    mock_session = Mock()
    mock_session.tmux_session = "tmux-test"
    mock_session_manager.get_session = Mock(return_value=mock_session)
    mock_session_manager.tmux = Mock()
    mock_session_manager.tmux.send_input_async = AsyncMock(return_value=True)

    manager = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=":memory:",
    )

    # Create a mock message
    from src.message_queue import QueuedMessage
    msg = QueuedMessage(
        id=1,
        target_session_id="test-123",
        text="urgent message",
        delivery_mode="urgent",
        queued_at=datetime.now(),
    )

    # Mock asyncio.create_subprocess_exec to track calls
    with patch('asyncio.create_subprocess_exec') as mock_subprocess:
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))
        mock_proc.returncode = 0
        mock_subprocess.return_value = mock_proc

        # Deliver urgent message
        await manager._deliver_urgent("test-123", msg)

        # Verify asyncio.create_subprocess_exec was used for Escape
        # Should be called for Escape key
        escape_call = [call for call in mock_subprocess.call_args_list
                      if "Escape" in str(call)]
        assert len(escape_call) > 0, "Escape should be sent via async subprocess"


@pytest.mark.asyncio
async def test_message_queue_no_blocking_in_delivery_path():
    """Comprehensive test that message queue delivery uses no blocking calls."""
    from src.message_queue import MessageQueueManager

    mock_session_manager = Mock()
    mock_session = Mock()
    mock_session.tmux_session = "tmux-test"
    mock_session_manager.get_session = Mock(return_value=mock_session)
    mock_session_manager.tmux = Mock()
    mock_session_manager.tmux.send_input_async = AsyncMock(return_value=True)

    manager = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=":memory:",
    )

    # Verify no blocking subprocess.run calls in the entire delivery flow
    with patch('subprocess.run') as mock_blocking_run:
        with patch('asyncio.create_subprocess_exec') as mock_async_exec:
            mock_proc = AsyncMock()
            mock_proc.communicate = AsyncMock(return_value=(b"", b""))
            mock_proc.returncode = 0
            mock_async_exec.return_value = mock_proc

            # Test clear user input
            await manager._clear_user_input_async("tmux-test")

            # Test restore user input
            await manager._restore_user_input_async("tmux-test", "test")

            # Test user input detection
            await manager._get_pending_user_input_async("tmux-test")

            # Verify no blocking calls were made
            mock_blocking_run.assert_not_called()

            # Verify async calls were made
            assert mock_async_exec.call_count >= 3
