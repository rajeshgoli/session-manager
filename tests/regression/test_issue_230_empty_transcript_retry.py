"""
Regression tests for issue #230: Empty transcript retry in Stop hook handler.

The Stop hook fires before Claude flushes the transcript JSONL to disk.
read_transcript() returns None. Previously the handler deferred the
notification to the next idle_prompt hook (up to 16 minutes later).

Fix: retry once after 500ms before deferring.
"""

import json
import pytest
from unittest.mock import MagicMock, AsyncMock, patch
from fastapi.testclient import TestClient

from src.server import create_app, EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS
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
        id="agent-230",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="claude-agent-230",
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
# Core: empty transcript triggers retry
# ============================================================================


def test_empty_transcript_triggers_retry(app_and_client, tmp_path):
    """
    When read_transcript() returns None on the first read, the handler retries
    after EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS.
    """
    app, client = app_and_client
    # No transcript file — read_transcript will return (True, None)
    transcript = tmp_path / "transcript.jsonl"
    transcript.write_text("")  # empty file → no assistant entry

    with patch("asyncio.sleep", new_callable=AsyncMock) as mock_sleep:
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "agent-230",
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    # Sleep should be called once for the empty-transcript retry
    calls = mock_sleep.await_args_list
    assert any(
        call.args == (EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS,) or
        call.args[0] == EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS
        for call in calls
    ), f"Expected asyncio.sleep({EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS}) but got: {calls}"


def test_empty_transcript_retry_succeeds_no_deferral(app_and_client, tmp_path):
    """
    When the retry succeeds (transcript written during the 500ms wait),
    the notification is sent immediately — session is NOT added to
    pending_stop_notifications.
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    transcript.write_text("")  # empty initially

    original_sleep = AsyncMock()

    async def write_transcript_then_sleep(duration):
        """Simulate Claude flushing the transcript during the 500ms retry wait."""
        _make_transcript(transcript, "response after flush")
        await original_sleep(duration)

    with patch("asyncio.sleep", side_effect=write_transcript_then_sleep):
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "agent-230",
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    # Retry succeeded → session must NOT be deferred
    assert "agent-230" not in app.state.pending_stop_notifications
    # Content stored after successful retry
    assert app.state.last_claude_output.get("agent-230") == "response after flush"


def test_empty_transcript_retry_fails_deferred(app_and_client, tmp_path):
    """
    When both the initial read and the retry return None, the session is added
    to pending_stop_notifications (deferred to the next idle_prompt hook).
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    transcript.write_text("")  # stays empty through both reads

    with patch("asyncio.sleep", new_callable=AsyncMock):
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "agent-230",
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    # Both reads returned None → deferred
    assert "agent-230" in app.state.pending_stop_notifications


# ============================================================================
# Retry only applies to Stop hooks
# ============================================================================


def test_notification_hook_no_empty_retry(app_and_client, tmp_path):
    """
    Notification hooks do not trigger the empty-transcript retry,
    even when the transcript is empty.
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    transcript.write_text("")  # empty

    with patch("asyncio.sleep", new_callable=AsyncMock) as mock_sleep:
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Notification",
                "notification_type": "idle_prompt",
                "session_manager_id": "agent-230",
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    mock_sleep.assert_not_awaited()


def test_stop_hook_fresh_transcript_no_empty_retry(app_and_client, tmp_path):
    """
    When the transcript already has content on the first read, the
    empty-transcript retry does not fire.
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    _make_transcript(transcript, "immediate response")

    with patch("asyncio.sleep", new_callable=AsyncMock) as mock_sleep:
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "agent-230",
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    mock_sleep.assert_not_awaited()
    # Content stored without retry
    assert app.state.last_claude_output.get("agent-230") == "immediate response"


# ============================================================================
# No session_manager_id — retry still fires
# ============================================================================


def test_no_session_manager_id_empty_retry_still_fires(app_and_client, tmp_path):
    """
    The empty-transcript retry fires even when session_manager_id is absent.
    The guard is `hook_event == 'Stop' and not last_message` only — no
    session_manager_id check — so sessions without the env var still benefit.
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    transcript.write_text("")  # empty

    with patch("asyncio.sleep", new_callable=AsyncMock) as mock_sleep:
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "transcript_path": str(transcript),
                "session_id": "some-claude-id",
                # no session_manager_id
            },
        )

    assert response.status_code == 200
    calls = mock_sleep.await_args_list
    assert any(
        call.args == (EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS,) or
        (call.args and call.args[0] == EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS)
        for call in calls
    ), f"Expected empty-transcript retry sleep but got: {calls}"


# ============================================================================
# Stale retry (#184) unaffected
# ============================================================================


def test_stale_retry_unaffected_when_content_present(app_and_client, tmp_path):
    """
    When the transcript already has content (non-None), the empty-transcript
    retry (#230) does not fire. The stale retry (#184) fires when content
    matches the stored output — mutually exclusive with #230.
    """
    from src.server import TRANSCRIPT_RETRY_DELAY_SECONDS

    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    _make_transcript(transcript, "old response")

    # Pre-store same content → stale retry (#184) will fire
    app.state.last_claude_output["agent-230"] = "old response"

    sleep_calls = []

    async def capture_sleep(duration):
        sleep_calls.append(duration)

    with patch("asyncio.sleep", side_effect=capture_sleep):
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "agent-230",
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    # Only the stale retry (#184) should have fired, not the empty retry (#230)
    assert TRANSCRIPT_RETRY_DELAY_SECONDS in sleep_calls
    assert EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS not in sleep_calls


# ============================================================================
# Retry resilience: exception on retry does not crash
# ============================================================================


def test_retry_exception_does_not_crash(app_and_client, tmp_path):
    """
    If read_transcript() raises an exception during the retry, the handler
    catches it, sets last_message=None, and continues without crashing.
    The session is added to pending_stop_notifications (deferred).
    """
    app, client = app_and_client
    transcript = tmp_path / "transcript.jsonl"
    transcript.write_text("")  # empty on first read

    call_count = [0]
    original_to_thread = __import__("asyncio").to_thread

    async def mock_to_thread(func, *args, **kwargs):
        call_count[0] += 1
        if call_count[0] == 1:
            return (True, None)  # first read: empty
        raise RuntimeError("simulated IO error on retry")

    with patch("asyncio.sleep", new_callable=AsyncMock), \
         patch("asyncio.to_thread", side_effect=mock_to_thread):
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "agent-230",
                "transcript_path": str(transcript),
            },
        )

    # Handler must complete without 500 error
    assert response.status_code == 200
    # Retry failed → deferred
    assert "agent-230" in app.state.pending_stop_notifications
