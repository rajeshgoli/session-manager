"""Sanity tests to verify pytest fixtures are working correctly."""

from pathlib import Path
from unittest.mock import MagicMock

import pytest
from fastapi.testclient import TestClient

from src.models import Session, SessionStatus
from src.session_manager import SessionManager


def test_mock_tmux(mock_tmux):
    """Verify mock_tmux fixture is configured correctly."""
    assert isinstance(mock_tmux, MagicMock)
    assert mock_tmux.session_exists() is True
    assert mock_tmux.create_session() is True


def test_temp_state_file(temp_state_file):
    """Verify temp_state_file fixture creates a valid file."""
    assert isinstance(temp_state_file, Path)
    assert temp_state_file.exists()
    assert temp_state_file.suffix == '.json'


def test_in_memory_db(in_memory_db):
    """Verify in_memory_db fixture is available."""
    assert isinstance(in_memory_db, Path)
    assert in_memory_db.exists()


def test_sample_session(sample_session):
    """Verify sample_session fixture creates a valid Session."""
    assert isinstance(sample_session, Session)
    assert sample_session.id == "test123"
    assert sample_session.name == "test-session"
    assert sample_session.status == SessionStatus.RUNNING


def test_test_client(test_client):
    """Verify test_client fixture creates a working TestClient."""
    assert isinstance(test_client, TestClient)
    # Test a basic health check endpoint if it exists
    response = test_client.get("/health")
    # Don't assert success since we don't know if /health exists yet


def test_session_manager(session_manager):
    """Verify session_manager fixture is configured correctly."""
    assert isinstance(session_manager, SessionManager)
    assert isinstance(session_manager.tmux, MagicMock)
    assert session_manager.sessions == {}


@pytest.mark.asyncio
async def test_async_works():
    """Verify pytest-asyncio is working."""
    await asyncio_sleep_mock()


async def asyncio_sleep_mock():
    """Mock async function for testing."""
    import asyncio
    await asyncio.sleep(0.001)
    return True
