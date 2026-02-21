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
