"""Tests for delivery feedback feature (#50).

Tests:
- DeliveryResult enum values
- send_input() returns correct DeliveryResult based on session state
- API endpoint returns delivery status
"""

import tempfile
from datetime import datetime
from pathlib import Path
from unittest.mock import MagicMock, AsyncMock, patch

import pytest
from fastapi.testclient import TestClient

from src.models import (
    Session,
    SessionStatus,
    DeliveryResult,
    UserInput,
    NotificationChannel,
)
from src.session_manager import SessionManager
from src.server import create_app


class TestDeliveryResult:
    """Test DeliveryResult enum."""

    def test_delivery_result_values(self):
        """Verify DeliveryResult has expected values."""
        assert DeliveryResult.DELIVERED.value == "delivered"
        assert DeliveryResult.QUEUED.value == "queued"
        assert DeliveryResult.FAILED.value == "failed"

    def test_delivery_result_comparison(self):
        """Verify DeliveryResult can be compared."""
        assert DeliveryResult.DELIVERED != DeliveryResult.QUEUED
        assert DeliveryResult.DELIVERED != DeliveryResult.FAILED
        assert DeliveryResult.QUEUED != DeliveryResult.FAILED


class TestUserInputDeliveryMode:
    """Test UserInput delivery_mode field."""

    def test_default_delivery_mode(self):
        """Verify default delivery mode is sequential."""
        user_input = UserInput(
            session_id="test123",
            text="hello",
            source=NotificationChannel.TELEGRAM,
        )
        assert user_input.delivery_mode == "sequential"

    def test_custom_delivery_mode(self):
        """Verify delivery mode can be set."""
        user_input = UserInput(
            session_id="test123",
            text="hello",
            source=NotificationChannel.TELEGRAM,
            delivery_mode="urgent",
        )
        assert user_input.delivery_mode == "urgent"


@pytest.fixture
def mock_tmux():
    """Create a mock TmuxController."""
    mock = MagicMock()
    mock.session_exists.return_value = True
    mock.send_input_async = AsyncMock(return_value=True)
    return mock


@pytest.fixture
def mock_message_queue():
    """Create a mock MessageQueueManager."""
    mock = MagicMock()
    mock.delivery_states = {}
    mock.queue_message = MagicMock()
    mock.is_session_idle = MagicMock(return_value=True)
    mock.get_queue_length = MagicMock(return_value=0)
    return mock


@pytest.fixture
def session_manager(mock_tmux):
    """Create a SessionManager with mocked dependencies."""
    with tempfile.TemporaryDirectory() as temp_dir:
        temp_state_file = Path(temp_dir) / "sessions.json"
        temp_state_file.write_text('{"sessions": []}')

        manager = SessionManager(
            log_dir=temp_dir,
            state_file=str(temp_state_file),
        )
        manager.tmux = mock_tmux
        yield manager


class TestSendInputDeliveryResult:
    """Test send_input() returns correct DeliveryResult."""

    @pytest.mark.asyncio
    async def test_returns_failed_for_nonexistent_session(self, session_manager):
        """Verify FAILED is returned for nonexistent session."""
        result = await session_manager.send_input(
            session_id="nonexistent",
            text="hello",
        )
        assert result == DeliveryResult.FAILED

    @pytest.mark.asyncio
    async def test_returns_delivered_on_bypass_queue_success(self, session_manager, mock_tmux):
        """Verify DELIVERED is returned when bypass_queue succeeds."""
        # Create a session
        session = Session(
            id="test123",
            name="test-session",
            working_dir="/tmp",
            tmux_session="claude-test123",
        )
        session_manager.sessions["test123"] = session

        result = await session_manager.send_input(
            session_id="test123",
            text="hello",
            bypass_queue=True,
        )
        assert result == DeliveryResult.DELIVERED

    @pytest.mark.asyncio
    async def test_returns_failed_on_bypass_queue_failure(self, session_manager, mock_tmux):
        """Verify FAILED is returned when bypass_queue fails."""
        mock_tmux.send_input_async = AsyncMock(return_value=False)

        session = Session(
            id="test123",
            name="test-session",
            working_dir="/tmp",
            tmux_session="claude-test123",
        )
        session_manager.sessions["test123"] = session

        result = await session_manager.send_input(
            session_id="test123",
            text="hello",
            bypass_queue=True,
        )
        assert result == DeliveryResult.FAILED

    @pytest.mark.asyncio
    async def test_returns_delivered_when_session_idle(self, session_manager, mock_message_queue):
        """Verify DELIVERED is returned when session is idle."""
        session = Session(
            id="test123",
            name="test-session",
            working_dir="/tmp",
            tmux_session="claude-test123",
        )
        session_manager.sessions["test123"] = session
        session_manager.message_queue_manager = mock_message_queue

        # Set up idle state
        from src.message_queue import SessionDeliveryState
        mock_message_queue.delivery_states["test123"] = MagicMock()
        mock_message_queue.delivery_states["test123"].is_idle = True

        result = await session_manager.send_input(
            session_id="test123",
            text="hello",
            delivery_mode="sequential",
        )
        assert result == DeliveryResult.DELIVERED

    @pytest.mark.asyncio
    async def test_returns_queued_when_session_busy(self, session_manager, mock_message_queue):
        """Verify QUEUED is returned when session is busy."""
        session = Session(
            id="test123",
            name="test-session",
            working_dir="/tmp",
            tmux_session="claude-test123",
        )
        session_manager.sessions["test123"] = session
        session_manager.message_queue_manager = mock_message_queue

        # Set up busy state
        mock_message_queue.delivery_states["test123"] = MagicMock()
        mock_message_queue.delivery_states["test123"].is_idle = False

        result = await session_manager.send_input(
            session_id="test123",
            text="hello",
            delivery_mode="sequential",
        )
        assert result == DeliveryResult.QUEUED

    @pytest.mark.asyncio
    async def test_urgent_returns_delivered(self, session_manager, mock_message_queue):
        """Verify urgent delivery mode always returns DELIVERED."""
        session = Session(
            id="test123",
            name="test-session",
            working_dir="/tmp",
            tmux_session="claude-test123",
        )
        session_manager.sessions["test123"] = session
        session_manager.message_queue_manager = mock_message_queue

        # Even when busy, urgent should return DELIVERED
        mock_message_queue.delivery_states["test123"] = MagicMock()
        mock_message_queue.delivery_states["test123"].is_idle = False

        result = await session_manager.send_input(
            session_id="test123",
            text="hello",
            delivery_mode="urgent",
        )
        assert result == DeliveryResult.DELIVERED


class TestAPIDeliveryResult:
    """Test API endpoint returns delivery status."""

    @pytest.fixture
    def test_client(self, session_manager, mock_message_queue):
        """Create a test client with session manager."""
        session_manager.message_queue_manager = mock_message_queue
        app = create_app(session_manager=session_manager)
        return TestClient(app)

    def test_api_returns_delivered_status(self, test_client, session_manager, mock_message_queue):
        """Verify API returns delivered status when session idle."""
        # Create a session
        session = Session(
            id="test123",
            name="test-session",
            working_dir="/tmp",
            tmux_session="claude-test123",
        )
        session_manager.sessions["test123"] = session

        # Set up idle state
        mock_message_queue.delivery_states["test123"] = MagicMock()
        mock_message_queue.delivery_states["test123"].is_idle = True

        response = test_client.post(
            "/sessions/test123/input",
            json={"text": "hello", "delivery_mode": "sequential"},
        )

        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "delivered"

    def test_api_returns_queued_status(self, test_client, session_manager, mock_message_queue):
        """Verify API returns queued status when session busy."""
        session = Session(
            id="test123",
            name="test-session",
            working_dir="/tmp",
            tmux_session="claude-test123",
        )
        session_manager.sessions["test123"] = session

        # Set up busy state
        mock_message_queue.delivery_states["test123"] = MagicMock()
        mock_message_queue.delivery_states["test123"].is_idle = False
        mock_message_queue.get_queue_length.return_value = 2

        response = test_client.post(
            "/sessions/test123/input",
            json={"text": "hello", "delivery_mode": "sequential"},
        )

        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "queued"
        assert data["queue_position"] == 2

    def test_api_returns_404_for_nonexistent_session(self, test_client):
        """Verify API returns 404 for nonexistent session."""
        response = test_client.post(
            "/sessions/nonexistent/input",
            json={"text": "hello"},
        )

        assert response.status_code == 404
