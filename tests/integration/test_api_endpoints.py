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
        assert data["service"] == "claude-session-manager"

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

        response = test_client.get("/sessions")
        assert response.status_code == 200

        data = response.json()
        assert "sessions" in data
        assert len(data["sessions"]) == 1
        assert data["sessions"][0]["id"] == "test123"
        assert data["sessions"][0]["friendly_name"] == "Test Session"

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

        response = test_client.get("/sessions/test123")
        assert response.status_code == 200

        data = response.json()
        assert data["id"] == "test123"
        assert data["name"] == "test-session"
        assert data["status"] == "running"
        assert data["friendly_name"] == "Test Session"

    def test_get_session_not_found(self, test_client, mock_session_manager):
        """GET /sessions/{id} returns 404 for unknown session."""
        mock_session_manager.get_session.return_value = None

        response = test_client.get("/sessions/unknown")
        assert response.status_code == 404

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

        # Verify mark_session_idle was called
        mock_queue_manager.mark_session_idle.assert_called_with("test123")

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
