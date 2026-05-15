from __future__ import annotations

import json
import sys
from datetime import datetime
from unittest.mock import AsyncMock

import pytest

from src.models import Session
from src.session_manager import SessionManager


class _FakeTmux:
    def __init__(self):
        self.created: list[dict] = []
        self.last_error_message = None

    def session_exists(self, session_name: str) -> bool:
        return False

    def create_session_with_command(
        self,
        session_name: str,
        working_dir: str,
        log_file: str,
        *,
        session_id: str | None = None,
        command: str = "claude",
        args: list[str] | None = None,
        model: str | None = None,
        initial_prompt: str | None = None,
    ) -> bool:
        self.created.append(
            {
                "session_name": session_name,
                "working_dir": working_dir,
                "log_file": log_file,
                "session_id": session_id,
                "command": command,
                "args": args or [],
                "model": model,
                "initial_prompt": initial_prompt,
            }
        )
        return True

    def kill_session(self, session_name: str) -> bool:
        return True

    async def rename_codex_thread_async(self, session_name: str, friendly_name: str) -> bool:
        return True


def _manager(tmp_path) -> SessionManager:
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={
            "codex": {"command": sys.executable, "args": []},
            "codex_fork": {
                "command": sys.executable,
                "args": [],
                "event_schema_version": 2,
                "fork_timeout_seconds": 0.1,
            },
        },
    )
    manager.tmux = _FakeTmux()
    manager._get_git_remote_url_async = AsyncMock(return_value=None)
    manager.queue_provider_native_rename = AsyncMock(return_value=True)
    manager._schedule_telegram_topic_ensure = lambda session, explicit_chat_id=None: None
    return manager


@pytest.mark.asyncio
async def test_fork_session_launches_codex_fork_and_preserves_source_resume_id(tmp_path):
    manager = _manager(tmp_path)
    source = Session(
        id="source01",
        name="codex-fork-source01",
        working_dir=str(tmp_path),
        provider="codex-fork",
        provider_resume_id="thread-source",
        model="gpt-test",
    )
    manager.sessions[source.id] = source

    async def _confirm(fork_session: Session, source_resume_id: str):
        assert source_resume_id == "thread-source"
        fork_session.provider_resume_id = "thread-fork"
        fork_session.forked_provider_resume_id = "thread-fork"
        fork_session.forked_at = datetime(2026, 5, 14, 12, 0, 0)
        return True, "thread-fork", None

    manager._wait_for_codex_fork_result = _confirm

    ok, fork_session, error = await manager.fork_session(
        source.id,
        name="source-fork",
        forked_by_session_id="caller01",
    )

    assert ok is True
    assert error is None
    assert fork_session is not None
    assert source.provider_resume_id == "thread-source"
    assert fork_session.provider == "codex-fork"
    assert fork_session.provider_resume_id == "thread-fork"
    assert fork_session.forked_from_session_id == source.id
    assert fork_session.forked_from_provider_resume_id == "thread-source"
    assert fork_session.forked_provider_resume_id == "thread-fork"
    assert fork_session.forked_by_session_id == "caller01"
    assert fork_session.friendly_name == "source-fork"

    created = manager.tmux.created[-1]
    assert created["command"] == sys.executable
    assert created["args"][:2] == ["fork", "thread-source"]
    assert "--event-stream" in created["args"]
    assert "--control-socket" in created["args"]


@pytest.mark.asyncio
async def test_fork_session_rejects_unsupported_provider(tmp_path):
    manager = _manager(tmp_path)
    manager.sessions["claude01"] = Session(
        id="claude01",
        working_dir=str(tmp_path),
        provider="claude",
        provider_resume_id="claude-thread",
    )

    ok, fork_session, error = await manager.fork_session("claude01")

    assert ok is False
    assert fork_session is None
    assert error == "Session forking is not supported for provider=claude yet."


@pytest.mark.asyncio
async def test_fork_session_rejects_missing_resume_id(tmp_path):
    manager = _manager(tmp_path)
    manager.sessions["source01"] = Session(
        id="source01",
        working_dir=str(tmp_path),
        provider="codex-fork",
    )

    ok, fork_session, error = await manager.fork_session("source01")

    assert ok is False
    assert fork_session is None
    assert error == "Source session has no provider resume id to fork"


@pytest.mark.asyncio
async def test_wait_for_codex_fork_result_binds_matching_thread_started_event(tmp_path):
    manager = _manager(tmp_path)
    fork_session = Session(
        id="fork01",
        working_dir=str(tmp_path),
        provider="codex-fork",
        forked_from_session_id="source01",
        forked_from_provider_resume_id="thread-source",
    )
    event_path = manager._codex_fork_event_stream_path(fork_session)
    event_path.parent.mkdir(parents=True, exist_ok=True)
    event_path.write_text(
        json.dumps(
            {
                "event_type": "thread/started",
                "ts": "2026-05-14T12:00:00Z",
                "payload": {
                    "thread": {
                        "id": "thread-fork",
                        "forkedFromId": "thread-source",
                    }
                },
            }
        )
        + "\n"
    )

    ok, fork_resume_id, error = await manager._wait_for_codex_fork_result(
        fork_session,
        "thread-source",
    )

    assert ok is True
    assert error is None
    assert fork_resume_id == "thread-fork"
    assert fork_session.provider_resume_id == "thread-fork"
    assert fork_session.forked_provider_resume_id == "thread-fork"
    assert fork_session.forked_at is not None


def test_ingest_codex_fork_manual_fork_preserves_previous_resume_id(tmp_path):
    manager = _manager(tmp_path)
    session = Session(
        id="livefork",
        working_dir=str(tmp_path),
        provider="codex-fork",
        provider_resume_id="thread-source",
    )
    manager.sessions[session.id] = session

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "thread/started",
            "ts": "2026-05-14T12:00:00Z",
            "payload": {
                "thread": {
                    "id": "thread-fork",
                    "forkedFromId": "thread-source",
                }
            },
        },
    )

    assert session.provider_resume_id == "thread-fork"
    assert session.forked_from_session_id == session.id
    assert session.forked_from_provider_resume_id == "thread-source"
    assert session.forked_provider_resume_id == "thread-fork"
    assert session.forked_at is not None
