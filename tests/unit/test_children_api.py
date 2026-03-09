from __future__ import annotations

from fastapi.testclient import TestClient

from src.models import Session, SessionStatus
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
        status=SessionStatus.IDLE,
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
    assert payload[0]["status"] == "idle"
    assert payload[0]["activity_state"] == "working"
