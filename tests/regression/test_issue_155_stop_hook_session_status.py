"""
Regression tests for issue #155: Stop hook does not update session.status

The Stop hook updates delivery_state.is_idle but not session.status. This
causes crash recovery to fail: _handle_crash() checks session.status, finds
RUNNING (never updated by Stop hook), and defers recovery unnecessarily.

Tests verify that:
1. Stop hook sets session.status to IDLE for RUNNING sessions
2. Stop hook skips STOPPED sessions (guard against late hooks after kill)
3. Both delivery_state.is_idle and session.status stay in sync after Stop hook
4. Crash recovery fires immediately after Stop hook (not deferred)
"""

import pytest
from unittest.mock import MagicMock, AsyncMock, patch
from datetime import datetime

from fastapi.testclient import TestClient

from src.server import create_app
from src.models import Session, SessionStatus
from src.output_monitor import OutputMonitor


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager for testing."""
    mock = MagicMock()
    mock.sessions = {}
    mock.tmux = MagicMock()
    mock.tmux.send_input_async = AsyncMock(return_value=True)
    mock.tmux.list_sessions = MagicMock(return_value=[])
    mock.message_queue_manager = None
    mock._save_state = MagicMock()
    return mock


@pytest.fixture
def mock_output_monitor():
    """Create a mock OutputMonitor."""
    mock = MagicMock()
    mock.start_monitoring = AsyncMock()
    mock.cleanup_session = AsyncMock()
    mock.update_activity = MagicMock()
    mock._tasks = {}
    return mock


@pytest.fixture
def test_client(mock_session_manager, mock_output_monitor):
    """Create a FastAPI TestClient with mocked dependencies."""
    app = create_app(
        session_manager=mock_session_manager,
        notifier=None,
        output_monitor=mock_output_monitor,
        config={},
    )
    return TestClient(app)


@pytest.fixture
def running_session():
    """Create a RUNNING session."""
    return Session(
        id="run-155",
        name="running-session",
        working_dir="/tmp/test",
        tmux_session="claude-run-155",
        log_file="/tmp/test-run.log",
        status=SessionStatus.RUNNING,
    )


@pytest.fixture
def stopped_session():
    """Create a STOPPED session (killed)."""
    return Session(
        id="stop-155",
        name="stopped-session",
        working_dir="/tmp/test",
        tmux_session="claude-stop-155",
        log_file="/tmp/test-stop.log",
        status=SessionStatus.STOPPED,
    )


def _make_queue_manager_mock():
    """Create a mock MessageQueueManager."""
    mock = MagicMock()
    mock._restore_user_input_after_response = AsyncMock()
    return mock


# ---------------------------------------------------------------------------
# 1. Stop hook sets session.status to IDLE
# ---------------------------------------------------------------------------

class TestStopHookSetsSessionStatusIdle:

    def test_stop_hook_sets_session_status_idle(
        self, test_client, mock_session_manager, running_session
    ):
        """Fire Stop hook for a RUNNING session — session.status must become IDLE."""
        mock_session_manager.get_session.return_value = running_session
        mock_queue = _make_queue_manager_mock()
        mock_session_manager.message_queue_manager = mock_queue

        response = test_client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "run-155",
                "transcript_path": "/tmp/transcript.jsonl",
            },
        )
        assert response.status_code == 200

        # Verify update_session_status was called with IDLE
        mock_session_manager.update_session_status.assert_called_with(
            "run-155", SessionStatus.IDLE
        )

    def test_stop_hook_also_marks_delivery_state_idle(
        self, test_client, mock_session_manager, running_session
    ):
        """Stop hook must update both delivery_state.is_idle AND session.status."""
        mock_session_manager.get_session.return_value = running_session
        mock_queue = _make_queue_manager_mock()
        mock_session_manager.message_queue_manager = mock_queue

        test_client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "run-155",
                "transcript_path": "/tmp/transcript.jsonl",
            },
        )

        # Both should be called (last_output=None when transcript not readable)
        mock_queue.mark_session_idle.assert_called_with("run-155", last_output=None, from_stop_hook=True)
        mock_session_manager.update_session_status.assert_called_with(
            "run-155", SessionStatus.IDLE
        )


# ---------------------------------------------------------------------------
# 2. Stop hook skips STOPPED sessions
# ---------------------------------------------------------------------------

class TestStopHookSkipsStoppedSession:

    def test_stop_hook_skips_stopped_session(
        self, test_client, mock_session_manager, stopped_session
    ):
        """Late Stop hook after kill must NOT flip STOPPED → IDLE."""
        mock_session_manager.get_session.return_value = stopped_session
        mock_queue = _make_queue_manager_mock()
        mock_session_manager.message_queue_manager = mock_queue

        response = test_client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "stop-155",
                "transcript_path": "/tmp/transcript.jsonl",
            },
        )
        assert response.status_code == 200

        # update_session_status must NOT have been called
        mock_session_manager.update_session_status.assert_not_called()

    def test_stop_hook_still_marks_delivery_idle_for_stopped(
        self, test_client, mock_session_manager, stopped_session
    ):
        """Delivery state can be marked idle even for STOPPED sessions (harmless)."""
        mock_session_manager.get_session.return_value = stopped_session
        mock_queue = _make_queue_manager_mock()
        mock_session_manager.message_queue_manager = mock_queue

        test_client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "stop-155",
                "transcript_path": "/tmp/transcript.jsonl",
            },
        )

        # mark_session_idle is called regardless (delivery queue cleanup)
        mock_queue.mark_session_idle.assert_called_with("stop-155", last_output=None, from_stop_hook=True)


# ---------------------------------------------------------------------------
# 3. Crash recovery fires immediately after Stop hook
# ---------------------------------------------------------------------------

class TestCrashRecoveryAfterStopHook:

    @pytest.mark.asyncio
    async def test_crash_recovery_immediate_after_stop_hook(self):
        """
        After Stop hook sets session.status = IDLE, _handle_crash must
        trigger immediate recovery (not defer).
        """
        session = Session(
            id="crash-155",
            name="crash-session",
            working_dir="/tmp/test",
            tmux_session="claude-crash-155",
            log_file="/tmp/test-crash.log",
            status=SessionStatus.RUNNING,
            provider="claude",
        )

        monitor = OutputMonitor(poll_interval=0.1)
        monitor._crash_recovery_callback = AsyncMock(return_value=True)

        # Simulate Stop hook setting session to IDLE
        session.status = SessionStatus.IDLE

        # Now crash occurs — should recover immediately, not defer
        await monitor._handle_crash(session, "RangeError: Maximum call stack size exceeded")

        monitor._crash_recovery_callback.assert_awaited_once_with(session)
        assert session.id not in monitor._pending_crash_recovery

    @pytest.mark.asyncio
    async def test_crash_deferred_without_stop_hook_fix(self):
        """
        Without the fix (session still RUNNING), _handle_crash defers recovery.
        This test documents the pre-fix behavior for contrast.
        """
        session = Session(
            id="deferred-155",
            name="deferred-session",
            working_dir="/tmp/test",
            tmux_session="claude-deferred-155",
            log_file="/tmp/test-deferred.log",
            status=SessionStatus.RUNNING,
            provider="claude",
        )

        monitor = OutputMonitor(poll_interval=0.1)
        monitor._crash_recovery_callback = AsyncMock(return_value=True)

        # Session still RUNNING (Stop hook didn't update status) → deferred
        await monitor._handle_crash(session, "RangeError: Maximum call stack size exceeded")

        monitor._crash_recovery_callback.assert_not_awaited()
        assert session.id in monitor._pending_crash_recovery


# ---------------------------------------------------------------------------
# 4. Idempotent IDLE → IDLE transition
# ---------------------------------------------------------------------------

class TestIdleIdempotent:

    def test_stop_hook_on_already_idle_session(
        self, test_client, mock_session_manager
    ):
        """Stop hook on an already-IDLE session is a harmless no-op."""
        idle_session = Session(
            id="idle-155",
            name="idle-session",
            working_dir="/tmp/test",
            tmux_session="claude-idle-155",
            log_file="/tmp/test-idle.log",
            status=SessionStatus.IDLE,
        )
        mock_session_manager.get_session.return_value = idle_session
        mock_queue = _make_queue_manager_mock()
        mock_session_manager.message_queue_manager = mock_queue

        response = test_client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "idle-155",
                "transcript_path": "/tmp/transcript.jsonl",
            },
        )
        assert response.status_code == 200

        # Should still call update (IDLE → IDLE is harmless)
        mock_session_manager.update_session_status.assert_called_with(
            "idle-155", SessionStatus.IDLE
        )
