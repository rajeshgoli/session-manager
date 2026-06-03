import pytest

from src.models import Session, SessionStatus
from src.session_manager import SessionManager


def _manager(tmp_path):
    return SessionManager(
        state_file=str(tmp_path / "sessions.json"),
        config={"nodes": {"registry": {"worker": {"ssh": "dev@example"}}}},
    )


def test_create_node_validation_rejects_inherited_remote_codex(tmp_path):
    manager = _manager(tmp_path)
    parent = Session(id="parent1", node="worker")
    manager.sessions[parent.id] = parent

    node, error = manager.validate_create_node_provider(
        "codex-fork",
        parent_session_id=parent.id,
    )

    assert node == "worker"
    assert error == "Remote placement is Claude-only in this phase (provider=codex-fork)"


def test_create_node_validation_allows_inherited_remote_claude(tmp_path):
    manager = _manager(tmp_path)
    parent = Session(id="parent1", node="worker")
    manager.sessions[parent.id] = parent

    node, error = manager.validate_create_node_provider(
        "claude",
        parent_session_id=parent.id,
    )

    assert node == "worker"
    assert error is None


@pytest.mark.asyncio
async def test_remote_node_transport_failure_does_not_stop_session(tmp_path):
    manager = _manager(tmp_path)
    session = Session(id="remote1", node="worker", provider="claude")
    session.status = SessionStatus.RUNNING
    manager.sessions[session.id] = session

    class FailingTmux:
        def session_exists(self, _tmux_session):
            raise RuntimeError("Node worker unreachable: ssh transport failed")

    manager.tmux = FailingTmux()

    marked = await manager._mark_tmux_runtime_missing_if_absent(session)

    assert marked is False
    assert session.status == SessionStatus.RUNNING
    assert manager.is_session_node_unreachable(session.id)


def test_hydrate_preserves_remote_session_when_node_unreachable(tmp_path):
    manager = _manager(tmp_path)
    session = Session(id="remote2", node="worker", provider="claude")
    session.status = SessionStatus.RUNNING

    class FailingTmux:
        def session_exists(self, _tmux_session):
            raise RuntimeError("Node worker unreachable: ssh transport failed")

    manager.tmux = FailingTmux()

    manager._hydrate_state_from_data({"sessions": [session.to_dict()]})

    assert "remote2" in manager.sessions
    assert manager.sessions["remote2"].status == SessionStatus.RUNNING
    assert manager.is_session_node_unreachable("remote2")


def test_kill_remote_session_unreachable_does_not_stop_session(tmp_path):
    manager = _manager(tmp_path)
    session = Session(id="remote3", node="worker", provider="claude")
    session.status = SessionStatus.RUNNING
    manager.sessions[session.id] = session

    class FailingTmux:
        def kill_session(self, _tmux_session):
            raise RuntimeError("Node worker unreachable: ssh transport failed")

    manager.tmux = FailingTmux()

    assert manager.kill_session(session.id) is False
    assert session.status == SessionStatus.RUNNING
    assert manager.is_session_node_unreachable(session.id)


@pytest.mark.asyncio
async def test_restore_remote_session_unreachable_returns_error(tmp_path):
    manager = _manager(tmp_path)
    session = Session(id="remote4", node="worker", provider="claude")
    session.status = SessionStatus.RUNNING
    manager.sessions[session.id] = session

    class FailingTmux:
        def session_exists(self, _tmux_session):
            raise RuntimeError("Node worker unreachable: ssh transport failed")

    manager.tmux = FailingTmux()

    success, returned, error = await manager.restore_session(session.id)

    assert success is False
    assert returned is session
    assert error == "Node worker unreachable"
    assert session.status == SessionStatus.RUNNING
    assert manager.is_session_node_unreachable(session.id)
