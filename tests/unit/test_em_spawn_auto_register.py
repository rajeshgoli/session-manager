"""Unit tests for sm#277: auto-register remind + context monitoring when EM parent calls sm spawn."""

from unittest.mock import MagicMock, call, patch

import pytest

from src.cli.commands import cmd_spawn, _register_em_monitoring
from src.cli.dispatch import DEFAULT_DISPATCH_SOFT_THRESHOLD, DEFAULT_DISPATCH_HARD_THRESHOLD
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


def _make_em_session() -> dict:
    return {"id": "em0000aa", "is_em": True, "friendly_name": "em", "name": "claude-em0000aa"}


def _make_non_em_session() -> dict:
    return {"id": "eng111bb", "is_em": False, "friendly_name": "engineer-277", "name": "claude-eng111bb"}


_REMIND_OK = {"status": "registered"}

def _make_client(
    parent_session: dict | None = None,
    spawn_result: dict | None = None,
    spawn_unavailable: bool = False,
    remind_result=_REMIND_OK,
    cm_result: tuple = (None, True, False),
    ns_result: tuple = (True, False),
):
    """Build a mock SessionManagerClient.

    Pass remind_result=None to simulate a failed remind registration.
    """
    client = MagicMock()

    if spawn_unavailable:
        client.spawn_child.return_value = None
    else:
        client.spawn_child.return_value = spawn_result if spawn_result is not None else _SPAWN_RESULT_CLAUDE

    client.get_session.return_value = parent_session
    client.register_remind.return_value = remind_result
    client.set_context_monitor.return_value = cm_result
    client.arm_stop_notify.return_value = ns_result
    return client


# ---------------------------------------------------------------------------
# Tests: cmd_spawn EM auto-registration (sm#277)
# ---------------------------------------------------------------------------


class TestEmSpawnAutoRegister:
    """sm spawn auto-registers remind + context monitor + notify-on-stop when parent is EM."""

    @pytest.fixture(autouse=True)
    def patch_remind_config(self):
        """Patch get_auto_remind_config to return defaults, keeping tests hermetic."""
        with patch(
            "src.cli.dispatch.get_auto_remind_config",
            return_value=(DEFAULT_DISPATCH_SOFT_THRESHOLD, DEFAULT_DISPATCH_HARD_THRESHOLD),
        ):
            yield

    def test_em_parent_registers_remind(self):
        """When parent is_em=True, register_remind called with default soft/hard thresholds."""
        client = _make_client(parent_session=_make_em_session())
        rc = cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        assert rc == 0
        client.register_remind.assert_called_once_with(
            "child001",
            soft_threshold=DEFAULT_DISPATCH_SOFT_THRESHOLD,
            hard_threshold=DEFAULT_DISPATCH_HARD_THRESHOLD,
        )

    def test_em_parent_enables_context_monitoring(self):
        """When parent is_em=True, set_context_monitor called with notify → EM session."""
        client = _make_client(parent_session=_make_em_session())
        rc = cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        assert rc == 0
        client.set_context_monitor.assert_called_once_with(
            "child001",
            enabled=True,
            requester_session_id="em0000aa",
            notify_session_id="em0000aa",
        )

    def test_em_parent_arms_stop_notify(self):
        """When parent is_em=True, arm_stop_notify called pointing to EM session."""
        client = _make_client(parent_session=_make_em_session())
        rc = cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        assert rc == 0
        client.arm_stop_notify.assert_called_once_with(
            "child001",
            sender_session_id="em0000aa",
            requester_session_id="em0000aa",
        )

    def test_em_parent_calls_get_session_to_check_is_em(self):
        """After spawn, cmd_spawn looks up the parent session to check is_em."""
        client = _make_client(parent_session=_make_em_session())
        cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        client.get_session.assert_called_once_with("em0000aa")

    def test_non_em_parent_no_remind(self):
        """When parent is_em=False, register_remind is NOT called."""
        client = _make_client(parent_session=_make_non_em_session())
        rc = cmd_spawn(client, "eng111bb", "claude", "Implement feature X")
        assert rc == 0
        client.register_remind.assert_not_called()

    def test_non_em_parent_no_context_monitor(self):
        """When parent is_em=False, set_context_monitor is NOT called."""
        client = _make_client(parent_session=_make_non_em_session())
        rc = cmd_spawn(client, "eng111bb", "claude", "Implement feature X")
        assert rc == 0
        client.set_context_monitor.assert_not_called()

    def test_non_em_parent_no_arm_stop_notify(self):
        """When parent is_em=False, arm_stop_notify is NOT called."""
        client = _make_client(parent_session=_make_non_em_session())
        rc = cmd_spawn(client, "eng111bb", "claude", "Implement feature X")
        assert rc == 0
        client.arm_stop_notify.assert_not_called()

    def test_parent_session_not_found_no_registration(self):
        """If get_session returns None, no EM registration is attempted."""
        client = _make_client(parent_session=None)
        rc = cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        assert rc == 0
        client.register_remind.assert_not_called()
        client.set_context_monitor.assert_not_called()
        client.arm_stop_notify.assert_not_called()

    def test_spawn_failure_skips_registration(self):
        """If spawn fails, EM registration is NOT attempted."""
        client = _make_client(
            parent_session=_make_em_session(),
            spawn_result={"error": "spawn failed"},
        )
        rc = cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        assert rc == 1
        client.get_session.assert_not_called()
        client.register_remind.assert_not_called()

    def test_spawn_unavailable_skips_registration(self):
        """If session manager unavailable, EM registration is NOT attempted."""
        client = _make_client(parent_session=_make_em_session(), spawn_unavailable=True)
        rc = cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        assert rc == 2
        client.get_session.assert_not_called()
        client.register_remind.assert_not_called()

    def test_remind_failure_prints_warning_and_continues(self, capsys):
        """register_remind returning None prints a warning but cmd_spawn still returns 0."""
        client = _make_client(parent_session=_make_em_session(), remind_result=None)
        rc = cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        assert rc == 0
        err = capsys.readouterr().err
        assert "Warning" in err
        assert "remind" in err
        # Context monitor and stop notify still attempted
        client.set_context_monitor.assert_called_once()
        client.arm_stop_notify.assert_called_once()

    def test_context_monitor_failure_prints_warning_and_continues(self, capsys):
        """set_context_monitor returning not-ok prints a warning but cmd_spawn still returns 0."""
        client = _make_client(
            parent_session=_make_em_session(),
            cm_result=(None, False, False),
        )
        rc = cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        assert rc == 0
        err = capsys.readouterr().err
        assert "Warning" in err
        assert "context monitoring" in err
        # Stop notify still attempted
        client.arm_stop_notify.assert_called_once()

    def test_stop_notify_failure_prints_warning(self, capsys):
        """arm_stop_notify returning (False, False) prints a warning but cmd_spawn still returns 0."""
        client = _make_client(
            parent_session=_make_em_session(),
            ns_result=(False, False),
        )
        rc = cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        assert rc == 0
        err = capsys.readouterr().err
        assert "Warning" in err
        assert "stop notification" in err

    def test_stop_notify_unavailable_prints_warning(self, capsys):
        """arm_stop_notify returning (False, True) (server unavailable) still prints a warning."""
        client = _make_client(
            parent_session=_make_em_session(),
            ns_result=(False, True),
        )
        rc = cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        assert rc == 0
        err = capsys.readouterr().err
        assert "Warning" in err
        assert "stop notification" in err


class TestSpawnProviderAwareModel:
    """Provider-aware model handling for sm spawn (#290)."""

    @pytest.fixture(autouse=True)
    def patch_remind_config(self):
        with patch(
            "src.cli.dispatch.get_auto_remind_config",
            return_value=(DEFAULT_DISPATCH_SOFT_THRESHOLD, DEFAULT_DISPATCH_HARD_THRESHOLD),
        ):
            yield

    def test_claude_invalid_model_rejected(self, capsys):
        client = _make_client(parent_session=_make_non_em_session())

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
        client = _make_client(parent_session=_make_non_em_session(), spawn_result=spawn_result)

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
        )

    def test_codex_app_model_forwarded(self):
        client = _make_client(parent_session=_make_non_em_session(), spawn_result=_SPAWN_RESULT_CODEX_APP)

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
        )

    def test_codex_model_rejects_shell_metacharacters(self, capsys):
        client = _make_client(parent_session=_make_non_em_session())

        rc = cmd_spawn(client, "eng111bb", "codex", "Implement feature X", model="codex-5.1;touch_/tmp/pwned")

        assert rc == 1
        assert "invalid codex model" in capsys.readouterr().err.lower()
        client.spawn_child.assert_not_called()

    def test_codex_app_em_parent_also_registers(self):
        """EM registration also fires for codex-app spawned children."""
        client = _make_client(
            parent_session=_make_em_session(),
            spawn_result=_SPAWN_RESULT_CODEX_APP,
        )
        rc = cmd_spawn(client, "em0000aa", "codex-app", "Implement feature X")
        assert rc == 0
        client.register_remind.assert_called_once_with(
            "child002",
            soft_threshold=DEFAULT_DISPATCH_SOFT_THRESHOLD,
            hard_threshold=DEFAULT_DISPATCH_HARD_THRESHOLD,
        )
        client.set_context_monitor.assert_called_once()
        client.arm_stop_notify.assert_called_once()

    def test_config_override_thresholds_used(self, patch_remind_config):
        """Config-overridden thresholds from config.yaml flow through to register_remind."""
        # Override the autouse patch to return non-default values
        with patch("src.cli.dispatch.get_auto_remind_config", return_value=(300, 600)):
            client = _make_client(parent_session=_make_em_session())
            rc = cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        assert rc == 0
        client.register_remind.assert_called_once_with("child001", soft_threshold=300, hard_threshold=600)

    def test_output_unchanged_for_non_em(self, capsys):
        """cmd_spawn output is unchanged when parent is not EM."""
        client = _make_client(parent_session=_make_non_em_session())
        cmd_spawn(client, "eng111bb", "claude", "Implement feature X")
        out = capsys.readouterr().out
        assert "Spawned scout-277 (child001) in tmux session claude-child001" in out

    def test_output_unchanged_for_em(self, capsys):
        """cmd_spawn output still shows spawn line even when parent is EM."""
        client = _make_client(parent_session=_make_em_session())
        cmd_spawn(client, "em0000aa", "claude", "Implement feature X")
        out = capsys.readouterr().out
        assert "Spawned scout-277 (child001) in tmux session claude-child001" in out


# ---------------------------------------------------------------------------
# Tests: _register_em_monitoring helper
# ---------------------------------------------------------------------------


class TestRegisterEmMonitoring:
    """Unit tests for the _register_em_monitoring helper."""

    def test_remind_called_with_correct_thresholds(self):
        """register_remind called with provided soft/hard thresholds."""
        client = MagicMock()
        client.register_remind.return_value = {"status": "registered"}
        client.set_context_monitor.return_value = (None, True, False)
        client.arm_stop_notify.return_value = (True, False)

        _register_em_monitoring(
            client, "childXXX", "emYYY",
            DEFAULT_DISPATCH_SOFT_THRESHOLD, DEFAULT_DISPATCH_HARD_THRESHOLD,
        )
        client.register_remind.assert_called_once_with(
            "childXXX",
            soft_threshold=DEFAULT_DISPATCH_SOFT_THRESHOLD,
            hard_threshold=DEFAULT_DISPATCH_HARD_THRESHOLD,
        )

    def test_context_monitor_notify_points_to_em(self):
        """set_context_monitor notify_session_id points to EM session."""
        client = MagicMock()
        client.register_remind.return_value = {"status": "registered"}
        client.set_context_monitor.return_value = (None, True, False)
        client.arm_stop_notify.return_value = (True, False)

        _register_em_monitoring(
            client, "childXXX", "emYYY",
            DEFAULT_DISPATCH_SOFT_THRESHOLD, DEFAULT_DISPATCH_HARD_THRESHOLD,
        )
        client.set_context_monitor.assert_called_once_with(
            "childXXX",
            enabled=True,
            requester_session_id="emYYY",
            notify_session_id="emYYY",
        )

    def test_arm_stop_notify_points_to_em(self):
        """arm_stop_notify sender_session_id points to EM session."""
        client = MagicMock()
        client.register_remind.return_value = {"status": "registered"}
        client.set_context_monitor.return_value = (None, True, False)
        client.arm_stop_notify.return_value = (True, False)

        _register_em_monitoring(
            client, "childXXX", "emYYY",
            DEFAULT_DISPATCH_SOFT_THRESHOLD, DEFAULT_DISPATCH_HARD_THRESHOLD,
        )
        client.arm_stop_notify.assert_called_once_with(
            "childXXX",
            sender_session_id="emYYY",
            requester_session_id="emYYY",
        )


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
            json={"sender_session_id": "em001", "requester_session_id": "em001"},
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
            json={"sender_session_id": "em001", "requester_session_id": "em001"},
        )
        mock_sm.message_queue_manager.arm_stop_notify.assert_called_once()
        call_kwargs = mock_sm.message_queue_manager.arm_stop_notify.call_args
        assert call_kwargs.kwargs["session_id"] == "child001"
        assert call_kwargs.kwargs["sender_session_id"] == "em001"
        assert call_kwargs.kwargs["sender_name"] == "em"  # friendly_name takes precedence
