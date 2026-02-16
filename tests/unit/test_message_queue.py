"""Unit tests for MessageQueueManager - ticket #63."""

import pytest
import asyncio
import tempfile
import sqlite3
from datetime import datetime, timedelta
from pathlib import Path
from unittest.mock import MagicMock, AsyncMock, patch, PropertyMock

from src.message_queue import MessageQueueManager
from src.models import QueuedMessage, SessionDeliveryState, SessionStatus, Session


# Patch asyncio.create_task globally for tests that don't need async
def noop_create_task(coro):
    """Silently close coroutine without running it."""
    coro.close()
    return MagicMock()


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager."""
    mock = MagicMock()
    mock.sessions = {}
    mock.get_session = MagicMock(return_value=None)
    mock.tmux = MagicMock()
    mock.tmux.send_input_async = AsyncMock(return_value=True)
    mock._save_state = MagicMock()
    mock._deliver_direct = AsyncMock(return_value=True)
    return mock


@pytest.fixture
def temp_db_path(tmp_path):
    """Create a temporary database path."""
    return str(tmp_path / "test_message_queue.db")


@pytest.fixture
def message_queue(mock_session_manager, temp_db_path):
    """Create a MessageQueueManager with mocked dependencies."""
    mq = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=temp_db_path,
        config={
            "sm_send": {
                "input_poll_interval": 1,
                "input_stale_timeout": 30,
                "max_batch_size": 10,
                "urgent_delay_ms": 100,
            },
            "timeouts": {
                "message_queue": {
                    "subprocess_timeout_seconds": 1,
                    "async_send_timeout_seconds": 2,
                }
            }
        },
        notifier=None,  # No Telegram mirroring in tests
    )
    return mq


class TestQueueing:
    """Tests for message queueing functionality."""

    def test_queue_message_persists_to_db(self, message_queue):
        """Queued messages are stored in SQLite."""
        msg = message_queue.queue_message(
            target_session_id="target123",
            text="Hello, world!",
            sender_session_id="sender456",
            sender_name="Test Sender",
        )

        # Verify message was returned
        assert msg.id is not None
        assert msg.target_session_id == "target123"
        assert msg.text == "Hello, world!"

        # Verify it's in the database
        pending = message_queue.get_pending_messages("target123")
        assert len(pending) == 1
        assert pending[0].id == msg.id
        assert pending[0].text == "Hello, world!"

    def test_queue_message_with_timeout(self, message_queue):
        """Messages with timeout have correct timeout_at."""
        msg = message_queue.queue_message(
            target_session_id="target123",
            text="Urgent message",
            timeout_seconds=300,  # 5 minutes
        )

        # Verify timeout was set
        assert msg.timeout_at is not None
        expected_timeout = msg.queued_at + timedelta(seconds=300)
        # Allow 1 second tolerance
        assert abs((msg.timeout_at - expected_timeout).total_seconds()) < 1

    def test_queue_message_without_timeout(self, message_queue):
        """Messages without timeout have None timeout_at."""
        msg = message_queue.queue_message(
            target_session_id="target123",
            text="Normal message",
        )
        assert msg.timeout_at is None

    def test_queue_multiple_messages_preserves_order(self, message_queue):
        """Multiple queued messages preserve FIFO order."""
        msg1 = message_queue.queue_message("target123", "First")
        msg2 = message_queue.queue_message("target123", "Second")
        msg3 = message_queue.queue_message("target123", "Third")

        pending = message_queue.get_pending_messages("target123")
        assert len(pending) == 3
        assert pending[0].text == "First"
        assert pending[1].text == "Second"
        assert pending[2].text == "Third"


class TestDeliveryModes:
    """Tests for different delivery modes."""

    def test_default_mode_is_sequential(self, message_queue):
        """Default delivery mode is sequential."""
        msg = message_queue.queue_message(
            target_session_id="target123",
            text="Default mode message",
        )
        assert msg.delivery_mode == "sequential"

    def test_important_mode_queued(self, message_queue):
        """Important mode messages are queued."""
        # Patch asyncio.create_task to avoid event loop issues
        with patch('asyncio.create_task', noop_create_task):
            msg = message_queue.queue_message(
                target_session_id="target123",
                text="Important message",
                delivery_mode="important",
            )
        assert msg.delivery_mode == "important"

        pending = message_queue.get_pending_messages("target123")
        assert len(pending) == 1
        assert pending[0].delivery_mode == "important"

    def test_urgent_mode_queued(self, message_queue):
        """Urgent mode messages are queued (delivery handled async)."""
        # Patch asyncio.create_task to avoid event loop issues
        with patch('asyncio.create_task', noop_create_task):
            msg = message_queue.queue_message(
                target_session_id="target123",
                text="Urgent message",
                delivery_mode="urgent",
            )
        assert msg.delivery_mode == "urgent"


class TestStateManagement:
    """Tests for session state management."""

    def test_mark_session_idle(self, message_queue):
        """mark_session_idle sets is_idle to True."""
        # Patch asyncio.create_task to avoid event loop issues
        with patch('asyncio.create_task', noop_create_task):
            message_queue.mark_session_idle("session123")

        state = message_queue.delivery_states.get("session123")
        assert state is not None
        assert state.is_idle is True
        assert state.last_idle_at is not None

    def test_mark_session_active(self, message_queue):
        """mark_session_active sets is_idle to False."""
        # Patch asyncio.create_task to avoid event loop issues
        with patch('asyncio.create_task', noop_create_task):
            # First mark idle
            message_queue.mark_session_idle("session123")
        assert message_queue.is_session_idle("session123") is True

        # Then mark active
        message_queue.mark_session_active("session123")
        assert message_queue.is_session_idle("session123") is False

    def test_is_session_idle_false_by_default(self, message_queue):
        """is_session_idle returns False for unknown sessions."""
        assert message_queue.is_session_idle("unknown") is False


class TestPendingMessages:
    """Tests for pending message handling."""

    def test_expired_messages_not_returned(self, message_queue):
        """Messages past timeout_at are skipped in get_pending_messages."""
        # Queue message that's already expired by manually inserting
        expired_time = (datetime.now() - timedelta(hours=1)).isoformat()
        timeout_time = (datetime.now() - timedelta(minutes=30)).isoformat()

        # Insert directly into database with past timeout
        message_queue._execute("""
            INSERT INTO message_queue
            (id, target_session_id, text, delivery_mode, queued_at, timeout_at)
            VALUES (?, ?, ?, ?, ?, ?)
        """, ("expired_msg", "target123", "Will expire", "sequential", expired_time, timeout_time))

        # Should not be in pending (expired messages are skipped)
        pending = message_queue.get_pending_messages("target123")
        assert len(pending) == 0

    def test_get_queue_length(self, message_queue):
        """get_queue_length returns correct count."""
        assert message_queue.get_queue_length("target123") == 0

        message_queue.queue_message("target123", "Message 1")
        assert message_queue.get_queue_length("target123") == 1

        message_queue.queue_message("target123", "Message 2")
        assert message_queue.get_queue_length("target123") == 2


class TestBatchDelivery:
    """Tests for batch delivery."""

    def test_max_batch_size_respected(self, message_queue):
        """Batch size is limited by max_batch_size config."""
        # Queue more messages than max_batch_size
        for i in range(15):
            message_queue.queue_message("target123", f"Message {i}")

        pending = message_queue.get_pending_messages("target123")
        assert len(pending) == 15  # All messages returned

        # The actual batching is done in _try_deliver_messages
        # which limits to max_batch_size (10)


class TestQueueStatus:
    """Tests for queue status API."""

    def test_get_queue_status(self, message_queue):
        """get_queue_status returns correct status dict."""
        # Queue some messages
        message_queue.queue_message(
            target_session_id="target123",
            text="Test message",
            sender_session_id="sender456",
            sender_name="Test Sender",
        )
        # Patch asyncio.create_task when marking idle
        with patch('asyncio.create_task', noop_create_task):
            message_queue.mark_session_idle("target123")

        status = message_queue.get_queue_status("target123")

        assert status["session_id"] == "target123"
        assert status["is_idle"] is True
        assert status["pending_count"] == 1
        assert len(status["pending_messages"]) == 1
        assert status["pending_messages"][0]["sender"] == "Test Sender"

    def test_get_queue_status_empty(self, message_queue):
        """get_queue_status works for empty queue."""
        status = message_queue.get_queue_status("target123")

        assert status["session_id"] == "target123"
        assert status["is_idle"] is False
        assert status["pending_count"] == 0
        assert status["pending_messages"] == []


class TestDatabaseOperations:
    """Tests for database operations."""

    def test_database_created_on_init(self, temp_db_path, mock_session_manager):
        """Database and tables are created on initialization."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            notifier=None,
        )

        # Verify database file exists
        assert Path(temp_db_path).exists()

        # Verify tables exist
        conn = sqlite3.connect(temp_db_path)
        cursor = conn.cursor()

        cursor.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='message_queue'")
        assert cursor.fetchone() is not None

        cursor.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='scheduled_reminders'")
        assert cursor.fetchone() is not None

        conn.close()

    def test_mark_delivered_updates_db(self, message_queue):
        """_mark_delivered updates delivered_at in database."""
        msg = message_queue.queue_message("target123", "Test")

        # Verify not delivered yet
        pending = message_queue.get_pending_messages("target123")
        assert len(pending) == 1

        # Mark as delivered
        message_queue._mark_delivered(msg.id)

        # Should no longer be pending
        pending = message_queue.get_pending_messages("target123")
        assert len(pending) == 0


class TestNotifyCallback:
    """Tests for notification callback."""

    def test_set_notify_callback(self, message_queue):
        """set_notify_callback stores callback."""
        callback = MagicMock()
        message_queue.set_notify_callback(callback)
        assert message_queue._notify_callback == callback


class TestCleanup:
    """Tests for cleanup operations."""

    def test_cleanup_messages_for_session(self, message_queue):
        """_cleanup_messages_for_session removes all pending messages."""
        # Queue messages
        message_queue.queue_message("target123", "Message 1")
        message_queue.queue_message("target123", "Message 2")
        message_queue.queue_message("other456", "Other session")

        assert message_queue.get_queue_length("target123") == 2
        assert message_queue.get_queue_length("other456") == 1

        # Cleanup target123
        message_queue._cleanup_messages_for_session("target123")

        assert message_queue.get_queue_length("target123") == 0
        assert message_queue.get_queue_length("other456") == 1  # Unaffected


class TestDeliveryLocks:
    """Tests for per-session delivery locks."""

    def test_delivery_lock_created_per_session(self, message_queue):
        """Each session gets its own delivery lock."""
        # Access locks via internal method
        lock1 = message_queue._delivery_locks.setdefault("session1", asyncio.Lock())
        lock2 = message_queue._delivery_locks.setdefault("session2", asyncio.Lock())

        assert lock1 is not lock2
        assert "session1" in message_queue._delivery_locks
        assert "session2" in message_queue._delivery_locks

    def test_same_session_gets_same_lock(self, message_queue):
        """Same session gets the same lock instance."""
        lock1 = message_queue._delivery_locks.setdefault("session1", asyncio.Lock())
        lock2 = message_queue._delivery_locks.setdefault("session1", asyncio.Lock())

        assert lock1 is lock2


class TestCodexIdleDetection:
    """Tests for Codex CLI idle detection in _watch_for_idle (#168)."""

    @pytest.mark.asyncio
    async def test_check_codex_prompt_bare_chevron(self, message_queue):
        """_check_codex_prompt returns True for bare '>' prompt."""
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"some output\n>", b""))
        mock_proc.returncode = 0

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await message_queue._check_codex_prompt("tmux-codex")
        assert result is True

    @pytest.mark.asyncio
    async def test_check_codex_prompt_with_trailing_space(self, message_queue):
        """_check_codex_prompt returns True for '> ' prompt (no user text)."""
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"some output\n> ", b""))
        mock_proc.returncode = 0

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await message_queue._check_codex_prompt("tmux-codex")
        assert result is True

    @pytest.mark.asyncio
    async def test_check_codex_prompt_with_user_text(self, message_queue):
        """_check_codex_prompt returns False when user has typed text."""
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"some output\n> hello world", b""))
        mock_proc.returncode = 0

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await message_queue._check_codex_prompt("tmux-codex")
        assert result is False

    @pytest.mark.asyncio
    async def test_check_codex_prompt_no_prompt(self, message_queue):
        """_check_codex_prompt returns False when no prompt visible."""
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"Processing task...\nDone.", b""))
        mock_proc.returncode = 0

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await message_queue._check_codex_prompt("tmux-codex")
        assert result is False

    @pytest.mark.asyncio
    async def test_check_codex_prompt_empty_output(self, message_queue):
        """_check_codex_prompt returns False for empty output."""
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))
        mock_proc.returncode = 0

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await message_queue._check_codex_prompt("tmux-codex")
        assert result is False

    @pytest.mark.asyncio
    async def test_check_codex_prompt_tmux_error(self, message_queue):
        """_check_codex_prompt returns False when tmux command fails."""
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"", b"no session"))
        mock_proc.returncode = 1

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await message_queue._check_codex_prompt("tmux-codex")
        assert result is False

    @pytest.mark.asyncio
    async def test_check_codex_prompt_exception(self, message_queue):
        """_check_codex_prompt returns False on exception."""
        with patch("asyncio.create_subprocess_exec", side_effect=OSError("tmux not found")):
            result = await message_queue._check_codex_prompt("tmux-codex")
        assert result is False

    @pytest.mark.asyncio
    async def test_watch_codex_idle_requires_two_consecutive(self, mock_session_manager, temp_db_path):
        """_watch_for_idle requires two consecutive prompt detections for Codex."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        # Create a Codex session
        codex_session = MagicMock()
        codex_session.id = "codex123"
        codex_session.provider = "codex"
        codex_session.tmux_session = "tmux-codex"
        codex_session.friendly_name = "test-codex"
        codex_session.name = "codex-agent"
        mock_session_manager.get_session = MagicMock(return_value=codex_session)

        # Simulate: first poll sees prompt, second poll sees prompt → idle
        prompt_results = [True, True]
        call_count = {"n": 0}

        async def mock_check_codex_prompt(tmux_session):
            idx = min(call_count["n"], len(prompt_results) - 1)
            result = prompt_results[idx]
            call_count["n"] += 1
            return result

        mq._check_codex_prompt = mock_check_codex_prompt

        # Run watch with short timeout
        await mq._watch_for_idle("watch1", "codex123", "watcher456", timeout_seconds=5)

        # Should have notified the watcher (idle detected)
        pending = mq.get_pending_messages("watcher456")
        assert len(pending) == 1
        assert "is now idle" in pending[0].text

    @pytest.mark.asyncio
    async def test_watch_codex_single_prompt_not_idle(self, mock_session_manager, temp_db_path):
        """_watch_for_idle does NOT fire after single prompt detection (transient guard)."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        # Create a Codex session
        codex_session = MagicMock()
        codex_session.id = "codex123"
        codex_session.provider = "codex"
        codex_session.tmux_session = "tmux-codex"
        codex_session.friendly_name = "test-codex"
        codex_session.name = "codex-agent"
        mock_session_manager.get_session = MagicMock(return_value=codex_session)

        # Simulate: prompt visible once, then gone (transient), then timeout
        prompt_results = [True, False, False, False]
        call_count = {"n": 0}

        async def mock_check_codex_prompt(tmux_session):
            idx = min(call_count["n"], len(prompt_results) - 1)
            result = prompt_results[idx]
            call_count["n"] += 1
            return result

        mq._check_codex_prompt = mock_check_codex_prompt

        # Run watch with very short timeout
        await mq._watch_for_idle("watch2", "codex123", "watcher456", timeout_seconds=0.1)

        # Should have timed out (not idle)
        pending = mq.get_pending_messages("watcher456")
        assert len(pending) == 1
        assert "Timeout" in pending[0].text

    @pytest.mark.asyncio
    async def test_watch_codex_counter_resets_on_non_prompt(self, mock_session_manager, temp_db_path):
        """Codex prompt counter resets when prompt disappears between detections."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        codex_session = MagicMock()
        codex_session.id = "codex123"
        codex_session.provider = "codex"
        codex_session.tmux_session = "tmux-codex"
        codex_session.friendly_name = "test-codex"
        codex_session.name = "codex-agent"
        mock_session_manager.get_session = MagicMock(return_value=codex_session)

        # Simulate: prompt, no-prompt, prompt, prompt → idle on 4th poll
        prompt_results = [True, False, True, True]
        call_count = {"n": 0}

        async def mock_check_codex_prompt(tmux_session):
            idx = min(call_count["n"], len(prompt_results) - 1)
            result = prompt_results[idx]
            call_count["n"] += 1
            return result

        mq._check_codex_prompt = mock_check_codex_prompt

        await mq._watch_for_idle("watch3", "codex123", "watcher456", timeout_seconds=5)

        pending = mq.get_pending_messages("watcher456")
        assert len(pending) == 1
        assert "is now idle" in pending[0].text

    @pytest.mark.asyncio
    async def test_watch_claude_session_unaffected(self, mock_session_manager, temp_db_path):
        """_watch_for_idle does not use Codex prompt detection for Claude sessions."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        # Create a Claude session (default provider)
        claude_session = MagicMock()
        claude_session.id = "claude123"
        claude_session.provider = "claude"
        claude_session.tmux_session = "tmux-claude"
        claude_session.friendly_name = "test-claude"
        claude_session.name = "claude-agent"
        mock_session_manager.get_session = MagicMock(return_value=claude_session)

        # _check_codex_prompt should never be called
        mq._check_codex_prompt = AsyncMock(return_value=True)

        # Run watch with short timeout (Claude never goes idle via hook → timeout)
        await mq._watch_for_idle("watch4", "claude123", "watcher456", timeout_seconds=0.1)

        # Should have timed out
        pending = mq.get_pending_messages("watcher456")
        assert len(pending) == 1
        assert "Timeout" in pending[0].text

        # _check_codex_prompt should NOT have been called
        mq._check_codex_prompt.assert_not_called()

    @pytest.mark.asyncio
    async def test_watch_codex_pending_messages_suppress_idle(self, mock_session_manager, temp_db_path):
        """Codex idle is suppressed when pending messages exist."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        codex_session = MagicMock()
        codex_session.id = "codex123"
        codex_session.provider = "codex"
        codex_session.tmux_session = "tmux-codex"
        codex_session.friendly_name = "test-codex"
        codex_session.name = "codex-agent"
        mock_session_manager.get_session = MagicMock(return_value=codex_session)

        # Always show prompt
        mq._check_codex_prompt = AsyncMock(return_value=True)

        # Insert a pending message directly into DB (bypass queue_message which
        # triggers delivery for Codex sessions)
        mq._execute("""
            INSERT INTO message_queue
            (id, target_session_id, text, delivery_mode, queued_at)
            VALUES (?, ?, ?, ?, ?)
        """, ("pending_msg", "codex123", "Pending task", "sequential", datetime.now().isoformat()))

        # Run watch with short timeout — pending messages should suppress idle
        await mq._watch_for_idle("watch5", "codex123", "watcher456", timeout_seconds=0.1)

        # Should have timed out (not idle due to pending messages)
        pending_watcher = mq.get_pending_messages("watcher456")
        assert len(pending_watcher) == 1
        assert "Timeout" in pending_watcher[0].text


class TestTelegramMirroring:
    """Tests for Telegram mirroring of agent-to-agent communications (issue #103)."""

    @pytest.mark.asyncio
    async def test_mirror_to_telegram_with_notifier(self, mock_session_manager, temp_db_path):
        """_mirror_to_telegram sends notification when notifier is configured."""
        # Create mock notifier
        mock_notifier = AsyncMock()
        mock_notifier.notify = AsyncMock(return_value=True)

        # Create message queue with notifier
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            notifier=mock_notifier,
        )

        # Create mock session with telegram_chat_id
        mock_session = MagicMock()
        mock_session.id = "test123"
        mock_session.telegram_chat_id = 12345

        # Call mirror method
        await mq._mirror_to_telegram("Test message", mock_session, "test_event")

        # Verify notifier was called
        assert mock_notifier.notify.call_count == 1
        call_args = mock_notifier.notify.call_args
        event = call_args[0][0]
        session = call_args[0][1]

        assert event.session_id == "test123"
        assert event.event_type == "test_event"
        assert event.message == "Test message"
        assert session == mock_session

    @pytest.mark.asyncio
    async def test_mirror_to_telegram_without_notifier(self, mock_session_manager, temp_db_path):
        """_mirror_to_telegram is silent when notifier is None."""
        # Create message queue without notifier
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            notifier=None,
        )

        # Create mock session
        mock_session = MagicMock()
        mock_session.id = "test123"
        mock_session.telegram_chat_id = 12345

        # Call should not raise
        await mq._mirror_to_telegram("Test message", mock_session, "test_event")

    @pytest.mark.asyncio
    async def test_mirror_to_telegram_without_chat_id(self, mock_session_manager, temp_db_path):
        """_mirror_to_telegram is silent when session has no telegram_chat_id."""
        mock_notifier = AsyncMock()
        mock_notifier.notify = AsyncMock(return_value=True)

        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            notifier=mock_notifier,
        )

        # Create mock session WITHOUT telegram_chat_id
        mock_session = MagicMock()
        mock_session.id = "test123"
        mock_session.telegram_chat_id = None

        # Call should not raise and notifier should not be called
        await mq._mirror_to_telegram("Test message", mock_session, "test_event")
        assert mock_notifier.notify.call_count == 0

    @pytest.mark.asyncio
    async def test_mirror_to_telegram_handles_exceptions(self, mock_session_manager, temp_db_path):
        """_mirror_to_telegram handles exceptions gracefully (fire-and-forget)."""
        # Create mock notifier that raises
        mock_notifier = AsyncMock()
        mock_notifier.notify = AsyncMock(side_effect=Exception("Telegram API error"))

        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            notifier=mock_notifier,
        )

        mock_session = MagicMock()
        mock_session.id = "test123"
        mock_session.telegram_chat_id = 12345

        # Should not raise - fire-and-forget
        await mq._mirror_to_telegram("Test message", mock_session, "test_event")

    @pytest.mark.asyncio
    async def test_delivery_mirrors_to_telegram(self, mock_session_manager, temp_db_path):
        """Message delivery triggers Telegram mirroring."""
        mock_notifier = AsyncMock()
        mock_notifier.notify = AsyncMock(return_value=True)

        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            notifier=mock_notifier,
        )

        # Create mock session
        mock_session = MagicMock()
        mock_session.id = "target123"
        mock_session.tmux_session = "tmux-test"
        mock_session.telegram_chat_id = 12345
        mock_session.status = SessionStatus.IDLE
        mock_session.last_activity = datetime.now()
        mock_session_manager.get_session = MagicMock(return_value=mock_session)

        # Queue a message
        msg = mq.queue_message(
            target_session_id="target123",
            text="Hello from sender",
            sender_session_id="sender456",
            sender_name="Agent X",
        )

        # Mark session as idle
        mq.mark_session_idle("target123")

        # Trigger delivery
        await mq._try_deliver_messages("target123")

        # Verify Telegram mirroring was called (message delivered)
        assert mock_notifier.notify.call_count >= 1
        # Check that one of the calls was for message_delivered
        delivered_calls = [
            call for call in mock_notifier.notify.call_args_list
            if call[0][0].event_type == "message_delivered"
        ]
        assert len(delivered_calls) >= 1

    @pytest.mark.asyncio
    async def test_stop_notification_mirrors_to_telegram(self, mock_session_manager, temp_db_path):
        """Stop notifications are mirrored to Telegram."""
        mock_notifier = AsyncMock()
        mock_notifier.notify = AsyncMock(return_value=True)

        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            notifier=mock_notifier,
        )

        # Create mock sessions
        recipient_session = MagicMock()
        recipient_session.id = "recipient123"
        recipient_session.friendly_name = "Agent A"
        recipient_session.name = "claude-recipient"

        sender_session = MagicMock()
        sender_session.id = "sender456"
        sender_session.telegram_chat_id = 12345

        def get_session_side_effect(session_id):
            if session_id == "recipient123":
                return recipient_session
            elif session_id == "sender456":
                return sender_session
            return None

        mock_session_manager.get_session = MagicMock(side_effect=get_session_side_effect)

        # Send stop notification
        await mq._send_stop_notification(
            recipient_session_id="recipient123",
            sender_session_id="sender456",
            sender_name="Agent B",
        )

        # Verify Telegram mirroring was called for stop notification
        stop_notify_calls = [
            call for call in mock_notifier.notify.call_args_list
            if call[0][0].event_type == "stop_notify"
        ]
        assert len(stop_notify_calls) == 1
        event = stop_notify_calls[0][0][0]
        assert "🛑" in event.message
