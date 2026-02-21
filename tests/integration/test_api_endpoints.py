"""Integration tests for API endpoints - ticket #65."""

import pytest
import json
from datetime import datetime
from unittest.mock import MagicMock, AsyncMock, patch
from fastapi.testclient import TestClient

from src.server import create_app
from src.models import Session, SessionStatus, Subagent, SubagentStatus, DeliveryResult


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager for testing."""
    mock = MagicMock()
    mock.sessions = {}
    mock.tmux = MagicMock()
    mock.tmux.send_input_async = AsyncMock(return_value=True)
    mock.tmux.list_sessions = MagicMock(return_value=[])
    mock.message_queue_manager = None
    mock._save_state = MagicMock()
    return mock


@pytest.fixture
def mock_output_monitor():
    """Create a mock OutputMonitor."""
    mock = MagicMock()
    mock.start_monitoring = AsyncMock()
    mock.cleanup_session = AsyncMock()
    mock.update_activity = MagicMock()
    mock._tasks = {}
    return mock


@pytest.fixture
def test_client(mock_session_manager, mock_output_monitor):
    """Create a FastAPI TestClient with mocked dependencies."""
    app = create_app(
        session_manager=mock_session_manager,
        notifier=None,
        output_monitor=mock_output_monitor,
        config={},
    )
    return TestClient(app)


@pytest.fixture
def sample_session():
    """Create a sample session for testing."""
    return Session(
        id="test123",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="claude-test123",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
        created_at=datetime(2024, 1, 15, 10, 0, 0),
        last_activity=datetime(2024, 1, 15, 11, 0, 0),
        friendly_name="Test Session",
        current_task="Testing",
    )


class TestHealthEndpoints:
    """Tests for health check endpoints."""

    def test_root_endpoint(self, test_client):
        """GET / returns health status."""
        response = test_client.get("/")
        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "ok"
        assert data["service"] == "session-manager"

    def test_health_endpoint(self, test_client):
        """GET /health returns healthy status."""
        response = test_client.get("/health")
        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "healthy"


class TestSessionEndpoints:
    """Tests for session CRUD endpoints."""

    def test_list_sessions(self, test_client, mock_session_manager, sample_session):
        """GET /sessions returns session list."""
        mock_session_manager.list_sessions.return_value = [sample_session]
        mock_session_manager.get_activity_state.return_value = "working"

        response = test_client.get("/sessions")
        assert response.status_code == 200

        data = response.json()
        assert "sessions" in data
        assert len(data["sessions"]) == 1
        assert data["sessions"][0]["id"] == "test123"
        assert data["sessions"][0]["friendly_name"] == "Test Session"
        assert data["sessions"][0]["activity_state"] == "working"

    def test_list_sessions_empty(self, test_client, mock_session_manager):
        """GET /sessions returns empty list when no sessions."""
        mock_session_manager.list_sessions.return_value = []

        response = test_client.get("/sessions")
        assert response.status_code == 200

        data = response.json()
        assert data["sessions"] == []

    def test_get_session(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id} returns session details."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.get_activity_state.return_value = "thinking"

        response = test_client.get("/sessions/test123")
        assert response.status_code == 200

        data = response.json()
        assert data["id"] == "test123"
        assert data["name"] == "test-session"
        assert data["status"] == "running"
        assert data["friendly_name"] == "Test Session"
        assert data["activity_state"] == "thinking"

    def test_get_session_not_found(self, test_client, mock_session_manager):
        """GET /sessions/{id} returns 404 for unknown session."""
        mock_session_manager.get_session.return_value = None

        response = test_client.get("/sessions/unknown")
        assert response.status_code == 404

    def test_get_codex_events_success(self, test_client, mock_session_manager):
        """GET /sessions/{id}/codex-events returns codex event page."""
        codex_session = Session(
            id="codex123",
            name="codex-app-codex123",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.get_codex_events.return_value = {
            "events": [
                {
                    "session_id": "codex123",
                    "seq": 1,
                    "timestamp": "2026-01-01T00:00:00+00:00",
                    "event_type": "turn_started",
                    "turn_id": "turn-1",
                    "payload_preview": {},
                    "persisted": True,
                }
            ],
            "earliest_seq": 1,
            "latest_seq": 1,
            "next_seq": 2,
            "history_gap": False,
            "gap_reason": None,
        }

        response = test_client.get("/sessions/codex123/codex-events?since_seq=0&limit=50")
        assert response.status_code == 200
        data = response.json()
        assert data["latest_seq"] == 1
        assert data["events"][0]["event_type"] == "turn_started"

    def test_get_codex_events_rejects_non_codex_app(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id}/codex-events rejects non-codex-app sessions."""
        mock_session_manager.get_session.return_value = sample_session
        response = test_client.get("/sessions/test123/codex-events")
        assert response.status_code == 400

    def test_get_codex_activity_actions_success(self, test_client, mock_session_manager):
        """GET /sessions/{id}/activity-actions returns projected actions for codex-app."""
        codex_session = Session(
            id="codexproj",
            name="codex-app-codexproj",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.get_codex_activity_actions.return_value = [
            {
                "source_provider": "codex-app",
                "action_kind": "command",
                "summary_text": "Started: pytest -q",
                "status": "running",
                "started_at": "2026-02-21T00:00:00+00:00",
                "ended_at": None,
                "session_id": "codexproj",
                "turn_id": "turn-1",
                "item_id": "item-1",
            }
        ]

        response = test_client.get("/sessions/codexproj/activity-actions?limit=5")
        assert response.status_code == 200
        data = response.json()
        assert len(data["actions"]) == 1
        assert data["actions"][0]["action_kind"] == "command"

    def test_list_children_includes_codex_activity_projection(self, test_client, mock_session_manager):
        """GET /sessions/{parent}/children includes activity projection for codex-app children."""
        child = Session(
            id="childcodex1",
            name="codex-app-childcodex1",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
            parent_session_id="parent123",
        )
        mock_session_manager.list_sessions.return_value = [child]
        mock_session_manager.get_codex_latest_activity_action.return_value = {
            "summary_text": "Started: pytest -q",
            "status": "running",
            "started_at": "2026-02-21T00:00:00+00:00",
            "ended_at": None,
        }

        response = test_client.get("/sessions/parent123/children")
        assert response.status_code == 200
        data = response.json()
        assert len(data["children"]) == 1
        assert data["children"][0]["activity_projection"]["summary_text"] == "Started: pytest -q"

    def test_list_codex_pending_requests(self, test_client, mock_session_manager):
        """GET /sessions/{id}/codex-pending-requests lists pending structured requests."""
        codex_session = Session(
            id="codexpending",
            name="codex-app-codexpending",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.list_codex_pending_requests.return_value = [
            {
                "request_id": "req-123",
                "request_type": "request_approval",
                "status": "pending",
                "requested_at": "2026-02-21T00:00:00+00:00",
            }
        ]

        response = test_client.get("/sessions/codexpending/codex-pending-requests")
        assert response.status_code == 200
        data = response.json()
        assert len(data["requests"]) == 1
        assert data["requests"][0]["request_id"] == "req-123"

    def test_respond_codex_request(self, test_client, mock_session_manager):
        """POST /sessions/{id}/codex-requests/{request_id}/respond resolves request."""
        codex_session = Session(
            id="codexresp",
            name="codex-app-codexresp",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.respond_codex_request = AsyncMock(
            return_value={
                "ok": True,
                "idempotent": False,
                "request": {
                    "request_id": "req-55",
                    "resolved_payload": {"decision": "accept"},
                    "status": "resolved",
                },
            }
        )

        response = test_client.post(
            "/sessions/codexresp/codex-requests/req-55/respond",
            json={"decision": "accept"},
        )
        assert response.status_code == 200
        assert response.json()["request"]["status"] == "resolved"

    def test_respond_codex_request_rejects_ambiguous_payload(self, test_client, mock_session_manager):
        """POST /sessions/{id}/codex-requests/{request_id}/respond requires one payload shape."""
        codex_session = Session(
            id="codexresp2",
            name="codex-app-codexresp2",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session

        response = test_client.post(
            "/sessions/codexresp2/codex-requests/req-99/respond",
            json={"decision": "accept", "answers": {"k": "v"}},
        )
        assert response.status_code == 422

    def test_create_session(self, test_client, mock_session_manager, sample_session):
        """POST /sessions creates new session."""
        mock_session_manager.create_session = AsyncMock(return_value=sample_session)

        response = test_client.post(
            "/sessions",
            json={"working_dir": "/tmp/test"}
        )
        assert response.status_code == 200

        data = response.json()
        assert data["id"] == "test123"
        assert data["working_dir"] == "/tmp/test"

    def test_create_session_failure(self, test_client, mock_session_manager):
        """POST /sessions returns 500 on creation failure."""
        mock_session_manager.create_session = AsyncMock(return_value=None)

        response = test_client.post(
            "/sessions",
            json={"working_dir": "/tmp/test"}
        )
        assert response.status_code == 500

    def test_kill_session(self, test_client, mock_session_manager, sample_session, mock_output_monitor):
        """DELETE /sessions/{id} kills session."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.kill_session.return_value = True

        response = test_client.delete("/sessions/test123")
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "killed"
        assert data["session_id"] == "test123"

    def test_kill_session_not_found(self, test_client, mock_session_manager):
        """DELETE /sessions/{id} returns 404 for unknown session."""
        mock_session_manager.get_session.return_value = None

        response = test_client.delete("/sessions/unknown")
        assert response.status_code == 404

    def test_send_input(self, test_client, mock_session_manager, sample_session):
        """POST /sessions/{id}/input sends input."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.send_input = AsyncMock(return_value=DeliveryResult.DELIVERED)

        response = test_client.post(
            "/sessions/test123/input",
            json={"text": "Hello, Claude!"}
        )
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "delivered"
        assert data["session_id"] == "test123"

    def test_send_input_codex_app_with_pending_structured_request_returns_409(self, test_client, mock_session_manager):
        """POST /sessions/{id}/input is blocked for codex-app when structured requests are pending."""
        codex_session = Session(
            id="codexpend",
            name="codex-app-codexpend",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.has_pending_codex_requests.return_value = True
        mock_session_manager.oldest_pending_codex_request.return_value = {
            "request_id": "req-1",
            "request_type": "request_approval",
            "requested_at": "2026-02-21T00:00:00+00:00",
        }

        response = test_client.post("/sessions/codexpend/input", json={"text": "continue"})
        assert response.status_code == 409
        detail = response.json()["detail"]
        assert detail["error_code"] == "pending_structured_request"
        assert detail["pending_request"]["request_id"] == "req-1"

    def test_send_input_queued(self, test_client, mock_session_manager, sample_session):
        """POST /sessions/{id}/input returns queued status."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.send_input = AsyncMock(return_value=DeliveryResult.QUEUED)
        mock_session_manager.message_queue_manager = MagicMock()
        mock_session_manager.message_queue_manager.get_queue_length.return_value = 3

        response = test_client.post(
            "/sessions/test123/input",
            json={"text": "Hello", "delivery_mode": "sequential"}
        )
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "queued"
        assert data["queue_position"] == 3

    def test_send_input_not_found(self, test_client, mock_session_manager):
        """POST /sessions/{id}/input returns 404 for unknown session."""
        mock_session_manager.get_session.return_value = None

        response = test_client.post(
            "/sessions/unknown/input",
            json={"text": "Hello"}
        )
        assert response.status_code == 404


class TestHookEndpoints:
    """Tests for Claude Code hook endpoints."""

    def test_claude_stop_hook(self, test_client, mock_session_manager, sample_session):
        """POST /hooks/claude with Stop event marks idle."""
        mock_session_manager.get_session.return_value = sample_session
        mock_queue_manager = MagicMock()
        # Make _restore_user_input_after_response an actual async function
        mock_queue_manager._restore_user_input_after_response = AsyncMock()
        mock_session_manager.message_queue_manager = mock_queue_manager

        response = test_client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "test123",
                "transcript_path": "/tmp/transcript.jsonl",
            }
        )
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "received"
        assert data["hook_event"] == "Stop"

        # Verify mark_session_idle was called (last_output=None when transcript not readable)
        mock_queue_manager.mark_session_idle.assert_called_with("test123", last_output=None, from_stop_hook=True)

    def test_claude_notification_hook(self, test_client):
        """POST /hooks/claude with Notification routes correctly."""
        response = test_client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Notification",
                "notification_type": "permission_prompt",
                "message": "Approve this action?",
            }
        )
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "received"
        assert data["hook_event"] == "Notification"

    def test_claude_idle_notification_filtered(self, test_client):
        """POST /hooks/claude filters idle_prompt notifications."""
        response = test_client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Notification",
                "notification_type": "idle_prompt",
                "message": "Claude is idle",
            }
        )
        assert response.status_code == 200
        # Should succeed but be filtered (no notification sent)

    def test_tool_use_hook_logs(self, test_client, mock_session_manager, sample_session):
        """POST /hooks/tool-use logs to database."""
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.post(
            "/hooks/tool-use",
            json={
                "hook_event_name": "PreToolUse",
                "tool_name": "Read",
                "tool_input": {"file_path": "/tmp/test.py"},
                "session_manager_id": "test123",
            }
        )
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "logged"


class TestSubagentEndpoints:
    """Tests for subagent management endpoints."""

    def test_spawn_subagent(self, test_client, mock_session_manager, sample_session, mock_output_monitor):
        """POST /sessions/{id}/subagents spawns child."""
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.post(
            "/sessions/test123/subagents",
            json={
                "agent_id": "agent456",
                "agent_type": "engineer",
            }
        )
        assert response.status_code == 200

        data = response.json()
        assert data["agent_id"] == "agent456"
        assert data["agent_type"] == "engineer"
        assert data["parent_session_id"] == "test123"
        assert data["status"] == "running"

    def test_list_subagents(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id}/subagents lists children."""
        subagent = Subagent(
            agent_id="agent456",
            agent_type="engineer",
            parent_session_id="test123",
            started_at=datetime(2024, 1, 15, 10, 0, 0),
            status=SubagentStatus.RUNNING,
        )
        sample_session.subagents = [subagent]
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.get("/sessions/test123/subagents")
        assert response.status_code == 200

        data = response.json()
        assert data["session_id"] == "test123"
        assert len(data["subagents"]) == 1
        assert data["subagents"][0]["agent_id"] == "agent456"

    def test_subagent_not_found(self, test_client, mock_session_manager):
        """GET /sessions/{id}/subagents returns 404 for unknown session."""
        mock_session_manager.get_session.return_value = None

        response = test_client.get("/sessions/unknown/subagents")
        assert response.status_code == 404


class TestSpawnChildSession:
    """Tests for child session spawning."""

    def test_spawn_child_session(self, test_client, mock_session_manager, sample_session, mock_output_monitor):
        """POST /sessions/spawn creates child session."""
        child_session = Session(
            id="child456",
            name="child-test12",
            working_dir="/tmp/test",
            tmux_session="claude-child456",
            log_file="/tmp/child.log",
            status=SessionStatus.RUNNING,
            parent_session_id="test123",
            spawned_at=datetime.now(),
        )

        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.spawn_child_session = AsyncMock(return_value=child_session)

        response = test_client.post(
            "/sessions/spawn",
            json={
                "parent_session_id": "test123",
                "prompt": "Test task",
            }
        )
        assert response.status_code == 200

        data = response.json()
        assert data["session_id"] == "child456"
        assert data["parent_session_id"] == "test123"


class TestUpdateSession:
    """Tests for session update endpoints."""

    def test_update_friendly_name(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} updates friendly name."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.tmux.set_status_bar.return_value = True

        response = test_client.patch(
            "/sessions/test123",
            json={"friendly_name": "new-name"}
        )
        assert response.status_code == 200

        data = response.json()
        assert data["friendly_name"] == "new-name"

    def test_update_friendly_name_rejects_empty(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} rejects empty friendly name (Issue #105)."""
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.patch(
            "/sessions/test123",
            json={"friendly_name": ""}
        )
        assert response.status_code == 400
        assert "empty" in response.json()["detail"].lower()

    def test_update_friendly_name_rejects_spaces(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} rejects names with spaces (Issue #105)."""
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.patch(
            "/sessions/test123",
            json={"friendly_name": "bad name"}
        )
        assert response.status_code == 400
        assert "alphanumeric" in response.json()["detail"].lower()

    def test_update_friendly_name_rejects_too_long(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} rejects names over 32 chars (Issue #105)."""
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.patch(
            "/sessions/test123",
            json={"friendly_name": "a" * 33}
        )
        assert response.status_code == 400
        assert "too long" in response.json()["detail"].lower()

    def test_update_friendly_name_logs_telegram_failure(self, mock_session_manager, mock_output_monitor, caplog):
        """PATCH /sessions/{id} logs warning when Telegram rename fails (Issue #106)."""
        import logging
        from src.server import create_app
        from fastapi.testclient import TestClient

        # Create session with Telegram thread ID
        session = Session(
            id="test123",
            name="test-session",
            working_dir="/tmp/test",
            tmux_session="claude-test123",
            log_file="/tmp/test.log",
            status=SessionStatus.RUNNING,
            created_at=datetime(2024, 1, 15, 10, 0, 0),
            last_activity=datetime(2024, 1, 15, 11, 0, 0),
            telegram_thread_id=42,  # Has Telegram thread
        )

        mock_session_manager.get_session.return_value = session
        mock_session_manager.tmux.set_status_bar.return_value = True

        # Mock notifier that fails to rename
        mock_notifier = MagicMock()
        mock_notifier.rename_session_topic = AsyncMock(return_value=False)

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            config={},
        )
        client = TestClient(app)

        with caplog.at_level(logging.WARNING):
            response = client.patch(
                "/sessions/test123",
                json={"friendly_name": "new-name"}
            )

        assert response.status_code == 200

        # Verify warning was logged
        assert any("Failed to rename Telegram topic" in record.message for record in caplog.records)
        assert any("test123" in record.message for record in caplog.records)

    def test_patch_is_em_sets_flag(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} with is_em=true sets is_em flag and returns it in response (#256)."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager._save_state = MagicMock()

        response = test_client.patch(
            "/sessions/test123",
            json={"is_em": True}
        )
        assert response.status_code == 200
        data = response.json()
        assert data["is_em"] is True
        assert sample_session.is_em is True
        mock_session_manager._save_state.assert_called()

    def test_patch_is_em_false_clears_flag(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} with is_em=false clears flag if previously set (#256)."""
        sample_session.is_em = True
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager._save_state = MagicMock()

        response = test_client.patch(
            "/sessions/test123",
            json={"is_em": False}
        )
        assert response.status_code == 200
        data = response.json()
        assert data["is_em"] is False
        assert sample_session.is_em is False

    def test_patch_mixed_friendly_name_and_is_em(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} with both friendly_name and is_em updates both fields (#256)."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager._save_state = MagicMock()
        mock_session_manager.tmux.set_status_bar.return_value = True

        response = test_client.patch(
            "/sessions/test123",
            json={"friendly_name": "em-session9", "is_em": True}
        )
        assert response.status_code == 200
        data = response.json()
        assert data["friendly_name"] == "em-session9"
        assert data["is_em"] is True

    def test_get_session_includes_is_em(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id} response includes is_em field (#256)."""
        sample_session.is_em = True
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.get("/sessions/test123")
        assert response.status_code == 200
        data = response.json()
        assert "is_em" in data
        assert data["is_em"] is True

    def test_update_task(self, test_client, mock_session_manager, sample_session):
        """PUT /sessions/{id}/task updates current task."""
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.put(
            "/sessions/test123/task",
            json={"task": "Working on new feature"}
        )
        assert response.status_code == 200

        data = response.json()
        assert data["task"] == "Working on new feature"


class TestOutputEndpoints:
    """Tests for output capture endpoints."""

    def test_capture_output(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id}/output captures tmux output."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.capture_output.return_value = "Claude output here"

        response = test_client.get("/sessions/test123/output")
        assert response.status_code == 200

        data = response.json()
        assert data["session_id"] == "test123"
        assert data["output"] == "Claude output here"

    def test_capture_output_with_lines(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id}/output?lines=100 passes lines parameter."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.capture_output.return_value = "Output"

        response = test_client.get("/sessions/test123/output?lines=100")
        assert response.status_code == 200

        mock_session_manager.capture_output.assert_called_with("test123", 100)


class TestQueueEndpoints:
    """Tests for message queue endpoints."""

    def test_get_send_queue(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id}/send-queue returns queue status."""
        mock_session_manager.get_session.return_value = sample_session
        mock_queue = MagicMock()
        mock_queue.get_queue_status.return_value = {
            "session_id": "test123",
            "is_idle": True,
            "pending_count": 2,
            "pending_messages": [],
            "saved_user_input": None,
        }
        mock_session_manager.message_queue_manager = mock_queue

        response = test_client.get("/sessions/test123/send-queue")
        assert response.status_code == 200

        data = response.json()
        assert data["is_idle"] is True
        assert data["pending_count"] == 2


class TestSessionManagerUnavailable:
    """Tests for when session manager is unavailable."""

    def test_list_sessions_unavailable(self):
        """GET /sessions returns 503 when session manager not configured."""
        app = create_app(session_manager=None)
        client = TestClient(app)

        response = client.get("/sessions")
        assert response.status_code == 503

    def test_create_session_unavailable(self):
        """POST /sessions returns 503 when session manager not configured."""
        app = create_app(session_manager=None)
        client = TestClient(app)

        response = client.post("/sessions", json={"working_dir": "/tmp"})
        assert response.status_code == 503
