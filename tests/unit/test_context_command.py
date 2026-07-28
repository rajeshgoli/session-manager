from unittest.mock import Mock

from src.cli.commands import cmd_context, cmd_context_monitor


def _snapshot(**overrides):
    payload = {
        "session_id": "abc12345",
        "friendly_name": "agent-1",
        "provider": "claude",
        "used_percentage": 43,
        "total_input_tokens": 86_214,
        "sampled_at": "2026-07-28T10:00:00",
        "state": "normal",
        "warning_percentage": 50,
        "critical_percentage": 65,
        "context_monitor_enabled": True,
        "notify_session_id": "abc12345",
        "compaction_active": False,
        "last_handoff_path": "specs/handoff.md",
    }
    payload.update(overrides)
    return payload


def test_context_default_prints_terse_percentage(capsys):
    client = Mock()
    client.get_context_snapshot.return_value = _snapshot()

    rc = cmd_context(client, "abc12345")

    assert rc == 0
    assert capsys.readouterr().out == "43%\n"
    client.get_context_snapshot.assert_called_once_with("abc12345")


def test_context_explicit_target_resolves_friendly_name(capsys):
    client = Mock()
    client.get_session.return_value = None
    client.list_sessions.return_value = [
        {"id": "abc12345", "friendly_name": "agent-1", "aliases": []}
    ]
    client.get_context_snapshot.return_value = _snapshot()

    rc = cmd_context(client, None, target="agent-1")

    assert rc == 0
    assert capsys.readouterr().out == "43%\n"
    client.get_context_snapshot.assert_called_once_with("abc12345")


def test_context_default_prints_unknown_without_sample(capsys):
    client = Mock()
    client.get_context_snapshot.return_value = _snapshot(used_percentage=None)

    rc = cmd_context(client, "abc12345")

    assert rc == 0
    assert capsys.readouterr().out == "unknown\n"


def test_context_details_prints_operational_snapshot(capsys):
    client = Mock()
    client.get_context_snapshot.return_value = _snapshot()

    rc = cmd_context(client, "abc12345", details=True)

    assert rc == 0
    output = capsys.readouterr().out
    assert "Context: 43% (86,214 tokens)" in output
    assert "State: normal (warning 50%, critical 65%)" in output
    assert "Monitor: enabled, alerts -> self" in output
    assert "Compaction: not active" in output
    assert "Last handoff: specs/handoff.md" in output
    assert "Session: agent-1 [abc12345] claude" in output


def test_context_json_prints_payload(capsys):
    client = Mock()
    client.get_context_snapshot.return_value = _snapshot()

    rc = cmd_context(client, "abc12345", json_output=True)

    assert rc == 0
    output = capsys.readouterr().out
    assert '"session_id": "abc12345"' in output
    assert '"used_percentage": 43' in output


def test_context_requires_target_without_session_context(capsys):
    client = Mock()

    rc = cmd_context(client, None)

    assert rc == 2
    assert "requires a managed session or explicit session target" in capsys.readouterr().err
    client.get_context_snapshot.assert_not_called()


def test_context_monitor_enable_reports_codex_fyi_contract(capsys):
    client = Mock()
    client.set_context_monitor.return_value = (
        {"status": "ok", "enabled": False},
        True,
        False,
    )

    rc = cmd_context_monitor(client, "abc12345", "enable", None)

    assert rc == 0
    assert capsys.readouterr().out == (
        "Context monitoring is FYI only for Codex agents; "
        "they manage compaction inline.\n"
    )
