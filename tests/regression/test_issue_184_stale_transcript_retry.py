"""
Regression tests for issue #184: Telegram notification delayed by one message.

The Stop hook can fire before Claude writes the current response to the
transcript JSONL file. When read_transcript() returns the previous response
(matching the stored output), the handler retries once after 300ms to allow
the transcript to be flushed.
"""

import json
import pytest
from unittest.mock import MagicMock, AsyncMock, patch
from fastapi.testclient import TestClient

from src.server import create_app, TRANSCRIPT_RETRY_DELAY_SECONDS
from src.models import Session, SessionStatus


def _make_transcript(path, assistant_text):
    """Write a minimal JSONL transcript with one assistant entry."""
    entry = {
        "type": "assistant",
        "message": {
            "content": [{"type": "text", "text": assistant_text}]
        },
    }
    path.write_text(json.dumps(entry) + "\n")


@pytest.fixture
def mock_session_manager():
    mock = MagicMock()
    mock.sessions = {}
    mock.tmux = MagicMock()
    mock.message_queue_manager = MagicMock()
    mock.message_queue_manager._restore_user_input_after_response = AsyncMock()
    mock._save_state = MagicMock()
    return mock


@pytest.fixture
def sample_session():
    return Session(
        id="agent-184",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="claude-agent-184",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
    )


@pytest.fixture
def app_and_client(mock_session_manager, sample_session):
    mock_session_manager.get_session.return_value = sample_session
    app = create_app(
        session_manager=mock_session_manager,
        notifier=None,
        output_monitor=MagicMock(),
        config={},
    )
    client = TestClient(app)
    return app, client


# ============================================================================
# Core: stale transcript triggers retry
# ============================================================================


def test_stale_transcript_triggers_retry(app_and_client, tmp_path):
    """
    When read_transcript() returns content matching the stored output,
    the handler retries after 300ms.
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    _make_transcript(transcript, "old response")

    # Pre-store the same content so the handler detects staleness
    app.state.last_claude_output["agent-184"] = "old response"

    with patch("asyncio.sleep", new_callable=AsyncMock) as mock_sleep:
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "agent-184",
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    mock_sleep.assert_awaited_once_with(TRANSCRIPT_RETRY_DELAY_SECONDS)


def test_stale_transcript_retry_picks_up_new_content(app_and_client, tmp_path):
    """
    After the 300ms retry, the handler uses the updated transcript content.
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    _make_transcript(transcript, "old response")

    app.state.last_claude_output["agent-184"] = "old response"

    original_sleep = AsyncMock()

    async def update_transcript_then_sleep(duration):
        """Simulate Claude writing the transcript during the 300ms wait."""
        _make_transcript(transcript, "new response")
        await original_sleep(duration)

    with patch("asyncio.sleep", side_effect=update_transcript_then_sleep):
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "agent-184",
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    # After retry, stored output should be the new content
    assert app.state.last_claude_output["agent-184"] == "new response"


# ============================================================================
# No retry when content is fresh
# ============================================================================


def test_fresh_transcript_no_retry(app_and_client, tmp_path):
    """
    When read_transcript() returns content different from stored output,
    no retry occurs.
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    _make_transcript(transcript, "new response")

    # Stored output is different — transcript is fresh
    app.state.last_claude_output["agent-184"] = "old response"

    with patch("asyncio.sleep", new_callable=AsyncMock) as mock_sleep:
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "agent-184",
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    mock_sleep.assert_not_awaited()


def test_no_stored_output_no_retry(app_and_client, tmp_path):
    """
    When there is no previously stored output for the session,
    no retry occurs (first interaction).
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    _make_transcript(transcript, "first response")

    # No pre-stored output
    assert "agent-184" not in app.state.last_claude_output

    with patch("asyncio.sleep", new_callable=AsyncMock) as mock_sleep:
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "agent-184",
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    mock_sleep.assert_not_awaited()
    # Output should still be stored normally
    assert app.state.last_claude_output["agent-184"] == "first response"


# ============================================================================
# Retry only applies to Stop hooks
# ============================================================================


def test_notification_hook_no_retry(app_and_client, tmp_path):
    """
    Notification hooks do not trigger the stale-transcript retry,
    even if transcript content matches stored output.
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    _make_transcript(transcript, "old response")

    app.state.last_claude_output["agent-184"] = "old response"

    with patch("asyncio.sleep", new_callable=AsyncMock) as mock_sleep:
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Notification",
                "notification_type": "idle_prompt",
                "session_manager_id": "agent-184",
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    mock_sleep.assert_not_awaited()


# ============================================================================
# No retry without session_manager_id
# ============================================================================


def test_no_session_manager_id_no_retry(app_and_client, tmp_path):
    """
    Stop hooks without session_manager_id do not trigger retry.
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    _make_transcript(transcript, "old response")

    app.state.last_claude_output["default"] = "old response"

    with patch("asyncio.sleep", new_callable=AsyncMock) as mock_sleep:
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "transcript_path": str(transcript),
                "session_id": "some-claude-id",
            },
        )

    assert response.status_code == 200
    mock_sleep.assert_not_awaited()


# ============================================================================
# Retry resilience: read failure on retry does not crash
# ============================================================================


def test_retry_read_failure_nullifies_stale_content(app_and_client, tmp_path):
    """
    If read_transcript() fails on retry (e.g. file deleted), last_message is
    set to None so stale content does not propagate to mark_session_idle.
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    _make_transcript(transcript, "old response")

    app.state.last_claude_output["agent-184"] = "old response"

    async def delete_file_then_sleep(duration):
        """Delete the transcript during the wait to simulate failure."""
        transcript.unlink()

    with patch("asyncio.sleep", side_effect=delete_file_then_sleep):
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "agent-184",
                "transcript_path": str(transcript),
            },
        )

    # Handler should complete without error
    assert response.status_code == 200
    # Stale content must NOT be stored — retry failure sets last_message = None
    # The stored output should remain the old value (not overwritten with stale)
    assert app.state.last_claude_output.get("agent-184") == "old response"
    # mark_session_idle should have been called with last_output=None
    queue_mgr = app_and_client[0].state.session_manager.message_queue_manager
    queue_mgr.mark_session_idle.assert_called_once_with("agent-184", last_output=None, from_stop_hook=True)
