"""
Regression tests for issue #283: agent_status_text not cleared on sm clear / sm dispatch.

Verifies that agent_status_text and agent_status_at are reset to None in each
of the three clear pathways:

  A) context_reset event handler (claude tmux /clear and TUI /clear)
  B) /sessions/{id}/clear endpoint (codex-app sessions)
  C) cmd_clear CLI success path (codex tmux sessions via /new)

Also verifies that set_agent_status with text=None acts as a clear (null-as-clear).
"""

import pytest
from datetime import datetime
from unittest.mock import AsyncMock, MagicMock, Mock, patch

from fastapi.testclient import TestClient

from src.models import CompletionStatus, Session, SessionStatus
from src.server import create_app


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------


def _make_session(session_id: str = "abc12345", provider: str = "claude") -> Session:
    s = Session(
        id=session_id,
        name=f"claude-{session_id}",
        working_dir="/tmp/test",
        tmux_session=f"claude-{session_id}",
        provider=provider,
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
    )
    s.agent_status_text = "doing task A"
    s.agent_status_at = datetime(2024, 1, 1, 12, 0, 0)
    return s


@pytest.fixture
def session():
    return _make_session()


@pytest.fixture
def mock_session_manager(session):
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


def _post_event(client, session_id: str, event: str):
    return client.post("/hooks/context-usage", json={"session_id": session_id, "event": event})


# ---------------------------------------------------------------------------
# A) context_reset event — clears agent status (#283 Location A)
# ---------------------------------------------------------------------------


class TestContextResetClearsAgentStatus:
    """context_reset event handler resets agent_status_text and agent_status_at."""

    def test_context_reset_clears_agent_status_text(self, client, session):
        assert session.agent_status_text == "doing task A"
        _post_event(client, session.id, event="context_reset")
        assert session.agent_status_text is None

    def test_context_reset_clears_agent_status_at(self, client, session):
        assert session.agent_status_at is not None
        _post_event(client, session.id, event="context_reset")
        assert session.agent_status_at is None

    def test_context_reset_saves_state(self, client, mock_session_manager, session):
        _post_event(client, session.id, event="context_reset")
        mock_session_manager._save_state.assert_called()

    def test_context_reset_still_returns_flags_reset(self, client, session):
        resp = _post_event(client, session.id, event="context_reset")
        assert resp.status_code == 200
        assert resp.json()["status"] == "flags_reset"

    def test_context_reset_still_resets_context_flags(self, client, session):
        session._context_warning_sent = True
        session._context_critical_sent = True
        _post_event(client, session.id, event="context_reset")
        assert session._context_warning_sent is False
        assert session._context_critical_sent is False


# ---------------------------------------------------------------------------
# B) /sessions/{id}/clear endpoint — clears agent status (#283 Location B)
# ---------------------------------------------------------------------------


class TestClearEndpointClearsAgentStatus:
    """POST /sessions/{id}/clear resets agent_status_text and agent_status_at."""

    @pytest.fixture
    def app_with_async_clear(self, session):
        """App fixture where session_manager.clear_session is an async coroutine."""
        mock_sm = MagicMock()
        mock_sm.sessions = {session.id: session}
        mock_sm.get_session = MagicMock(return_value=session)
        mock_sm._save_state = MagicMock()
        mock_sm.message_queue_manager = MagicMock()
        mock_sm.clear_session = AsyncMock(return_value=True)
        return create_app(session_manager=mock_sm), mock_sm

    def test_clear_endpoint_clears_agent_status_text(self, app_with_async_clear, session):
        app, _ = app_with_async_clear
        assert session.agent_status_text == "doing task A"
        c = TestClient(app)
        c.post(f"/sessions/{session.id}/clear", json={})
        assert session.agent_status_text is None

    def test_clear_endpoint_clears_agent_status_at(self, app_with_async_clear, session):
        app, _ = app_with_async_clear
        assert session.agent_status_at is not None
        c = TestClient(app)
        c.post(f"/sessions/{session.id}/clear", json={})
        assert session.agent_status_at is None

    def test_clear_endpoint_saves_state(self, app_with_async_clear, session):
        app, mock_sm = app_with_async_clear
        c = TestClient(app)
        c.post(f"/sessions/{session.id}/clear", json={})
        mock_sm._save_state.assert_called()

    def test_clear_endpoint_returns_cleared_status(self, app_with_async_clear, session):
        app, _ = app_with_async_clear
        c = TestClient(app)
        resp = c.post(f"/sessions/{session.id}/clear", json={})
        assert resp.status_code == 200
        assert resp.json()["status"] == "cleared"


# ---------------------------------------------------------------------------
# C) cmd_clear CLI — codex tmux calls clear_agent_status (#283 Location C)
# ---------------------------------------------------------------------------


class TestCmdClearCallsClearAgentStatusForCodex:
    """cmd_clear calls client.clear_agent_status after codex tmux /new succeeds."""

    @pytest.fixture
    def codex_session(self):
        return {
            "id": "codex-tmx-01",
            "name": "codex-session",
            "tmux_session": "claude-codex-tmx-01",
            "provider": "codex",
            "parent_session_id": "parent-001",
            "completion_status": None,
            "friendly_name": "codex-child",
        }

    @pytest.fixture
    def mock_client(self, codex_session):
        from src.cli.client import SessionManagerClient

        client = Mock(spec=SessionManagerClient)
        client.get_session = Mock(return_value=codex_session)
        client.list_sessions = Mock(return_value=[codex_session])
        client.invalidate_cache = Mock(return_value=(True, False))
        client.clear_agent_status = Mock(return_value=(True, False))
        return client

    def test_codex_tmux_calls_clear_agent_status(self, mock_client, codex_session):
        from src.cli.commands import cmd_clear

        with patch("subprocess.run") as mock_run, \
             patch("src.cli.commands._wait_for_claude_prompt", return_value=True):
            mock_run.return_value = Mock(returncode=0, stdout="", stderr="")

            result = cmd_clear(
                client=mock_client,
                requester_session_id="parent-001",
                target_identifier="codex-tmx-01",
                new_prompt=None,
            )

        assert result == 0
        mock_client.clear_agent_status.assert_called_once_with("codex-tmx-01")

    def test_codex_tmux_clear_agent_status_is_best_effort(self, mock_client, codex_session):
        """clear_agent_status failure does not abort cmd_clear (non-critical)."""
        from src.cli.commands import cmd_clear

        mock_client.clear_agent_status = Mock(return_value=(False, False))

        with patch("subprocess.run") as mock_run, \
             patch("src.cli.commands._wait_for_claude_prompt", return_value=True):
            mock_run.return_value = Mock(returncode=0, stdout="", stderr="")

            result = cmd_clear(
                client=mock_client,
                requester_session_id="parent-001",
                target_identifier="codex-tmx-01",
                new_prompt=None,
            )

        assert result == 0

    def test_claude_tmux_does_not_call_clear_agent_status(self, codex_session):
        """Claude tmux sessions rely on context_reset hook; no CLI call needed."""
        from src.cli.client import SessionManagerClient
        from src.cli.commands import cmd_clear

        claude_session = {**codex_session, "provider": "claude", "id": "claude-001",
                          "tmux_session": "claude-claude-001", "parent_session_id": "parent-001"}
        client = Mock(spec=SessionManagerClient)
        client.get_session = Mock(return_value=claude_session)
        client.list_sessions = Mock(return_value=[claude_session])
        client.invalidate_cache = Mock(return_value=(True, False))
        client.clear_agent_status = Mock(return_value=(True, False))

        with patch("subprocess.run") as mock_run, \
             patch("src.cli.commands._wait_for_claude_prompt", return_value=True):
            mock_run.return_value = Mock(returncode=0, stdout="", stderr="")
            cmd_clear(
                client=client,
                requester_session_id="parent-001",
                target_identifier="claude-001",
                new_prompt=None,
            )

        client.clear_agent_status.assert_not_called()


# ---------------------------------------------------------------------------
# D) _invalidate_session_cache canonical reset (#286)
# ---------------------------------------------------------------------------


class TestInvalidateCacheCanonicalReset:
    """POST /sessions/{id}/invalidate-cache clears status fields across providers."""

    @pytest.mark.parametrize("provider", ["claude", "codex", "codex-app"])
    def test_invalidate_cache_clears_completion_and_agent_status(self, provider):
        session = _make_session(provider=provider)
        session.completion_status = CompletionStatus.COMPLETED
        session.role = "engineer"
        mock_sm = MagicMock()
        mock_sm.sessions = {session.id: session}
        mock_sm.get_session = MagicMock(return_value=session)
        mock_sm._save_state = MagicMock()
        mock_sm.message_queue_manager = MagicMock()
        client = TestClient(create_app(session_manager=mock_sm))

        resp = client.post(f"/sessions/{session.id}/invalidate-cache")

        assert resp.status_code == 200
        assert session.role is None
        assert session.completion_status is None
        assert session.agent_status_text is None
        assert session.agent_status_at is None
        mock_sm._save_state.assert_called()


# ---------------------------------------------------------------------------
# E) set_agent_status with text=None — null-as-clear (#283)
# ---------------------------------------------------------------------------


class TestSetAgentStatusNullAsClear:
    """POST /sessions/{id}/agent-status with text=null clears the status fields."""

    def test_null_text_clears_agent_status_text(self, client, session):
        session.agent_status_text = "previous status"
        resp = client.post(f"/sessions/{session.id}/agent-status", json={"text": None})
        assert resp.status_code == 200
        assert session.agent_status_text is None

    def test_null_text_clears_agent_status_at(self, client, session):
        session.agent_status_at = datetime(2024, 1, 1)
        resp = client.post(f"/sessions/{session.id}/agent-status", json={"text": None})
        assert session.agent_status_at is None

    def test_null_text_does_not_reset_remind_timer(self, client, mock_session_manager, session):
        queue_mgr = mock_session_manager.message_queue_manager
        client.post(f"/sessions/{session.id}/agent-status", json={"text": None})
        queue_mgr.reset_remind.assert_not_called()

    def test_non_null_text_resets_remind_timer(self, client, mock_session_manager, session):
        queue_mgr = mock_session_manager.message_queue_manager
        client.post(f"/sessions/{session.id}/agent-status", json={"text": "doing work"})
        queue_mgr.reset_remind.assert_called_once_with(session.id)

    def test_null_text_response_contains_null_status(self, client, session):
        resp = client.post(f"/sessions/{session.id}/agent-status", json={"text": None})
        assert resp.json()["agent_status_text"] is None

    def test_client_has_clear_agent_status_method(self):
        from src.cli.client import SessionManagerClient
        assert hasattr(SessionManagerClient, "clear_agent_status")
