from __future__ import annotations

from types import SimpleNamespace
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
    manager.queue_provider_native_rename = AsyncMock(return_value=True)

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

    manager.tmux.set_status_bar.assert_called_with(
        session.tmux_session,
        "maintainer",
        timeout_seconds=None,
    )
    manager.queue_provider_native_rename.assert_awaited_once_with(session, "maintainer")
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


@pytest.mark.parametrize(
    ("provider", "expected_label"),
    [
        ("claude", "Claude"),
        ("codex", "Codex"),
        ("codex-fork", "Codex\\-fork"),
        ("codex-app", "Codex\\-app"),
    ],
)
def test_notifier_response_message_uses_provider_label(tmp_path, provider, expected_label):
    session = _session("reply01", tmp_path, provider=provider)
    notifier = Notifier()
    event = NotificationEvent(
        session_id=session.id,
        event_type="response",
        message="",
        context="done",
        channel=NotificationChannel.TELEGRAM,
    )

    message = notifier._format_message(event, session)

    expected_name = provider.replace("-", "\\-")
    assert message.startswith(f"{expected_name}\\-reply01 \\[reply01\\] *{expected_label}:*")


def test_notifier_response_message_resolves_provider_from_live_session(tmp_path):
    manager = _manager(tmp_path)
    session = _session("snap01", tmp_path, provider="codex")
    manager.sessions[session.id] = session

    notifier = Notifier()
    notifier.session_manager = manager
    snapshot = SimpleNamespace(id=session.id)
    event = NotificationEvent(
        session_id=session.id,
        event_type="response",
        message="",
        context="done",
        channel=NotificationChannel.TELEGRAM,
    )

    message = notifier._format_message(event, snapshot)

    assert message.startswith("codex\\-snap01 \\[snap01\\] *Codex:*")


def test_notifier_uses_live_session_identity_for_snapshot_routing_objects(tmp_path):
    manager = _manager(tmp_path)
    session = _session("maint123", tmp_path)
    session.friendly_name = "codex-345"
    session.telegram_chat_id = 123
    session.telegram_thread_id = 456
    manager.sessions[session.id] = session
    manager.register_agent_role(session.id, "maintainer")

    notifier = Notifier()
    notifier.session_manager = manager
    snapshot = SimpleNamespace(id=session.id, telegram_chat_id=123, telegram_thread_id=456)
    event = NotificationEvent(
        session_id=session.id,
        event_type="message_delivered",
        message="hello from queue",
        channel=NotificationChannel.TELEGRAM,
    )

    message = notifier._format_message(event, snapshot)

    assert "[MESSAGE_DELIVERED] hello from queue" in message
    assert f"Session: {session.id}" in message
