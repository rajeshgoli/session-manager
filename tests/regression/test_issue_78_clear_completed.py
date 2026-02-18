"""
Regression tests for issue #78: sm clear fails with 'not in a mode' error

Tests verify that sm clear works on sessions in different states,
particularly the "completed" state.
"""

import pytest
import subprocess
import time
from pathlib import Path
from unittest.mock import Mock, patch, MagicMock
from datetime import datetime

from src.cli.commands import cmd_clear, resolve_session_id
from src.cli.client import SessionManagerClient


@pytest.fixture
def mock_client():
    """Create a mock SessionManagerClient."""
    client = Mock(spec=SessionManagerClient)
    client.invalidate_cache = Mock(return_value=(True, False))
    return client


@pytest.fixture
def mock_subprocess_run():
    """Mock subprocess.run, time.sleep, and _wait_for_claude_prompt to avoid tmux calls."""
    with patch('subprocess.run') as mock_run, \
         patch('src.cli.commands._wait_for_claude_prompt', return_value=True), \
         patch('time.sleep'):  # Suppress settle-delay sleeps in tests (#178)
        # Default: successful tmux commands
        mock_run.return_value = Mock(
            returncode=0,
            stdout="",
            stderr="",
        )
        yield mock_run


def test_clear_completed_session_wakes_up_first(mock_client, mock_subprocess_run):
    """Test that clearing a completed session sends Enter first to wake it up."""
    # Mock session with completion_status="completed"
    session = {
        "id": "test-123",
        "name": "test-session",
        "tmux_session": "claude-test-123",
        "parent_session_id": "parent-456",
        "completion_status": "completed",  # This is the key - session is completed
        "friendly_name": "test-child",
    }

    mock_client.get_session.return_value = session
    mock_client.list_sessions.return_value = [session]

    # Call cmd_clear with parent as requester
    result = cmd_clear(
        client=mock_client,
        requester_session_id="parent-456",  # Parent can clear child
        target_identifier="test-123",
        new_prompt=None,
    )

    # Should succeed
    assert result == 0

    # Verify tmux commands were sent in correct order
    calls = mock_subprocess_run.call_args_list

    # First call should be Enter to wake up the completed session
    assert calls[0][0][0] == ["tmux", "send-keys", "-t", "claude-test-123", "Enter"]

    # Second call should be Escape (to interrupt)
    assert calls[1][0][0] == ["tmux", "send-keys", "-t", "claude-test-123", "Escape"]

    # Third call should be /clear text (no \r — two-call approach, #178)
    assert calls[2][0][0] == ["tmux", "send-keys", "-t", "claude-test-123", "--", "/clear"]

    # Fourth call should be Enter as separate keystroke (#178 settle delay)
    assert calls[3][0][0] == ["tmux", "send-keys", "-t", "claude-test-123", "Enter"]


def test_clear_running_session_no_wake_up(mock_client, mock_subprocess_run):
    """Test that clearing a running session doesn't send wake-up Enter."""
    # Mock session without completion_status (or with None)
    session = {
        "id": "test-456",
        "name": "test-session",
        "tmux_session": "claude-test-456",
        "parent_session_id": "parent-789",
        "completion_status": None,  # Not completed
        "friendly_name": "running-child",
    }

    mock_client.get_session.return_value = session
    mock_client.list_sessions.return_value = [session]

    # Call cmd_clear
    result = cmd_clear(
        client=mock_client,
        requester_session_id="parent-789",
        target_identifier="test-456",
        new_prompt=None,
    )

    # Should succeed
    assert result == 0

    # Verify tmux commands - should NOT start with wake-up Enter
    calls = mock_subprocess_run.call_args_list

    # First call should be Escape (NOT Enter)
    assert calls[0][0][0] == ["tmux", "send-keys", "-t", "claude-test-456", "Escape"]

    # Second call should be /clear text (two-call approach, #178)
    assert calls[1][0][0] == ["tmux", "send-keys", "-t", "claude-test-456", "--", "/clear"]

    # Third call should be Enter as separate keystroke (#178)
    assert calls[2][0][0] == ["tmux", "send-keys", "-t", "claude-test-456", "Enter"]


def test_clear_error_session_no_wake_up(mock_client, mock_subprocess_run):
    """Test that clearing an error session doesn't send wake-up Enter."""
    # Mock session with completion_status="error"
    session = {
        "id": "test-error",
        "name": "test-session",
        "tmux_session": "claude-test-error",
        "parent_session_id": "parent-abc",
        "completion_status": "error",  # Error, not completed
        "friendly_name": "error-child",
    }

    mock_client.get_session.return_value = session
    mock_client.list_sessions.return_value = [session]

    # Call cmd_clear
    result = cmd_clear(
        client=mock_client,
        requester_session_id="parent-abc",
        target_identifier="test-error",
        new_prompt=None,
    )

    # Should succeed
    assert result == 0

    # Verify tmux commands - should NOT start with wake-up Enter
    calls = mock_subprocess_run.call_args_list

    # First call should be Escape (NOT Enter)
    assert calls[0][0][0] == ["tmux", "send-keys", "-t", "claude-test-error", "Escape"]


def test_clear_with_new_prompt_after_completed(mock_client, mock_subprocess_run):
    """Test clearing a completed session with a new prompt."""
    session = {
        "id": "test-prompt",
        "name": "test-session",
        "tmux_session": "claude-test-prompt",
        "parent_session_id": "parent-def",
        "completion_status": "completed",
        "friendly_name": "prompt-child",
    }

    mock_client.get_session.return_value = session
    mock_client.list_sessions.return_value = [session]

    # Call cmd_clear with new prompt
    result = cmd_clear(
        client=mock_client,
        requester_session_id="parent-def",
        target_identifier="test-prompt",
        new_prompt="Start working on new task",
    )

    # Should succeed
    assert result == 0

    # Verify sequence includes wake-up, clear, and new prompt
    calls = mock_subprocess_run.call_args_list

    # Should have: Enter (wake), Escape, /clear text, Enter, new_prompt text, Enter (two-call, #178)
    assert len(calls) >= 6

    # First: wake up
    assert calls[0][0][0][4] == "Enter"
    # Then Escape
    assert calls[1][0][0][4] == "Escape"
    # Then /clear text (no \r — two-call approach, #178)
    assert calls[2][0][0] == ["tmux", "send-keys", "-t", "claude-test-prompt", "--", "/clear"]
    # Then Enter as separate keystroke (#178)
    assert calls[3][0][0] == ["tmux", "send-keys", "-t", "claude-test-prompt", "Enter"]
    # Then new prompt text (no \r — two-call approach, #178)
    assert calls[4][0][0] == ["tmux", "send-keys", "-t", "claude-test-prompt", "--", "Start working on new task"]
    # Then Enter as separate keystroke (#178)
    assert calls[5][0][0] == ["tmux", "send-keys", "-t", "claude-test-prompt", "Enter"]


def test_clear_not_authorized(mock_client, mock_subprocess_run):
    """Test that clearing requires parent-child ownership."""
    session = {
        "id": "test-unauthorized",
        "name": "test-session",
        "tmux_session": "claude-test-unauthorized",
        "parent_session_id": "different-parent",  # Different parent
        "completion_status": "completed",
        "friendly_name": "unauthorized-child",
    }

    mock_client.get_session.return_value = session
    mock_client.list_sessions.return_value = [session]

    # Try to clear from wrong parent
    result = cmd_clear(
        client=mock_client,
        requester_session_id="wrong-parent",  # Not the actual parent
        target_identifier="test-unauthorized",
        new_prompt=None,
    )

    # Should fail with authorization error
    assert result == 1

    # Should not send any tmux commands
    assert mock_subprocess_run.call_count == 0


def test_clear_abandoned_session_no_wake_up(mock_client, mock_subprocess_run):
    """Test that clearing an abandoned session doesn't send wake-up Enter."""
    session = {
        "id": "test-abandoned",
        "name": "test-session",
        "tmux_session": "claude-test-abandoned",
        "parent_session_id": "parent-xyz",
        "completion_status": "abandoned",  # Abandoned, not completed
        "friendly_name": "abandoned-child",
    }

    mock_client.get_session.return_value = session
    mock_client.list_sessions.return_value = [session]

    # Call cmd_clear
    result = cmd_clear(
        client=mock_client,
        requester_session_id="parent-xyz",
        target_identifier="test-abandoned",
        new_prompt=None,
    )

    # Should succeed
    assert result == 0

    # Verify first command is Escape (not Enter wake-up)
    calls = mock_subprocess_run.call_args_list
    assert calls[0][0][0][4] == "Escape"
