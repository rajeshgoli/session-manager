"""
Regression tests for issue #175: sm send truncates first characters / missing Enter.

Bug A: Race between Escape delivery and next tmux send-keys.
Bug B: Separate text and Enter subprocess calls with no atomic guarantee.

Tests verify:
- Bug A: _wait_for_claude_prompt_async is called before _deliver_direct in urgent delivery
- Bug A: Prompt polling correctly detects bare '>' prompt
- Bug B: send_input_async sends text+Enter atomically in a single send-keys call
- Bug B: send_input_async returns False and logs error when unified call fails
"""

import pytest
import asyncio
from unittest.mock import Mock, AsyncMock, patch, MagicMock, call

from src.models import Session, SessionStatus, QueuedMessage
from src.message_queue import MessageQueueManager
from src.tmux_controller import TmuxController


# --- Fixtures ---


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
        "sm_send": {
            "urgent_delay_ms": 100,
        },
        "timeouts": {
            "message_queue": {
                "subprocess_timeout_seconds": 1,
            }
        },
    }
    queue_mgr = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=temp_db,
        config=config,
    )
    return queue_mgr


@pytest.fixture
def tmux_controller():
    """Create a TmuxController with short timeouts for testing."""
    config = {
        "timeouts": {
            "tmux": {
                "send_keys_timeout_seconds": 2,
                "send_keys_settle_seconds": 0.01,
            }
        }
    }
    return TmuxController(log_dir="/tmp/test-sessions", config=config)


# --- Bug A Tests ---


class TestBugA_PromptDetectionBeforeDelivery:
    """Verify _wait_for_claude_prompt_async is called before _deliver_direct."""

    @pytest.mark.asyncio
    async def test_urgent_delivery_waits_for_prompt_before_deliver(
        self, message_queue, mock_session_manager
    ):
        """_wait_for_claude_prompt_async must be awaited before _deliver_direct."""
        session = Session(
            id="test-175a",
            name="test-session",
            working_dir="/tmp/test",
            tmux_session="claude-test-175a",
            completion_status=None,
        )
        mock_session_manager.get_session.return_value = session

        msg = QueuedMessage(
            id="msg-175a",
            target_session_id="test-175a",
            text="test message",
            delivery_mode="urgent",
        )

        call_order = []

        # Track call order: Escape send-keys, then prompt wait, then deliver
        original_wait = message_queue._wait_for_claude_prompt_async

        async def mock_wait(*args, **kwargs):
            call_order.append("wait_for_prompt")
            return True

        async def mock_deliver(*args, **kwargs):
            call_order.append("deliver_direct")
            return True

        async def mock_subprocess(*args, **kwargs):
            call_order.append("escape_send")
            proc = AsyncMock()
            proc.communicate = AsyncMock(return_value=(b"", b""))
            proc.returncode = 0
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            message_queue._wait_for_claude_prompt_async = AsyncMock(side_effect=mock_wait)
            mock_session_manager._deliver_direct = AsyncMock(side_effect=mock_deliver)

            await message_queue._deliver_urgent("test-175a", msg)

        # Verify order: escape first, then prompt wait, then deliver
        assert "escape_send" in call_order
        assert "wait_for_prompt" in call_order
        assert "deliver_direct" in call_order
        assert call_order.index("escape_send") < call_order.index("wait_for_prompt")
        assert call_order.index("wait_for_prompt") < call_order.index("deliver_direct")

    @pytest.mark.asyncio
    async def test_urgent_delivery_no_longer_uses_sleep_delay(
        self, message_queue, mock_session_manager
    ):
        """Urgent delivery must NOT use asyncio.sleep for the post-Escape delay."""
        session = Session(
            id="test-175a2",
            name="test-session",
            working_dir="/tmp/test",
            tmux_session="claude-test-175a2",
            completion_status=None,
        )
        mock_session_manager.get_session.return_value = session

        msg = QueuedMessage(
            id="msg-175a2",
            target_session_id="test-175a2",
            text="test message",
            delivery_mode="urgent",
        )

        sleep_calls = []

        async def tracking_sleep(seconds):
            sleep_calls.append(seconds)
            # Don't actually sleep

        async def mock_subprocess(*args, **kwargs):
            proc = AsyncMock()
            proc.communicate = AsyncMock(return_value=(b"", b""))
            proc.returncode = 0
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess), \
             patch("asyncio.sleep", side_effect=tracking_sleep):
            message_queue._wait_for_claude_prompt_async = AsyncMock(return_value=True)

            await message_queue._deliver_urgent("test-175a2", msg)

        # The old code did asyncio.sleep(self.urgent_delay_ms / 1000) = 0.1s
        # That should no longer happen (replaced by prompt wait)
        assert 0.1 not in sleep_calls, (
            "Urgent delivery still uses asyncio.sleep for post-Escape delay"
        )


class TestBugA_PromptPolling:
    """Verify _wait_for_claude_prompt_async correctly detects prompt state."""

    @pytest.mark.asyncio
    async def test_returns_true_when_prompt_detected(self, message_queue):
        """Polling returns True when capture-pane output ends with '>'."""
        async def mock_subprocess(*args, **kwargs):
            proc = AsyncMock()
            proc.communicate = AsyncMock(
                return_value=(b"Some output\n>", b"")
            )
            proc.returncode = 0
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            result = await message_queue._wait_for_claude_prompt_async(
                "claude-test", timeout=1.0, poll_interval=0.05
            )

        assert result is True

    @pytest.mark.asyncio
    async def test_returns_true_with_trailing_whitespace(self, message_queue):
        """Prompt with trailing spaces is still detected (rstrip)."""
        async def mock_subprocess(*args, **kwargs):
            proc = AsyncMock()
            proc.communicate = AsyncMock(
                return_value=(b"Some output\n>   ", b"")
            )
            proc.returncode = 0
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            result = await message_queue._wait_for_claude_prompt_async(
                "claude-test", timeout=1.0, poll_interval=0.05
            )

        assert result is True

    @pytest.mark.asyncio
    async def test_returns_false_when_prompt_has_user_text(self, message_queue):
        """'> some text' is NOT an idle prompt — should not match."""
        call_count = 0

        async def mock_subprocess(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            proc = AsyncMock()
            proc.communicate = AsyncMock(
                return_value=(b"Some output\n> partial input", b"")
            )
            proc.returncode = 0
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            result = await message_queue._wait_for_claude_prompt_async(
                "claude-test", timeout=0.3, poll_interval=0.05
            )

        assert result is False
        assert call_count > 1  # Polled multiple times before timeout

    @pytest.mark.asyncio
    async def test_returns_false_on_timeout(self, message_queue):
        """Returns False when prompt never appears within timeout."""
        async def mock_subprocess(*args, **kwargs):
            proc = AsyncMock()
            proc.communicate = AsyncMock(
                return_value=(b"Claude is streaming...\nSome output", b"")
            )
            proc.returncode = 0
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            result = await message_queue._wait_for_claude_prompt_async(
                "claude-test", timeout=0.2, poll_interval=0.05
            )

        assert result is False

    @pytest.mark.asyncio
    async def test_prompt_detected_after_initial_non_idle(self, message_queue):
        """Prompt appears after a few polls (simulates Claude finishing response)."""
        call_count = 0

        async def mock_subprocess(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            proc = AsyncMock()
            if call_count < 3:
                # Still streaming
                proc.communicate = AsyncMock(
                    return_value=(b"Still working...", b"")
                )
            else:
                # Done, prompt visible
                proc.communicate = AsyncMock(
                    return_value=(b"Done.\n>", b"")
                )
            proc.returncode = 0
            return proc

        with patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            result = await message_queue._wait_for_claude_prompt_async(
                "claude-test", timeout=2.0, poll_interval=0.05
            )

        assert result is True
        assert call_count >= 3


# --- Bug B Tests ---


class TestBugB_AtomicSendInput:
    """Verify send_input_async sends text+Enter in a single tmux send-keys call."""

    @pytest.mark.asyncio
    async def test_single_subprocess_call_with_carriage_return(self, tmux_controller):
        """send_input_async makes exactly one subprocess call with text + \\r."""
        subprocess_calls = []

        async def mock_subprocess(*args, **kwargs):
            subprocess_calls.append(args)
            proc = AsyncMock()
            proc.communicate = AsyncMock(return_value=(b"", b""))
            proc.returncode = 0
            return proc

        with patch.object(tmux_controller, "session_exists", return_value=True), \
             patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            result = await tmux_controller.send_input_async("claude-test", "hello world")

        assert result is True
        # Exactly one subprocess call (no separate Enter call)
        assert len(subprocess_calls) == 1

        call_args = subprocess_calls[0]
        assert call_args[0] == "tmux"
        assert call_args[1] == "send-keys"
        assert call_args[2] == "-t"
        assert call_args[3] == "claude-test"
        assert call_args[4] == "--"
        assert call_args[5] == "hello world\r"

    @pytest.mark.asyncio
    async def test_returns_false_on_nonzero_returncode(self, tmux_controller):
        """send_input_async returns False when the unified send-keys call fails."""
        async def mock_subprocess(*args, **kwargs):
            proc = AsyncMock()
            proc.communicate = AsyncMock(return_value=(b"", b"tmux error"))
            proc.returncode = 1
            return proc

        with patch.object(tmux_controller, "session_exists", return_value=True), \
             patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            result = await tmux_controller.send_input_async("claude-test", "test message")

        assert result is False

    @pytest.mark.asyncio
    async def test_returns_false_on_timeout(self, tmux_controller):
        """send_input_async returns False on subprocess timeout."""
        async def mock_subprocess(*args, **kwargs):
            proc = AsyncMock()
            proc.communicate = AsyncMock(side_effect=asyncio.TimeoutError())
            proc.returncode = None
            return proc

        with patch.object(tmux_controller, "session_exists", return_value=True), \
             patch("asyncio.create_subprocess_exec", side_effect=mock_subprocess):
            result = await tmux_controller.send_input_async("claude-test", "test message")

        assert result is False

    @pytest.mark.asyncio
    async def test_no_dead_shlex_code(self, tmux_controller):
        """The dead shlex.quote(text) call has been removed."""
        import inspect
        source = inspect.getsource(tmux_controller.send_input_async)
        assert "shlex.quote" not in source
        assert "escaped_text" not in source

    @pytest.mark.asyncio
    async def test_uses_communicate_not_wait(self, tmux_controller):
        """Uses proc.communicate() (safe with PIPE) instead of proc.wait()."""
        import inspect
        source = inspect.getsource(tmux_controller.send_input_async)
        assert "proc.wait()" not in source
        assert "proc.communicate()" in source
