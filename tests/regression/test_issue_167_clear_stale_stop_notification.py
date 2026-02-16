"""
Regression tests for issue #167: sm clear does not reset stop-hook notification message

When an agent is reused via `sm clear` + `sm send`, the stop-hook notification
should not relay the previous task's final message. These tests verify that:
1. The server's /clear endpoint invalidates cached output and notification state
2. The new /invalidate-cache endpoint works for CLI-driven clears
3. The CLI cmd_clear calls invalidate_cache after tmux operations
"""

import pytest
from unittest.mock import Mock, patch, AsyncMock, MagicMock
from datetime import datetime

from src.cli.commands import cmd_clear
from src.cli.client import SessionManagerClient
from src.models import SessionDeliveryState


# ============================================================================
# Server endpoint tests
# ============================================================================


@pytest.fixture
def app_with_state():
    """Create a mock FastAPI app with the state fields used by _invalidate_session_cache."""
    from src.server import _invalidate_session_cache

    app = Mock()
    app.state.last_claude_output = {}
    app.state.pending_stop_notifications = set()

    # Set up message_queue_manager with delivery_states
    queue_mgr = Mock()
    queue_mgr.delivery_states = {}
    app.state.session_manager = Mock()
    app.state.session_manager.message_queue_manager = queue_mgr

    return app, queue_mgr


def test_invalidate_clears_last_claude_output(app_with_state):
    """After invalidation, last_claude_output should not contain the session's entry."""
    from src.server import _invalidate_session_cache

    app, _ = app_with_state
    app.state.last_claude_output["session-abc"] = "Task 1 final message"

    _invalidate_session_cache(app, "session-abc")

    assert "session-abc" not in app.state.last_claude_output


def test_invalidate_clears_pending_stop_notifications(app_with_state):
    """After invalidation, session should be removed from pending_stop_notifications."""
    from src.server import _invalidate_session_cache

    app, _ = app_with_state
    app.state.pending_stop_notifications.add("session-abc")

    _invalidate_session_cache(app, "session-abc")

    assert "session-abc" not in app.state.pending_stop_notifications


def test_invalidate_clears_stop_notify_sender(app_with_state):
    """After invalidation, delivery_state stop_notify fields should be None."""
    from src.server import _invalidate_session_cache

    app, queue_mgr = app_with_state
    state = SessionDeliveryState(session_id="session-abc")
    state.stop_notify_sender_id = "parent-123"
    state.stop_notify_sender_name = "parent-agent"
    queue_mgr.delivery_states["session-abc"] = state

    _invalidate_session_cache(app, "session-abc")

    assert state.stop_notify_sender_id is None
    assert state.stop_notify_sender_name is None


def test_invalidate_no_delivery_state_is_noop(app_with_state):
    """Invalidation should not error when no delivery_state exists for the session."""
    from src.server import _invalidate_session_cache

    app, _ = app_with_state

    # Should not raise
    _invalidate_session_cache(app, "nonexistent-session")


def test_invalidate_no_session_manager_is_safe(app_with_state):
    """Invalidation should be safe when session_manager is None."""
    from src.server import _invalidate_session_cache

    app, _ = app_with_state
    app.state.session_manager = None
    app.state.last_claude_output["session-abc"] = "stale message"

    # Should not raise, and should still clear app.state caches
    _invalidate_session_cache(app, "session-abc")

    assert "session-abc" not in app.state.last_claude_output


def test_invalidate_preserves_other_sessions(app_with_state):
    """Invalidation should only affect the specified session, not others."""
    from src.server import _invalidate_session_cache

    app, queue_mgr = app_with_state
    app.state.last_claude_output["session-abc"] = "Task 1 message"
    app.state.last_claude_output["session-xyz"] = "Other session message"
    app.state.pending_stop_notifications.add("session-abc")
    app.state.pending_stop_notifications.add("session-xyz")

    other_state = SessionDeliveryState(session_id="session-xyz")
    other_state.stop_notify_sender_id = "other-parent"
    other_state.stop_notify_sender_name = "other-name"
    queue_mgr.delivery_states["session-xyz"] = other_state

    _invalidate_session_cache(app, "session-abc")

    # Other session should be untouched
    assert app.state.last_claude_output["session-xyz"] == "Other session message"
    assert "session-xyz" in app.state.pending_stop_notifications
    assert other_state.stop_notify_sender_id == "other-parent"
    assert other_state.stop_notify_sender_name == "other-name"


# ============================================================================
# CLI cmd_clear tests — verify invalidate_cache is called after tmux ops
# ============================================================================


@pytest.fixture
def mock_client():
    """Create a mock SessionManagerClient."""
    client = Mock(spec=SessionManagerClient)
    # Add invalidate_cache to the mock (spec= restricts to real methods,
    # but we just added it so Mock may not pick it up)
    client.invalidate_cache = Mock(return_value=(True, False))
    return client


@pytest.fixture
def mock_subprocess_run():
    """Mock subprocess.run to avoid actually sending tmux commands."""
    with patch("subprocess.run") as mock_run:
        mock_run.return_value = Mock(returncode=0, stdout="", stderr="")
        yield mock_run


def test_cmd_clear_calls_invalidate_cache(mock_client, mock_subprocess_run):
    """cmd_clear should call invalidate_cache after tmux operations succeed."""
    session = {
        "id": "child-001",
        "name": "test-session",
        "tmux_session": "claude-child-001",
        "parent_session_id": "parent-001",
        "completion_status": None,
        "friendly_name": "test-child",
    }

    mock_client.get_session.return_value = session
    mock_client.list_sessions.return_value = [session]

    result = cmd_clear(
        client=mock_client,
        requester_session_id="parent-001",
        target_identifier="child-001",
        new_prompt=None,
    )

    assert result == 0
    mock_client.invalidate_cache.assert_called_once_with("child-001")


def test_cmd_clear_invalidates_before_new_prompt(mock_client, mock_subprocess_run):
    """Cache invalidation should happen before the new prompt is sent."""
    session = {
        "id": "child-002",
        "name": "test-session",
        "tmux_session": "claude-child-002",
        "parent_session_id": "parent-002",
        "completion_status": None,
        "friendly_name": "test-child",
    }

    mock_client.get_session.return_value = session
    mock_client.list_sessions.return_value = [session]

    result = cmd_clear(
        client=mock_client,
        requester_session_id="parent-002",
        target_identifier="child-002",
        new_prompt="Start new task",
    )

    assert result == 0
    mock_client.invalidate_cache.assert_called_once_with("child-002")

    # Verify invalidate_cache was called — tmux calls for the new prompt
    # should follow after the cache invalidation
    tmux_calls = mock_subprocess_run.call_args_list
    # Escape, /clear, Enter are the tmux ops before invalidation
    # Then new prompt, Enter are after
    assert any(
        call[0][0][4] == "Start new task" for call in tmux_calls
    ), "New prompt should have been sent via tmux"


def test_cmd_clear_codex_app_does_not_call_invalidate(mock_client):
    """Codex app sessions use the server clear endpoint directly, no CLI invalidation needed."""
    session = {
        "id": "codex-001",
        "name": "codex-session",
        "provider": "codex-app",
        "parent_session_id": "parent-003",
        "friendly_name": "codex-child",
    }

    mock_client.get_session.return_value = session
    mock_client.list_sessions.return_value = [session]
    mock_client.clear_session.return_value = (True, False)

    result = cmd_clear(
        client=mock_client,
        requester_session_id="parent-003",
        target_identifier="codex-001",
        new_prompt=None,
    )

    assert result == 0
    # Codex app path calls client.clear_session which goes through server endpoint
    # (which already does cache invalidation), so no separate invalidate_cache call
    mock_client.invalidate_cache.assert_not_called()


# ============================================================================
# Integration-style: full stale notification scenario
# ============================================================================


def test_stale_cache_cleared_on_reuse(app_with_state):
    """Simulate the exact scenario from the bug report:
    Task 1 completes -> cache populated -> sm clear -> Task 2 stop hook fires.
    After the fix, the stale Task 1 message should not be in the cache."""
    from src.server import _invalidate_session_cache

    app, queue_mgr = app_with_state
    session_id = "engineer-1601"

    # Step 1: Task 1 completes — cache populated with Task 1's message
    app.state.last_claude_output[session_id] = "Task 1: PR #153 created"
    state = SessionDeliveryState(session_id=session_id)
    state.stop_notify_sender_id = "em-parent"
    state.stop_notify_sender_name = "em-1604"
    queue_mgr.delivery_states[session_id] = state

    # Step 2: sm clear is called — should invalidate all cached state
    _invalidate_session_cache(app, session_id)

    # Step 3: Verify — cache should be clean
    assert session_id not in app.state.last_claude_output
    assert session_id not in app.state.pending_stop_notifications
    assert state.stop_notify_sender_id is None
    assert state.stop_notify_sender_name is None
