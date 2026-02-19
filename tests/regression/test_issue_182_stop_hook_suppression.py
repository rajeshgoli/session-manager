"""
Regression tests for issue #182: suppress redundant stop hook notifications after sm send.

When an agent reports its result via `sm send` and then goes idle, the stop hook
fires a redundant notification to the same target. The fix records the outgoing
sm send target/timestamp in send_input and checks in mark_session_idle with a 30s
suppression window.
"""

import pytest
from unittest.mock import MagicMock, AsyncMock, patch
from datetime import datetime, timedelta

from src.models import SessionDeliveryState, Session, SessionStatus, DeliveryResult
from src.message_queue import MessageQueueManager
from src.session_manager import SessionManager


def noop_create_task(coro):
    """Silently close coroutine without running it."""
    coro.close()
    return MagicMock()


# ============================================================================
# Fixtures
# ============================================================================


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager for MessageQueueManager."""
    manager = MagicMock()
    manager.get_session = MagicMock(return_value=None)
    return manager


@pytest.fixture
def message_queue(mock_session_manager, tmp_path):
    """Create a real MessageQueueManager for testing."""
    mq = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=str(tmp_path / "test_182.db"),
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
                    "watch_poll_interval_seconds": 0.1,
                }
            },
        },
        notifier=None,
    )
    return mq


# ============================================================================
# Unit test: recent sm send suppresses stop notification
# ============================================================================


def test_recent_sm_send_suppresses_stop_notification(message_queue):
    """
    When agent recently sm-sent to the same target as stop_notify_sender_id,
    the stop notification should be suppressed.
    """
    session_id = "agent-a"
    em_id = "em-parent"

    state = message_queue._get_or_create_state(session_id)
    state.stop_notify_sender_id = em_id
    state.stop_notify_sender_name = "em-session"
    state.last_outgoing_sm_send_target = em_id
    state.last_outgoing_sm_send_at = datetime.now()

    with patch("asyncio.create_task", noop_create_task), \
         patch.object(message_queue, "_send_stop_notification") as mock_notify:
        message_queue.mark_session_idle(session_id, from_stop_hook=True)

        # Stop notification should NOT be sent
        mock_notify.assert_not_called()

    # Fields should be cleared
    assert state.stop_notify_sender_id is None
    assert state.stop_notify_sender_name is None
    assert state.last_outgoing_sm_send_target is None
    assert state.last_outgoing_sm_send_at is None


# ============================================================================
# Unit test: expired window does NOT suppress
# ============================================================================


def test_expired_window_does_not_suppress(message_queue):
    """
    When sm send happened more than 30s ago, the stop notification should
    fire normally (window expired).
    """
    session_id = "agent-expired"
    em_id = "em-parent"

    state = message_queue._get_or_create_state(session_id)
    state.stop_notify_sender_id = em_id
    state.stop_notify_sender_name = "em-session"
    state.last_outgoing_sm_send_target = em_id
    state.last_outgoing_sm_send_at = datetime.now() - timedelta(seconds=60)

    with patch("asyncio.create_task", noop_create_task), \
         patch.object(message_queue, "_send_stop_notification") as mock_notify:
        message_queue.mark_session_idle(session_id, from_stop_hook=True)

        # Stop notification SHOULD be sent (window expired)
        mock_notify.assert_called_once()
        assert mock_notify.call_args[1]["sender_session_id"] == em_id


# ============================================================================
# Unit test: non-matching target preserves notification
# ============================================================================


def test_non_matching_target_preserves_notification(message_queue):
    """
    When agent sm-sent to a different target than stop_notify_sender_id,
    the stop notification should fire normally.
    """
    session_id = "agent-mismatch"
    em_id = "em-parent"
    other_id = "other-agent"

    state = message_queue._get_or_create_state(session_id)
    state.stop_notify_sender_id = em_id
    state.stop_notify_sender_name = "em-session"
    state.last_outgoing_sm_send_target = other_id
    state.last_outgoing_sm_send_at = datetime.now()

    with patch("asyncio.create_task", noop_create_task), \
         patch.object(message_queue, "_send_stop_notification") as mock_notify:
        message_queue.mark_session_idle(session_id, from_stop_hook=True)

        # Stop notification SHOULD be sent (targets don't match)
        mock_notify.assert_called_once()
        assert mock_notify.call_args[1]["sender_session_id"] == em_id


# ============================================================================
# Unit test: send_input records outgoing target after enqueue
# ============================================================================


@pytest.mark.asyncio
async def test_send_input_records_outgoing_target():
    """
    When send_input is called from sm send, the sender's delivery state
    should record the outgoing target and timestamp.
    """
    # Set up real SessionManager with mocked tmux
    manager = MagicMock(spec=SessionManager)

    # Create a real MessageQueueManager
    import tempfile
    with tempfile.TemporaryDirectory() as tmp_dir:
        mq = MessageQueueManager(
            session_manager=manager,
            db_path=f"{tmp_dir}/test_record.db",
            config={},
            notifier=None,
        )

        # Create target and sender sessions
        em_session = Session(
            id="em-001",
            name="em-session",
            working_dir="/tmp",
            tmux_session="claude-em-001",
            status=SessionStatus.IDLE,
        )
        agent_session = Session(
            id="agent-001",
            name="agent-session",
            working_dir="/tmp",
            tmux_session="claude-agent-001",
            status=SessionStatus.RUNNING,
            friendly_name="test-agent",
        )

        # Create a real SessionManager
        real_manager = MagicMock(spec=SessionManager)
        real_manager.sessions = {"em-001": em_session, "agent-001": agent_session}
        real_manager.get_session = lambda sid: real_manager.sessions.get(sid)
        real_manager.message_queue_manager = mq
        real_manager.notifier = None
        real_manager._save_state = MagicMock()
        real_manager._deliver_direct = AsyncMock(return_value=True)

        # Create actual SessionManager.send_input by binding the real method
        sm = SessionManager.__new__(SessionManager)
        sm.sessions = real_manager.sessions
        sm.message_queue_manager = mq
        sm.notifier = None
        sm._save_state = MagicMock()
        sm._deliver_direct = AsyncMock(return_value=True)

        # Initialize the delivery state as idle for em-001
        mq._get_or_create_state("em-001").is_idle = True

        # Call send_input as sm send (from_sm_send=True)
        with patch("asyncio.create_task", noop_create_task):
            result = await sm.send_input(
                session_id="em-001",
                text="done: PR #42 created",
                sender_session_id="agent-001",
                delivery_mode="sequential",
                from_sm_send=True,
            )

        assert result == DeliveryResult.DELIVERED

        # Check that sender's delivery state was updated
        sender_state = mq.delivery_states.get("agent-001")
        assert sender_state is not None
        assert sender_state.last_outgoing_sm_send_target == "em-001"
        assert sender_state.last_outgoing_sm_send_at is not None
        assert (datetime.now() - sender_state.last_outgoing_sm_send_at).total_seconds() < 5


# ============================================================================
# Unit test: failed enqueue does not record target
# ============================================================================


@pytest.mark.asyncio
async def test_failed_enqueue_does_not_record_target():
    """
    When queue_message raises an exception, the sender's delivery state
    should NOT record the outgoing target.
    """
    import tempfile
    with tempfile.TemporaryDirectory() as tmp_dir:
        manager = MagicMock(spec=SessionManager)
        mq = MessageQueueManager(
            session_manager=manager,
            db_path=f"{tmp_dir}/test_fail.db",
            config={},
            notifier=None,
        )

        em_session = Session(
            id="em-001",
            name="em-session",
            working_dir="/tmp",
            tmux_session="claude-em-001",
            status=SessionStatus.IDLE,
        )
        agent_session = Session(
            id="agent-001",
            name="agent-session",
            working_dir="/tmp",
            tmux_session="claude-agent-001",
            status=SessionStatus.RUNNING,
            friendly_name="test-agent",
        )

        sm = SessionManager.__new__(SessionManager)
        sm.sessions = {"em-001": em_session, "agent-001": agent_session}
        sm.message_queue_manager = mq
        sm.notifier = None
        sm._save_state = MagicMock()
        sm._deliver_direct = AsyncMock(return_value=True)

        mq._get_or_create_state("em-001").is_idle = True

        # Make queue_message raise an exception
        original_queue = mq.queue_message
        with patch.object(mq, "queue_message", side_effect=Exception("DB error")):
            with patch("asyncio.create_task", noop_create_task):
                try:
                    await sm.send_input(
                        session_id="em-001",
                        text="done: PR #42 created",
                        sender_session_id="agent-001",
                        delivery_mode="sequential",
                        from_sm_send=True,
                    )
                except Exception:
                    pass

        # Sender state should NOT have outgoing target recorded
        sender_state = mq.delivery_states.get("agent-001")
        if sender_state:
            assert sender_state.last_outgoing_sm_send_target is None


# ============================================================================
# Unit test: system messages don't record target
# ============================================================================


@pytest.mark.asyncio
async def test_system_messages_dont_record_target():
    """
    When send_input is called without from_sm_send=True, no outgoing
    target should be recorded.
    """
    import tempfile
    with tempfile.TemporaryDirectory() as tmp_dir:
        manager = MagicMock(spec=SessionManager)
        mq = MessageQueueManager(
            session_manager=manager,
            db_path=f"{tmp_dir}/test_sys.db",
            config={},
            notifier=None,
        )

        em_session = Session(
            id="em-001",
            name="em-session",
            working_dir="/tmp",
            tmux_session="claude-em-001",
            status=SessionStatus.IDLE,
        )

        sm = SessionManager.__new__(SessionManager)
        sm.sessions = {"em-001": em_session}
        sm.message_queue_manager = mq
        sm.notifier = None
        sm._save_state = MagicMock()
        sm._deliver_direct = AsyncMock(return_value=True)

        mq._get_or_create_state("em-001").is_idle = True

        with patch("asyncio.create_task", noop_create_task):
            await sm.send_input(
                session_id="em-001",
                text="system notification",
                sender_session_id="agent-001",
                delivery_mode="sequential",
                from_sm_send=False,  # NOT from sm send
            )

        # No outgoing target should be recorded
        sender_state = mq.delivery_states.get("agent-001")
        if sender_state:
            assert sender_state.last_outgoing_sm_send_target is None


# ============================================================================
# Unit test: mid-task sm send outside window does not suppress
# ============================================================================


def test_mid_task_sm_send_outside_window_does_not_suppress(message_queue):
    """
    When sm send happened 31s ago (just outside window), stop notification
    should fire normally.
    """
    session_id = "agent-midtask"
    em_id = "em-parent"

    state = message_queue._get_or_create_state(session_id)
    state.stop_notify_sender_id = em_id
    state.stop_notify_sender_name = "em-session"
    state.last_outgoing_sm_send_target = em_id
    state.last_outgoing_sm_send_at = datetime.now() - timedelta(seconds=31)

    with patch("asyncio.create_task", noop_create_task), \
         patch.object(message_queue, "_send_stop_notification") as mock_notify:
        message_queue.mark_session_idle(session_id, from_stop_hook=True)

        # Stop notification SHOULD fire (window expired at 31s > 30s)
        mock_notify.assert_called_once()


# ============================================================================
# Unit test: suppression does not interfere with skip_count
# ============================================================================


def test_skip_count_takes_precedence_over_suppression(message_queue):
    """
    When skip_count > 0, the skip_count path fires first (returns early).
    Suppression check is never reached. After skip_count is consumed,
    suppression should work on the next stop hook.
    """
    session_id = "agent-both"
    em_id = "em-parent"

    state = message_queue._get_or_create_state(session_id)
    state.stop_notify_skip_count = 1
    state.stop_notify_sender_id = em_id
    state.stop_notify_sender_name = "em-session"
    state.last_outgoing_sm_send_target = em_id
    state.last_outgoing_sm_send_at = datetime.now()

    # First stop hook: skip_count absorbs it
    with patch("asyncio.create_task", noop_create_task), \
         patch.object(message_queue, "_send_stop_notification") as mock_notify:
        message_queue.mark_session_idle(session_id, from_stop_hook=True)
        mock_notify.assert_not_called()

    assert state.stop_notify_skip_count == 0
    # sender_id preserved by skip_count path
    assert state.stop_notify_sender_id == em_id
    # sm send target still recorded
    assert state.last_outgoing_sm_send_target == em_id

    # Second stop hook: skip_count=0, suppression check fires
    with patch("asyncio.create_task", noop_create_task), \
         patch.object(message_queue, "_send_stop_notification") as mock_notify:
        message_queue.mark_session_idle(session_id, from_stop_hook=True)
        # Suppressed by #182 (recent sm send to same target)
        mock_notify.assert_not_called()

    # All fields cleared by suppression
    assert state.stop_notify_sender_id is None
    assert state.last_outgoing_sm_send_target is None


# ============================================================================
# Unit test: no sm send target means no suppression
# ============================================================================


def test_no_sm_send_target_means_no_suppression(message_queue):
    """
    When last_outgoing_sm_send_target is None (agent didn't sm send),
    stop notification should fire normally.
    """
    session_id = "agent-no-send"
    em_id = "em-parent"

    state = message_queue._get_or_create_state(session_id)
    state.stop_notify_sender_id = em_id
    state.stop_notify_sender_name = "em-session"
    # No sm send recorded
    assert state.last_outgoing_sm_send_target is None

    with patch("asyncio.create_task", noop_create_task), \
         patch.object(message_queue, "_send_stop_notification") as mock_notify:
        message_queue.mark_session_idle(session_id, from_stop_hook=True)

        # Stop notification SHOULD fire
        mock_notify.assert_called_once()


# ============================================================================
# Unit test: important delivery mode also records target
# ============================================================================


@pytest.mark.asyncio
async def test_important_mode_records_outgoing_target():
    """
    When send_input is called with delivery_mode=important and from_sm_send=True,
    the sender's delivery state should record the outgoing target.
    """
    import tempfile
    with tempfile.TemporaryDirectory() as tmp_dir:
        manager = MagicMock(spec=SessionManager)
        mq = MessageQueueManager(
            session_manager=manager,
            db_path=f"{tmp_dir}/test_important.db",
            config={},
            notifier=None,
        )

        em_session = Session(
            id="em-001",
            name="em-session",
            working_dir="/tmp",
            tmux_session="claude-em-001",
            status=SessionStatus.IDLE,
        )
        agent_session = Session(
            id="agent-001",
            name="agent-session",
            working_dir="/tmp",
            tmux_session="claude-agent-001",
            status=SessionStatus.RUNNING,
            friendly_name="test-agent",
        )

        sm = SessionManager.__new__(SessionManager)
        sm.sessions = {"em-001": em_session, "agent-001": agent_session}
        sm.message_queue_manager = mq
        sm.notifier = None
        sm._save_state = MagicMock()
        sm._deliver_direct = AsyncMock(return_value=True)

        mq._get_or_create_state("em-001").is_idle = True

        with patch("asyncio.create_task", noop_create_task):
            result = await sm.send_input(
                session_id="em-001",
                text="done: PR #42 created",
                sender_session_id="agent-001",
                delivery_mode="important",
                from_sm_send=True,
            )

        # Check sender state was updated
        sender_state = mq.delivery_states.get("agent-001")
        assert sender_state is not None
        assert sender_state.last_outgoing_sm_send_target == "em-001"
        assert sender_state.last_outgoing_sm_send_at is not None


# ============================================================================
# Regression: invalidate_session_cache clears stale sm send recording (#182)
# ============================================================================


def test_invalidate_cache_clears_stale_sm_send_recording(message_queue):
    """
    Scenario: agent sm-sends → EM clears → EM resends → agent completes
    without sm send within 30s.

    Without the fix, the stale last_outgoing_sm_send_target from the first
    cycle would cause false suppression on the second cycle's stop hook.

    _invalidate_session_cache must clear both new fields alongside
    stop_notify_sender_id to prevent this.
    """
    from src.server import _invalidate_session_cache
    from unittest.mock import Mock

    app = Mock()
    app.state.last_claude_output = {}
    app.state.pending_stop_notifications = set()

    # Wire app's queue_mgr to real MessageQueueManager
    app.state.session_manager = Mock()
    app.state.session_manager.message_queue_manager = message_queue

    agent_id = "agent-cycle"
    em_id = "em-parent"

    # === Cycle 1: agent sm-sends to EM ===
    state = message_queue._get_or_create_state(agent_id)
    state.stop_notify_sender_id = em_id
    state.stop_notify_sender_name = "em-session"
    state.last_outgoing_sm_send_target = em_id
    state.last_outgoing_sm_send_at = datetime.now()

    # Verify suppression would fire (cycle 1 happy path)
    with patch("asyncio.create_task", noop_create_task), \
         patch.object(message_queue, "_send_stop_notification") as mock_notify:
        message_queue.mark_session_idle(agent_id, from_stop_hook=True)
        mock_notify.assert_not_called()  # suppressed as expected

    # === EM clears the agent (sm clear) ===
    # arm_skip=True increments stop_notify_skip_count to absorb /clear stop hook
    _invalidate_session_cache(app, agent_id, arm_skip=True)

    # Verify sm send fields were cleared
    assert state.last_outgoing_sm_send_target is None
    assert state.last_outgoing_sm_send_at is None
    assert state.stop_notify_sender_id is None
    assert state.stop_notify_skip_count == 1

    # /clear stop hook fires and is absorbed by skip_count
    with patch("asyncio.create_task", noop_create_task):
        message_queue.mark_session_idle(agent_id, from_stop_hook=True)
    assert state.stop_notify_skip_count == 0

    # === Cycle 2: EM resends, agent completes WITHOUT sm send ===
    # Delivery sets stop_notify_sender_id but agent does NOT sm send back
    state.stop_notify_sender_id = em_id
    state.stop_notify_sender_name = "em-session"
    # last_outgoing_sm_send_target remains None (no sm send in cycle 2)

    with patch("asyncio.create_task", noop_create_task), \
         patch.object(message_queue, "_send_stop_notification") as mock_notify:
        message_queue.mark_session_idle(agent_id, from_stop_hook=True)

        # Stop notification MUST fire (no sm send in this cycle)
        mock_notify.assert_called_once()
        assert mock_notify.call_args[1]["sender_session_id"] == em_id


def test_invalidate_cache_arm_skip_false_also_clears_sm_send_fields():
    """
    _invalidate_session_cache with arm_skip=False (codex-app /clear path)
    also clears last_outgoing_sm_send_target and last_outgoing_sm_send_at.
    """
    from src.server import _invalidate_session_cache
    from unittest.mock import Mock

    app = Mock()
    app.state.last_claude_output = {}
    app.state.pending_stop_notifications = set()

    queue_mgr = Mock()
    queue_mgr.delivery_states = {}
    app.state.session_manager = Mock()
    app.state.session_manager.message_queue_manager = queue_mgr

    session_id = "codex-app-182"
    state = SessionDeliveryState(session_id=session_id)
    state.last_outgoing_sm_send_target = "em-parent"
    state.last_outgoing_sm_send_at = datetime.now()
    queue_mgr.delivery_states[session_id] = state

    _invalidate_session_cache(app, session_id)  # arm_skip=False

    assert state.last_outgoing_sm_send_target is None
    assert state.last_outgoing_sm_send_at is None
