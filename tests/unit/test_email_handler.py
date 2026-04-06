"""Unit tests for the Resend-backed email bridge."""

from pathlib import Path
from unittest.mock import AsyncMock, Mock, patch

import pytest

from src.email_handler import EmailHandler


def _write_bridge_config(path: Path) -> None:
    path.write_text(
        "\n".join(
            [
                "resend:",
                "  api_key: re_test",
                "  domain: rajeshgo.li",
                "users:",
                "  rajesh:",
                "    email: rajesh@example.com",
                "    aliases:",
                "      - owner",
                "  architect: architect@example.com",
                "email_bridge:",
                "  authorized_senders:",
                "    - rajesh@example.com",
                "  webhook_path: /api/email-inbound",
            ]
        ),
        encoding="utf-8",
    )


def test_lookup_user_and_authorized_sender(tmp_path):
    config_path = tmp_path / "email_send.yaml"
    _write_bridge_config(config_path)
    handler = EmailHandler(bridge_config=str(config_path))

    rajesh = handler.lookup_user("owner")

    assert rajesh is not None
    assert rajesh.username == "rajesh"
    assert rajesh.email == "rajesh@example.com"
    assert handler.is_authorized_sender("rajesh@example.com") is True
    assert handler.is_authorized_sender("other@example.com") is False
    assert handler.bridge_webhook_path() == "/api/email-inbound"


@pytest.mark.asyncio
async def test_send_agent_email_builds_resend_payload(tmp_path):
    config_path = tmp_path / "email_send.yaml"
    _write_bridge_config(config_path)
    handler = EmailHandler(bridge_config=str(config_path))

    response = Mock()
    response.status_code = 200
    response.json.return_value = {"id": "email_123"}

    with patch("src.email_handler.httpx.AsyncClient.post", new=AsyncMock(return_value=response)) as post_mock:
        result = await handler.send_agent_email(
            sender_session_id="abc12345",
            sender_name="engineer-issue497",
            to_identifiers=["rajesh"],
            cc_identifiers=["architect"],
            subject="Reading list",
            body_text="# Summary\n\n- item",
            body_markdown=True,
        )

    assert result["message_id"] == "email_123"
    post_mock.assert_awaited_once()
    call = post_mock.await_args
    assert call.args[0] == "https://api.resend.com/emails"
    payload = call.kwargs["json"]
    assert payload["from"] == "engineer-issue497 <abc12345@rajeshgo.li>"
    assert payload["reply_to"] == "abc12345@rajeshgo.li"
    assert payload["to"] == ["rajesh@example.com"]
    assert payload["cc"] == ["architect@example.com"]
    assert payload["subject"] == "Reading list"
    assert "<ul>" in payload["html"]
    assert payload["headers"]["X-SM-Session-ID"] == "abc12345"
