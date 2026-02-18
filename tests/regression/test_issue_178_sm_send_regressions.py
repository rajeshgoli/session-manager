"""
Regression tests for issue #178: Enter never sent + delivery race (regressions from #175).

Regression 1 — Atomic text+\\r bypasses paste detection settle delay:
  PR #176 replaced two-call approach (text, sleep, Enter) with a single call containing
  text + "\\r". Claude Code (Node.js TUI in raw mode) treats the rapid burst as pasted text;
  \\r at the end is treated as a literal byte, not submit. Fix: restore two-call approach.

Regression 2 — 3-second prompt polling creates Stop hook race window:
  _deliver_urgent didn't acquire the per-session delivery lock, allowing _try_deliver_messages
  (triggered by a Stop hook firing during prompt polling) to deliver sequential messages
  before the urgent one. Fix: add delivery lock to _deliver_urgent.

Tests verify:
- send_input_async uses two separate tmux send-keys calls with settle delay between them
- send_input_async settle delay is positioned between text call and Enter call
- send_input_async returns False when EITHER the text or Enter call fails
- cmd_clear uses two-call approach for clear command and for new_prompt
- _deliver_urgent acquires the per-session delivery lock
- urgent and sequential delivery cannot overlap for the same session
"""

import asyncio
import pytest
from unittest.mock import AsyncMock, Mock, patch, call

from src.models import Session, SessionStatus, QueuedMessage
from src.message_queue import MessageQueueManager
from src.tmux_controller import TmuxController
from src.cli.commands import cmd_clear
from src.cli.client import SessionManagerClient


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def mock_session_manager():
    manager = Mock()
    manager.get_session = Mock()
    manager.tmux = Mock()
    manager.tmux.send_input_async = AsyncMock(return_value=True)
    manager._save_state = Mock()
    manager._deliver_direct = AsyncMock(return_value=True)
    return manager


@pytest.fixture
def temp_db(tmp_path):
    return str(tmp_path / "test_queue_178.db")


@pytest.fixture
def message_queue(mock_session_manager, temp_db):
    config = {
        "sm_send": {"urgent_delay_ms": 100},
        "timeouts": {"message_queue": {"subprocess_timeout_seconds": 1}},
    }
    return MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=temp_db,
        config=config,
    )


@pytest.fixture
def tmux_controller():
    config = {
        "timeouts": {
            "tmux": {
                "send_keys_timeout_seconds": 2,
                "send_keys_settle_seconds": 0.3,
            }
        }
    }
    return TmuxController(log_dir="/tmp/test-sessions-178", config=config)


@pytest.fixture
def mock_client():
    client = Mock(spec=SessionManagerClient)
    client.invalidate_cache = Mock(return_value=(True, False))
    return client


# ---------------------------------------------------------------------------
# Regression 1: send_input_async — two-call approach
# ---------------------------------------------------------------------------

class TestRegression1_TwoCallSendInput:
    """Verify send_input_async uses two separate send-keys calls with a settle delay."""

    @pytest.mark.asyncio
    async def test_two_calls_text_then_enter(self, tmux_controller):
        """send_input_async makes exactly TWO subprocess calls: text then Enter."""
        calls_made = []

        async def mock_subprocess(*args, **kwargs):
            calls_made.append(list(args))
            proc = AsyncMock()
            proc.communicate = AsyncMock(return_value=(b"", b""))
            proc.returncode = 0
            return proc

        with patch.object(tmux_controller, "session_exists", return_value=True), \
             patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess), \
             patch("asyncio.sleep", new_callable=AsyncMock):
            result = await tmux_controller.send_input_async("claude-session", "test payload")

        assert result is True
        assert len(calls_made) == 2, f"Expected 2 subprocess calls, got {len(calls_made)}"

        # First call: text
        assert calls_made[0][:6] == ["tmux", "send-keys", "-t", "claude-session", "--", "test payload"]
        assert "\r" not in calls_made[0][5]

        # Second call: Enter
        assert calls_made[1][:5] == ["tmux", "send-keys", "-t", "claude-session", "Enter"]

    @pytest.mark.asyncio
    async def test_settle_delay_between_text_and_enter(self, tmux_controller):
        """asyncio.sleep(send_keys_settle_seconds) is called between text and Enter."""
        event_log = []

        async def mock_subprocess(*args, **kwargs):
            # Track which send-keys call this is by the key argument position
            key = args[4] if len(args) > 4 else "?"
            event_log.append(f"send-keys:{key}")
            proc = AsyncMock()
            proc.communicate = AsyncMock(return_value=(b"", b""))
            proc.returncode = 0
            return proc

        async def mock_sleep(secs):
            event_log.append(f"sleep:{secs}")

        with patch.object(tmux_controller, "session_exists", return_value=True), \
             patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess), \
             patch("asyncio.sleep", side_effect=mock_sleep):
            await tmux_controller.send_input_async("claude-session", "hello")

        # Expect: send-keys:-- → sleep:0.3 → send-keys:Enter
        assert event_log[0] == "send-keys:--"
        assert event_log[1] == f"sleep:{tmux_controller.send_keys_settle_seconds}"
        assert event_log[2] == "send-keys:Enter"

    @pytest.mark.asyncio
    async def test_returns_false_when_text_call_fails(self, tmux_controller):
        """Returns False immediately when the text send-keys call returns non-zero."""
        call_count = 0

        async def mock_subprocess(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            proc = AsyncMock()
            proc.communicate = AsyncMock(return_value=(b"", b"error"))
            proc.returncode = 1  # Fail on first call
            return proc

        with patch.object(tmux_controller, "session_exists", return_value=True), \
             patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess), \
             patch("asyncio.sleep", new_callable=AsyncMock):
            result = await tmux_controller.send_input_async("claude-session", "msg")

        assert result is False
        assert call_count == 1, "Should stop after first failing call"

    @pytest.mark.asyncio
    async def test_returns_false_when_enter_call_fails(self, tmux_controller):
        """Returns False when the Enter send-keys call returns non-zero."""
        call_count = 0

        async def mock_subprocess(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            proc = AsyncMock()
            if call_count == 1:
                proc.communicate = AsyncMock(return_value=(b"", b""))
                proc.returncode = 0
            else:
                proc.communicate = AsyncMock(return_value=(b"", b"enter error"))
                proc.returncode = 1
            return proc

        with patch.object(tmux_controller, "session_exists", return_value=True), \
             patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess), \
             patch("asyncio.sleep", new_callable=AsyncMock):
            result = await tmux_controller.send_input_async("claude-session", "msg")

        assert result is False
        assert call_count == 2

    @pytest.mark.asyncio
    async def test_no_atomic_carriage_return_in_source(self, tmux_controller):
        """The broken text+\\r atomic pattern is not present in send_input_async."""
        import inspect
        source = inspect.getsource(tmux_controller.send_input_async)
        assert 'text + "\\r"' not in source, "Atomic \\r approach should not be used"
        assert '"\\r"' not in source, "\\r should not appear in send_input_async"


# ---------------------------------------------------------------------------
# Regression 1: cmd_clear — two-call approach
# ---------------------------------------------------------------------------

class TestRegression1_CmdClearTwoCall:
    """Verify cmd_clear uses two separate send-keys calls for /clear and new_prompt."""

    @pytest.fixture
    def clear_session(self):
        return {
            "id": "clear-test",
            "name": "test-session",
            "tmux_session": "claude-clear-test",
            "parent_session_id": "parent-178",
            "completion_status": None,
            "friendly_name": "clear-child",
            "provider": "claude",
        }

    def test_clear_uses_two_calls_for_command(self, mock_client, clear_session):
        """cmd_clear sends /clear text then a separate Enter (not /clear\\r atomic)."""
        mock_client.get_session.return_value = clear_session
        mock_client.list_sessions.return_value = [clear_session]

        with patch("subprocess.run") as mock_run, \
             patch("src.cli.commands._wait_for_claude_prompt", return_value=True), \
             patch("time.sleep"):
            mock_run.return_value = Mock(returncode=0, stdout="", stderr="")
            result = cmd_clear(
                client=mock_client,
                requester_session_id="parent-178",
                target_identifier="clear-test",
                new_prompt=None,
            )

        assert result == 0
        run_calls = mock_run.call_args_list
        # Find /clear text call
        text_call = next(
            (c for c in run_calls if c[0][0] == ["tmux", "send-keys", "-t", "claude-clear-test", "--", "/clear"]),
            None,
        )
        enter_call = next(
            (c for c in run_calls if c[0][0] == ["tmux", "send-keys", "-t", "claude-clear-test", "Enter"]),
            None,
        )
        assert text_call is not None, "/clear text call not found"
        assert enter_call is not None, "Enter call not found"
        # Ensure no atomic /clear\r call
        atomic_call = next(
            (c for c in run_calls if c[0][0][4:] == ["--", "/clear\r"]),
            None,
        )
        assert atomic_call is None, "Atomic /clear\\r call should not be used"

    def test_clear_uses_two_calls_for_new_prompt(self, mock_client, clear_session):
        """cmd_clear sends new_prompt text then a separate Enter (not new_prompt\\r)."""
        mock_client.get_session.return_value = clear_session
        mock_client.list_sessions.return_value = [clear_session]

        with patch("subprocess.run") as mock_run, \
             patch("src.cli.commands._wait_for_claude_prompt", return_value=True), \
             patch("time.sleep"):
            mock_run.return_value = Mock(returncode=0, stdout="", stderr="")
            result = cmd_clear(
                client=mock_client,
                requester_session_id="parent-178",
                target_identifier="clear-test",
                new_prompt="do the next task",
            )

        assert result == 0
        run_calls = mock_run.call_args_list
        prompt_text_call = next(
            (c for c in run_calls
             if c[0][0] == ["tmux", "send-keys", "-t", "claude-clear-test", "--", "do the next task"]),
            None,
        )
        assert prompt_text_call is not None, "new_prompt text call not found"
        # Ensure no atomic new_prompt\r call
        atomic_prompt_call = next(
            (c for c in run_calls if c[0][0][4:] == ["--", "do the next task\r"]),
            None,
        )
        assert atomic_prompt_call is None, "Atomic new_prompt\\r call should not be used"

    def test_clear_settle_delay_called_after_clear_text(self, mock_client, clear_session):
        """time.sleep is called after the /clear text send-keys call."""
        mock_client.get_session.return_value = clear_session
        mock_client.list_sessions.return_value = [clear_session]

        sleep_calls = []

        with patch("subprocess.run") as mock_run, \
             patch("src.cli.commands._wait_for_claude_prompt", return_value=True), \
             patch("time.sleep", side_effect=lambda s: sleep_calls.append(s)):
            mock_run.return_value = Mock(returncode=0, stdout="", stderr="")
            result = cmd_clear(
                client=mock_client,
                requester_session_id="parent-178",
                target_identifier="clear-test",
                new_prompt=None,
            )

        assert result == 0
        assert len(sleep_calls) >= 1
        from src.cli.commands import _SEND_KEYS_SETTLE_SECONDS
        assert sleep_calls[0] == _SEND_KEYS_SETTLE_SECONDS, (
            f"Expected settle delay {_SEND_KEYS_SETTLE_SECONDS}s, got {sleep_calls[0]}"
        )


# ---------------------------------------------------------------------------
# Regression 2: _deliver_urgent acquires delivery lock
# ---------------------------------------------------------------------------

class TestRegression2_UrgentDeliveryLock:
    """Verify _deliver_urgent acquires the per-session delivery lock."""

    @pytest.mark.asyncio
    async def test_deliver_urgent_acquires_lock(self, message_queue, mock_session_manager):
        """_deliver_urgent must acquire the per-session delivery lock."""
        session = Session(
            id="urgent-lock",
            name="test-session",
            working_dir="/tmp/test",
            tmux_session="claude-urgent-lock",
            completion_status=None,
        )
        mock_session_manager.get_session.return_value = session

        msg = QueuedMessage(
            id="msg-urgent-lock",
            target_session_id="urgent-lock",
            text="urgent message",
            delivery_mode="urgent",
        )

        lock_acquired_count = []
        real_lock = asyncio.Lock()
        real_acquire = real_lock.acquire

        async def tracking_acquire():
            lock_acquired_count.append(True)
            return await real_acquire()

        real_lock.acquire = tracking_acquire
        message_queue._delivery_locks["urgent-lock"] = real_lock

        async def mock_subprocess(*args, **kwargs):
            proc = AsyncMock()
            proc.communicate = AsyncMock(return_value=(b"", b""))
            proc.returncode = 0
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            message_queue._wait_for_claude_prompt_async = AsyncMock(return_value=True)
            await message_queue._deliver_urgent("urgent-lock", msg)

        assert len(lock_acquired_count) == 1, "Lock must be acquired exactly once"
        assert not real_lock.locked(), "Lock must be released after delivery"

    @pytest.mark.asyncio
    async def test_urgent_and_sequential_cannot_overlap(
        self, message_queue, mock_session_manager
    ):
        """_deliver_urgent and _try_deliver_messages cannot run concurrently for the same session."""
        session = Session(
            id="overlap-test",
            name="test-session",
            working_dir="/tmp/test",
            tmux_session="claude-overlap",
            completion_status=None,
        )
        mock_session_manager.get_session.return_value = session

        active_deliveries = []
        overlaps_detected = []

        async def slow_deliver_direct(*args, **kwargs):
            active_deliveries.append("running")
            if len(active_deliveries) > 1:
                overlaps_detected.append(True)
            await asyncio.sleep(0.05)
            active_deliveries.remove("running")
            return True

        mock_session_manager._deliver_direct = AsyncMock(side_effect=slow_deliver_direct)

        msg = QueuedMessage(
            id="msg-overlap",
            target_session_id="overlap-test",
            text="urgent",
            delivery_mode="urgent",
        )

        # Queue a sequential message so _try_deliver_messages has work
        message_queue.queue_message(
            target_session_id="overlap-test",
            text="sequential",
            delivery_mode="sequential",
        )
        state = message_queue._get_or_create_state("overlap-test")
        state.is_idle = True

        async def mock_subprocess(*args, **kwargs):
            proc = AsyncMock()
            proc.communicate = AsyncMock(return_value=(b"", b""))
            proc.returncode = 0
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            message_queue._wait_for_claude_prompt_async = AsyncMock(return_value=True)
            urgent_task = asyncio.create_task(
                message_queue._deliver_urgent("overlap-test", msg)
            )
            await asyncio.sleep(0.01)
            # Try sequential delivery while urgent is in progress
            try_task = asyncio.create_task(
                message_queue._try_deliver_messages("overlap-test")
            )
            await asyncio.gather(urgent_task, try_task)

        assert len(overlaps_detected) == 0, (
            "Concurrent delivery detected — lock not preventing overlap"
        )

    @pytest.mark.asyncio
    async def test_urgent_delivery_lock_not_held_after_success(
        self, message_queue, mock_session_manager
    ):
        """Lock is released after successful urgent delivery (no lock leak)."""
        session = Session(
            id="lock-release",
            name="test-session",
            working_dir="/tmp/test",
            tmux_session="claude-lock-release",
            completion_status=None,
        )
        mock_session_manager.get_session.return_value = session

        msg = QueuedMessage(
            id="msg-lock-release",
            target_session_id="lock-release",
            text="urgent",
            delivery_mode="urgent",
        )

        async def mock_subprocess(*args, **kwargs):
            proc = AsyncMock()
            proc.communicate = AsyncMock(return_value=(b"", b""))
            proc.returncode = 0
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            message_queue._wait_for_claude_prompt_async = AsyncMock(return_value=True)
            await message_queue._deliver_urgent("lock-release", msg)

        lock = message_queue._delivery_locks.get("lock-release")
        assert lock is not None
        assert not lock.locked(), "Lock must not be held after delivery completes"

    @pytest.mark.asyncio
    async def test_codex_app_delivery_bypasses_lock(
        self, message_queue, mock_session_manager
    ):
        """codex-app sessions exit before acquiring the lock (not relevant for them)."""
        session = Session(
            id="codex-lock",
            name="codex-session",
            working_dir="/tmp/test",
            tmux_session=None,
            completion_status=None,
        )
        # Simulate codex-app provider
        session.provider = "codex-app"
        mock_session_manager.get_session.return_value = session

        # codex-app uses _deliver_urgent on the session_manager, not the tmux path
        mock_session_manager._deliver_urgent = AsyncMock(return_value=True)

        msg = QueuedMessage(
            id="msg-codex-lock",
            target_session_id="codex-lock",
            text="urgent",
            delivery_mode="urgent",
        )

        await message_queue._deliver_urgent("codex-lock", msg)

        # codex-app path returns before acquiring lock — lock should not exist
        lock = message_queue._delivery_locks.get("codex-lock")
        # Either no lock was created, or it's not held
        if lock is not None:
            assert not lock.locked()
