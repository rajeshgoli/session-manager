from __future__ import annotations

import asyncio
import json
import os
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock
import pytest

from src.models import Session, SessionStatus
from src.server import create_app
from src.session_manager import SessionManager


def _manager(tmp_path: Path) -> SessionManager:
    return SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={
            "claude": {
                "transcript_root": str(tmp_path / ".claude" / "projects"),
            }
        },
    )


def _write_transcript(path: Path, *entries: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w") as handle:
        for entry in entries:
            handle.write(json.dumps(entry) + "\n")


def _claude_session(
    tmp_path: Path,
    transcript_path: Path | None,
    *,
    friendly_name: str | None = None,
    created_at: datetime | None = None,
) -> Session:
    return Session(
        id="claude123",
        name="claude-claude123",
        working_dir=str(tmp_path),
        tmux_session="claude-claude123",
        provider="claude",
        log_file=str(tmp_path / "claude123.log"),
        status=SessionStatus.RUNNING,
        created_at=created_at or datetime.now(),
        last_activity=created_at or datetime.now(),
        transcript_path=str(transcript_path) if transcript_path else None,
        friendly_name=friendly_name,
    )


def _claude_project_dir(tmp_path: Path, working_dir: Path) -> Path:
    normalized = str(working_dir.expanduser().resolve()).replace("/", "-")
    return tmp_path / ".claude" / "projects" / normalized


def test_effective_name_uses_claude_custom_title_when_no_friendly_name(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(
        transcript,
        {"type": "agent-name", "agentName": "fallback-agent"},
        {"type": "custom-title", "customTitle": "native-claude-title"},
    )
    session = _claude_session(tmp_path, transcript)
    manager.sessions[session.id] = session

    assert manager.get_effective_session_name(session.id) == "native-claude-title"
    assert session.native_title == "native-claude-title"
    assert session.native_title_source_mtime_ns is not None


def test_effective_name_uses_live_tmux_title_when_transcript_path_missing(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()
    manager.tmux.get_pane_title.return_value = "⠂ bork-investigator"
    session = _claude_session(tmp_path, None)
    manager.sessions[session.id] = session

    assert manager.get_effective_session_name(session.id) == "bork-investigator"
    assert session.native_title == "bork-investigator"
    assert session.transcript_path is None


def test_effective_name_ignores_hostname_pane_title_when_friendly_name_exists(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()
    manager.tmux.get_pane_title.return_value = "Rajeshs-MacBook-Pro.local"
    session = _claude_session(tmp_path, None, friendly_name="spawned-name")
    manager.set_session_friendly_name(session, "spawned-name", explicit=True)
    manager.sessions[session.id] = session

    assert manager.get_effective_session_name(session.id) == "spawned-name"
    assert session.native_title is None


@pytest.mark.asyncio
async def test_create_session_common_marks_requested_friendly_name_explicit(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()
    manager.tmux.create_session_with_command.return_value = True
    manager._get_git_remote_url_async = AsyncMock(return_value=None)
    manager._ensure_telegram_topic = AsyncMock()

    session = await manager._create_session_common(
        working_dir=str(tmp_path),
        friendly_name="spawned-name",
        provider="claude",
    )

    assert session is not None
    assert session.friendly_name == "spawned-name"
    assert session.friendly_name_is_explicit is True
    assert isinstance(session.friendly_name_updated_at_ns, int)


def test_effective_name_discovers_matching_transcript_path_when_missing(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()
    manager.tmux.get_pane_title.return_value = "✳ bork-investigator"
    working_dir = tmp_path / "repo"
    working_dir.mkdir()
    created_at = datetime.now(timezone.utc)
    project_dir = _claude_project_dir(tmp_path, working_dir)
    transcript = project_dir / "chosen.jsonl"
    _write_transcript(
        transcript,
        {
            "type": "user",
            "timestamp": created_at.isoformat(),
            "cwd": str(working_dir.resolve()),
        },
        {"type": "custom-title", "customTitle": "bork-investigator"},
    )
    other_transcript = project_dir / "other.jsonl"
    _write_transcript(
        other_transcript,
        {
            "type": "user",
            "timestamp": (created_at + timedelta(seconds=2)).isoformat(),
            "cwd": str(working_dir.resolve()),
        },
        {"type": "custom-title", "customTitle": "other-title"},
    )
    session = Session(
        id="claude123",
        name="claude-claude123",
        working_dir=str(working_dir),
        tmux_session="claude-claude123",
        provider="claude",
        log_file=str(tmp_path / "claude123.log"),
        status=SessionStatus.RUNNING,
        created_at=created_at,
        last_activity=created_at,
    )
    manager.sessions[session.id] = session

    assert manager.get_effective_session_name(session.id) == "bork-investigator"
    assert session.native_title == "bork-investigator"
    assert session.transcript_path == str(transcript.resolve())
    assert session.native_title_source_mtime_ns is not None


def test_effective_name_discovery_skips_transcript_claimed_by_other_session(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()
    manager.tmux.get_pane_title.return_value = "✳ bork-investigator"
    working_dir = tmp_path / "repo"
    working_dir.mkdir()
    created_at = datetime.now(timezone.utc)
    project_dir = _claude_project_dir(tmp_path, working_dir)
    claimed_transcript = project_dir / "claimed.jsonl"
    _write_transcript(
        claimed_transcript,
        {
            "type": "user",
            "timestamp": created_at.isoformat(),
            "cwd": str(working_dir.resolve()),
        },
        {"type": "custom-title", "customTitle": "bork-investigator"},
    )
    chosen_transcript = project_dir / "chosen.jsonl"
    _write_transcript(
        chosen_transcript,
        {
            "type": "user",
            "timestamp": (created_at + timedelta(seconds=1)).isoformat(),
            "cwd": str(working_dir.resolve()),
        },
        {"type": "custom-title", "customTitle": "bork-investigator"},
    )
    claimed_session = Session(
        id="claimed",
        name="claude-claimed",
        working_dir=str(working_dir),
        tmux_session="claude-claimed",
        provider="claude",
        log_file=str(tmp_path / "claimed.log"),
        status=SessionStatus.RUNNING,
        created_at=created_at,
        last_activity=created_at,
        transcript_path=str(claimed_transcript.resolve()),
    )
    session = Session(
        id="claude123",
        name="claude-claude123",
        working_dir=str(working_dir),
        tmux_session="claude-claude123",
        provider="claude",
        log_file=str(tmp_path / "claude123.log"),
        status=SessionStatus.RUNNING,
        created_at=created_at,
        last_activity=created_at,
    )
    manager.sessions[claimed_session.id] = claimed_session
    manager.sessions[session.id] = session

    assert manager.get_effective_session_name(session.id) == "bork-investigator"
    assert session.transcript_path == str(chosen_transcript.resolve())


def test_effective_name_prefers_claude_native_title_over_stale_friendly_name(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(transcript, {"type": "custom-title", "customTitle": "native-claude-title"})
    session = _claude_session(tmp_path, transcript, friendly_name="sm-explicit-name")
    manager.sessions[session.id] = session

    assert manager.get_effective_session_name(session.id) == "native-claude-title"
    assert session.native_title == "native-claude-title"


def test_effective_name_prefers_explicit_sm_name_over_claude_native_title(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(transcript, {"type": "custom-title", "customTitle": "native-claude-title"})
    session = _claude_session(tmp_path, transcript, friendly_name="sm-explicit-name")
    manager.sessions[session.id] = session
    assert manager.sync_claude_native_title(session.id) == "native-claude-title"
    manager.set_session_friendly_name(session, "sm-explicit-name", explicit=True)

    assert manager.get_effective_session_name(session.id) == "sm-explicit-name"
    assert session.native_title == "native-claude-title"


def test_effective_name_prefers_newer_claude_native_title_over_older_explicit_sm_name(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(transcript, {"type": "custom-title", "customTitle": "first-native-title"})
    session = _claude_session(tmp_path, transcript, friendly_name="older-sm-name")
    manager.sessions[session.id] = session
    manager.set_session_friendly_name(session, "older-sm-name", explicit=True, updated_at_ns=1)

    assert manager.get_effective_session_name(session.id) == "first-native-title"
    assert session.native_title == "first-native-title"


def test_effective_name_prefers_newer_sm_name_over_claude_native_title(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(transcript, {"type": "custom-title", "customTitle": "native-claude-title"})
    session = _claude_session(tmp_path, transcript)
    manager.sessions[session.id] = session

    assert manager.get_effective_session_name(session.id) == "native-claude-title"
    manager.set_session_friendly_name(
        session,
        "sm-renamed-later",
        explicit=True,
        updated_at_ns=(session.native_title_source_mtime_ns or 0) + 1,
    )

    assert manager.get_effective_session_name(session.id) == "sm-renamed-later"


def test_effective_name_refreshes_when_claude_transcript_title_changes(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(transcript, {"type": "custom-title", "customTitle": "first-title"})
    session = _claude_session(tmp_path, transcript)
    manager.sessions[session.id] = session

    assert manager.get_effective_session_name(session.id) == "first-title"

    with transcript.open("a") as handle:
        handle.write(json.dumps({"type": "custom-title", "customTitle": "second-title"}) + "\n")
    os.utime(transcript, None)

    assert manager.get_effective_session_name(session.id) == "second-title"
    assert session.native_title == "second-title"
    assert session.native_title_updated_at_ns == session.native_title_source_mtime_ns


def test_transcript_mtime_churn_does_not_persist_without_title_change(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(transcript, {"type": "custom-title", "customTitle": "stable-title"})
    session = _claude_session(tmp_path, transcript)
    manager.sessions[session.id] = session
    manager._save_state = MagicMock()

    assert manager.get_effective_session_name(session.id) == "stable-title"
    manager._save_state.reset_mock()

    with transcript.open("a") as handle:
        handle.write(json.dumps({"type": "assistant", "message": {"content": [{"type": "text", "text": "still working"}]}}) + "\n")
    os.utime(transcript, None)

    assert manager.sync_claude_native_title(session.id) == "stable-title"
    manager._save_state.assert_not_called()


def test_transcript_mtime_churn_does_not_override_later_sm_name(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(transcript, {"type": "custom-title", "customTitle": "stable-title"})
    session = _claude_session(tmp_path, transcript)
    manager.sessions[session.id] = session

    assert manager.get_effective_session_name(session.id) == "stable-title"
    native_title_updated_at_ns = session.native_title_updated_at_ns or 0
    manager.set_session_friendly_name(
        session,
        "sm-renamed-later",
        explicit=True,
        updated_at_ns=native_title_updated_at_ns + 1,
    )
    assert manager.get_effective_session_name(session.id) == "sm-renamed-later"

    with transcript.open("a") as handle:
        handle.write(json.dumps({"type": "assistant", "message": {"content": [{"type": "text", "text": "still working"}]}}) + "\n")
    os.utime(transcript, None)

    assert manager.get_effective_session_name(session.id) == "sm-renamed-later"


def test_claude_hook_resyncs_tmux_and_telegram_when_native_title_changes(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()
    manager.tmux.set_status_bar.return_value = True
    manager.message_queue_manager = MagicMock()
    manager.message_queue_manager.mark_session_idle = MagicMock()
    manager.message_queue_manager.delivery_states = {}
    manager.message_queue_manager._restore_user_input_after_response = AsyncMock()

    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(
        transcript,
        {"type": "assistant", "message": {"content": [{"type": "text", "text": "done"}]}},
        {"type": "custom-title", "customTitle": "native-hook-title"},
    )
    session = _claude_session(tmp_path, transcript)
    session.native_title = "old-title"
    session.native_title_source_mtime_ns = 1
    session.telegram_chat_id = 123
    session.telegram_thread_id = 456
    manager.sessions[session.id] = session

    notifier = MagicMock()
    notifier.rename_session_topic = AsyncMock(return_value=True)
    notifier.notify = AsyncMock(return_value=True)

    client = create_app(
        session_manager=manager,
        notifier=notifier,
        output_monitor=MagicMock(),
        config={},
    )

    from fastapi.testclient import TestClient

    response = TestClient(client).post(
        "/hooks/claude",
        json={
            "hook_event_name": "Stop",
            "session_manager_id": session.id,
            "transcript_path": str(transcript),
        },
    )

    assert response.status_code == 200
    assert session.native_title == "native-hook-title"
    manager.tmux.set_status_bar.assert_called_with(session.tmux_session, "native-hook-title")
    notifier.rename_session_topic.assert_awaited_with(session, "native-hook-title")
    assert session.display_identity_synced_name == "native-hook-title"
    assert isinstance(session.display_identity_synced_at_ns, int)
    assert session.display_identity_synced_chat_id == 123
    assert session.display_identity_synced_thread_id == 456


def test_get_session_resyncs_lazy_native_title_to_telegram(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()
    manager.tmux.set_status_bar.return_value = True

    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(
        transcript,
        {"type": "assistant", "message": {"content": [{"type": "text", "text": "done"}]}},
        {"type": "custom-title", "customTitle": "lazy-native-title"},
    )
    session = _claude_session(tmp_path, transcript)
    session.telegram_chat_id = 123
    session.telegram_thread_id = 456
    manager.sessions[session.id] = session

    notifier = MagicMock()
    notifier.rename_session_topic = AsyncMock(return_value=True)

    client = create_app(session_manager=manager, notifier=notifier, config={})

    from fastapi.testclient import TestClient

    response = TestClient(client).get(f"/sessions/{session.id}")

    assert response.status_code == 200
    assert response.json()["friendly_name"] == "lazy-native-title"
    manager.tmux.set_status_bar.assert_called_with(session.tmux_session, "lazy-native-title")
    notifier.rename_session_topic.assert_awaited_once_with(session, "lazy-native-title")
    assert session.display_identity_synced_name == "lazy-native-title"
    assert isinstance(session.display_identity_synced_at_ns, int)
    assert session.display_identity_synced_chat_id == 123
    assert session.display_identity_synced_thread_id == 456


def test_get_session_does_not_resync_when_display_identity_is_current(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()

    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(
        transcript,
        {"type": "custom-title", "customTitle": "stable-native-title"},
    )
    session = _claude_session(tmp_path, transcript)
    session.native_title = "stable-native-title"
    session.native_title_source_mtime_ns = transcript.stat().st_mtime_ns
    session.display_identity_synced_name = "stable-native-title"
    session.display_identity_synced_at_ns = 1
    session.telegram_chat_id = 123
    session.telegram_thread_id = 456
    session.display_identity_synced_chat_id = 123
    session.display_identity_synced_thread_id = 456
    manager.sessions[session.id] = session

    notifier = MagicMock()
    notifier.rename_session_topic = AsyncMock(return_value=True)

    client = create_app(session_manager=manager, notifier=notifier, config={})

    from fastapi.testclient import TestClient

    response = TestClient(client).get(f"/sessions/{session.id}")

    assert response.status_code == 200
    manager.tmux.set_status_bar.assert_not_called()
    notifier.rename_session_topic.assert_not_awaited()


def test_get_session_does_not_mark_telegram_synced_when_bot_missing(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()

    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(
        transcript,
        {"type": "custom-title", "customTitle": "lazy-native-title"},
    )
    session = _claude_session(tmp_path, transcript)
    session.telegram_chat_id = 123
    session.telegram_thread_id = 456
    manager.sessions[session.id] = session

    notifier = MagicMock()
    notifier.telegram = None
    notifier.rename_session_topic = AsyncMock(return_value=True)

    client = create_app(session_manager=manager, notifier=notifier, config={})

    from fastapi.testclient import TestClient

    response = TestClient(client).get(f"/sessions/{session.id}")

    assert response.status_code == 200
    manager.tmux.set_status_bar.assert_called_with(session.tmux_session, "lazy-native-title")
    notifier.rename_session_topic.assert_not_awaited()
    assert session.display_identity_synced_name is None
    assert session.display_identity_synced_at_ns is None


def test_get_session_resyncs_when_telegram_thread_changes(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()

    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(
        transcript,
        {"type": "custom-title", "customTitle": "stable-native-title"},
    )
    session = _claude_session(tmp_path, transcript)
    session.native_title = "stable-native-title"
    session.native_title_source_mtime_ns = transcript.stat().st_mtime_ns
    session.display_identity_synced_name = "stable-native-title"
    session.display_identity_synced_at_ns = 1
    session.display_identity_synced_chat_id = 123
    session.display_identity_synced_thread_id = 111
    session.telegram_chat_id = 123
    session.telegram_thread_id = 456
    manager.sessions[session.id] = session

    notifier = MagicMock()
    notifier.rename_session_topic = AsyncMock(return_value=True)

    client = create_app(session_manager=manager, notifier=notifier, config={})

    from fastapi.testclient import TestClient

    response = TestClient(client).get(f"/sessions/{session.id}")

    assert response.status_code == 200
    notifier.rename_session_topic.assert_awaited_once_with(session, "stable-native-title")
    assert session.display_identity_synced_chat_id == 123
    assert session.display_identity_synced_thread_id == 456


def test_get_session_bounds_lazy_telegram_rename_timeout(tmp_path: Path, monkeypatch) -> None:
    import src.server as server_module

    monkeypatch.setattr(server_module, "DISPLAY_IDENTITY_SYNC_TIMEOUT_SECONDS", 0.01)
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()

    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(
        transcript,
        {"type": "custom-title", "customTitle": "lazy-native-title"},
    )
    session = _claude_session(tmp_path, transcript)
    session.telegram_chat_id = 123
    session.telegram_thread_id = 456
    manager.sessions[session.id] = session

    async def slow_rename(*args, **kwargs):
        await asyncio.sleep(1)
        return True

    notifier = MagicMock()
    notifier.rename_session_topic = AsyncMock(side_effect=slow_rename)

    client = create_app(session_manager=manager, notifier=notifier, config={})

    from fastapi.testclient import TestClient

    response = TestClient(client).get(f"/sessions/{session.id}")

    assert response.status_code == 200
    notifier.rename_session_topic.assert_awaited_once_with(session, "lazy-native-title")
    assert session.display_identity_synced_name is None
    assert session.display_identity_synced_at_ns is None
    assert session.display_identity_synced_chat_id is None
    assert session.display_identity_synced_thread_id is None


def test_get_session_does_not_mark_synced_when_tmux_update_fails(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    manager.tmux = MagicMock()
    manager.tmux.set_status_bar.return_value = False

    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(
        transcript,
        {"type": "custom-title", "customTitle": "lazy-native-title"},
    )
    session = _claude_session(tmp_path, transcript)
    session.telegram_chat_id = 123
    session.telegram_thread_id = 456
    manager.sessions[session.id] = session

    notifier = MagicMock()
    notifier.rename_session_topic = AsyncMock(return_value=True)

    client = create_app(session_manager=manager, notifier=notifier, config={})

    from fastapi.testclient import TestClient

    response = TestClient(client).get(f"/sessions/{session.id}")

    assert response.status_code == 200
    manager.tmux.set_status_bar.assert_called_once_with(session.tmux_session, "lazy-native-title")
    notifier.rename_session_topic.assert_awaited_once_with(session, "lazy-native-title")
    assert session.display_identity_synced_name is None
    assert session.display_identity_synced_at_ns is None
    assert session.display_identity_synced_chat_id is None
    assert session.display_identity_synced_thread_id is None
