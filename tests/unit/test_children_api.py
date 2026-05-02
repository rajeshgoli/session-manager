from __future__ import annotations

from fastapi.testclient import TestClient

from src.models import CompletionStatus, Session, SessionStatus
from src.server import create_app
from src.session_manager import SessionManager


def _manager(tmp_path) -> SessionManager:
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={},
    )
    return manager


def test_children_endpoint_includes_activity_state(tmp_path):
    manager = _manager(tmp_path)
    parent = Session(
        id="parent01",
        working_dir=str(tmp_path),
        provider="claude",
        status=SessionStatus.RUNNING,
    )
    child = Session(
        id="child001",
        working_dir=str(tmp_path),
        provider="codex-fork",
        parent_session_id=parent.id,
        status=SessionStatus.RUNNING,
        tmux_session="codex-fork-child001",
        tmux_socket_name="session-manager-test",
    )
    manager.sessions[parent.id] = parent
    manager.sessions[child.id] = child
    manager.codex_fork_lifecycle[child.id] = {
        "state": "running",
        "cause_event_type": "turn_diff",
        "updated_at": parent.last_activity.isoformat(),
    }

    client = TestClient(create_app(session_manager=manager))
    response = client.get(f"/sessions/{parent.id}/children")

    assert response.status_code == 200
    payload = response.json()["children"]
    assert len(payload) == 1
    assert payload[0]["id"] == child.id
    assert payload[0]["status"] == "running"
    assert payload[0]["activity_state"] == "working"
    assert payload[0]["tmux_session"] == "codex-fork-child001"
    assert payload[0]["tmux_socket_name"] == "session-manager-test"


def test_children_endpoint_hides_terminated_by_default(tmp_path):
    manager = _manager(tmp_path)
    parent = Session(
        id="parent01",
        working_dir=str(tmp_path),
        provider="claude",
        status=SessionStatus.RUNNING,
    )
    child = Session(
        id="child001",
        working_dir=str(tmp_path),
        provider="claude",
        parent_session_id=parent.id,
        status=SessionStatus.STOPPED,
        completion_status=CompletionStatus.KILLED,
    )
    manager.sessions[parent.id] = parent
    manager.sessions[child.id] = child

    client = TestClient(create_app(session_manager=manager))
    response = client.get(f"/sessions/{parent.id}/children")

    assert response.status_code == 200
    assert response.json()["children"] == []


def test_children_endpoint_includes_terminated_when_requested(tmp_path):
    manager = _manager(tmp_path)
    parent = Session(
        id="parent01",
        working_dir=str(tmp_path),
        provider="claude",
        status=SessionStatus.RUNNING,
    )
    child = Session(
        id="child001",
        working_dir=str(tmp_path),
        provider="claude",
        parent_session_id=parent.id,
        status=SessionStatus.STOPPED,
        completion_status=CompletionStatus.KILLED,
    )
    manager.sessions[parent.id] = parent
    manager.sessions[child.id] = child

    client = TestClient(create_app(session_manager=manager))
    response = client.get(f"/sessions/{parent.id}/children?include_terminated=true")

    assert response.status_code == 200
    payload = response.json()["children"]
    assert len(payload) == 1
    assert payload[0]["id"] == child.id
    assert payload[0]["completion_status"] == "killed"
    assert payload[0]["activity_state"] == "stopped"


def test_children_endpoint_recursive_hides_terminated_descendants_by_default(tmp_path):
    manager = _manager(tmp_path)
    parent = Session(
        id="parent01",
        working_dir=str(tmp_path),
        provider="claude",
        status=SessionStatus.RUNNING,
    )
    child = Session(
        id="child001",
        working_dir=str(tmp_path),
        provider="claude",
        parent_session_id=parent.id,
        status=SessionStatus.RUNNING,
    )
    grandchild = Session(
        id="grand001",
        working_dir=str(tmp_path),
        provider="claude",
        parent_session_id=child.id,
        status=SessionStatus.STOPPED,
        completion_status=CompletionStatus.KILLED,
    )
    manager.sessions[parent.id] = parent
    manager.sessions[child.id] = child
    manager.sessions[grandchild.id] = grandchild

    client = TestClient(create_app(session_manager=manager))
    response = client.get(f"/sessions/{parent.id}/children?recursive=true")

    assert response.status_code == 200
    payload = response.json()["children"]
    assert [entry["id"] for entry in payload] == [child.id]


def test_children_endpoint_uses_cached_display_name_only(tmp_path):
    manager = _manager(tmp_path)
    parent = Session(
        id="parent01",
        working_dir=str(tmp_path),
        provider="claude",
        status=SessionStatus.RUNNING,
    )
    child = Session(
        id="child001",
        working_dir=str(tmp_path),
        provider="claude",
        parent_session_id=parent.id,
        status=SessionStatus.RUNNING,
        friendly_name="Cached Friendly Name",
        friendly_name_is_explicit=True,
    )
    manager.sessions[parent.id] = parent
    manager.sessions[child.id] = child

    def _fail_live_name(_: Session) -> str:
        raise AssertionError("children endpoint should not call live display-name resolution")

    manager.get_effective_session_name = _fail_live_name

    client = TestClient(create_app(session_manager=manager))
    response = client.get(f"/sessions/{parent.id}/children")

    assert response.status_code == 200
    payload = response.json()["children"]
    assert len(payload) == 1
    assert payload[0]["friendly_name"] == "Cached Friendly Name"
