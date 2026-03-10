from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import pytest
from fastapi.testclient import TestClient

from src.models import DeliveryResult, NotificationChannel, NotificationEvent, Session, SessionStatus
from src.notifier import Notifier
from src.server import create_app
from src.session_manager import SessionManager


def _manager(tmp_path) -> SessionManager:
    return SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={},
    )


def _session(session_id: str, tmp_path, *, provider: str = "claude") -> Session:
    return Session(
        id=session_id,
        name=f"{provider}-{session_id}",
        working_dir=str(tmp_path),
        tmux_session=f"{provider}-{session_id}",
        provider=provider,
        log_file=str(tmp_path / f"{session_id}.log"),
        status=SessionStatus.RUNNING,
    )


def test_registry_alias_becomes_effective_api_name_and_syncs_surfaces(tmp_path):
    manager = _manager(tmp_path)
    session = _session("maint123", tmp_path)
    session.friendly_name = "codex-345"
    session.telegram_chat_id = 123
    session.telegram_thread_id = 456
    manager.sessions[session.id] = session
    manager.tmux = MagicMock()

    notifier = MagicMock()
    notifier.rename_session_topic = AsyncMock(return_value=True)
    client = TestClient(create_app(session_manager=manager, notifier=notifier))

    response = client.post(
        f"/sessions/{session.id}/registry",
        json={"requester_session_id": session.id, "role": "maintainer"},
    )

    assert response.status_code == 200
    assert response.json()["friendly_name"] == "maintainer"
    assert session.friendly_name == "codex-345"

    session_response = client.get(f"/sessions/{session.id}")
    assert session_response.status_code == 200
    payload = session_response.json()
    assert payload["friendly_name"] == "maintainer"
    assert payload["aliases"] == ["maintainer"]

    manager.tmux.set_status_bar.assert_called_with(session.tmux_session, "maintainer")
    notifier.rename_session_topic.assert_awaited_with(session, "maintainer")


def test_update_session_rejects_reserved_registry_name_for_unregistered_session(tmp_path):
    manager = _manager(tmp_path)
    session = _session("worker01", tmp_path)
    manager.sessions[session.id] = session
    client = TestClient(create_app(session_manager=manager))

    response = client.patch(f"/sessions/{session.id}", json={"friendly_name": "maintainer"})

    assert response.status_code == 400
    assert "reserved for registry identity" in response.json()["detail"]


def test_update_session_rejects_conflicting_name_when_alias_controls_identity(tmp_path):
    manager = _manager(tmp_path)
    session = _session("maint123", tmp_path)
    session.friendly_name = "codex-345"
    manager.sessions[session.id] = session
    manager.register_agent_role(session.id, "maintainer")
    client = TestClient(create_app(session_manager=manager))

    response = client.patch(f"/sessions/{session.id}", json={"friendly_name": "codex-345"})

    assert response.status_code == 400
    assert 'registry role "maintainer"' in response.json()["detail"]


@pytest.mark.asyncio
async def test_send_input_prefers_registry_alias_for_sender_label(tmp_path):
    manager = _manager(tmp_path)
    sender = _session("sender01", tmp_path)
    sender.friendly_name = "codex-345"
    recipient = _session("target01", tmp_path)
    manager.sessions[sender.id] = sender
    manager.sessions[recipient.id] = recipient
    manager.register_agent_role(sender.id, "maintainer")

    queue = MagicMock()
    queue.queue_message = MagicMock(return_value=MagicMock(id="msg-123"))
    queue.deliver_queued_message_now = AsyncMock(return_value=True)
    manager.message_queue_manager = queue

    result = await manager.send_input(
        session_id=recipient.id,
        text="fix this bug",
        sender_session_id=sender.id,
        delivery_mode="sequential",
    )

    assert result == DeliveryResult.DELIVERED
    assert queue.queue_message.call_args.kwargs["sender_name"] == "maintainer"
    assert queue.queue_message.call_args.kwargs["text"].startswith(
        f"[Input from: maintainer ({sender.id[:8]}) via sm send]\n"
    )


def test_notifier_response_message_prefers_registry_alias(tmp_path):
    manager = _manager(tmp_path)
    session = _session("maint123", tmp_path)
    session.friendly_name = "codex-345"
    manager.sessions[session.id] = session
    manager.register_agent_role(session.id, "maintainer")

    notifier = Notifier()
    notifier.session_manager = manager
    event = NotificationEvent(
        session_id=session.id,
        event_type="response",
        message="",
        context="done",
        channel=NotificationChannel.TELEGRAM,
    )

    message = notifier._format_message(event, session)

    assert message.startswith("maintainer \\[maint123\\] *Claude:*")
