"""Unit tests for the Resend-backed email bridge."""

from email.message import EmailMessage
from email import policy
from pathlib import Path
from unittest.mock import AsyncMock, Mock, patch

import pytest
import httpx

from src.email_handler import EmailHandler


def _write_bridge_config(path: Path) -> None:
    path.write_text(
        "\n".join(
            [
                "resend:",
                "  api_key: re_test",
                "  domain: sm.rajeshgo.li",
                "  reply_address: reply@sm.rajeshgo.li",
                "users:",
                "  rajesh:",
                "    email: rajesh@example.com",
                "    aliases:",
                "      - owner",
                "  architect: architect@example.com",
                "email_bridge:",
                "  authorized_senders:",
                "    - rajesh@example.com",
                "  worker_secret: worker-secret-123",
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
    assert handler.bridge_worker_secret() == "worker-secret-123"
    assert handler.bridge_worker_secret_header() == "x-email-worker-secret"
    assert handler.bridge_session_id_header() == "x-email-session-id"
    assert handler.normalize_explicit_session_id("ABC12345") == "abc12345"
    assert handler.normalize_explicit_session_id("reply@sm.rajeshgo.li") is None


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
            sender_provider="codex",
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
    assert payload["from"] == "engineer-issue497 <reply@sm.rajeshgo.li>"
    assert payload["reply_to"] == "reply@sm.rajeshgo.li"
    assert payload["to"] == ["rajesh@example.com"]
    assert payload["cc"] == ["architect@example.com"]
    assert payload["subject"] == "Reading list"
    assert "<ul>" in payload["html"]
    assert payload["text"].endswith("--\nSM: engineer-issue497 abc12345 codex")
    assert "SM: engineer-issue497 abc12345 codex" in payload["html"]
    assert payload["headers"]["X-SM-Session-ID"] == "abc12345"


@pytest.mark.asyncio
async def test_send_agent_email_wraps_transport_failure(tmp_path):
    config_path = tmp_path / "email_send.yaml"
    _write_bridge_config(config_path)
    handler = EmailHandler(bridge_config=str(config_path))

    with patch(
        "src.email_handler.httpx.AsyncClient.post",
        new=AsyncMock(side_effect=httpx.ConnectError("boom")),
    ):
        with pytest.raises(RuntimeError, match="Resend email send failed"):
            await handler.send_agent_email(
                sender_session_id="abc12345",
                sender_name="engineer-issue497",
                sender_provider="codex",
                to_identifiers=["rajesh"],
                subject="Reading list",
                body_text="Hello",
            )


def test_extract_routed_session_id_from_quoted_footer(tmp_path):
    config_path = tmp_path / "email_send.yaml"
    _write_bridge_config(config_path)
    handler = EmailHandler(bridge_config=str(config_path))

    body = "\n".join(
        [
            "Please take a look.",
            "",
            "On Sun, Apr 5, 2026 at 10:00 AM maintainer wrote:",
            "> prior context",
            "> --",
            "> SM: maintainer abc12345 codex",
        ]
    )

    assert handler.extract_routed_session_id(body) == "abc12345"
    assert handler.extract_reply_message_body(body) == "Please take a look."


def test_extract_text_from_raw_email_prefers_text_plain_part(tmp_path):
    config_path = tmp_path / "email_send.yaml"
    _write_bridge_config(config_path)
    handler = EmailHandler(bridge_config=str(config_path))

    message = EmailMessage()
    message["From"] = "Rajesh <rajesh@example.com>"
    message["To"] = "reply@sm.rajeshgo.li"
    message["Subject"] = "Re: test"
    message.set_content(
        "inbound footer test live\n\n"
        "On Sun, Apr 6, 2026 at 12:00 AM maintainer wrote:\n"
        "> hello\n"
        "> --\n"
        "> SM: maintainer 057f8de4 codex-fork\n"
    )
    message.add_alternative(
        "<div>inbound footer test live</div><blockquote>--<br>SM: maintainer 057f8de4 codex-fork</blockquote>",
        subtype="html",
    )

    raw_email = message.as_bytes(policy=policy.SMTP).decode("utf-8", errors="replace")
    extracted = handler.extract_text_from_raw_email(raw_email)

    assert "inbound footer test live" in extracted
    assert "SM: maintainer 057f8de4 codex-fork" in extracted
    assert handler.extract_subject_from_raw_email(raw_email) == "Re: test"
