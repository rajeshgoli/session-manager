"""Codex provider-native title synchronization tests (#650)."""

from __future__ import annotations

import json
from unittest.mock import AsyncMock, MagicMock

import pytest

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
    assert session.native_title_updated_at_ns == 1777182384375958000
    assert manager.get_effective_session_name(session) == "3047-reviewer"


def test_codex_fork_thread_name_event_preserves_nanosecond_timestamp(tmp_path):
    manager = _manager(tmp_path)
    session = Session(
        id="cf655",
        name="codex-fork-cf655",
        working_dir="/tmp",
        provider="codex-fork",
        provider_resume_id="thread-655",
        native_title="old-title",
        native_title_updated_at_ns=1777182384375958016,
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "thread_name_updated",
            "seq": 2,
            "session_epoch": 1,
            "ts": "2026-04-26T05:46:25.123456789Z",
            "payload": {
                "thread_id": "thread-655",
                "thread_name": "new-title",
            },
        },
    )

    assert session.native_title == "new-title"
    assert session.native_title_updated_at_ns == 1777182385123456789


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


def test_codex_thread_name_event_ignores_unknown_thread_id(tmp_path):
    manager = _manager(tmp_path)
    session = Session(
        id="cf652",
        name="codex-fork-cf652",
        working_dir="/tmp",
        provider="codex-fork",
        provider_resume_id="real-thread",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session

    manager.ingest_codex_fork_event(
        session.id,
        {
            "event_type": "thread_name_updated",
            "seq": 1,
            "session_epoch": 1,
            "payload": {"thread_name": "native-reviewer"},
            "session_id": "unknown",
        },
    )

    assert session.provider_resume_id == "real-thread"
    assert session.native_title == "native-reviewer"


def test_codex_index_title_without_timestamp_does_not_beat_explicit_name(tmp_path):
    manager = _manager(tmp_path)
    session = Session(
        id="cf653",
        name="codex-fork-cf653",
        working_dir="/tmp",
        provider="codex-fork",
        provider_resume_id="thread-653",
        friendly_name="explicit-reviewer",
        friendly_name_is_explicit=True,
        friendly_name_updated_at_ns=100,
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session

    changed = manager._sync_codex_native_title(
        session,
        thread_name="index-reviewer",
        updated_at_ns=None,
        thread_id="thread-653",
    )

    assert changed is True
    assert session.native_title_updated_at_ns == 0
    assert manager.get_effective_session_name(session) == "explicit-reviewer"


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
        id="cf654",
        name="codex-fork-cf654",
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

    restored = manager.sessions["cf654"]
    assert restored.native_title == "index-reviewer"
    assert manager.get_effective_session_name(restored) == "index-reviewer"


@pytest.mark.asyncio
async def test_queue_provider_native_rename_queues_codex_fork_rename(tmp_path):
    manager = _manager(tmp_path)
    session = Session(
        id="cf656",
        name="codex-fork-cf656",
        working_dir="/tmp",
        tmux_session="codex-fork-cf656",
        provider="codex-fork",
        status=SessionStatus.IDLE,
    )
    manager.sessions[session.id] = session
    manager.message_queue_manager = MagicMock()

    queued = await manager.queue_provider_native_rename(session, "codex-reviewer")

    assert queued is True
    manager.message_queue_manager.cancel_queued_messages_for_target.assert_called_once_with(
        session.id,
        "native_rename",
    )
    manager.message_queue_manager.queue_message.assert_called_once_with(
        target_session_id=session.id,
        text="/rename codex-reviewer",
        delivery_mode="sequential",
        message_category="native_rename",
    )


@pytest.mark.asyncio
async def test_create_session_common_queues_codex_native_rename_for_spawn_name(tmp_path):
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()
    manager.tmux.create_session_with_command.return_value = True
    manager._get_git_remote_url_async = AsyncMock(return_value=None)
    manager._ensure_telegram_topic = AsyncMock()
    manager.message_queue_manager = MagicMock()

    session = await manager._create_session_common(
        working_dir=str(tmp_path),
        friendly_name="spawned-codex",
        provider="codex-fork",
    )

    assert session is not None
    assert session.friendly_name == "spawned-codex"
    manager.message_queue_manager.cancel_queued_messages_for_target.assert_called_once_with(
        session.id,
        "native_rename",
    )
    manager.message_queue_manager.queue_message.assert_called_once_with(
        target_session_id=session.id,
        text="/rename spawned-codex",
        delivery_mode="sequential",
        message_category="native_rename",
    )
