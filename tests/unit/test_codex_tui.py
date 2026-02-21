"""Unit tests for codex-tui command helpers and routing."""

from io import StringIO
from unittest.mock import MagicMock

from src.cli.codex_tui import (
    CodexTui,
    format_event_line,
    normalize_mode,
    parse_answers_json,
    parse_approval_decision,
)


def _make_tui():
    client = MagicMock()
    client.session_id = "sender123"
    tui = CodexTui(
        client=client,
        session_id="codex123",
        stream_in=StringIO(),
        stream_out=StringIO(),
    )
    return tui, client


def test_normalize_mode_aliases():
    assert normalize_mode("chat") == "chat"
    assert normalize_mode("APPROVE") == "approval"
    assert normalize_mode("answers") == "input"
    assert normalize_mode("invalid") is None


def test_parse_approval_decision_aliases():
    assert parse_approval_decision("accept") == "accept"
    assert parse_approval_decision("accept-for-session") == "acceptForSession"
    assert parse_approval_decision("decline") == "decline"
    assert parse_approval_decision("cancel") == "cancel"
    assert parse_approval_decision("bad") is None


def test_parse_answers_json_requires_object():
    assert parse_answers_json('{"k":"v"}') == {"k": "v"}
    assert parse_answers_json("[1,2,3]") is None
    assert parse_answers_json("{not-json}") is None


def test_format_event_line_includes_summary():
    line = format_event_line(
        {
            "seq": 42,
            "timestamp": "2026-02-21T00:00:00+00:00",
            "event_type": "request_approval",
            "payload_preview": {"method": "item/fileChange/requestApproval"},
        },
        width=120,
    )
    assert "42" in line
    assert "request_approval" in line
    assert "method=item/fileChange/requestApproval" in line


def test_chat_mode_routes_to_send_input():
    tui, client = _make_tui()
    tui.state.mode = "chat"
    tui.state.pending_requests = []
    client.send_input_with_result.return_value = {
        "ok": True,
        "unavailable": False,
        "status_code": 200,
        "detail": None,
    }

    tui.handle_line("ship it")

    client.send_input_with_result.assert_called_once()
    assert "queued" in tui.state.status_message.lower()


def test_chat_mode_blocks_when_pending_structured_requests_exist():
    tui, client = _make_tui()
    tui.state.mode = "chat"
    tui.state.pending_requests = [{"request_id": "req-1", "request_type": "request_approval"}]

    tui.handle_line("hello")

    client.send_input_with_result.assert_not_called()
    assert "blocked" in tui.state.status_message.lower()


def test_approval_mode_routes_to_respond_endpoint():
    tui, client = _make_tui()
    tui.state.mode = "approval"
    tui.state.pending_requests = [{"request_id": "req-7", "request_type": "request_approval"}]
    tui.state.selected_request_id = "req-7"
    client.respond_codex_request.return_value = {
        "ok": True,
        "unavailable": False,
        "status_code": 200,
    }

    tui.handle_line("accept-for-session")

    client.respond_codex_request.assert_called_once_with(
        "codex123",
        "req-7",
        decision="acceptForSession",
    )
    assert "resolved" in tui.state.status_message.lower()


def test_input_mode_routes_to_answers_payload():
    tui, client = _make_tui()
    tui.state.mode = "input"
    tui.state.pending_requests = [{"request_id": "req-9", "request_type": "request_user_input"}]
    tui.state.selected_request_id = "req-9"
    client.respond_codex_request.return_value = {
        "ok": True,
        "unavailable": False,
        "status_code": 200,
    }

    tui.handle_line('{"choice":"a"}')

    client.respond_codex_request.assert_called_once_with(
        "codex123",
        "req-9",
        answers={"choice": "a"},
    )
    assert "answered" in tui.state.status_message.lower()


def test_select_command_by_index():
    tui, _ = _make_tui()
    tui.state.pending_requests = [
        {"request_id": "req-a", "request_type": "request_approval"},
        {"request_id": "req-b", "request_type": "request_user_input"},
    ]

    tui.handle_line("/select 2")

    assert tui.state.selected_request_id == "req-b"


def test_read_line_with_timeout_treats_eof_as_stop():
    tui, _ = _make_tui()

    line = tui._read_line_with_timeout(0.1)

    assert line is None
    assert tui.running is False


def test_read_line_with_timeout_keeps_blank_line_as_input():
    tui, _ = _make_tui()
    tui.stream_in = StringIO("\n")

    line = tui._read_line_with_timeout(0.1)

    assert line == "\n"
    assert tui.running is True
