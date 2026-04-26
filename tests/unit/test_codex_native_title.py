"""Codex provider-native title synchronization tests (#650)."""

from __future__ import annotations

import json

from src.models import Session, SessionStatus
from src.session_manager import SessionManager


def _manager(tmp_path, session_index_path=None) -> SessionManager:
    config = {}
    if session_index_path is not None:
        config = {"codex": {"session_index_path": str(session_index_path)}}
    return SessionManager(
        log_dir=str(tmp_path),
        state_file=str(tmp_path / "state.json"),
        config=config,
    )


def test_codex_fork_thread_name_event_updates_native_title(tmp_path):
    manager = _manager(tmp_path)
    session = Session(
        id="cf650",
        name="codex-fork-cf650",
        working_dir="/tmp",
        provider="codex-fork",
        provider_resume_id="thread-650",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "thread_name_updated",
            "seq": 1,
            "session_epoch": 1,
            "ts": "2026-04-26T05:46:24.375958Z",
            "payload": {
                "thread_id": "thread-650",
                "thread_name": "3047-reviewer",
            },
        },
    )

    assert session.native_title == "3047-reviewer"
    assert session.native_title_updated_at_ns == 1777182384375958016
    assert manager.get_effective_session_name(session) == "3047-reviewer"


def test_codex_native_title_does_not_override_newer_explicit_sm_name(tmp_path):
    manager = _manager(tmp_path)
    session = Session(
        id="cf651",
        name="codex-fork-cf651",
        working_dir="/tmp",
        provider="codex-fork",
        provider_resume_id="thread-651",
        friendly_name="sm-reviewer",
        friendly_name_is_explicit=True,
        friendly_name_updated_at_ns=200,
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session

    changed = manager._sync_codex_native_title(
        session,
        thread_name="native-reviewer",
        updated_at_ns=100,
        thread_id="thread-651",
    )

    assert changed is True
    assert manager.get_effective_session_name(session) == "sm-reviewer"


def test_codex_session_index_backfills_native_title_on_startup(tmp_path):
    index_path = tmp_path / "session_index.jsonl"
    index_path.write_text(
        json.dumps(
            {
                "id": "thread-652",
                "thread_name": "index-reviewer",
                "updated_at": "2026-04-26T05:46:24.375958Z",
            }
        )
        + "\n",
        encoding="utf-8",
    )

    state_file = tmp_path / "state.json"
    session = Session(
        id="cf652",
        name="codex-fork-cf652",
        working_dir="/tmp",
        provider="codex-fork",
        provider_resume_id="thread-652",
        status=SessionStatus.STOPPED,
    )
    state_file.write_text(
        json.dumps({"sessions": [session.to_dict()]}),
        encoding="utf-8",
    )

    manager = SessionManager(
        log_dir=str(tmp_path),
        state_file=str(state_file),
        config={"codex": {"session_index_path": str(index_path)}},
    )

    restored = manager.sessions["cf652"]
    assert restored.native_title == "index-reviewer"
    assert manager.get_effective_session_name(restored) == "index-reviewer"
