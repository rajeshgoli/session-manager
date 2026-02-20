"""Unit tests for TelegramBot methods.

Covers internal logic not exercised by regression tests that mock at the
whole-function boundary.
"""

import pytest
from unittest.mock import Mock, AsyncMock, MagicMock, call, patch

from src.telegram_bot import TelegramBot


# ============================================================================
# send_notification silent= parameter tests
# ============================================================================


@pytest.mark.asyncio
async def test_send_notification_silent_logs_warning_not_error():
    """When silent=True and send fails, logs WARNING (not ERROR), returns None."""
    tg = TelegramBot.__new__(TelegramBot)
    tg.bot = AsyncMock()
    tg.bot.send_message = AsyncMock(side_effect=Exception("TOPIC_CLOSED"))

    with patch("src.telegram_bot.logger") as mock_logger:
        result = await tg.send_notification(
            chat_id=10000, message="hello", message_thread_id=50000, silent=True
        )

    assert result is None
    mock_logger.warning.assert_called_once()
    mock_logger.error.assert_not_called()


@pytest.mark.asyncio
async def test_send_notification_noisy_logs_error():
    """When silent=False (default) and send fails, logs ERROR."""
    tg = TelegramBot.__new__(TelegramBot)
    tg.bot = AsyncMock()
    tg.bot.send_message = AsyncMock(side_effect=Exception("some error"))

    with patch("src.telegram_bot.logger") as mock_logger:
        result = await tg.send_notification(
            chat_id=10000, message="hello", message_thread_id=50000
        )

    assert result is None
    mock_logger.error.assert_called_once()


# ============================================================================
# send_with_fallback unit tests
# ============================================================================


@pytest.mark.asyncio
async def test_send_with_fallback_forum_success_no_fallback():
    """Forum send succeeds: send_notification called once with message_thread_id, msg_id returned."""
    tg = Mock(spec=TelegramBot)
    tg.send_notification = AsyncMock(return_value=9001)

    result = await TelegramBot.send_with_fallback(
        tg, chat_id=10000, message="Session stopped [sess]", thread_id=50000
    )

    # Only the forum path is attempted (silent=True suppresses error log on probe failure)
    tg.send_notification.assert_called_once_with(
        chat_id=10000,
        message="Session stopped [sess]",
        message_thread_id=50000,
        silent=True,
    )
    # Forum msg_id returned — caller uses this to decide whether to close the topic
    assert result == 9001


@pytest.mark.asyncio
async def test_send_with_fallback_forum_failure_uses_reply_thread():
    """Forum send fails (returns None): fallback send_notification with reply_to_message_id called."""
    tg = Mock(spec=TelegramBot)
    tg.send_notification = AsyncMock(return_value=None)

    result = await TelegramBot.send_with_fallback(
        tg, chat_id=10000, message="Session stopped [sess]", thread_id=50000
    )

    # Two calls: forum first (silent probe), then reply-thread fallback (also silent)
    assert tg.send_notification.call_count == 2
    assert tg.send_notification.call_args_list == [
        call(chat_id=10000, message="Session stopped [sess]", message_thread_id=50000, silent=True),
        call(chat_id=10000, message="Session stopped [sess]", reply_to_message_id=50000, silent=True),
    ]
    # Returns None (forum result) so callers know not to close forum topic
    assert result is None


@pytest.mark.asyncio
async def test_send_with_fallback_both_fail_logs_warning_not_error():
    """Spec item 7: when both forum and fallback sends fail, failures are logged at WARNING not ERROR."""
    tg = TelegramBot.__new__(TelegramBot)
    tg.bot = AsyncMock()
    tg.bot.send_message = AsyncMock(side_effect=Exception("network error"))

    with patch("src.telegram_bot.logger") as mock_logger:
        result = await tg.send_with_fallback(
            chat_id=10000, message="Session stopped [sess]", thread_id=50000
        )

    assert result is None
    mock_logger.error.assert_not_called()
    assert mock_logger.warning.call_count == 2
