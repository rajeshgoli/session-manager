"""Unit tests for TelegramBot methods.

Covers internal logic not exercised by regression tests that mock at the
whole-function boundary.
"""

import pytest
from unittest.mock import Mock, AsyncMock, call

from src.telegram_bot import TelegramBot


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

    # Only the forum path is attempted
    tg.send_notification.assert_called_once_with(
        chat_id=10000,
        message="Session stopped [sess]",
        message_thread_id=50000,
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

    # Two calls: forum first, then reply-thread fallback
    assert tg.send_notification.call_count == 2
    assert tg.send_notification.call_args_list == [
        call(chat_id=10000, message="Session stopped [sess]", message_thread_id=50000),
        call(chat_id=10000, message="Session stopped [sess]", reply_to_message_id=50000),
    ]
    # Returns None (forum result) so callers know not to close forum topic
    assert result is None
