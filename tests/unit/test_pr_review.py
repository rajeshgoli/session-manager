"""Unit tests for PR review feature — #141.

Tests the server endpoint, CLI command dispatch, client method,
and Codex review bug fixes.
"""

import json
from datetime import datetime
from unittest.mock import MagicMock, AsyncMock, patch

import pytest
from fastapi.testclient import TestClient

from src.server import create_app
from src.models import Session, SessionStatus, ReviewConfig


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager for PR review tests."""
    mock = MagicMock()
    mock.sessions = {}
    mock.tmux = MagicMock()
    mock.tmux.list_sessions = MagicMock(return_value=[])
    mock.message_queue_manager = None
    mock._save_state = MagicMock()
    return mock


@pytest.fixture
def mock_output_monitor():
    mock = MagicMock()
    mock.start_monitoring = AsyncMock()
    mock.cleanup_session = AsyncMock()
    mock.update_activity = MagicMock()
    mock._tasks = {}
    return mock


@pytest.fixture
def test_client(mock_session_manager, mock_output_monitor):
    app = create_app(
        session_manager=mock_session_manager,
        notifier=None,
        output_monitor=mock_output_monitor,
        config={},
    )
    return TestClient(app)


@pytest.fixture
def sample_session():
    return Session(
        id="test123",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="codex-test123",
        provider="codex",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
        created_at=datetime(2024, 1, 15, 10, 0, 0),
        last_activity=datetime(2024, 1, 15, 11, 0, 0),
    )


class TestPRReviewEndpoint:
    """Tests for POST /reviews/pr endpoint."""

    def test_pr_review_success(self, test_client, mock_session_manager):
        """POST /reviews/pr returns success result."""
        mock_session_manager.start_pr_review = AsyncMock(return_value={
            "repo": "owner/repo",
            "pr_number": 42,
            "posted_at": "2026-02-14T10:00:00",
            "comment_id": 12345,
            "comment_body": "@codex review",
            "status": "posted",
            "server_polling": False,
        })

        response = test_client.post(
            "/reviews/pr",
            json={"pr_number": 42, "repo": "owner/repo"},
        )
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "posted"
        assert data["pr_number"] == 42
        assert data["repo"] == "owner/repo"

    def test_pr_review_returns_error_as_200(self, test_client, mock_session_manager):
        """POST /reviews/pr returns 200 with error payload (not HTTP 400)."""
        mock_session_manager.start_pr_review = AsyncMock(return_value={
            "error": "PR #999 not found in owner/repo",
        })

        response = test_client.post(
            "/reviews/pr",
            json={"pr_number": 999, "repo": "owner/repo"},
        )
        # Key fix: should be 200, not 400
        assert response.status_code == 200
        data = response.json()
        assert "error" in data

    def test_pr_review_with_steer(self, test_client, mock_session_manager):
        """POST /reviews/pr passes steer to session manager."""
        mock_session_manager.start_pr_review = AsyncMock(return_value={
            "repo": "owner/repo",
            "pr_number": 42,
            "posted_at": "2026-02-14T10:00:00",
            "comment_id": 12345,
            "comment_body": "@codex review for security",
            "status": "posted",
            "server_polling": False,
        })

        response = test_client.post(
            "/reviews/pr",
            json={
                "pr_number": 42,
                "repo": "owner/repo",
                "steer": "security",
            },
        )
        assert response.status_code == 200

        mock_session_manager.start_pr_review.assert_called_once()
        call_kwargs = mock_session_manager.start_pr_review.call_args
        assert call_kwargs.kwargs["steer"] == "security"

    def test_pr_review_with_wait_and_caller(self, test_client, mock_session_manager):
        """POST /reviews/pr with wait and caller_session_id triggers server polling."""
        mock_session_manager.start_pr_review = AsyncMock(return_value={
            "repo": "owner/repo",
            "pr_number": 42,
            "posted_at": "2026-02-14T10:00:00",
            "comment_id": 12345,
            "comment_body": "@codex review",
            "status": "posted",
            "server_polling": True,
        })

        response = test_client.post(
            "/reviews/pr",
            json={
                "pr_number": 42,
                "repo": "owner/repo",
                "wait": 600,
                "caller_session_id": "parent123",
            },
        )
        assert response.status_code == 200
        data = response.json()
        assert data["server_polling"] is True

    def test_pr_review_unavailable(self):
        """POST /reviews/pr returns 503 when session manager not configured."""
        app = create_app(session_manager=None)
        client = TestClient(app)

        response = client.post("/reviews/pr", json={"pr_number": 42})
        assert response.status_code == 503


class TestReviewEndpointErrorFix:
    """Tests for Codex P2 fix: review endpoints return 200 with error payload."""

    def test_start_review_error_returns_200(self, test_client, mock_session_manager, sample_session):
        """POST /sessions/{id}/review returns 200 with error, not HTTP 400."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.start_review = AsyncMock(return_value={
            "error": "Session is busy",
        })

        response = test_client.post(
            "/sessions/test123/review",
            json={"mode": "branch", "base_branch": "main"},
        )
        # Codex P2 fix: was 400, should now be 200 with error payload
        assert response.status_code == 200
        data = response.json()
        assert "error" in data

    def test_spawn_review_failure_returns_200(self, test_client, mock_session_manager, sample_session):
        """POST /sessions/review returns 200 with error, not HTTP 500."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.spawn_review_session = AsyncMock(return_value=None)

        response = test_client.post(
            "/sessions/review",
            json={
                "parent_session_id": "test123",
                "mode": "branch",
                "base_branch": "main",
            },
        )
        # Codex P2 fix: was 500, should now be 200 with error payload
        assert response.status_code == 200
        data = response.json()
        assert "error" in data


class TestCmdReviewPR:
    """Tests for cmd_review --pr dispatch path."""

    def test_pr_mode_calls_start_pr_review(self):
        """cmd_review with pr dispatches to client.start_pr_review."""
        from src.cli.commands import cmd_review

        mock_client = MagicMock()
        mock_client.start_pr_review.return_value = {
            "repo": "owner/repo",
            "pr_number": 42,
            "posted_at": "2026-02-14T10:00:00",
            "comment_id": 12345,
            "status": "posted",
            "server_polling": True,
        }

        exit_code = cmd_review(
            client=mock_client,
            parent_session_id="parent123",
            pr=42,
            repo="owner/repo",
            wait=600,
        )

        assert exit_code == 0
        mock_client.start_pr_review.assert_called_once_with(
            pr_number=42,
            repo="owner/repo",
            steer=None,
            wait=600,
            caller_session_id="parent123",
        )

    def test_pr_mode_mutually_exclusive_with_session(self):
        """--pr rejects session argument."""
        from src.cli.commands import cmd_review

        mock_client = MagicMock()
        exit_code = cmd_review(
            client=mock_client,
            parent_session_id=None,
            session="some-session",
            pr=42,
        )
        assert exit_code == 1

    def test_pr_mode_mutually_exclusive_with_new(self):
        """--pr rejects --new."""
        from src.cli.commands import cmd_review

        mock_client = MagicMock()
        exit_code = cmd_review(
            client=mock_client,
            parent_session_id=None,
            new=True,
            pr=42,
        )
        assert exit_code == 1

    def test_pr_mode_mutually_exclusive_with_base(self):
        """--pr rejects --base."""
        from src.cli.commands import cmd_review

        mock_client = MagicMock()
        exit_code = cmd_review(
            client=mock_client,
            parent_session_id=None,
            base="main",
            pr=42,
        )
        assert exit_code == 1

    def test_pr_mode_error_from_api(self):
        """cmd_review returns 1 when API returns error."""
        from src.cli.commands import cmd_review

        mock_client = MagicMock()
        mock_client.start_pr_review.return_value = {
            "error": "PR not found",
        }

        exit_code = cmd_review(
            client=mock_client,
            parent_session_id=None,
            pr=42,
            repo="owner/repo",
        )
        assert exit_code == 1

    def test_pr_mode_unavailable(self):
        """cmd_review returns 2 when session manager unavailable."""
        from src.cli.commands import cmd_review

        mock_client = MagicMock()
        mock_client.start_pr_review.return_value = None

        exit_code = cmd_review(
            client=mock_client,
            parent_session_id=None,
            pr=42,
            repo="owner/repo",
        )
        assert exit_code == 2

    def test_pr_mode_defaults_wait_with_session_context(self):
        """--wait defaults to 600 when caller has session context."""
        from src.cli.commands import cmd_review

        mock_client = MagicMock()
        mock_client.start_pr_review.return_value = {
            "repo": "owner/repo",
            "pr_number": 42,
            "posted_at": "2026-02-14T10:00:00",
            "comment_id": 12345,
            "status": "posted",
            "server_polling": True,
        }

        cmd_review(
            client=mock_client,
            parent_session_id="parent123",
            pr=42,
            repo="owner/repo",
            # wait not provided
        )

        call_kwargs = mock_client.start_pr_review.call_args
        assert call_kwargs.kwargs["wait"] == 600


class TestClientStartPrReview:
    """Tests for SessionManagerClient.start_pr_review."""

    def test_sends_correct_payload(self):
        """start_pr_review sends correct POST to /reviews/pr."""
        from src.cli.client import SessionManagerClient

        client = SessionManagerClient()

        with patch.object(client, "_request") as mock_request:
            mock_request.return_value = (
                {"status": "posted", "repo": "owner/repo"},
                True,
                False,
            )

            result = client.start_pr_review(
                pr_number=42,
                repo="owner/repo",
                steer="security",
                wait=600,
                caller_session_id="parent123",
            )

            mock_request.assert_called_once_with(
                "POST",
                "/reviews/pr",
                {
                    "pr_number": 42,
                    "repo": "owner/repo",
                    "steer": "security",
                    "wait": 600,
                    "caller_session_id": "parent123",
                },
                timeout=30,
            )
            assert result["status"] == "posted"

    def test_returns_none_when_unavailable(self):
        """start_pr_review returns None when server is unavailable."""
        from src.cli.client import SessionManagerClient

        client = SessionManagerClient()

        with patch.object(client, "_request") as mock_request:
            mock_request.return_value = (None, False, True)

            result = client.start_pr_review(pr_number=42, repo="owner/repo")
            assert result is None


class TestReviewConfigPRFields:
    """Tests for ReviewConfig PR mode fields."""

    def test_pr_mode_config(self):
        """ReviewConfig supports PR mode fields."""
        config = ReviewConfig(
            mode="pr",
            pr_number=42,
            pr_repo="owner/repo",
            pr_comment_id=12345,
            steer_text="focus on security",
        )

        assert config.mode == "pr"
        assert config.pr_number == 42
        assert config.pr_repo == "owner/repo"
        assert config.pr_comment_id == 12345

    def test_pr_config_roundtrip(self):
        """ReviewConfig with PR fields serializes correctly."""
        config = ReviewConfig(
            mode="pr",
            pr_number=42,
            pr_repo="owner/repo",
            pr_comment_id=12345,
        )

        as_dict = config.to_dict()
        restored = ReviewConfig.from_dict(as_dict)

        assert restored.mode == "pr"
        assert restored.pr_number == 42
        assert restored.pr_repo == "owner/repo"
        assert restored.pr_comment_id == 12345
