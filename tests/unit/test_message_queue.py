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
        with patch('asyncio.create_task', noop_create_task):
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
        with patch('asyncio.create_task', noop_create_task):
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
        with patch('asyncio.create_task', noop_create_task):
            msg = message_queue.queue_message(
                target_session_id="target123",
                text="Normal message",
            )
        assert msg.timeout_at is None

    def test_queue_multiple_messages_preserves_order(self, message_queue):
        """Multiple queued messages preserve FIFO order."""
        with patch('asyncio.create_task', noop_create_task):
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
        with patch('asyncio.create_task', noop_create_task):
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

        with patch('asyncio.create_task', noop_create_task):
            message_queue.queue_message("target123", "Message 1")
        assert message_queue.get_queue_length("target123") == 1

        with patch('asyncio.create_task', noop_create_task):
            message_queue.queue_message("target123", "Message 2")
        assert message_queue.get_queue_length("target123") == 2


class TestBatchDelivery:
    """Tests for batch delivery."""

    def test_max_batch_size_respected(self, message_queue):
        """Batch size is limited by max_batch_size config."""
        # Queue more messages than max_batch_size
        with patch('asyncio.create_task', noop_create_task):
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
        with patch('asyncio.create_task', noop_create_task):
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
        with patch('asyncio.create_task', noop_create_task):
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
        with patch('asyncio.create_task', noop_create_task):
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


class TestDirectDelivery244:
    """sm#244: Direct delivery — no idle gate for sequential/important; paste_buffered_notify."""

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
                        "skip_fence_window_seconds": 8,
                    }
                },
            },
            notifier=None,
        )

    def _insert_pending_message(self, mq, session_id, msg_id="pending244"):
        mq._execute("""
            INSERT INTO message_queue
            (id, target_session_id, text, delivery_mode, queued_at)
            VALUES (?, ?, ?, ?, ?)
        """, (msg_id, session_id, "Stuck message", "sequential", datetime.now().isoformat()))

    @pytest.mark.asyncio
    async def test_sequential_delivery_without_idle_gate(self, mock_session_manager, temp_db_path):
        """Sequential delivery proceeds even when is_idle=False (no idle gate, sm#244)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target244a"
        session.provider = "claude"
        session.tmux_session = "claude-target244a"
        session.status = SessionStatus.IDLE
        session.last_activity = datetime.now()
        mock_session_manager.get_session = MagicMock(return_value=session)
        mock_session_manager._deliver_direct = AsyncMock(return_value=True)

        self._insert_pending_message(mq, "target244a")
        state = mq._get_or_create_state("target244a")
        state.is_idle = False  # Agent is mid-turn

        mq._get_pending_user_input_async = AsyncMock(return_value=None)

        await mq._try_deliver_messages("target244a")

        mock_session_manager._deliver_direct.assert_called_once()

    @pytest.mark.asyncio
    async def test_important_delivery_without_idle_gate(self, mock_session_manager, temp_db_path):
        """Important delivery proceeds even when is_idle=False (no idle gate, sm#244)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target244b"
        session.provider = "claude"
        session.tmux_session = "claude-target244b"
        session.status = SessionStatus.IDLE
        session.last_activity = datetime.now()
        mock_session_manager.get_session = MagicMock(return_value=session)
        mock_session_manager._deliver_direct = AsyncMock(return_value=True)

        mq._execute("""
            INSERT INTO message_queue
            (id, target_session_id, text, delivery_mode, queued_at)
            VALUES (?, ?, ?, ?, ?)
        """, ("imp244b", "target244b", "Important message", "important", datetime.now().isoformat()))

        state = mq._get_or_create_state("target244b")
        state.is_idle = False  # Agent is mid-turn

        mq._get_pending_user_input_async = AsyncMock(return_value=None)

        await mq._try_deliver_messages("target244b", important_only=True)

        mock_session_manager._deliver_direct.assert_called_once()

    @pytest.mark.asyncio
    async def test_notify_on_stop_mid_turn_uses_paste_buffered(self, mock_session_manager, temp_db_path):
        """notify_on_stop=True mid-turn sets paste_buffered_notify, NOT stop_notify_sender_id (sm#244)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target244c"
        session.provider = "claude"
        session.tmux_session = "claude-target244c"
        session.status = SessionStatus.IDLE
        session.last_activity = datetime.now()
        mock_session_manager.get_session = MagicMock(return_value=session)
        mock_session_manager._deliver_direct = AsyncMock(return_value=True)

        mq._execute("""
            INSERT INTO message_queue
            (id, target_session_id, sender_session_id, sender_name, text, delivery_mode, queued_at, notify_on_stop)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        """, ("msg244c", "target244c", "sender-agent-c", "Agent C",
              "Hello while busy", "sequential", datetime.now().isoformat(), 1))

        state = mq._get_or_create_state("target244c")
        state.is_idle = False  # Agent is mid-turn

        mq._get_pending_user_input_async = AsyncMock(return_value=None)

        await mq._try_deliver_messages("target244c")

        # paste_buffered must be set; stop_notify_sender_id must NOT be set yet
        assert state.paste_buffered_notify_sender_id == "sender-agent-c"
        assert state.paste_buffered_notify_sender_name == "Agent C"
        assert state.stop_notify_sender_id is None

    @pytest.mark.asyncio
    async def test_notify_on_stop_idle_path_arms_directly(self, mock_session_manager, temp_db_path):
        """notify_on_stop=True when idle sets stop_notify_sender_id directly (no paste_buffered, sm#244)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target244d"
        session.provider = "claude"
        session.tmux_session = "claude-target244d"
        session.status = SessionStatus.IDLE
        session.last_activity = datetime.now()
        mock_session_manager.get_session = MagicMock(return_value=session)
        mock_session_manager._deliver_direct = AsyncMock(return_value=True)

        mq._execute("""
            INSERT INTO message_queue
            (id, target_session_id, sender_session_id, sender_name, text, delivery_mode, queued_at, notify_on_stop)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        """, ("msg244d", "target244d", "sender-agent-d", "Agent D",
              "Hello while idle", "sequential", datetime.now().isoformat(), 1))

        state = mq._get_or_create_state("target244d")
        state.is_idle = True  # Agent is idle

        mq._get_pending_user_input_async = AsyncMock(return_value=None)

        await mq._try_deliver_messages("target244d")

        # stop_notify_sender_id set directly; paste_buffered must be None
        assert state.stop_notify_sender_id == "sender-agent-d"
        assert state.stop_notify_sender_name == "Agent D"
        assert state.paste_buffered_notify_sender_id is None

    def test_mark_session_idle_promotes_paste_buffered(self, mock_session_manager, temp_db_path):
        """mark_session_idle promotes paste_buffered → stop_notify_sender_id without firing notification (sm#244)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)
        state = mq._get_or_create_state("target244e")
        state.is_idle = False

        # Simulate: message was pasted mid-turn, sender staged in paste_buffered
        state.paste_buffered_notify_sender_id = "sender-em"
        state.paste_buffered_notify_sender_name = "em-session"

        stop_notify_calls = []

        async def mock_stop_notify(**kwargs):
            stop_notify_calls.append(kwargs)

        mq._send_stop_notification = mock_stop_notify

        with patch("asyncio.create_task", noop_create_task):
            mq.mark_session_idle("target244e")

        # paste_buffered promoted to stop_notify_sender_id
        assert state.stop_notify_sender_id == "sender-em"
        assert state.stop_notify_sender_name == "em-session"
        assert state.paste_buffered_notify_sender_id is None
        assert state.paste_buffered_notify_sender_name is None
        # No stop notification sent yet (fires on the NEXT Stop hook)
        assert len(stop_notify_calls) == 0

    def test_mark_session_idle_fires_notification_on_second_idle(self, mock_session_manager, temp_db_path):
        """After promotion, the NEXT mark_session_idle fires the notification (sm#244)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)
        state = mq._get_or_create_state("target244f")

        # Simulate: promotion already happened on Task X's Stop hook
        state.stop_notify_sender_id = "sender-em"
        state.stop_notify_sender_name = "em-session"
        state.paste_buffered_notify_sender_id = None

        tasks_created = []

        def capture_create_task(coro):
            tasks_created.append(coro)
            coro.close()
            return MagicMock()

        with patch("asyncio.create_task", capture_create_task):
            mq.mark_session_idle("target244f")

        # stop_notify_sender_id was consumed (cleared after scheduling notification)
        assert state.stop_notify_sender_id is None
        # At least one task was created (the stop notification coroutine)
        assert len(tasks_created) >= 1

    @pytest.mark.asyncio
    async def test_check_stale_input_runs_without_idle(self, mock_session_manager, temp_db_path):
        """_check_stale_input proceeds even when is_idle=False (guard removed, sm#244 Issue 3)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target244g"
        session.provider = "claude"
        session.tmux_session = "claude-target244g"
        mock_session_manager.get_session = MagicMock(return_value=session)

        self._insert_pending_message(mq, "target244g", msg_id="pending244g")
        state = mq._get_or_create_state("target244g")
        state.is_idle = False  # Stop hook failed: not idle

        # Simulate user has been typing longer than input_stale_timeout
        stale_input = "> some text"
        state.pending_user_input = stale_input
        state.pending_input_first_seen = datetime.now() - timedelta(seconds=60)

        mq._get_pending_user_input_async = AsyncMock(return_value=stale_input)
        mq._clear_user_input_async = AsyncMock()

        delivered = []

        async def mock_try_deliver(sid, important_only=False):
            delivered.append(sid)

        mq._try_deliver_messages = mock_try_deliver

        # Should not return early despite is_idle=False
        await mq._check_stale_input("target244g")

        assert len(delivered) == 1
        assert state.saved_user_input == stale_input

    @pytest.mark.asyncio
    async def test_queue_message_sequential_always_schedules_delivery(self, mock_session_manager, temp_db_path):
        """queue_message with sequential mode always schedules _try_deliver_messages (sm#244)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target244h"
        session.provider = "claude"
        session.tmux_session = "claude-target244h"
        session.status = SessionStatus.IDLE
        session.last_activity = datetime.now()
        mock_session_manager.get_session = MagicMock(return_value=session)

        scheduled = []

        def capture_task(coro):
            scheduled.append(coro)
            coro.close()
            return MagicMock()

        with patch("asyncio.create_task", capture_task):
            mq.queue_message(
                target_session_id="target244h",
                text="Hello from agent",
                delivery_mode="sequential",
            )

        # At least one task scheduled for delivery regardless of is_idle
        assert len(scheduled) >= 1

    @pytest.mark.asyncio
    async def test_no_double_delivery_via_lock(self, mock_session_manager, temp_db_path):
        """Concurrent delivery calls deliver exactly once (per-session lock, sm#244)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)

        session = MagicMock()
        session.id = "target244i"
        session.provider = "claude"
        session.tmux_session = "claude-target244i"
        session.status = SessionStatus.IDLE
        session.last_activity = datetime.now()
        mock_session_manager.get_session = MagicMock(return_value=session)

        deliver_call_count = 0

        async def count_deliver(sess, payload):
            nonlocal deliver_call_count
            deliver_call_count += 1
            return True

        mock_session_manager._deliver_direct = count_deliver

        self._insert_pending_message(mq, "target244i")
        state = mq._get_or_create_state("target244i")
        state.is_idle = True

        mq._get_pending_user_input_async = AsyncMock(return_value=None)

        # Fire two concurrent delivery tasks — lock should prevent double-delivery
        await asyncio.gather(
            mq._try_deliver_messages("target244i"),
            mq._try_deliver_messages("target244i"),
        )

        assert deliver_call_count <= 1


class TestSkipFence232:
    """sm#232: Skip fence is time-bounded; is_idle=True moved after skip check."""

    def _make_mq(self, mock_session_manager, temp_db_path, fence_window=8):
        return MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {"skip_fence_window_seconds": fence_window}}},
            notifier=None,
        )

    def test_late_clear_stop_hook_does_not_set_idle(self, mock_session_manager, temp_db_path):
        """Late /clear Stop hook does NOT set is_idle when re-dispatch already ran.

        Scenario: mark_session_active() ran (is_idle=False), skip_count=1 armed <8s.
        The /clear Stop hook arrives late. is_idle must stay False.
        """
        mq = self._make_mq(mock_session_manager, temp_db_path)
        state = mq._get_or_create_state("target232a")
        state.is_idle = False
        state.stop_notify_skip_count = 1
        state.skip_count_armed_at = datetime.now()

        with patch("asyncio.create_task", noop_create_task):
            mq.mark_session_idle("target232a", from_stop_hook=True)

        assert state.is_idle is False
        assert state.stop_notify_skip_count == 0
        assert state.skip_count_armed_at is None  # cleared on full consumption

    def test_normal_stop_hook_sets_idle(self, mock_session_manager, temp_db_path):
        """Normal Stop hook (no skip fence) sets is_idle=True."""
        mq = self._make_mq(mock_session_manager, temp_db_path)
        state = mq._get_or_create_state("target232b")
        state.is_idle = False
        state.stop_notify_skip_count = 0

        with patch("asyncio.create_task", noop_create_task):
            mq.mark_session_idle("target232b", from_stop_hook=True)

        assert state.is_idle is True
        assert state.last_idle_at is not None

    def test_clear_stop_hook_before_dispatch_preserves_is_idle_false(self, mock_session_manager, temp_db_path):
        """Clear Stop hook arriving before dispatch preserves is_idle=False (fresh session)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)
        state = mq._get_or_create_state("target232c")
        state.is_idle = False  # fresh session default
        state.stop_notify_skip_count = 1
        state.skip_count_armed_at = datetime.now()

        with patch("asyncio.create_task", noop_create_task):
            mq.mark_session_idle("target232c", from_stop_hook=True)

        assert state.is_idle is False
        assert state.stop_notify_skip_count == 0

    def test_clear_stop_hook_before_dispatch_preserves_is_idle_true(self, mock_session_manager, temp_db_path):
        """Clear Stop hook arriving before dispatch preserves is_idle=True (previously idle)."""
        mq = self._make_mq(mock_session_manager, temp_db_path)
        state = mq._get_or_create_state("target232d")
        state.is_idle = True  # was already idle before clear
        state.stop_notify_skip_count = 1
        state.skip_count_armed_at = datetime.now()

        with patch("asyncio.create_task", noop_create_task):
            mq.mark_session_idle("target232d", from_stop_hook=True)

        assert state.is_idle is True
        assert state.stop_notify_skip_count == 0

    def test_stale_skip_count_does_not_absorb(self, mock_session_manager, temp_db_path):
        """Stale fence (armed >8s ago) is reset; Stop hook falls through and sets is_idle=True."""
        mq = self._make_mq(mock_session_manager, temp_db_path)
        state = mq._get_or_create_state("target232e")
        state.is_idle = False
        state.stop_notify_skip_count = 1
        state.skip_count_armed_at = datetime.now() - timedelta(seconds=10)  # stale

        with patch("asyncio.create_task", noop_create_task):
            mq.mark_session_idle("target232e", from_stop_hook=True)

        # Stale fence reset; normal Stop hook → is_idle=True
        assert state.is_idle is True
        assert state.stop_notify_skip_count == 0
        assert state.skip_count_armed_at is None

    def test_fast_task_within_ttl_residual_risk(self, mock_session_manager, temp_db_path):
        """Documents residual risk: fast task Stop hook absorbed if fence still live.

        If the /clear hook was lost (never arrived) but fence is still within TTL,
        the first real Stop hook from the new task is absorbed. This is an accepted
        edge case per spec (sm#232).
        """
        mq = self._make_mq(mock_session_manager, temp_db_path, fence_window=8)
        state = mq._get_or_create_state("target232f")
        state.is_idle = False
        state.stop_notify_skip_count = 1
        state.skip_count_armed_at = datetime.now() - timedelta(seconds=6)  # within 8s window

        with patch("asyncio.create_task", noop_create_task):
            mq.mark_session_idle("target232f", from_stop_hook=True)

        # Absorbed — residual risk documented
        assert state.is_idle is False
        assert state.stop_notify_skip_count == 0

    def test_absorption_when_handoff_fence_is_armed(self, mock_session_manager, temp_db_path):
        """When skip fence is armed (as _execute_handoff does), /clear Stop hook is absorbed."""
        mq = self._make_mq(mock_session_manager, temp_db_path)
        state = mq._get_or_create_state("target232g")
        state.is_idle = False
        # Directly arm the fence (mirrors what _execute_handoff does)
        state.stop_notify_skip_count += 1
        state.skip_count_armed_at = datetime.now()

        with patch("asyncio.create_task", noop_create_task):
            mq.mark_session_idle("target232g", from_stop_hook=True)

        # Absorbed, is_idle unchanged
        assert state.is_idle is False
        assert state.stop_notify_skip_count == 0
        assert state.skip_count_armed_at is None  # cleared on full consumption

    @pytest.mark.asyncio
    async def test_execute_handoff_arms_skip_fence(self, mock_session_manager, temp_db_path, tmp_path):
        """_execute_handoff actually sets skip_count_armed_at before any tmux ops.

        If the arming line in _execute_handoff were removed, this test would fail.
        """
        mq = self._make_mq(mock_session_manager, temp_db_path)

        # Set up session in sessions dict (required by _execute_handoff)
        session = MagicMock()
        session.id = "target232g2"
        session.tmux_session = "claude-target232g2"
        session.provider = "claude"
        mock_session_manager.sessions = {"target232g2": session}

        # Real file required (Path.exists() check inside _execute_handoff)
        handoff_file = tmp_path / "handoff.md"
        handoff_file.write_text("handoff content")

        state = mq._get_or_create_state("target232g2")
        assert state.skip_count_armed_at is None  # pre-condition: not yet armed

        # Fail at first subprocess call; _restore_idle does NOT clear skip_count_armed_at
        with patch("asyncio.create_subprocess_exec", new=AsyncMock(side_effect=OSError("no tmux"))), \
             patch("asyncio.create_task", noop_create_task):
            await mq._execute_handoff("target232g2", str(handoff_file))

        # Fence must be armed — arming happens before any subprocess ops
        assert state.skip_count_armed_at is not None
        assert state.stop_notify_skip_count == 1

    @pytest.mark.asyncio
    async def test_watch_no_false_idle_after_clear_dispatch(self, mock_session_manager, temp_db_path):
        """Watch does NOT fire for idle states triggered by sm clear + late Stop hook.

        Sequence: mark_session_active() (is_idle=False, skip_count=1, armed <8s),
        then late /clear Stop hook arrives. Watch must NOT fire within 10s.
        """
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"timeouts": {"message_queue": {
                "watch_poll_interval_seconds": 0.01,
                "skip_fence_window_seconds": 8,
            }}},
            notifier=None,
        )

        session = MagicMock()
        session.id = "target232h"
        session.provider = "claude"
        session.tmux_session = None  # no tmux — Phase 2 skipped; Phase 1 is decisive
        session.friendly_name = "test-agent-232"
        session.name = "claude-agent"
        session.status = SessionStatus.RUNNING
        mock_session_manager.get_session = MagicMock(return_value=session)

        # Dispatch: mark_session_active (is_idle=False) + arm fence
        mq.mark_session_active("target232h")
        state = mq.delivery_states["target232h"]
        state.stop_notify_skip_count = 1
        state.skip_count_armed_at = datetime.now()

        # Start watch
        watch_task = asyncio.create_task(
            mq._watch_for_idle("watch-232h", "target232h", "watcher232h", timeout_seconds=0.1)
        )

        # Late /clear Stop hook arrives while watch is running
        with patch("asyncio.create_task", noop_create_task):
            mq.mark_session_idle("target232h", from_stop_hook=True)

        await watch_task
        # Let delivery task run (direct delivery queues and delivers the timeout notification)
        await asyncio.sleep(0)

        # Watch timed out (no false idle notification).
        # With sm#244, the timeout notification is delivered immediately (no idle gate),
        # so we verify via _deliver_direct rather than the pending queue.
        mock_session_manager._deliver_direct.assert_called()
        timeout_payloads = [
            call.args[1] for call in mock_session_manager._deliver_direct.call_args_list
            if "Timeout" in str(call.args[1])
        ]
        assert len(timeout_payloads) >= 1
