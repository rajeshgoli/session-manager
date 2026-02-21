"""Unit tests for sm role command (#287)."""

from unittest.mock import Mock

from src.cli.client import SessionManagerClient
from src.cli.commands import cmd_role


def _make_client() -> SessionManagerClient:
    return Mock(spec=SessionManagerClient)


def test_cmd_role_requires_managed_session():
    client = _make_client()
    rc = cmd_role(client, session_id=None, role="engineer", clear=False)
    assert rc == 2


def test_cmd_role_set_success():
    client = _make_client()
    client.set_role.return_value = (True, False)
    rc = cmd_role(client, session_id="abc12345", role="engineer", clear=False)
    assert rc == 0
    client.set_role.assert_called_once_with("abc12345", "engineer")


def test_cmd_role_clear_success():
    client = _make_client()
    client.clear_role.return_value = (True, False)
    rc = cmd_role(client, session_id="abc12345", role=None, clear=True)
    assert rc == 0
    client.clear_role.assert_called_once_with("abc12345")
