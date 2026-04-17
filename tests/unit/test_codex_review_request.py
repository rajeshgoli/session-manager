"""Unit tests for durable Codex PR review requests (#618)."""

from __future__ import annotations

import asyncio
import sqlite3
from datetime import datetime
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from src.cli.client import SessionManagerClient
from src.cli.commands import (
    cmd_request_codex_review_cancel,
    cmd_request_codex_review_create,
    cmd_request_codex_review_list,
    cmd_request_codex_review_status,
)
from src.message_queue import MessageQueueManager
from src.models import CodexReviewRequestRegistration, Session, SessionStatus
from src.server import create_app


def noop_create_task(coro):
    """Silently close coroutine without running it."""
    coro.close()
    return MagicMock()


@pytest.fixture
def temp_db_path(tmp_path):
    return str(tmp_path / "test_codex_review_request.db")


@pytest.fixture
def mock_session_manager(tmp_path):
    session = Session(
        id="agent618",
        name="maintainer",
        working_dir=str(tmp_path),
        tmux_session="claude-agent618",
        provider="claude",
        log_file=str(tmp_path / "agent.log"),
        status=SessionStatus.RUNNING,
    )
    mock = MagicMock()
    mock.sessions = {session.id: session}
    mock.get_session.side_effect = lambda session_id: session if session_id == session.id else None
    mock.get_effective_session_name.side_effect = lambda current: current.name if current else None
    mock.tmux = MagicMock()
    mock._save_state = MagicMock()
    mock._deliver_direct = AsyncMock(return_value=True)
    mock.message_queue_manager = None
    mock.lookup_agent_registration = MagicMock(return_value=None)
    return mock


@pytest.fixture
def mq(mock_session_manager, temp_db_path):
    queue = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=temp_db_path,
        config={},
        notifier=None,
    )
    mock_session_manager.message_queue_manager = queue
    return queue


@pytest.mark.asyncio
async def test_register_codex_review_request_persists_and_lists(mq, mock_session_manager, temp_db_path):
    with patch("asyncio.create_task", noop_create_task):
        with patch("src.message_queue.validate_open_pr", return_value={"state": "OPEN"}):
            with patch(
                "src.message_queue.post_pr_review_comment",
                return_value={
                    "comment_id": 321,
                    "comment_url": "https://github.com/owner/repo/pull/42#issuecomment-321",
                    "posted_at": "2026-04-17T00:00:00+00:00",
                },
            ):
                with patch(
                    "src.message_queue.fetch_issue_comment",
                    return_value={
                        "id": 321,
                        "html_url": "https://github.com/owner/repo/pull/42#issuecomment-321",
                        "created_at": "2026-04-17T00:00:00Z",
                    },
                ):
                    reg = await mq.register_codex_review_request(
                        pr_number=42,
                        repo="owner/repo",
                        requester_session_id="agent618",
                        notify_session_id="agent618",
                    )

    assert reg.id in mq._codex_review_requests
    assert mq.list_codex_review_requests(notify_session_id="agent618")[0].pr_number == 42

    conn = sqlite3.connect(temp_db_path)
    row = conn.execute(
        "SELECT repo, pr_number, notify_session_id, attempt_count, is_active "
        "FROM codex_review_request_registrations WHERE id = ?",
        (reg.id,),
    ).fetchone()
    conn.close()
    assert row == ("owner/repo", 42, "agent618", 1, 1)


@pytest.mark.asyncio
async def test_register_codex_review_request_rejects_duplicates(mq):
    with patch("asyncio.create_task", noop_create_task):
        with patch("src.message_queue.validate_open_pr", return_value={"state": "OPEN"}):
            with patch(
                "src.message_queue.post_pr_review_comment",
                return_value={
                    "comment_id": 321,
                    "comment_url": "https://github.com/owner/repo/pull/42#issuecomment-321",
                    "posted_at": "2026-04-17T00:00:00+00:00",
                },
            ):
                with patch("src.message_queue.fetch_issue_comment", return_value=None):
                    await mq.register_codex_review_request(
                        pr_number=42,
                        repo="owner/repo",
                        requester_session_id="agent618",
                        notify_session_id="agent618",
                    )
                    with pytest.raises(ValueError, match="already exists"):
                        await mq.register_codex_review_request(
                            pr_number=42,
                            repo="owner/repo",
                            requester_session_id="agent618",
                            notify_session_id="agent618",
                        )


@pytest.mark.asyncio
async def test_codex_review_request_task_completes_and_queues_message(mq):
    reg = CodexReviewRequestRegistration(
        id="req123",
        repo="owner/repo",
        pr_number=42,
        requester_session_id="agent618",
        notify_session_id="agent618",
        steer=None,
        requested_at=datetime(2026, 4, 17, 0, 0, 0),
        latest_request_comment_id=321,
        latest_request_comment_url="https://github.com/owner/repo/pull/42#issuecomment-321",
        latest_request_posted_at=datetime(2026, 4, 17, 0, 0, 0),
        attempt_count=1,
        next_retry_at=datetime(2026, 4, 17, 0, 10, 0),
    )
    mq._codex_review_requests[reg.id] = reg
    mq.queue_message = MagicMock()

    async def immediate_sleep(_seconds):
        return None

    with patch("asyncio.sleep", side_effect=immediate_sleep):
        with patch("src.message_queue.detect_codex_pickup", return_value=False):
            with patch(
                "src.message_queue.find_fresh_codex_review_or_comment",
                return_value={
                    "source": "comment",
                    "created_at": "2026-04-17T00:01:00+00:00",
                    "id": 777,
                    "url": "https://github.com/owner/repo/pull/42#issuecomment-777",
                },
            ):
                await mq._run_codex_review_request_task(reg.id)

    assert reg.state == "completed"
    assert reg.is_active is False
    mq.queue_message.assert_called_once()
    assert "Codex comment for PR #42 is here" in mq.queue_message.call_args.kwargs["text"]


def test_recover_codex_review_requests_restores_active_records(mock_session_manager, temp_db_path):
    mq1 = MessageQueueManager(mock_session_manager, db_path=temp_db_path, config={}, notifier=None)
    mock_session_manager.message_queue_manager = mq1

    with patch("asyncio.create_task", noop_create_task):
        with patch("src.message_queue.validate_open_pr", return_value={"state": "OPEN"}):
            with patch(
                "src.message_queue.post_pr_review_comment",
                return_value={
                    "comment_id": 321,
                    "comment_url": "https://github.com/owner/repo/pull/42#issuecomment-321",
                    "posted_at": "2026-04-17T00:00:00+00:00",
                },
            ):
                with patch("src.message_queue.fetch_issue_comment", return_value=None):
                    asyncio.run(
                        mq1.register_codex_review_request(
                            pr_number=42,
                            repo="owner/repo",
                            requester_session_id="agent618",
                            notify_session_id="agent618",
                        )
                    )

    mq2 = MessageQueueManager(mock_session_manager, db_path=temp_db_path, config={}, notifier=None)
    with patch("asyncio.create_task", noop_create_task):
        asyncio.run(mq2._recover_codex_review_requests())

    recovered = mq2.list_codex_review_requests(notify_session_id="agent618")
    assert len(recovered) == 1
    assert recovered[0].repo == "owner/repo"


def test_codex_review_request_endpoints_roundtrip(mock_session_manager):
    queue_mgr = MagicMock()
    reg = CodexReviewRequestRegistration(
        id="req123",
        repo="owner/repo",
        pr_number=42,
        requester_session_id="agent618",
        notify_session_id="agent618",
        steer="focus on races",
        requested_at=datetime(2026, 4, 17, 0, 0, 0),
        latest_request_comment_id=321,
        latest_request_comment_url="https://github.com/owner/repo/pull/42#issuecomment-321",
        latest_request_posted_at=datetime(2026, 4, 17, 0, 0, 0),
        attempt_count=1,
        next_retry_at=datetime(2026, 4, 17, 0, 10, 0),
    )
    queue_mgr.register_codex_review_request = AsyncMock(return_value=reg)
    queue_mgr.list_codex_review_requests.return_value = [reg]
    queue_mgr.get_codex_review_request.return_value = reg
    cancelled = CodexReviewRequestRegistration(
        id=reg.id,
        repo=reg.repo,
        pr_number=reg.pr_number,
        requester_session_id=reg.requester_session_id,
        notify_session_id=reg.notify_session_id,
        steer=reg.steer,
        requested_at=reg.requested_at,
        latest_request_comment_id=reg.latest_request_comment_id,
        latest_request_comment_url=reg.latest_request_comment_url,
        latest_request_posted_at=reg.latest_request_posted_at,
        attempt_count=reg.attempt_count,
        next_retry_at=reg.next_retry_at,
    )
    cancelled.is_active = False
    cancelled.state = "cancelled"
    queue_mgr.cancel_codex_review_request.return_value = cancelled

    mock_session_manager.message_queue_manager = queue_mgr
    app = create_app(session_manager=mock_session_manager)
    client = TestClient(app)

    create_response = client.post(
        "/codex-review-requests",
        json={
            "pr_number": 42,
            "repo": "owner/repo",
            "requester_session_id": "agent618",
        },
    )
    assert create_response.status_code == 200
    assert create_response.json()["id"] == "req123"

    list_response = client.get("/codex-review-requests?notify_target=agent618")
    assert list_response.status_code == 200
    assert list_response.json()["requests"][0]["id"] == "req123"

    get_response = client.get("/codex-review-requests/req123")
    assert get_response.status_code == 200
    assert get_response.json()["repo"] == "owner/repo"

    cancel_response = client.delete("/codex-review-requests/req123")
    assert cancel_response.status_code == 200
    assert cancel_response.json()["state"] == "cancelled"
    assert cancel_response.json()["is_active"] is False


class TestClientCodexReviewRequest:
    """Tests for SessionManagerClient codex review request helpers."""

    def test_create_request_payload(self):
        client = SessionManagerClient()
        with patch.object(client, "_request_with_status") as mock_request:
            mock_request.return_value = ({"id": "req123"}, 200, False)

            result = client.create_codex_review_request(
                pr_number=42,
                repo="owner/repo",
                steer="focus on races",
                notify_target="maintainer",
                requester_session_id="agent618",
            )

            mock_request.assert_called_once_with(
                "POST",
                "/codex-review-requests",
                {
                    "pr_number": 42,
                    "poll_interval_seconds": 30,
                    "retry_interval_seconds": 600,
                    "repo": "owner/repo",
                    "steer": "focus on races",
                    "notify_target": "maintainer",
                    "requester_session_id": "agent618",
                },
                timeout=30,
            )
            assert result["ok"] is True


def test_cmd_request_codex_review_create_list_status_cancel(capsys):
    client = MagicMock()
    client.create_codex_review_request.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "id": "req123",
            "notify_name": "maintainer",
            "notify_session_id": "agent618",
        },
    }
    client.list_codex_review_requests.return_value = [
        {
            "id": "req123",
            "repo": "owner/repo",
            "pr_number": 42,
            "notify_name": "maintainer",
            "state": "active",
            "attempt_count": 1,
            "pickup_detected_at": None,
            "next_retry_at": "2026-04-17T00:10:00",
        }
    ]
    client.get_codex_review_request.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "id": "req123",
            "repo": "owner/repo",
            "pr_number": 42,
            "notify_name": "maintainer",
            "state": "active",
            "attempt_count": 1,
            "pickup_detected_at": None,
            "review_landed_at": None,
            "review_source": None,
            "next_retry_at": "2026-04-17T00:10:00",
            "last_error": None,
        },
    }
    client.cancel_codex_review_request.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {"id": "req123"},
    }

    rc = cmd_request_codex_review_create(
        client,
        current_session_id="agent618",
        pr_number=42,
        repo="owner/repo",
        steer=None,
        notify_target=None,
        poll_interval_seconds=30,
        retry_interval_seconds=600,
    )
    assert rc == 0
    assert "Review requested for PR #42" in capsys.readouterr().out

    rc = cmd_request_codex_review_list(
        client,
        current_session_id="agent618",
        notify_target=None,
        list_all=False,
        include_inactive=False,
        json_output=False,
    )
    assert rc == 0
    assert "req123" in capsys.readouterr().out

    rc = cmd_request_codex_review_status(
        client,
        current_session_id="agent618",
        request_id="req123",
        pr_number=None,
        notify_target=None,
        list_all=False,
        json_output=False,
    )
    assert rc == 0
    assert "Request: req123" in capsys.readouterr().out

    rc = cmd_request_codex_review_cancel(client, "req123")
    assert rc == 0
    assert "Cancelled Codex review request: req123" in capsys.readouterr().out
