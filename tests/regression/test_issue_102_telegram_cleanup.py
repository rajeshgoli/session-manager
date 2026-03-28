"""Regression test for issue #102: Telegram topics never cleaned up when sessions die.

Bug: output_monitor.py and server.py were accessing 'telegram_bot' attribute instead of 'telegram',
causing topic cleanup to be skipped and health check to always report "Telegram not configured".
"""

import pytest
from unittest.mock import Mock, AsyncMock, MagicMock
from fastapi.testclient import TestClient

from src.output_monitor import OutputMonitor
from src.server import create_app
from src.models import Session, SessionStatus
from src.notifier import Notifier, NotificationEvent, NotificationChannel


@pytest.fixture
def mock_session():
    """Create a test session with Telegram integration."""
    return Session(
        id="test123",
        name="test-session",
        working_dir="/tmp",
        tmux_session="claude-test123",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
        telegram_chat_id=12345,
        telegram_thread_id=67890,
    )


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager with notifier."""
    manager = Mock()
    manager.sessions = {}
    manager._save_state = Mock()
    manager.app = Mock()
    manager.app.state = Mock()
    manager.app.state.last_claude_output = {}

    # Mock notifier with telegram (NOT telegram_bot)
    manager.notifier = Mock()
    manager.notifier.telegram = Mock()
    manager.notifier.telegram.bot = AsyncMock()
    manager.notifier.telegram._topic_sessions = {(12345, 67890): "test123"}
    manager.notifier.telegram._session_threads = {}
    # send_with_fallback is async; return a message_id so forum path is taken
    manager.notifier.telegram.send_with_fallback = AsyncMock(return_value=9999)

    return manager


@pytest.fixture
def output_monitor(mock_session_manager):
    """Create OutputMonitor with mocked session manager."""
    monitor = OutputMonitor(poll_interval=0.1)
    monitor.set_session_manager(mock_session_manager)
    monitor.set_save_state_callback(mock_session_manager._save_state)
    return monitor


@pytest.mark.asyncio
async def test_cleanup_accesses_correct_telegram_attribute(output_monitor, mock_session, mock_session_manager):
    """Test that cleanup_session accesses notifier.telegram, not notifier.telegram_bot.

    This is the fix for Bug 2 in issue #102.  The new behaviour sends a "Session stopped"
    message via send_notification (try-and-fallback) instead of delete_forum_topic (#200).
    """
    # Add session to manager
    mock_session_manager.sessions[mock_session.id] = mock_session

    # Call cleanup
    await output_monitor.cleanup_session(mock_session)

    # Verify that telegram.send_with_fallback was called.
    # This would NOT happen if the code was still accessing telegram_bot (which doesn't exist).
    mock_session_manager.notifier.telegram.send_with_fallback.assert_called_once_with(
        chat_id=12345,
        message=f"Session stopped [{mock_session.id}]",
        thread_id=67890,
        allow_reply_fallback=False,
    )


def test_notifier_attribute_name():
    """Test that demonstrates why the bug existed: wrong attribute name.

    The bug was accessing notifier.telegram_bot when the actual attribute is notifier.telegram.
    """
    from src.notifier import Notifier

    # Create a real notifier
    mock_bot = Mock()
    notifier = Notifier(telegram_bot=mock_bot)

    # The CORRECT attribute is 'telegram'
    assert hasattr(notifier, 'telegram')
    assert notifier.telegram is mock_bot

    # The code was trying to access 'telegram_bot' which doesn't exist on the instance
    # (it exists as a parameter name, but is stored as 'telegram')
    # Using getattr with a default would return None for telegram_bot
    telegram_via_wrong_name = getattr(notifier, 'telegram_bot', None)
    telegram_via_correct_name = getattr(notifier, 'telegram', None)

    # This demonstrates the bug: wrong name returns None, correct name returns the bot
    assert telegram_via_wrong_name is None
    assert telegram_via_correct_name is mock_bot


def test_health_check_telegram_attribute():
    """Verify health check fix: server.py now accesses notifier.telegram correctly.

    Bug 3 from issue #102: server.py:481 was using getattr(notifier, 'telegram_bot', None)
    which always returned None, causing health check to report "Telegram not configured"
    even when Telegram WAS configured.

    Fix: Changed to getattr(notifier, 'telegram', None)

    This test just verifies the attribute access pattern.
    Full health check testing is in tests/unit/test_health_check.py
    """
    from src.notifier import Notifier

    # Create a notifier with telegram bot
    mock_bot = Mock()
    notifier = Notifier(telegram_bot=mock_bot)

    # Simulate what the health check does (after the fix)
    telegram_bot = getattr(notifier, 'telegram', None)

    # This should NOT be None (the fix)
    assert telegram_bot is not None
    assert telegram_bot is mock_bot

    # The old buggy code would have done this:
    telegram_bot_wrong = getattr(notifier, 'telegram_bot', None)

    # Which would return None, causing false "not configured" reports
    assert telegram_bot_wrong is None


def test_notifier_stores_telegram_bot_as_telegram():
    """Verify that Notifier stores the bot as self.telegram, not self.telegram_bot."""
    from src.notifier import Notifier
    from src.telegram_bot import TelegramBot

    # Create a mock telegram bot
    mock_bot = Mock(spec=TelegramBot)

    # Create notifier
    notifier = Notifier(telegram_bot=mock_bot)

    # Verify it's stored as 'telegram', not 'telegram_bot'
    assert hasattr(notifier, 'telegram')
    assert notifier.telegram is mock_bot

    # Verify telegram_bot attribute doesn't exist (or is different)
    # The parameter name is telegram_bot, but it's stored as telegram
    assert not hasattr(notifier, 'telegram_bot') or getattr(notifier, 'telegram_bot', None) != mock_bot


@pytest.mark.asyncio
async def test_notifier_ensures_topic_for_live_session_instead_of_posting_to_general():
    session = Session(
        id="live123",
        name="live-session",
        working_dir="/tmp",
        tmux_session="claude-live123",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
        telegram_chat_id=12345,
        telegram_thread_id=None,
    )
    telegram = Mock()
    telegram.send_notification = AsyncMock(return_value=4321)
    telegram.get_session_thread = Mock(return_value=None)
    telegram.delete_pending_input_msg = AsyncMock()

    manager = Mock()
    manager._ensure_telegram_topic = AsyncMock(
        side_effect=lambda s: setattr(s, "telegram_thread_id", 67890)
    )

    notifier = Notifier(telegram_bot=telegram)
    notifier.session_manager = manager

    event = NotificationEvent(
        session_id=session.id,
        event_type="response",
        message="ok",
        context="response body",
        channel=NotificationChannel.TELEGRAM,
    )

    ok = await notifier._notify_telegram(event, session, "response body", use_markdown=False)

    assert ok is True
    manager._ensure_telegram_topic.assert_awaited_once_with(session)
    telegram.send_notification.assert_awaited_once()
    assert telegram.send_notification.await_args.kwargs["message_thread_id"] == 67890
    assert telegram.send_notification.await_args.kwargs["reply_to_message_id"] is None


@pytest.mark.asyncio
async def test_notifier_suppresses_live_session_notification_when_topic_still_missing():
    session = Session(
        id="live124",
        name="live-session",
        working_dir="/tmp",
        tmux_session="claude-live124",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
        telegram_chat_id=12345,
        telegram_thread_id=None,
    )
    telegram = Mock()
    telegram.send_notification = AsyncMock(return_value=4321)
    telegram.get_session_thread = Mock(return_value=None)
    telegram.delete_pending_input_msg = AsyncMock()

    manager = Mock()
    manager._ensure_telegram_topic = AsyncMock(return_value=None)

    notifier = Notifier(telegram_bot=telegram)
    notifier.session_manager = manager

    event = NotificationEvent(
        session_id=session.id,
        event_type="response",
        message="ok",
        context="response body",
        channel=NotificationChannel.TELEGRAM,
    )

    ok = await notifier._notify_telegram(event, session, "response body", use_markdown=False)

    assert ok is False
    manager._ensure_telegram_topic.assert_awaited_once_with(session)
    telegram.send_notification.assert_not_awaited()
