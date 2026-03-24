"""Unit tests for role endpoints in SessionManagerClient (#287)."""

from unittest.mock import patch

from src.cli.client import SessionManagerClient


def _make_client() -> SessionManagerClient:
    return SessionManagerClient(api_url="http://127.0.0.1:8420")


def test_set_role_sends_put_role_payload():
    client = _make_client()
    captured = {}

    def fake_request(method, path, data=None, timeout=None):
        captured["method"] = method
        captured["path"] = path
        captured["data"] = data
        return {"id": "abc12345", "role": "engineer"}, True, False

    with patch.object(client, "_request", side_effect=fake_request):
        success, unavailable = client.set_role("abc12345", "engineer")

    assert captured["method"] == "PUT"
    assert captured["path"] == "/sessions/abc12345/role"
    assert captured["data"] == {"role": "engineer"}
    assert success is True
    assert unavailable is False


def test_clear_role_sends_delete():
    client = _make_client()
    captured = {}

    def fake_request(method, path, data=None, timeout=None):
        captured["method"] = method
        captured["path"] = path
        captured["data"] = data
        return {"id": "abc12345", "role": None}, True, False

    with patch.object(client, "_request", side_effect=fake_request):
        success, unavailable = client.clear_role("abc12345")

    assert captured["method"] == "DELETE"
    assert captured["path"] == "/sessions/abc12345/role"
    assert captured["data"] is None
    assert success is True
    assert unavailable is False


def test_ensure_role_posts_generic_role_endpoint():
    client = _make_client()
    captured = {}

    def fake_request_with_status(method, path, data=None, timeout=None):
        captured["method"] = method
        captured["path"] = path
        captured["data"] = data
        captured["timeout"] = timeout
        return {"created": True, "session": {"id": "chief001"}}, 200, False

    with patch.object(client, "_request_with_status", side_effect=fake_request_with_status):
        result = client.ensure_role("chief-scientist", requester_session_id="sender123")

    assert captured["method"] == "POST"
    assert captured["path"] == "/registry/chief-scientist/ensure"
    assert captured["data"] == {"requester_session_id": "sender123"}
    assert captured["timeout"] == 10
    assert result["ok"] is True
    assert result["data"]["session"]["id"] == "chief001"


def test_ensure_maintainer_delegates_to_generic_role_endpoint():
    client = _make_client()

    with patch.object(client, "ensure_role", return_value={"ok": True}) as ensure_role:
        result = client.ensure_maintainer(requester_session_id="sender123")

    ensure_role.assert_called_once_with("maintainer", requester_session_id="sender123")
    assert result == {"ok": True}
