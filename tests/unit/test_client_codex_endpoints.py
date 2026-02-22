"""Unit tests for codex-specific SessionManagerClient helpers used by codex-tui."""

from unittest.mock import patch

from src.cli.client import SessionManagerClient


def _make_client() -> SessionManagerClient:
    return SessionManagerClient(api_url="http://127.0.0.1:8420")


def test_get_codex_events_success():
    client = _make_client()
    payload = {"events": [], "next_seq": 1}
    with patch.object(client, "_request", return_value=(payload, True, False)) as req:
        result = client.get_codex_events("abc123", since_seq=10, limit=25)
    assert result == payload
    req.assert_called_once_with("GET", "/sessions/abc123/codex-events?limit=25&since_seq=10")


def test_get_codex_events_error_returns_none():
    client = _make_client()
    with patch.object(client, "_request", return_value=(None, False, False)):
        assert client.get_codex_events("abc123") is None


def test_get_codex_pending_requests_success():
    client = _make_client()
    payload = {"requests": []}
    with patch.object(client, "_request", return_value=(payload, True, False)) as req:
        result = client.get_codex_pending_requests("abc123", include_orphaned=True)
    assert result == payload
    req.assert_called_once_with("GET", "/sessions/abc123/codex-pending-requests?include_orphaned=true")


def test_send_input_with_result_returns_409_detail():
    client = _make_client()
    response = {"detail": {"error_code": "pending_structured_request"}}
    with patch.object(client, "_request_with_status", return_value=(response, 409, False)):
        result = client.send_input_with_result("abc123", "hello")
    assert result["ok"] is False
    assert result["unavailable"] is False
    assert result["status_code"] == 409
    assert result["detail"]["error_code"] == "pending_structured_request"


def test_respond_codex_request_validation_error():
    client = _make_client()
    result = client.respond_codex_request(
        "abc123",
        "req1",
        decision="accept",
        answers={"x": "y"},
    )
    assert result["ok"] is False
    assert result["status_code"] == 422


def test_respond_codex_request_success():
    client = _make_client()
    payload = {"ok": True}
    with patch.object(client, "_request_with_status", return_value=(payload, 200, False)) as req:
        result = client.respond_codex_request("abc123", "req1", decision="accept")
    assert result["ok"] is True
    assert result["status_code"] == 200
    req.assert_called_once_with(
        "POST",
        "/sessions/abc123/codex-requests/req1/respond",
        {"decision": "accept"},
    )


def test_get_rollout_flags_success():
    client = _make_client()
    payload = {"codex_rollout": {"enable_codex_tui": False}}
    with patch.object(client, "_request", return_value=(payload, True, False)):
        result = client.get_rollout_flags()
    assert result == {"enable_codex_tui": False}


def test_get_output_success_with_timeout():
    client = _make_client()
    payload = {"session_id": "abc123", "output": "line1\\nline2"}
    with patch.object(client, "_request", return_value=(payload, True, False)) as req:
        result = client.get_output("abc123", lines=7, timeout=4)
    assert result == payload
    req.assert_called_once_with("GET", "/sessions/abc123/output?lines=7", timeout=4)


def test_get_tool_calls_success_with_timeout():
    client = _make_client()
    payload = {"session_id": "abc123", "tool_calls": []}
    with patch.object(client, "_request", return_value=(payload, True, False)) as req:
        result = client.get_tool_calls("abc123", limit=12, timeout=3)
    assert result == payload
    req.assert_called_once_with("GET", "/sessions/abc123/tool-calls?limit=12", timeout=3)
