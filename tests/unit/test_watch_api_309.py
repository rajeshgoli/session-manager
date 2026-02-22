"""Unit tests for watch observability payloads and endpoints (#309)."""

from __future__ import annotations

import sqlite3
from datetime import datetime
from types import SimpleNamespace
from unittest.mock import MagicMock

from fastapi.testclient import TestClient

from src.models import Session, SessionStatus
from src.server import create_app
from src.session_manager import SessionManager


class _QueueStub:
    def __init__(self):
        self.mark_session_active = MagicMock()
        self.cancel_remind = MagicMock()
        self.cancel_parent_wake = MagicMock()
        self.cancel_context_monitor_messages_from = MagicMock()
        self._get_or_create_state = MagicMock()
        self.delivery_states = {}


def _make_session(session_id: str, provider: str = "claude") -> Session:
    return Session(
        id=session_id,
        name=f"{provider}-{session_id}",
        working_dir="/tmp/test",
        tmux_session=f"{provider}-{session_id}" if provider != "codex-app" else "",
        provider=provider,
        log_file="/tmp/test.log",
        status=SessionStatus.RUNNING,
    )


def _make_sm(session: Session) -> MagicMock:
    sm = MagicMock()
    sm.get_session = MagicMock(return_value=session)
    sm.list_sessions = MagicMock(return_value=[session])
    sm.sessions = {session.id: session}
    sm._save_state = MagicMock()
    sm.get_activity_state = MagicMock(return_value="thinking")
    sm.is_codex_rollout_enabled = MagicMock(return_value=True)
    sm.message_queue_manager = _QueueStub()
    return sm


def test_sessions_payload_includes_watch_fields():
    session = _make_session("abc12345")
    session.last_tool_name = "Read"
    session.last_tool_call = datetime(2026, 2, 20, 10, 0, 0)
    session.tokens_used = 3210
    session.context_monitor_enabled = True

    app = create_app(session_manager=_make_sm(session))
    client = TestClient(app)

    payload = client.get("/sessions").json()["sessions"][0]
    assert payload["last_tool_name"] == "Read"
    assert payload["last_tool_call"] == "2026-02-20T10:00:00"
    assert payload["tokens_used"] == 3210
    assert payload["context_monitor_enabled"] is True


def test_codex_app_projection_fields_hidden_when_rollout_disabled():
    session = _make_session("codx1111", provider="codex-app")
    sm = _make_sm(session)
    sm.is_codex_rollout_enabled = MagicMock(return_value=False)
    sm.get_codex_latest_activity_action = MagicMock(return_value={
        "summary_text": "Completed command",
        "ended_at": "2026-02-20T10:12:00",
    })

    app = create_app(session_manager=sm)
    client = TestClient(app)

    payload = client.get(f"/sessions/{session.id}").json()
    assert payload["last_action_summary"] is None
    assert payload["last_action_at"] is None


def test_codex_app_projection_fields_exposed_when_rollout_enabled():
    session = _make_session("codx2222", provider="codex-app")
    sm = _make_sm(session)
    sm.get_codex_latest_activity_action = MagicMock(return_value={
        "summary_text": "Completed command",
        "ended_at": "2026-02-20T10:12:00",
    })

    app = create_app(session_manager=sm)
    client = TestClient(app)

    payload = client.get(f"/sessions/{session.id}").json()
    assert payload["last_action_summary"] == "Completed command"
    assert payload["last_action_at"] == "2026-02-20T10:12:00"


def test_tool_calls_endpoint_reads_pretooluse_rows(tmp_path):
    session = _make_session("tool1234")
    sm = _make_sm(session)
    app = create_app(session_manager=sm)

    db_path = tmp_path / "tool_usage.db"
    conn = sqlite3.connect(db_path)
    conn.execute(
        """
        CREATE TABLE tool_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT,
            session_id TEXT,
            hook_type TEXT,
            tool_name TEXT
        )
        """
    )
    conn.execute(
        "INSERT INTO tool_usage (timestamp, session_id, hook_type, tool_name) VALUES (?, ?, ?, ?)",
        ("2026-02-20 10:00:00", session.id, "PreToolUse", "Read"),
    )
    conn.execute(
        "INSERT INTO tool_usage (timestamp, session_id, hook_type, tool_name) VALUES (?, ?, ?, ?)",
        ("2026-02-20 10:00:01", session.id, "PostToolUse", "Read"),
    )
    conn.execute(
        "INSERT INTO tool_usage (timestamp, session_id, hook_type, tool_name) VALUES (?, ?, ?, ?)",
        ("2026-02-20 10:00:02", session.id, "PreToolUse", "Write"),
    )
    conn.commit()
    conn.close()

    app.state.tool_logger = SimpleNamespace(db_path=str(db_path))
    client = TestClient(app)

    payload = client.get(f"/sessions/{session.id}/tool-calls?limit=10").json()
    names = [row["tool_name"] for row in payload["tool_calls"]]
    assert names == ["Write", "Read"]


def test_hook_tool_use_updates_last_tool_fields():
    session = _make_session("hook1234")
    sm = _make_sm(session)

    app = create_app(session_manager=sm)
    client = TestClient(app)

    response = client.post(
        "/hooks/tool-use",
        json={
            "session_manager_id": session.id,
            "hook_event_name": "PreToolUse",
            "tool_name": "Read",
            "tool_input": {"file_path": "README.md"},
            "cwd": "/tmp/test",
        },
    )

    assert response.status_code == 200
    assert session.last_tool_name == "Read"
    assert session.last_tool_call is not None
    sm.message_queue_manager.mark_session_active.assert_called_once_with(session.id)


def test_capture_output_codex_app_respects_lines(tmp_path):
    sm = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = _make_session("codx3333", provider="codex-app")
    sm.sessions[session.id] = session
    sm.set_hook_output_store({session.id: "line1\nline2\nline3\n"})

    assert sm.capture_output(session.id, lines=2) == "line2\nline3\n"
    assert sm.capture_output(session.id, lines=1) == "line3\n"
