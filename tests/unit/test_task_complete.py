"""Unit tests for sm#269: sm task-complete command."""

import pytest
from unittest.mock import MagicMock, patch

from fastapi.testclient import TestClient

from src.models import Session, SessionStatus
from src.server import create_app
from src.message_queue import MessageQueueManager


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

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
        friendly_name="worker-1",
    )
    mock.sessions = {"abc12345": session}
    mock.get_session = MagicMock(return_value=session)
    mock.message_queue_manager = None
    mock._save_state = MagicMock()
    return mock


@pytest.fixture
def mq(mock_session_manager, tmp_path):
    """Create a real MessageQueueManager for the mock session manager."""
    db_path = str(tmp_path / "test_mq.db")
    queue_mgr = MessageQueueManager(
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
    mock_session_manager.message_queue_manager = queue_mgr
    return queue_mgr


@pytest.fixture
def app_client(mock_session_manager, mq):
    """Create a TestClient wired with session manager + message queue."""
    application = create_app(session_manager=mock_session_manager)
    return TestClient(application), mq


# ---------------------------------------------------------------------------
# 1. cancel_remind is called after task-complete
# ---------------------------------------------------------------------------

class TestTaskCompleteCancelsRemind:
    def test_task_complete_cancels_remind(self, app_client):
        client, mq = app_client

        # Register a remind for the session (patch create_task — no event loop in sync test)
        with patch("asyncio.create_task") as mock_ct:
            mock_ct.return_value = MagicMock()
            mq.register_periodic_remind("abc12345", soft_threshold=210, hard_threshold=420)
        assert "abc12345" in mq._remind_registrations

        resp = client.post(
            "/sessions/abc12345/task-complete",
            json={"requester_session_id": "abc12345"},
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "completed"

        # remind should be cancelled
        assert "abc12345" not in mq._remind_registrations


# ---------------------------------------------------------------------------
# 2. cancel_parent_wake is called after task-complete
# ---------------------------------------------------------------------------

class TestTaskCompleteCancelsParentWake:
    def test_task_complete_cancels_parent_wake(self, app_client):
        client, mq = app_client

        # Register a parent wake for the session
        with patch("asyncio.create_task") as mock_ct:
            mock_ct.return_value = MagicMock()
            mq.register_parent_wake("abc12345", "em000001")
        assert "abc12345" in mq._parent_wake_registrations

        resp = client.post(
            "/sessions/abc12345/task-complete",
            json={"requester_session_id": "abc12345"},
        )
        assert resp.status_code == 200
        assert resp.json()["status"] == "completed"

        # parent wake should be cancelled
        assert "abc12345" not in mq._parent_wake_registrations


# ---------------------------------------------------------------------------
# 3. EM notified via parent_wake_registrations
# ---------------------------------------------------------------------------

class TestTaskCompleteNotifiesEmViaParentWake:
    def test_task_complete_notifies_em_via_parent_wake(self, app_client):
        client, mq = app_client

        # Insert an active parent_wake_registration in the DB to simulate dispatch
        import uuid
        from datetime import datetime
        reg_id = uuid.uuid4().hex[:12]
        mq._execute("""
            INSERT INTO parent_wake_registrations
            (id, child_session_id, parent_session_id, period_seconds, registered_at, is_active)
            VALUES (?, ?, ?, ?, ?, 1)
        """, (reg_id, "abc12345", "em000001", 600, datetime.now().isoformat()))

        resp = client.post(
            "/sessions/abc12345/task-complete",
            json={"requester_session_id": "abc12345"},
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["em_notified"] is True

        # EM should have received a queued message
        msgs = mq.get_pending_messages("em000001")
        assert any("[sm task-complete]" in m.text for m in msgs)
        assert any("abc12345" in m.text for m in msgs)


# ---------------------------------------------------------------------------
# 4. Falls back to session.parent_session_id when no parent_wake_registration
# ---------------------------------------------------------------------------

class TestTaskCompleteFallsBackToSessionParent:
    def test_task_complete_falls_back_to_session_parent(self, mock_session_manager, tmp_path):
        # Session has parent_session_id set
        session = Session(
            id="child001",
            name="claude-child001",
            working_dir="/tmp",
            tmux_session="claude-child001",
            provider="claude",
            log_file="/tmp/c.log",
            status=SessionStatus.RUNNING,
            parent_session_id="em000001",
            friendly_name="child-worker",
        )
        mock_session_manager.get_session.return_value = session
        mock_session_manager.sessions = {"child001": session}

        db_path = str(tmp_path / "test_fallback.db")
        queue_mgr = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=db_path,
            config={},
            notifier=None,
        )
        mock_session_manager.message_queue_manager = queue_mgr

        application = create_app(session_manager=mock_session_manager)
        http_client = TestClient(application)

        resp = http_client.post(
            "/sessions/child001/task-complete",
            json={"requester_session_id": "child001"},
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["em_notified"] is True

        msgs = queue_mgr.get_pending_messages("em000001")
        assert any("[sm task-complete]" in m.text for m in msgs)
        assert any("child001" in m.text for m in msgs)


# ---------------------------------------------------------------------------
# 5. No EM found — endpoint returns success with em_notified=false, no crash
# ---------------------------------------------------------------------------

class TestTaskCompleteNoEm:
    def test_task_complete_no_em_no_error(self, mock_session_manager, tmp_path):
        # Session has no parent_session_id and no parent_wake_registration
        session = Session(
            id="lone001",
            name="claude-lone001",
            working_dir="/tmp",
            tmux_session="claude-lone001",
            provider="claude",
            log_file="/tmp/l.log",
            status=SessionStatus.RUNNING,
            parent_session_id=None,
        )
        mock_session_manager.get_session.return_value = session
        mock_session_manager.sessions = {"lone001": session}

        db_path = str(tmp_path / "test_noem.db")
        queue_mgr = MessageQueueManager(
            session_manager=mock_session_manager,
            db_path=db_path,
            config={},
            notifier=None,
        )
        mock_session_manager.message_queue_manager = queue_mgr

        application = create_app(session_manager=mock_session_manager)
        http_client = TestClient(application)

        resp = http_client.post(
            "/sessions/lone001/task-complete",
            json={"requester_session_id": "lone001"},
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "completed"
        assert data["em_notified"] is False


# ---------------------------------------------------------------------------
# 6. Self-auth enforced — wrong requester is rejected
# ---------------------------------------------------------------------------

class TestTaskCompleteSelfAuth:
    def test_self_auth_enforced(self, app_client):
        client, _ = app_client

        resp = client.post(
            "/sessions/abc12345/task-complete",
            json={"requester_session_id": "other999"},
        )
        assert resp.status_code == 200
        data = resp.json()
        assert "error" in data
        assert "self-directed" in data["error"]


# ---------------------------------------------------------------------------
# 7. Remind message includes task-complete hint
# ---------------------------------------------------------------------------

class TestRemindMessageIncludesTaskCompleteHint:
    def test_soft_remind_message_contains_task_complete(self, mq):
        """The soft remind message contains 'sm task-complete'."""
        # Queue a message and check text directly by triggering the remind queue
        # We verify the constant text used in _run_remind_task
        import inspect
        import src.message_queue as mq_module
        source = inspect.getsource(mq_module.MessageQueueManager._run_remind_task)
        assert "sm task-complete" in source

    def test_hard_remind_message_contains_task_complete(self, mq):
        """The hard remind message contains 'sm task-complete'."""
        import inspect
        import src.message_queue as mq_module
        source = inspect.getsource(mq_module.MessageQueueManager._run_remind_task)
        # Both soft and hard messages should have the hint
        assert source.count("sm task-complete") >= 2


# ---------------------------------------------------------------------------
# 8. CLI: cmd_task_complete requires CLAUDE_SESSION_MANAGER_ID
# ---------------------------------------------------------------------------

class TestCliTaskCompleteRequiresSessionId:
    def test_error_when_session_id_not_set(self, capsys):
        from src.cli.commands import cmd_task_complete
        client = MagicMock()
        result = cmd_task_complete(client, None)
        assert result == 2
        captured = capsys.readouterr()
        assert "CLAUDE_SESSION_MANAGER_ID" in captured.err

    def test_success_with_em_notified(self, capsys):
        from src.cli.commands import cmd_task_complete
        client = MagicMock()
        client.task_complete.return_value = (True, False, True)
        result = cmd_task_complete(client, "abc12345")
        assert result == 0
        captured = capsys.readouterr()
        assert "EM notified" in captured.out

    def test_success_without_em(self, capsys):
        from src.cli.commands import cmd_task_complete
        client = MagicMock()
        client.task_complete.return_value = (True, False, False)
        result = cmd_task_complete(client, "abc12345")
        assert result == 0
        captured = capsys.readouterr()
        assert "No EM registered" in captured.out

    def test_unavailable_returns_2(self, capsys):
        from src.cli.commands import cmd_task_complete
        client = MagicMock()
        client.task_complete.return_value = (False, True, False)
        result = cmd_task_complete(client, "abc12345")
        assert result == 2
        captured = capsys.readouterr()
        assert "unavailable" in captured.err

    def test_server_error_body_returns_failure(self, capsys):
        from src.cli.commands import cmd_task_complete
        client = MagicMock()
        # Server returns success=False (error-body path in client.task_complete)
        client.task_complete.return_value = (False, False, False)
        result = cmd_task_complete(client, "abc12345")
        assert result == 1
        captured = capsys.readouterr()
        assert "Failed" in captured.err


# ---------------------------------------------------------------------------
# 9. client.task_complete() handles error body in HTTP 200 response
# ---------------------------------------------------------------------------

class TestClientTaskCompleteHandlesErrorBody:
    def test_error_body_returns_false_success(self):
        """When _request returns HTTP-200 success=True but body has 'error' key, task_complete returns success=False."""
        from src.cli.client import SessionManagerClient

        sm_client = SessionManagerClient.__new__(SessionManagerClient)
        # Patch _request to simulate: HTTP 200 with {"error": "Session X not found"}
        sm_client._request = MagicMock(
            return_value=({"error": "Session nonexistent not found"}, True, False)
        )

        success, unavailable, em_notified = sm_client.task_complete("nonexistent")

        assert success is False
        assert unavailable is False
        assert em_notified is False

    def test_success_body_returns_true(self):
        """When _request returns HTTP-200 success=True with proper body, task_complete returns success=True."""
        from src.cli.client import SessionManagerClient

        sm_client = SessionManagerClient.__new__(SessionManagerClient)
        sm_client._request = MagicMock(
            return_value=({"status": "completed", "session_id": "abc12345", "em_notified": True}, True, False)
        )

        success, unavailable, em_notified = sm_client.task_complete("abc12345")

        assert success is True
        assert unavailable is False
        assert em_notified is True
