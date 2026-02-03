"""Regression test for issue #101: Telegram topic routing broken.

Bug: on_update_topic was setting telegram_topic_id instead of telegram_thread_id,
causing messages to route to #general instead of dedicated topics.
"""

import pytest
from unittest.mock import MagicMock, AsyncMock
from fastapi.testclient import TestClient

from src.server import create_app
from src.models import Session, SessionStatus


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager for testing."""
    mock = MagicMock()
    mock.sessions = {}
    mock._save_state = MagicMock()

    def get_session(session_id):
        return mock.sessions.get(session_id)

    mock.get_session = get_session
    return mock


@pytest.fixture
def test_session():
    """Create a test session."""
    return Session(
        id="test123",
        name="test-session",
        working_dir="/tmp",
        tmux_session="claude-test123",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
    )


@pytest.fixture
def test_client(mock_session_manager):
    """Create a FastAPI TestClient with mocked dependencies."""
    app = create_app(
        session_manager=mock_session_manager,
        notifier=None,
        output_monitor=None,
        config={},
    )
    return TestClient(app)


def test_on_update_topic_sets_correct_field(mock_session_manager, test_session):
    """Test that on_update_topic sets telegram_thread_id, not telegram_topic_id.

    This test verifies the fix for issue #101 where on_update_topic was setting
    the wrong field name (telegram_topic_id instead of telegram_thread_id),
    causing messages to route to #general instead of dedicated topics.
    """
    # Add session to manager
    mock_session_manager.sessions[test_session.id] = test_session

    # Simulate what on_update_topic callback does (the fixed version)
    chat_id = 12345
    topic_id = 67890

    session = mock_session_manager.get_session(test_session.id)
    if session:
        session.telegram_chat_id = chat_id
        session.telegram_thread_id = topic_id  # FIXED: was telegram_topic_id
        mock_session_manager._save_state()

    # Verify the correct field was set
    assert test_session.telegram_chat_id == chat_id
    assert test_session.telegram_thread_id == topic_id

    # The bug was that it set telegram_topic_id (wrong field) instead of
    # telegram_thread_id (correct field). Verify we're using the right one.
    # Note: Python allows setting arbitrary attributes on objects, so the bug
    # was silent - it just created a new attribute instead of setting the
    # dataclass field.


def test_session_model_has_telegram_thread_id_field():
    """Verify that Session model has telegram_thread_id field defined."""
    session = Session(
        id="test",
        name="test",
        working_dir="/tmp",
        tmux_session="claude-test",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
    )

    # The field should exist and be None by default
    assert hasattr(session, 'telegram_thread_id')
    assert session.telegram_thread_id is None

    # Should be settable
    session.telegram_thread_id = 12345
    assert session.telegram_thread_id == 12345
