from src.models import Session, SessionStatus
from src.session_manager import SessionManager


def test_codex_fork_attach_descriptor_preserves_waiting_state(tmp_path):
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={},
    )
    session = Session(
        id="fork1001",
        name="codex-fork-fork1001",
        provider="codex-fork",
        working_dir=str(tmp_path),
        tmux_session="codex-fork-fork1001",
        log_file=str(tmp_path / "logs" / "codex-fork-fork1001.log"),
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.codex_fork_runtime_owner[session.id] = "owner-session"
    manager.codex_fork_lifecycle[session.id] = {
        "state": "waiting_on_user_input",
        "cause_event_type": "request_user_input",
    }

    descriptor = manager.get_attach_descriptor(session.id)
    assert descriptor is not None
    assert descriptor["attach_supported"] is True
    assert descriptor["runtime_mode"] == "detached_runtime"
    assert descriptor["runtime_id"] == "codex-fork:fork1001"
    assert descriptor["runtime_owner"] == "owner-session"
    assert descriptor["lifecycle_state"] == "waiting_on_user_input"
    assert descriptor["lifecycle_cause"] == "request_user_input"


def test_codex_app_attach_descriptor_is_headless(tmp_path):
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={},
    )
    session = Session(
        id="app1001",
        name="codex-app-app1001",
        provider="codex-app",
        working_dir=str(tmp_path),
        tmux_session="",
        log_file="",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session

    descriptor = manager.get_attach_descriptor(session.id)
    assert descriptor is not None
    assert descriptor["attach_supported"] is False
    assert descriptor["runtime_mode"] == "headless"


def test_stopped_codex_fork_attach_descriptor_is_not_attachable(tmp_path):
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={},
    )
    session = Session(
        id="forkstopped",
        name="codex-fork-forkstopped",
        provider="codex-fork",
        working_dir=str(tmp_path),
        tmux_session="codex-fork-forkstopped",
        log_file="",
        status=SessionStatus.STOPPED,
    )
    manager.sessions[session.id] = session

    descriptor = manager.get_attach_descriptor(session.id)
    assert descriptor is not None
    assert descriptor["attach_supported"] is False
    assert descriptor["runtime_mode"] == "stopped"
