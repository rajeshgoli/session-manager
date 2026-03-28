"""Unit tests for TelegramBot methods.

Covers internal logic not exercised by regression tests that mock at the
whole-function boundary.
"""

import pytest
from unittest.mock import Mock, AsyncMock, MagicMock, call, patch
from types import SimpleNamespace

from src.models import DeliveryResult
from src.telegram_bot import TelegramBot, TELEGRAM_MESSAGE_CHAR_LIMIT


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


@pytest.mark.asyncio
async def test_send_notification_chunks_oversized_plain_text():
    """Oversized plain-text messages should be sent as numbered chunks."""
    tg = TelegramBot.__new__(TelegramBot)
    tg.bot = AsyncMock()
    tg.bot.send_message = AsyncMock(
        side_effect=[
            SimpleNamespace(message_id=101),
            SimpleNamespace(message_id=102),
        ]
    )

    message = ("a" * (TELEGRAM_MESSAGE_CHAR_LIMIT - 100)) + "\n" + ("b" * 300)
    result = await tg.send_notification(chat_id=10000, message=message, message_thread_id=50000)

    assert result == 101
    assert tg.bot.send_message.await_count == 2
    first_call = tg.bot.send_message.await_args_list[0]
    second_call = tg.bot.send_message.await_args_list[1]
    assert first_call.kwargs["message_thread_id"] == 50000
    assert first_call.kwargs["text"].startswith("[1/2]\n")
    assert second_call.kwargs["text"].startswith("[2/2]\n")


@pytest.mark.asyncio
async def test_send_notification_chunks_oversized_markdown_as_plain_text():
    """Oversized Markdown messages should degrade to plain-text chunks, not fail."""
    tg = TelegramBot.__new__(TelegramBot)
    tg.bot = AsyncMock()
    tg.bot.send_message = AsyncMock(
        side_effect=[
            SimpleNamespace(message_id=201),
            SimpleNamespace(message_id=202),
            SimpleNamespace(message_id=203),
        ]
    )

    message = ("\\*hello\\* " * 1000)
    result = await tg.send_notification(
        chat_id=10000,
        message=message,
        message_thread_id=50000,
        parse_mode="MarkdownV2",
    )

    assert result == 201
    assert tg.bot.send_message.await_count >= 2
    for call_args in tg.bot.send_message.await_args_list:
        assert "parse_mode" not in call_args.kwargs
        assert "\\" not in call_args.kwargs["text"]


@pytest.mark.asyncio
async def test_send_notification_keeps_markdown_when_only_escaped_length_exceeds_limit():
    """Escaped MarkdownV2 should not be chunked if rendered text is still under Telegram's limit."""
    tg = TelegramBot.__new__(TelegramBot)
    tg.bot = AsyncMock()
    tg.bot.send_message = AsyncMock(return_value=SimpleNamespace(message_id=301))

    message = "\\[" * 2500
    result = await tg.send_notification(
        chat_id=10000,
        message=message,
        message_thread_id=50000,
        parse_mode="MarkdownV2",
    )

    assert result == 301
    tg.bot.send_message.assert_awaited_once()
    call_args = tg.bot.send_message.await_args
    assert call_args.kwargs["parse_mode"] == "MarkdownV2"


@pytest.mark.asyncio
async def test_send_notification_chunked_markdown_preserves_literal_backslashes():
    """Chunked Markdown fallback should preserve literal backslashes such as Windows paths."""
    tg = TelegramBot.__new__(TelegramBot)
    tg.bot = AsyncMock()
    tg.bot.send_message = AsyncMock(
        side_effect=[
            SimpleNamespace(message_id=401),
            SimpleNamespace(message_id=402),
        ]
    )

    message = ("C:\\temp " * 600) + ("\\*hello\\* " * 100)
    result = await tg.send_notification(
        chat_id=10000,
        message=message,
        message_thread_id=50000,
        parse_mode="MarkdownV2",
    )

    assert result == 401
    assert tg.bot.send_message.await_count == 2
    first_text = tg.bot.send_message.await_args_list[0].kwargs["text"]
    assert "C:\\temp" in first_text


def test_split_message_chunks_preserves_indentation_after_newline_boundary():
    """Chunk splitting should not strip indentation from the next chunk."""
    tg = TelegramBot.__new__(TelegramBot)
    message = ("a" * 20) + "\n    indented line"

    chunks = tg._split_message_chunks(message, limit=20)

    assert chunks == ["a" * 20, "    indented line"]


def test_markdown_v2_to_plain_text_preserves_backslashes_inside_code():
    """Plain-text fallback should keep literal backslashes inside code spans."""
    tg = TelegramBot.__new__(TelegramBot)

    plain = tg._markdown_v2_to_plain_text(r"Prefix `\\_` and `\\.py` suffix")

    assert plain == r"Prefix `\\_` and `\\.py` suffix"


@pytest.mark.asyncio
async def test_send_notification_keeps_markdown_when_only_link_urls_exceed_limit():
    """Link-heavy Markdown should stay formatted when rendered text is still within the limit."""
    tg = TelegramBot.__new__(TelegramBot)
    tg.bot = AsyncMock()
    tg.bot.send_message = AsyncMock(return_value=SimpleNamespace(message_id=501))

    message = ("[x](https://example.com/" + ("a" * 80) + ") " ) * 120
    result = await tg.send_notification(
        chat_id=10000,
        message=message,
        message_thread_id=50000,
        parse_mode="MarkdownV2",
    )

    assert result == 501
    tg.bot.send_message.assert_awaited_once()
    assert tg.bot.send_message.await_args.kwargs["parse_mode"] == "MarkdownV2"


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
async def test_send_with_fallback_forum_failure_can_skip_reply_fallback():
    """Forum-backed callers can disable reply fallback to avoid leaking into general chat."""
    tg = Mock(spec=TelegramBot)
    tg.send_notification = AsyncMock(return_value=None)

    result = await TelegramBot.send_with_fallback(
        tg,
        chat_id=10000,
        message="Session stopped [sess]",
        thread_id=50000,
        allow_reply_fallback=False,
    )

    tg.send_notification.assert_called_once_with(
        chat_id=10000,
        message="Session stopped [sess]",
        message_thread_id=50000,
        silent=True,
    )
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


@pytest.mark.asyncio
async def test_handle_message_ignores_command_text():
    """Slash commands should not also go through the plain-text input handler."""
    tg = TelegramBot(token="test-token")
    tg._on_session_input = AsyncMock()
    tg._is_allowed = lambda chat_id, user_id=None: True
    tg._get_session_from_context = lambda update: "sess123"

    reply_text = AsyncMock()
    update = SimpleNamespace(
        effective_chat=SimpleNamespace(id=100, is_forum=True),
        effective_user=SimpleNamespace(id=200),
        message=SimpleNamespace(
            text="/force next message",
            message_id=1,
            message_thread_id=10,
            reply_text=reply_text,
        ),
    )

    await tg._handle_message(update, SimpleNamespace(args=["next", "message"]))

    tg._on_session_input.assert_not_awaited()
    reply_text.assert_not_awaited()


@pytest.mark.asyncio
async def test_force_next_message_arms_urgent_delivery_for_followup():
    """`/force next message` should arm the next plain-text reply as urgent input."""
    tg = TelegramBot(token="test-token")
    tg._on_session_input = AsyncMock(return_value=DeliveryResult.DELIVERED)
    tg._is_allowed = lambda chat_id, user_id=None: True
    tg._get_session_from_context = lambda update: "sess123"

    arm_reply = AsyncMock()
    arm_update = SimpleNamespace(
        effective_chat=SimpleNamespace(id=100, is_forum=True),
        effective_user=SimpleNamespace(id=200),
        message=SimpleNamespace(
            text="/force next message",
            message_id=1,
            message_thread_id=10,
            reply_text=arm_reply,
        ),
    )

    await tg._cmd_force(arm_update, SimpleNamespace(args=["next", "message"]))

    tg._on_session_input.assert_not_awaited()
    arm_reply.assert_awaited_once()
    assert "Next message will interrupt immediately" in arm_reply.await_args.args[0]

    followup_reply = AsyncMock(return_value=SimpleNamespace(message_id=999))
    followup_update = SimpleNamespace(
        effective_chat=SimpleNamespace(id=100, is_forum=True),
        effective_user=SimpleNamespace(id=200),
        message=SimpleNamespace(
            text="Actual urgent payload",
            message_id=2,
            message_thread_id=10,
            reply_text=followup_reply,
        ),
    )

    await tg._handle_message(followup_update, SimpleNamespace(args=[]))

    tg._on_session_input.assert_awaited_once()
    delivered_input = tg._on_session_input.await_args.args[0]
    assert delivered_input.session_id == "sess123"
    assert delivered_input.text == "Actual urgent payload"
    assert delivered_input.delivery_mode == "urgent"


@pytest.mark.asyncio
async def test_handle_message_allows_slash_prefixed_non_command_text():
    """Slash-prefixed code snippets should still reach the session input handler."""
    tg = TelegramBot(token="test-token")
    tg._on_session_input = AsyncMock(return_value=DeliveryResult.DELIVERED)
    tg._is_allowed = lambda chat_id, user_id=None: True
    tg._get_session_from_context = lambda update: "sess123"

    delivered_reply = AsyncMock(return_value=SimpleNamespace(message_id=555))
    update = SimpleNamespace(
        effective_chat=SimpleNamespace(id=100, is_forum=True),
        effective_user=SimpleNamespace(id=200),
        message=SimpleNamespace(
            text="/tmp/config.yaml",
            message_id=3,
            message_thread_id=10,
            reply_text=delivered_reply,
        ),
    )

    await tg._handle_message(update, SimpleNamespace(args=[]))

    tg._on_session_input.assert_awaited_once()
    delivered_input = tg._on_session_input.await_args.args[0]
    assert delivered_input.text == "/tmp/config.yaml"
    assert delivered_input.delivery_mode == "sequential"


@pytest.mark.asyncio
async def test_handle_message_delivered_starts_typing_indicator_without_reply_stub():
    """Immediate Telegram delivery should start native typing instead of posting a progress stub."""
    tg = TelegramBot(token="test-token")
    tg._on_session_input = AsyncMock(return_value=DeliveryResult.DELIVERED)
    tg._is_allowed = lambda chat_id, user_id=None: True
    tg._get_session_from_context = lambda update: "sess123"
    tg._start_typing_indicator = Mock()

    delivered_reply = AsyncMock(return_value=SimpleNamespace(message_id=555))
    update = SimpleNamespace(
        effective_chat=SimpleNamespace(id=100, is_forum=True),
        effective_user=SimpleNamespace(id=200),
        message=SimpleNamespace(
            text="hello",
            message_id=3,
            message_thread_id=10,
            reply_text=delivered_reply,
        ),
    )

    await tg._handle_message(update, SimpleNamespace(args=[]))

    tg._on_session_input.assert_awaited_once()
    delivered_reply.assert_not_awaited()
    tg._start_typing_indicator.assert_called_once_with("sess123", 100, 10)


@pytest.mark.asyncio
async def test_delete_pending_input_msg_cancels_typing_indicator():
    """Response completion should stop any active typing indicator loop."""
    tg = TelegramBot(token="test-token")
    typing_task = Mock()
    typing_task.done.return_value = False
    tg._typing_indicator_tasks = {"sess123": typing_task}
    tg._pending_input_msgs = {}
    tg._completed_sessions = set()
    tg.bot = None

    await tg.delete_pending_input_msg("sess123")

    typing_task.cancel.assert_called_once()
    assert "sess123" not in tg._typing_indicator_tasks


@pytest.mark.asyncio
async def test_configure_bot_commands_registers_private_and_group_menus():
    """Bot startup should publish a curated Telegram command menu."""
    tg = TelegramBot(token="test-token")
    tg.bot = AsyncMock()

    await tg._configure_bot_commands()

    assert tg.bot.set_my_commands.await_count == 2
    private_call = tg.bot.set_my_commands.await_args_list[0]
    group_call = tg.bot.set_my_commands.await_args_list[1]
    private_names = [command.command for command in private_call.args[0]]
    group_names = [command.command for command in group_call.args[0]]

    assert "session" in private_names
    assert "follow" in private_names
    assert "force" in group_names
    assert "kill" in group_names
    assert type(private_call.kwargs["scope"]).__name__ == "BotCommandScopeAllPrivateChats"
    assert type(group_call.kwargs["scope"]).__name__ == "BotCommandScopeAllGroupChats"
    tg.bot.set_chat_menu_button.assert_awaited_once()
    assert type(tg.bot.set_chat_menu_button.await_args.kwargs["menu_button"]).__name__ == "MenuButtonCommands"


@pytest.mark.asyncio
async def test_run_typing_indicator_exits_for_stopped_session():
    """Typing indicator should not run forever after the session stops."""
    tg = TelegramBot(token="test-token")
    tg.bot = AsyncMock()
    tg._on_session_status = AsyncMock(
        return_value=SimpleNamespace(status=SimpleNamespace(value="stopped"))
    )
    tg._completed_sessions = set()
    tg._typing_indicator_tasks = {}

    await tg._run_typing_indicator("sess123", 100, 10)

    tg.bot.send_chat_action.assert_not_awaited()


@pytest.mark.asyncio
async def test_run_typing_indicator_exits_when_session_missing():
    """Typing indicator should stop when the target session disappears."""
    tg = TelegramBot(token="test-token")
    tg.bot = AsyncMock()
    tg._on_session_status = AsyncMock(return_value=None)
    tg._completed_sessions = set()
    tg._typing_indicator_tasks = {}

    await tg._run_typing_indicator("sess123", 100, 10)

    tg.bot.send_chat_action.assert_not_awaited()
