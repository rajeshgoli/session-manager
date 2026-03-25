"""Unit tests for sm spawn client wiring and server-side EM monitoring setup."""

from datetime import datetime
from unittest.mock import AsyncMock, MagicMock

import pytest

from src.cli.commands import cmd_spawn
from src.message_queue import MessageQueueManager
from src.models import Session, SessionStatus


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_SPAWN_RESULT_CLAUDE = {
    "session_id": "child001",
    "name": "claude-child001",
    "friendly_name": "scout-277",
    "tmux_session": "claude-child001",
    "provider": "claude",
}

_SPAWN_RESULT_CODEX_APP = {
    "session_id": "child002",
    "name": "codex-child002",
    "friendly_name": None,
    "provider": "codex-app",
}

def _make_client(
    spawn_result: dict | None = None,
    spawn_unavailable: bool = False,
):
    """Build a mock SessionManagerClient for cmd_spawn."""
    client = MagicMock()

    if spawn_unavailable:
        client.spawn_child.return_value = None
    else:
        client.spawn_child.return_value = spawn_result if spawn_result is not None else _SPAWN_RESULT_CLAUDE
    return client


class TestCmdSpawnClientWiring:
    """sm spawn delegates all monitoring setup to the server-side spawn flow."""

    def test_track_is_forwarded_in_spawn_request(self):
        client = _make_client()

        rc = cmd_spawn(client, "eng111bb", "claude", "Implement feature X", track_seconds=300)

        assert rc == 0
        client.spawn_child.assert_called_once_with(
            parent_session_id="eng111bb",
            prompt="Implement feature X",
            name=None,
            wait=None,
            model=None,
            working_dir=None,
            provider="claude",
            track_seconds=300,
        )

    def test_no_client_side_monitoring_followups_after_spawn(self):
        client = _make_client()

        rc = cmd_spawn(client, "em0000aa", "claude", "Implement feature X", track_seconds=300)

        assert rc == 0
        client.get_session.assert_not_called()
        client.register_remind.assert_not_called()
        client.set_context_monitor.assert_not_called()
        client.arm_stop_notify.assert_not_called()

    def test_spawn_warning_payload_is_printed(self, capsys):
        client = _make_client(spawn_result={**_SPAWN_RESULT_CLAUDE, "warnings": ["monitoring degraded"]})

        rc = cmd_spawn(client, "eng111bb", "claude", "Implement feature X")

        assert rc == 0
        assert "monitoring degraded" in capsys.readouterr().err

    def test_spawn_failure_skips_followup_work(self):
        client = _make_client(spawn_result={"error": "spawn failed"})

        rc = cmd_spawn(client, "eng111bb", "claude", "Implement feature X")

        assert rc == 1
        client.get_session.assert_not_called()
        client.register_remind.assert_not_called()

    def test_spawn_unavailable_skips_followup_work(self):
        client = _make_client(spawn_unavailable=True)

        rc = cmd_spawn(client, "eng111bb", "claude", "Implement feature X")

        assert rc == 2
        client.get_session.assert_not_called()
        client.register_remind.assert_not_called()


class TestSpawnProviderAwareModel:
    """Provider-aware model handling for sm spawn (#290)."""

    def test_claude_invalid_model_rejected(self, capsys):
        client = _make_client()

        rc = cmd_spawn(client, "eng111bb", "claude", "Implement feature X", model="codex-5.1\x1b[31m")

        assert rc == 1
        err = capsys.readouterr().err.lower()
        assert "invalid claude model" in err
        assert "\x1b" not in err
        client.spawn_child.assert_not_called()

    def test_codex_model_forwarded(self):
        spawn_result = dict(_SPAWN_RESULT_CLAUDE)
        spawn_result["provider"] = "codex"
        spawn_result["name"] = "codex-child001"
        spawn_result["tmux_session"] = "codex-child001"
        client = _make_client(spawn_result=spawn_result)

        rc = cmd_spawn(client, "eng111bb", "codex", "Implement feature X", model="codex-5.1")

        assert rc == 0
        client.spawn_child.assert_called_once_with(
            parent_session_id="eng111bb",
            prompt="Implement feature X",
            name=None,
            wait=None,
            model="codex-5.1",
            working_dir=None,
            provider="codex",
            track_seconds=None,
        )

    def test_codex_app_model_forwarded(self):
        client = _make_client(spawn_result=_SPAWN_RESULT_CODEX_APP)

        rc = cmd_spawn(client, "eng111bb", "codex-app", "Implement feature X", model="codex-5.1")

        assert rc == 0
        client.spawn_child.assert_called_once_with(
            parent_session_id="eng111bb",
            prompt="Implement feature X",
            name=None,
            wait=None,
            model="codex-5.1",
            working_dir=None,
            provider="codex-app",
            track_seconds=None,
        )

    def test_codex_model_rejects_shell_metacharacters(self, capsys):
        client = _make_client()

        rc = cmd_spawn(client, "eng111bb", "codex", "Implement feature X", model="codex-5.1;touch_/tmp/pwned")

        assert rc == 1
        assert "invalid codex model" in capsys.readouterr().err.lower()
        client.spawn_child.assert_not_called()

    def test_codex_app_em_parent_still_uses_server_side_spawn(self):
        """codex-app spawn keeps using the same spawn endpoint contract."""
        client = _make_client(spawn_result=_SPAWN_RESULT_CODEX_APP)
        rc = cmd_spawn(client, "em0000aa", "codex-app", "Implement feature X")
        assert rc == 0
        client.spawn_child.assert_called_once()

    def test_output_unchanged_for_non_em(self, capsys):
        """cmd_spawn output is unchanged when parent is not EM."""
        client = _make_client()
        cmd_spawn(client, "eng111bb", "claude", "Implement feature X")
        out = capsys.readouterr().out
        assert "Spawned scout-277 (child001) in tmux session claude-child001" in out

    def test_output_unchanged_for_em(self, capsys):
        """cmd_spawn output still shows spawn line even when parent is EM."""
        client = _make_client()
        cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        out = capsys.readouterr().out
        assert "Spawned scout-277 (child001) in tmux session claude-child001" in out


# ---------------------------------------------------------------------------
# Tests: server-side spawn monitoring registration (sm#465)
# ---------------------------------------------------------------------------


class TestSpawnEndpointMonitoring:
    """Spawn endpoint wires tracking and EM monitoring atomically server-side."""

    @pytest.fixture
    def app_client(self):
        from fastapi.testclient import TestClient
        from src.server import create_app

        app = create_app({})
        mock_sm = MagicMock()
        mock_sm._save_state = MagicMock()
        mock_sm.message_queue_manager = MagicMock()
        mock_output_monitor = MagicMock()
        mock_output_monitor.start_monitoring = AsyncMock()
        app.state.session_manager = mock_sm
        app.state.output_monitor = mock_output_monitor
        return TestClient(app), mock_sm, mock_output_monitor

    def test_spawn_with_track_registers_server_side_tracking(self, app_client):
        tc, mock_sm, _ = app_client
        parent = Session(
            id="eng111bb",
            name="claude-eng111bb",
            working_dir="/tmp/parent",
            tmux_session="claude-eng111bb",
            log_file="/tmp/parent.log",
            status=SessionStatus.IDLE,
        )
        child = Session(
            id="child456",
            name="claude-child456",
            working_dir="/tmp/parent",
            tmux_session="claude-child456",
            log_file="/tmp/child.log",
            status=SessionStatus.RUNNING,
            parent_session_id="eng111bb",
            spawned_at=datetime.now(),
        )
        mock_sm.get_session.side_effect = lambda sid: {"eng111bb": parent, "child456": child}.get(sid)
        mock_sm.spawn_child_session = AsyncMock(return_value=child)

        response = tc.post(
            "/sessions/spawn",
            json={
                "parent_session_id": "eng111bb",
                "prompt": "Implement feature X",
                "track_seconds": 300,
            },
        )

        assert response.status_code == 200
        mock_sm.spawn_child_session.assert_awaited_once_with(
            parent_session_id="eng111bb",
            prompt="Implement feature X",
            name=None,
            wait=None,
            model=None,
            working_dir="/tmp/parent",
            provider=None,
            defer_telegram_topic=True,
        )
        mock_sm.message_queue_manager.register_periodic_remind.assert_called_once_with(
            target_session_id="child456",
            soft_threshold=300,
            hard_threshold=600,
            cancel_on_reply_session_id="eng111bb",
        )
        assert child.context_monitor_enabled is False
        mock_sm.message_queue_manager.arm_stop_notify.assert_not_called()

    def test_spawn_em_parent_registers_server_side_monitoring(self, app_client):
        tc, mock_sm, _ = app_client
        parent = Session(
            id="em0000aa",
            name="claude-em0000aa",
            working_dir="/tmp/parent",
            tmux_session="claude-em0000aa",
            log_file="/tmp/parent.log",
            status=SessionStatus.IDLE,
            is_em=True,
        )
        child = Session(
            id="child456",
            name="claude-child456",
            working_dir="/tmp/parent",
            tmux_session="claude-child456",
            log_file="/tmp/child.log",
            status=SessionStatus.RUNNING,
            parent_session_id="em0000aa",
            spawned_at=datetime.now(),
        )
        mock_sm.get_session.side_effect = lambda sid: {"em0000aa": parent, "child456": child}.get(sid)
        mock_sm.spawn_child_session = AsyncMock(return_value=child)

        with pytest.MonkeyPatch.context() as mp:
            mp.setattr("src.server.get_auto_remind_config", lambda working_dir: (210, 420))
            response = tc.post(
                "/sessions/spawn",
                json={
                    "parent_session_id": "em0000aa",
                    "prompt": "Implement feature X",
                },
            )

        assert response.status_code == 200
        mock_sm.message_queue_manager.register_periodic_remind.assert_called_once_with(
            target_session_id="child456",
            soft_threshold=210,
            hard_threshold=420,
        )
        assert child.context_monitor_enabled is True
        assert child.context_monitor_notify == "em0000aa"
        mock_sm.message_queue_manager.arm_stop_notify.assert_called_once()

    def test_spawn_returns_warning_when_tracking_registration_raises(self, app_client):
        tc, mock_sm, _ = app_client
        parent = Session(
            id="eng111bb",
            name="claude-eng111bb",
            working_dir="/tmp/parent",
            tmux_session="claude-eng111bb",
            log_file="/tmp/parent.log",
            status=SessionStatus.IDLE,
        )
        child = Session(
            id="child456",
            name="claude-child456",
            working_dir="/tmp/parent",
            tmux_session="claude-child456",
            log_file="/tmp/child.log",
            status=SessionStatus.RUNNING,
            parent_session_id="eng111bb",
            spawned_at=datetime.now(),
        )
        mock_sm.get_session.side_effect = lambda sid: {"eng111bb": parent, "child456": child}.get(sid)
        mock_sm.spawn_child_session = AsyncMock(return_value=child)
        mock_sm.message_queue_manager.register_periodic_remind.side_effect = RuntimeError("sqlite busy")

        response = tc.post(
            "/sessions/spawn",
            json={
                "parent_session_id": "eng111bb",
                "prompt": "Implement feature X",
                "track_seconds": 300,
            },
        )

        assert response.status_code == 200
        data = response.json()
        assert data["session_id"] == "child456"
        assert data["warnings"] == ["failed to register spawn tracking"]


# ---------------------------------------------------------------------------
# Tests: MessageQueueManager.arm_stop_notify (sm#277)
# ---------------------------------------------------------------------------


def _make_mq(tmp_path) -> MessageQueueManager:
    mock_sm = MagicMock()
    mock_sm.sessions = {}
    mock_sm.get_session = MagicMock(return_value=None)
    return MessageQueueManager(
        session_manager=mock_sm,
        db_path=str(tmp_path / "test_arm_stop.db"),
        config={},
        notifier=None,
    )


class TestArmStopNotify:
    """Unit tests for MessageQueueManager.arm_stop_notify."""

    def test_sets_stop_notify_sender_id(self, tmp_path):
        """arm_stop_notify sets stop_notify_sender_id in delivery state."""
        mq = _make_mq(tmp_path)
        mq.arm_stop_notify("sessionA", "em0000aa", sender_name="em")
        state = mq.delivery_states.get("sessionA")
        assert state is not None
        assert state.stop_notify_sender_id == "em0000aa"

    def test_sets_stop_notify_sender_name(self, tmp_path):
        """arm_stop_notify sets stop_notify_sender_name in delivery state."""
        mq = _make_mq(tmp_path)
        mq.arm_stop_notify("sessionA", "em0000aa", sender_name="em-271")
        state = mq.delivery_states["sessionA"]
        assert state.stop_notify_sender_name == "em-271"

    def test_empty_sender_name_defaults(self, tmp_path):
        """arm_stop_notify with no sender_name sets sender_name to empty string."""
        mq = _make_mq(tmp_path)
        mq.arm_stop_notify("sessionA", "em0000aa")
        state = mq.delivery_states["sessionA"]
        assert state.stop_notify_sender_name == ""

    def test_overwrites_existing_stop_notify(self, tmp_path):
        """arm_stop_notify replaces any prior stop_notify_sender_id."""
        mq = _make_mq(tmp_path)
        mq.arm_stop_notify("sessionA", "old_em", sender_name="old")
        mq.arm_stop_notify("sessionA", "new_em", sender_name="new")
        state = mq.delivery_states["sessionA"]
        assert state.stop_notify_sender_id == "new_em"
        assert state.stop_notify_sender_name == "new"

    def test_creates_delivery_state_if_missing(self, tmp_path):
        """arm_stop_notify creates delivery state for session if not yet tracked."""
        mq = _make_mq(tmp_path)
        assert "sessionA" not in mq.delivery_states
        mq.arm_stop_notify("sessionA", "em0000aa")
        assert "sessionA" in mq.delivery_states


# ---------------------------------------------------------------------------
# Tests: Server endpoint POST /sessions/{id}/notify-on-stop
# ---------------------------------------------------------------------------

class TestArmStopNotifyEndpoint:
    """Tests for the server-side POST /sessions/{session_id}/notify-on-stop endpoint."""

    @pytest.fixture
    def app_client(self):
        """Set up a FastAPI test client with mocked session manager."""
        from fastapi.testclient import TestClient
        from src.server import create_app

        config = {}
        mock_sm = MagicMock()
        mock_sm.message_queue_manager = MagicMock()

        app = create_app(config)

        # EM parent session
        em_session = MagicMock()
        em_session.is_em = True
        em_session.friendly_name = "em"
        em_session.name = "claude-em001"

        # Child session (parent = EM)
        child_session = MagicMock()
        child_session.parent_session_id = "em001"

        def get_session(sid):
            return {"em001": em_session, "child001": child_session}.get(sid)

        mock_sm.get_session.side_effect = get_session
        app.state.session_manager = mock_sm

        return TestClient(app), mock_sm

    def test_arm_stop_notify_success(self, app_client):
        """POST /sessions/{id}/notify-on-stop returns 200 for valid EM request."""
        tc, mock_sm = app_client
        response = tc.post(
            "/sessions/child001/notify-on-stop",
            json={"sender_session_id": "em001", "requester_session_id": "em001", "delay_seconds": 8},
        )
        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "ok"
        assert data["session_id"] == "child001"
        assert data["sender_session_id"] == "em001"

    def test_arm_stop_notify_unknown_session_404(self, app_client):
        """POST /sessions/unknown/notify-on-stop → 404 when session not found."""
        tc, _ = app_client
        response = tc.post(
            "/sessions/unknown999/notify-on-stop",
            json={"sender_session_id": "em001", "requester_session_id": "em001"},
        )
        assert response.status_code == 404

    def test_arm_stop_notify_non_em_requester_403(self, app_client):
        """Non-EM requester → 403 Forbidden."""
        tc, mock_sm = app_client

        non_em = MagicMock()
        non_em.is_em = False

        original_side_effect = mock_sm.get_session.side_effect

        def get_session_with_non_em(sid):
            if sid == "non_em_001":
                return non_em
            return original_side_effect(sid)

        mock_sm.get_session.side_effect = get_session_with_non_em

        response = tc.post(
            "/sessions/child001/notify-on-stop",
            json={"sender_session_id": "non_em_001", "requester_session_id": "non_em_001"},
        )
        assert response.status_code == 403

    def test_arm_stop_notify_non_parent_em_403(self, app_client):
        """EM requester that is not the parent of the target session → 403."""
        tc, mock_sm = app_client

        other_em = MagicMock()
        other_em.is_em = True

        original_side_effect = mock_sm.get_session.side_effect

        def get_session_with_other_em(sid):
            if sid == "other_em":
                return other_em
            return original_side_effect(sid)

        mock_sm.get_session.side_effect = get_session_with_other_em

        response = tc.post(
            "/sessions/child001/notify-on-stop",
            json={"sender_session_id": "other_em", "requester_session_id": "other_em"},
        )
        assert response.status_code == 403

    def test_arm_stop_notify_unknown_sender_422(self, app_client):
        """POST /sessions/{id}/notify-on-stop → 422 when sender_session_id doesn't exist."""
        tc, _ = app_client
        response = tc.post(
            "/sessions/child001/notify-on-stop",
            json={"sender_session_id": "nonexistent_sender", "requester_session_id": "em001"},
        )
        assert response.status_code == 422

    def test_arm_stop_notify_calls_queue_mgr(self, app_client):
        """POST /sessions/{id}/notify-on-stop calls queue_mgr.arm_stop_notify."""
        tc, mock_sm = app_client
        # Set sender name
        mock_sm.get_session.side_effect = lambda sid: {
            "em001": MagicMock(is_em=True, friendly_name="em", name="claude-em001"),
            "child001": MagicMock(parent_session_id="em001"),
        }.get(sid)

        tc.post(
            "/sessions/child001/notify-on-stop",
            json={"sender_session_id": "em001", "requester_session_id": "em001", "delay_seconds": 8},
        )
        mock_sm.message_queue_manager.arm_stop_notify.assert_called_once()
        call_kwargs = mock_sm.message_queue_manager.arm_stop_notify.call_args
        assert call_kwargs.kwargs["session_id"] == "child001"
        assert call_kwargs.kwargs["sender_session_id"] == "em001"
        assert call_kwargs.kwargs["sender_name"] == "em"  # friendly_name takes precedence
        assert call_kwargs.kwargs["delay_seconds"] == 8

    def test_arm_stop_notify_codex_fork_target_is_suppressed(self, app_client):
        """POST /sessions/{id}/notify-on-stop returns suppressed for codex-fork targets."""
        tc, mock_sm = app_client

        child_session = MagicMock()
        child_session.parent_session_id = "em001"
        child_session.provider = "codex-fork"
        mock_sm.get_session.side_effect = lambda sid: {
            "em001": MagicMock(is_em=True, friendly_name="em", name="claude-em001"),
            "child001": child_session,
        }.get(sid)

        response = tc.post(
            "/sessions/child001/notify-on-stop",
            json={"sender_session_id": "em001", "requester_session_id": "em001", "delay_seconds": 8},
        )

        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "suppressed"
        assert data["reason"] == "notify_on_stop disabled for codex-fork sessions"
        mock_sm.message_queue_manager.arm_stop_notify.assert_not_called()
