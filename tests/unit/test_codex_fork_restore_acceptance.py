"""Hostile restore-admission coverage for account-bound codex-fork resumes."""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
from pathlib import Path

import pytest

from src.models import Session, SessionStatus
from src.session_manager import SessionManager


class FakeProviderTmux:
    """Launch the configured fake provider, then expose its tmux liveness."""

    socket_name = None

    def __init__(self, *, remains_live: bool) -> None:
        self.remains_live = remains_live
        self.live = False
        self.launches: list[tuple[str, list[str]]] = []

    def session_exists(self, _name: str, node: str = "primary") -> bool:
        return self.live

    def create_session_with_command(self, _name: str, _cwd: str, _log_file: str, **kwargs: object) -> bool:
        command = str(kwargs["command"])
        args = [str(arg) for arg in kwargs["args"]]
        self.launches.append((command, args))
        # The fake provider writes structured JSONL and exits.  Its terminal
        # text is intentionally unavailable to the test.
        subprocess.run([command, *args], check=True, cwd=_cwd, env=os.environ.copy())
        self.live = self.remains_live
        return True

    def kill_session(self, _name: str) -> bool:
        self.live = False
        return True


def _fake_provider(tmp_path: Path) -> Path:
    command = tmp_path / "fake_codex_fork.py"
    command.write_text(
        "#!/usr/bin/env python3\n"
        "import os\n"
        "import sys\n"
        "from pathlib import Path\n"
        "args = sys.argv[1:]\n"
        "event_path = args[args.index('--event-stream') + 1]\n"
        "Path(event_path).write_text(os.environ['FAKE_CODEX_FORK_EVENTS'])\n"
    )
    command.chmod(0o755)
    return command


def _manager(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, events: list[dict], *, remains_live: bool):
    monkeypatch.setenv("FAKE_CODEX_FORK_EVENTS", "".join(json.dumps(event) + "\n" for event in events))
    manager = SessionManager(
        log_dir=str(tmp_path),
        state_file=str(tmp_path / "sessions.json"),
        config={
            "codex_fork": {
                "command": str(_fake_provider(tmp_path)),
                "args": [],
                "restore_acceptance_timeout_seconds": 0.15,
                "restore_acceptance_window_seconds": 0.02,
                "event_poll_interval_seconds": 0.01,
            }
        },
    )
    tmux = FakeProviderTmux(remains_live=remains_live)
    manager.tmux = tmux
    session = Session(
        id="restore01",
        name="codex-fork-restore01",
        working_dir=str(tmp_path),
        tmux_session="codex-fork-restore01",
        log_file=str(tmp_path / "restore.log"),
        provider="codex-fork",
        status=SessionStatus.STOPPED,
        provider_resume_id="account-bound-resume-id",
    )
    manager.sessions[session.id] = session
    return manager, tmux, session


@pytest.mark.asyncio
async def test_restore_rejects_account_bound_resume_when_fake_provider_starts_then_ends(monkeypatch, tmp_path):
    """The measured production shape never crosses restore's success boundary."""
    manager, tmux, session = _manager(
        tmp_path,
        monkeypatch,
        [
            {"event_type": "session_start", "seq": 1},
            {"event_type": "session_end", "seq": 2},
        ],
        remains_live=False,
    )

    success, restored, error = await manager.restore_session(session.id)

    assert success is False
    assert restored is session
    assert error == "Codex-fork provider ended before restore acceptance"
    assert tmux.launches  # process launch itself succeeded; provider acceptance did not
    assert session.status == SessionStatus.STOPPED
    assert session.provider_resume_id == "account-bound-resume-id"
    assert session.error_message == error
    assert manager.get_activity_state(session) == "stopped"
    assert manager.get_attach_descriptor(session.id)["attach_supported"] is False


@pytest.mark.asyncio
async def test_restore_rejects_mismatched_thread_started_identity(monkeypatch, tmp_path):
    manager, _tmux, session = _manager(
        tmp_path,
        monkeypatch,
        [{"event_type": "thread_started", "payload": {"thread": {"id": "new-account-thread"}}}],
        remains_live=True,
    )

    success, _restored, error = await manager.restore_session(session.id)

    assert success is False
    assert error == "Codex-fork provider resumed a different thread (new-account-thread != account-bound-resume-id)"
    assert session.status == SessionStatus.STOPPED
    assert session.provider_resume_id == "account-bound-resume-id"


@pytest.mark.asyncio
async def test_restore_rejects_tmux_disappearance_without_a_thread_started_event(monkeypatch, tmp_path):
    manager, _tmux, session = _manager(
        tmp_path,
        monkeypatch,
        [{"event_type": "session_start", "seq": 1}],
        remains_live=False,
    )

    success, _restored, error = await manager.restore_session(session.id)

    assert success is False
    assert error == "Codex-fork tmux runtime disappeared before restore acceptance"
    assert session.status == SessionStatus.STOPPED


@pytest.mark.asyncio
async def test_restore_rejects_live_runtime_that_never_confirms_resume_identity(monkeypatch, tmp_path):
    manager, _tmux, session = _manager(
        tmp_path,
        monkeypatch,
        [{"event_type": "session_start", "seq": 1}],
        remains_live=True,
    )

    success, _restored, error = await manager.restore_session(session.id)

    assert success is False
    assert error == "Codex-fork provider did not confirm the expected resume identity before timeout"
    assert session.status == SessionStatus.STOPPED
    assert session.provider_resume_id == "account-bound-resume-id"


@pytest.mark.asyncio
async def test_restore_rejects_an_overlapping_attempt_while_acceptance_is_pending(monkeypatch, tmp_path):
    manager, tmux, session = _manager(
        tmp_path,
        monkeypatch,
        [{"event_type": "session_start", "seq": 1}],
        remains_live=True,
    )
    first_restore = asyncio.create_task(manager.restore_session(session.id))
    for _ in range(20):
        if session.restore_launch_pending:
            break
        await asyncio.sleep(0.01)

    second_success, second_session, second_error = await manager.restore_session(session.id)
    first_success, _first_session, first_error = await first_restore

    assert second_success is False
    assert second_session is session
    assert second_error == "Codex-fork restore is already awaiting provider acceptance"
    assert len(tmux.launches) == 1
    assert first_success is False
    assert first_error == "Codex-fork provider did not confirm the expected resume identity before timeout"


@pytest.mark.asyncio
async def test_restore_applies_only_a_live_matching_thread_started_identity(monkeypatch, tmp_path):
    manager, _tmux, session = _manager(
        tmp_path,
        monkeypatch,
        [
            {"event_type": "session_start", "seq": 1},
            {
                "event_type": "thread_started",
                "seq": 2,
                "payload": {"thread": {"id": "account-bound-resume-id"}},
            },
        ],
        remains_live=True,
    )

    success, restored, error = await manager.restore_session(session.id)

    assert success is True
    assert error is None
    assert restored is session
    assert session.status == SessionStatus.RUNNING
    assert session.provider_resume_id == "account-bound-resume-id"
    assert session.restore_launch_pending is False
    assert manager.get_attach_descriptor(session.id)["attach_supported"] is True
    await manager._stop_codex_fork_event_monitor(session.id)


def test_restart_recovers_ambiguous_restore_as_stopped_and_unattachable(tmp_path):
    state_file = tmp_path / "sessions.json"
    session = Session(
        id="ambig001",
        name="codex-fork-ambig001",
        working_dir=str(tmp_path),
        tmux_session="codex-fork-ambig001",
        log_file=str(tmp_path / "ambig.log"),
        provider="codex-fork",
        status=SessionStatus.STOPPED,
        provider_resume_id="account-bound-resume-id",
        restore_launch_pending=True,
        restore_pending_resume_id="account-bound-resume-id",
    )
    state_file.write_text(json.dumps({"sessions": [session.to_dict()]}))

    manager = SessionManager(log_dir=str(tmp_path), state_file=str(state_file), config={})
    restored = manager.get_session(session.id)

    assert restored is not None
    assert restored.status == SessionStatus.STOPPED
    assert restored.restore_launch_pending is False
    assert restored.provider_resume_id == "account-bound-resume-id"
    assert "acceptance was interrupted" in restored.error_message
    assert manager.get_activity_state(restored) == "stopped"
    assert manager.get_attach_descriptor(restored.id)["attach_supported"] is False
