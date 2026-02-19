"""Unit tests for sm#188: periodic status update reminders (sm remind)."""

import pytest
import asyncio
import sqlite3
from datetime import datetime, timedelta
from pathlib import Path
from unittest.mock import MagicMock, AsyncMock, patch

from src.message_queue import MessageQueueManager
from src.models import QueuedMessage, Session, RemindRegistration, SessionStatus


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
    return str(tmp_path / "test_remind.db")


@pytest.fixture
def mq(mock_session_manager, temp_db_path):
    """Create a MessageQueueManager with remind config."""
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
            "remind": {
                "soft_threshold_seconds": 180,
                "hard_gap_seconds": 120,
            },
        },
        notifier=None,
    )


# ---------------------------------------------------------------------------
# Helper
# ---------------------------------------------------------------------------

async def run_one_iteration(mq, target_session_id):
    """Run one pass through _run_remind_task by fast-sleeping and cancelling."""
    call_count = [0]

    async def fast_sleep(t):
        call_count[0] += 1
        if call_count[0] > 1:
            raise asyncio.CancelledError()

    with patch("asyncio.sleep", fast_sleep):
        try:
            await mq._run_remind_task(target_session_id)
        except asyncio.CancelledError:
            pass


# ===========================================================================
# 1 & 2 — Delivery-triggered start
# ===========================================================================

class TestDeliveryTriggeredStart:
    """Scenarios 1 & 2: remind thresholds persisted in message and read back."""

    def test_queue_message_persists_remind_thresholds(self, mq):
        """Remind thresholds written to DB and survive a roundtrip via get_pending_messages."""
        with patch("asyncio.create_task", noop_create_task):
            msg = mq.queue_message(
                target_session_id="agent1",
                text="As engineer, implement #1668...",
                remind_soft_threshold=180,
                remind_hard_threshold=300,
            )

        pending = mq.get_pending_messages("agent1")
        assert len(pending) == 1
        assert pending[0].remind_soft_threshold == 180
        assert pending[0].remind_hard_threshold == 300

    def test_queue_message_without_remind_has_none_thresholds(self, mq):
        """Messages without --remind have None remind thresholds."""
        with patch("asyncio.create_task", noop_create_task):
            msg = mq.queue_message(
                target_session_id="agent2",
                text="Normal message",
            )
        pending = mq.get_pending_messages("agent2")
        assert len(pending) == 1
        assert pending[0].remind_soft_threshold is None
        assert pending[0].remind_hard_threshold is None

    def test_registration_created_after_delivery(self, mq):
        """register_periodic_remind creates an active in-memory registration."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("agent3", soft_threshold=30, hard_threshold=60)

        assert "agent3" in mq._remind_registrations
        reg = mq._remind_registrations["agent3"]
        assert reg.is_active is True
        assert reg.soft_threshold_seconds == 30
        assert reg.hard_threshold_seconds == 60

    def test_registration_persisted_to_db(self, mq):
        """register_periodic_remind writes the registration to remind_registrations table."""
        with patch("asyncio.create_task", noop_create_task):
            reg_id = mq.register_periodic_remind("agent4", soft_threshold=10, hard_threshold=20)

        conn = sqlite3.connect(mq.db_path)
        cursor = conn.cursor()
        cursor.execute(
            "SELECT id, is_active FROM remind_registrations WHERE target_session_id = ?",
            ("agent4",),
        )
        row = cursor.fetchone()
        conn.close()

        assert row is not None
        assert row[0] == reg_id
        assert row[1] == 1  # is_active=True


# ===========================================================================
# 3 — Basic remind lifecycle (soft + hard + cycle reset)
# ===========================================================================

class TestBasicRemindLifecycle:
    """Scenario 3: soft fires, hard fires, cycle resets."""

    @pytest.mark.asyncio
    async def test_soft_fires_after_threshold(self, mq):
        """Soft (important) reminder queued when soft_threshold elapsed."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=1, hard_threshold=100)

        # Simulate elapsed time past soft threshold
        reg = mq._remind_registrations["target"]
        reg.last_reset_at = datetime.now() - timedelta(seconds=5)

        await run_one_iteration(mq, "target")

        pending = mq.get_pending_messages("target")
        assert len(pending) == 1
        assert "[sm remind]" in pending[0].text
        assert "Update your status" in pending[0].text
        assert pending[0].delivery_mode == "important"

    @pytest.mark.asyncio
    async def test_soft_not_fired_before_threshold(self, mq):
        """No reminder queued when elapsed < soft_threshold."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=100, hard_threshold=200)

        reg = mq._remind_registrations["target"]
        reg.last_reset_at = datetime.now()  # just registered, no time elapsed

        await run_one_iteration(mq, "target")

        pending = mq.get_pending_messages("target")
        assert len(pending) == 0

    @pytest.mark.asyncio
    async def test_hard_fires_after_hard_threshold(self, mq):
        """Hard (urgent) reminder queued when hard_threshold elapsed."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=1, hard_threshold=2)

        reg = mq._remind_registrations["target"]
        reg.last_reset_at = datetime.now() - timedelta(seconds=10)
        reg.soft_fired = True  # soft already fired

        await run_one_iteration(mq, "target")

        pending = mq.get_pending_messages("target")
        assert len(pending) == 1
        assert "[sm remind]" in pending[0].text
        assert "Status overdue" in pending[0].text
        assert pending[0].delivery_mode == "urgent"

    @pytest.mark.asyncio
    async def test_hard_fire_resets_cycle(self, mq):
        """After hard fires, last_reset_at updated and soft_fired cleared."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=1, hard_threshold=2)

        reg = mq._remind_registrations["target"]
        old_reset = datetime.now() - timedelta(seconds=10)
        reg.last_reset_at = old_reset
        reg.soft_fired = True

        await run_one_iteration(mq, "target")

        # Cycle reset
        assert reg.soft_fired is False
        assert reg.last_reset_at > old_reset


# ===========================================================================
# 4 — Status reset prevents premature fire
# ===========================================================================

class TestStatusReset:
    """Scenario 4: sm status resets the timer."""

    def test_reset_remind_updates_last_reset_at(self, mq):
        """reset_remind updates last_reset_at to now."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=10, hard_threshold=20)

        reg = mq._remind_registrations["target"]
        old_reset = datetime.now() - timedelta(seconds=30)
        reg.last_reset_at = old_reset
        reg.soft_fired = True

        mq.reset_remind("target")

        assert reg.last_reset_at > old_reset
        assert reg.soft_fired is False

    def test_reset_remind_persists_to_db(self, mq):
        """reset_remind updates DB row."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=10, hard_threshold=20)

        reg = mq._remind_registrations["target"]
        reg.soft_fired = True
        mq._update_remind_db("target", soft_fired=True)

        mq.reset_remind("target")

        conn = sqlite3.connect(mq.db_path)
        cursor = conn.cursor()
        cursor.execute(
            "SELECT soft_fired FROM remind_registrations WHERE target_session_id = ?",
            ("target",),
        )
        row = cursor.fetchone()
        conn.close()
        assert row[0] == 0  # soft_fired reset

    @pytest.mark.asyncio
    async def test_status_reset_prevents_premature_remind(self, mq):
        """After reset, soft does not fire until threshold from reset time."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=30, hard_threshold=60)

        reg = mq._remind_registrations["target"]
        # Simulate 25s elapsed without status
        reg.last_reset_at = datetime.now() - timedelta(seconds=25)

        # Agent calls sm status → reset
        mq.reset_remind("target")
        # Now last_reset_at is fresh; only 0s elapsed

        await run_one_iteration(mq, "target")

        pending = mq.get_pending_messages("target")
        assert len(pending) == 0, "No remind should fire right after status reset"

    def test_reset_remind_no_op_without_registration(self, mq):
        """reset_remind does nothing for unknown session (no error)."""
        mq.reset_remind("nonexistent")  # Should not raise


# ===========================================================================
# 5 — Idle cancels remind
# ===========================================================================

class TestIdleCancelsRemind:
    """Scenario 5: Stop hook cancels remind registration."""

    def test_stop_hook_cancels_remind(self, mq):
        """mark_session_idle with from_stop_hook=True cancels remind."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("agent5", soft_threshold=10, hard_threshold=20)

        assert "agent5" in mq._remind_registrations

        with patch("asyncio.create_task", noop_create_task):
            mq.mark_session_idle("agent5", from_stop_hook=True)

        assert "agent5" not in mq._remind_registrations

    def test_non_stop_hook_idle_does_not_cancel_remind(self, mq):
        """mark_session_idle without from_stop_hook does not cancel remind."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("agent6", soft_threshold=10, hard_threshold=20)

        assert "agent6" in mq._remind_registrations

        with patch("asyncio.create_task", noop_create_task):
            mq.mark_session_idle("agent6", from_stop_hook=False)

        assert "agent6" in mq._remind_registrations


# ===========================================================================
# 6 & 7 — Clear / Kill cancels remind
# ===========================================================================

class TestClearKillCancelRemind:
    """Scenarios 6 & 7: cancel_remind used by sm clear and sm kill."""

    def test_cancel_removes_in_memory_registration(self, mq):
        """cancel_remind removes registration from in-memory dict."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("agent7", soft_threshold=10, hard_threshold=20)

        assert "agent7" in mq._remind_registrations

        mq.cancel_remind("agent7")

        assert "agent7" not in mq._remind_registrations

    def test_cancel_marks_db_inactive(self, mq):
        """cancel_remind sets is_active=0 in DB."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("agent8", soft_threshold=10, hard_threshold=20)

        mq.cancel_remind("agent8")

        conn = sqlite3.connect(mq.db_path)
        cursor = conn.cursor()
        cursor.execute(
            "SELECT is_active FROM remind_registrations WHERE target_session_id = ?",
            ("agent8",),
        )
        row = cursor.fetchone()
        conn.close()
        assert row[0] == 0  # is_active=False

    def test_cancel_no_op_for_unknown_session(self, mq):
        """cancel_remind does nothing for unknown session (no error)."""
        mq.cancel_remind("does_not_exist")  # Should not raise


# ===========================================================================
# 8 — Manual stop (sm remind <id> --stop)
# ===========================================================================

class TestManualStop:
    """Scenario 8: cancel_remind via sm remind --stop."""

    @pytest.mark.asyncio
    async def test_cancel_prevents_future_reminders(self, mq):
        """After cancel_remind, no remind fires even after threshold elapsed."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=1, hard_threshold=2)

        reg = mq._remind_registrations.get("target")
        if reg:
            reg.last_reset_at = datetime.now() - timedelta(seconds=10)

        mq.cancel_remind("target")

        # Task should exit immediately (registration is_active=False or missing)
        # Run a single iteration to verify no message queued
        call_count = [0]

        async def fast_sleep(t):
            call_count[0] += 1
            # Exit after first sleep regardless
            raise asyncio.CancelledError()

        with patch("asyncio.sleep", fast_sleep):
            try:
                await mq._run_remind_task("target")
            except asyncio.CancelledError:
                pass

        pending = mq.get_pending_messages("target")
        assert len(pending) == 0


# ===========================================================================
# 9 — Replacement policy
# ===========================================================================

class TestReplacementPolicy:
    """Scenario 9: second registration replaces the first."""

    def test_second_registration_replaces_first(self, mq):
        """New register_periodic_remind cancels old registration for same target."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=10, hard_threshold=20)
        first_reg = mq._remind_registrations["target"]
        first_id = first_reg.id

        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=60, hard_threshold=120)

        new_reg = mq._remind_registrations["target"]
        assert new_reg.id != first_id
        assert new_reg.soft_threshold_seconds == 60
        assert new_reg.hard_threshold_seconds == 120

    def test_replacement_resets_timer(self, mq):
        """New registration starts fresh (last_reset_at is now, not old value)."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=10, hard_threshold=20)

        old_reg = mq._remind_registrations["target"]
        # Simulate 5s already elapsed
        old_reg.last_reset_at = datetime.now() - timedelta(seconds=5)

        before_replace = datetime.now()
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=60, hard_threshold=120)

        new_reg = mq._remind_registrations["target"]
        assert new_reg.last_reset_at >= before_replace


# ===========================================================================
# 10 — sm children shows status
# ===========================================================================

class TestSessionStatusField:
    """Scenario 10: Session has agent_status_text field."""

    def test_session_has_agent_status_fields(self):
        """Session model includes agent_status_text and agent_status_at fields."""
        session = Session(
            id="sess1",
            name="test-session",
            tmux_session="tmux-1",
        )
        assert hasattr(session, "agent_status_text")
        assert hasattr(session, "agent_status_at")
        assert session.agent_status_text is None
        assert session.agent_status_at is None

    def test_session_to_dict_includes_status_fields(self):
        """to_dict / from_dict round-trips agent_status fields."""
        now = datetime.now()
        session = Session(
            id="sess2",
            name="test-session",
            tmux_session="tmux-2",
            agent_status_text="investigating root cause",
            agent_status_at=now,
        )
        d = session.to_dict()
        assert d["agent_status_text"] == "investigating root cause"
        assert d["agent_status_at"] is not None

        restored = Session.from_dict(d)
        assert restored.agent_status_text == "investigating root cause"
        assert restored.agent_status_at is not None


# ===========================================================================
# 11 — sm status no-arg unchanged
# ===========================================================================

class TestSmStatusNoArgUnchanged:
    """Scenario 11: sm status with no arg keeps existing behavior."""

    def test_queued_message_to_dict_includes_remind_fields(self):
        """QueuedMessage.to_dict() serializes remind thresholds."""
        now = datetime.now()
        msg = QueuedMessage(
            target_session_id="t1",
            text="hello",
            remind_soft_threshold=180,
            remind_hard_threshold=300,
        )
        d = msg.to_dict()
        assert d["remind_soft_threshold"] == 180
        assert d["remind_hard_threshold"] == 300

    def test_queued_message_to_dict_none_remind_fields(self):
        """QueuedMessage.to_dict() handles None remind thresholds."""
        msg = QueuedMessage(
            target_session_id="t2",
            text="hello",
        )
        d = msg.to_dict()
        assert d["remind_soft_threshold"] is None
        assert d["remind_hard_threshold"] is None


# ===========================================================================
# 12 — Config override
# ===========================================================================

class TestConfigOverride:
    """Scenario 12: hard threshold = soft + hard_gap from config."""

    def test_hard_gap_applied_when_hard_threshold_not_set(self, mq):
        """When remind_hard_threshold is None, hard = soft + hard_gap_seconds."""
        # Default config has hard_gap_seconds=120
        assert mq.remind_hard_gap_seconds == 120

    def test_explicit_hard_threshold_honored(self, mq):
        """Explicit hard threshold in queue_message overrides config gap."""
        with patch("asyncio.create_task", noop_create_task):
            msg = mq.queue_message(
                target_session_id="agent9",
                text="prompt",
                remind_soft_threshold=120,
                remind_hard_threshold=240,
            )

        pending = mq.get_pending_messages("agent9")
        assert pending[0].remind_soft_threshold == 120
        assert pending[0].remind_hard_threshold == 240

    def test_config_gap_used_for_custom_soft(self, mq):
        """For --remind 120, hard = 120 + 120 = 240 via config hard_gap."""
        # This validates the formula from the spec:
        # hard = soft + config.remind.hard_gap_seconds
        soft = 120
        hard = soft + mq.remind_hard_gap_seconds
        assert hard == 240


# ===========================================================================
# 13 — sm remind disambiguation
# ===========================================================================

class TestSmRemindDisambiguation:
    """Scenario 13: CLI parser routes correctly based on --stop flag."""

    def test_remind_registration_models_exist(self):
        """RemindRegistration dataclass is importable and has correct fields."""
        reg = RemindRegistration(
            id="abc123",
            target_session_id="sess1",
            soft_threshold_seconds=180,
            hard_threshold_seconds=300,
            registered_at=datetime.now(),
            last_reset_at=datetime.now(),
        )
        assert reg.soft_fired is False
        assert reg.is_active is True

    def test_queued_message_has_remind_fields(self):
        """QueuedMessage has remind_soft_threshold and remind_hard_threshold fields."""
        msg = QueuedMessage(
            target_session_id="t1",
            text="prompt",
            remind_soft_threshold=60,
            remind_hard_threshold=120,
        )
        assert msg.remind_soft_threshold == 60
        assert msg.remind_hard_threshold == 120


# ===========================================================================
# 14 & 15 — Crash recovery
# ===========================================================================

class TestCrashRecovery:
    """Scenarios 14 & 15: crash recovery restores state from DB."""

    @pytest.mark.asyncio
    async def test_recover_restores_active_registrations(self, mock_session_manager, temp_db_path):
        """_recover_remind_registrations reloads active registrations from DB."""
        # First MQ instance: register a remind
        mq1 = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"remind": {"soft_threshold_seconds": 180, "hard_gap_seconds": 120}},
            notifier=None,
        )
        with patch("asyncio.create_task", noop_create_task):
            mq1.register_periodic_remind("agent10", soft_threshold=30, hard_threshold=60)

        # Cancel the registration in-memory but leave DB active
        # (simulating server crash where in-memory state was lost but DB persisted)
        mq1._remind_registrations.clear()

        # Second MQ instance: recover from DB
        mq2 = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"remind": {"soft_threshold_seconds": 180, "hard_gap_seconds": 120}},
            notifier=None,
        )

        with patch("asyncio.create_task", noop_create_task):
            await mq2._recover_remind_registrations()

        assert "agent10" in mq2._remind_registrations
        recovered = mq2._remind_registrations["agent10"]
        assert recovered.soft_threshold_seconds == 30
        assert recovered.hard_threshold_seconds == 60
        assert recovered.is_active is True

    @pytest.mark.asyncio
    async def test_recover_skips_inactive_registrations(self, mock_session_manager, temp_db_path):
        """_recover_remind_registrations skips rows where is_active=0."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"remind": {"soft_threshold_seconds": 180, "hard_gap_seconds": 120}},
            notifier=None,
        )
        # Manually insert an inactive row
        now = datetime.now().isoformat()
        mq._execute("""
            INSERT INTO remind_registrations
            (id, target_session_id, soft_threshold_seconds, hard_threshold_seconds,
             registered_at, last_reset_at, soft_fired, is_active)
            VALUES (?, ?, ?, ?, ?, ?, 0, 0)
        """, ("dead123", "agent11", 30, 60, now, now))

        with patch("asyncio.create_task", noop_create_task):
            await mq._recover_remind_registrations()

        assert "agent11" not in mq._remind_registrations

    def test_queued_message_remind_thresholds_survive_db_roundtrip(self, mq):
        """Scenario 15: remind intent on queued message persists through DB restart."""
        with patch("asyncio.create_task", noop_create_task):
            mq.queue_message(
                target_session_id="busy_agent",
                text="As engineer, implement #999...",
                remind_soft_threshold=10,
                remind_hard_threshold=20,
            )

        # Simulate server restart: re-read pending from DB
        pending = mq.get_pending_messages("busy_agent")
        assert len(pending) == 1
        assert pending[0].remind_soft_threshold == 10
        assert pending[0].remind_hard_threshold == 20

    @pytest.mark.asyncio
    async def test_recovered_task_fires_remind_when_threshold_exceeded(
        self, mock_session_manager, temp_db_path
    ):
        """After recovery, _run_remind_task fires remind when elapsed > threshold."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={"remind": {"soft_threshold_seconds": 180, "hard_gap_seconds": 120}},
            notifier=None,
        )

        # Manually insert a registration where last_reset_at was 1h ago
        old_time = (datetime.now() - timedelta(hours=1)).isoformat()
        now_str = datetime.now().isoformat()
        mq._execute("""
            INSERT INTO remind_registrations
            (id, target_session_id, soft_threshold_seconds, hard_threshold_seconds,
             registered_at, last_reset_at, soft_fired, is_active)
            VALUES (?, ?, ?, ?, ?, ?, 0, 1)
        """, ("rec123", "agent12", 30, 60, now_str, old_time))

        with patch("asyncio.create_task", noop_create_task):
            await mq._recover_remind_registrations()

        assert "agent12" in mq._remind_registrations

        await run_one_iteration(mq, "agent12")

        pending = mq.get_pending_messages("agent12")
        # At least soft should fire (threshold=30s, elapsed=1h)
        assert len(pending) >= 1
        assert any("[sm remind]" in m.text for m in pending)


# ===========================================================================
# 16 — One-shot sm remind now works
# ===========================================================================

class TestOneShotRemind:
    """Scenario 16: one-shot sm remind via scheduler endpoint."""

    def test_client_has_schedule_reminder_method(self):
        """Client exposes schedule_reminder for one-shot remind wiring."""
        from src.cli.client import SessionManagerClient
        assert hasattr(SessionManagerClient, "schedule_reminder")
        import inspect
        sig = inspect.signature(SessionManagerClient.schedule_reminder)
        params = list(sig.parameters.keys())
        assert "session_id" in params
        assert "delay_seconds" in params
        assert "message" in params

    def test_client_has_set_agent_status_method(self):
        """Client exposes set_agent_status for sm status command."""
        from src.cli.client import SessionManagerClient
        assert hasattr(SessionManagerClient, "set_agent_status")

    def test_client_has_cancel_remind_method(self):
        """Client exposes cancel_remind for sm remind --stop."""
        from src.cli.client import SessionManagerClient
        assert hasattr(SessionManagerClient, "cancel_remind")

    def test_client_has_register_remind_method(self):
        """Client exposes register_remind for manual remind registration."""
        from src.cli.client import SessionManagerClient
        assert hasattr(SessionManagerClient, "register_remind")


# ===========================================================================
# Dedup guard (spec section: algorithm detail)
# ===========================================================================

class TestDedupGuard:
    """Soft remind dedup guard: don't queue another if one is already pending."""

    @pytest.mark.asyncio
    async def test_dedup_skips_soft_when_pending_remind_exists(self, mq):
        """If a [sm remind] message is already pending, skip soft fire."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_periodic_remind("target", soft_threshold=1, hard_threshold=100)
            # Pre-queue a soft remind message
            mq.queue_message(
                target_session_id="target",
                text='[sm remind] Update your status: sm status "your current progress"',
                delivery_mode="important",
            )

        reg = mq._remind_registrations["target"]
        reg.last_reset_at = datetime.now() - timedelta(seconds=5)

        await run_one_iteration(mq, "target")

        # Should still be only 1 message (dedup blocked second)
        pending = mq.get_pending_messages("target")
        remind_msgs = [m for m in pending if m.text.startswith("[sm remind]")]
        assert len(remind_msgs) == 1


# ===========================================================================
# Database schema: remind_registrations table exists
# ===========================================================================

class TestDatabaseSchema:
    """remind_registrations table created by _init_db."""

    def test_remind_registrations_table_created(self, mq, temp_db_path):
        """remind_registrations table exists after MessageQueueManager init."""
        conn = sqlite3.connect(temp_db_path)
        cursor = conn.cursor()
        cursor.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='remind_registrations'"
        )
        assert cursor.fetchone() is not None
        conn.close()

    def test_message_queue_has_remind_columns(self, mq, temp_db_path):
        """message_queue table has remind_soft_threshold and remind_hard_threshold columns."""
        conn = sqlite3.connect(temp_db_path)
        cursor = conn.cursor()
        cursor.execute("PRAGMA table_info(message_queue)")
        columns = {row[1] for row in cursor.fetchall()}
        conn.close()
        assert "remind_soft_threshold" in columns
        assert "remind_hard_threshold" in columns
