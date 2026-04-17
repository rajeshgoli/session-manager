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
    with patch.object(mq, "_run_codex_review_request_task", AsyncMock()):
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
    assert reg.requested_at == datetime(2026, 4, 17, 0, 0, 0)
    assert reg.next_retry_at == datetime(2026, 4, 17, 0, 10, 0)

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
    with patch.object(mq, "_run_codex_review_request_task", AsyncMock()):
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
async def test_register_codex_review_request_requires_explicit_repo_without_requester_context(mq):
    with pytest.raises(ValueError, match="pass --repo explicitly"):
        await mq.register_codex_review_request(
            pr_number=42,
            repo=None,
            requester_session_id=None,
            notify_session_id="agent618",
        )


@pytest.mark.asyncio
async def test_register_codex_review_request_serializes_duplicate_creation(mq):
    comment_call_count = 0

    def fake_post(*_args, **_kwargs):
        nonlocal comment_call_count
        comment_call_count += 1
        return {
            "comment_id": 321,
            "comment_url": "https://github.com/owner/repo/pull/42#issuecomment-321",
            "posted_at": "2026-04-17T00:00:00+00:00",
        }

    with patch.object(mq, "_run_codex_review_request_task", AsyncMock()):
        with patch("src.message_queue.validate_open_pr", return_value={"state": "OPEN"}):
            with patch("src.message_queue.post_pr_review_comment", side_effect=fake_post):
                with patch("src.message_queue.fetch_issue_comment", return_value=None):
                    first = asyncio.create_task(
                        mq.register_codex_review_request(
                            pr_number=42,
                            repo="owner/repo",
                            requester_session_id="agent618",
                            notify_session_id="agent618",
                        )
                    )
                    second = asyncio.create_task(
                        mq.register_codex_review_request(
                            pr_number=42,
                            repo="owner/repo",
                            requester_session_id="agent618",
                            notify_session_id="agent618",
                        )
                    )
                    first_result, second_result = await asyncio.gather(first, second, return_exceptions=True)

    assert isinstance(first_result, CodexReviewRequestRegistration)
    assert isinstance(second_result, ValueError)
    assert "already exists" in str(second_result)
    assert comment_call_count == 1


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
        with patch(
            "src.message_queue.get_codex_request_reaction_state",
            return_value={"picked_up": False, "clean_pass": False},
        ):
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


@pytest.mark.asyncio
async def test_codex_review_request_task_still_checks_review_when_pickup_lookup_fails(mq):
    reg = CodexReviewRequestRegistration(
        id="req124",
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
        with patch(
            "src.message_queue.get_codex_request_reaction_state",
            side_effect=RuntimeError("boom"),
        ):
            with patch(
                "src.message_queue.find_fresh_codex_review_or_comment",
                return_value={
                    "source": "review",
                    "created_at": "2026-04-17T00:01:00+00:00",
                    "id": 778,
                    "url": "https://github.com/owner/repo/pull/42#pullrequestreview-778",
                },
            ):
                await mq._run_codex_review_request_task(reg.id)

    assert reg.state == "completed"
    assert reg.is_active is False
    mq.queue_message.assert_called_once()
    assert "Codex review for PR #42 is here" in mq.queue_message.call_args.kwargs["text"]


@pytest.mark.asyncio
async def test_codex_review_request_task_completes_on_clean_pass_reaction(mq):
    reg = CodexReviewRequestRegistration(
        id="req125",
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
        with patch(
            "src.message_queue.get_codex_request_reaction_state",
            return_value={"picked_up": True, "clean_pass": True},
        ):
            await mq._run_codex_review_request_task(reg.id)

    assert reg.state == "completed"
    assert reg.is_active is False
    assert reg.review_source == "reaction"
    assert reg.review_comment_id == 321
    mq.queue_message.assert_called_once()
    assert "Codex review for PR #42 is here" in mq.queue_message.call_args.kwargs["text"]


def test_recover_codex_review_requests_restores_active_records(mock_session_manager, temp_db_path):
    mq1 = MessageQueueManager(mock_session_manager, db_path=temp_db_path, config={}, notifier=None)
    mock_session_manager.message_queue_manager = mq1

    with patch.object(mq1, "_run_codex_review_request_task", AsyncMock()):
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
    with patch.object(mq2, "_run_codex_review_request_task", AsyncMock()):
        asyncio.run(mq2._recover_codex_review_requests())

    recovered = mq2.list_codex_review_requests(notify_session_id="agent618")
    assert len(recovered) == 1
    assert recovered[0].repo == "owner/repo"


def test_recover_codex_review_requests_preserves_inactive_history(mock_session_manager, temp_db_path):
    mq1 = MessageQueueManager(mock_session_manager, db_path=temp_db_path, config={}, notifier=None)
    mock_session_manager.message_queue_manager = mq1

    with patch.object(mq1, "_run_codex_review_request_task", AsyncMock()):
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
                    reg = asyncio.run(
                        mq1.register_codex_review_request(
                            pr_number=42,
                            repo="owner/repo",
                            requester_session_id="agent618",
                            notify_session_id="agent618",
                        )
                    )
    mq1.cancel_codex_review_request(reg.id)

    mq2 = MessageQueueManager(mock_session_manager, db_path=temp_db_path, config={}, notifier=None)
    with patch.object(mq2, "_run_codex_review_request_task", AsyncMock()):
        asyncio.run(mq2._recover_codex_review_requests())

    recovered = mq2.list_codex_review_requests(notify_session_id="agent618", include_inactive=True)
    assert len(recovered) == 1
    assert recovered[0].id == reg.id
    assert recovered[0].state == "cancelled"
    assert recovered[0].is_active is False
    assert mq2.get_codex_review_request(reg.id) is not None
    assert reg.id not in mq2._codex_review_request_tasks


def test_cancel_codex_review_request_preserves_terminal_state(mq):
    reg = CodexReviewRequestRegistration(
        id="req-done",
        repo="owner/repo",
        pr_number=42,
        requester_session_id="agent618",
        notify_session_id="agent618",
        steer=None,
        requested_at=datetime(2026, 4, 17, 0, 0, 0),
        latest_request_comment_id=None,
        latest_request_comment_url=None,
        latest_request_posted_at=None,
        attempt_count=1,
        next_retry_at=None,
    )
    reg.is_active = False
    reg.state = "completed"
    reg.review_landed_at = datetime(2026, 4, 17, 0, 1, 0)
    mq._codex_review_requests[reg.id] = reg

    with patch.object(mq, "_update_codex_review_request_db") as update_db:
        cancelled = mq.cancel_codex_review_request(reg.id)

    assert cancelled is reg
    assert cancelled.state == "completed"
    assert cancelled.is_active is False
    update_db.assert_not_called()


@pytest.mark.asyncio
async def test_register_codex_review_request_resolves_repo_via_to_thread(mq):
    seen_repo_resolution = False

    async def fake_to_thread(func, *args, **kwargs):
        nonlocal seen_repo_resolution
        if func is infer_repo:
            seen_repo_resolution = True
        return func(*args, **kwargs)

    with patch.object(mq, "_run_codex_review_request_task", AsyncMock()):
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
                    with patch("src.message_queue.get_pr_repo_from_git", return_value="owner/repo") as infer_repo:
                        with patch("src.message_queue.asyncio.to_thread", side_effect=fake_to_thread):
                            reg = await mq.register_codex_review_request(
                                pr_number=42,
                                repo=None,
                                requester_session_id="agent618",
                                notify_session_id="agent618",
                            )

    assert reg.repo == "owner/repo"
    assert seen_repo_resolution is True


@pytest.mark.asyncio
async def test_codex_review_request_retry_persists_attempt_when_comment_refresh_fails(mq):
    reg = CodexReviewRequestRegistration(
        id="req-retry",
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
        next_retry_at=datetime(2026, 4, 17, 0, 0, 0),
    )
    mq._codex_review_requests[reg.id] = reg

    async def immediate_sleep(_seconds):
        return None

    persisted_attempts = []

    def capture_update(_request_id, **kwargs):
        if "attempt_count" in kwargs:
            persisted_attempts.append(kwargs)
            reg.is_active = False

    with patch("asyncio.sleep", side_effect=immediate_sleep):
        with patch(
            "src.message_queue.get_codex_request_reaction_state",
            return_value={"picked_up": False, "clean_pass": False},
        ):
            with patch("src.message_queue.find_fresh_codex_review_or_comment", return_value=None):
                with patch(
                    "src.message_queue.post_pr_review_comment",
                    return_value={
                        "comment_id": 999,
                        "comment_url": "https://github.com/owner/repo/pull/42#issuecomment-999",
                        "posted_at": "2026-04-17T00:05:00+00:00",
                    },
                    ):
                        with patch("src.message_queue.fetch_issue_comment", side_effect=RuntimeError("boom")):
                            with patch.object(mq, "_update_codex_review_request_db", side_effect=capture_update):
                                await mq._run_codex_review_request_task(reg.id)

    assert reg.attempt_count == 2
    assert reg.latest_request_comment_id == 999
    assert reg.latest_request_comment_url.endswith("999")
    assert reg.latest_request_posted_at == datetime(2026, 4, 17, 0, 5, 0)
    assert persisted_attempts
    assert persisted_attempts[-1]["attempt_count"] == 2
    assert persisted_attempts[-1]["latest_request_comment_id"] == 999


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


def test_codex_review_request_endpoint_maps_operational_failures(mock_session_manager):
    queue_mgr = MagicMock()
    queue_mgr.register_codex_review_request = AsyncMock(side_effect=RuntimeError("gh timed out"))
    mock_session_manager.message_queue_manager = queue_mgr
    app = create_app(session_manager=mock_session_manager)
    client = TestClient(app)

    response = client.post(
        "/codex-review-requests",
        json={
            "pr_number": 42,
            "repo": "owner/repo",
            "requester_session_id": "agent618",
        },
    )
    assert response.status_code == 400
    assert response.json()["detail"] == "gh timed out"

    queue_mgr.register_codex_review_request = AsyncMock(side_effect=TimeoutError("gh hung"))
    response = client.post(
        "/codex-review-requests",
        json={
            "pr_number": 42,
            "repo": "owner/repo",
            "requester_session_id": "agent618",
        },
    )
    assert response.status_code == 502
    assert response.json()["detail"] == "Failed to request Codex review: gh hung"


def test_codex_review_request_endpoint_serializes_string_review_ids(mock_session_manager):
    queue_mgr = MagicMock()
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
        review_landed_at=datetime(2026, 4, 17, 0, 11, 0),
        review_source="review",
        review_comment_id="R_kw123",
        review_url="https://github.com/owner/repo/pull/42",
    )
    reg.is_active = False
    reg.state = "completed"
    queue_mgr.get_codex_review_request.return_value = reg

    mock_session_manager.message_queue_manager = queue_mgr
    app = create_app(session_manager=mock_session_manager)
    client = TestClient(app)

    response = client.get("/codex-review-requests/req123")
    assert response.status_code == 200
    assert response.json()["review_comment_id"] == "R_kw123"


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

    def test_list_request_preserves_api_error_detail(self):
        client = SessionManagerClient()
        with patch.object(client, "_request_with_status") as mock_request:
            mock_request.return_value = ({"detail": "Notify target not found"}, 404, False)

            result = client.list_codex_review_requests(notify_target="missing")

            assert result["ok"] is False
            assert result["unavailable"] is False
            assert result["detail"] == "Notify target not found"


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
    client.list_codex_review_requests.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "requests": [
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
        },
    }
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
        repo=None,
        notify_target=None,
        list_all=False,
        json_output=False,
    )
    assert rc == 0
    assert "Request: req123" in capsys.readouterr().out

    rc = cmd_request_codex_review_cancel(client, "req123")
    assert rc == 0
    assert "Cancelled Codex review request: req123" in capsys.readouterr().out


def test_cmd_request_codex_review_create_infers_repo_outside_managed_session(capsys):
    client = MagicMock()
    client.create_codex_review_request.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {"id": "req123", "notify_name": "maintainer", "notify_session_id": "maintainer"},
    }

    with patch("src.cli.commands.get_pr_repo_from_git", return_value="owner/repo"):
        rc = cmd_request_codex_review_create(
            client,
            current_session_id=None,
            pr_number=42,
            repo=None,
            steer=None,
            notify_target="maintainer",
            poll_interval_seconds=30,
            retry_interval_seconds=600,
        )

    assert rc == 0
    call_kwargs = client.create_codex_review_request.call_args.kwargs
    assert call_kwargs["repo"] == "owner/repo"


def test_cmd_request_codex_review_create_managed_session_prefers_cwd_repo(capsys):
    client = MagicMock()
    client.create_codex_review_request.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {"id": "req123", "notify_name": "maintainer", "notify_session_id": "agent618"},
    }

    with patch("src.cli.commands.get_pr_repo_from_git", return_value="owner/repo") as infer_repo:
        rc = cmd_request_codex_review_create(
            client,
            current_session_id="agent618",
            pr_number=42,
            repo=None,
            steer=None,
            notify_target=None,
            poll_interval_seconds=30,
            retry_interval_seconds=600,
        )

    assert rc == 0
    infer_repo.assert_called_once()
    call_kwargs = client.create_codex_review_request.call_args.kwargs
    assert call_kwargs["repo"] == "owner/repo"


def test_cmd_request_codex_review_create_managed_session_falls_back_when_cwd_repo_unknown(capsys):
    client = MagicMock()
    client.create_codex_review_request.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {"id": "req123", "notify_name": "maintainer", "notify_session_id": "agent618"},
    }

    with patch("src.cli.commands.get_pr_repo_from_git", return_value=None) as infer_repo:
        rc = cmd_request_codex_review_create(
            client,
            current_session_id="agent618",
            pr_number=42,
            repo=None,
            steer=None,
            notify_target=None,
            poll_interval_seconds=30,
            retry_interval_seconds=600,
        )

    assert rc == 0
    infer_repo.assert_called_once()
    call_kwargs = client.create_codex_review_request.call_args.kwargs
    assert call_kwargs["repo"] is None


def test_cmd_request_codex_review_list_preserves_api_errors(capsys):
    client = MagicMock()
    client.list_codex_review_requests.return_value = {
        "ok": False,
        "unavailable": False,
        "detail": "Notify target not found",
        "data": None,
    }

    rc = cmd_request_codex_review_list(
        client,
        current_session_id="agent618",
        notify_target="missing",
        list_all=False,
        include_inactive=False,
        json_output=False,
    )

    assert rc == 1
    assert "Notify target not found" in capsys.readouterr().err


def test_cmd_request_codex_review_status_preserves_list_api_errors(capsys):
    client = MagicMock()
    client.list_codex_review_requests.return_value = {
        "ok": False,
        "unavailable": False,
        "detail": "Notify target not found",
        "data": None,
    }

    rc = cmd_request_codex_review_status(
        client,
        current_session_id=None,
        request_id=None,
        pr_number=42,
        repo="owner/repo",
        notify_target="missing",
        list_all=False,
        json_output=False,
    )

    assert rc == 1
    assert "Notify target not found" in capsys.readouterr().err


def test_cmd_request_codex_review_create_requires_repo_when_no_context(capsys):
    client = MagicMock()

    with patch("src.cli.commands.get_pr_repo_from_git", return_value=None):
        rc = cmd_request_codex_review_create(
            client,
            current_session_id=None,
            pr_number=42,
            repo=None,
            steer=None,
            notify_target="maintainer",
            poll_interval_seconds=30,
            retry_interval_seconds=600,
        )

    assert rc == 1
    assert "Could not determine GitHub repo" in capsys.readouterr().err
