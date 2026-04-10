"""Unit tests for email bridge CLI commands."""

import sys
from unittest.mock import Mock, patch

import pytest

from src.cli.commands import cmd_email, cmd_send


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
    assert "Email sent to rajesh <rajesh@example.com>" in capsys.readouterr().out


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
    )
    assert expected_snippet in capsys.readouterr().out


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
    assert "Email sent to rajesh <rajesh@example.com>" in capsys.readouterr().out


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
    assert "Email sent to rajesh <rajesh@example.com>" in capsys.readouterr().out
