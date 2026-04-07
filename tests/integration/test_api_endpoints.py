"""Integration tests for API endpoints - ticket #65."""

import pytest
import json
from datetime import datetime
from unittest.mock import MagicMock, AsyncMock, patch
from fastapi.testclient import TestClient

from src.server import create_app
from src.models import Session, SessionStatus, Subagent, SubagentStatus, DeliveryResult


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager for testing."""
    mock = MagicMock()
    mock.sessions = {}
    mock.tmux = MagicMock()
    mock.tmux.send_input_async = AsyncMock(return_value=True)
    mock.tmux.list_sessions = MagicMock(return_value=[])
    mock.message_queue_manager = None
    mock.is_codex_rollout_enabled = MagicMock(return_value=True)
    mock.validate_friendly_name_update = MagicMock(return_value=None)
    mock._save_state = MagicMock()
    return mock


@pytest.fixture
def mock_output_monitor():
    """Create a mock OutputMonitor."""
    mock = MagicMock()
    mock.start_monitoring = AsyncMock()
    mock.cleanup_session = AsyncMock()
    mock.update_activity = MagicMock()
    mock._tasks = {}
    return mock


@pytest.fixture
def mock_email_handler():
    """Create a mock EmailHandler."""
    mock = MagicMock()
    mock.bridge_is_available.return_value = True
    mock.bridge_webhook_path.return_value = "/api/email-inbound"
    mock.bridge_worker_secret.return_value = None
    mock.bridge_worker_secret_header.return_value = "x-email-worker-secret"
    mock.bridge_session_id_header.return_value = "x-email-session-id"
    mock.normalize_explicit_session_id.side_effect = lambda value: value.strip().lower() if value else None
    mock.send_agent_email = AsyncMock(return_value={"to": [], "cc": [], "subject": "test"})
    mock.is_authorized_sender.return_value = True
    mock.extract_routed_session_id.return_value = None
    mock.extract_subject_from_raw_email.return_value = None
    mock.extract_reply_message_body.side_effect = lambda value: value
    return mock


@pytest.fixture
def test_client(mock_session_manager, mock_output_monitor, mock_email_handler):
    """Create a FastAPI TestClient with mocked dependencies."""
    app = create_app(
        session_manager=mock_session_manager,
        notifier=None,
        output_monitor=mock_output_monitor,
        email_handler=mock_email_handler,
        config={},
    )
    return TestClient(app)


@pytest.fixture
def sample_session():
    """Create a sample session for testing."""
    return Session(
        id="test123",
        name="test-session",
        working_dir="/tmp/test",
        tmux_session="claude-test123",
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
        created_at=datetime(2024, 1, 15, 10, 0, 0),
        last_activity=datetime(2024, 1, 15, 11, 0, 0),
        friendly_name="Test Session",
        current_task="Testing",
    )


class TestHealthEndpoints:
    """Tests for health check endpoints."""

    def test_root_endpoint(self, test_client):
        """GET / returns health status."""
        response = test_client.get("/")
        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "ok"
        assert data["service"] == "session-manager"

    def test_health_endpoint(self, test_client):
        """GET /health returns healthy status."""
        response = test_client.get("/health")
        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "healthy"


class TestSessionEndpoints:
    """Tests for session CRUD endpoints."""

    def test_list_sessions(self, test_client, mock_session_manager, sample_session):
        """GET /sessions returns session list."""
        mock_session_manager.list_sessions.return_value = [sample_session]
        mock_session_manager.get_activity_state.return_value = "working"
        sample_session.agent_status_text = "running tests"
        sample_session.agent_status_at = datetime(2024, 1, 15, 11, 5, 0)
        sample_session.agent_task_completed_at = datetime(2024, 1, 15, 11, 9, 0)

        response = test_client.get("/sessions")
        assert response.status_code == 200

        data = response.json()
        assert "sessions" in data
        assert len(data["sessions"]) == 1
        assert data["sessions"][0]["id"] == "test123"
        assert data["sessions"][0]["friendly_name"] == "Test Session"
        assert data["sessions"][0]["activity_state"] == "working"
        assert data["sessions"][0]["agent_status_text"] == "running tests"
        assert data["sessions"][0]["agent_status_at"] == "2024-01-15T11:05:00"

    def test_set_agent_status_notifies_telegram_when_notifier_present(
        self,
        mock_session_manager,
        mock_output_monitor,
        mock_email_handler,
        sample_session,
    ):
        notifier = AsyncMock()
        notifier.notify = AsyncMock(return_value=True)
        sample_session.telegram_chat_id = 12345
        sample_session.telegram_thread_id = 67890
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.message_queue_manager = MagicMock()

        app = create_app(
            session_manager=mock_session_manager,
            notifier=notifier,
            output_monitor=mock_output_monitor,
            email_handler=mock_email_handler,
            config={},
        )
        client = TestClient(app)

        response = client.post(
            f"/sessions/{sample_session.id}/agent-status",
            json={"text": "Investigating Telegram mirror drops"},
        )

        assert response.status_code == 200
        notifier.notify.assert_awaited_once()
        event, notified_session = notifier.notify.await_args.args
        assert event.event_type == "agent_status"
        assert event.message == "Investigating Telegram mirror drops"
        assert notified_session is sample_session
        mock_session_manager.message_queue_manager.reset_remind.assert_called_once_with(sample_session.id)

    def test_list_sessions_include_stopped(self, test_client, mock_session_manager, sample_session):
        """GET /sessions can explicitly include stopped sessions."""
        sample_session.status = SessionStatus.STOPPED
        mock_session_manager.list_sessions.return_value = [sample_session]
        mock_session_manager.get_activity_state.return_value = "stopped"

        response = test_client.get("/sessions?include_stopped=true")

        assert response.status_code == 200
        assert response.json()["sessions"][0]["status"] == "stopped"
        mock_session_manager.list_sessions.assert_called_once_with(include_stopped=True)

    def test_list_sessions_empty(self, test_client, mock_session_manager):
        """GET /sessions returns empty list when no sessions."""
        mock_session_manager.list_sessions.return_value = []

        response = test_client.get("/sessions")
        assert response.status_code == 200

        data = response.json()
        assert data["sessions"] == []

    def test_get_session(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id} returns session details."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.get_activity_state.return_value = "thinking"
        sample_session.agent_status_text = "reviewing logs"
        sample_session.agent_status_at = datetime(2024, 1, 15, 11, 7, 0)
        sample_session.agent_task_completed_at = datetime(2024, 1, 15, 11, 10, 0)

        response = test_client.get("/sessions/test123")
        assert response.status_code == 200

        data = response.json()
        assert data["id"] == "test123"
        assert data["name"] == "test-session"
        assert data["status"] == "running"
        assert data["friendly_name"] == "Test Session"
        assert data["activity_state"] == "thinking"
        assert data["agent_status_text"] == "reviewing logs"
        assert data["agent_status_at"] == "2024-01-15T11:07:00"
        assert data["agent_task_completed_at"] == "2024-01-15T11:10:00"

    def test_client_request_status_broadcasts_to_live_sessions(
        self,
        test_client,
        mock_session_manager,
        sample_session,
    ):
        second_session = Session(
            id="idle456",
            name="idle-session",
            working_dir="/tmp/idle",
            tmux_session="claude-idle456",
            log_file="/tmp/idle.log",
            status=SessionStatus.IDLE,
            created_at=datetime(2024, 1, 15, 10, 30, 0),
            last_activity=datetime(2024, 1, 15, 10, 45, 0),
        )
        mock_session_manager.list_sessions.return_value = [sample_session, second_session]
        mock_session_manager.send_input = AsyncMock(
            side_effect=[DeliveryResult.DELIVERED, DeliveryResult.QUEUED]
        )

        response = test_client.post("/client/request-status")

        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "requested"
        assert data["targeted_count"] == 2
        assert data["delivered_count"] == 1
        assert data["queued_count"] == 1
        assert data["failed_count"] == 0
        assert data["targeted_session_ids"] == ["test123", "idle456"]
        mock_session_manager.list_sessions.assert_called_once_with()
        expected_prompt = "[sm] user requests status, please update now using sm status"
        mock_session_manager.send_input.assert_any_await(
            session_id="test123",
            text=expected_prompt,
            delivery_mode="important",
        )
        mock_session_manager.send_input.assert_any_await(
            session_id="idle456",
            text=expected_prompt,
            delivery_mode="important",
        )


class TestEmailBridgeEndpoints:
    """Tests for email bridge API endpoints."""

    def test_send_registered_email_endpoint(
        self,
        test_client,
        mock_session_manager,
        mock_email_handler,
        sample_session,
    ):
        mock_session_manager.get_session.return_value = sample_session
        mock_email_handler.send_agent_email = AsyncMock(
            return_value={
                "to": [{"username": "rajesh", "email": "rajesh@example.com"}],
                "cc": [],
                "subject": "Reading list",
                "message_id": "email_123",
            }
        )

        response = test_client.post(
            "/email/send",
            json={
                "requester_session_id": "test123",
                "recipients": ["rajesh"],
                "subject": "Reading list",
                "body_text": "Hello",
            },
        )

        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "sent"
        assert data["subject"] == "Reading list"
        mock_email_handler.send_agent_email.assert_awaited_once_with(
            sender_session_id="test123",
            sender_name="Test Session",
            sender_provider="claude",
            to_identifiers=["rajesh"],
            cc_identifiers=[],
            subject="Reading list",
            body_text="Hello",
            body_html=None,
            body_markdown=False,
            auto_subject=False,
        )

    def test_inbound_email_restores_stopped_session(
        self,
        test_client,
        mock_session_manager,
        mock_output_monitor,
        mock_email_handler,
        sample_session,
    ):
        stopped = sample_session
        stopped.status = SessionStatus.STOPPED
        restored = Session(
            id=stopped.id,
            name=stopped.name,
            working_dir=stopped.working_dir,
            tmux_session=stopped.tmux_session,
            log_file=stopped.log_file,
            status=SessionStatus.RUNNING,
            created_at=stopped.created_at,
            last_activity=stopped.last_activity,
            friendly_name=stopped.friendly_name,
        )
        mock_session_manager.get_session.return_value = stopped
        mock_session_manager.restore_session = AsyncMock(return_value=(True, restored, None))
        mock_session_manager.send_input = AsyncMock(return_value=DeliveryResult.DELIVERED)

        response = test_client.post(
            "/api/email-inbound",
            json={
                "session_id": "test123",
                "body": "please continue",
                "from_address": "rajesh@example.com",
            },
        )

        assert response.status_code == 200
        assert response.json()["restored"] is True
        mock_session_manager.restore_session.assert_awaited_once_with("test123")
        mock_session_manager.send_input.assert_awaited_once_with(
            "test123",
            "{sm email from rajesh@example.com}\nplease continue",
            sender_session_id=None,
            delivery_mode="sequential",
            from_sm_send=False,
        )
        mock_output_monitor.start_monitoring.assert_awaited_once_with(restored)
        mock_email_handler.extract_reply_message_body.assert_called_once_with("please continue")

    def test_inbound_email_rejects_unauthorized_sender(
        self,
        test_client,
        mock_email_handler,
    ):
        mock_email_handler.is_authorized_sender.return_value = False

        response = test_client.post(
            "/api/email-inbound",
            json={
                "session_id": "test123",
                "body": "hello",
                "from_address": "intruder@example.com",
            },
        )

        assert response.status_code == 403

    def test_inbound_email_rejects_invalid_worker_secret(
        self,
        test_client,
        mock_email_handler,
    ):
        mock_email_handler.bridge_worker_secret.return_value = "worker-secret-123"

        response = test_client.post(
            "/api/email-inbound",
            json={
                "body": "hello",
                "from_address": "rajesh@example.com",
            },
        )

        assert response.status_code == 401

    def test_inbound_email_accepts_valid_worker_secret(
        self,
        test_client,
        mock_email_handler,
        mock_session_manager,
        sample_session,
    ):
        mock_email_handler.bridge_worker_secret.return_value = "worker-secret-123"
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.send_input = AsyncMock(return_value=DeliveryResult.DELIVERED)

        response = test_client.post(
            "/api/email-inbound",
            headers={"x-email-worker-secret": "worker-secret-123"},
            json={
                "session_id": "test123",
                "body": "hello",
                "from_address": "rajesh@example.com",
            },
        )

        assert response.status_code == 200
        mock_session_manager.send_input.assert_awaited_once()

    def test_inbound_email_accepts_explicit_session_header_when_worker_secret_is_valid(
        self,
        test_client,
        mock_email_handler,
        mock_session_manager,
        sample_session,
    ):
        mock_email_handler.bridge_worker_secret.return_value = "worker-secret-123"
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.send_input = AsyncMock(return_value=DeliveryResult.DELIVERED)

        response = test_client.post(
            "/api/email-inbound",
            headers={
                "x-email-worker-secret": "worker-secret-123",
                "x-email-session-id": "test123",
            },
            json={
                "body": "hello from explicit route",
                "from_address": "rajesh@example.com",
            },
        )

        assert response.status_code == 200
        assert response.json()["session_id"] == "test123"
        mock_email_handler.normalize_explicit_session_id.assert_called_once_with("test123")
        mock_email_handler.extract_routed_session_id.assert_not_called()
        mock_session_manager.send_input.assert_awaited_once_with(
            "test123",
            "{sm email from rajesh@example.com}\nhello from explicit route",
            sender_session_id=None,
            delivery_mode="sequential",
            from_sm_send=False,
        )

    def test_inbound_email_ignores_explicit_session_header_without_worker_secret(
        self,
        test_client,
        mock_email_handler,
        mock_session_manager,
    ):
        mock_session_manager.send_input = AsyncMock(return_value=DeliveryResult.DELIVERED)

        response = test_client.post(
            "/api/email-inbound",
            headers={"x-email-session-id": "test123"},
            json={
                "body": "hello without routing metadata",
                "from_address": "rajesh@example.com",
            },
        )

        assert response.status_code == 200
        assert response.json()["status"] == "ignored"
        assert response.json()["reason"] == "missing_routing_footer"
        mock_email_handler.normalize_explicit_session_id.assert_not_called()
        mock_session_manager.send_input.assert_not_called()

    def test_inbound_email_honors_codex_pending_request_gate(
        self,
        test_client,
        mock_session_manager,
        mock_email_handler,
    ):
        codex_session = Session(
            id="codex123",
            name="codex-app-codex123",
            working_dir="/tmp/test",
            tmux_session="codex-app-codex123",
            provider="codex-app",
            log_file="/tmp/codex.log",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.has_pending_codex_requests.return_value = True
        mock_session_manager.oldest_pending_codex_request.return_value = {"id": "req-1"}

        response = test_client.post(
            "/api/email-inbound",
            json={
                "session_id": "codex123",
                "body": "hello",
                "from_address": "rajesh@example.com",
            },
        )

        assert response.status_code == 409
        assert response.json()["detail"]["error_code"] == "pending_structured_request"
        mock_session_manager.send_input.assert_not_called()
        mock_email_handler.extract_reply_message_body.assert_called_once_with("hello")

    def test_inbound_email_parses_session_id_from_footer(
        self,
        test_client,
        mock_session_manager,
        mock_email_handler,
        sample_session,
    ):
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.send_input = AsyncMock(return_value=DeliveryResult.DELIVERED)
        body = "\n".join(
            [
                "Please continue with the rollout.",
                "",
                "On Sun, Apr 5, 2026 at 10:00 AM maintainer wrote:",
                "> context",
                "> --",
                "> SM: maintainer test123 codex",
            ]
        )
        mock_email_handler.extract_routed_session_id.return_value = "test123"
        mock_email_handler.extract_reply_message_body.side_effect = None
        mock_email_handler.extract_reply_message_body.return_value = "Please continue with the rollout."

        response = test_client.post(
            "/api/email-inbound",
            json={
                "body": body,
                "from_address": "rajesh@example.com",
            },
        )

        assert response.status_code == 200
        assert response.json()["session_id"] == "test123"
        mock_session_manager.send_input.assert_awaited_once_with(
            "test123",
            "{sm email from rajesh@example.com}\nPlease continue with the rollout.",
            sender_session_id=None,
            delivery_mode="sequential",
            from_sm_send=False,
        )
        mock_email_handler.extract_routed_session_id.assert_called_once_with(body)

    def test_inbound_email_ignores_missing_routing_footer(
        self,
        test_client,
        mock_session_manager,
    ):
        mock_session_manager.send_input = AsyncMock(return_value=DeliveryResult.DELIVERED)

        response = test_client.post(
            "/api/email-inbound",
            json={
                "body": "hello without routing metadata",
                "from_address": "rajesh@example.com",
            },
        )

        assert response.status_code == 200
        assert response.json()["status"] == "ignored"
        assert response.json()["reason"] == "missing_routing_footer"
        mock_session_manager.send_input.assert_not_called()

    def test_inbound_email_accepts_raw_email_payload(
        self,
        test_client,
        mock_session_manager,
        mock_email_handler,
        sample_session,
    ):
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.send_input = AsyncMock(return_value=DeliveryResult.DELIVERED)
        raw_email = "Content-Type: text/plain\\n\\ninbound footer test live\\n\\n> --\\n> SM: maintainer test123 codex-fork\\n"
        mock_email_handler.extract_text_from_raw_email.return_value = (
            "inbound footer test live\\n\\n> --\\n> SM: maintainer test123 codex-fork"
        )
        mock_email_handler.extract_subject_from_raw_email.return_value = "Re: reply mailbox footer routing test 6"
        mock_email_handler.extract_routed_session_id.return_value = "test123"
        mock_email_handler.extract_reply_message_body.side_effect = None
        mock_email_handler.extract_reply_message_body.return_value = "inbound footer test live"

        response = test_client.post(
            "/api/email-inbound",
            json={
                "raw_email": raw_email,
                "from_address": "rajesh@example.com",
            },
        )

        assert response.status_code == 200
        assert response.json()["session_id"] == "test123"
        mock_email_handler.extract_text_from_raw_email.assert_called_once_with(raw_email)
        mock_email_handler.extract_subject_from_raw_email.assert_called_once_with(raw_email)
        mock_session_manager.send_input.assert_awaited_once_with(
            "test123",
            "{sm email from rajesh@example.com subj: Re: reply mailbox footer routing test 6}\ninbound footer test live",
            sender_session_id=None,
            delivery_mode="sequential",
            from_sm_send=False,
        )

    def test_get_session_not_found(self, test_client, mock_session_manager):
        """GET /sessions/{id} returns 404 for unknown session."""
        mock_session_manager.get_session.return_value = None

        response = test_client.get("/sessions/unknown")
        assert response.status_code == 404

    def test_get_codex_events_success(self, test_client, mock_session_manager):
        """GET /sessions/{id}/codex-events returns codex event page."""
        codex_session = Session(
            id="codex123",
            name="codex-app-codex123",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.get_codex_events.return_value = {
            "events": [
                {
                    "session_id": "codex123",
                    "seq": 1,
                    "timestamp": "2026-01-01T00:00:00+00:00",
                    "event_type": "turn_started",
                    "turn_id": "turn-1",
                    "payload_preview": {},
                    "persisted": True,
                }
            ],
            "earliest_seq": 1,
            "latest_seq": 1,
            "next_seq": 2,
            "history_gap": False,
            "gap_reason": None,
        }

        response = test_client.get("/sessions/codex123/codex-events?since_seq=0&limit=50")
        assert response.status_code == 200
        data = response.json()
        assert data["latest_seq"] == 1
        assert data["events"][0]["event_type"] == "turn_started"

    def test_get_codex_events_rejects_non_codex_app(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id}/codex-events rejects non-codex-app sessions."""
        mock_session_manager.get_session.return_value = sample_session
        response = test_client.get("/sessions/test123/codex-events")
        assert response.status_code == 400

    def test_get_codex_events_respects_rollout_flag(self, test_client, mock_session_manager):
        """GET /sessions/{id}/codex-events returns 503 when durable events rollout is disabled."""
        codex_session = Session(
            id="codex123",
            name="codex-app-codex123",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.is_codex_rollout_enabled.side_effect = (
            lambda key: False if key == "enable_durable_events" else True
        )

        response = test_client.get("/sessions/codex123/codex-events")
        assert response.status_code == 503

    def test_get_codex_activity_actions_success(self, test_client, mock_session_manager):
        """GET /sessions/{id}/activity-actions returns projected actions for codex-app."""
        codex_session = Session(
            id="codexproj",
            name="codex-app-codexproj",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.get_codex_activity_actions.return_value = [
            {
                "source_provider": "codex-app",
                "action_kind": "command",
                "summary_text": "Started: pytest -q",
                "status": "running",
                "started_at": "2026-02-21T00:00:00+00:00",
                "ended_at": None,
                "session_id": "codexproj",
                "turn_id": "turn-1",
                "item_id": "item-1",
            }
        ]

        response = test_client.get("/sessions/codexproj/activity-actions?limit=5")
        assert response.status_code == 200
        data = response.json()
        assert len(data["actions"]) == 1
        assert data["actions"][0]["action_kind"] == "command"

    def test_list_children_includes_codex_activity_projection(self, test_client, mock_session_manager):
        """GET /sessions/{parent}/children includes activity projection for codex-app children."""
        child = Session(
            id="childcodex1",
            name="codex-app-childcodex1",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
            parent_session_id="parent123",
        )
        mock_session_manager.list_sessions.return_value = [child]
        mock_session_manager.get_codex_latest_activity_action.return_value = {
            "summary_text": "Started: pytest -q",
            "status": "running",
            "started_at": "2026-02-21T00:00:00+00:00",
            "ended_at": None,
        }

        response = test_client.get("/sessions/parent123/children")
        assert response.status_code == 200
        data = response.json()
        assert len(data["children"]) == 1
        assert data["children"][0]["activity_projection"]["summary_text"] == "Started: pytest -q"

    def test_list_codex_pending_requests(self, test_client, mock_session_manager):
        """GET /sessions/{id}/codex-pending-requests lists pending structured requests."""
        codex_session = Session(
            id="codexpending",
            name="codex-app-codexpending",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.list_codex_pending_requests.return_value = [
            {
                "request_id": "req-123",
                "request_type": "request_approval",
                "status": "pending",
                "requested_at": "2026-02-21T00:00:00+00:00",
            }
        ]

        response = test_client.get("/sessions/codexpending/codex-pending-requests")
        assert response.status_code == 200
        data = response.json()
        assert len(data["requests"]) == 1
        assert data["requests"][0]["request_id"] == "req-123"

    def test_list_codex_pending_requests_respects_rollout_flag(self, test_client, mock_session_manager):
        """GET /sessions/{id}/codex-pending-requests returns 503 when structured-request flag is disabled."""
        codex_session = Session(
            id="codexpending",
            name="codex-app-codexpending",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.is_codex_rollout_enabled.side_effect = (
            lambda key: False if key == "enable_structured_requests" else True
        )

        response = test_client.get("/sessions/codexpending/codex-pending-requests")
        assert response.status_code == 503

    def test_respond_codex_request(self, test_client, mock_session_manager):
        """POST /sessions/{id}/codex-requests/{request_id}/respond resolves request."""
        codex_session = Session(
            id="codexresp",
            name="codex-app-codexresp",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.respond_codex_request = AsyncMock(
            return_value={
                "ok": True,
                "idempotent": False,
                "request": {
                    "request_id": "req-55",
                    "resolved_payload": {"decision": "accept"},
                    "status": "resolved",
                },
            }
        )

        response = test_client.post(
            "/sessions/codexresp/codex-requests/req-55/respond",
            json={"decision": "accept"},
        )
        assert response.status_code == 200
        assert response.json()["request"]["status"] == "resolved"

    def test_respond_codex_request_rejects_ambiguous_payload(self, test_client, mock_session_manager):
        """POST /sessions/{id}/codex-requests/{request_id}/respond requires one payload shape."""
        codex_session = Session(
            id="codexresp2",
            name="codex-app-codexresp2",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session

        response = test_client.post(
            "/sessions/codexresp2/codex-requests/req-99/respond",
            json={"decision": "accept", "answers": {"k": "v"}},
        )
        assert response.status_code == 422

    def test_create_session(self, test_client, mock_session_manager, sample_session):
        """POST /sessions creates new session."""
        mock_session_manager.create_session = AsyncMock(return_value=sample_session)

        response = test_client.post(
            "/sessions",
            json={"working_dir": "/tmp/test"}
        )
        assert response.status_code == 200

        data = response.json()
        assert data["id"] == "test123"
        assert data["working_dir"] == "/tmp/test"

    def test_create_session_with_parent_ownership(self, test_client, mock_session_manager, sample_session):
        """POST /sessions forwards parent_session_id for direct creates from managed sessions."""
        mock_session_manager.create_session = AsyncMock(return_value=sample_session)

        response = test_client.post(
            "/sessions",
            json={"working_dir": "/tmp/test", "provider": "codex-fork", "parent_session_id": "parent123"},
        )
        assert response.status_code == 200
        mock_session_manager.create_session.assert_awaited_once_with(
            working_dir="/tmp/test",
            name=None,
            provider="codex-fork",
            parent_session_id="parent123",
        )

    def test_create_session_failure(self, test_client, mock_session_manager):
        """POST /sessions returns 500 on creation failure."""
        mock_session_manager.create_session = AsyncMock(return_value=None)

        response = test_client.post(
            "/sessions",
            json={"working_dir": "/tmp/test"}
        )
        assert response.status_code == 500

    def test_kill_session(self, test_client, mock_session_manager, sample_session, mock_output_monitor):
        """DELETE /sessions/{id} kills session."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.kill_session.return_value = True

        response = test_client.delete("/sessions/test123")
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "killed"
        assert data["session_id"] == "test123"
        mock_output_monitor.cleanup_session.assert_awaited_once_with(sample_session, preserve_record=True)

    def test_restore_session(self, test_client, mock_session_manager, sample_session, mock_output_monitor):
        """POST /sessions/{id}/restore restores a stopped session."""
        stopped_session = Session(
            id=sample_session.id,
            name=sample_session.name,
            working_dir=sample_session.working_dir,
            tmux_session=sample_session.tmux_session,
            log_file=sample_session.log_file,
            status=SessionStatus.STOPPED,
            created_at=sample_session.created_at,
            last_activity=sample_session.last_activity,
            friendly_name=sample_session.friendly_name,
            current_task=sample_session.current_task,
        )
        restored_session = Session(
            id=sample_session.id,
            name=sample_session.name,
            working_dir=sample_session.working_dir,
            tmux_session=sample_session.tmux_session,
            log_file=sample_session.log_file,
            status=SessionStatus.RUNNING,
            created_at=sample_session.created_at,
            last_activity=sample_session.last_activity,
            friendly_name=sample_session.friendly_name,
            current_task=sample_session.current_task,
        )
        mock_session_manager.get_session.return_value = stopped_session
        mock_session_manager.restore_session = AsyncMock(return_value=(True, restored_session, None))
        mock_session_manager.get_activity_state.return_value = "thinking"

        response = test_client.post("/sessions/test123/restore")

        assert response.status_code == 200
        assert response.json()["id"] == "test123"
        mock_output_monitor.start_monitoring.assert_awaited_once_with(restored_session)

    def test_kill_session_not_found(self, test_client, mock_session_manager):
        """DELETE /sessions/{id} returns 404 for unknown session."""
        mock_session_manager.get_session.return_value = None

        response = test_client.delete("/sessions/unknown")
        assert response.status_code == 404

    def test_send_input(self, test_client, mock_session_manager, sample_session):
        """POST /sessions/{id}/input sends input."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.send_input = AsyncMock(return_value=DeliveryResult.DELIVERED)

        response = test_client.post(
            "/sessions/test123/input",
            json={"text": "Hello, Claude!"}
        )
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "delivered"
        assert data["session_id"] == "test123"

    def test_send_input_codex_app_with_pending_structured_request_returns_409(self, test_client, mock_session_manager):
        """POST /sessions/{id}/input is blocked for codex-app when structured requests are pending."""
        codex_session = Session(
            id="codexpend",
            name="codex-app-codexpend",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.has_pending_codex_requests.return_value = True
        mock_session_manager.oldest_pending_codex_request.return_value = {
            "request_id": "req-1",
            "request_type": "request_approval",
            "requested_at": "2026-02-21T00:00:00+00:00",
        }

        response = test_client.post("/sessions/codexpend/input", json={"text": "continue"})
        assert response.status_code == 409
        detail = response.json()["detail"]
        assert detail["error_code"] == "pending_structured_request"
        assert detail["pending_request"]["request_id"] == "req-1"

    def test_send_input_codex_app_rollout_disabled_skips_pending_gate(self, test_client, mock_session_manager):
        """POST /sessions/{id}/input does not enforce pending gate when structured-request rollout is disabled."""
        codex_session = Session(
            id="codexpend",
            name="codex-app-codexpend",
            working_dir="/tmp/test",
            provider="codex-app",
            status=SessionStatus.RUNNING,
        )
        mock_session_manager.get_session.return_value = codex_session
        mock_session_manager.send_input = AsyncMock(return_value=DeliveryResult.DELIVERED)
        mock_session_manager.has_pending_codex_requests.return_value = True
        mock_session_manager.is_codex_rollout_enabled.side_effect = (
            lambda key: False if key == "enable_structured_requests" else True
        )

        response = test_client.post("/sessions/codexpend/input", json={"text": "continue"})
        assert response.status_code == 200
        assert response.json()["status"] == "delivered"

    def test_get_rollout_flags_endpoint(self, test_client, mock_session_manager):
        """GET /admin/rollout-flags exposes current codex rollout gates."""
        mock_session_manager.is_codex_rollout_enabled.side_effect = (
            lambda key: key != "enable_codex_tui"
        )

        response = test_client.get("/admin/rollout-flags")
        assert response.status_code == 200
        data = response.json()["codex_rollout"]
        assert data["enable_durable_events"] is True
        assert data["enable_structured_requests"] is True
        assert data["enable_observability_projection"] is True
        assert data["enable_codex_tui"] is False

    def test_send_input_queued(self, test_client, mock_session_manager, sample_session):
        """POST /sessions/{id}/input returns queued status."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.send_input = AsyncMock(return_value=DeliveryResult.QUEUED)
        mock_session_manager.message_queue_manager = MagicMock()
        mock_session_manager.message_queue_manager.get_queue_length.return_value = 3

        response = test_client.post(
            "/sessions/test123/input",
            json={"text": "Hello", "delivery_mode": "sequential"}
        )
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "queued"
        assert data["queue_position"] == 3

    def test_send_input_not_found(self, test_client, mock_session_manager):
        """POST /sessions/{id}/input returns 404 for unknown session."""
        mock_session_manager.get_session.return_value = None

        response = test_client.post(
            "/sessions/unknown/input",
            json={"text": "Hello"}
        )
        assert response.status_code == 404


class TestHookEndpoints:
    """Tests for Claude Code hook endpoints."""

    def test_claude_stop_hook(self, test_client, mock_session_manager, sample_session):
        """POST /hooks/claude with Stop event marks idle."""
        mock_session_manager.get_session.return_value = sample_session
        mock_queue_manager = MagicMock()
        # Make _restore_user_input_after_response an actual async function
        mock_queue_manager._restore_user_input_after_response = AsyncMock()
        mock_session_manager.message_queue_manager = mock_queue_manager

        response = test_client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": "test123",
                "transcript_path": "/tmp/transcript.jsonl",
            }
        )
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "received"
        assert data["hook_event"] == "Stop"

        # Verify mark_session_idle was called (last_output=None when transcript not readable)
        mock_queue_manager.mark_session_idle.assert_called_with("test123", last_output=None, from_stop_hook=True)

    def test_claude_notification_hook(self, test_client):
        """POST /hooks/claude with Notification routes correctly."""
        response = test_client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Notification",
                "notification_type": "permission_prompt",
                "message": "Approve this action?",
            }
        )
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "received"
        assert data["hook_event"] == "Notification"

    def test_claude_idle_notification_filtered(self, test_client):
        """POST /hooks/claude filters idle_prompt notifications."""
        response = test_client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Notification",
                "notification_type": "idle_prompt",
                "message": "Claude is idle",
            }
        )
        assert response.status_code == 200
        # Should succeed but be filtered (no notification sent)

    def test_tool_use_hook_logs(self, test_client, mock_session_manager, sample_session):
        """POST /hooks/tool-use logs to database."""
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.post(
            "/hooks/tool-use",
            json={
                "hook_event_name": "PreToolUse",
                "tool_name": "Read",
                "tool_input": {"file_path": "/tmp/test.py"},
                "session_manager_id": "test123",
            }
        )
        assert response.status_code == 200

        data = response.json()
        assert data["status"] == "logged"


class TestSubagentEndpoints:
    """Tests for subagent management endpoints."""

    def test_spawn_subagent(self, test_client, mock_session_manager, sample_session, mock_output_monitor):
        """POST /sessions/{id}/subagents spawns child."""
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.post(
            "/sessions/test123/subagents",
            json={
                "agent_id": "agent456",
                "agent_type": "engineer",
            }
        )
        assert response.status_code == 200

        data = response.json()
        assert data["agent_id"] == "agent456"
        assert data["agent_type"] == "engineer"
        assert data["parent_session_id"] == "test123"
        assert data["status"] == "running"

    def test_list_subagents(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id}/subagents lists children."""
        subagent = Subagent(
            agent_id="agent456",
            agent_type="engineer",
            parent_session_id="test123",
            started_at=datetime(2024, 1, 15, 10, 0, 0),
            status=SubagentStatus.RUNNING,
        )
        sample_session.subagents = [subagent]
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.get("/sessions/test123/subagents")
        assert response.status_code == 200

        data = response.json()
        assert data["session_id"] == "test123"
        assert len(data["subagents"]) == 1
        assert data["subagents"][0]["agent_id"] == "agent456"

    def test_subagent_not_found(self, test_client, mock_session_manager):
        """GET /sessions/{id}/subagents returns 404 for unknown session."""
        mock_session_manager.get_session.return_value = None

        response = test_client.get("/sessions/unknown/subagents")
        assert response.status_code == 404


class TestSpawnChildSession:
    """Tests for child session spawning."""

    def test_spawn_child_session(self, test_client, mock_session_manager, sample_session, mock_output_monitor):
        """POST /sessions/spawn creates child session."""
        child_session = Session(
            id="child456",
            name="child-test12",
            working_dir="/tmp/test",
            tmux_session="claude-child456",
            log_file="/tmp/child.log",
            status=SessionStatus.RUNNING,
            parent_session_id="test123",
            spawned_at=datetime.now(),
        )

        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.spawn_child_session = AsyncMock(return_value=child_session)

        response = test_client.post(
            "/sessions/spawn",
            json={
                "parent_session_id": "test123",
                "prompt": "Test task",
            }
        )
        assert response.status_code == 200

        data = response.json()
        assert data["session_id"] == "child456"
        assert data["parent_session_id"] == "test123"


class TestUpdateSession:
    """Tests for session update endpoints."""

    def test_update_friendly_name(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} updates friendly name."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.tmux.set_status_bar.return_value = True

        response = test_client.patch(
            "/sessions/test123",
            json={"friendly_name": "new-name"}
        )
        assert response.status_code == 200

        data = response.json()
        assert data["friendly_name"] == "new-name"

    def test_update_friendly_name_rejects_empty(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} rejects empty friendly name (Issue #105)."""
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.patch(
            "/sessions/test123",
            json={"friendly_name": ""}
        )
        assert response.status_code == 400
        assert "empty" in response.json()["detail"].lower()

    def test_update_friendly_name_rejects_spaces(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} rejects names with spaces (Issue #105)."""
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.patch(
            "/sessions/test123",
            json={"friendly_name": "bad name"}
        )
        assert response.status_code == 400
        assert "alphanumeric" in response.json()["detail"].lower()

    def test_update_friendly_name_rejects_too_long(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} rejects names over 32 chars (Issue #105)."""
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.patch(
            "/sessions/test123",
            json={"friendly_name": "a" * 33}
        )
        assert response.status_code == 400
        assert "too long" in response.json()["detail"].lower()

    def test_update_friendly_name_logs_telegram_failure(self, mock_session_manager, mock_output_monitor, caplog):
        """PATCH /sessions/{id} logs warning when Telegram rename fails (Issue #106)."""
        import logging
        from src.server import create_app
        from fastapi.testclient import TestClient

        # Create session with Telegram thread ID
        session = Session(
            id="test123",
            name="test-session",
            working_dir="/tmp/test",
            tmux_session="claude-test123",
            log_file="/tmp/test.log",
            status=SessionStatus.RUNNING,
            created_at=datetime(2024, 1, 15, 10, 0, 0),
            last_activity=datetime(2024, 1, 15, 11, 0, 0),
            telegram_thread_id=42,  # Has Telegram thread
        )

        mock_session_manager.get_session.return_value = session
        mock_session_manager.tmux.set_status_bar.return_value = True

        # Mock notifier that fails to rename
        mock_notifier = MagicMock()
        mock_notifier.rename_session_topic = AsyncMock(return_value=False)

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            config={},
        )
        client = TestClient(app)

        with caplog.at_level(logging.WARNING):
            response = client.patch(
                "/sessions/test123",
                json={"friendly_name": "new-name"}
            )

        assert response.status_code == 200

        # Verify warning was logged
        assert any("Failed to rename Telegram topic" in record.message for record in caplog.records)
        assert any("test123" in record.message for record in caplog.records)

    def test_patch_is_em_sets_flag(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} with is_em=true sets is_em flag and returns it in response (#256)."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager._save_state = MagicMock()

        response = test_client.patch(
            "/sessions/test123",
            json={"is_em": True}
        )
        assert response.status_code == 200
        data = response.json()
        assert data["is_em"] is True
        assert data["role"] == "em"
        assert sample_session.is_em is True
        assert sample_session.role == "em"
        mock_session_manager._save_state.assert_called()

    def test_patch_is_em_syncs_telegram_topic_title(self, mock_session_manager, mock_output_monitor, sample_session):
        """PATCH /sessions/{id} with is_em=true also syncs the topic title for topic-backed sessions."""
        sample_session.friendly_name = "em-e1-proximity"
        sample_session.telegram_chat_id = 123456
        sample_session.telegram_thread_id = 789
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager._save_state = MagicMock()
        mock_session_manager.tmux.set_status_bar.return_value = True

        mock_notifier = MagicMock()
        mock_notifier.rename_session_topic = AsyncMock(return_value=True)

        app = create_app(
            session_manager=mock_session_manager,
            notifier=mock_notifier,
            output_monitor=mock_output_monitor,
            config={},
        )
        client = TestClient(app)

        response = client.patch(
            "/sessions/test123",
            json={"is_em": True}
        )

        assert response.status_code == 200
        mock_notifier.rename_session_topic.assert_awaited_once_with(
            sample_session,
            "em-e1-proximity",
        )

    def test_patch_is_em_false_clears_flag(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} with is_em=false clears flag if previously set (#256)."""
        sample_session.is_em = True
        sample_session.role = "em"
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager._save_state = MagicMock()

        response = test_client.patch(
            "/sessions/test123",
            json={"is_em": False}
        )
        assert response.status_code == 200
        data = response.json()
        assert data["is_em"] is False
        assert sample_session.is_em is False
        assert sample_session.role is None

    def test_patch_mixed_friendly_name_and_is_em(self, test_client, mock_session_manager, sample_session):
        """PATCH /sessions/{id} with both friendly_name and is_em updates both fields (#256)."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager._save_state = MagicMock()
        mock_session_manager.tmux.set_status_bar.return_value = True

        response = test_client.patch(
            "/sessions/test123",
            json={"friendly_name": "em-session9", "is_em": True}
        )
        assert response.status_code == 200
        data = response.json()
        assert data["friendly_name"] == "em-session9"
        assert data["is_em"] is True

    def test_get_session_includes_is_em(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id} response includes is_em field (#256)."""
        sample_session.is_em = True
        sample_session.role = "architect"
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.get("/sessions/test123")
        assert response.status_code == 200
        data = response.json()
        assert "is_em" in data
        assert data["is_em"] is True
        assert data["role"] == "architect"

    def test_put_role_sets_role(self, test_client, mock_session_manager, sample_session):
        """PUT /sessions/{id}/role sets free-form role tag."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.set_role = None  # fallback path mutates session directly
        mock_session_manager._save_state = MagicMock()

        response = test_client.put("/sessions/test123/role", json={"role": "engineer"})
        assert response.status_code == 200
        data = response.json()
        assert data["role"] == "engineer"
        assert sample_session.role == "engineer"

    def test_put_role_rejects_em(self, test_client, mock_session_manager, sample_session):
        """PUT /sessions/{id}/role rejects em role (must go through sm em path)."""
        mock_session_manager.get_session.return_value = sample_session
        response = test_client.put("/sessions/test123/role", json={"role": "em"})
        assert response.status_code == 400

    def test_put_role_rejects_when_session_is_em(self, test_client, mock_session_manager, sample_session):
        """PUT /sessions/{id}/role cannot override role while is_em=true."""
        sample_session.is_em = True
        sample_session.role = "em"
        mock_session_manager.get_session.return_value = sample_session
        response = test_client.put("/sessions/test123/role", json={"role": "engineer"})
        assert response.status_code == 400

    def test_delete_role_clears_role(self, test_client, mock_session_manager, sample_session):
        """DELETE /sessions/{id}/role clears role tag."""
        sample_session.role = "engineer"
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.clear_role = None  # fallback path mutates session directly
        mock_session_manager._save_state = MagicMock()

        response = test_client.delete("/sessions/test123/role")
        assert response.status_code == 200
        data = response.json()
        assert data["role"] is None
        assert sample_session.role is None

    def test_update_task(self, test_client, mock_session_manager, sample_session):
        """PUT /sessions/{id}/task updates current task."""
        mock_session_manager.get_session.return_value = sample_session

        response = test_client.put(
            "/sessions/test123/task",
            json={"task": "Working on new feature"}
        )
        assert response.status_code == 200

        data = response.json()
        assert data["task"] == "Working on new feature"


class TestOutputEndpoints:
    """Tests for output capture endpoints."""

    def test_capture_output(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id}/output captures tmux output."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.capture_output.return_value = "Claude output here"

        response = test_client.get("/sessions/test123/output")
        assert response.status_code == 200

        data = response.json()
        assert data["session_id"] == "test123"
        assert data["output"] == "Claude output here"

    def test_capture_output_with_lines(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id}/output?lines=100 passes lines parameter."""
        mock_session_manager.get_session.return_value = sample_session
        mock_session_manager.capture_output.return_value = "Output"

        response = test_client.get("/sessions/test123/output?lines=100")
        assert response.status_code == 200

        mock_session_manager.capture_output.assert_called_with("test123", 100)


class TestQueueEndpoints:
    """Tests for message queue endpoints."""

    def test_get_send_queue(self, test_client, mock_session_manager, sample_session):
        """GET /sessions/{id}/send-queue returns queue status."""
        mock_session_manager.get_session.return_value = sample_session
        mock_queue = MagicMock()
        mock_queue.get_queue_status.return_value = {
            "session_id": "test123",
            "is_idle": True,
            "pending_count": 2,
            "pending_messages": [],
            "saved_user_input": None,
        }
        mock_session_manager.message_queue_manager = mock_queue

        response = test_client.get("/sessions/test123/send-queue")
        assert response.status_code == 200

        data = response.json()
        assert data["is_idle"] is True
        assert data["pending_count"] == 2


class TestSessionManagerUnavailable:
    """Tests for when session manager is unavailable."""

    def test_list_sessions_unavailable(self):
        """GET /sessions returns 503 when session manager not configured."""
        app = create_app(session_manager=None)
        client = TestClient(app)

        response = client.get("/sessions")
        assert response.status_code == 503

    def test_create_session_unavailable(self):
        """POST /sessions returns 503 when session manager not configured."""
        app = create_app(session_manager=None)
        client = TestClient(app)

        response = client.post("/sessions", json={"working_dir": "/tmp"})
        assert response.status_code == 503
