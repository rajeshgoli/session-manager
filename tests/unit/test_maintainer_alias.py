from __future__ import annotations

import asyncio
import json
from unittest.mock import AsyncMock, Mock

from fastapi.testclient import TestClient

from src.cli.commands import cmd_maintainer, cmd_send, resolve_session_id
from src.models import Session, SessionStatus
from src.server import create_app
from src.session_manager import SessionManager


def _manager(tmp_path, config=None) -> SessionManager:
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config=config or {},
    )
    manager.tmux = Mock()
    manager.tmux.list_sessions.return_value = []
    manager.tmux.session_exists.return_value = False
    manager.tmux.set_status_bar.return_value = True
    return manager


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


def test_set_maintainer_persists_and_roundtrips(tmp_path):
    manager = _manager(tmp_path)
    session = _session("maint123", tmp_path)
    manager.sessions[session.id] = session

    assert manager.set_maintainer_session(session.id) is True
    assert manager.get_session_aliases(session.id) == ["maintainer"]
    state_data = json.loads((tmp_path / "sessions.json").read_text())
    assert state_data["maintainer_session_id"] == session.id


def test_list_sessions_exposes_maintainer_alias(tmp_path):
    manager = _manager(tmp_path)
    session = _session("maint123", tmp_path)
    manager.sessions[session.id] = session
    manager.set_maintainer_session(session.id)

    client = TestClient(create_app(session_manager=manager))
    response = client.get("/sessions")

    assert response.status_code == 200
    payload = response.json()["sessions"][0]
    assert payload["aliases"] == ["maintainer"]
    assert payload["is_maintainer"] is True


def test_put_maintainer_requires_self_auth(tmp_path):
    manager = _manager(tmp_path)
    session = _session("maint123", tmp_path)
    manager.sessions[session.id] = session
    client = TestClient(create_app(session_manager=manager))

    response = client.put(f"/sessions/{session.id}/maintainer", json={"requester_session_id": "other"})

    assert response.status_code == 400
    assert "self-directed" in response.json()["detail"]


def test_put_maintainer_requires_session_manager():
    client = TestClient(create_app(session_manager=None))

    response = client.put("/sessions/maint123/maintainer", json={"requester_session_id": "maint123"})

    assert response.status_code == 503
    assert "not configured" in response.json()["detail"]


def test_resolve_session_id_matches_alias():
    client = Mock()
    client.get_session.return_value = None
    client.list_sessions.return_value = [
        {"id": "maint123", "friendly_name": "codex-ops", "aliases": ["maintainer"]},
    ]

    resolved_id, resolved_session = resolve_session_id(client, "maintainer")

    assert resolved_id == "maint123"
    assert resolved_session["friendly_name"] == "codex-ops"


def test_resolve_session_id_prefers_alias_over_friendly_name():
    client = Mock()
    client.get_session.return_value = None
    client.list_sessions.return_value = [
        {"id": "shadow123", "friendly_name": "maintainer", "aliases": []},
        {"id": "maint123", "friendly_name": "codex-ops", "aliases": ["maintainer"]},
    ]

    resolved_id, _ = resolve_session_id(client, "maintainer")

    assert resolved_id == "maint123"


def test_cmd_maintainer_registers_alias(capsys):
    client = Mock()
    client.set_maintainer.return_value = (True, False)

    rc = cmd_maintainer(client, "maint123")

    assert rc == 0
    client.set_maintainer.assert_called_once_with("maint123")
    assert "maintainer -> maint123" in capsys.readouterr().out


def test_cmd_maintainer_clear(capsys):
    client = Mock()
    client.clear_maintainer.return_value = (True, False)

    rc = cmd_maintainer(client, "maint123", clear=True)

    assert rc == 0
    client.clear_maintainer.assert_called_once_with("maint123")
    assert "cleared" in capsys.readouterr().out


def test_ensure_maintainer_session_prefers_codex_and_registers_alias(tmp_path):
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    manager = _manager(tmp_path)
    manager.maintainer_working_dir = str(repo_dir)

    async def _fake_create_session_common(**kwargs):
        session = Session(
            id="maint001",
            working_dir=kwargs["working_dir"],
            provider=kwargs["provider"],
            friendly_name=kwargs["friendly_name"],
            log_file=str(tmp_path / "maint001.log"),
            status=SessionStatus.RUNNING,
        )
        manager.sessions[session.id] = session
        _fake_create_session_common.kwargs = kwargs
        return session

    manager._provider_entrypoint_available = Mock(return_value=True)
    manager._create_session_common = AsyncMock(side_effect=_fake_create_session_common)

    session, created = asyncio.run(manager.ensure_maintainer_session())

    assert created is True
    assert session.id == "maint001"
    assert session.provider == "codex"
    assert session.role == "maintainer"
    assert session.auto_bootstrapped_role == "maintainer"
    assert manager.lookup_agent_registration("maintainer").session_id == session.id
    assert _fake_create_session_common.kwargs["working_dir"] == str(repo_dir)
    assert "sm send maintainer" in _fake_create_session_common.kwargs["initial_prompt"]


def test_ensure_maintainer_session_falls_back_to_claude(tmp_path):
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    manager = _manager(tmp_path)
    manager.maintainer_working_dir = str(repo_dir)

    async def _fake_create_session_common(**kwargs):
        session = Session(
            id="maint002",
            working_dir=kwargs["working_dir"],
            provider=kwargs["provider"],
            friendly_name=kwargs["friendly_name"],
            log_file=str(tmp_path / "maint002.log"),
            status=SessionStatus.RUNNING,
        )
        manager.sessions[session.id] = session
        return session

    manager._provider_entrypoint_available = Mock(side_effect=lambda provider: provider != "codex")
    manager._create_session_common = AsyncMock(side_effect=_fake_create_session_common)

    session, created = asyncio.run(manager.ensure_maintainer_session())

    assert created is True
    assert session.provider == "claude"
    manager._create_session_common.assert_awaited_once()


def test_post_ensure_maintainer_bootstraps_session(tmp_path):
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    manager = _manager(tmp_path)
    manager.maintainer_working_dir = str(repo_dir)

    async def _fake_create_session_common(**kwargs):
        session = Session(
            id="maint003",
            working_dir=kwargs["working_dir"],
            provider=kwargs["provider"],
            friendly_name=kwargs["friendly_name"],
            log_file=str(tmp_path / "maint003.log"),
            status=SessionStatus.RUNNING,
        )
        manager.sessions[session.id] = session
        return session

    manager._provider_entrypoint_available = Mock(return_value=True)
    manager._create_session_common = AsyncMock(side_effect=_fake_create_session_common)
    client = TestClient(create_app(session_manager=manager))

    response = client.post("/maintainer/ensure", json={})

    assert response.status_code == 200
    payload = response.json()
    assert payload["created"] is True
    assert payload["session"]["id"] == "maint003"
    assert payload["session"]["aliases"] == ["maintainer"]
    assert payload["session"]["provider"] == "codex"


def test_maintainer_fallback_uses_bootstrap_prompt_file(tmp_path):
    repo_dir = tmp_path / "repo"
    prompt_file = repo_dir / "docs" / "product" / "maintainer_bootstrap.md"
    prompt_file.parent.mkdir(parents=True)
    prompt_file.write_text("Read docs/product/lessons.md first.\nAct as {role} in {working_dir}.")
    manager = _manager(
        tmp_path,
        config={
            "maintainer_agent": {
                "working_dir": str(repo_dir),
                "friendly_name": "maintainer",
                "preferred_providers": ["claude"],
                "bootstrap_prompt_file": "docs/product/maintainer_bootstrap.md",
            }
        },
    )

    async def _fake_create_session_common(**kwargs):
        session = Session(
            id="maint004",
            working_dir=kwargs["working_dir"],
            provider=kwargs["provider"],
            friendly_name=kwargs["friendly_name"],
            log_file=str(tmp_path / "maint004.log"),
            status=SessionStatus.RUNNING,
        )
        manager.sessions[session.id] = session
        _fake_create_session_common.kwargs = kwargs
        return session

    manager._provider_entrypoint_available = Mock(return_value=True)
    manager._create_session_common = AsyncMock(side_effect=_fake_create_session_common)

    session, created = asyncio.run(manager.ensure_maintainer_session())

    assert created is True
    assert session.id == "maint004"
    assert manager.get_service_role_bootstrap_spec("maintainer")["bootstrap_prompt_file"] == "docs/product/maintainer_bootstrap.md"
    assert _fake_create_session_common.kwargs["initial_prompt"] == (
        f"Read docs/product/lessons.md first.\nAct as maintainer in {repo_dir}."
    )


def test_manual_maintainer_registration_does_not_mark_auto_bootstrapped_role(tmp_path):
    manager = _manager(tmp_path)
    session = _session("maint-manual", tmp_path)
    manager.sessions[session.id] = session

    manager.set_maintainer_session(session.id)

    assert session.auto_bootstrapped_role is None


def test_ensure_service_role_session_uses_prompt_file_and_registers_alias(tmp_path):
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    prompt_file = tmp_path / "chief-scientist.md"
    prompt_file.write_text("Act as {role} in {working_dir}.")
    manager = _manager(
        tmp_path,
        config={
            "service_roles": {
                "chief-scientist": {
                    "auto_bootstrap": True,
                    "working_dir": str(repo_dir),
                    "friendly_name": "chief-scientist",
                    "preferred_providers": ["claude"],
                    "bootstrap_prompt_file": str(prompt_file),
                }
            }
        },
    )

    async def _fake_create_session_common(**kwargs):
        session = Session(
            id="chief001",
            working_dir=kwargs["working_dir"],
            provider=kwargs["provider"],
            friendly_name=kwargs["friendly_name"],
            log_file=str(tmp_path / "chief001.log"),
            status=SessionStatus.RUNNING,
        )
        manager.sessions[session.id] = session
        _fake_create_session_common.kwargs = kwargs
        return session

    manager._provider_entrypoint_available = Mock(return_value=True)
    manager._create_session_common = AsyncMock(side_effect=_fake_create_session_common)

    session, created = asyncio.run(manager.ensure_role_session("chief-scientist"))

    assert created is True
    assert session.id == "chief001"
    assert session.provider == "claude"
    assert session.role == "chief-scientist"
    assert manager.lookup_agent_registration("chief-scientist").session_id == session.id
    assert _fake_create_session_common.kwargs["initial_prompt"] == (
        f"Act as chief-scientist in {repo_dir}."
    )


def test_post_ensure_role_bootstraps_generic_service_role(tmp_path):
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    manager = _manager(
        tmp_path,
        config={
            "service_roles": {
                "chief-scientist": {
                    "auto_bootstrap": True,
                    "working_dir": str(repo_dir),
                    "friendly_name": "chief-scientist",
                    "preferred_providers": ["claude"],
                    "bootstrap_prompt": "Act as {role} in {working_dir}.",
                }
            }
        },
    )

    async def _fake_create_session_common(**kwargs):
        session = Session(
            id="chief002",
            working_dir=kwargs["working_dir"],
            provider=kwargs["provider"],
            friendly_name=kwargs["friendly_name"],
            log_file=str(tmp_path / "chief002.log"),
            status=SessionStatus.RUNNING,
        )
        manager.sessions[session.id] = session
        return session

    manager._provider_entrypoint_available = Mock(return_value=True)
    manager._create_session_common = AsyncMock(side_effect=_fake_create_session_common)
    client = TestClient(create_app(session_manager=manager))

    response = client.post("/registry/chief-scientist/ensure", json={})

    assert response.status_code == 200
    payload = response.json()
    assert payload["created"] is True
    assert payload["session"]["id"] == "chief002"
    assert payload["session"]["aliases"] == ["chief-scientist"]
    assert payload["session"]["provider"] == "claude"


def test_cmd_send_bootstraps_maintainer_when_missing(capsys):
    client = Mock()
    client.get_session.return_value = None
    client.list_sessions.return_value = []
    client.session_id = "sender123"
    client.ensure_role.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "created": True,
            "session": {
                "id": "maint004",
                "friendly_name": "sm-maintainer",
                "name": "codex-maint004",
                "provider": "codex",
            },
        },
    }
    client.send_input.return_value = (True, False)

    rc = cmd_send(client, "maintainer", "bug report")

    assert rc == 0
    client.ensure_role.assert_called_once_with("maintainer", requester_session_id="sender123")
    client.send_input.assert_called_once()
    assert client.send_input.call_args[0][0] == "maint004"
    output = capsys.readouterr().out
    assert "Role bootstrapped: maintainer -> sm-maintainer (maint004) [codex]" in output
    assert "Input sent to sm-maintainer (maint004)" in output


def test_cmd_send_bootstraps_generic_role_when_missing(capsys):
    client = Mock()
    client.get_session.return_value = None
    client.list_sessions.return_value = []
    client.session_id = "sender123"
    client.ensure_role.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {
            "created": True,
            "session": {
                "id": "chief003",
                "friendly_name": "chief-scientist",
                "name": "claude-chief003",
                "provider": "claude",
            },
        },
    }
    client.send_input.return_value = (True, False)

    rc = cmd_send(client, "chief-scientist", "continue")

    assert rc == 0
    client.ensure_role.assert_called_once_with("chief-scientist", requester_session_id="sender123")
    assert client.send_input.call_args[0][0] == "chief003"
    output = capsys.readouterr().out
    assert "Role bootstrapped: chief-scientist -> chief-scientist (chief003) [claude]" in output
