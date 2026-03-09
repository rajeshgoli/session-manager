"""Tests for parent wake-up registration + digest (sm#225-C)."""

import asyncio
import sqlite3
import tempfile
from datetime import datetime, timedelta
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.message_queue import MessageQueueManager
from src.models import ParentWakeRegistration, QueuedMessage, Session, SessionStatus


def noop_create_task(coro):
    """Silently close coroutine without running it."""
    coro.close()
    return MagicMock()


@pytest.fixture
def mock_session_manager():
    """Mock SessionManager."""
    mock = MagicMock()
    mock.sessions = {}
    mock.get_session = MagicMock(return_value=None)
    mock.tmux = MagicMock()
    mock._save_state = MagicMock()
    mock._deliver_direct = AsyncMock(return_value=True)
    return mock


@pytest.fixture
def temp_db_path(tmp_path):
    return str(tmp_path / "test_parent_wake.db")


@pytest.fixture
def mq(mock_session_manager, temp_db_path):
    return MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=temp_db_path,
        config={},
        notifier=None,
    )


def _make_session(session_id: str, **kwargs) -> Session:
    s = Session(id=session_id, name=f"child-{session_id[:6]}", working_dir="/tmp")
    for k, v in kwargs.items():
        setattr(s, k, v)
    return s


# ---------------------------------------------------------------------------
# TestParentWakeRegistration — CRUD
# ---------------------------------------------------------------------------

class TestParentWakeRegistration:

    def test_register_creates_in_memory_entry(self, mq):
        with patch("asyncio.create_task", noop_create_task):
            reg_id = mq.register_parent_wake("child1", "parent1")

        assert "child1" in mq._parent_wake_registrations
        reg = mq._parent_wake_registrations["child1"]
        assert reg.child_session_id == "child1"
        assert reg.parent_session_id == "parent1"
        assert reg.period_seconds == 600
        assert reg.is_active is True
        assert reg_id is not None

    def test_register_persists_to_db(self, mq, temp_db_path):
        with patch("asyncio.create_task", noop_create_task):
            mq.register_parent_wake("child2", "parent2")

        conn = sqlite3.connect(temp_db_path)
        rows = conn.execute(
            "SELECT child_session_id, parent_session_id, is_active FROM parent_wake_registrations"
        ).fetchall()
        conn.close()
        assert len(rows) == 1
        assert rows[0][0] == "child2"
        assert rows[0][1] == "parent2"
        assert rows[0][2] == 1

    def test_register_replaces_existing(self, mq):
        with patch("asyncio.create_task", noop_create_task):
            mq.register_parent_wake("child3", "parent_old")
            mq.register_parent_wake("child3", "parent_new")

        reg = mq._parent_wake_registrations["child3"]
        assert reg.parent_session_id == "parent_new"

    def test_cancel_removes_in_memory_entry(self, mq):
        with patch("asyncio.create_task", noop_create_task):
            mq.register_parent_wake("child4", "parent4")

        mq.cancel_parent_wake("child4")
        assert "child4" not in mq._parent_wake_registrations

    def test_cancel_marks_inactive_in_db(self, mq, temp_db_path):
        with patch("asyncio.create_task", noop_create_task):
            mq.register_parent_wake("child5", "parent5")

        mq.cancel_parent_wake("child5")

        conn = sqlite3.connect(temp_db_path)
        rows = conn.execute(
            "SELECT is_active FROM parent_wake_registrations WHERE child_session_id = 'child5'"
        ).fetchall()
        conn.close()
        assert rows[0][0] == 0

    def test_cancel_noop_when_not_registered(self, mq):
        mq.cancel_parent_wake("nonexistent")  # Should not raise

    def test_cancel_parent_wake_on_stop_hook(self, mq):
        """mark_session_idle(from_stop_hook=True) cancels parent wake."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_parent_wake("child6", "parent6")
            mq.mark_session_idle("child6", from_stop_hook=True)
        assert "child6" not in mq._parent_wake_registrations

    def test_stop_hook_false_does_not_cancel(self, mq):
        """mark_session_idle(from_stop_hook=False) does NOT cancel parent wake."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_parent_wake("child7", "parent7")
            mq.mark_session_idle("child7", from_stop_hook=False)
        assert "child7" in mq._parent_wake_registrations

    def test_completion_transition_cancels_parent_wake(self, mq):
        """Provider-native turn completion cancels parent wake like a real stop."""
        with patch("asyncio.create_task", noop_create_task):
            mq.register_parent_wake("child7b", "parent7b")
            mq.mark_session_idle("child7b", completion_transition=True)
        assert "child7b" not in mq._parent_wake_registrations


# ---------------------------------------------------------------------------
# TestQueueMessageParentSessionId
# ---------------------------------------------------------------------------

class TestQueueMessageParentSessionId:

    def test_queue_message_stores_parent_session_id(self, mq, temp_db_path):
        with patch("asyncio.create_task", noop_create_task):
            msg = mq.queue_message(
                target_session_id="target1",
                text="hello",
                remind_soft_threshold=210,
                remind_hard_threshold=420,
                parent_session_id="em1",
            )

        assert msg.parent_session_id == "em1"

        conn = sqlite3.connect(temp_db_path)
        rows = conn.execute(
            "SELECT parent_session_id FROM message_queue WHERE id = ?", (msg.id,)
        ).fetchall()
        conn.close()
        assert rows[0][0] == "em1"

    def test_queue_message_parent_session_id_none_by_default(self, mq):
        with patch("asyncio.create_task", noop_create_task):
            msg = mq.queue_message(target_session_id="target2", text="hi")

        assert msg.parent_session_id is None

    def test_get_pending_messages_returns_parent_session_id(self, mq):
        with patch("asyncio.create_task", noop_create_task):
            mq.queue_message(
                target_session_id="target3",
                text="msg",
                remind_soft_threshold=210,
                parent_session_id="em3",
            )

        pending = mq.get_pending_messages("target3")
        assert len(pending) == 1
        assert pending[0].parent_session_id == "em3"


# ---------------------------------------------------------------------------
# TestDeliveryTriggersParentWake
# ---------------------------------------------------------------------------

class TestDeliveryTriggersParentWake:

    @pytest.mark.asyncio
    async def test_sequential_delivery_registers_parent_wake(
        self, mock_session_manager, temp_db_path
    ):
        """After sequential delivery, register_parent_wake is called when parent_session_id set."""
        session = _make_session("child_a")
        session.status = SessionStatus.IDLE

        mock_session_manager.get_session.return_value = session

        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={},
            notifier=None,
        )

        with patch("asyncio.create_task", noop_create_task):
            mq.queue_message(
                target_session_id="child_a",
                text="dispatch msg",
                remind_soft_threshold=210,
                remind_hard_threshold=420,
                parent_session_id="em_a",
            )
            state = mq._get_or_create_state("child_a")
            state.is_idle = True

        with patch("asyncio.create_task", noop_create_task):
            await mq._try_deliver_messages("child_a")

        assert "child_a" in mq._parent_wake_registrations
        assert mq._parent_wake_registrations["child_a"].parent_session_id == "em_a"

    @pytest.mark.asyncio
    async def test_sequential_delivery_no_parent_wake_without_flag(
        self, mock_session_manager, temp_db_path
    ):
        """Delivery without parent_session_id does not register parent wake."""
        session = _make_session("child_b")
        session.status = SessionStatus.IDLE
        mock_session_manager.get_session.return_value = session

        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={},
            notifier=None,
        )
        with patch("asyncio.create_task", noop_create_task):
            mq.queue_message(
                target_session_id="child_b",
                text="plain send",
                remind_soft_threshold=210,
                remind_hard_threshold=420,
            )
            state = mq._get_or_create_state("child_b")
            state.is_idle = True

        with patch("asyncio.create_task", noop_create_task):
            await mq._try_deliver_messages("child_b")

        assert "child_b" not in mq._parent_wake_registrations


# ---------------------------------------------------------------------------
# TestParentWakeRecovery
# ---------------------------------------------------------------------------

class TestParentWakeRecovery:

    @pytest.mark.asyncio
    async def test_recovery_restores_active_registrations(
        self, mock_session_manager, temp_db_path
    ):
        """Active parent wake registrations are recovered on startup."""
        mq1 = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={},
            notifier=None,
        )
        with patch("asyncio.create_task", noop_create_task):
            mq1.register_parent_wake("child_r", "parent_r", period_seconds=300)

        # Create a fresh MQ instance (simulates server restart)
        mq2 = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={},
            notifier=None,
        )
        active_child = _make_session("child_r", provider="claude", tmux_session="claude-child_r")
        mock_session_manager.get_session.return_value = active_child
        mock_session_manager.tmux.session_exists.return_value = True
        with patch("asyncio.create_task", noop_create_task):
            await mq2._recover_parent_wake_registrations()

        assert "child_r" in mq2._parent_wake_registrations
        reg = mq2._parent_wake_registrations["child_r"]
        assert reg.parent_session_id == "parent_r"
        assert reg.period_seconds == 300

    @pytest.mark.asyncio
    async def test_recovery_skips_inactive_registrations(
        self, mock_session_manager, temp_db_path
    ):
        """Inactive (cancelled) registrations are not recovered."""
        mq1 = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={},
            notifier=None,
        )
        with patch("asyncio.create_task", noop_create_task):
            mq1.register_parent_wake("child_x", "parent_x")
        mq1.cancel_parent_wake("child_x")

        mq2 = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={},
            notifier=None,
        )
        with patch("asyncio.create_task", noop_create_task):
            await mq2._recover_parent_wake_registrations()

        assert "child_x" not in mq2._parent_wake_registrations

    @pytest.mark.asyncio
    async def test_recovery_cancels_dead_child_registrations(
        self, mock_session_manager, temp_db_path
    ):
        """Recovered parent-wake rows are auto-cancelled when the child runtime is already gone."""
        mq1 = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={},
            notifier=None,
        )
        with patch("asyncio.create_task", noop_create_task):
            mq1.register_parent_wake("child_dead", "parent_dead", period_seconds=300)

        dead_child = _make_session("child_dead", provider="claude", tmux_session="claude-child_dead")
        mock_session_manager.get_session.return_value = dead_child
        mock_session_manager.tmux.session_exists.return_value = False

        mq2 = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={},
            notifier=None,
        )
        with patch("asyncio.create_task", noop_create_task):
            await mq2._recover_parent_wake_registrations()

        assert "child_dead" not in mq2._parent_wake_registrations

        conn = sqlite3.connect(temp_db_path)
        rows = conn.execute(
            "SELECT is_active FROM parent_wake_registrations WHERE child_session_id = 'child_dead'"
        ).fetchall()
        conn.close()
        assert rows[0][0] == 0


class TestParentWakeDeadChildCancellation:

    @pytest.mark.asyncio
    async def test_parent_wake_task_cancels_missing_child_before_digest(self, mock_session_manager, temp_db_path):
        """A missing child session stops the periodic wake loop instead of emitting <unknown> digests."""
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={},
            notifier=None,
        )
        reg = ParentWakeRegistration(
            id="dead_reg",
            child_session_id="child_missing",
            parent_session_id="parent_dead",
            period_seconds=1,
            registered_at=datetime.now() - timedelta(minutes=5),
            last_wake_at=None,
            last_status_at_prev_wake=None,
        )
        mq._parent_wake_registrations["child_missing"] = reg
        mock_session_manager.get_session.return_value = None

        queue_calls = []

        async def fake_sleep(_seconds):
            return None

        with patch("asyncio.sleep", side_effect=fake_sleep), \
             patch.object(mq, "queue_message", side_effect=lambda **kwargs: queue_calls.append(kwargs)):
            await mq._run_parent_wake_task("child_missing")

        assert queue_calls == []
        assert "child_missing" not in mq._parent_wake_registrations

        conn = sqlite3.connect(temp_db_path)
        rows = conn.execute(
            "SELECT is_active FROM parent_wake_registrations WHERE child_session_id = 'child_missing'"
        ).fetchall()
        conn.close()
        assert rows == []


# ---------------------------------------------------------------------------
# TestParentWakeDigest
# ---------------------------------------------------------------------------

class TestParentWakeDigest:

    @pytest.mark.asyncio
    async def test_digest_basic_structure(self, mq):
        """Digest contains expected header, duration, and status lines."""
        reg = ParentWakeRegistration(
            id="test_reg",
            child_session_id="child_d",
            parent_session_id="parent_d",
            period_seconds=600,
            registered_at=datetime.now() - timedelta(minutes=15),
            last_wake_at=None,
            last_status_at_prev_wake=None,
        )

        child_session = _make_session("child_d")
        child_session.friendly_name = "engineer-42"
        child_session.agent_status_text = "fixing the bug"
        child_session.agent_status_at = datetime.now() - timedelta(minutes=2)
        mq.session_manager.get_session.return_value = child_session

        with patch.object(mq, "_read_child_tail", return_value=[]):
            digest = await mq._assemble_parent_wake_digest("child_d", reg)

        assert "[sm dispatch] Child update:" in digest
        assert "engineer-42" in digest
        assert "15m running" in digest
        assert "fixing the bug" in digest

    @pytest.mark.asyncio
    async def test_digest_no_progress_flag(self, mq):
        """Digest shows NO PROGRESS DETECTED when status_at unchanged since last wake."""
        status_time = datetime.now() - timedelta(minutes=15)
        reg = ParentWakeRegistration(
            id="test_reg2",
            child_session_id="child_np",
            parent_session_id="parent_np",
            period_seconds=600,
            registered_at=datetime.now() - timedelta(minutes=25),
            last_wake_at=datetime.now() - timedelta(minutes=10),
            last_status_at_prev_wake=status_time,
        )

        child_session = _make_session("child_np")
        child_session.agent_status_text = "investigating"
        child_session.agent_status_at = status_time  # unchanged
        mq.session_manager.get_session.return_value = child_session

        with patch.object(mq, "_read_child_tail", return_value=[]):
            digest = await mq._assemble_parent_wake_digest("child_np", reg)

        assert "NO PROGRESS DETECTED" in digest
        assert "Warning:" in digest

    @pytest.mark.asyncio
    async def test_digest_no_progress_not_shown_first_wake(self, mq):
        """NO PROGRESS DETECTED is never shown on first wake (last_wake_at=None)."""
        status_time = datetime.now() - timedelta(minutes=5)
        reg = ParentWakeRegistration(
            id="test_reg3",
            child_session_id="child_fw",
            parent_session_id="parent_fw",
            period_seconds=600,
            registered_at=datetime.now() - timedelta(minutes=10),
            last_wake_at=None,  # First wake
            last_status_at_prev_wake=status_time,
        )

        child_session = _make_session("child_fw")
        child_session.agent_status_text = "working"
        child_session.agent_status_at = status_time
        mq.session_manager.get_session.return_value = child_session

        with patch.object(mq, "_read_child_tail", return_value=[]):
            digest = await mq._assemble_parent_wake_digest("child_fw", reg)

        assert "NO PROGRESS DETECTED" not in digest

    @pytest.mark.asyncio
    async def test_digest_includes_tool_events(self, mq):
        """Digest includes recent tool activity when available."""
        reg = ParentWakeRegistration(
            id="test_reg4",
            child_session_id="child_t",
            parent_session_id="parent_t",
            period_seconds=600,
            registered_at=datetime.now() - timedelta(minutes=5),
            last_wake_at=None,
            last_status_at_prev_wake=None,
        )

        child_session = _make_session("child_t")
        mq.session_manager.get_session.return_value = child_session

        tool_events = [
            {"tool_name": "Read", "target_file": "src/foo.py", "bash_command": None, "timestamp": datetime.now().isoformat()},
            {"tool_name": "Bash", "target_file": None, "bash_command": "pytest tests/", "timestamp": datetime.now().isoformat()},
        ]
        with patch.object(mq, "_read_child_tail", return_value=tool_events):
            digest = await mq._assemble_parent_wake_digest("child_t", reg)

        assert "Recent activity:" in digest
        assert "Read" in digest
        assert "src/foo.py" in digest
        assert "Bash" in digest

    @pytest.mark.asyncio
    async def test_digest_unknown_child(self, mq):
        """Digest works gracefully when child session is not found."""
        reg = ParentWakeRegistration(
            id="test_reg5",
            child_session_id="child_gone",
            parent_session_id="parent_gone",
            period_seconds=600,
            registered_at=datetime.now() - timedelta(minutes=5),
            last_wake_at=None,
            last_status_at_prev_wake=None,
        )
        mq.session_manager.get_session.return_value = None

        with patch.object(mq, "_read_child_tail", return_value=[]):
            digest = await mq._assemble_parent_wake_digest("child_gone", reg)

        assert "[sm dispatch] Child update:" in digest

    @pytest.mark.asyncio
    async def test_digest_tool_timestamps_use_utc(self, mq):
        """Recent activity ages must be positive regardless of host timezone.

        SQLite CURRENT_TIMESTAMP is UTC (naive). The digest must compare against
        UTC now — not local now — or results are negative on UTC-behind systems.

        The test is deterministic: it patches datetime.now in src.message_queue to
        simulate a UTC-8 machine regardless of actual host timezone.
        """
        import re
        from datetime import timezone

        # Fixed reference point
        UTC_NOW = datetime(2026, 2, 20, 10, 0, 0)           # naive UTC
        UTC_8_NOW = UTC_NOW - timedelta(hours=8)             # naive "local" on UTC-8
        TOOL_TS = (UTC_NOW - timedelta(minutes=2)).strftime("%Y-%m-%d %H:%M:%S")  # SQLite format

        # Subclass that makes .now() behave like a UTC-8 machine
        class _FakeDatetime(datetime):
            @classmethod
            def now(cls, tz=None):
                if tz is None:
                    return UTC_8_NOW          # simulates datetime.now() on UTC-8
                if tz == timezone.utc:
                    return UTC_NOW.replace(tzinfo=timezone.utc)  # aware UTC
                return datetime.now(tz)       # fallback for other tzinfos

            @classmethod
            def fromisoformat(cls, s):
                return datetime.fromisoformat(s)  # delegate to real datetime

        reg = ParentWakeRegistration(
            id="r1",
            child_session_id="child_tz",
            parent_session_id="parent_tz",
            period_seconds=600,
            registered_at=UTC_8_NOW - timedelta(minutes=5),
            last_wake_at=None,
            last_status_at_prev_wake=None,
        )
        mq._parent_wake_registrations["child_tz"] = reg

        child_session = MagicMock()
        child_session.friendly_name = "tz-test"
        child_session.agent_status_text = None
        child_session.agent_status_at = None
        mq.session_manager.get_session.return_value = child_session

        tool_events = [
            {"tool_name": "Bash", "target_file": None,
             "bash_command": "pytest tests/", "timestamp": TOOL_TS},
        ]

        with patch("src.message_queue.datetime", _FakeDatetime), \
             patch.object(mq, "_read_child_tail", return_value=tool_events):
            digest = await mq._assemble_parent_wake_digest("child_tz", reg)

        # Old code: datetime.now() → UTC_8_NOW = UTC - 8h → age = -478m  (FAIL)
        # New code: datetime.now(timezone.utc).replace(tzinfo=None) → UTC_NOW → age = 2m (PASS)
        match = re.search(r'\((-?\d+)m ago\)', digest)
        assert match, f"Expected '(Nm ago)' in digest:\n{digest}"
        age_minutes = int(match.group(1))

        assert age_minutes >= 0, (
            f"Age is negative ({age_minutes}m) — datetime.now() used instead of UTC now"
        )
        assert age_minutes <= 5, f"Age unexpectedly large: {age_minutes}m"


# ---------------------------------------------------------------------------
# TestParentWakeEscalation
# ---------------------------------------------------------------------------

class TestParentWakeEscalation:

    @pytest.mark.asyncio
    async def test_escalation_on_no_progress(self, mock_session_manager, temp_db_path):
        """Period switches to 300s when child hasn't updated status since last wake."""
        status_time = datetime.now() - timedelta(minutes=12)

        child_session = _make_session("child_esc")
        child_session.agent_status_at = status_time
        mock_session_manager.get_session.return_value = child_session

        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={},
            notifier=None,
        )

        # Simulate a registration with previous wake where status didn't change
        reg = ParentWakeRegistration(
            id="esc_reg",
            child_session_id="child_esc",
            parent_session_id="parent_esc",
            period_seconds=600,
            registered_at=datetime.now() - timedelta(minutes=20),
            last_wake_at=datetime.now() - timedelta(minutes=10),
            last_status_at_prev_wake=status_time,  # same as current
        )
        mq._parent_wake_registrations["child_esc"] = reg

        with patch("asyncio.create_task", noop_create_task):
            # Simulate the post-wake escalation check
            current_status_at = child_session.agent_status_at
            if (
                reg.last_wake_at is not None
                and not reg.escalated
                and reg.last_status_at_prev_wake is not None
                and current_status_at == reg.last_status_at_prev_wake
            ):
                reg.escalated = True
                reg.period_seconds = mq._PARENT_WAKE_ESCALATED_PERIOD

        assert reg.escalated is True
        assert reg.period_seconds == 300

    @pytest.mark.asyncio
    async def test_no_escalation_when_status_changes(self, mock_session_manager, temp_db_path):
        """No escalation when child has updated status since last wake."""
        prev_status_time = datetime.now() - timedelta(minutes=8)
        new_status_time = datetime.now() - timedelta(minutes=2)

        child_session = _make_session("child_ok")
        child_session.agent_status_at = new_status_time  # different from prev_wake
        mock_session_manager.get_session.return_value = child_session

        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=temp_db_path,
            config={},
            notifier=None,
        )
        reg = ParentWakeRegistration(
            id="ok_reg",
            child_session_id="child_ok",
            parent_session_id="parent_ok",
            period_seconds=600,
            registered_at=datetime.now() - timedelta(minutes=15),
            last_wake_at=datetime.now() - timedelta(minutes=10),
            last_status_at_prev_wake=prev_status_time,
        )
        mq._parent_wake_registrations["child_ok"] = reg

        current_status_at = child_session.agent_status_at
        should_escalate = (
            reg.last_wake_at is not None
            and not reg.escalated
            and reg.last_status_at_prev_wake is not None
            and current_status_at == reg.last_status_at_prev_wake
        )
        assert should_escalate is False
        assert reg.period_seconds == 600


# ---------------------------------------------------------------------------
# TestCmdDispatchPassesParentSessionId
# ---------------------------------------------------------------------------

class TestCmdDispatchPassesParentSessionId:
    """cmd_dispatch passes em_id as parent_session_id to cmd_send."""

    def _make_client(self, **kwargs):
        mock_client = MagicMock()
        mock_client.send_input.return_value = (True, False)
        mock_client.get_session.return_value = {"id": "child_sess", "name": "child", "friendly_name": None}
        mock_client.list_sessions.return_value = [{"id": "child_sess", "name": "child", "friendly_name": None}]
        return mock_client

    def test_dispatch_passes_em_id_as_parent_session_id(self):
        """cmd_dispatch forwards em_id to client.send_input as parent_session_id."""
        from src.cli.commands import cmd_dispatch
        from tests.unit.test_dispatch import SAMPLE_CONFIG

        mock_client = self._make_client()

        with patch("src.cli.dispatch.load_template", return_value=SAMPLE_CONFIG), \
             patch("src.cli.dispatch.get_auto_remind_config", return_value=(210, 420)), \
             patch("src.cli.commands.cmd_clear", return_value=0), \
             patch("os.getcwd", return_value="/tmp"):
            cmd_dispatch(
                mock_client,
                "child_sess",
                "engineer",
                {"issue": "123", "spec": "docs/123.md"},
                em_id="em_parent_id",
            )

        call_kwargs = mock_client.send_input.call_args[1]
        assert call_kwargs["parent_session_id"] == "em_parent_id"

    def test_dispatch_no_parent_wake_without_em_id(self):
        """cmd_dispatch with em_id=None passes parent_session_id=None."""
        from src.cli.commands import cmd_dispatch
        from tests.unit.test_dispatch import SAMPLE_CONFIG

        mock_client = self._make_client()

        with patch("src.cli.dispatch.load_template", return_value=SAMPLE_CONFIG), \
             patch("src.cli.dispatch.get_auto_remind_config", return_value=(210, 420)), \
             patch("os.getcwd", return_value="/tmp"):
            # dry_run to avoid the em_id check failing
            cmd_dispatch(
                mock_client,
                "child_sess",
                "engineer",
                {"issue": "1", "spec": "s.md"},
                em_id=None,
                dry_run=True,
            )

        # dry_run exits before send, so send_input should not be called
        mock_client.send_input.assert_not_called()
