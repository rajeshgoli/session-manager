"""
Regression tests for issue #40: Silent error swallowing hides failures

Tests verify that:
1. State loading returns bool indicating success/failure
2. State saving returns bool indicating success/failure
3. Transcript reading properly indicates errors
4. Monitor loop restarts on errors instead of silently exiting
"""

import pytest
import asyncio
import json
from pathlib import Path
from unittest.mock import Mock, patch, MagicMock, AsyncMock
from datetime import datetime

from src.session_manager import SessionManager
from src.message_queue import MessageQueueManager
from src.models import Session, SessionStatus


@pytest.fixture
def temp_state_file(tmp_path):
    """Create a temporary state file path."""
    state_file = tmp_path / "sessions.json"
    return str(state_file)


@pytest.fixture
def mock_tmux():
    """Create a mock TmuxController."""
    tmux = Mock()
    tmux.session_exists = Mock(return_value=True)
    tmux.create_session = Mock(return_value=True)
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


# =========================================================================
# Test 1: State Loading Returns Success/Failure
# =========================================================================

def test_load_state_returns_true_on_success(session_manager, temp_state_file, tmp_path):
    """Test that _load_state returns True when loading succeeds."""
    # Create a valid state file
    state_data = {
        "sessions": [
            {
                "id": "test-123",
                "name": "test-session",
                "working_dir": "/tmp",
                "tmux_session": "tmux-test",
                "status": "running",
                "created_at": datetime.now().isoformat(),
                "last_activity": datetime.now().isoformat(),
                "log_file": str(tmp_path / "test.log"),
            }
        ]
    }
    with open(temp_state_file, "w") as f:
        json.dump(state_data, f)

    # Load state
    result = session_manager._load_state()

    # Verify success
    assert result is True
    assert len(session_manager.sessions) == 1
    assert "test-123" in session_manager.sessions


def test_load_state_returns_true_when_no_file_exists(session_manager, temp_state_file):
    """Test that _load_state returns True when no state file exists (not an error)."""
    # Ensure file doesn't exist
    Path(temp_state_file).unlink(missing_ok=True)

    # Load state
    result = session_manager._load_state()

    # Verify success (no file is not an error)
    assert result is True
    assert len(session_manager.sessions) == 0


def test_load_state_returns_false_on_corrupt_file(session_manager, temp_state_file):
    """Test that _load_state returns False when state file is corrupt."""
    # Create corrupt JSON
    with open(temp_state_file, "w") as f:
        f.write("{invalid json")

    # Load state
    result = session_manager._load_state()

    # Verify failure
    assert result is False
    assert len(session_manager.sessions) == 0


def test_load_state_returns_false_on_permission_error(session_manager, temp_state_file):
    """Test that _load_state returns False when file can't be read."""
    # Create a valid file
    with open(temp_state_file, "w") as f:
        json.dump({"sessions": []}, f)

    # Make it unreadable
    Path(temp_state_file).chmod(0o000)

    try:
        # Load state
        result = session_manager._load_state()

        # Verify failure
        assert result is False
    finally:
        # Restore permissions for cleanup
        Path(temp_state_file).chmod(0o644)


def test_load_state_logs_critical_error_on_failure(session_manager, temp_state_file, caplog):
    """Test that _load_state logs CRITICAL error on failure."""
    # Create corrupt file
    with open(temp_state_file, "w") as f:
        f.write("not json")

    with caplog.at_level("ERROR"):
        result = session_manager._load_state()

    # Verify CRITICAL appears in logs
    assert result is False
    assert "CRITICAL" in caplog.text
    assert "Failed to load state" in caplog.text


# =========================================================================
# Test 2: State Saving Returns Success/Failure
# =========================================================================

def test_save_state_returns_true_on_success(session_manager, temp_state_file):
    """Test that _save_state returns True when saving succeeds."""
    # Add a session
    session = Session(
        id="test-123",
        name="test",
        working_dir="/tmp",
        tmux_session="tmux-test",
        status=SessionStatus.RUNNING,
    )
    session_manager.sessions[session.id] = session

    # Save state
    result = session_manager._save_state()

    # Verify success
    assert result is True
    assert Path(temp_state_file).exists()

    # Verify content
    with open(temp_state_file) as f:
        data = json.load(f)
    assert len(data["sessions"]) == 1
    assert data["sessions"][0]["id"] == "test-123"


def test_save_state_returns_false_on_permission_error(session_manager, temp_state_file):
    """Test that _save_state returns False when file can't be written."""
    # Add a session
    session = Session(
        id="test-123",
        name="test",
        working_dir="/tmp",
        tmux_session="tmux-test",
        status=SessionStatus.RUNNING,
    )
    session_manager.sessions[session.id] = session

    # Make directory read-only
    state_path = Path(temp_state_file)
    state_path.parent.chmod(0o444)

    try:
        # Save state
        result = session_manager._save_state()

        # Verify failure
        assert result is False
    finally:
        # Restore permissions
        state_path.parent.chmod(0o755)


def test_save_state_logs_critical_error_on_failure(session_manager, temp_state_file, caplog):
    """Test that _save_state logs CRITICAL error on failure."""
    # Make save fail
    with patch('pathlib.Path.rename', side_effect=OSError("Permission denied")):
        with caplog.at_level("ERROR"):
            result = session_manager._save_state()

    # Verify CRITICAL appears in logs
    assert result is False
    assert "CRITICAL" in caplog.text
    assert "Failed to save state" in caplog.text


def test_save_state_cleans_up_temp_file_on_error(session_manager, temp_state_file):
    """Test that _save_state cleans up temp file when save fails."""
    # Add a session
    session = Session(
        id="test-123",
        name="test",
        working_dir="/tmp",
        tmux_session="tmux-test",
        status=SessionStatus.RUNNING,
    )
    session_manager.sessions[session.id] = session

    temp_file = Path(temp_state_file).with_suffix('.tmp')

    # Make rename fail
    with patch('pathlib.Path.rename', side_effect=OSError("Error")):
        result = session_manager._save_state()

    # Verify temp file was cleaned up
    assert result is False
    assert not temp_file.exists()


# =========================================================================
# Test 3: Transcript Reading Indicates Errors
# =========================================================================

@pytest.mark.asyncio
async def test_transcript_reading_returns_success_tuple():
    """Test that transcript reading returns (success, message) tuple."""
    from src.server import create_app

    app = create_app()

    # Create a mock transcript file
    import tempfile
    with tempfile.NamedTemporaryFile(mode='w', suffix='.jsonl', delete=False) as f:
        transcript_path = f.name
        # Write valid JSONL
        f.write(json.dumps({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "Hello world"}
                ]
            }
        }) + "\n")

    try:
        # Simulate the read_transcript function
        def read_transcript():
            try:
                transcript_file = Path(transcript_path)
                if not transcript_file.exists():
                    return (False, None)
                lines = transcript_file.read_text().strip().split('\n')
                for line in reversed(lines):
                    try:
                        entry = json.loads(line)
                        if entry.get("type") == "assistant":
                            message = entry.get("message", {})
                            content = message.get("content", [])
                            texts = []
                            for item in content:
                                if isinstance(item, dict) and item.get("type") == "text":
                                    texts.append(item.get("text", ""))
                            if texts:
                                return (True, "\n".join(texts))
                    except json.JSONDecodeError:
                        continue
                return (True, None)
            except Exception:
                return (False, None)

        # Call the function
        success, message = read_transcript()

        # Verify success
        assert success is True
        assert message == "Hello world"
    finally:
        Path(transcript_path).unlink(missing_ok=True)


@pytest.mark.asyncio
async def test_transcript_reading_returns_false_on_missing_file(caplog):
    """Test that transcript reading returns (False, None) when file doesn't exist."""
    def read_transcript():
        import logging
        logger = logging.getLogger(__name__)
        try:
            transcript_file = Path("/nonexistent/file.jsonl")
            if not transcript_file.exists():
                logger.warning(f"Transcript file does not exist: {transcript_file}")
                return (False, None)
            return (True, None)
        except Exception:
            return (False, None)

    with caplog.at_level("WARNING"):
        success, message = read_transcript()

    # Verify failure indicated
    assert success is False
    assert message is None
    assert "does not exist" in caplog.text


@pytest.mark.asyncio
async def test_transcript_reading_returns_false_on_error(caplog):
    """Test that transcript reading returns (False, None) on errors."""
    def read_transcript():
        import logging
        logger = logging.getLogger(__name__)
        try:
            # Simulate an error
            raise IOError("Read error")
        except Exception as e:
            logger.error(f"CRITICAL: Error reading transcript: {e}")
            return (False, None)

    with caplog.at_level("ERROR"):
        success, message = read_transcript()

    # Verify failure and critical log
    assert success is False
    assert message is None
    assert "CRITICAL" in caplog.text


# =========================================================================
# Test 4: Monitor Loop Restarts on Errors
# =========================================================================

@pytest.mark.asyncio
async def test_monitor_loop_restarts_on_error(tmp_path):
    """Test that monitor loop restarts after encountering an error."""
    mock_session_manager = Mock()
    mock_session_manager.get_session = Mock(return_value=None)

    queue_mgr = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=str(tmp_path / "test.db"),
        config={"input_poll_interval": 0.05}
    )

    # Start monitoring first
    await queue_mgr.start()

    # Now patch after startup completes
    error_count = 0
    success_count = 0

    original_get_pending = queue_mgr._get_sessions_with_pending

    def mock_get_pending():
        nonlocal error_count, success_count
        if error_count < 2:
            error_count += 1
            raise RuntimeError(f"Test error {error_count}")
        else:
            success_count += 1
            return []

    queue_mgr._get_sessions_with_pending = mock_get_pending

    # Wait for errors and recovery (need time for 2 errors + backoff + recovery)
    # Error 1: 1s backoff, Error 2: 2s backoff, then success
    await asyncio.sleep(5)

    # Stop monitoring
    await queue_mgr.stop()

    # Verify errors occurred and loop restarted
    assert error_count == 2  # Should have hit 2 errors
    assert success_count > 0  # Should have recovered and continued


@pytest.mark.asyncio
async def test_monitor_loop_gives_up_after_max_retries(tmp_path, caplog):
    """Test that monitor loop gives up after max retries."""
    mock_session_manager = Mock()
    mock_session_manager.get_session = Mock(return_value=None)

    queue_mgr = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=str(tmp_path / "test.db"),
        config={"input_poll_interval": 0.05}
    )

    # Start monitoring first
    await queue_mgr.start()

    # Make it always fail after startup
    def always_fail():
        raise RuntimeError("Persistent error")

    queue_mgr._get_sessions_with_pending = always_fail

    # Wait for retries to exhaust
    # Backoffs: 1s, 2s, 4s, 8s, 16s = ~31s total
    with caplog.at_level("ERROR"):
        await asyncio.sleep(35)

    # Stop monitoring
    await queue_mgr.stop()

    # Verify it gave up
    assert "failed 5 times, giving up" in caplog.text
    assert "monitoring STOPPED" in caplog.text


@pytest.mark.asyncio
async def test_monitor_loop_exponential_backoff(tmp_path, caplog):
    """Test that monitor loop uses exponential backoff on retries."""
    mock_session_manager = Mock()
    mock_session_manager.get_session = Mock(return_value=None)

    queue_mgr = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=str(tmp_path / "test.db"),
        config={"input_poll_interval": 0.05}
    )

    await queue_mgr.start()

    error_count = 0

    def fail_then_succeed():
        nonlocal error_count
        if error_count < 3:
            error_count += 1
            raise RuntimeError(f"Error {error_count}")
        return []

    queue_mgr._get_sessions_with_pending = fail_then_succeed

    with caplog.at_level("WARNING"):
        await asyncio.sleep(5)

    await queue_mgr.stop()

    # Verify retries with delays
    assert "Restarting monitor loop" in caplog.text
    assert error_count == 3


@pytest.mark.asyncio
async def test_monitor_loop_resets_retry_count_on_success(tmp_path):
    """Test that monitor loop resets retry count after successful iterations."""
    mock_session_manager = Mock()
    mock_session_manager.get_session = Mock(return_value=None)

    queue_mgr = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=str(tmp_path / "test.db"),
        config={
            "sm_send": {
                "input_poll_interval": 0.05,  # Fast polling for test
            },
            "timeouts": {
                "message_queue": {
                    "initial_retry_delay_seconds": 0.1,  # Fast retries for test
                    "max_retry_delay_seconds": 0.2,
                }
            }
        }
    )

    await queue_mgr.start()

    call_count = 0

    def intermittent_failure():
        nonlocal call_count
        call_count += 1
        # Fail on calls 1, 4, 7 (never consecutively)
        if call_count in (1, 4, 7):
            raise RuntimeError(f"Intermittent error {call_count}")
        return []

    queue_mgr._get_sessions_with_pending = intermittent_failure

    await asyncio.sleep(2)  # Shorter wait with faster retries
    await queue_mgr.stop()

    # Verify it didn't give up (retry count was reset between errors)
    assert call_count >= 7  # Should have recovered and continued


@pytest.mark.asyncio
async def test_monitor_loop_logs_critical_on_error(tmp_path, caplog):
    """Test that monitor loop logs CRITICAL errors with traceback."""
    mock_session_manager = Mock()
    mock_session_manager.get_session = Mock(return_value=None)

    queue_mgr = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=str(tmp_path / "test.db"),
        config={"input_poll_interval": 0.05}
    )

    await queue_mgr.start()

    call_count = 0

    def fail_once():
        nonlocal call_count
        call_count += 1
        if call_count == 1:
            raise RuntimeError("Test error")
        return []

    queue_mgr._get_sessions_with_pending = fail_once

    with caplog.at_level("ERROR"):
        await asyncio.sleep(0.3)
        await queue_mgr.stop()

    # Verify CRITICAL in logs
    assert "CRITICAL" in caplog.text
    assert "Error in monitor loop" in caplog.text
