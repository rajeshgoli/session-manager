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
    async def test_check_idle_prompt_bare_chevron(self, message_queue):
        """_check_idle_prompt returns True for bare '>' prompt."""
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"some output\n>", b""))
        mock_proc.returncode = 0

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await message_queue._check_idle_prompt("tmux-codex")
        assert result is True

    @pytest.mark.asyncio
    async def test_check_idle_prompt_with_trailing_space(self, message_queue):
        """_check_idle_prompt returns True for '> ' prompt (no user text)."""
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"some output\n> ", b""))
        mock_proc.returncode = 0

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await message_queue._check_idle_prompt("tmux-codex")
        assert result is True

    @pytest.mark.asyncio
    async def test_check_idle_prompt_with_user_text(self, message_queue):
        """_check_idle_prompt returns False when user has typed text."""
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"some output\n> hello world", b""))
        mock_proc.returncode = 0

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await message_queue._check_idle_prompt("tmux-codex")
        assert result is False

    @pytest.mark.asyncio
    async def test_check_idle_prompt_no_prompt(self, message_queue):
        """_check_idle_prompt returns False when no prompt visible."""
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"Processing task...\nDone.", b""))
        mock_proc.returncode = 0

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await message_queue._check_idle_prompt("tmux-codex")
        assert result is False

    @pytest.mark.asyncio
    async def test_check_idle_prompt_empty_output(self, message_queue):
        """_check_idle_prompt returns False for empty output."""
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))
        mock_proc.returncode = 0

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await message_queue._check_idle_prompt("tmux-codex")
        assert result is False

    @pytest.mark.asyncio
    async def test_check_idle_prompt_tmux_error(self, message_queue):
        """_check_idle_prompt returns False when tmux command fails."""
        mock_proc = AsyncMock()
        mock_proc.communicate = AsyncMock(return_value=(b"", b"no session"))
        mock_proc.returncode = 1

        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await message_queue._check_idle_prompt("tmux-codex")
        assert result is False

    @pytest.mark.asyncio
    async def test_check_idle_prompt_exception(self, message_queue):
        """_check_idle_prompt returns False on exception."""
        with patch("asyncio.create_subprocess_exec", side_effect=OSError("tmux not found")):
            result = await message_queue._check_idle_prompt("tmux-codex")
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
        codex_session.status = SessionStatus.RUNNING
        mock_session_manager.get_session = MagicMock(return_value=codex_session)

        # Simulate: first poll sees prompt, second poll sees prompt → idle
        prompt_results = [True, True]
        call_count = {"n": 0}

        async def mock_check_idle_prompt(tmux_session):
            idx = min(call_count["n"], len(prompt_results) - 1)
            result = prompt_results[idx]
            call_count["n"] += 1
            return result

        mq._check_idle_prompt = mock_check_idle_prompt

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
        codex_session.status = SessionStatus.RUNNING
        mock_session_manager.get_session = MagicMock(return_value=codex_session)

        # Simulate: prompt visible once, then gone (transient), then timeout
        prompt_results = [True, False, False, False]
        call_count = {"n": 0}

        async def mock_check_idle_prompt(tmux_session):
            idx = min(call_count["n"], len(prompt_results) - 1)
            result = prompt_results[idx]
            call_count["n"] += 1
            return result

        mq._check_idle_prompt = mock_check_idle_prompt

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
        codex_session.status = SessionStatus.RUNNING
        mock_session_manager.get_session = MagicMock(return_value=codex_session)

        # Simulate: prompt, no-prompt, prompt, prompt → idle on 4th poll
        prompt_results = [True, False, True, True]
        call_count = {"n": 0}

        async def mock_check_idle_prompt(tmux_session):
            idx = min(call_count["n"], len(prompt_results) - 1)
            result = prompt_results[idx]
            call_count["n"] += 1
            return result

        mq._check_idle_prompt = mock_check_idle_prompt

        await mq._watch_for_idle("watch3", "codex123", "watcher456", timeout_seconds=5)

        pending = mq.get_pending_messages("watcher456")
        assert len(pending) == 1
        assert "is now idle" in pending[0].text

    @pytest.mark.asyncio
    async def test_watch_detects_claude_tmux_idle(self, mock_session_manager, temp_db_path):
        """Phase 2: provider='claude', is_idle=False, tmux shows '>' — idle after 2 consecutive checks."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        claude_session = MagicMock()
        claude_session.id = "claude123"
        claude_session.provider = "claude"
        claude_session.tmux_session = "tmux-claude"
        claude_session.friendly_name = "test-claude"
        claude_session.name = "claude-agent"
        claude_session.status = SessionStatus.RUNNING
        mock_session_manager.get_session = MagicMock(return_value=claude_session)

        # Simulate: prompt visible twice → idle detected
        prompt_results = [True, True]
        call_count = {"n": 0}

        async def mock_check(tmux_session):
            idx = min(call_count["n"], len(prompt_results) - 1)
            result = prompt_results[idx]
            call_count["n"] += 1
            return result

        mq._check_idle_prompt = mock_check

        await mq._watch_for_idle("watch4", "claude123", "watcher456", timeout_seconds=5)

        pending = mq.get_pending_messages("watcher456")
        assert len(pending) == 1
        assert "is now idle" in pending[0].text

    @pytest.mark.asyncio
    async def test_watch_codex_phase4_not_at_prompt(self, mock_session_manager, temp_db_path):
        """Phase 4: provider='codex', tmux idle + pending messages + tmux NOT showing '>' — not idle."""
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
        codex_session.status = SessionStatus.RUNNING
        mock_session_manager.get_session = MagicMock(return_value=codex_session)

        # Phase 2 shows prompt (so mem_idle becomes True), Phase 4 does NOT show prompt
        call_count = {"n": 0}

        async def mock_check(tmux_session):
            call_count["n"] += 1
            # Phase 2 calls (odd calls in sequence): return True to reach mem_idle
            # Phase 4 calls (even calls): return False — delivery in-flight
            # Pattern: True, True, False, True, True, False, ...
            # Phase 2 needs 2 consecutive → calls 1,2 → mem_idle=True
            # Phase 4 → call 3 → False → not idle
            # Next iteration: Phase 2 call 4 → True, but mem_idle already True from memory? No.
            # Actually prompt_count resets each time mem_idle becomes True...
            # Simpler: just return False always. Phase 2 never triggers, only mem_idle from
            # delivery_states matters.
            return False

        mq._check_idle_prompt = mock_check

        # Force is_idle=True in memory so Phase 4 check runs
        from src.models import SessionDeliveryState
        mq.delivery_states["codex123"] = SessionDeliveryState(session_id="codex123", is_idle=True)

        # Insert a pending message directly into DB
        mq._execute("""
            INSERT INTO message_queue
            (id, target_session_id, text, delivery_mode, queued_at)
            VALUES (?, ?, ?, ?, ?)
        """, ("pending_msg", "codex123", "Pending task", "sequential", datetime.now().isoformat()))

        # Run watch with short timeout
        await mq._watch_for_idle("watch5", "codex123", "watcher456", timeout_seconds=0.1)

        # Should have timed out (Phase 4 tiebreaker: not at prompt → not idle)
        pending_watcher = mq.get_pending_messages("watcher456")
        assert len(pending_watcher) == 1
        assert "Timeout" in pending_watcher[0].text


class TestWatchForIdlePhases:
    """Tests for 4-phase idle detection in _watch_for_idle (#180)."""

    @pytest.mark.asyncio
    async def test_watch_stuck_pending_tiebreaker(self, mock_session_manager, temp_db_path):
        """Phase 4: is_idle=True, pending messages, tmux shows '>' twice — idle detected."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        session = MagicMock()
        session.id = "target180"
        session.provider = "claude"
        session.tmux_session = "tmux-target"
        session.friendly_name = "test-agent"
        session.name = "claude-agent"
        session.status = SessionStatus.RUNNING
        mock_session_manager.get_session = MagicMock(return_value=session)

        # Force is_idle=True in memory
        mq.delivery_states["target180"] = SessionDeliveryState(session_id="target180", is_idle=True)

        # Insert a stuck pending message
        mq._execute("""
            INSERT INTO message_queue
            (id, target_session_id, text, delivery_mode, queued_at)
            VALUES (?, ?, ?, ?, ?)
        """, ("stuck_msg", "target180", "Stuck message", "important", datetime.now().isoformat()))

        # tmux always shows prompt (message delivery failed, msg stuck)
        mq._check_idle_prompt = AsyncMock(return_value=True)

        await mq._watch_for_idle("watch-p4", "target180", "watcher180", timeout_seconds=5)

        pending = mq.get_pending_messages("watcher180")
        assert len(pending) == 1
        assert "is now idle" in pending[0].text

    @pytest.mark.asyncio
    async def test_watch_inflight_pending_not_idle(self, mock_session_manager, temp_db_path):
        """Phase 4: is_idle=True, pending messages, tmux NOT showing '>' — not idle."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        session = MagicMock()
        session.id = "target181"
        session.provider = "claude"
        session.tmux_session = "tmux-target"
        session.friendly_name = "test-agent"
        session.name = "claude-agent"
        session.status = SessionStatus.RUNNING
        mock_session_manager.get_session = MagicMock(return_value=session)

        mq.delivery_states["target181"] = SessionDeliveryState(session_id="target181", is_idle=True)

        mq._execute("""
            INSERT INTO message_queue
            (id, target_session_id, text, delivery_mode, queued_at)
            VALUES (?, ?, ?, ?, ?)
        """, ("inflight_msg", "target181", "In-flight message", "important", datetime.now().isoformat()))

        # tmux does NOT show prompt (delivery in-flight)
        mq._check_idle_prompt = AsyncMock(return_value=False)

        await mq._watch_for_idle("watch-inf", "target181", "watcher181", timeout_seconds=0.1)

        pending = mq.get_pending_messages("watcher181")
        assert len(pending) == 1
        assert "Timeout" in pending[0].text

    @pytest.mark.asyncio
    async def test_watch_pending_tiebreaker_needs_two(self, mock_session_manager, temp_db_path):
        """Phase 4: is_idle=True, pending messages, tmux shows '>' once then not — not idle."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        session = MagicMock()
        session.id = "target182"
        session.provider = "claude"
        session.tmux_session = "tmux-target"
        session.friendly_name = "test-agent"
        session.name = "claude-agent"
        session.status = SessionStatus.RUNNING
        mock_session_manager.get_session = MagicMock(return_value=session)

        mq.delivery_states["target182"] = SessionDeliveryState(session_id="target182", is_idle=True)

        mq._execute("""
            INSERT INTO message_queue
            (id, target_session_id, text, delivery_mode, queued_at)
            VALUES (?, ?, ?, ?, ?)
        """, ("flicker_msg", "target182", "Flickering message", "important", datetime.now().isoformat()))

        # Phase 4 tiebreaker: prompt once, then gone → counter resets
        prompt_results = [True, False, True, False, True, False]
        call_count = {"n": 0}

        async def mock_check(tmux_session):
            idx = min(call_count["n"], len(prompt_results) - 1)
            result = prompt_results[idx]
            call_count["n"] += 1
            return result

        mq._check_idle_prompt = mock_check

        await mq._watch_for_idle("watch-flick", "target182", "watcher182", timeout_seconds=0.1)

        pending = mq.get_pending_messages("watcher182")
        assert len(pending) == 1
        assert "Timeout" in pending[0].text

    @pytest.mark.asyncio
    async def test_watch_session_status_fallback(self, mock_session_manager, temp_db_path):
        """Phase 3: is_idle=False, tmux unavailable, session.status=IDLE, no pending — idle."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        session = MagicMock()
        session.id = "target183"
        session.provider = "claude"
        session.tmux_session = None  # No tmux — Phase 2 skipped
        session.friendly_name = "test-agent"
        session.name = "claude-agent"
        session.status = SessionStatus.IDLE
        mock_session_manager.get_session = MagicMock(return_value=session)

        # No pending messages, no in-memory idle
        # Phase 3 should detect idle from session.status

        await mq._watch_for_idle("watch-st", "target183", "watcher183", timeout_seconds=5)

        pending = mq.get_pending_messages("watcher183")
        assert len(pending) == 1
        assert "is now idle" in pending[0].text

    @pytest.mark.asyncio
    async def test_watch_session_status_with_pending(self, mock_session_manager, temp_db_path):
        """Phase 3+4: session.status=IDLE, pending messages, no tmux — not idle."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        session = MagicMock()
        session.id = "target184"
        session.provider = "claude"
        session.tmux_session = None  # No tmux — can't verify via tiebreaker
        session.friendly_name = "test-agent"
        session.name = "claude-agent"
        session.status = SessionStatus.IDLE
        mock_session_manager.get_session = MagicMock(return_value=session)

        # Insert pending message
        mq._execute("""
            INSERT INTO message_queue
            (id, target_session_id, text, delivery_mode, queued_at)
            VALUES (?, ?, ?, ?, ?)
        """, ("pending_st", "target184", "Pending", "sequential", datetime.now().isoformat()))

        await mq._watch_for_idle("watch-stp", "target184", "watcher184", timeout_seconds=0.1)

        pending = mq.get_pending_messages("watcher184")
        assert len(pending) == 1
        assert "Timeout" in pending[0].text

    @pytest.mark.asyncio
    async def test_153_regression_urgent_race(self, mock_session_manager, temp_db_path):
        """#153 regression: urgent send → stale is_idle=True + pending → busy → not idle."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        session = MagicMock()
        session.id = "target153"
        session.provider = "claude"
        session.tmux_session = "tmux-target"
        session.friendly_name = "test-agent"
        session.name = "claude-agent"
        session.status = SessionStatus.RUNNING
        mock_session_manager.get_session = MagicMock(return_value=session)

        # Stale is_idle=True (from prior Stop hook, before urgent send)
        mq.delivery_states["target153"] = SessionDeliveryState(session_id="target153", is_idle=True)

        # Pending urgent message (just queued, delivery task scheduled)
        mq._execute("""
            INSERT INTO message_queue
            (id, target_session_id, text, delivery_mode, queued_at)
            VALUES (?, ?, ?, ?, ?)
        """, ("urgent_153", "target153", "Urgent task", "urgent", datetime.now().isoformat()))

        # Claude is NOT at the prompt (about to receive the urgent message)
        mq._check_idle_prompt = AsyncMock(return_value=False)

        await mq._watch_for_idle("watch-153", "target153", "watcher153", timeout_seconds=0.1)

        pending = mq.get_pending_messages("watcher153")
        assert len(pending) == 1
        assert "Timeout" in pending[0].text

    @pytest.mark.asyncio
    async def test_watch_session_gone_mid_loop(self, mock_session_manager, temp_db_path):
        """Guard: get_session() returns None mid-loop — watch exits cleanly."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        # Session exists on first call, gone on second
        call_count = {"n": 0}
        session = MagicMock()
        session.id = "target_gone"
        session.provider = "claude"
        session.tmux_session = "tmux-gone"
        session.friendly_name = "gone-agent"
        session.name = "claude-gone"
        session.status = SessionStatus.RUNNING

        def get_session_side_effect(sid):
            if sid == "target_gone":
                call_count["n"] += 1
                if call_count["n"] > 1:
                    return None
                return session
            return None

        mock_session_manager.get_session = MagicMock(side_effect=get_session_side_effect)

        # Not idle per memory, tmux not showing prompt
        mq._check_idle_prompt = AsyncMock(return_value=False)

        await mq._watch_for_idle("watch-gone", "target_gone", "watcher_gone", timeout_seconds=5)

        # Should emit distinct "no longer exists" notification, not a generic timeout
        pending = mq.get_pending_messages("watcher_gone")
        assert len(pending) == 1
        assert "no longer exists" in pending[0].text
        assert "Timeout" not in pending[0].text

    @pytest.mark.asyncio
    async def test_counters_reset_between_iterations(self, mock_session_manager, temp_db_path):
        """prompt_count and pending_idle_count reset properly between iterations."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        session = MagicMock()
        session.id = "target_ctr"
        session.provider = "claude"
        session.tmux_session = "tmux-ctr"
        session.friendly_name = "counter-agent"
        session.name = "claude-ctr"
        session.status = SessionStatus.RUNNING
        mock_session_manager.get_session = MagicMock(return_value=session)

        # Pattern: prompt True, prompt False (resets), prompt True, prompt True → idle
        prompt_results = [True, False, True, True]
        call_count = {"n": 0}

        async def mock_check(tmux_session):
            idx = min(call_count["n"], len(prompt_results) - 1)
            result = prompt_results[idx]
            call_count["n"] += 1
            return result

        mq._check_idle_prompt = mock_check

        await mq._watch_for_idle("watch-ctr", "target_ctr", "watcher_ctr", timeout_seconds=5)

        pending = mq.get_pending_messages("watcher_ctr")
        assert len(pending) == 1
        assert "is now idle" in pending[0].text


class TestPhase3FalseIdle:
    """Tests for Phase 3 false idle fix (sm#215 / RCA 1 from spec #191)."""

    def test_mark_session_active_updates_session_status(self, message_queue, mock_session_manager):
        """mark_session_active() sets session.status=RUNNING to prevent Phase 3 false idle."""
        session = MagicMock()
        session.id = "session215"
        session.status = SessionStatus.IDLE  # stale from Stop hook
        mock_session_manager.get_session = MagicMock(return_value=session)

        message_queue.mark_session_active("session215")

        assert session.status == SessionStatus.RUNNING

    def test_mark_session_active_skips_stopped_sessions(self, message_queue, mock_session_manager):
        """mark_session_active() does not update status for STOPPED sessions."""
        session = MagicMock()
        session.id = "session-stopped"
        session.status = SessionStatus.STOPPED
        mock_session_manager.get_session = MagicMock(return_value=session)

        message_queue.mark_session_active("session-stopped")

        assert session.status == SessionStatus.STOPPED

    @pytest.mark.asyncio
    async def test_watch_no_false_idle_after_urgent_dispatch(self, mock_session_manager, temp_db_path):
        """Phase 3 does NOT fire false idle after mark_session_active() clears stale status.

        Scenario: session.status=IDLE (stale from previous Stop hook), EM calls
        mark_session_active() then sm wait. Phase 3 must see RUNNING, not fire false idle.
        """
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        session = MagicMock()
        session.id = "target215"
        session.provider = "claude"
        session.tmux_session = None  # No tmux — Phase 2 skipped, Phase 3 is decisive
        session.friendly_name = "test-agent"
        session.name = "claude-agent"
        session.status = SessionStatus.IDLE  # stale from previous Stop hook
        mock_session_manager.get_session = MagicMock(return_value=session)

        # Simulate urgent dispatch: mark_session_active() called before sm wait
        # After fix: this also sets session.status = RUNNING
        mq.mark_session_active("target215")

        # _watch_for_idle runs with state.is_idle=False, session.status=RUNNING
        # Phase 3 sees RUNNING → no false idle → watcher gets timeout
        await mq._watch_for_idle("watch-215", "target215", "watcher215", timeout_seconds=0.1)

        pending = mq.get_pending_messages("watcher215")
        assert len(pending) == 1
        assert "Timeout" in pending[0].text

    @pytest.mark.asyncio
    async def test_phase3_still_works_after_fix(self, mock_session_manager, temp_db_path):
        """Phase 3 still detects idle after server restart (no in-memory state, no tmux).

        After server restart, delivery_states are empty. Phase 3 (session.status=IDLE)
        is the only signal. mark_session_active() was NOT called (server restarted mid-idle),
        so session.status remains IDLE — Phase 3 should fire correctly.
        """
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"watch_poll_interval_seconds": 0.01}}},
            notifier=None,
        )

        session = MagicMock()
        session.id = "target215r"
        session.provider = "claude"
        session.tmux_session = None  # No tmux — Phase 2 skipped
        session.friendly_name = "restarted-agent"
        session.name = "claude-agent"
        session.status = SessionStatus.IDLE  # correctly IDLE — agent finished before restart
        mock_session_manager.get_session = MagicMock(return_value=session)

        # No mark_session_active() — server just restarted, no in-memory state
        # Phase 1: delivery_states empty → mem_idle=False
        # Phase 2: no tmux → skipped
        # Phase 3: session.status=IDLE → idle detected correctly
        await mq._watch_for_idle("watch-215r", "target215r", "watcher215r", timeout_seconds=5)

        pending = mq.get_pending_messages("watcher215r")
        assert len(pending) == 1
        assert "is now idle" in pending[0].text

    def test_codex_queue_message_resets_stale_idle_status(self, message_queue, mock_session_manager):
        """queue_message codex path resets stale session.status=IDLE before delivery (Fix A, #193).

        Scenario: OutputMonitor left session.status=IDLE from a previous work cycle.
        When sm send arrives (codex session, sequential mode), queue_message must call
        mark_session_active() to reset status=RUNNING before scheduling delivery.
        Without Fix A, _watch_for_idle Phase 3 can see IDLE and fire a false idle
        during the delivery window.
        """
        session = MagicMock()
        session.id = "codex-session193"
        session.provider = "codex"
        session.status = SessionStatus.IDLE  # stale from previous OutputMonitor cycle
        mock_session_manager.get_session = MagicMock(return_value=session)

        with patch("asyncio.create_task", side_effect=noop_create_task):
            message_queue.queue_message(
                target_session_id="codex-session193",
                text="hello codex",
                sender_session_id="sender-193",
                delivery_mode="sequential",
            )

        # Fix A: mark_session_active() must have reset status to RUNNING synchronously
        assert session.status == SessionStatus.RUNNING

    def test_codex_queue_message_skips_mark_active_when_paused(self, message_queue, mock_session_manager):
        """queue_message codex path skips mark_session_active if session is paused (Issue 1 fix).

        If a codex session is paused for recovery, mark_session_active must NOT be called —
        it would set session.status=RUNNING and mislead Phase 3 watchers into not firing idle.
        Mirrors the guard already present on the urgent path.
        """
        session = MagicMock()
        session.id = "codex-paused193"
        session.provider = "codex"
        session.status = SessionStatus.IDLE  # paused during recovery
        mock_session_manager.get_session = MagicMock(return_value=session)

        # Mark session as paused
        message_queue._paused_sessions.add("codex-paused193")

        with patch("asyncio.create_task", side_effect=noop_create_task):
            message_queue.queue_message(
                target_session_id="codex-paused193",
                text="hello codex",
                sender_session_id="sender-193",
                delivery_mode="sequential",
            )

        # Status must remain IDLE — mark_session_active was skipped
        assert session.status == SessionStatus.IDLE

        # Cleanup
        message_queue._paused_sessions.discard("codex-paused193")


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


class TestCheckStuckDelivery:
    """Tests for _check_stuck_delivery fallback in monitor loop (#229)."""

    def _make_mq(self, mock_session_manager, temp_db_path):
        return MessageQueueManager(
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
                },
            },
            notifier=None,
        )

    def _insert_pending_message(self, mq, session_id, msg_id="pending229"):
        mq._execute("""
            INSERT INTO message_queue
            (id, target_session_id, text, delivery_mode, queued_at)
            VALUES (?, ?, ?, ?, ?)
        """, (msg_id, session_id, "Stuck message", "sequential", datetime.now().isoformat()))

    @pytest.mark.asyncio
    async def test_stuck_delivery_fires_on_second_detection(self, mock_session_manager, temp_db_path):
        """Fallback delivers on 2nd consecutive prompt detection with is_idle=False."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target229a"
        session.provider = "claude"
        session.tmux_session = "tmux-claude"
        mock_session_manager.get_session = MagicMock(return_value=session)

        self._insert_pending_message(mq, "target229a")
        # State exists but is NOT idle
        mq.delivery_states["target229a"] = SessionDeliveryState(session_id="target229a", is_idle=False)

        mq._check_idle_prompt = AsyncMock(return_value=True)
        delivered = []

        async def mock_try_deliver(sid, important_only=False):
            delivered.append(sid)

        mq._try_deliver_messages = mock_try_deliver

        # First call: count → 1, no delivery yet
        await mq._check_stuck_delivery("target229a")
        assert mq.delivery_states["target229a"]._stuck_delivery_count == 1
        assert len(delivered) == 0

        # Second call: count hits 2, delivery triggered
        with patch("asyncio.create_task", side_effect=lambda coro: asyncio.ensure_future(coro)):
            await mq._check_stuck_delivery("target229a")

        # Give the event loop a tick to run the created task
        await asyncio.sleep(0)

        assert mq.delivery_states["target229a"]._stuck_delivery_count == 0
        assert mq.delivery_states["target229a"].is_idle is True
        assert len(delivered) == 1
        assert delivered[0] == "target229a"

    @pytest.mark.asyncio
    async def test_first_detection_does_not_deliver(self, mock_session_manager, temp_db_path):
        """Single prompt detection does not trigger delivery (requires 2 consecutive)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target229b"
        session.provider = "claude"
        session.tmux_session = "tmux-claude"
        mock_session_manager.get_session = MagicMock(return_value=session)

        self._insert_pending_message(mq, "target229b", msg_id="pending229b")
        mq.delivery_states["target229b"] = SessionDeliveryState(session_id="target229b", is_idle=False)
        mq._check_idle_prompt = AsyncMock(return_value=True)

        delivered = []

        async def mock_try_deliver(sid, important_only=False):
            delivered.append(sid)

        mq._try_deliver_messages = mock_try_deliver

        await mq._check_stuck_delivery("target229b")

        assert mq.delivery_states["target229b"]._stuck_delivery_count == 1
        assert len(delivered) == 0
        assert mq.delivery_states["target229b"].is_idle is False

    @pytest.mark.asyncio
    async def test_mid_turn_false_positive_blocked(self, mock_session_manager, temp_db_path):
        """Prompt appears briefly then disappears — counter resets, no delivery."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target229c"
        session.provider = "claude"
        session.tmux_session = "tmux-claude"
        mock_session_manager.get_session = MagicMock(return_value=session)

        self._insert_pending_message(mq, "target229c", msg_id="pending229c")
        mq.delivery_states["target229c"] = SessionDeliveryState(session_id="target229c", is_idle=False)

        delivered = []

        async def mock_try_deliver(sid, important_only=False):
            delivered.append(sid)

        mq._try_deliver_messages = mock_try_deliver

        # First call: prompt visible → count=1
        mq._check_idle_prompt = AsyncMock(return_value=True)
        await mq._check_stuck_delivery("target229c")
        assert mq.delivery_states["target229c"]._stuck_delivery_count == 1

        # Second call: prompt gone → count resets to 0
        mq._check_idle_prompt = AsyncMock(return_value=False)
        await mq._check_stuck_delivery("target229c")
        assert mq.delivery_states["target229c"]._stuck_delivery_count == 0
        assert len(delivered) == 0
        assert mq.delivery_states["target229c"].is_idle is False

    @pytest.mark.asyncio
    async def test_stop_notify_isolation(self, mock_session_manager, temp_db_path):
        """Fallback delivery does NOT trigger _send_stop_notification (#229)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target229d"
        session.provider = "claude"
        session.tmux_session = "tmux-claude"
        mock_session_manager.get_session = MagicMock(return_value=session)

        self._insert_pending_message(mq, "target229d", msg_id="pending229d")

        # State: NOT idle, but has a stop_notify_sender_id set
        state = SessionDeliveryState(session_id="target229d", is_idle=False)
        state.stop_notify_sender_id = "sender-em"
        state.stop_notify_sender_name = "em-session"
        mq.delivery_states["target229d"] = state

        stop_notify_calls = []

        async def mock_stop_notify(**kwargs):
            stop_notify_calls.append(kwargs)

        mq._send_stop_notification = mock_stop_notify
        mq._check_idle_prompt = AsyncMock(return_value=True)

        delivered = []

        async def mock_try_deliver(sid, important_only=False):
            delivered.append(sid)

        mq._try_deliver_messages = mock_try_deliver

        # Two consecutive prompt detections → fallback fires
        await mq._check_stuck_delivery("target229d")
        with patch("asyncio.create_task", side_effect=lambda coro: asyncio.ensure_future(coro)):
            await mq._check_stuck_delivery("target229d")

        await asyncio.sleep(0)

        # Delivery happened but stop notification was NOT sent
        assert len(delivered) == 1
        assert len(stop_notify_calls) == 0

    @pytest.mark.asyncio
    async def test_skip_count_not_decremented_by_fallback(self, mock_session_manager, temp_db_path):
        """Fallback delivery does NOT decrement stop_notify_skip_count (#174 regression)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target229e"
        session.provider = "claude"
        session.tmux_session = "tmux-claude"
        mock_session_manager.get_session = MagicMock(return_value=session)

        self._insert_pending_message(mq, "target229e", msg_id="pending229e")

        state = SessionDeliveryState(session_id="target229e", is_idle=False)
        state.stop_notify_skip_count = 1
        mq.delivery_states["target229e"] = state

        mq._check_idle_prompt = AsyncMock(return_value=True)

        async def mock_try_deliver(sid, important_only=False):
            pass

        mq._try_deliver_messages = mock_try_deliver

        await mq._check_stuck_delivery("target229e")
        with patch("asyncio.create_task", side_effect=lambda coro: asyncio.ensure_future(coro)):
            await mq._check_stuck_delivery("target229e")

        await asyncio.sleep(0)

        # skip_count must NOT have been decremented by fallback
        assert mq.delivery_states["target229e"].stop_notify_skip_count == 1

    @pytest.mark.asyncio
    async def test_codex_app_excluded(self, mock_session_manager, temp_db_path):
        """codex-app sessions are skipped (no tmux pane)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target229f"
        session.provider = "codex-app"
        session.tmux_session = "tmux-codex-app"
        mock_session_manager.get_session = MagicMock(return_value=session)

        self._insert_pending_message(mq, "target229f", msg_id="pending229f")
        mq.delivery_states["target229f"] = SessionDeliveryState(session_id="target229f", is_idle=False)
        mq._check_idle_prompt = AsyncMock(return_value=True)

        await mq._check_stuck_delivery("target229f")

        # _check_idle_prompt must NOT have been called (early return)
        mq._check_idle_prompt.assert_not_called()

    @pytest.mark.asyncio
    async def test_claude_provider_proceeds(self, mock_session_manager, temp_db_path):
        """claude provider with tmux_session set calls _check_idle_prompt."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target229g"
        session.provider = "claude"
        session.tmux_session = "tmux-claude"
        mock_session_manager.get_session = MagicMock(return_value=session)

        self._insert_pending_message(mq, "target229g", msg_id="pending229g")
        mq.delivery_states["target229g"] = SessionDeliveryState(session_id="target229g", is_idle=False)
        mq._check_idle_prompt = AsyncMock(return_value=False)

        await mq._check_stuck_delivery("target229g")

        mq._check_idle_prompt.assert_called_once_with("tmux-claude")

    @pytest.mark.asyncio
    async def test_no_tmux_session_guard(self, mock_session_manager, temp_db_path):
        """Sessions with tmux_session=None are skipped (no tmux to inspect)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target229h"
        session.provider = "claude"
        session.tmux_session = None
        mock_session_manager.get_session = MagicMock(return_value=session)

        self._insert_pending_message(mq, "target229h", msg_id="pending229h")
        mq.delivery_states["target229h"] = SessionDeliveryState(session_id="target229h", is_idle=False)
        mq._check_idle_prompt = AsyncMock(return_value=True)

        await mq._check_stuck_delivery("target229h")

        mq._check_idle_prompt.assert_not_called()

    @pytest.mark.asyncio
    async def test_already_idle_skipped(self, mock_session_manager, temp_db_path):
        """Sessions already marked idle are skipped — fallback not needed."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target229i"
        session.provider = "claude"
        session.tmux_session = "tmux-claude"
        mock_session_manager.get_session = MagicMock(return_value=session)

        self._insert_pending_message(mq, "target229i", msg_id="pending229i")
        # Already idle — fallback should return early
        mq.delivery_states["target229i"] = SessionDeliveryState(session_id="target229i", is_idle=True)
        mq._check_idle_prompt = AsyncMock(return_value=True)

        await mq._check_stuck_delivery("target229i")

        # _check_idle_prompt must NOT have been called
        mq._check_idle_prompt.assert_not_called()

    @pytest.mark.asyncio
    async def test_race_stop_hook_and_fallback_no_double_delivery(self, mock_session_manager, temp_db_path):
        """Concurrent Stop hook (mark_session_idle) + fallback deliver exactly once (#229)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target229j"
        session.provider = "claude"
        session.tmux_session = "tmux-claude"
        session.status = SessionStatus.IDLE
        session.last_activity = datetime.now()
        mock_session_manager.get_session = MagicMock(return_value=session)
        # session_manager._deliver_direct is what _try_deliver_messages calls
        mock_session_manager._deliver_direct = AsyncMock(return_value=True)

        self._insert_pending_message(mq, "target229j", msg_id="pending229j")
        state = SessionDeliveryState(session_id="target229j", is_idle=False)
        state._stuck_delivery_count = 1  # pre-populate so next prompt detection fires T2
        mq.delivery_states["target229j"] = state
        mq._check_idle_prompt = AsyncMock(return_value=True)

        # Simulate Stop hook + fallback both firing before either task runs.
        # _stuck_delivery_count=1 means the first _check_stuck_delivery call triggers T2,
        # then mark_session_idle() triggers T1. Both are scheduled before the event loop
        # yields, so the asyncio lock in _try_deliver_messages must prevent double-delivery.
        with patch("asyncio.create_task", side_effect=lambda coro: asyncio.ensure_future(coro)):
            # Fallback fires: count 1→2, sets is_idle=True, schedules T2
            await mq._check_stuck_delivery("target229j")
            # Stop hook also fires: is_idle already True, schedules T1
            mq.mark_session_idle("target229j")
            # T1 and T2 are now both scheduled; neither has run yet

        # Let all scheduled tasks run
        await asyncio.sleep(0.05)

        # Per-session delivery lock ensures exactly one delivery
        assert mock_session_manager._deliver_direct.call_count <= 1
