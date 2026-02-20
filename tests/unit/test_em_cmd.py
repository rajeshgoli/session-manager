"""Unit tests for sm#233: sm em one-shot EM pre-flight command."""

from unittest.mock import MagicMock, call

from src.cli.commands import cmd_em


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


_EMPTY_CHILDREN = {"children": []}


def _make_client(
    name_result=(True, False),
    em_role_result=(True, False),
    context_monitor_result=(None, True, False),
    children_data=_EMPTY_CHILDREN,
    remind_result={"status": "ok"},
):
    """Build a mock SessionManagerClient with configurable responses.

    Pass children_data=None to simulate server unavailable (list_children returns None).
    Pass children_data={"children": []} for empty children.
    """
    client = MagicMock()
    client.update_friendly_name.return_value = name_result
    client.set_em_role.return_value = em_role_result
    client.set_context_monitor.return_value = context_monitor_result
    client.list_children.return_value = children_data
    client.register_remind.return_value = remind_result
    return client


def _make_child(child_id: str, friendly_name: str) -> dict:
    return {"id": child_id, "friendly_name": friendly_name, "name": f"claude-{child_id}"}


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_name_set_with_suffix(capsys):
    """sm em session9 sets friendly name to em-session9."""
    client = _make_client()
    rc = cmd_em(client, "a1b2c3d4", "session9")
    assert rc == 0
    client.update_friendly_name.assert_called_once_with("a1b2c3d4", "em-session9")
    out = capsys.readouterr().out
    assert "Name set: em-session9" in out


def test_name_set_without_suffix(capsys):
    """sm em (no arg) sets friendly name to em."""
    client = _make_client()
    rc = cmd_em(client, "a1b2c3d4", None)
    assert rc == 0
    client.update_friendly_name.assert_called_once_with("a1b2c3d4", "em")
    out = capsys.readouterr().out
    assert "Name set: em " in out


def test_name_validation_invalid_chars(capsys):
    """name_suffix with invalid chars triggers validate_friendly_name error, exit 1."""
    client = _make_client()
    rc = cmd_em(client, "a1b2c3d4", "bad name!")
    assert rc == 1
    client.update_friendly_name.assert_not_called()
    err = capsys.readouterr().err
    assert "Error:" in err


def test_self_context_monitoring_enabled(capsys):
    """set_context_monitor called with session_id as both target and notify target."""
    client = _make_client()
    rc = cmd_em(client, "a1b2c3d4", "test")
    assert rc == 0
    # First call should be for self
    first_call = client.set_context_monitor.call_args_list[0]
    assert first_call == call(
        "a1b2c3d4",
        enabled=True,
        requester_session_id="a1b2c3d4",
        notify_session_id="a1b2c3d4",
    )
    out = capsys.readouterr().out
    assert "Context monitoring: enabled" in out


def test_children_auto_registered_context_monitor():
    """set_context_monitor called once per child with child['id']."""
    child1 = _make_child("b2c3d4e5", "scout-1465")
    child2 = _make_child("c3d4e5f6", "engineer-1465")
    client = _make_client(children_data={"children": [child1, child2]})
    rc = cmd_em(client, "a1b2c3d4", "test")
    assert rc == 0
    # First call is self; next two are children
    calls = client.set_context_monitor.call_args_list
    child_calls = calls[1:]
    child_ids = [c[0][0] for c in child_calls]
    assert "b2c3d4e5" in child_ids
    assert "c3d4e5f6" in child_ids


def test_children_auto_registered_remind():
    """register_remind called with (child_id, 180, 300) for each child."""
    child1 = _make_child("b2c3d4e5", "scout-1465")
    child2 = _make_child("c3d4e5f6", "engineer-1465")
    client = _make_client(children_data={"children": [child1, child2]})
    rc = cmd_em(client, "a1b2c3d4", "test")
    assert rc == 0
    expected_calls = [
        call("b2c3d4e5", soft_threshold=180, hard_threshold=300),
        call("c3d4e5f6", soft_threshold=180, hard_threshold=300),
    ]
    client.register_remind.assert_has_calls(expected_calls, any_order=True)


def test_no_children_output(capsys):
    """When list_children returns empty, output includes 'No existing children found'."""
    client = _make_client(children_data={"children": []})
    rc = cmd_em(client, "a1b2c3d4", "test")
    assert rc == 0
    out = capsys.readouterr().out
    assert "No existing children found" in out


def test_partial_child_failure(capsys):
    """One child's context monitor fails; continues with others, reports correct counts."""
    child1 = _make_child("b2c3d4e5", "scout-1465")
    child2 = _make_child("c3d4e5f6", "engineer-1465")
    client = _make_client(children_data={"children": [child1, child2]})

    # Self context monitor: success; child1: success; child2: failure
    client.set_context_monitor.side_effect = [
        (None, True, False),   # self
        (None, True, False),   # child1 success
        (None, False, False),  # child2 failure
    ]
    rc = cmd_em(client, "a1b2c3d4", "test")
    assert rc == 0
    out = capsys.readouterr().out
    assert "1 succeeded, 1 failed" in out


def test_child_remind_failure_is_warning(capsys):
    """register_remind returning None is treated as warning/continue, not exit 2."""
    child1 = _make_child("b2c3d4e5", "scout-1465")
    client = _make_client(
        children_data={"children": [child1]},
        remind_result=None,
    )
    rc = cmd_em(client, "a1b2c3d4", "test")
    assert rc == 0  # Not exit 2
    out = capsys.readouterr().out
    assert "remind registration failed" in out
    assert "0 succeeded, 1 failed" in out


def test_no_session_id(capsys):
    """Exit 2 with error message when session_id is None."""
    client = _make_client()
    rc = cmd_em(client, None, "test")
    assert rc == 2
    err = capsys.readouterr().err
    assert "CLAUDE_SESSION_MANAGER_ID" in err
    client.update_friendly_name.assert_not_called()


def test_server_unavailable_on_name_set(capsys):
    """Exit 2 when name set returns unavailable=True."""
    client = _make_client(name_result=(False, True))
    rc = cmd_em(client, "a1b2c3d4", "test")
    assert rc == 2
    err = capsys.readouterr().err
    assert "unavailable" in err.lower()


def test_server_unavailable_on_children_list(capsys):
    """Exit 2 when list_children returns None."""
    client = _make_client(children_data=None)
    rc = cmd_em(client, "a1b2c3d4", "test")
    assert rc == 2
    err = capsys.readouterr().err
    assert "unavailable" in err.lower()


def test_output_format_with_children(capsys):
    """Verify printed summary format matches spec."""
    child1 = _make_child("b2c3d4e5", "scout-1465")
    child2 = _make_child("c3d4e5f6", "engineer-1465")
    client = _make_client(children_data={"children": [child1, child2]})
    rc = cmd_em(client, "a1b2c3d4", "session9")
    assert rc == 0
    out = capsys.readouterr().out
    assert "EM pre-flight complete:" in out
    assert "Name set: em-session9 (a1b2c3d4)" in out
    assert "Context monitoring: enabled (notifications → self)" in out
    assert "Children processed: 2 (2 succeeded, 0 failed)" in out
    assert "scout-1465 (b2c3d4e5) → context monitoring enabled; remind registered (soft=180s, hard=300s)" in out
    assert "engineer-1465 (c3d4e5f6) → context monitoring enabled; remind registered (soft=180s, hard=300s)" in out


# ---------------------------------------------------------------------------
# Tests for set_em_role() call from cmd_em (#256)
# ---------------------------------------------------------------------------


def test_cmd_em_calls_set_em_role(capsys):
    """cmd_em calls client.set_em_role(session_id) to register EM flag server-side."""
    client = _make_client()
    rc = cmd_em(client, "a1b2c3d4", "session9")
    assert rc == 0
    client.set_em_role.assert_called_once_with("a1b2c3d4")
    out = capsys.readouterr().out
    assert "EM role: registered" in out


def test_cmd_em_set_em_role_unavailable_returns_exit2(capsys):
    """set_em_role returns (False, True) → cmd_em exits 2 with unavailable error."""
    client = _make_client(em_role_result=(False, True))
    rc = cmd_em(client, "a1b2c3d4", "session9")
    assert rc == 2
    err = capsys.readouterr().err
    assert "unavailable" in err.lower()


def test_cmd_em_set_em_role_api_failure_warns_and_continues(capsys):
    """set_em_role returns (False, False) → warning printed, execution continues (exit 0)."""
    client = _make_client(em_role_result=(False, False))
    rc = cmd_em(client, "a1b2c3d4", "session9")
    assert rc == 0
    out = capsys.readouterr().out
    assert "Warning: Failed to register EM role" in out
    # Execution continued — context monitoring step should still have been called
    client.set_context_monitor.assert_called()
