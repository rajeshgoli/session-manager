from __future__ import annotations

import json
import os
from pathlib import Path

from src.models import Session, SessionStatus
from src.session_manager import SessionManager


def _manager(tmp_path: Path) -> SessionManager:
    return SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={},
    )


def _write_transcript(path: Path, *entries: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w") as handle:
        for entry in entries:
            handle.write(json.dumps(entry) + "\n")


def _claude_session(tmp_path: Path, transcript_path: Path, *, friendly_name: str | None = None) -> Session:
    return Session(
        id="claude123",
        name="claude-claude123",
        working_dir=str(tmp_path),
        tmux_session="claude-claude123",
        provider="claude",
        log_file=str(tmp_path / "claude123.log"),
        status=SessionStatus.RUNNING,
        transcript_path=str(transcript_path),
        friendly_name=friendly_name,
    )


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


def test_effective_name_prefers_explicit_friendly_name_over_claude_custom_title(tmp_path: Path) -> None:
    manager = _manager(tmp_path)
    transcript = tmp_path / "transcript.jsonl"
    _write_transcript(transcript, {"type": "custom-title", "customTitle": "native-claude-title"})
    session = _claude_session(tmp_path, transcript, friendly_name="sm-explicit-name")
    manager.sessions[session.id] = session

    assert manager.get_effective_session_name(session.id) == "sm-explicit-name"
    assert session.native_title is None


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
