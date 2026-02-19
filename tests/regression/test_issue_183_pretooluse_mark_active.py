"""Regression tests for sm#183: PreToolUse clears stale is_idle.

When a PreToolUse hook fires, mark_session_active is called to clear
stale is_idle=True. This prevents non-urgent sm send from delivering
messages mid-turn once tool calls begin.
"""

import pytest
import asyncio
from datetime import datetime
from unittest.mock import MagicMock, AsyncMock, patch

from src.message_queue import MessageQueueManager
from src.models import SessionDeliveryState, SessionStatus


@pytest.fixture
def mock_session_manager():
    mock = MagicMock()
    mock.sessions = {}
    mock.get_session = MagicMock(return_value=None)
    mock._save_state = MagicMock()
    mock._deliver_direct = AsyncMock(return_value=True)
    return mock


@pytest.fixture
def mq(mock_session_manager, tmp_path):
    return MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=str(tmp_path / "test_mq.db"),
        config={
            "sm_send": {"input_poll_interval": 1, "input_stale_timeout": 30},
            "timeouts": {"message_queue": {"subprocess_timeout_seconds": 1}},
        },
        notifier=None,
    )


def _make_session(session_id="target183", provider="claude"):
    s = MagicMock()
    s.id = session_id
    s.provider = provider
    s.tmux_session = f"tmux-{session_id}"
    s.friendly_name = "test-agent"
    s.name = "claude-agent"
    s.status = SessionStatus.RUNNING
    s.last_activity = datetime.now()
    return s


class TestPreToolUseClearsStaleIdle:
    """Core fix: PreToolUse fires mark_session_active to clear stale is_idle."""

    def test_mark_session_active_clears_idle(self, mq):
        """mark_session_active sets is_idle=False, preventing stale delivery."""
        # Simulate: Stop hook fired → is_idle=True
        state = mq._get_or_create_state("target183")
        state.is_idle = True

        # PreToolUse fires → mark_session_active
        mq.mark_session_active("target183")

        assert state.is_idle is False

    @pytest.mark.asyncio
    async def test_important_deferred_after_mark_active(self, mq, mock_session_manager):
        """Important message deferred when is_idle cleared by mark_session_active."""
        session = _make_session()
        mock_session_manager.get_session.return_value = session

        # Simulate stale idle
        state = mq._get_or_create_state("target183")
        state.is_idle = True

        # PreToolUse fires → clears idle
        mq.mark_session_active("target183")

        # Queue important message
        mq.queue_message("target183", "Hello", delivery_mode="important")

        # Mock user input check to return None
        mq._get_pending_user_input_async = AsyncMock(return_value=None)

        # Try delivery — should be deferred (is_idle=False)
        await mq._try_deliver_messages("target183", important_only=True)

        mock_session_manager._deliver_direct.assert_not_called()
        assert mq.get_queue_length("target183") == 1

    @pytest.mark.asyncio
    async def test_sequential_deferred_after_mark_active(self, mq, mock_session_manager):
        """Sequential message deferred when is_idle cleared by mark_session_active."""
        session = _make_session()
        mock_session_manager.get_session.return_value = session

        state = mq._get_or_create_state("target183")
        state.is_idle = True

        mq.mark_session_active("target183")
        mq.queue_message("target183", "Hello", delivery_mode="sequential")
        mq._get_pending_user_input_async = AsyncMock(return_value=None)

        await mq._try_deliver_messages("target183")

        mock_session_manager._deliver_direct.assert_not_called()
        assert mq.get_queue_length("target183") == 1


class TestIdleDeliveryUnaffected:
    """Regression: delivery to genuinely idle agents still works."""

    @pytest.mark.asyncio
    async def test_important_delivers_when_idle(self, mq, mock_session_manager):
        """Important message delivers immediately when is_idle=True (genuine)."""
        session = _make_session()
        mock_session_manager.get_session.return_value = session

        # Genuinely idle (Stop hook fired, no PreToolUse since)
        state = mq._get_or_create_state("target183")
        state.is_idle = True

        mq.queue_message("target183", "Important msg", delivery_mode="important")
        mq._get_pending_user_input_async = AsyncMock(return_value=None)

        await mq._try_deliver_messages("target183", important_only=True)

        mock_session_manager._deliver_direct.assert_called_once()
        assert mq.get_queue_length("target183") == 0

    @pytest.mark.asyncio
    async def test_sequential_delivers_when_idle(self, mq, mock_session_manager):
        """Sequential message delivers when is_idle=True (genuine)."""
        session = _make_session()
        mock_session_manager.get_session.return_value = session

        state = mq._get_or_create_state("target183")
        state.is_idle = True

        mq.queue_message("target183", "Sequential msg", delivery_mode="sequential")
        mq._get_pending_user_input_async = AsyncMock(return_value=None)

        await mq._try_deliver_messages("target183")

        mock_session_manager._deliver_direct.assert_called_once()
        assert mq.get_queue_length("target183") == 0


class TestStopHookResetsIdle:
    """Stop hook → mark_session_idle delivers deferred messages."""

    @pytest.mark.asyncio
    async def test_deferred_message_delivered_on_stop_hook(self, mq, mock_session_manager):
        """Message deferred by mark_session_active is delivered when Stop hook fires."""
        session = _make_session()
        mock_session_manager.get_session.return_value = session

        # Queue message
        mq.queue_message("target183", "Deferred msg", delivery_mode="sequential")

        # Agent is active (PreToolUse cleared idle)
        mq.mark_session_active("target183")
        mq._get_pending_user_input_async = AsyncMock(return_value=None)

        # Delivery attempt fails (is_idle=False)
        await mq._try_deliver_messages("target183")
        mock_session_manager._deliver_direct.assert_not_called()

        # Stop hook fires → mark_session_idle → triggers delivery
        with patch("asyncio.create_task") as mock_task:
            mq.mark_session_idle("target183")

        assert mq.is_session_idle("target183") is True
        # mark_session_idle creates a task to deliver
        assert mock_task.called
