"""Unit tests for sm#203 context-aware handoff triggering.

Tests for POST /hooks/context-usage endpoint covering:
- Warning at 50%, suppressed on repeat
- Critical at 65%, suppressed on repeat
- Flag reset on compaction event
- Flag reset on context_reset event
- Edge cases: null used_percentage, unknown session, no queue_mgr
"""

import pytest
from unittest.mock import MagicMock, patch
from fastapi.testclient import TestClient

from src.models import Session, SessionStatus
from src.server import create_app


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def _make_session(session_id: str = "abc12345") -> Session:
    """Create a Session object for testing."""
    s = Session(
        id=session_id,
        name=f"claude-{session_id}",
        working_dir="/tmp/test",
        tmux_session=f"claude-{session_id}",
        provider="claude",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
    )
    # Default: registered for context monitoring so existing tests pass the new gate (#206)
    s.context_monitor_enabled = True
    s.context_monitor_notify = session_id
    return s


@pytest.fixture
def session():
    return _make_session()


@pytest.fixture
def mock_session_manager(session):
    """Mock SessionManager with one session and a queue manager."""
    mock = MagicMock()
    mock.sessions = {session.id: session}
    mock.get_session = MagicMock(return_value=session)
    mock._save_state = MagicMock()
    mock.message_queue_manager = MagicMock()
    return mock


@pytest.fixture
def app(mock_session_manager):
    return create_app(session_manager=mock_session_manager)


@pytest.fixture
def client(app):
    return TestClient(app)


def _post_context(client, session_id: str, used_pct, **extra):
    """Helper: POST a context usage update."""
    payload = {"session_id": session_id, "used_percentage": used_pct, **extra}
    return client.post("/hooks/context-usage", json=payload)


def _post_event(client, session_id: str, event: str, **extra):
    """Helper: POST a named event (compaction / context_reset)."""
    payload = {"session_id": session_id, "event": event, **extra}
    return client.post("/hooks/context-usage", json=payload)


# ---------------------------------------------------------------------------
# 1. Warning at 50% (one-shot)
# ---------------------------------------------------------------------------


class TestWarningThreshold:
    """Warning fires at 50%, suppressed on repeat calls."""

    def test_warning_fires_at_50_pct(self, client, mock_session_manager, session):
        resp = _post_context(client, session.id, used_pct=50, total_input_tokens=100_000)
        assert resp.status_code == 200
        assert resp.json()["status"] == "ok"

        queue_mgr = mock_session_manager.message_queue_manager
        assert queue_mgr.queue_message.called
        call_kwargs = queue_mgr.queue_message.call_args[1]
        assert "[sm context]" in call_kwargs["text"]
        assert "50%" in call_kwargs["text"]
        assert call_kwargs["delivery_mode"] == "sequential"
        assert call_kwargs["target_session_id"] == session.id

    def test_warning_flag_set_after_firing(self, client, session):
        _post_context(client, session.id, used_pct=50)
        assert session._context_warning_sent is True

    def test_warning_suppressed_on_repeat(self, client, mock_session_manager, session):
        _post_context(client, session.id, used_pct=50)
        _post_context(client, session.id, used_pct=55)

        queue_mgr = mock_session_manager.message_queue_manager
        # Should only have been called once
        assert queue_mgr.queue_message.call_count == 1

    def test_below_warning_no_message(self, client, mock_session_manager, session):
        _post_context(client, session.id, used_pct=49)

        queue_mgr = mock_session_manager.message_queue_manager
        assert not queue_mgr.queue_message.called
        assert session._context_warning_sent is False


# ---------------------------------------------------------------------------
# 2. Critical at 65% (one-shot, urgent)
# ---------------------------------------------------------------------------


class TestCriticalThreshold:
    """Critical fires at 65%, is urgent, suppressed on repeat."""

    def test_critical_fires_at_65_pct(self, client, mock_session_manager, session):
        resp = _post_context(client, session.id, used_pct=65)
        assert resp.status_code == 200

        queue_mgr = mock_session_manager.message_queue_manager
        assert queue_mgr.queue_message.called
        call_kwargs = queue_mgr.queue_message.call_args[1]
        assert "critically high" in call_kwargs["text"].lower()
        assert call_kwargs["delivery_mode"] == "urgent"

    def test_critical_flag_set_after_firing(self, client, session):
        _post_context(client, session.id, used_pct=65)
        assert session._context_critical_sent is True

    def test_critical_suppressed_on_repeat(self, client, mock_session_manager, session):
        _post_context(client, session.id, used_pct=65)
        _post_context(client, session.id, used_pct=80)

        queue_mgr = mock_session_manager.message_queue_manager
        assert queue_mgr.queue_message.call_count == 1

    def test_critical_takes_precedence_over_warning(self, client, mock_session_manager, session):
        """At 65% both thresholds crossed — critical fires (not warning)."""
        _post_context(client, session.id, used_pct=65)

        queue_mgr = mock_session_manager.message_queue_manager
        assert queue_mgr.queue_message.call_count == 1
        call_kwargs = queue_mgr.queue_message.call_args[1]
        assert call_kwargs["delivery_mode"] == "urgent"

    def test_warning_then_critical_two_messages(self, client, mock_session_manager, session):
        """Warning at 50%, then critical at 65% → two separate messages."""
        _post_context(client, session.id, used_pct=50)
        _post_context(client, session.id, used_pct=65)

        queue_mgr = mock_session_manager.message_queue_manager
        assert queue_mgr.queue_message.call_count == 2


# ---------------------------------------------------------------------------
# 3. Flag reset on compaction event
# ---------------------------------------------------------------------------


class TestCompactionEvent:
    """Compaction event resets both flags."""

    def test_compaction_resets_warning_flag(self, client, session):
        session._context_warning_sent = True
        _post_event(client, session.id, event="compaction", trigger="auto")
        assert session._context_warning_sent is False

    def test_compaction_resets_critical_flag(self, client, session):
        session._context_critical_sent = True
        _post_event(client, session.id, event="compaction", trigger="auto")
        assert session._context_critical_sent is False

    def test_compaction_resets_both_flags(self, client, session):
        session._context_warning_sent = True
        session._context_critical_sent = True
        _post_event(client, session.id, event="compaction", trigger="auto")
        assert session._context_warning_sent is False
        assert session._context_critical_sent is False

    def test_compaction_returns_logged_status(self, client, session):
        resp = _post_event(client, session.id, event="compaction", trigger="auto")
        assert resp.json()["status"] == "compaction_logged"

    def test_compaction_notifies_parent(self, client, mock_session_manager, session):
        """When session has context_monitor_notify set, compaction sends a notification."""
        session.context_monitor_notify = "parent999"
        _post_event(client, session.id, event="compaction", trigger="auto")

        queue_mgr = mock_session_manager.message_queue_manager
        assert queue_mgr.queue_message.called
        call_kwargs = queue_mgr.queue_message.call_args[1]
        assert call_kwargs["target_session_id"] == "parent999"
        assert "Compaction fired" in call_kwargs["text"] or "compaction" in call_kwargs["text"].lower()

    def test_compaction_notification_wording(self, client, mock_session_manager, session):
        """Compaction notification says 'compacted — agent is still running', not 'Context was lost'."""
        session.context_monitor_notify = "parent999"
        _post_event(client, session.id, event="compaction", trigger="auto")

        queue_mgr = mock_session_manager.message_queue_manager
        text = queue_mgr.queue_message.call_args[1]["text"]
        assert "Context was compacted — agent is still running." in text
        assert "Context was lost" not in text

    def test_compaction_no_notify_no_notification(self, client, mock_session_manager, session):
        """Without context_monitor_notify, no notification is sent."""
        session.context_monitor_notify = None
        _post_event(client, session.id, event="compaction", trigger="auto")

        queue_mgr = mock_session_manager.message_queue_manager
        assert not queue_mgr.queue_message.called

    def test_warning_fires_after_compaction_reset(self, client, mock_session_manager, session):
        """After compaction resets flags, next 50% crossing sends warning again."""
        # Suppress compaction notification so we only count warning calls
        session.context_monitor_notify = None
        # Arm warning
        _post_context(client, session.id, used_pct=50)
        assert session._context_warning_sent is True

        # Compaction fires — resets flags
        _post_event(client, session.id, event="compaction", trigger="auto")
        assert session._context_warning_sent is False

        # Re-enable routing for the post-compaction warning
        session.context_monitor_notify = session.id
        # New cycle: warning fires again (even at same percentage)
        _post_context(client, session.id, used_pct=55)
        # Total queue_message calls: warning (1) + compaction notification (0, notify=None) + warning again (1) = 2
        queue_mgr = mock_session_manager.message_queue_manager
        assert queue_mgr.queue_message.call_count == 2

    def test_compaction_resets_even_if_post_compaction_above_warning(self, client, mock_session_manager, session):
        """Anti-regression: flags reset even when post-compaction used_pct stays above warning_pct.

        Post-compaction context can land at 55K–110K tokens (out of 200K = 27%–55%).
        With warning at 50%, flags MUST reset via PreCompact — not via used_pct < warning_pct.
        """
        session._context_warning_sent = True
        session._context_critical_sent = True

        # Suppress compaction notification so only warning calls are counted below
        session.context_monitor_notify = None

        # Simulate: compaction fires, then status line reports 55% (above warning threshold)
        _post_event(client, session.id, event="compaction", trigger="auto")
        assert session._context_warning_sent is False  # Reset by compaction, not by pct check
        assert session._context_critical_sent is False

        # Re-enable routing for the post-compaction warning
        session.context_monitor_notify = session.id

        # The status line update at 55% should now fire warning (flags were reset)
        _post_context(client, session.id, used_pct=55)
        queue_mgr = mock_session_manager.message_queue_manager
        # Warning should have fired once (after reset)
        warning_calls = [
            c for c in queue_mgr.queue_message.call_args_list
            if c[1].get("delivery_mode") == "sequential"
        ]
        assert len(warning_calls) == 1


# ---------------------------------------------------------------------------
# 4. Flag reset on context_reset event (manual /clear)
# ---------------------------------------------------------------------------


class TestContextResetEvent:
    """context_reset event (SessionStart clear) resets both flags."""

    def test_context_reset_resets_warning_flag(self, client, session):
        session._context_warning_sent = True
        _post_event(client, session.id, event="context_reset")
        assert session._context_warning_sent is False

    def test_context_reset_resets_critical_flag(self, client, session):
        session._context_critical_sent = True
        _post_event(client, session.id, event="context_reset")
        assert session._context_critical_sent is False

    def test_context_reset_returns_flags_reset_status(self, client, session):
        resp = _post_event(client, session.id, event="context_reset")
        assert resp.json()["status"] == "flags_reset"

    def test_warning_fires_again_after_clear(self, client, mock_session_manager, session):
        """After /clear re-arms flags, next 50% crossing sends warning again."""
        _post_context(client, session.id, used_pct=50)
        assert session._context_warning_sent is True

        # User runs /clear → context_reset
        _post_event(client, session.id, event="context_reset")
        assert session._context_warning_sent is False

        # New cycle starts — warning fires again
        _post_context(client, session.id, used_pct=52)
        queue_mgr = mock_session_manager.message_queue_manager
        assert queue_mgr.queue_message.call_count == 2


# ---------------------------------------------------------------------------
# 5. Edge cases
# ---------------------------------------------------------------------------


class TestEdgeCases:
    """Edge cases: null pct, unknown session, no queue_mgr."""

    def test_null_used_percentage_returns_ok(self, client):
        """used_percentage=null (before first API call) should be handled gracefully."""
        resp = client.post("/hooks/context-usage", json={"session_id": "abc12345"})
        # used_percentage is absent/null — endpoint should return ok, not 500
        assert resp.status_code == 200
        assert resp.json()["status"] == "ok"
        assert resp.json()["used_percentage"] is None

    def test_unknown_session_returns_unknown_session(self, client, mock_session_manager):
        """Session not tracked by sm returns unknown_session, doesn't crash."""
        mock_session_manager.get_session.return_value = None
        resp = _post_context(client, "ghost999", used_pct=60)
        assert resp.status_code == 200
        assert resp.json()["status"] == "unknown_session"

    def test_no_queue_mgr_no_crash(self, session):
        """With message_queue_manager=None, endpoint doesn't crash."""
        mock_sm = MagicMock()
        mock_sm.get_session.return_value = session
        mock_sm.message_queue_manager = None
        app = create_app(session_manager=mock_sm)
        client = TestClient(app)

        resp = _post_context(client, session.id, used_pct=65)
        assert resp.status_code == 200
        # No crash — critical threshold crossed but no queue to send to
        assert session._context_critical_sent is True

    def test_tokens_used_updated_on_context_update(self, client, session):
        """tokens_used field on Session is updated from the status line payload."""
        _post_context(client, session.id, used_pct=30, total_input_tokens=60_000)
        assert session.tokens_used == 60_000

    def test_zero_pct_no_warning(self, client, mock_session_manager, session):
        """used_percentage=0 should not fire any warning."""
        _post_context(client, session.id, used_pct=0)
        queue_mgr = mock_session_manager.message_queue_manager
        assert not queue_mgr.queue_message.called

    def test_custom_thresholds_from_config(self, session):
        """Config overrides: warning_percentage=40, critical_percentage=60."""
        mock_sm = MagicMock()
        mock_sm.get_session.return_value = session
        mock_sm.message_queue_manager = MagicMock()

        config = {"context_monitor": {"warning_percentage": 40, "critical_percentage": 60}}
        app = create_app(session_manager=mock_sm, config=config)
        client = TestClient(app)

        # At 40%, warning fires
        _post_context(client, session.id, used_pct=40)
        queue_mgr = mock_sm.message_queue_manager
        assert queue_mgr.queue_message.called
        call_kwargs = queue_mgr.queue_message.call_args[1]
        assert call_kwargs["delivery_mode"] == "sequential"

    def test_no_session_manager_returns_unknown(self):
        """With no session_manager configured, endpoint returns unknown_session."""
        app = create_app(session_manager=None)
        client = TestClient(app)
        resp = _post_context(client, "abc", used_pct=50)
        assert resp.status_code == 200
        assert resp.json()["status"] == "unknown_session"


# ---------------------------------------------------------------------------
# 6. Anti-regression: no flag reset via used_pct < warning_pct
# ---------------------------------------------------------------------------


class TestAntiRegression:
    """Flags must NOT be reset by the status line update path (only by events)."""

    def test_flags_not_reset_by_low_pct(self, client, session):
        """Sending used_pct=10 must not reset flags set before."""
        session._context_warning_sent = True
        session._context_critical_sent = True

        _post_context(client, session.id, used_pct=10)

        # Flags must remain set — only events reset them
        assert session._context_warning_sent is True
        assert session._context_critical_sent is True

    def test_flags_not_reset_by_zero_pct(self, client, session):
        """used_percentage=0 must not reset flags."""
        session._context_warning_sent = True
        session._context_critical_sent = True

        _post_context(client, session.id, used_pct=0)

        assert session._context_warning_sent is True
        assert session._context_critical_sent is True


# ---------------------------------------------------------------------------
# 7. SessionResponse includes last_handoff_path
# ---------------------------------------------------------------------------


class TestSessionResponseLastHandoffPath:
    """last_handoff_path is exposed in GET /sessions/{id}."""

    def test_last_handoff_path_in_response(self, mock_session_manager, session):
        session.last_handoff_path = "/tmp/handoff.md"
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        resp = client.get(f"/sessions/{session.id}")
        assert resp.status_code == 200
        assert resp.json()["last_handoff_path"] == "/tmp/handoff.md"

    def test_last_handoff_path_null_when_unset(self, mock_session_manager, session):
        session.last_handoff_path = None
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        resp = client.get(f"/sessions/{session.id}")
        assert resp.status_code == 200
        assert resp.json()["last_handoff_path"] is None


# ---------------------------------------------------------------------------
# 8. Registration gate (#206)
# ---------------------------------------------------------------------------


class TestRegistrationGate:
    """Gate: unregistered sessions return not_registered, no queue calls."""

    def test_unregistered_session_returns_not_registered(self, mock_session_manager, session):
        session.context_monitor_enabled = False
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        resp = _post_context(client, session.id, used_pct=55)
        assert resp.status_code == 200
        assert resp.json()["status"] == "not_registered"
        assert not mock_session_manager.message_queue_manager.queue_message.called

    def test_registered_session_processes_normally(self, mock_session_manager, session):
        session.context_monitor_enabled = True
        session.context_monitor_notify = session.id
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        resp = _post_context(client, session.id, used_pct=55)
        assert resp.status_code == 200
        assert resp.json()["status"] == "ok"
        assert mock_session_manager.message_queue_manager.queue_message.called

    def test_compaction_bypasses_gate_when_not_registered(self, mock_session_manager, session):
        """Compaction event returns compaction_logged even when context_monitor_enabled=False (#210)."""
        session.context_monitor_enabled = False
        session.context_monitor_notify = None
        session.parent_session_id = None
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        resp = _post_event(client, session.id, event="compaction", trigger="auto")
        assert resp.status_code == 200
        assert resp.json()["status"] == "compaction_logged"

    def test_compaction_notifies_parent_when_monitor_disabled(self, mock_session_manager, session):
        """When context_monitor_enabled=False, compaction falls back to parent_session_id (#210)."""
        session.context_monitor_enabled = False
        session.context_monitor_notify = None
        session.parent_session_id = "parent-abc"
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        _post_event(client, session.id, event="compaction", trigger="auto")

        queue_mgr = mock_session_manager.message_queue_manager
        assert queue_mgr.queue_message.called
        call_kwargs = queue_mgr.queue_message.call_args[1]
        assert call_kwargs["target_session_id"] == "parent-abc"
        assert "Compaction fired" in call_kwargs["text"]

    def test_warning_still_gated_when_monitor_disabled(self, mock_session_manager, session):
        """Warning/critical usage events still gated behind context_monitor_enabled (#206, #210)."""
        session.context_monitor_enabled = False
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        resp = _post_context(client, session.id, used_pct=65)
        assert resp.status_code == 200
        assert resp.json()["status"] == "not_registered"
        assert not mock_session_manager.message_queue_manager.queue_message.called

    def test_critical_still_gated_when_monitor_disabled(self, mock_session_manager, session):
        """Critical threshold event still gated behind context_monitor_enabled (#206, #210)."""
        session.context_monitor_enabled = False
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        resp = _post_context(client, session.id, used_pct=80)
        assert resp.status_code == 200
        assert resp.json()["status"] == "not_registered"
        assert not mock_session_manager.message_queue_manager.queue_message.called


# ---------------------------------------------------------------------------
# 9. Notification routing (#206)
# ---------------------------------------------------------------------------


class TestNotificationRouting:
    """All notifications route through context_monitor_notify, not hardcoded targets."""

    def test_compaction_notifies_context_monitor_notify_not_parent(self, mock_session_manager, session):
        """Compaction routes to context_monitor_notify (Y), not parent_session_id (X)."""
        session.parent_session_id = "parent-X"
        session.context_monitor_notify = "notify-Y"
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        _post_event(client, session.id, event="compaction", trigger="auto")

        queue_mgr = mock_session_manager.message_queue_manager
        assert queue_mgr.queue_message.called
        call_kwargs = queue_mgr.queue_message.call_args[1]
        assert call_kwargs["target_session_id"] == "notify-Y"
        assert call_kwargs["target_session_id"] != "parent-X"

    def test_warning_routes_to_context_monitor_notify(self, mock_session_manager, session):
        """Warning routes to context_monitor_notify, not session.id."""
        session.context_monitor_notify = "parent-id"
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        _post_context(client, session.id, used_pct=55)

        queue_mgr = mock_session_manager.message_queue_manager
        assert queue_mgr.queue_message.called
        call_kwargs = queue_mgr.queue_message.call_args[1]
        assert call_kwargs["target_session_id"] == "parent-id"

    def test_critical_routes_to_context_monitor_notify(self, mock_session_manager, session):
        """Critical alert routes to context_monitor_notify, not session.id."""
        session.context_monitor_notify = "parent-id"
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        _post_context(client, session.id, used_pct=70)

        queue_mgr = mock_session_manager.message_queue_manager
        assert queue_mgr.queue_message.called
        call_kwargs = queue_mgr.queue_message.call_args[1]
        assert call_kwargs["target_session_id"] == "parent-id"


# ---------------------------------------------------------------------------
# 10. Registration endpoint (#206)
# ---------------------------------------------------------------------------


class TestRegistrationEndpoint:
    """Tests for POST /sessions/{id}/context-monitor."""

    def _reg_payload(self, session_id, enabled=True, notify_session_id=None, requester_session_id=None):
        return {
            "enabled": enabled,
            "notify_session_id": notify_session_id or session_id,
            "requester_session_id": requester_session_id or session_id,
        }

    def test_enable_sets_fields(self, client, mock_session_manager, session):
        payload = self._reg_payload(session.id)
        resp = client.post(f"/sessions/{session.id}/context-monitor", json=payload)
        assert resp.status_code == 200
        assert resp.json() == {"status": "ok", "enabled": True}
        assert session.context_monitor_enabled is True
        assert session.context_monitor_notify == session.id
        mock_session_manager._save_state.assert_called()

    def test_disable_clears_fields(self, client, mock_session_manager, session):
        session.context_monitor_enabled = True
        session.context_monitor_notify = session.id
        payload = {"enabled": False, "notify_session_id": None, "requester_session_id": session.id}
        resp = client.post(f"/sessions/{session.id}/context-monitor", json=payload)
        assert resp.status_code == 200
        assert session.context_monitor_enabled is False
        assert session.context_monitor_notify is None
        mock_session_manager._save_state.assert_called()

    def test_enable_without_notify_session_id_returns_422(self, client, session):
        payload = {"enabled": True, "requester_session_id": session.id}
        resp = client.post(f"/sessions/{session.id}/context-monitor", json=payload)
        assert resp.status_code == 422

    def test_unknown_session_returns_404(self, client, mock_session_manager):
        mock_session_manager.get_session.return_value = None
        payload = {"enabled": True, "notify_session_id": "abc", "requester_session_id": "abc"}
        resp = client.post("/sessions/ghost999/context-monitor", json=payload)
        assert resp.status_code == 404

    def test_auth_rejects_missing_requester(self, client, session):
        """Pydantic rejects request with missing required field requester_session_id."""
        payload = {"enabled": True, "notify_session_id": session.id}
        resp = client.post(f"/sessions/{session.id}/context-monitor", json=payload)
        assert resp.status_code == 422

    def test_auth_rejects_unrelated_requester(self, client, session):
        """Requester that is neither self nor parent gets 403."""
        session.parent_session_id = "other-parent"
        payload = {
            "enabled": True,
            "notify_session_id": session.id,
            "requester_session_id": "unrelated-id",
        }
        resp = client.post(f"/sessions/{session.id}/context-monitor", json=payload)
        assert resp.status_code == 403

    def test_auth_allows_self(self, client, mock_session_manager, session):
        """Self-registration succeeds."""
        mock_session_manager.get_session.return_value = session
        payload = {
            "enabled": True,
            "notify_session_id": session.id,
            "requester_session_id": session.id,
        }
        resp = client.post(f"/sessions/{session.id}/context-monitor", json=payload)
        assert resp.status_code == 200

    def test_auth_allows_parent(self, client, mock_session_manager, session):
        """Parent registration succeeds."""
        session.parent_session_id = "parent-abc"
        notify_session = _make_session("parent-abc")
        mock_session_manager.get_session.side_effect = lambda sid: (
            session if sid == session.id else notify_session
        )
        payload = {
            "enabled": True,
            "notify_session_id": "parent-abc",
            "requester_session_id": "parent-abc",
        }
        resp = client.post(f"/sessions/{session.id}/context-monitor", json=payload)
        assert resp.status_code == 200

    def test_invalid_notify_session_id_returns_422(self, client, mock_session_manager, session):
        """notify_session_id that doesn't exist returns 422."""
        mock_session_manager.get_session.side_effect = lambda sid: (
            session if sid == session.id else None
        )
        payload = {
            "enabled": True,
            "notify_session_id": "nonexistent-id",
            "requester_session_id": session.id,
        }
        resp = client.post(f"/sessions/{session.id}/context-monitor", json=payload)
        assert resp.status_code == 422

    def test_enable_rearms_flags(self, client, mock_session_manager, session):
        """Enabling resets both one-shot flags."""
        session._context_warning_sent = True
        session._context_critical_sent = True
        payload = self._reg_payload(session.id)
        resp = client.post(f"/sessions/{session.id}/context-monitor", json=payload)
        assert resp.status_code == 200
        assert session._context_warning_sent is False
        assert session._context_critical_sent is False

    def test_disable_reenable_reraises_warning(self, mock_session_manager, session):
        """After disable+enable, warning fires again (flags re-armed on enable)."""
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        # Step 1: trigger warning (flag set)
        _post_context(client, session.id, used_pct=55)
        assert session._context_warning_sent is True

        # Step 2: disable
        client.post(f"/sessions/{session.id}/context-monitor", json={
            "enabled": False, "notify_session_id": None, "requester_session_id": session.id,
        })
        assert session.context_monitor_enabled is False

        # Step 3: re-enable (flags re-armed)
        client.post(f"/sessions/{session.id}/context-monitor", json={
            "enabled": True, "notify_session_id": session.id, "requester_session_id": session.id,
        })
        assert session._context_warning_sent is False

        # Step 4: trigger warning again
        mock_session_manager.message_queue_manager.queue_message.reset_mock()
        _post_context(client, session.id, used_pct=55)
        assert mock_session_manager.message_queue_manager.queue_message.called


# ---------------------------------------------------------------------------
# 11. Status endpoint (#206)
# ---------------------------------------------------------------------------


class TestStatusEndpoint:
    """Tests for GET /sessions/context-monitor."""

    def test_status_lists_registered_sessions(self, mock_session_manager, session):
        other = _make_session("xyz99999")
        other.context_monitor_enabled = False
        mock_session_manager.sessions = {session.id: session, other.id: other}

        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        resp = client.get("/sessions/context-monitor")
        assert resp.status_code == 200
        monitored = resp.json()["monitored"]
        assert len(monitored) == 1
        assert monitored[0]["session_id"] == session.id

    def test_status_empty_when_none_registered(self, mock_session_manager, session):
        session.context_monitor_enabled = False
        mock_session_manager.sessions = {session.id: session}

        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        resp = client.get("/sessions/context-monitor")
        assert resp.status_code == 200
        assert resp.json()["monitored"] == []


# ---------------------------------------------------------------------------
# 12. Child-forwarded notifications (#212)
# ---------------------------------------------------------------------------


class TestChildForwardedNotifications:
    """Child-forwarded alerts use 'Child <name> (<id>)' format, omit handoff instructions."""

    def _make_child_session(self, session_id: str = "abc12345", friendly_name=None) -> object:
        """Create a session configured as a child being monitored by a parent."""
        s = _make_session(session_id)
        s.friendly_name = friendly_name
        s.context_monitor_notify = "parent-id"
        return s

    def test_child_critical_contains_friendly_name(self, mock_session_manager):
        """Child critical message includes the friendly_name when set."""
        session = self._make_child_session(friendly_name="engineer-abc")
        mock_session_manager.sessions = {session.id: session}
        mock_session_manager.get_session.return_value = session
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        _post_context(client, session.id, used_pct=70)

        call_kwargs = mock_session_manager.message_queue_manager.queue_message.call_args[1]
        assert "Child" in call_kwargs["text"]
        assert "engineer-abc" in call_kwargs["text"]

    def test_child_critical_falls_back_to_session_id(self, mock_session_manager):
        """Child critical message falls back to session.id when friendly_name is None."""
        session = self._make_child_session(friendly_name=None)
        mock_session_manager.sessions = {session.id: session}
        mock_session_manager.get_session.return_value = session
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        _post_context(client, session.id, used_pct=70)

        call_kwargs = mock_session_manager.message_queue_manager.queue_message.call_args[1]
        assert "Child" in call_kwargs["text"]
        assert session.id in call_kwargs["text"]

    def test_child_critical_omits_handoff_instruction(self, mock_session_manager):
        """Child critical message does not include the handoff instruction."""
        session = self._make_child_session(friendly_name=None)
        mock_session_manager.sessions = {session.id: session}
        mock_session_manager.get_session.return_value = session
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        _post_context(client, session.id, used_pct=70)

        call_kwargs = mock_session_manager.message_queue_manager.queue_message.call_args[1]
        assert "Write your handoff doc" not in call_kwargs["text"]

    def test_child_warning_contains_session_label(self, mock_session_manager):
        """Child warning message includes 'Child' and the session id."""
        session = self._make_child_session(friendly_name=None)
        mock_session_manager.sessions = {session.id: session}
        mock_session_manager.get_session.return_value = session
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        _post_context(client, session.id, used_pct=55)

        call_kwargs = mock_session_manager.message_queue_manager.queue_message.call_args[1]
        assert "Child" in call_kwargs["text"]
        assert session.id in call_kwargs["text"]

    def test_child_warning_omits_handoff_suggestion(self, mock_session_manager):
        """Child warning message does not include the handoff suggestion."""
        session = self._make_child_session(friendly_name=None)
        mock_session_manager.sessions = {session.id: session}
        mock_session_manager.get_session.return_value = session
        app = create_app(session_manager=mock_session_manager)
        client = TestClient(app)

        _post_context(client, session.id, used_pct=55)

        call_kwargs = mock_session_manager.message_queue_manager.queue_message.call_args[1]
        assert "Consider writing a handoff" not in call_kwargs["text"]

    def test_self_critical_includes_handoff_instruction(self, mock_session_manager, session):
        """Self critical alert (context_monitor_notify == session.id) includes handoff instruction."""
        # session fixture has context_monitor_notify = session.id (self-alert path)
        _post_context(TestClient(create_app(session_manager=mock_session_manager)), session.id, used_pct=70)

        call_kwargs = mock_session_manager.message_queue_manager.queue_message.call_args[1]
        assert "Write your handoff doc" in call_kwargs["text"]
        assert "Child" not in call_kwargs["text"]

    def test_self_warning_includes_handoff_suggestion(self, mock_session_manager, session):
        """Self warning alert (context_monitor_notify == session.id) includes handoff suggestion."""
        _post_context(TestClient(create_app(session_manager=mock_session_manager)), session.id, used_pct=55)

        call_kwargs = mock_session_manager.message_queue_manager.queue_message.call_args[1]
        assert "Consider writing a handoff" in call_kwargs["text"]
        assert "Child" not in call_kwargs["text"]


# ---------------------------------------------------------------------------
# 9. sm#249: compaction suppress remind
# ---------------------------------------------------------------------------


class TestCompactionSuppressRemind:
    """sm#249: _is_compacting flag set/cleared via compaction/compaction_complete events."""

    def test_compaction_sets_is_compacting_flag(self, client, session):
        """compaction event sets _is_compacting=True on session."""
        assert session._is_compacting is False
        _post_event(client, session.id, event="compaction", trigger="auto")
        assert session._is_compacting is True

    def test_compaction_complete_clears_is_compacting_flag(self, client, session):
        """compaction_complete event clears _is_compacting=False on session."""
        session._is_compacting = True
        _post_event(client, session.id, event="compaction_complete")
        assert session._is_compacting is False

    def test_compaction_complete_resets_remind_timer(self, client, mock_session_manager, session):
        """compaction_complete calls reset_remind on queue manager."""
        session._is_compacting = True
        _post_event(client, session.id, event="compaction_complete")
        queue_mgr = mock_session_manager.message_queue_manager
        queue_mgr.reset_remind.assert_called_once_with(session.id)

    def test_compaction_complete_returns_logged_status(self, client, session):
        """compaction_complete returns compaction_complete_logged status."""
        resp = _post_event(client, session.id, event="compaction_complete")
        assert resp.status_code == 200
        assert resp.json()["status"] == "compaction_complete_logged"

    def test_compaction_complete_no_queue_mgr_no_crash(self, session):
        """compaction_complete with no queue manager doesn't crash."""
        mock_sm = MagicMock()
        mock_sm.get_session.return_value = session
        mock_sm.message_queue_manager = None
        app = create_app(session_manager=mock_sm)
        client = TestClient(app)
        session._is_compacting = True
        resp = _post_event(client, session.id, event="compaction_complete")
        assert resp.status_code == 200
        assert session._is_compacting is False

    def test_session_is_compacting_defaults_false(self):
        """Session._is_compacting defaults to False (runtime-only, not persisted)."""
        s = Session(
            id="test1",
            name="claude-test1",
            tmux_session="claude-test1",
        )
        assert s._is_compacting is False

    def test_is_compacting_not_in_to_dict(self):
        """_is_compacting is a runtime flag and must not appear in to_dict()."""
        s = Session(
            id="test2",
            name="claude-test2",
            tmux_session="claude-test2",
        )
        s._is_compacting = True
        d = s.to_dict()
        assert "_is_compacting" not in d

    def test_from_dict_always_starts_with_is_compacting_false(self):
        """from_dict never restores _is_compacting (always starts False, safe default)."""
        s = Session(
            id="test3",
            name="claude-test3",
            tmux_session="claude-test3",
        )
        d = s.to_dict()
        restored = Session.from_dict(d)
        assert restored._is_compacting is False
