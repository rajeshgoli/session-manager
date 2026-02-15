"""
Regression tests for issue #151: Crash recovery blocked for RUNNING sessions

Tests verify that:
1. IDLE sessions recover immediately on crash detection
2. RUNNING sessions defer recovery until idle
3. Debounce prevents double-recovery from overlapping crash dumps
4. Permission-prompt state blocks deferred recovery flush
5. Successful immediate recovery clears stale pending state
6. Retry loop retries failed deferred recoveries
7. Non-Claude providers are excluded from crash recovery
8. Graceful recovery uses the correct callback parameter
"""

import pytest
import asyncio
from datetime import datetime, timedelta
from unittest.mock import AsyncMock, Mock

from src.output_monitor import (
    OutputMonitor,
    CRASH_DEBOUNCE_SUCCESS,
    CRASH_DEBOUNCE_FAILURE,
)
from src.models import Session, SessionStatus


@pytest.fixture
def idle_session():
    """Create an IDLE Claude session."""
    return Session(
        id="idle-001",
        name="idle-session",
        working_dir="/tmp/test",
        tmux_session="claude-idle-001",
        log_file="/tmp/test-idle.log",
        status=SessionStatus.IDLE,
        provider="claude",
    )


@pytest.fixture
def running_session():
    """Create a RUNNING Claude session."""
    return Session(
        id="run-001",
        name="running-session",
        working_dir="/tmp/test",
        tmux_session="claude-run-001",
        log_file="/tmp/test-run.log",
        status=SessionStatus.RUNNING,
        provider="claude",
    )


@pytest.fixture
def codex_session():
    """Create a Codex session (non-Claude provider)."""
    return Session(
        id="codex-001",
        name="codex-session",
        working_dir="/tmp/test",
        tmux_session="codex-codex-001",
        log_file="/tmp/test-codex.log",
        status=SessionStatus.IDLE,
        provider="codex",
    )


@pytest.fixture
def monitor():
    """Create an OutputMonitor with crash recovery callback."""
    mon = OutputMonitor(poll_interval=0.1)
    mon._crash_recovery_callback = AsyncMock(return_value=True)
    return mon


# --- 1. IDLE sessions recover immediately ---

class TestImmediateRecovery:
    @pytest.mark.asyncio
    async def test_idle_session_recovers_immediately(self, monitor, idle_session):
        """IDLE session triggers immediate recovery on crash detection."""
        await monitor._handle_crash(idle_session, "RangeError: Maximum call stack size exceeded")

        monitor._crash_recovery_callback.assert_awaited_once_with(idle_session)

    @pytest.mark.asyncio
    async def test_stopped_session_recovers_immediately(self, monitor):
        """STOPPED session triggers immediate recovery on crash detection."""
        session = Session(
            id="stop-001",
            name="stopped-session",
            working_dir="/tmp/test",
            tmux_session="claude-stop-001",
            log_file="/tmp/test-stop.log",
            status=SessionStatus.STOPPED,
            provider="claude",
        )
        await monitor._handle_crash(session, "RangeError: Maximum call stack size exceeded")

        monitor._crash_recovery_callback.assert_awaited_once_with(session)

    @pytest.mark.asyncio
    async def test_immediate_recovery_records_success(self, monitor, idle_session):
        """Successful immediate recovery records timestamp and success flag."""
        await monitor._handle_crash(idle_session, "crash")

        state = monitor._last_crash_recovery[idle_session.id]
        assert state[1] is True  # succeeded
        assert datetime.now() - state[0] < timedelta(seconds=2)

    @pytest.mark.asyncio
    async def test_immediate_recovery_records_failure(self, monitor, idle_session):
        """Failed immediate recovery records timestamp and failure flag."""
        monitor._crash_recovery_callback = AsyncMock(return_value=False)
        await monitor._handle_crash(idle_session, "crash")

        state = monitor._last_crash_recovery[idle_session.id]
        assert state[1] is False

    @pytest.mark.asyncio
    async def test_immediate_recovery_records_exception_as_failure(self, monitor, idle_session):
        """Exception during recovery records as failure."""
        monitor._crash_recovery_callback = AsyncMock(side_effect=RuntimeError("boom"))
        await monitor._handle_crash(idle_session, "crash")

        state = monitor._last_crash_recovery[idle_session.id]
        assert state[1] is False


# --- 2. RUNNING sessions defer recovery ---

class TestDeferredRecovery:
    @pytest.mark.asyncio
    async def test_running_session_defers_recovery(self, monitor, running_session):
        """RUNNING session does not recover immediately; adds to pending set."""
        await monitor._handle_crash(running_session, "crash")

        monitor._crash_recovery_callback.assert_not_awaited()
        assert running_session.id in monitor._pending_crash_recovery

    @pytest.mark.asyncio
    async def test_deferred_recovery_flushes_on_idle(self, monitor, running_session):
        """Pending recovery triggers when session transitions to IDLE."""
        # Defer
        await monitor._handle_crash(running_session, "crash")
        assert running_session.id in monitor._pending_crash_recovery

        # Transition to IDLE
        running_session.status = SessionStatus.IDLE
        await monitor._flush_pending_crash_recovery(running_session)

        monitor._crash_recovery_callback.assert_awaited_once_with(running_session, graceful=True)
        assert running_session.id not in monitor._pending_crash_recovery

    @pytest.mark.asyncio
    async def test_flush_noop_when_no_pending(self, monitor, idle_session):
        """Flush does nothing when session has no pending recovery."""
        await monitor._flush_pending_crash_recovery(idle_session)

        monitor._crash_recovery_callback.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_flush_noop_when_still_running(self, monitor, running_session):
        """Flush does nothing when session is still RUNNING."""
        monitor._pending_crash_recovery.add(running_session.id)

        await monitor._flush_pending_crash_recovery(running_session)

        monitor._crash_recovery_callback.assert_not_awaited()
        assert running_session.id in monitor._pending_crash_recovery

    @pytest.mark.asyncio
    async def test_flush_failure_keeps_pending(self, monitor, running_session):
        """Failed flush keeps session in pending set for retry."""
        monitor._crash_recovery_callback = AsyncMock(return_value=False)
        monitor._pending_crash_recovery.add(running_session.id)

        running_session.status = SessionStatus.IDLE
        await monitor._flush_pending_crash_recovery(running_session)

        assert running_session.id in monitor._pending_crash_recovery

    @pytest.mark.asyncio
    async def test_flush_called_from_handle_completion(self, monitor, running_session):
        """Completion handler triggers flush for pending sessions."""
        monitor._pending_crash_recovery.add(running_session.id)
        monitor._status_callback = AsyncMock()

        running_session.status = SessionStatus.IDLE
        await monitor._handle_completion(running_session, "Task complete")

        monitor._crash_recovery_callback.assert_awaited_once_with(running_session, graceful=True)

    @pytest.mark.asyncio
    async def test_flush_called_from_check_idle(self, monitor, running_session):
        """Idle timeout handler triggers flush for pending sessions."""
        monitor._pending_crash_recovery.add(running_session.id)
        monitor._status_callback = AsyncMock()
        # Set up for idle timeout to fire
        monitor._last_activity[running_session.id] = datetime.now() - timedelta(seconds=600)

        running_session.status = SessionStatus.IDLE
        await monitor._check_idle(running_session)

        monitor._crash_recovery_callback.assert_awaited_once_with(running_session, graceful=True)


# --- 3. Debounce ---

class TestDebounce:
    @pytest.mark.asyncio
    async def test_debounce_after_success(self, monitor, idle_session):
        """Second crash within success cooldown is suppressed."""
        await monitor._handle_crash(idle_session, "crash")
        assert monitor._crash_recovery_callback.await_count == 1

        # Second crash within 30s - should be debounced
        await monitor._handle_crash(idle_session, "crash")
        assert monitor._crash_recovery_callback.await_count == 1

    @pytest.mark.asyncio
    async def test_debounce_after_failure_shorter(self, monitor, idle_session):
        """Failure cooldown (5s) is shorter than success cooldown (30s)."""
        monitor._crash_recovery_callback = AsyncMock(return_value=False)
        await monitor._handle_crash(idle_session, "crash")

        # Simulate time passing past failure cooldown but within success cooldown
        monitor._last_crash_recovery[idle_session.id] = (
            datetime.now() - CRASH_DEBOUNCE_FAILURE - timedelta(seconds=1),
            False,
        )

        # Should not be debounced (past 5s failure cooldown)
        monitor._crash_recovery_callback = AsyncMock(return_value=True)
        await monitor._handle_crash(idle_session, "crash")
        assert monitor._crash_recovery_callback.await_count == 1

    @pytest.mark.asyncio
    async def test_running_dedup(self, monitor, running_session):
        """Multiple crash detections while RUNNING only add to pending once."""
        await monitor._handle_crash(running_session, "crash")
        await monitor._handle_crash(running_session, "crash")

        assert running_session.id in monitor._pending_crash_recovery
        monitor._crash_recovery_callback.assert_not_awaited()


# --- 4. Permission-prompt gating ---

class TestPermissionGating:
    @pytest.mark.asyncio
    async def test_flush_blocked_while_awaiting_permission(self, monitor, running_session):
        """Deferred recovery does not flush while at a permission prompt."""
        monitor._pending_crash_recovery.add(running_session.id)
        monitor._awaiting_permission[running_session.id] = True

        running_session.status = SessionStatus.IDLE
        await monitor._flush_pending_crash_recovery(running_session)

        monitor._crash_recovery_callback.assert_not_awaited()
        assert running_session.id in monitor._pending_crash_recovery

    @pytest.mark.asyncio
    async def test_permission_state_set_before_debounce(self, monitor, idle_session):
        """Permission state is set even when notification is debounced."""
        # Set up debounce: pretend we just notified
        monitor._notified_permissions[idle_session.id] = datetime.now()
        monitor._permission_debounce = 30

        await monitor._handle_permission_prompt(idle_session, "Allow once? [Y/n]")

        # State should be set even though notification was debounced
        assert monitor._awaiting_permission.get(idle_session.id) is True

    @pytest.mark.asyncio
    async def test_permission_state_cleared_on_new_content(self, monitor, idle_session):
        """New content clears permission-awaiting state."""
        monitor._awaiting_permission[idle_session.id] = True

        await monitor._analyze_content(idle_session, "some new output")

        assert idle_session.id not in monitor._awaiting_permission


# --- 5. Immediate recovery clears stale pending state ---

class TestPendingCleanup:
    @pytest.mark.asyncio
    async def test_immediate_recovery_clears_pending(self, monitor, idle_session):
        """Successful immediate recovery clears any stale pending state."""
        # Simulate stale pending from earlier deferred attempt
        monitor._pending_crash_recovery.add(idle_session.id)

        await monitor._handle_crash(idle_session, "crash")

        assert idle_session.id not in monitor._pending_crash_recovery

    @pytest.mark.asyncio
    async def test_failed_immediate_recovery_keeps_pending(self, monitor, idle_session):
        """Failed immediate recovery does not clear pending state."""
        monitor._pending_crash_recovery.add(idle_session.id)
        monitor._crash_recovery_callback = AsyncMock(return_value=False)

        await monitor._handle_crash(idle_session, "crash")

        assert idle_session.id in monitor._pending_crash_recovery


# --- 6. Retry loop ---

class TestRetryLoop:
    @pytest.mark.asyncio
    async def test_retry_fires_after_failure_cooldown(self, monitor, running_session):
        """Monitor loop retries deferred recovery after failure cooldown."""
        monitor._pending_crash_recovery.add(running_session.id)
        # Simulate past failure
        monitor._last_crash_recovery[running_session.id] = (
            datetime.now() - CRASH_DEBOUNCE_FAILURE - timedelta(seconds=1),
            False,
        )
        running_session.status = SessionStatus.IDLE

        await monitor._flush_pending_crash_recovery(running_session)

        monitor._crash_recovery_callback.assert_awaited_once_with(running_session, graceful=True)

    @pytest.mark.asyncio
    async def test_retry_skipped_within_cooldown(self, monitor, running_session):
        """Retry does not fire within failure cooldown."""
        monitor._pending_crash_recovery.add(running_session.id)
        # Fresh failure
        monitor._last_crash_recovery[running_session.id] = (datetime.now(), False)
        running_session.status = SessionStatus.IDLE

        # Simulate the retry check from _monitor_loop
        recovery_state = monitor._last_crash_recovery.get(running_session.id)
        if recovery_state:
            last_time, last_succeeded = recovery_state
            if not last_succeeded and datetime.now() - last_time > CRASH_DEBOUNCE_FAILURE:
                await monitor._flush_pending_crash_recovery(running_session)

        monitor._crash_recovery_callback.assert_not_awaited()


# --- 7. Provider gate ---

class TestProviderGate:
    @pytest.mark.asyncio
    async def test_non_claude_provider_skipped(self, monitor, codex_session):
        """Codex sessions are not eligible for crash recovery."""
        await monitor._handle_crash(codex_session, "RangeError: Maximum call stack size exceeded")

        monitor._crash_recovery_callback.assert_not_awaited()
        assert codex_session.id not in monitor._pending_crash_recovery

    @pytest.mark.asyncio
    async def test_claude_provider_eligible(self, monitor, idle_session):
        """Claude sessions are eligible for crash recovery."""
        await monitor._handle_crash(idle_session, "crash")

        monitor._crash_recovery_callback.assert_awaited_once()


# --- 8. Graceful parameter ---

class TestGracefulParameter:
    @pytest.mark.asyncio
    async def test_immediate_recovery_not_graceful(self, monitor, idle_session):
        """Immediate recovery does not pass graceful=True."""
        await monitor._handle_crash(idle_session, "crash")

        monitor._crash_recovery_callback.assert_awaited_once_with(idle_session)

    @pytest.mark.asyncio
    async def test_deferred_recovery_is_graceful(self, monitor, running_session):
        """Deferred recovery passes graceful=True."""
        monitor._pending_crash_recovery.add(running_session.id)

        running_session.status = SessionStatus.IDLE
        await monitor._flush_pending_crash_recovery(running_session)

        monitor._crash_recovery_callback.assert_awaited_once_with(running_session, graceful=True)


# --- 9. Cleanup ---

class TestCleanup:
    @pytest.mark.asyncio
    async def test_stop_monitoring_cleans_up_crash_state(self, monitor, idle_session):
        """stop_monitoring removes all crash recovery state."""
        monitor._pending_crash_recovery.add(idle_session.id)
        monitor._last_crash_recovery[idle_session.id] = (datetime.now(), True)
        monitor._awaiting_permission[idle_session.id] = True
        monitor._tasks[idle_session.id] = asyncio.create_task(asyncio.sleep(999))

        await monitor.stop_monitoring(idle_session.id)

        assert idle_session.id not in monitor._pending_crash_recovery
        assert idle_session.id not in monitor._last_crash_recovery
        assert idle_session.id not in monitor._awaiting_permission
