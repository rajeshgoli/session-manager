"""Unit tests for activity-state computation (#288)."""

from __future__ import annotations

import tempfile
from datetime import datetime, timedelta
from types import SimpleNamespace
from unittest.mock import patch, MagicMock

from src.models import CompletionStatus, MonitorState, Session, SessionDeliveryState, SessionStatus
from src.message_queue import MessageQueueManager
from src.session_manager import SessionManager


def _make_manager() -> SessionManager:
    tmpdir = tempfile.TemporaryDirectory()
    manager = SessionManager(log_dir=tmpdir.name, state_file=f"{tmpdir.name}/state.json")
    manager._tmpdir = tmpdir  # keep alive for test scope
    return manager


def _noop_create_task(coro):
    """Close scheduled coroutines immediately for deterministic unit tests."""
    coro.close()
    return MagicMock()


def test_non_codex_activity_uses_queue_and_monitor_signals():
    manager = _make_manager()
    session = Session(
        id="s1",
        name="claude-s1",
        working_dir="/tmp",
        tmux_session="claude-s1",
        provider="claude",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.message_queue_manager = SimpleNamespace(
        delivery_states={session.id: SessionDeliveryState(session_id=session.id, is_idle=False)}
    )
    manager.output_monitor = SimpleNamespace(
        get_session_state=lambda _sid: MonitorState(is_output_flowing=True)
    )

    assert manager.get_activity_state(session.id) == "working"
    manager.output_monitor = SimpleNamespace(
        get_session_state=lambda _sid: MonitorState(is_output_flowing=False)
    )
    assert manager.get_activity_state(session.id) == "thinking"

    manager.message_queue_manager.delivery_states[session.id].is_idle = True
    assert manager.get_activity_state(session.id) == "idle"


def test_non_codex_waiting_permission_and_waiting_input_precedence():
    manager = _make_manager()
    session = Session(
        id="perm1",
        name="claude-perm1",
        working_dir="/tmp",
        provider="claude",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.message_queue_manager = SimpleNamespace(
        delivery_states={session.id: SessionDeliveryState(session_id=session.id, is_idle=False)}
    )
    manager.output_monitor = SimpleNamespace(
        get_session_state=lambda _sid: MonitorState(is_output_flowing=False, last_pattern="permission")
    )

    assert manager.get_activity_state(session.id) == "waiting_permission"

    session.completion_status = CompletionStatus.COMPLETED
    manager.output_monitor = SimpleNamespace(
        get_session_state=lambda _sid: MonitorState(is_output_flowing=False, last_pattern=None)
    )
    assert manager.get_activity_state(session.id) == "waiting_input"


def test_non_codex_fallback_without_hook_data_uses_last_activity():
    manager = _make_manager()
    session = Session(
        id="fallback1",
        name="claude-fallback1",
        working_dir="/tmp",
        provider="claude",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    session.last_activity = datetime.now() - timedelta(seconds=5)
    assert manager.get_activity_state(session.id) == "thinking"

    session.last_activity = datetime.now() - timedelta(seconds=45)
    assert manager.get_activity_state(session.id) == "idle"


def test_non_codex_idle_status_beats_recent_last_activity_without_queue_state():
    manager = _make_manager()
    session = Session(
        id="idle1",
        name="claude-idle1",
        working_dir="/tmp",
        provider="claude",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session
    manager.output_monitor = SimpleNamespace(
        get_session_state=lambda _sid: MonitorState(is_output_flowing=False)
    )

    session.last_activity = datetime.now() - timedelta(seconds=5)
    assert manager.get_activity_state(session.id) == "idle"


def test_plain_codex_idle_status_uses_recent_activity_grace_window():
    manager = _make_manager()
    session = Session(
        id="codex-idle1",
        name="codex-codex-idle1",
        working_dir="/tmp",
        provider="codex",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session
    manager.output_monitor = SimpleNamespace(
        get_session_state=lambda _sid: MonitorState(is_output_flowing=False)
    )

    session.last_activity = datetime.now() - timedelta(seconds=5)
    assert manager.get_activity_state(session.id) == "thinking"

    session.last_activity = datetime.now() - timedelta(seconds=45)
    assert manager.get_activity_state(session.id) == "idle"


def test_codex_app_uses_queue_tristate_and_completion():
    manager = _make_manager()
    session = Session(
        id="codex2",
        name="codex-app-codex2",
        working_dir="/tmp",
        provider="codex-app",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.message_queue_manager = SimpleNamespace(
        delivery_states={session.id: SessionDeliveryState(session_id=session.id, is_idle=False)}
    )

    assert manager.get_activity_state(session.id) == "working"

    manager.message_queue_manager.delivery_states[session.id].is_idle = True
    assert manager.get_activity_state(session.id) == "idle"

    session.completion_status = CompletionStatus.COMPLETED
    assert manager.get_activity_state(session.id) == "waiting_input"


def test_codex_app_fallback_without_hook_data():
    manager = _make_manager()
    session = Session(
        id="codex3",
        name="codex-app-codex3",
        working_dir="/tmp",
        provider="codex-app",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    session.last_activity = datetime.now() - timedelta(seconds=2)
    assert manager.get_activity_state(session.id) == "thinking"

    session.last_activity = datetime.now() - timedelta(seconds=40)
    assert manager.get_activity_state(session.id) == "idle"


def test_codex_fork_running_state_prevents_false_idle():
    manager = _make_manager()
    session = Session(
        id="cf1",
        name="codex-fork-cf1",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    session.last_activity = datetime.now() - timedelta(minutes=5)

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "TurnStarted",
            "seq": 1,
            "session_epoch": 1,
            "payload": {},
        },
    )

    assert manager.get_activity_state(session.id) == "working"


def test_codex_fork_reducer_overrides_stale_stopped_status():
    manager = _make_manager()
    session = Session(
        id="cf2",
        name="codex-fork-cf2",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.STOPPED,
    )
    manager.sessions[session.id] = session

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "TurnStarted",
            "seq": 1,
            "session_epoch": 1,
            "payload": {},
        },
    )

    assert manager.get_activity_state(session.id) == "working"


def test_codex_fork_waiting_transitions_and_cause_tracking():
    manager = _make_manager()
    session = Session(
        id="cf3",
        name="codex-fork-cf3",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "TurnStarted",
            "seq": 1,
            "session_epoch": 1,
            "payload": {},
        },
    )
    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "ExecApprovalRequest",
            "seq": 2,
            "session_epoch": 1,
            "payload": {},
        },
    )
    assert manager.get_activity_state(session.id) == "waiting_permission"
    lifecycle = manager.get_codex_fork_lifecycle_state(session.id)
    assert lifecycle is not None
    assert lifecycle["state"] == "waiting_on_approval"
    assert lifecycle["cause_event_type"] == "approval_request"

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "approval_decision",
            "seq": 3,
            "session_epoch": 1,
            "payload": {},
        },
    )
    assert manager.get_activity_state(session.id) == "working"

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "TurnComplete",
            "seq": 4,
            "session_epoch": 1,
            "payload": {},
        },
    )
    assert manager.get_activity_state(session.id) == "idle"


def test_codex_fork_stream_error_does_not_stop_active_runtime():
    manager = _make_manager()
    session = Session(
        id="cf-stream",
        name="codex-fork-cf-stream",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "TurnStarted",
            "seq": 1,
            "session_epoch": 1,
            "payload": {},
        },
    )
    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "stream_error",
            "seq": 2,
            "session_epoch": 1,
            "payload": {"message": "Reconnecting... 1/5"},
        },
    )

    assert session.status == SessionStatus.RUNNING
    assert manager.get_activity_state(session.id) == "working"
    lifecycle = manager.get_codex_fork_lifecycle_state(session.id)
    assert lifecycle is not None
    assert lifecycle["state"] == "running"
    assert lifecycle["cause_event_type"] == "stream_error"


def test_codex_fork_stream_error_preserves_waiting_states():
    manager = _make_manager()
    session = Session(
        id="cf-stream-wait",
        name="codex-fork-cf-stream-wait",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "TurnStarted",
            "seq": 1,
            "session_epoch": 1,
            "payload": {},
        },
    )
    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "ExecApprovalRequest",
            "seq": 2,
            "session_epoch": 1,
            "payload": {},
        },
    )
    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "stream_error",
            "seq": 3,
            "session_epoch": 1,
            "payload": {"message": "Reconnecting... 1/5"},
        },
    )

    assert session.status == SessionStatus.RUNNING
    assert manager.get_activity_state(session.id) == "waiting_permission"
    lifecycle = manager.get_codex_fork_lifecycle_state(session.id)
    assert lifecycle is not None
    assert lifecycle["state"] == "waiting_on_approval"
    assert lifecycle["cause_event_type"] == "stream_error"


def test_codex_fork_interrupted_turn_abort_does_not_mark_idle():
    manager = _make_manager()
    session = Session(
        id="cf-abort",
        name="codex-fork-cf-abort",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    calls = {"idle": 0, "active": 0}
    manager.message_queue_manager = SimpleNamespace(
        mark_session_idle=lambda _sid, **_kwargs: calls.__setitem__("idle", calls["idle"] + 1),
        mark_session_active=lambda _sid: calls.__setitem__("active", calls["active"] + 1),
    )

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "TurnStarted",
            "seq": 1,
            "session_epoch": 1,
            "payload": {"turn_id": "t1"},
        },
    )
    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "turn_aborted",
            "seq": 2,
            "session_epoch": 1,
            "payload": {"turn_id": "t1", "reason": "interrupted"},
        },
    )

    assert session.status == SessionStatus.RUNNING
    assert manager.get_activity_state(session.id) == "working"
    lifecycle = manager.get_codex_fork_lifecycle_state(session.id)
    assert lifecycle is not None
    assert lifecycle["state"] == "running"
    assert lifecycle["cause_event_type"] == "turn_aborted"
    assert calls["idle"] == 0


def test_codex_fork_interrupted_abort_after_completion_preserves_idle():
    manager = _make_manager()
    session = Session(
        id="cf-abort-idle",
        name="codex-fork-cf-abort-idle",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    calls = {"idle": 0, "active": 0}
    manager.message_queue_manager = SimpleNamespace(
        mark_session_idle=lambda _sid, **_kwargs: calls.__setitem__("idle", calls["idle"] + 1),
        mark_session_active=lambda _sid: calls.__setitem__("active", calls["active"] + 1),
    )

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "TurnStarted",
            "seq": 1,
            "session_epoch": 1,
            "payload": {"turn_id": "t1"},
        },
    )
    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "TurnComplete",
            "seq": 2,
            "session_epoch": 1,
            "payload": {"turn_id": "t1"},
        },
    )
    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "turn_aborted",
            "seq": 3,
            "session_epoch": 1,
            "payload": {"turn_id": "t1", "reason": "interrupted"},
        },
    )

    assert session.status == SessionStatus.IDLE
    assert manager.get_activity_state(session.id) == "idle"
    lifecycle = manager.get_codex_fork_lifecycle_state(session.id)
    assert lifecycle is not None
    assert lifecycle["state"] == "idle"
    assert lifecycle["cause_event_type"] == "turn_aborted"
    assert calls["idle"] == 1


def test_codex_fork_shutdown_complete_preserves_idle_completion_state():
    manager = _make_manager()
    session = Session(
        id="cf-shutdown",
        name="codex-fork-cf-shutdown",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    calls = {"idle": 0, "active": 0}
    manager.message_queue_manager = SimpleNamespace(
        mark_session_idle=lambda _sid, **_kwargs: calls.__setitem__("idle", calls["idle"] + 1),
        mark_session_active=lambda _sid: calls.__setitem__("active", calls["active"] + 1),
    )

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "TurnStarted",
            "seq": 1,
            "session_epoch": 1,
            "payload": {"turn_id": "t1"},
        },
    )
    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "TurnComplete",
            "seq": 2,
            "session_epoch": 1,
            "payload": {"turn_id": "t1"},
        },
    )
    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "shutdown_complete",
            "seq": 3,
            "session_epoch": 1,
            "payload": {},
        },
    )

    assert session.status == SessionStatus.IDLE
    assert manager.get_activity_state(session.id) == "idle"
    lifecycle = manager.get_codex_fork_lifecycle_state(session.id)
    assert lifecycle is not None
    assert lifecycle["state"] == "idle"
    assert lifecycle["cause_event_type"] == "shutdown_complete"
    assert calls["idle"] == 1


def test_codex_fork_non_transition_events_do_not_mark_idle_again():
    manager = _make_manager()
    session = Session(
        id="cf4",
        name="codex-fork-cf4",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session

    calls = {"idle": 0, "active": 0}
    manager.message_queue_manager = SimpleNamespace(
        mark_session_idle=lambda _sid, **_kwargs: calls.__setitem__("idle", calls["idle"] + 1),
        mark_session_active=lambda _sid: calls.__setitem__("active", calls["active"] + 1),
    )

    manager.codex_fork_lifecycle[session.id] = {"state": "idle", "updated_at": datetime.now().isoformat()}
    manager.codex_fork_last_seq[session.id] = 10

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "session_configured",
            "seq": 11,
            "session_epoch": 1,
            "payload": {},
        },
    )

    assert calls["idle"] == 0
    assert calls["active"] == 0


def test_codex_fork_marks_queue_idle_on_real_transitions_and_active_on_running_events():
    manager = _make_manager()
    session = Session(
        id="cf5",
        name="codex-fork-cf5",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session

    calls = {"idle": 0, "active": 0}
    manager.message_queue_manager = SimpleNamespace(
        mark_session_idle=lambda _sid, **_kwargs: calls.__setitem__("idle", calls["idle"] + 1),
        mark_session_active=lambda _sid: calls.__setitem__("active", calls["active"] + 1),
    )

    manager.codex_fork_lifecycle[session.id] = {"state": "idle", "updated_at": datetime.now().isoformat()}

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "turn_started",
            "seq": 1,
            "session_epoch": 1,
            "payload": {},
        },
    )
    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "turn_delta",
            "seq": 2,
            "session_epoch": 1,
            "payload": {},
        },
    )
    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "turn_complete",
            "seq": 3,
            "session_epoch": 1,
            "payload": {},
        },
    )

    assert calls["active"] == 2
    assert calls["idle"] == 1


def test_codex_fork_non_transition_event_does_not_consume_stop_notify():
    manager = _make_manager()
    manager.message_queue_manager = MessageQueueManager(
        session_manager=manager,
        db_path=f"{manager._tmpdir.name}/mq_test.db",
        config={},
        notifier=None,
    )
    session = Session(
        id="cf6",
        name="codex-fork-cf6",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session

    # Prime lifecycle and queued stop-notify state.
    manager.codex_fork_lifecycle[session.id] = {"state": "idle", "updated_at": datetime.now().isoformat()}
    manager.codex_fork_last_seq[session.id] = 20
    state = manager.message_queue_manager._get_or_create_state(session.id)
    state.stop_notify_sender_id = "em-parent"
    state.stop_notify_sender_name = "em"

    with patch("asyncio.create_task", _noop_create_task), \
         patch.object(manager.message_queue_manager, "_send_stop_notification") as mock_notify:
        manager.ingest_codex_fork_event(
            session.id,
            {
                "event_type": "session_configured",
                "seq": 21,
                "session_epoch": 1,
                "payload": {},
            },
        )
        mock_notify.assert_not_called()

    # Stop notification must remain armed until a real idle transition.
    assert state.stop_notify_sender_id == "em-parent"


def test_codex_fork_non_transition_running_event_reactivates_stale_queue_state():
    manager = _make_manager()
    manager.message_queue_manager = MessageQueueManager(
        session_manager=manager,
        db_path=f"{manager._tmpdir.name}/mq_test.db",
        config={},
        notifier=None,
    )
    session = Session(
        id="cf7",
        name="codex-fork-cf7",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    manager.codex_fork_lifecycle[session.id] = {"state": "running", "updated_at": datetime.now().isoformat()}
    manager.codex_fork_last_seq[session.id] = 40
    manager.codex_fork_turns_in_flight.add(session.id)

    state = manager.message_queue_manager._get_or_create_state(session.id)
    state.is_idle = True

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "turn_delta",
            "seq": 41,
            "session_epoch": 1,
            "payload": {},
        },
    )

    assert manager.message_queue_manager.is_session_idle(session.id) is False


def test_codex_fork_turn_diff_reasserts_running_after_restart_without_turn_started():
    manager = _make_manager()
    session = Session(
        id="cf8",
        name="codex-fork-cf8",
        working_dir="/tmp",
        provider="codex-fork",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session
    manager.codex_fork_lifecycle[session.id] = {
        "state": "idle",
        "cause_event_type": "session_created",
        "updated_at": datetime.now().isoformat(),
    }
    manager.codex_fork_last_seq[session.id] = 10

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "turn_diff",
            "seq": 11,
            "session_epoch": 1,
            "payload": {},
        },
    )

    lifecycle = manager.get_codex_fork_lifecycle_state(session.id)
    assert lifecycle is not None
    assert lifecycle["state"] == "running"
    assert lifecycle["cause_event_type"] == "turn_diff"
    assert manager.get_activity_state(session.id) == "working"


def test_codex_fork_idle_reducer_checks_pane_for_background_terminal_work():
    manager = _make_manager()
    session = Session(
        id="cf-bg",
        name="codex-fork-cf-bg",
        working_dir="/tmp",
        tmux_session="codex-fork-cf-bg",
        provider="codex-fork",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session
    manager.codex_fork_lifecycle[session.id] = {
        "state": "idle",
        "cause_event_type": "turn_complete",
        "updated_at": datetime.now().isoformat(),
    }
    manager.tmux.capture_pane = MagicMock(
        return_value="• Working (17m 46s • esc to interrupt) · 1 background terminal running · /ps to view"
    )

    assert manager.get_activity_state(session.id) == "working"
    manager.tmux.capture_pane.assert_called_once_with(session.tmux_session, lines=8)


def test_codex_fork_idle_reducer_remains_idle_without_active_pane_markers():
    manager = _make_manager()
    session = Session(
        id="cf-bg-idle",
        name="codex-fork-cf-bg-idle",
        working_dir="/tmp",
        tmux_session="codex-fork-cf-bg-idle",
        provider="codex-fork",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session
    manager.codex_fork_lifecycle[session.id] = {
        "state": "idle",
        "cause_event_type": "turn_complete",
        "updated_at": datetime.now().isoformat(),
    }
    manager.tmux.capture_pane = MagicMock(return_value="› tab to queue message")

    assert manager.get_activity_state(session.id) == "idle"


def test_codex_fork_idle_reducer_ignores_stale_background_terminal_scrollback():
    manager = _make_manager()
    session = Session(
        id="cf-bg-stale",
        name="codex-fork-cf-bg-stale",
        working_dir="/tmp",
        tmux_session="codex-fork-cf-bg-stale",
        provider="codex-fork",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session
    manager.codex_fork_lifecycle[session.id] = {
        "state": "idle",
        "cause_event_type": "turn_complete",
        "updated_at": datetime.now().isoformat(),
    }
    manager.tmux.capture_pane = MagicMock(
        return_value="\n".join(
            [
                "• Working (17m 46s • esc to interrupt) · 1 background terminal running",
                "old transcript line",
                "• Waited for background terminal",
                "",
                "PR opened: https://github.com/example/repo/pull/1",
                "› tab to queue message",
            ]
        )
    )

    assert manager.get_activity_state(session.id) == "idle"
