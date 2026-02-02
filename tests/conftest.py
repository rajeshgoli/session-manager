"""Shared pytest fixtures for Claude Session Manager tests."""

import json
import tempfile
from datetime import datetime
from pathlib import Path
from typing import Generator
from unittest.mock import MagicMock, AsyncMock

import pytest
from fastapi.testclient import TestClient

from src.models import Session, SessionStatus
from src.tmux_controller import TmuxController
from src.session_manager import SessionManager
from src.server import create_app


@pytest.fixture
def mock_tmux() -> MagicMock:
    """
    Mock TmuxController for testing without actual tmux sessions.

    Returns:
        MagicMock with common tmux methods configured
    """
    mock = MagicMock(spec=TmuxController)
    mock.session_exists.return_value = True
    mock.create_session.return_value = True
    mock.create_session_with_command.return_value = True
    mock.send_input.return_value = True
    mock.send_input_async = AsyncMock(return_value=True)
    mock.send_key.return_value = True
    mock.kill_session.return_value = True
    mock.list_sessions.return_value = []
    mock.capture_pane.return_value = "Mock tmux output"
    mock.set_status_bar.return_value = True
    mock.open_in_terminal.return_value = True
    return mock


@pytest.fixture
def temp_state_file() -> Generator[Path, None, None]:
    """
    Create a temporary state file for testing.

    Yields:
        Path to temporary state file
    """
    with tempfile.NamedTemporaryFile(mode='w', suffix='.json', delete=False) as f:
        temp_path = Path(f.name)
        # Initialize with empty sessions
        json.dump({"sessions": []}, f)

    yield temp_path

    # Cleanup
    if temp_path.exists():
        temp_path.unlink()


@pytest.fixture
def in_memory_db(temp_state_file: Path) -> Path:
    """
    Provide an in-memory database (temporary state file) for testing.

    This fixture provides a temporary JSON state file that can be used
    as an in-memory database for testing SessionManager.

    Args:
        temp_state_file: Temporary state file fixture

    Returns:
        Path to temporary state file
    """
    return temp_state_file


@pytest.fixture
def sample_session() -> Session:
    """
    Create a pre-configured Session object for testing.

    Returns:
        Session with realistic test data
    """
    return Session(
        id="test123",
        name="test-session",
        working_dir="/tmp/test-workspace",
        tmux_session="claude-test123",
        log_file="/tmp/claude-sessions/test123.log",
        status=SessionStatus.RUNNING,
        created_at=datetime(2024, 1, 1, 12, 0, 0),
        last_activity=datetime(2024, 1, 1, 12, 30, 0),
        telegram_chat_id=123456,
        telegram_thread_id=789,
        friendly_name="Test Session",
        current_task="Running tests",
        git_remote_url="https://github.com/test/repo.git",
    )


@pytest.fixture
def test_client(session_manager: SessionManager) -> TestClient:
    """
    Create a FastAPI TestClient for testing API endpoints.

    Args:
        session_manager: SessionManager fixture to inject into app

    Returns:
        TestClient configured with the app and session_manager
    """
    app = create_app(session_manager=session_manager)
    return TestClient(app)


@pytest.fixture
def session_manager(mock_tmux: MagicMock, temp_state_file: Path) -> Generator[SessionManager, None, None]:
    """
    Create a SessionManager instance with mocked dependencies.

    Args:
        mock_tmux: Mocked TmuxController
        temp_state_file: Temporary state file

    Yields:
        SessionManager configured for testing
    """
    with tempfile.TemporaryDirectory() as temp_dir:
        manager = SessionManager(
            log_dir=temp_dir,
            state_file=str(temp_state_file),
        )
        # Replace tmux controller with mock
        manager.tmux = mock_tmux
        yield manager
