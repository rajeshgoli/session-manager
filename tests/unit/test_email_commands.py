"""Unit tests for email bridge CLI commands."""
from unittest.mock import Mock

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
