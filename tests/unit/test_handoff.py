"""Unit tests for sm handoff feature (#196)."""

import asyncio
import pytest
import tempfile
from pathlib import Path
from unittest.mock import MagicMock, AsyncMock, patch

from fastapi.testclient import TestClient

from src.models import Session, SessionDeliveryState, SessionStatus
from src.server import create_app
from src.message_queue import MessageQueueManager


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def handoff_doc(tmp_path):
    """Create a real handoff document file."""
    doc = tmp_path / "handoff.md"
    doc.write_text("# Handoff\nContinue from here.")
    return str(doc)


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager with a claude session."""
    mock = MagicMock()
    session = Session(
        id="abc12345",
        name="claude-abc12345",
        working_dir="/tmp/test",
        tmux_session="claude-abc12345",
        provider="claude",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
    )
    mock.sessions = {"abc12345": session}
    mock.get_session = MagicMock(return_value=session)
    mock.message_queue_manager = None  # Will be set in tests that need it
    mock._save_state = MagicMock()
    return mock


@pytest.fixture
def app(mock_session_manager, tmp_path):
    """Create a test FastAPI app."""
    return create_app(session_manager=mock_session_manager)


@pytest.fixture
def client(app):
    """Create a TestClient."""
    return TestClient(app)


@pytest.fixture
def message_queue(mock_session_manager, tmp_path):
    """Create a MessageQueueManager with mocked dependencies."""
    db_path = str(tmp_path / "test_mq.db")
    mq = MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=db_path,
        config={
            "sm_send": {"input_poll_interval": 1, "input_stale_timeout": 30,
                        "max_batch_size": 10, "urgent_delay_ms": 100},
            "timeouts": {"message_queue": {"subprocess_timeout_seconds": 1,
                                           "async_send_timeout_seconds": 2}},
        },
        notifier=None,
    )
    mock_session_manager.message_queue_manager = mq
    return mq


# ---------------------------------------------------------------------------
# Test 1: File validation — CLI rejects nonexistent file
# ---------------------------------------------------------------------------

class TestCmdHandoff:
    """Tests for cmd_handoff in commands.py."""

    def test_missing_session_id_returns_2(self, tmp_path):
        from src.cli.commands import cmd_handoff
        from src.cli.client import SessionManagerClient
        client = MagicMock(spec=SessionManagerClient)
        doc = tmp_path / "h.md"
        doc.touch()
        result = cmd_handoff(client, None, str(doc))
        assert result == 2

    def test_nonexistent_file_returns_1(self, tmp_path):
        from src.cli.commands import cmd_handoff
        from src.cli.client import SessionManagerClient
        client = MagicMock(spec=SessionManagerClient)
        result = cmd_handoff(client, "abc12345", str(tmp_path / "nonexistent.md"))
        assert result == 1

    def test_server_unavailable_returns_2(self, handoff_doc):
        from src.cli.commands import cmd_handoff
        client = MagicMock()
        client.schedule_handoff.return_value = None
        result = cmd_handoff(client, "abc12345", handoff_doc)
        assert result == 2

    def test_server_error_returns_1(self, handoff_doc):
        from src.cli.commands import cmd_handoff
        client = MagicMock()
        client.schedule_handoff.return_value = {"error": "session not found"}
        result = cmd_handoff(client, "abc12345", handoff_doc)
        assert result == 1

    def test_success_returns_0(self, handoff_doc, capsys):
        from src.cli.commands import cmd_handoff
        client = MagicMock()
        client.schedule_handoff.return_value = {"status": "scheduled"}
        result = cmd_handoff(client, "abc12345", handoff_doc)
        assert result == 0
        captured = capsys.readouterr()
        assert "scheduled" in captured.out


# ---------------------------------------------------------------------------
# Test 2: Self-only auth — server rejects when requester != session
# ---------------------------------------------------------------------------

class TestHandoffEndpoint:
    """Tests for POST /sessions/{session_id}/handoff."""

    def setup_method(self):
        """Ensure message_queue_manager is wired per test."""

    def _make_client(self, mock_session_manager, tmp_path):
        mq = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=str(tmp_path / "mq.db"),
            config={},
            notifier=None,
        )
        mock_session_manager.message_queue_manager = mq
        app = create_app(session_manager=mock_session_manager)
        return TestClient(app), mq

    def test_self_auth_accepted(self, mock_session_manager, tmp_path, handoff_doc):
        client, _ = self._make_client(mock_session_manager, tmp_path)
        resp = client.post(
            "/sessions/abc12345/handoff",
            json={"requester_session_id": "abc12345", "file_path": handoff_doc},
        )
        assert resp.status_code == 200
        assert resp.json().get("status") == "scheduled"

    def test_wrong_requester_rejected(self, mock_session_manager, tmp_path, handoff_doc):
        client, _ = self._make_client(mock_session_manager, tmp_path)
        resp = client.post(
            "/sessions/abc12345/handoff",
            json={"requester_session_id": "other999", "file_path": handoff_doc},
        )
        assert resp.status_code == 200
        assert "error" in resp.json()
        assert "self-directed" in resp.json()["error"]

    def test_codex_app_rejected(self, tmp_path, handoff_doc):
        """Codex-app sessions have no tmux, so handoff must be rejected."""
        mock_sm = MagicMock()
        codex_session = Session(
            id="codex001",
            name="codex-app-codex001",
            working_dir="/tmp",
            tmux_session="",
            provider="codex-app",
            log_file="/tmp/c.log",
        )
        mock_sm.get_session.return_value = codex_session
        mq = MessageQueueManager(
            session_manager=mock_sm,
            db_path=str(tmp_path / "mq.db"),
            config={},
            notifier=None,
        )
        mock_sm.message_queue_manager = mq
        app = create_app(session_manager=mock_sm)
        client = TestClient(app)
        resp = client.post(
            "/sessions/codex001/handoff",
            json={"requester_session_id": "codex001", "file_path": handoff_doc},
        )
        assert resp.status_code == 200
        assert "error" in resp.json()
        assert "not supported" in resp.json()["error"]

    def test_unknown_session_rejected(self, tmp_path, handoff_doc):
        mock_sm = MagicMock()
        mock_sm.get_session.return_value = None
        mq = MessageQueueManager(
            session_manager=mock_sm,
            db_path=str(tmp_path / "mq.db"),
            config={},
            notifier=None,
        )
        mock_sm.message_queue_manager = mq
        app = create_app(session_manager=mock_sm)
        client = TestClient(app)
        resp = client.post(
            "/sessions/ghost123/handoff",
            json={"requester_session_id": "ghost123", "file_path": handoff_doc},
        )
        assert resp.status_code == 200
        assert "error" in resp.json()

    def test_pending_handoff_path_stored(self, mock_session_manager, tmp_path, handoff_doc):
        client, mq = self._make_client(mock_session_manager, tmp_path)
        client.post(
            "/sessions/abc12345/handoff",
            json={"requester_session_id": "abc12345", "file_path": handoff_doc},
        )
        state = mq.delivery_states.get("abc12345")
        assert state is not None
        assert state.pending_handoff_path == handoff_doc


# ---------------------------------------------------------------------------
# Test 3: Skip fence — handoff triggered in mark_session_idle
# ---------------------------------------------------------------------------

class TestMarkSessionIdle:
    """Tests for handoff logic inside mark_session_idle."""

    def test_handoff_triggered_on_stop_hook(self, message_queue, handoff_doc):
        """Handoff is triggered when from_stop_hook=True and path is set."""
        state = message_queue._get_or_create_state("abc12345")
        state.pending_handoff_path = handoff_doc

        with patch("asyncio.create_task") as mock_create_task:
            message_queue.mark_session_idle("abc12345", from_stop_hook=True)

        # is_idle should be False (handoff in progress)
        assert not state.is_idle
        # pending path should be cleared
        assert state.pending_handoff_path is None
        # create_task should have been called (for _execute_handoff)
        assert mock_create_task.called

    def test_handoff_not_triggered_without_stop_hook(self, message_queue, handoff_doc):
        """Handoff is NOT triggered when from_stop_hook=False."""
        state = message_queue._get_or_create_state("abc12345")
        state.pending_handoff_path = handoff_doc

        with patch("asyncio.create_task") as mock_create_task:
            message_queue.mark_session_idle("abc12345", from_stop_hook=False)

        # Path should still be set (not consumed)
        assert state.pending_handoff_path == handoff_doc
        # Session should be idle (normal path taken)
        assert state.is_idle

    def test_handoff_skips_delivery(self, message_queue, handoff_doc):
        """When handoff is pending, queued message delivery is skipped."""
        state = message_queue._get_or_create_state("abc12345")
        state.pending_handoff_path = handoff_doc

        deliver_calls = []
        with patch.object(message_queue, "_try_deliver_messages") as mock_deliver:
            with patch("asyncio.create_task") as mock_create_task:
                # Capture what coros are scheduled
                def capture(coro):
                    deliver_calls.append(getattr(coro, "__name__", str(coro)))
                    coro.close()
                    return MagicMock()
                mock_create_task.side_effect = capture
                message_queue.mark_session_idle("abc12345", from_stop_hook=True)

        # _try_deliver_messages should NOT have been called directly
        mock_deliver.assert_not_called()

# ---------------------------------------------------------------------------
# Test 4: Failure recovery — _execute_handoff restores idle on error
# ---------------------------------------------------------------------------

class TestExecuteHandoff:
    """Tests for _execute_handoff async method."""

    @pytest.mark.asyncio
    async def test_missing_file_restores_idle(self, message_queue, tmp_path):
        """If handoff file is deleted between schedule and execution, idle is restored."""
        gone_path = str(tmp_path / "gone.md")  # never created

        restore_called = []

        async def run():
            # Set is_idle=False as mark_session_idle does
            state = message_queue._get_or_create_state("abc12345")
            state.is_idle = False

            with patch.object(message_queue, "_try_deliver_messages", new_callable=AsyncMock) as mock_deliver:
                with patch("asyncio.create_task") as mock_task:
                    def capture(coro):
                        restore_called.append(coro.__name__ if hasattr(coro, "__name__") else "unknown")
                        coro.close()
                        return MagicMock()
                    mock_task.side_effect = capture
                    await message_queue._execute_handoff("abc12345", gone_path)

            # After failure, idle should be restored
            assert state.is_idle

        await run()

    @pytest.mark.asyncio
    async def test_missing_session_restores_idle(self, message_queue):
        """If session is not found, idle is restored."""
        message_queue.session_manager.sessions = {}

        state = message_queue._get_or_create_state("ghost123")
        state.is_idle = False

        with patch("asyncio.create_task") as mock_task:
            mock_task.side_effect = lambda coro: (coro.close(), MagicMock())[1]
            await message_queue._execute_handoff("ghost123", "/tmp/any.md")

        assert state.is_idle

    @pytest.mark.asyncio
    async def test_skip_count_incremented_on_success(self, message_queue, handoff_doc):
        """_execute_handoff increments skip_count to absorb the /clear Stop hook."""
        state = message_queue._get_or_create_state("abc12345")
        state.is_idle = False
        initial_skip = state.stop_notify_skip_count

        # Mock all subprocess calls
        with patch("asyncio.create_subprocess_exec", new_callable=AsyncMock) as mock_exec:
            mock_proc = AsyncMock()
            mock_proc.communicate.return_value = (b"", b"")
            mock_exec.return_value = mock_proc

            with patch.object(message_queue, "_wait_for_claude_prompt_async", new_callable=AsyncMock):
                with patch("asyncio.create_task") as mock_task:
                    mock_task.side_effect = lambda coro: (coro.close(), MagicMock())[1]
                    with patch("asyncio.wait_for", new_callable=AsyncMock) as mock_wait:
                        mock_wait.return_value = (b"", b"")
                        await message_queue._execute_handoff("abc12345", handoff_doc)

        assert state.stop_notify_skip_count == initial_skip + 1

    @pytest.mark.asyncio
    async def test_delivery_lock_acquired(self, message_queue, handoff_doc):
        """_execute_handoff holds the delivery lock during execution."""
        state = message_queue._get_or_create_state("abc12345")
        state.is_idle = False

        lock_was_locked_during = []

        with patch("asyncio.create_subprocess_exec", new_callable=AsyncMock) as mock_exec:
            mock_proc = AsyncMock()
            mock_proc.communicate.return_value = (b"", b"")
            mock_exec.return_value = mock_proc

            original_wait_for = asyncio.wait_for

            async def track_lock(*args, **kwargs):
                lock = message_queue._delivery_locks.get("abc12345")
                if lock:
                    lock_was_locked_during.append(lock.locked())
                return (b"", b"")

            with patch("asyncio.wait_for", side_effect=track_lock):
                with patch.object(message_queue, "_wait_for_claude_prompt_async", new_callable=AsyncMock):
                    with patch("asyncio.create_task") as mock_task:
                        mock_task.side_effect = lambda coro: (coro.close(), MagicMock())[1]
                        await message_queue._execute_handoff("abc12345", handoff_doc)

        # Lock should have been acquired (True) during execution
        assert any(lock_was_locked_during)


# ---------------------------------------------------------------------------
# Test 5: Model field
# ---------------------------------------------------------------------------

class TestModelField:
    def test_pending_handoff_path_default_none(self):
        state = SessionDeliveryState(session_id="test")
        assert state.pending_handoff_path is None

    def test_pending_handoff_path_assignable(self):
        state = SessionDeliveryState(session_id="test")
        state.pending_handoff_path = "/tmp/doc.md"
        assert state.pending_handoff_path == "/tmp/doc.md"
