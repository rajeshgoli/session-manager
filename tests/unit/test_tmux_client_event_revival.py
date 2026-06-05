import json
from datetime import datetime
from unittest.mock import Mock

from src.models import Session, SessionStatus


def test_tmux_client_event_revives_stopped_tmux_backed_session(
    session_manager,
    mock_tmux,
    temp_state_file,
):
    mock_tmux.socket_name = "session-manager"
    mock_tmux.session_exists.return_value = True
    stopped_at = datetime(2026, 6, 5, 15, 28, 53)
    session = Session(
        id="e7f61918",
        name="codex-fork-e7f61918",
        working_dir="/Users/rajesh/projects/fractal-algo-rust",
        tmux_session="codex-fork-e7f61918",
        provider="codex-fork",
        status=SessionStatus.STOPPED,
        stopped_at=stopped_at,
        completed_at=stopped_at,
        error_message="marked stopped",
    )
    session_manager.sessions[session.id] = session

    payload = session_manager.record_tmux_client_event(
        event="client-attached",
        tmux_session="codex-fork-e7f61918",
        tty="/dev/ttys001",
        client_pid="2675",
    )

    assert payload["revived_session_id"] == "e7f61918"
    assert session.status == SessionStatus.IDLE
    assert session.stopped_at is None
    assert session.completed_at is None
    assert session.error_message is None
    assert session.tmux_socket_name == "session-manager"

    saved = json.loads(temp_state_file.read_text())
    saved_session = next(item for item in saved["sessions"] if item["id"] == "e7f61918")
    assert saved_session["status"] == "idle"
    assert saved_session["stopped_at"] is None


def test_tmux_client_detach_does_not_revive_stopped_session(
    session_manager,
    mock_tmux,
):
    mock_tmux.session_exists.return_value = True
    session = Session(
        id="e7f61918",
        name="codex-fork-e7f61918",
        working_dir="/Users/rajesh/projects/fractal-algo-rust",
        tmux_session="codex-fork-e7f61918",
        provider="codex-fork",
        status=SessionStatus.STOPPED,
        stopped_at=datetime(2026, 6, 5, 15, 28, 53),
    )
    session_manager.sessions[session.id] = session

    payload = session_manager.record_tmux_client_event(
        event="client-detached",
        tmux_session="codex-fork-e7f61918",
        tty="/dev/ttys001",
        client_pid="2675",
    )

    assert "revived_session_id" not in payload
    assert session.status == SessionStatus.STOPPED


def test_tmux_client_event_does_not_revive_non_codex_fork_session(
    session_manager,
    mock_tmux,
):
    mock_tmux.session_exists.return_value = True
    session = Session(
        id="claude-live",
        name="claude-live",
        working_dir="/Users/rajesh/projects/fractal-algo-rust",
        tmux_session="claude-live",
        provider="claude",
        status=SessionStatus.STOPPED,
        stopped_at=datetime(2026, 6, 5, 15, 28, 53),
    )
    session_manager.sessions[session.id] = session

    payload = session_manager.record_tmux_client_event(
        event="client-attached",
        tmux_session="claude-live",
        tty="/dev/ttys002",
        client_pid="2676",
    )

    assert "revived_session_id" not in payload
    assert session.status == SessionStatus.STOPPED


def test_tmux_client_event_replays_codex_fork_history_on_revival(
    session_manager,
    mock_tmux,
    monkeypatch,
):
    mock_tmux.session_exists.return_value = True
    session = Session(
        id="e7f61918",
        name="codex-fork-e7f61918",
        working_dir="/Users/rajesh/projects/fractal-algo-rust",
        tmux_session="codex-fork-e7f61918",
        provider="codex-fork",
        status=SessionStatus.STOPPED,
        stopped_at=datetime(2026, 6, 5, 15, 28, 53),
    )
    session_manager.sessions[session.id] = session
    session_manager.codex_fork_lifecycle[session.id] = {"state": "shutdown", "seq": 99}
    session_manager.codex_fork_turns_in_flight.add(session.id)
    session_manager.codex_fork_wait_resume_state[session.id] = "running"
    session_manager.codex_fork_wait_kind[session.id] = "approval"
    session_manager.codex_fork_last_seq[session.id] = 99
    session_manager.codex_fork_session_epoch[session.id] = "old-epoch"
    session_manager.codex_fork_event_offsets[session.id] = 1234
    session_manager.codex_fork_event_buffers[session.id] = "{\"partial\":"
    session_manager.codex_fork_provider_cursors[session.id] = {
        "session_epoch": "old-epoch",
        "session_epoch_key": "s:old-epoch",
        "seq": 99,
    }
    session_manager.codex_event_store.record_codex_fork_provider_event_applied(
        session.id,
        session_epoch="old-epoch",
        seq=99,
    )
    start_monitor = Mock()
    monkeypatch.setattr(session_manager, "_start_codex_fork_event_monitor", start_monitor)

    payload = session_manager.record_tmux_client_event(
        event="client-attached",
        tmux_session="codex-fork-e7f61918",
        tty="/dev/ttys001",
        client_pid="2675",
    )

    assert payload["revived_session_id"] == "e7f61918"
    assert session_manager.codex_fork_lifecycle.get(session.id) is None
    assert session.id not in session_manager.codex_fork_turns_in_flight
    assert session.id not in session_manager.codex_fork_wait_resume_state
    assert session.id not in session_manager.codex_fork_wait_kind
    assert session.id not in session_manager.codex_fork_last_seq
    assert session.id not in session_manager.codex_fork_session_epoch
    assert session.id not in session_manager.codex_fork_event_offsets
    assert session.id not in session_manager.codex_fork_event_buffers
    assert session.id not in session_manager.codex_fork_provider_cursors
    assert session_manager.codex_event_store.get_codex_fork_provider_cursor(session.id) is None
    start_monitor.assert_called_once_with(session, from_eof=False)
