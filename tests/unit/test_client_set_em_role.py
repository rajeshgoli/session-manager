"""Unit tests for sm#256: set_em_role() in client.py."""

from unittest.mock import patch

from src.cli.client import SessionManagerClient


def _make_client() -> SessionManagerClient:
    return SessionManagerClient(api_url="http://127.0.0.1:8420")


def test_set_em_role_sends_patch_with_is_em_true():
    """set_em_role sends PATCH /sessions/{id} with {"is_em": True}."""
    client = _make_client()
    captured = {}

    def fake_request(method, path, data=None, timeout=None):
        captured["method"] = method
        captured["path"] = path
        captured["data"] = data
        return {"id": "abc12345", "is_em": True}, True, False

    with patch.object(client, "_request", side_effect=fake_request):
        success, unavailable = client.set_em_role("abc12345")

    assert captured["method"] == "PATCH"
    assert captured["path"] == "/sessions/abc12345"
    assert captured["data"] == {"is_em": True}
    assert success is True
    assert unavailable is False


def test_set_em_role_returns_true_false_on_success():
    """set_em_role returns (True, False) on HTTP 200."""
    client = _make_client()
    with patch.object(client, "_request", return_value=({"is_em": True}, True, False)):
        success, unavailable = client.set_em_role("abc12345")
    assert success is True
    assert unavailable is False


def test_set_em_role_returns_false_true_on_unavailable():
    """set_em_role returns (False, True) when session manager is unavailable."""
    client = _make_client()
    with patch.object(client, "_request", return_value=(None, False, True)):
        success, unavailable = client.set_em_role("abc12345")
    assert success is False
    assert unavailable is True


def test_set_em_role_returns_false_false_on_api_error():
    """set_em_role returns (False, False) on API error (4xx/5xx)."""
    client = _make_client()
    with patch.object(client, "_request", return_value=(None, False, False)):
        success, unavailable = client.set_em_role("abc12345")
    assert success is False
    assert unavailable is False
