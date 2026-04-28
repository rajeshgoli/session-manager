from __future__ import annotations

import json
import os
from unittest.mock import AsyncMock, Mock, patch

import pytest
from fastapi.testclient import TestClient

from src.cli.commands import cmd_lookup, cmd_register, cmd_roster, cmd_unregister, resolve_session_id
from src.cli.main import main
from src.models import Session, SessionStatus
from src.output_monitor import OutputMonitor
from src.server import create_app
from src.session_manager import SessionManager


def _manager(tmp_path) -> SessionManager:
    return SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={},
    )


def _session(session_id: str, tmp_path) -> Session:
    return Session(
        id=session_id,
        name=f"claude-{session_id}",
        working_dir=str(tmp_path),
        tmux_session=f"claude-{session_id}",
        provider="claude",
        log_file=str(tmp_path / f"{session_id}.log"),
        status=SessionStatus.RUNNING,
    )


def test_register_role_persists_and_exposes_aliases(tmp_path):
    manager = _manager(tmp_path)
    session = _session("role1234", tmp_path)
    manager.sessions[session.id] = session

    registration = manager.register_agent_role(session.id, "Reviewer")

    assert registration.role == "reviewer"
    assert manager.get_session_aliases(session.id) == ["reviewer"]
    state_data = json.loads((tmp_path / "sessions.json").read_text())
    assert state_data["agent_registrations"] == [
        {
            "role": "reviewer",
            "session_id": session.id,
            "created_at": registration.created_at.isoformat(),
        }
    ]


def test_registering_maintainer_updates_compat_field(tmp_path):
    manager = _manager(tmp_path)
    session = _session("maint123", tmp_path)
    manager.sessions[session.id] = session

    manager.register_agent_role(session.id, "maintainer")

    state_data = json.loads((tmp_path / "sessions.json").read_text())
    assert state_data["maintainer_session_id"] == session.id
    assert manager.get_maintainer_session().id == session.id


def test_lookup_prunes_stopped_registration(tmp_path):
    manager = _manager(tmp_path)
    session = _session("stop1234", tmp_path)
    manager.sessions[session.id] = session
    manager.register_agent_role(session.id, "reviewer")

    session.status = SessionStatus.STOPPED

    assert manager.lookup_agent_registration("reviewer") is None
    assert manager.list_agent_registrations() == []
    state_data = json.loads((tmp_path / "sessions.json").read_text())
    assert state_data["agent_registrations"] == []


def test_register_role_reparents_live_children_from_stopped_prior_holder(tmp_path):
    manager = _manager(tmp_path)
    old_owner = _session("chiefold", tmp_path)
    new_owner = _session("chiefnew", tmp_path)
    child_live = _session("child001", tmp_path)
    child_stopped = _session("child002", tmp_path)
    child_live.parent_session_id = old_owner.id
    child_stopped.parent_session_id = old_owner.id
    child_stopped.status = SessionStatus.STOPPED
    manager.sessions[old_owner.id] = old_owner
    manager.sessions[new_owner.id] = new_owner
    manager.sessions[child_live.id] = child_live
    manager.sessions[child_stopped.id] = child_stopped

    manager.register_agent_role(old_owner.id, "chief-scientist")
    old_owner.status = SessionStatus.STOPPED

    manager.register_agent_role(new_owner.id, "chief-scientist")

    assert child_live.parent_session_id == new_owner.id
    assert child_stopped.parent_session_id == old_owner.id
    assert manager.lookup_agent_registration("chief-scientist").session_id == new_owner.id


def test_register_role_reparents_from_last_dead_holder_after_registration_pruned(tmp_path):
    manager = _manager(tmp_path)
    old_owner = _session("chiefold", tmp_path)
    new_owner = _session("chiefnew", tmp_path)
    child_live = _session("child001", tmp_path)
    child_live.parent_session_id = old_owner.id
    manager.sessions[old_owner.id] = old_owner
    manager.sessions[new_owner.id] = new_owner
    manager.sessions[child_live.id] = child_live

    manager.register_agent_role(old_owner.id, "chief-scientist")
    old_owner.status = SessionStatus.STOPPED

    # Simulate the normal prune path that removes dead live-role registrations.
    assert manager.lookup_agent_registration("chief-scientist") is None

    manager.register_agent_role(new_owner.id, "chief-scientist")

    assert child_live.parent_session_id == new_owner.id
    state_data = json.loads((tmp_path / "sessions.json").read_text())
    assert state_data["agent_role_last_session_ids"]["chief-scientist"] == new_owner.id


def test_kill_session_unregisters_roles(tmp_path):
    manager = _manager(tmp_path)
    session = _session("kill1234", tmp_path)
    manager.sessions[session.id] = session
    manager.register_agent_role(session.id, "reviewer")

    assert manager.kill_session(session.id) is True
    assert manager.lookup_agent_registration("reviewer") is None

    state_data = json.loads((tmp_path / "sessions.json").read_text())
    assert state_data["agent_registrations"] == []


def test_output_monitor_cleanup_unregisters_roles(tmp_path):
    manager = _manager(tmp_path)
    session = _session("dead1234", tmp_path)
    manager.sessions[session.id] = session
    manager.register_agent_role(session.id, "reviewer")

    monitor = OutputMonitor()
    monitor.set_session_manager(manager)

    import asyncio

    asyncio.run(monitor.cleanup_session(session))

    assert session.id not in manager.sessions
    assert manager.lookup_agent_registration("reviewer") is None

    state_data = json.loads((tmp_path / "sessions.json").read_text())
    assert state_data["agent_registrations"] == []


def test_registry_endpoints_register_lookup_and_roster(tmp_path):
    manager = _manager(tmp_path)
    manager.queue_provider_native_rename = AsyncMock(return_value=True)
    session = _session("role1234", tmp_path)
    manager.sessions[session.id] = session
    client = TestClient(create_app(session_manager=manager))

    register_response = client.post(
        f"/sessions/{session.id}/registry",
        json={"requester_session_id": session.id, "role": "SM Maintainer"},
    )

    assert register_response.status_code == 200
    assert register_response.json()["role"] == "sm-maintainer"

    lookup_response = client.get("/registry/sm-maintainer")
    assert lookup_response.status_code == 200
    assert lookup_response.json()["session_id"] == session.id

    roster_response = client.get("/registry")
    assert roster_response.status_code == 200
    assert roster_response.json()["registrations"][0]["role"] == "sm-maintainer"

    sessions_response = client.get("/sessions")
    assert sessions_response.status_code == 200
    payload = sessions_response.json()["sessions"][0]
    assert payload["aliases"] == ["sm-maintainer"]
    manager.queue_provider_native_rename.assert_awaited_once_with(session, "sm-maintainer")


def test_register_route_rejects_live_conflict(tmp_path):
    manager = _manager(tmp_path)
    reviewer = _session("review01", tmp_path)
    other = _session("other001", tmp_path)
    manager.sessions[reviewer.id] = reviewer
    manager.sessions[other.id] = other
    manager.register_agent_role(reviewer.id, "reviewer")
    client = TestClient(create_app(session_manager=manager))

    response = client.post(
        f"/sessions/{other.id}/registry",
        json={"requester_session_id": other.id, "role": "reviewer"},
    )

    assert response.status_code == 409
    assert "already registered" in response.json()["detail"]


def test_unregister_route_requires_owner(tmp_path):
    manager = _manager(tmp_path)
    owner = _session("owner001", tmp_path)
    other = _session("other001", tmp_path)
    manager.sessions[owner.id] = owner
    manager.sessions[other.id] = other
    manager.register_agent_role(owner.id, "reviewer")
    client = TestClient(create_app(session_manager=manager))

    response = client.request(
        "DELETE",
        f"/sessions/{other.id}/registry",
        json={"requester_session_id": other.id, "role": "reviewer"},
    )

    assert response.status_code == 409
    assert "not owned" in response.json()["detail"]


def test_resolve_session_id_matches_generic_registry_alias():
    client = Mock()
    client.get_session.return_value = None
    client.list_sessions.return_value = [
        {"id": "role1234", "friendly_name": "codex-ops", "aliases": ["reviewer", "sm-maintainer"]},
    ]

    resolved_id, resolved_session = resolve_session_id(client, "reviewer")

    assert resolved_id == "role1234"
    assert resolved_session["friendly_name"] == "codex-ops"


def test_cmd_register_lookup_unregister_and_roster(capsys):
    client = Mock()
    client.register_role.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {"role": "reviewer", "session_id": "sess1234"},
    }
    client.lookup_role.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {"role": "reviewer", "session_id": "sess1234"},
    }
    client.unregister_role.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {"role": "reviewer"},
    }
    client.list_registry.return_value = [
        {
            "role": "reviewer",
            "session_id": "sess1234",
            "friendly_name": "engineer-1",
            "provider": "codex-fork",
            "activity_state": "idle",
        }
    ]

    assert cmd_register(client, "sess1234", "Reviewer") == 0
    assert "reviewer -> sess1234" in capsys.readouterr().out

    assert cmd_lookup(client, "reviewer") == 0
    assert capsys.readouterr().out.strip() == "sess1234"

    assert cmd_unregister(client, "sess1234", "reviewer") == 0
    assert "Unregistered: reviewer" in capsys.readouterr().out

    assert cmd_roster(client) == 0
    roster_output = capsys.readouterr().out
    assert "Role" in roster_output
    assert "reviewer" in roster_output
    assert "sess1234" in roster_output


def test_cmd_lookup_falls_back_to_exact_session_name(capsys):
    client = Mock()
    client.lookup_role.return_value = {
        "ok": False,
        "unavailable": False,
        "detail": "Role not registered",
    }
    client.get_session.return_value = None
    client.list_sessions.return_value = [
        {"id": "sess1234", "name": "claude-sess1234", "friendly_name": "super-orchestrator", "aliases": []},
    ]

    assert cmd_lookup(client, "super-orchestrator") == 0
    assert capsys.readouterr().out.strip() == "sess1234"


def test_cmd_lookup_does_not_fallback_on_registry_error(capsys):
    client = Mock()
    client.lookup_role.return_value = {
        "ok": False,
        "unavailable": False,
        "status_code": 500,
        "detail": "registry exploded",
    }

    assert cmd_lookup(client, "super-orchestrator") == 1
    captured = capsys.readouterr()
    assert captured.out == ""
    assert "registry exploded" in captured.err
    client.get_session.assert_not_called()
    client.list_sessions.assert_not_called()


def test_cmd_lookup_rejects_ambiguous_exact_session_name(capsys):
    client = Mock()
    client.lookup_role.return_value = {
        "ok": False,
        "unavailable": False,
        "detail": "Role not registered",
    }
    client.get_session.return_value = None
    client.list_sessions.return_value = [
        {"id": "sess1234", "name": "claude-sess1234", "friendly_name": "super-orchestrator", "aliases": []},
        {"id": "other123", "name": "claude-other123", "friendly_name": "super-orchestrator", "aliases": []},
    ]

    assert cmd_lookup(client, "super-orchestrator") == 1
    captured = capsys.readouterr()
    assert captured.out == ""
    assert "Multiple sessions match 'super-orchestrator'" in captured.err
    assert "super-orchestrator (sess1234)" in captured.err
    assert "super-orchestrator (other123)" in captured.err


def test_cmd_lookup_falls_back_to_unique_session_name_fragment(capsys):
    client = Mock()
    client.lookup_role.return_value = {
        "ok": False,
        "unavailable": False,
        "detail": "Role not registered",
    }
    client.get_session.return_value = None
    client.list_sessions.return_value = [
        {"id": "sess1234", "name": "claude-sess1234", "friendly_name": "em-super-orchestrator-3047", "aliases": []},
        {"id": "other123", "name": "claude-other123", "friendly_name": "reviewer-3047", "aliases": []},
    ]

    assert cmd_lookup(client, "super-orchestrator") == 0
    assert capsys.readouterr().out.strip() == "sess1234"


def test_cmd_lookup_rejects_ambiguous_session_name_fragment(capsys):
    client = Mock()
    client.lookup_role.return_value = {
        "ok": False,
        "unavailable": False,
        "detail": "Role not registered",
    }
    client.get_session.return_value = None
    client.list_sessions.return_value = [
        {"id": "sess1234", "name": "claude-sess1234", "friendly_name": "em-super-orchestrator-3047", "aliases": []},
        {"id": "other123", "name": "claude-other123", "friendly_name": "backup-super-orchestrator", "aliases": []},
    ]

    assert cmd_lookup(client, "super-orchestrator") == 1
    captured = capsys.readouterr()
    assert captured.out == ""
    assert "Multiple sessions match 'super-orchestrator'" in captured.err
    assert "em-super-orchestrator-3047 (sess1234)" in captured.err
    assert "backup-super-orchestrator (other123)" in captured.err


def test_main_register_lookup_roster_dispatch():
    with patch.dict(os.environ, {"CLAUDE_SESSION_MANAGER_ID": "sess1234"}, clear=False):
        with patch("sys.argv", ["sm", "register", "reviewer"]):
            with patch("src.cli.main.commands.cmd_register", return_value=0) as mock_register:
                with pytest.raises(SystemExit) as exc_info:
                    main()
    assert exc_info.value.code == 0
    mock_register.assert_called_once()

    with patch.dict(os.environ, {"CLAUDE_SESSION_MANAGER_ID": ""}, clear=False):
        with patch("sys.argv", ["sm", "lookup", "reviewer"]):
            with patch("src.cli.main.commands.cmd_lookup", return_value=0) as mock_lookup:
                with pytest.raises(SystemExit) as exc_info:
                    main()
    assert exc_info.value.code == 0
    mock_lookup.assert_called_once()

    with patch.dict(os.environ, {"CLAUDE_SESSION_MANAGER_ID": ""}, clear=False):
        with patch("sys.argv", ["sm", "roster"]):
            with patch("src.cli.main.commands.cmd_roster", return_value=0) as mock_roster:
                with pytest.raises(SystemExit) as exc_info:
                    main()
    assert exc_info.value.code == 0
    mock_roster.assert_called_once()
