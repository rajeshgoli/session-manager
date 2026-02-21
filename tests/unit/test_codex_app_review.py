"""Unit tests for codex-app review integration — #140.

Tests review_start() RPC method, review lifecycle notification handling,
and session_manager wiring for codex-app provider.
"""

import asyncio
import json
from datetime import datetime
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.codex_app_server import (
    CodexAppServerConfig,
    CodexAppServerError,
    CodexAppServerSession,
)
from src.models import ReviewConfig, Session, SessionStatus


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def config():
    return CodexAppServerConfig(request_timeout_seconds=2)


@pytest.fixture
def codex_session(config):
    """CodexAppServerSession with mocked process (no real subprocess)."""
    session = CodexAppServerSession(
        session_id="test-sess",
        working_dir="/tmp/test",
        config=config,
        on_turn_complete=AsyncMock(),
        on_turn_started=AsyncMock(),
        on_turn_delta=AsyncMock(),
        on_review_complete=AsyncMock(),
    )
    session.thread_id = "thread-abc"
    return session


# ---------------------------------------------------------------------------
# review_start() — target object construction
# ---------------------------------------------------------------------------

class TestReviewStartTargetBuilding:
    """Verify review_start builds correct target objects per mode."""

    @pytest.mark.asyncio
    async def test_branch_mode_target(self, codex_session):
        codex_session._request = AsyncMock(return_value={})

        await codex_session.review_start(mode="branch", base_branch="main")

        call_args = codex_session._request.call_args
        params = call_args[0][1]
        assert params["target"] == {"type": "baseBranch", "branch": "main"}
        assert params["threadId"] == "thread-abc"
        assert params["delivery"] == "inline"

    @pytest.mark.asyncio
    async def test_branch_mode_defaults_to_main(self, codex_session):
        codex_session._request = AsyncMock(return_value={})

        await codex_session.review_start(mode="branch")

        params = codex_session._request.call_args[0][1]
        assert params["target"]["branch"] == "main"

    @pytest.mark.asyncio
    async def test_uncommitted_mode_target(self, codex_session):
        codex_session._request = AsyncMock(return_value={})

        await codex_session.review_start(mode="uncommitted")

        params = codex_session._request.call_args[0][1]
        assert params["target"] == {"type": "uncommittedChanges"}

    @pytest.mark.asyncio
    async def test_commit_mode_target(self, codex_session):
        codex_session._request = AsyncMock(return_value={})

        await codex_session.review_start(mode="commit", commit_sha="abc123")

        params = codex_session._request.call_args[0][1]
        assert params["target"] == {"type": "commit", "sha": "abc123"}

    @pytest.mark.asyncio
    async def test_commit_mode_requires_sha(self, codex_session):
        codex_session._request = AsyncMock(return_value={})

        with pytest.raises(CodexAppServerError, match="commit_sha required"):
            await codex_session.review_start(mode="commit")

    @pytest.mark.asyncio
    async def test_custom_mode_target(self, codex_session):
        codex_session._request = AsyncMock(return_value={})

        await codex_session.review_start(mode="custom")

        params = codex_session._request.call_args[0][1]
        assert params["target"] == {"type": "custom"}

    @pytest.mark.asyncio
    async def test_unknown_mode_raises(self, codex_session):
        codex_session._request = AsyncMock(return_value={})

        with pytest.raises(CodexAppServerError, match="Unknown review mode"):
            await codex_session.review_start(mode="invalid")

    @pytest.mark.asyncio
    async def test_detached_delivery(self, codex_session):
        codex_session._request = AsyncMock(return_value={})

        await codex_session.review_start(mode="uncommitted", delivery="detached")

        params = codex_session._request.call_args[0][1]
        assert params["delivery"] == "detached"

    @pytest.mark.asyncio
    async def test_no_thread_raises(self, codex_session):
        codex_session.thread_id = None

        with pytest.raises(CodexAppServerError, match="thread not initialized"):
            await codex_session.review_start(mode="branch")

    @pytest.mark.asyncio
    async def test_review_start_sends_rpc(self, codex_session):
        codex_session._request = AsyncMock(return_value={"ok": True})

        result = await codex_session.review_start(mode="branch", base_branch="dev")

        codex_session._request.assert_called_once()
        method = codex_session._request.call_args[0][0]
        assert method == "review/start"
        assert result == {"ok": True}

    @pytest.mark.asyncio
    async def test_review_in_progress_flag_set(self, codex_session):
        codex_session._request = AsyncMock(return_value={})

        assert not codex_session._review_in_progress
        await codex_session.review_start(mode="uncommitted")
        assert codex_session._review_in_progress


# ---------------------------------------------------------------------------
# Review lifecycle notifications
# ---------------------------------------------------------------------------

class TestReviewLifecycleNotifications:
    """Test handling of enteredReviewMode / exitedReviewMode notifications."""

    @pytest.mark.asyncio
    async def test_entered_review_mode(self, codex_session):
        message = {
            "method": "item/started",
            "params": {
                "item": {
                    "type": "enteredReviewMode",
                    "id": "review-1",
                    "review": "current changes",
                }
            },
        }
        await codex_session._handle_notification(message)

        assert codex_session._review_in_progress is True
        assert codex_session._review_id == "review-1"

    @pytest.mark.asyncio
    async def test_exited_review_mode(self, codex_session):
        # Set up as if review was in progress
        codex_session._review_in_progress = True
        codex_session._review_id = "review-1"

        review_text = "Found 2 issues:\n1. Missing null check\n2. Unused import"
        message = {
            "method": "item/completed",
            "params": {
                "item": {
                    "type": "exitedReviewMode",
                    "id": "review-1",
                    "review": review_text,
                }
            },
        }
        await codex_session._handle_notification(message)

        assert codex_session._review_in_progress is False
        assert codex_session._review_id is None
        codex_session.on_review_complete.assert_awaited_once_with("test-sess", review_text)

    @pytest.mark.asyncio
    async def test_exited_review_no_callback(self, config):
        """No crash when on_review_complete is None."""
        session = CodexAppServerSession(
            session_id="test",
            working_dir="/tmp",
            config=config,
            on_turn_complete=AsyncMock(),
        )
        session.thread_id = "t"
        session._review_in_progress = True

        message = {
            "method": "item/completed",
            "params": {
                "item": {"type": "exitedReviewMode", "review": "text"}
            },
        }
        await session._handle_notification(message)

        assert not session._review_in_progress

    @pytest.mark.asyncio
    async def test_non_review_item_started_ignored(self, codex_session):
        """item/started with non-review type does not set review state."""
        message = {
            "method": "item/started",
            "params": {
                "item": {"type": "someOtherType", "id": "x"}
            },
        }
        await codex_session._handle_notification(message)

        assert not codex_session._review_in_progress

    @pytest.mark.asyncio
    async def test_non_review_item_completed_ignored(self, codex_session):
        """item/completed with non-review type does not call review callback."""
        message = {
            "method": "item/completed",
            "params": {
                "item": {"type": "someOtherType", "id": "x"}
            },
        }
        await codex_session._handle_notification(message)

        codex_session.on_review_complete.assert_not_awaited()


class TestServerRequestCallbacks:
    """Ensure server-request callbacks are emitted for lifecycle tracking."""

    @pytest.mark.asyncio
    async def test_server_request_callback_response_is_forwarded(self, config):
        callback = AsyncMock()
        callback.return_value = {"decision": "accept"}
        session = CodexAppServerSession(
            session_id="test-server-request",
            working_dir="/tmp",
            config=config,
            on_turn_complete=AsyncMock(),
            on_server_request=callback,
        )
        session._send = AsyncMock()

        message = {
            "jsonrpc": "2.0",
            "id": 42,
            "method": "item/commandExecution/requestApproval",
            "params": {"turnId": "turn-1"},
        }
        await session._handle_server_request(message)

        callback.assert_awaited_once_with(
            "test-server-request",
            42,
            "item/commandExecution/requestApproval",
            {"turnId": "turn-1"},
        )
        session._send.assert_awaited_once_with(
            {"jsonrpc": "2.0", "id": 42, "result": {"decision": "accept"}}
        )

    @pytest.mark.asyncio
    async def test_server_request_without_callback_response_returns_error(self, config):
        session = CodexAppServerSession(
            session_id="test-server-request-error",
            working_dir="/tmp",
            config=config,
            on_turn_complete=AsyncMock(),
        )
        session._send = AsyncMock()

        message = {
            "jsonrpc": "2.0",
            "id": 77,
            "method": "item/tool/requestUserInput",
            "params": {"turnId": "turn-2"},
        }
        await session._handle_server_request(message)
        sent = session._send.call_args.args[0]
        assert sent["id"] == 77
        assert sent["error"]["code"] == -32601


# ---------------------------------------------------------------------------
# SessionManager.start_review() — codex-app wiring
# ---------------------------------------------------------------------------

class TestSessionManagerCodexAppReview:
    """Test start_review() codex-app branch in SessionManager."""

    @pytest.fixture
    def codex_app_session(self):
        return Session(
            id="app123",
            name="codex-app-app123",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.IDLE,
            created_at=datetime(2024, 1, 15, 10, 0, 0),
            last_activity=datetime(2024, 1, 15, 11, 0, 0),
            codex_thread_id="thread-xyz",
        )

    @pytest.fixture
    def session_manager(self, codex_app_session):
        from src.session_manager import SessionManager
        import tempfile

        with tempfile.TemporaryDirectory() as tmpdir:
            mgr = SessionManager(log_dir=tmpdir, state_file=f"{tmpdir}/state.json")
            mgr.tmux = MagicMock()
            mgr.sessions[codex_app_session.id] = codex_app_session

            # Mock _ensure_codex_session to return a mock CodexAppServerSession
            mock_codex = AsyncMock()
            mock_codex.review_start = AsyncMock(return_value={})
            mgr._ensure_codex_session = AsyncMock(return_value=mock_codex)
            mgr.codex_sessions[codex_app_session.id] = mock_codex

            yield mgr

    @pytest.mark.asyncio
    async def test_codex_app_review_calls_rpc(self, session_manager, codex_app_session):
        result = await session_manager.start_review(
            session_id=codex_app_session.id,
            mode="branch",
            base_branch="main",
        )

        assert result["status"] == "started"
        assert result["review_mode"] == "branch"
        assert result["session_id"] == codex_app_session.id

        mock_codex = session_manager.codex_sessions[codex_app_session.id]
        mock_codex.review_start.assert_awaited_once_with(
            mode="branch",
            base_branch="main",
            commit_sha=None,
            custom_prompt=None,
        )

    @pytest.mark.asyncio
    async def test_codex_app_review_stores_config(self, session_manager, codex_app_session):
        await session_manager.start_review(
            session_id=codex_app_session.id,
            mode="uncommitted",
        )

        assert codex_app_session.review_config is not None
        assert codex_app_session.review_config.mode == "uncommitted"

    @pytest.mark.asyncio
    async def test_codex_app_review_sets_running(self, session_manager, codex_app_session):
        await session_manager.start_review(
            session_id=codex_app_session.id,
            mode="branch",
            base_branch="main",
        )

        assert codex_app_session.status == SessionStatus.RUNNING

    @pytest.mark.asyncio
    async def test_codex_app_review_rpc_failure(self, session_manager, codex_app_session):
        mock_codex = session_manager.codex_sessions[codex_app_session.id]
        mock_codex.review_start = AsyncMock(
            side_effect=CodexAppServerError("connection lost")
        )

        result = await session_manager.start_review(
            session_id=codex_app_session.id,
            mode="branch",
            base_branch="main",
        )

        assert "error" in result
        assert "review/start RPC failed" in result["error"]

    @pytest.mark.asyncio
    async def test_codex_app_review_no_server(self, session_manager, codex_app_session):
        session_manager._ensure_codex_session = AsyncMock(return_value=None)

        result = await session_manager.start_review(
            session_id=codex_app_session.id,
            mode="branch",
        )

        assert "error" in result
        assert "Failed to connect" in result["error"]

    @pytest.mark.asyncio
    async def test_codex_app_review_rejects_claude_provider(self, session_manager):
        claude_session = Session(
            id="claude1",
            name="claude-claude1",
            working_dir="/tmp/test",
            tmux_session="claude-claude1",
            provider="claude",
            status=SessionStatus.IDLE,
        )
        session_manager.sessions["claude1"] = claude_session

        result = await session_manager.start_review(
            session_id="claude1",
            mode="branch",
        )

        assert "error" in result
        assert "Codex session" in result["error"]

    @pytest.mark.asyncio
    async def test_codex_app_review_steer_not_queued(self, session_manager, codex_app_session):
        result = await session_manager.start_review(
            session_id=codex_app_session.id,
            mode="branch",
            base_branch="main",
            steer_text="focus on security",
        )

        assert result["steer_queued"] is False


# ---------------------------------------------------------------------------
# _handle_codex_review_complete
# ---------------------------------------------------------------------------

class TestHandleCodexReviewComplete:
    """Test the review complete handler in SessionManager."""

    @pytest.fixture
    def session_manager_with_session(self):
        from src.session_manager import SessionManager
        import tempfile

        with tempfile.TemporaryDirectory() as tmpdir:
            mgr = SessionManager(log_dir=tmpdir, state_file=f"{tmpdir}/state.json")
            mgr.tmux = MagicMock()

            session = Session(
                id="rev123",
                name="codex-app-rev123",
                working_dir="/tmp/test",
                provider="codex-app",
                status=SessionStatus.RUNNING,
                review_config=ReviewConfig(mode="branch", base_branch="main"),
            )
            mgr.sessions["rev123"] = session
            mgr.hook_output_store = {}

            yield mgr, session

    @pytest.mark.asyncio
    async def test_review_complete_sets_idle(self, session_manager_with_session):
        mgr, session = session_manager_with_session

        await mgr._handle_codex_review_complete("rev123", "Review findings here")

        assert session.status == SessionStatus.IDLE

    @pytest.mark.asyncio
    async def test_review_complete_stores_output(self, session_manager_with_session):
        mgr, session = session_manager_with_session

        await mgr._handle_codex_review_complete("rev123", "Review findings here")

        assert mgr.hook_output_store["rev123"] == "Review findings here"
        assert mgr.hook_output_store["latest"] == "Review findings here"

    @pytest.mark.asyncio
    async def test_review_complete_unknown_session(self, session_manager_with_session):
        mgr, _ = session_manager_with_session

        # Should not raise
        await mgr._handle_codex_review_complete("nonexistent", "text")
