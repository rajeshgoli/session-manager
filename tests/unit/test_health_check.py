"""Tests for the detailed health check endpoint.

Verifies all health check components:
- State file integrity
- Session consistency (memory vs tmux)
- Message queue health
- Component status (telegram, monitors)
- Resource usage
"""

import json
import tempfile
from datetime import datetime
from pathlib import Path
from unittest.mock import MagicMock, AsyncMock, patch

import pytest
from fastapi.testclient import TestClient

from src.server import create_app
from src.models import Session, SessionStatus


@pytest.fixture
def mock_session_manager():
    """Create a mock session manager with basic structure."""
    mock = MagicMock()
    mock.sessions = {}
    mock.state_file = Path(tempfile.gettempdir()) / "test_sessions.json"
    mock.tmux = MagicMock()
    mock.tmux.list_sessions.return_value = []
    mock.message_queue_manager = None
    return mock


@pytest.fixture
def mock_output_monitor():
    """Create a mock output monitor."""
    mock = MagicMock()
    mock._tasks = {}
    return mock


@pytest.fixture
def mock_child_monitor():
    """Create a mock child monitor."""
    mock = MagicMock()
    mock._running = True
    return mock


@pytest.fixture
def mock_notifier():
    """Create a mock notifier."""
    mock = MagicMock()
    mock.telegram = None
    return mock


@pytest.fixture
def test_client(mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
    """Create a test client with all mocked components."""
    app = create_app(
        session_manager=mock_session_manager,
        notifier=mock_notifier,
        output_monitor=mock_output_monitor,
        child_monitor=mock_child_monitor,
    )
    return TestClient(app)


class TestHealthCheckBasic:
    """Test basic health check behavior."""

    def test_basic_health_endpoint(self, test_client):
        """Verify basic /health endpoint still works."""
        response = test_client.get("/health")
        assert response.status_code == 200
        assert response.json() == {"status": "healthy"}

    def test_detailed_health_endpoint_returns_200(self, test_client):
        """Verify /health/detailed returns 200."""
        response = test_client.get("/health/detailed")
        assert response.status_code == 200

    def test_detailed_health_has_required_fields(self, test_client):
        """Verify response has all required fields."""
        response = test_client.get("/health/detailed")
        data = response.json()

        assert "status" in data
        assert data["status"] in ("healthy", "degraded", "unhealthy")

        assert "checks" in data
        assert isinstance(data["checks"], dict)

        assert "resources" in data
        assert isinstance(data["resources"], dict)

        assert "timestamp" in data
        # Verify ISO format
        datetime.fromisoformat(data["timestamp"].replace("Z", "+00:00"))

    def test_detailed_health_includes_all_checks(self, test_client):
        """Verify all check categories are present."""
        response = test_client.get("/health/detailed")
        data = response.json()

        expected_checks = [
            "state_file",
            "tmux_sessions",
            "message_queue",
            "telegram",
            "monitors",
            "infrastructure",
        ]

        for check in expected_checks:
            assert check in data["checks"], f"Missing check: {check}"


class TestStateFileCheck:
    """Test state file integrity checks."""

    def test_state_file_not_exists(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test when state file doesn't exist (fresh start)."""
        mock_session_manager.state_file = Path("/nonexistent/path/sessions.json")

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        assert data["checks"]["state_file"]["status"] == "ok"
        assert "fresh start" in data["checks"]["state_file"]["message"].lower()

    def test_state_file_valid(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test with valid state file."""
        with tempfile.NamedTemporaryFile(mode='w', suffix='.json', delete=False) as f:
            json.dump({"sessions": [{"id": "test1"}, {"id": "test2"}]}, f)
            temp_path = Path(f.name)

        try:
            mock_session_manager.state_file = temp_path

            app = create_app(
                session_manager=mock_session_manager,
                notifier=mock_notifier,
                output_monitor=mock_output_monitor,
                child_monitor=mock_child_monitor,
            )
            client = TestClient(app)

            response = client.get("/health/detailed")
            data = response.json()

            assert data["checks"]["state_file"]["status"] == "ok"
            assert data["checks"]["state_file"]["details"]["sessions_in_file"] == 2
        finally:
            temp_path.unlink()

    def test_state_file_invalid_json(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test with corrupted state file."""
        with tempfile.NamedTemporaryFile(mode='w', suffix='.json', delete=False) as f:
            f.write("not valid json {{{")
            temp_path = Path(f.name)

        try:
            mock_session_manager.state_file = temp_path

            app = create_app(
                session_manager=mock_session_manager,
                notifier=mock_notifier,
                output_monitor=mock_output_monitor,
                child_monitor=mock_child_monitor,
            )
            client = TestClient(app)

            response = client.get("/health/detailed")
            data = response.json()

            assert data["checks"]["state_file"]["status"] == "error"
            assert data["status"] == "unhealthy"
        finally:
            temp_path.unlink()


class TestSessionConsistencyCheck:
    """Test session consistency checks."""

    def test_sessions_consistent(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test when sessions are consistent."""
        # Setup: session in memory matches tmux
        session = MagicMock(spec=Session)
        session.id = "test123"
        session.tmux_session = "claude-test123"
        session.status = SessionStatus.RUNNING

        mock_session_manager.sessions = {"test123": session}
        mock_session_manager.tmux.list_sessions.return_value = ["claude-test123"]

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        assert data["checks"]["tmux_sessions"]["status"] == "ok"

    def test_orphaned_tmux_session(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test when tmux session exists but not in memory."""
        mock_session_manager.sessions = {}
        mock_session_manager.tmux.list_sessions.return_value = ["claude-orphaned123"]

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        assert data["checks"]["tmux_sessions"]["status"] == "warning"
        assert len(data["checks"]["tmux_sessions"]["details"]["orphaned_tmux"]) == 1

    def test_session_missing_in_tmux(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test when session in memory but tmux doesn't exist."""
        session = MagicMock(spec=Session)
        session.id = "test123"
        session.tmux_session = "claude-test123"
        session.status = SessionStatus.RUNNING

        mock_session_manager.sessions = {"test123": session}
        mock_session_manager.tmux.list_sessions.return_value = []

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        assert data["checks"]["tmux_sessions"]["status"] == "error"
        assert data["status"] == "unhealthy"


class TestMessageQueueCheck:
    """Test message queue health checks."""

    def test_message_queue_not_configured(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test when message queue is not configured."""
        mock_session_manager.message_queue_manager = None

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        assert data["checks"]["message_queue"]["status"] == "warning"

    def test_message_queue_db_not_exists(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test when message queue DB doesn't exist."""
        mock_mq = MagicMock()
        mock_mq.db_path = Path("/nonexistent/path/queue.db")
        mock_session_manager.message_queue_manager = mock_mq

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        assert data["checks"]["message_queue"]["status"] == "warning"
        assert data["checks"]["message_queue"]["details"]["db_exists"] is False


class TestTelegramCheck:
    """Test Telegram bot status checks."""

    def test_telegram_not_configured(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test when Telegram is not configured."""
        mock_notifier.telegram = None

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        assert data["checks"]["telegram"]["status"] == "ok"
        assert data["checks"]["telegram"]["details"]["configured"] is False

    def test_telegram_configured_and_running(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test when Telegram bot is configured and running."""
        mock_telegram = MagicMock()
        mock_telegram.bot = MagicMock()
        mock_telegram.application = MagicMock()
        mock_telegram.application.running = True
        mock_telegram._session_threads = {"session1": (123, 456)}
        mock_telegram._topic_sessions = {(123, 456): "session1"}
        mock_notifier.telegram = mock_telegram

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        assert data["checks"]["telegram"]["status"] == "ok"
        assert data["checks"]["telegram"]["details"]["configured"] is True
        assert data["checks"]["telegram"]["details"]["tracked_sessions"] == 1


class TestMonitorsCheck:
    """Test output and child monitor checks."""

    def test_monitors_running(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test when monitors are running normally."""
        mock_output_monitor._tasks = {"session1": MagicMock()}
        mock_child_monitor._running = True

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        assert data["checks"]["monitors"]["status"] == "ok"

    def test_output_monitor_not_configured(self, mock_session_manager, mock_child_monitor, mock_notifier):
        """Test when output monitor is not configured."""
        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=None,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        assert data["checks"]["monitors"]["status"] == "warning"


class TestResourceUsage:
    """Test resource usage reporting."""

    def test_resource_counts(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test resource usage counts."""
        # Setup sessions with all required attributes
        session1 = MagicMock(spec=Session)
        session1.id = "s1"
        session1.status = SessionStatus.RUNNING
        session1.tmux_session = "claude-s1"

        session2 = MagicMock(spec=Session)
        session2.id = "s2"
        session2.status = SessionStatus.STOPPED
        session2.tmux_session = "claude-s2"

        mock_session_manager.sessions = {
            "s1": session1,
            "s2": session2,
        }
        # Tmux has the running session
        mock_session_manager.tmux.list_sessions.return_value = ["claude-s1"]

        mock_output_monitor._tasks = {"s1": MagicMock()}

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        resources = data["resources"]
        assert resources["active_sessions"] == 1  # Only running session
        assert resources["total_sessions"] == 2  # Both sessions
        assert resources["monitor_tasks"] == 1


class TestOverallStatus:
    """Test overall status determination."""

    def test_healthy_when_all_ok(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test overall status is healthy when all checks pass."""
        # Minimal valid setup
        mock_session_manager.state_file = Path("/nonexistent/state.json")  # Fresh start
        mock_session_manager.tmux.list_sessions.return_value = []
        mock_session_manager.message_queue_manager = None

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        # Should be healthy or degraded (message queue not configured is a warning)
        assert data["status"] in ("healthy", "degraded")

    def test_unhealthy_on_error(self, mock_session_manager, mock_output_monitor, mock_child_monitor, mock_notifier):
        """Test overall status is unhealthy when there's an error."""
        # Setup: session missing from tmux (error condition)
        session = MagicMock(spec=Session)
        session.id = "test123"
        session.tmux_session = "claude-test123"
        session.status = SessionStatus.RUNNING

        mock_session_manager.sessions = {"test123": session}
        mock_session_manager.tmux.list_sessions.return_value = []  # Missing!

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            child_monitor=mock_child_monitor,
        )
        client = TestClient(app)

        response = client.get("/health/detailed")
        data = response.json()

        assert data["status"] == "unhealthy"
