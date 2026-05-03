"""Unit tests for email bridge CLI commands."""

import sys
from unittest.mock import Mock, patch

import pytest

from src.cli.client import SEND_API_TIMEOUT
from src.cli.commands import cmd_email, cmd_send, cmd_telegram


def test_cmd_send_falls_back_to_registered_user_email(capsys):
    client = Mock()
    client.session_id = "sender123"
    client.get_session.return_value = None
    client.ensure_role.return_value = {
        "ok": False,
        "unavailable": False,
        "status_code": 404,
        "detail": "Role not configured for auto-bootstrap",
    }
    client.list_sessions.return_value = []
    client.send_email_result.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "to": [{"username": "rajesh", "email": "rajesh@example.com"}],
            "subject": "Deployment done",
        },
    }

    rc = cmd_send(client, "rajesh", "Deployment done")

    assert rc == 0
    client.send_input.assert_not_called()
    client.send_email_result.assert_called_once_with(
        requester_session_id="sender123",
        recipients=["rajesh"],
        body_text="Deployment done",
        auto_subject=True,
    )
    output = capsys.readouterr().out
    assert "Email sent to rajesh" in output
    assert "rajesh@example.com" not in output


def test_cmd_send_email_fallback_rejects_track(capsys):
    client = Mock()
    client.session_id = "sender123"
    client.get_session.return_value = None
    client.ensure_role.return_value = {
        "ok": False,
        "unavailable": False,
        "status_code": 404,
        "detail": "Role not configured for auto-bootstrap",
    }
    client.list_sessions.return_value = []

    rc = cmd_send(client, "rajesh", "Deployment done", track_seconds=300)

    assert rc == 1
    client.send_email_result.assert_not_called()
    assert "email fallback only supports plain sequential sends" in capsys.readouterr().err


def _human_lookup_payload() -> dict:
    return {
        "recipient": "rajesh",
        "display_name": "Human operator",
        "aliases": ["rajesh", "rajeshgoli", "user"],
        "default_channel": "telegram",
        "available_channels": ["telegram", "email"],
        "telegram_delivery": "sender_session_topic",
        "email_use": "fallback_only",
    }


def test_cmd_send_human_alias_defaults_to_telegram(capsys):
    client = Mock()
    client.session_id = "sender123"
    client.get_session.return_value = None
    client.list_sessions.return_value = []
    client.lookup_human.return_value = {
        "ok": True,
        "unavailable": False,
        "data": _human_lookup_payload(),
    }
    client.send_human_telegram_result.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {"recipient": "rajesh", "thread": "sender session topic"},
    }

    rc = cmd_send(client, "user", "status for the human")

    assert rc == 0
    client.send_human_telegram_result.assert_called_once_with(
        requester_session_id="sender123",
        recipient="user",
        text="status for the human",
    )
    client.ensure_role.assert_not_called()
    client.send_email_result.assert_not_called()
    output = capsys.readouterr().out
    assert "Telegram sent to rajesh" in output
    assert "Thread: sender session topic" in output


@pytest.mark.parametrize(
    "session_identity",
    [
        {"friendly_name": "user"},
        {"friendly_name": "historical-user", "aliases": ["user"]},
    ],
)
def test_cmd_send_prefers_live_session_identity_over_human_alias(
    session_identity: dict,
    capsys,
):
    client = Mock()
    client.session_id = "sender123"
    client.get_session.return_value = None
    client.list_sessions.return_value = [
        {
            "id": "live-user",
            "status": "idle",
            "provider": "claude",
            **session_identity,
        }
    ]
    client.lookup_human.return_value = {
        "ok": True,
        "unavailable": False,
        "data": _human_lookup_payload(),
    }
    client.send_input.return_value = (True, False)

    rc = cmd_send(client, "user", "message for existing session")

    assert rc == 0
    client.lookup_human.assert_not_called()
    client.send_human_telegram_result.assert_not_called()
    client.ensure_role.assert_not_called()
    client.send_email_result.assert_not_called()
    client.send_input.assert_called_once_with(
        "live-user",
        "message for existing session",
        sender_session_id="sender123",
        delivery_mode="sequential",
        from_sm_send=True,
        timeout_seconds=None,
        notify_on_delivery=False,
        notify_after_seconds=None,
        notify_on_stop=True,
        remind_soft_threshold=None,
        remind_hard_threshold=None,
        remind_cancel_on_reply_session_id=None,
        parent_session_id=None,
        timeout=SEND_API_TIMEOUT,
    )
    output = capsys.readouterr().out
    expected_name = session_identity["friendly_name"]
    assert f"Input sent to {expected_name} (live-user)" in output


def test_cmd_telegram_forces_human_telegram(capsys):
    client = Mock()
    client.send_human_telegram_result.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {"recipient": "rajesh", "thread": "sender session topic"},
    }

    rc = cmd_telegram(
        client,
        sender_session_id="sender123",
        recipient="rajeshgoli",
        text="forced telegram",
    )

    assert rc == 0
    client.send_human_telegram_result.assert_called_once_with(
        requester_session_id="sender123",
        recipient="rajeshgoli",
        text="forced telegram",
    )
    output = capsys.readouterr().out
    assert "Telegram sent to rajesh" in output


def test_cmd_email_human_uses_explicit_email_endpoint_and_redacts_address(capsys):
    client = Mock()
    client.lookup_human.return_value = {
        "ok": True,
        "unavailable": False,
        "data": _human_lookup_payload(),
    }
    client.send_human_email_result.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "recipient": "rajesh",
            "subject": "Auto subject",
            "to": [{"username": "rajesh", "email": "private@example.com"}],
        },
    }

    rc = cmd_email(
        client,
        sender_session_id="sender123",
        recipients_raw="rajeshgoli",
        body="explicit fallback",
    )

    assert rc == 0
    client.send_human_email_result.assert_called_once_with(
        requester_session_id="sender123",
        recipient="rajeshgoli",
        text="explicit fallback",
        subject=None,
        body_markdown=False,
        auto_subject=True,
    )
    client.send_email_result.assert_not_called()
    output = capsys.readouterr().out
    assert "Email sent to rajesh" in output
    assert "private@example.com" not in output


def test_cmd_email_human_with_cc_rejects_before_registered_email(capsys):
    client = Mock()
    client.lookup_human.side_effect = lambda identifier: (
        {
            "ok": True,
            "unavailable": False,
            "data": _human_lookup_payload(),
        }
        if identifier == "rajeshgoli"
        else {"ok": False, "unavailable": False, "status_code": 404}
    )

    rc = cmd_email(
        client,
        sender_session_id="sender123",
        recipients_raw="rajeshgoli",
        subject="Fallback",
        body="explicit fallback",
        cc_raw="architect",
    )

    assert rc == 1
    client.send_human_email_result.assert_not_called()
    client.send_email_result.assert_not_called()
    assert "supports exactly one recipient and no --cc" in capsys.readouterr().err


def test_cmd_email_human_html_rejects_before_registered_email(tmp_path, capsys):
    html_path = tmp_path / "body.html"
    html_path.write_text("<p>explicit fallback</p>", encoding="utf-8")

    client = Mock()
    client.lookup_human.return_value = {
        "ok": True,
        "unavailable": False,
        "data": _human_lookup_payload(),
    }

    rc = cmd_email(
        client,
        sender_session_id="sender123",
        recipients_raw="rajeshgoli",
        subject="Fallback",
        html_file=str(html_path),
    )

    assert rc == 1
    client.send_human_email_result.assert_not_called()
    client.send_email_result.assert_not_called()
    assert "supports plain text or markdown bodies only" in capsys.readouterr().err


@pytest.mark.parametrize(
    ("delivery_mode", "expected_snippet"),
    [
        ("sequential", "Input sent to em-2773-new (live123)"),
        ("urgent", "Input sent to em-2773-new (live123) (interrupted)"),
    ],
)
def test_cmd_send_prefers_live_named_session_over_email_fallback(
    delivery_mode: str,
    expected_snippet: str,
    capsys,
):
    client = Mock()
    client.session_id = "sender123"
    client.get_session.return_value = None
    client.list_sessions.return_value = [
        {
            "id": "live123",
            "friendly_name": "em-2773-new",
            "status": "idle",
            "provider": "claude",
        }
    ]
    client.send_input.return_value = (True, False)

    rc = cmd_send(client, "em-2773-new", "routing check", delivery_mode=delivery_mode)

    assert rc == 0
    client.ensure_role.assert_not_called()
    client.send_email_result.assert_not_called()
    client.send_input.assert_called_once_with(
        "live123",
        "routing check",
        sender_session_id="sender123",
        delivery_mode=delivery_mode,
        from_sm_send=True,
        timeout_seconds=None,
        notify_on_delivery=False,
        notify_after_seconds=None,
        notify_on_stop=True,
        remind_soft_threshold=None,
        remind_hard_threshold=None,
        remind_cancel_on_reply_session_id=None,
        parent_session_id=None,
        timeout=SEND_API_TIMEOUT,
    )
    assert expected_snippet in capsys.readouterr().out


def test_cmd_send_resolution_timeout_returns_unavailable_without_email_fallback(capsys):
    client = Mock()
    client.session_id = "sender123"
    client.get_session.return_value = None
    client.list_sessions.return_value = None

    rc = cmd_send(client, "fcb79e8b", "routing check")

    assert rc == 2
    client.ensure_role.assert_not_called()
    client.send_email_result.assert_not_called()
    assert "request timed out" in capsys.readouterr().err


def test_cmd_send_uses_extended_timeout_for_resolution_and_delivery(capsys):
    client = Mock()
    client.session_id = "sender123"
    client.get_session.return_value = {
        "id": "live123",
        "friendly_name": "worker",
        "status": "running",
        "provider": "claude",
    }
    client.send_input.return_value = (True, False)

    rc = cmd_send(client, "live123", "routing check")

    assert rc == 0
    client.get_session.assert_called_once_with("live123", timeout=SEND_API_TIMEOUT)
    assert client.send_input.call_args.kwargs["timeout"] == SEND_API_TIMEOUT
    assert "Input sent to worker (live123)" in capsys.readouterr().out


def test_cmd_send_multiple_recipients_uses_batch_endpoint(capsys):
    client = Mock()
    client.session_id = "sender123"
    client.send_input_batch_result.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "failure_count": 0,
            "results": [
                {
                    "identifier": "owner123",
                    "status": "delivered",
                    "delivery_kind": "session",
                    "session_id": "owner123",
                    "target_name": "spec-owner-3004",
                },
                {
                    "identifier": "d030a600",
                    "status": "emailed",
                    "delivery_kind": "email",
                    "email_username": "orchestrator",
                    "email_address": "orch@example.com",
                },
            ],
        },
    }

    rc = cmd_send(client, "owner123, d030a600, owner123,", "review landed")

    assert rc == 0
    client.send_input.assert_not_called()
    client.send_input_batch_result.assert_called_once_with(
        ["owner123", "d030a600"],
        "review landed",
        sender_session_id="sender123",
        delivery_mode="sequential",
        from_sm_send=True,
        timeout_seconds=None,
        notify_on_delivery=False,
        notify_after_seconds=None,
        notify_on_stop=True,
        remind_soft_threshold=None,
        remind_hard_threshold=None,
        remind_cancel_on_reply_session_id=None,
        parent_session_id=None,
        timeout=SEND_API_TIMEOUT,
    )
    output = capsys.readouterr().out
    assert "Input sent to spec-owner-3004 (owner123)" in output
    assert "Email sent to orchestrator" in output
    assert "orch@example.com" not in output


def test_cmd_send_multiple_recipients_returns_nonzero_on_partial_failure(capsys):
    client = Mock()
    client.session_id = "sender123"
    client.send_input_batch_result.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "failure_count": 1,
            "results": [
                {
                    "identifier": "owner123",
                    "status": "delivered",
                    "delivery_kind": "session",
                    "session_id": "owner123",
                    "target_name": "spec-owner-3004",
                },
                {
                    "identifier": "missing-user",
                    "status": "failed",
                    "delivery_kind": "none",
                    "detail": "Session 'missing-user' not found",
                },
            ],
        },
    }

    rc = cmd_send(client, "owner123,missing-user", "review landed")

    assert rc == 1
    output = capsys.readouterr()
    assert "Input sent to spec-owner-3004 (owner123)" in output.out
    assert "Error: missing-user: Session 'missing-user' not found" in output.err


def test_cmd_email_reads_markdown_file_and_calls_api(tmp_path, capsys):
    body_path = tmp_path / "summary.md"
    body_path.write_text("# Summary\n\n- one\n- two\n", encoding="utf-8")

    client = Mock()
    client.send_email_result.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "to": [{"username": "rajesh", "email": "rajesh@example.com"}],
            "subject": "Reading list",
        },
    }

    rc = cmd_email(
        client,
        sender_session_id="sender123",
        recipients_raw="rajesh",
        subject="Reading list",
        text_file=str(body_path),
        cc_raw="architect",
    )

    assert rc == 0
    client.send_email_result.assert_called_once()
    kwargs = client.send_email_result.call_args.kwargs
    assert kwargs["requester_session_id"] == "sender123"
    assert kwargs["recipients"] == ["rajesh"]
    assert kwargs["cc"] == ["architect"]
    assert kwargs["subject"] == "Reading list"
    assert kwargs["body_text"] == body_path.read_text(encoding="utf-8")
    assert kwargs["body_markdown"] is True
    output = capsys.readouterr().out
    assert "Email sent to rajesh" in output
    assert "rajesh@example.com" not in output


def test_cmd_email_treats_stdin_as_markdown(capsys):
    client = Mock()
    client.send_email_result.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "to": [{"username": "rajesh", "email": "rajesh@example.com"}],
            "subject": "From stdin",
        },
    }

    with patch.object(sys.stdin, "isatty", return_value=False), patch.object(
        sys.stdin,
        "read",
        return_value="# Heading\n\n## Subhead\n",
    ):
        rc = cmd_email(
            client,
            sender_session_id="sender123",
            recipients_raw="rajesh",
            subject="From stdin",
        )

    assert rc == 0
    kwargs = client.send_email_result.call_args.kwargs
    assert kwargs["body_text"] == "# Heading\n\n## Subhead\n"
    assert kwargs["body_markdown"] is True
    output = capsys.readouterr().out
    assert "Email sent to rajesh" in output
    assert "rajesh@example.com" not in output
