"""Unit tests for sm#233: list_children() error-masking fix in client.py."""

from unittest.mock import MagicMock, patch

import pytest

from src.cli.client import SessionManagerClient


def _make_client() -> SessionManagerClient:
    return SessionManagerClient(api_url="http://127.0.0.1:8420")


def test_list_children_returns_none_on_api_error():
    """When _request returns (data, False, False) — API error — list_children returns None."""
    client = _make_client()
    with patch.object(client, "_request", return_value=({"children": []}, False, False)):
        result = client.list_children("abc12345")
    assert result is None


def test_list_children_returns_none_on_unavailable():
    """When _request returns (None, False, True) — unavailable — list_children returns None."""
    client = _make_client()
    with patch.object(client, "_request", return_value=(None, False, True)):
        result = client.list_children("abc12345")
    assert result is None


def test_list_children_returns_data_on_success():
    """When _request returns success, list_children returns the data dict."""
    children_payload = {"children": [{"id": "b2c3d4e5", "name": "scout-1465"}]}
    client = _make_client()
    with patch.object(client, "_request", return_value=(children_payload, True, False)):
        result = client.list_children("abc12345")
    assert result == children_payload
    assert len(result["children"]) == 1
