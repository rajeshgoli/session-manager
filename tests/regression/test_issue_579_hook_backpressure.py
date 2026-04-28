"""Regression tests for sm#579: large hook payloads should not starve the API."""

from __future__ import annotations

import threading
import time
from datetime import datetime
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock
from unittest.mock import patch

from fastapi.testclient import TestClient

import src.server as server_module
from src.models import Session, SessionStatus
from src.server import create_app


def _session_manager_with_session(session: Session) -> MagicMock:
    manager = MagicMock()
    manager.sessions = {session.id: session}
    manager.get_session.side_effect = lambda session_id: manager.sessions.get(session_id)
    manager.get_activity_state.return_value = "thinking"
    manager.get_session_aliases.return_value = []
    manager.get_primary_session_alias.return_value = None
    manager._save_state = MagicMock()
    manager._save_state_async = AsyncMock(return_value=True)
    manager.message_queue_manager = MagicMock()
    manager.message_queue_manager.mark_session_active = MagicMock()
    return manager


def _output_monitor() -> MagicMock:
    monitor = MagicMock()
    monitor.update_activity = MagicMock()
    monitor.mark_response_sent = MagicMock()
    return monitor


def test_large_hook_decode_does_not_block_health(monkeypatch):
    """JSON decoding for hook traffic should run off-thread so health stays responsive."""
    session = Session(
        id="sess579a",
        name="codex-sess579a",
        working_dir="/tmp",
        tmux_session="codex-sess579a",
        status=SessionStatus.RUNNING,
        provider="codex",
        created_at=datetime.now(),
        last_activity=datetime.now(),
    )
    manager = _session_manager_with_session(session)
    app = create_app(session_manager=manager, output_monitor=_output_monitor(), config={})
    app.state.tool_logger = MagicMock(log=AsyncMock(return_value=None))
    client = TestClient(app)

    entered = threading.Event()
    original_loads = server_module.json.loads

    def slow_loads(raw):
        entered.set()
        time.sleep(0.25)
        return original_loads(raw)

    monkeypatch.setattr(server_module.json, "loads", slow_loads)

    payload = {
        "session_manager_id": session.id,
        "session_id": "native-579",
        "hook_event_name": "PostToolUse",
        "tool_name": "Read",
        "tool_input": {"file_path": "big.txt"},
        "tool_response": {"output": "x" * 250_000},
    }

    result: dict[str, int] = {}

    def post_hook() -> None:
        response = client.post("/hooks/tool-use", json=payload)
        result["status_code"] = response.status_code

    thread = threading.Thread(target=post_hook)
    thread.start()
    assert entered.wait(timeout=1.0), "hook decode never started"

    start = time.monotonic()
    response = client.get("/health")
    elapsed = time.monotonic() - start

    assert response.status_code == 200
    assert response.json() == {"status": "healthy"}
    assert elapsed < 0.15

    thread.join(timeout=2.0)
    assert not thread.is_alive()
    assert result["status_code"] == 200


def test_hook_tool_use_logs_cached_session_name_without_live_title_sync():
    """Tool logging should use cached names and avoid display-identity sync in hook path."""
    session = Session(
        id="sess579b",
        name="claude-sess579b",
        working_dir="/tmp",
        tmux_session="claude-sess579b",
        status=SessionStatus.RUNNING,
        provider="claude",
        friendly_name="cached-friendly-name",
        friendly_name_is_explicit=True,
        created_at=datetime.now(),
        last_activity=datetime.now(),
    )
    manager = _session_manager_with_session(session)
    manager.get_effective_session_name.side_effect = AssertionError(
        "hook_tool_use should not call live display-name resolution"
    )
    tool_logger = MagicMock(log=AsyncMock(return_value=None))

    app = create_app(session_manager=manager, output_monitor=_output_monitor(), config={})
    app.state.tool_logger = tool_logger
    client = TestClient(app)

    response = client.post(
        "/hooks/tool-use",
        json={
            "session_manager_id": session.id,
            "session_id": "native-579b",
            "hook_event_name": "PostToolUse",
            "tool_name": "Read",
            "tool_input": {"file_path": "notes.txt"},
            "tool_response": {"output": "ok"},
        },
    )

    assert response.status_code == 200
    tool_logger.log.assert_awaited_once()
    assert tool_logger.log.await_args.kwargs["session_name"] == "cached-friendly-name"


def test_hook_tool_use_offloads_git_lock_acquisition(monkeypatch, tmp_path):
    """PreToolUse lock acquisition runs git/lock-file work off the event loop."""
    session = Session(
        id="sess579c",
        name="claude-sess579c",
        working_dir=str(tmp_path),
        tmux_session="claude-sess579c",
        status=SessionStatus.RUNNING,
        provider="claude",
        created_at=datetime.now(),
        last_activity=datetime.now(),
    )
    manager = _session_manager_with_session(session)
    app = create_app(session_manager=manager, output_monitor=_output_monitor(), config={})
    app.state.tool_logger = MagicMock(log=AsyncMock(return_value=None))
    client = TestClient(app)

    calls: list[str] = []
    original_to_thread = server_module.asyncio.to_thread

    async def tracking_to_thread(func, *args, **kwargs):
        calls.append(getattr(func, "__name__", repr(func)))
        return await original_to_thread(func, *args, **kwargs)

    class FakeLockManager:
        def __init__(self, working_dir: str):
            self.working_dir = working_dir

        def try_acquire(self, repo_root: str, session_id: str):
            return SimpleNamespace(locked_by_other=False, owner_session_id=None)

    monkeypatch.setattr(server_module.asyncio, "to_thread", tracking_to_thread)
    with patch("src.lock_manager.get_git_root", return_value=str(tmp_path)), \
         patch("src.lock_manager.LockManager", FakeLockManager):
        response = client.post(
            "/hooks/tool-use",
            json={
                "session_manager_id": session.id,
                "session_id": "native-579c",
                "hook_event_name": "PreToolUse",
                "tool_name": "Edit",
                "tool_input": {"file_path": str(tmp_path / "file.py")},
                "cwd": str(tmp_path),
            },
        )

    assert response.status_code == 200
    assert "acquire_lock_sync" in calls
    assert str(tmp_path) in session.touched_repos


def test_empty_hook_body_returns_400():
    """Empty hook calls should fail loudly instead of being treated as valid no-ops."""
    app = create_app(session_manager=MagicMock(), output_monitor=_output_monitor(), config={})
    client = TestClient(app)

    response = client.post("/hooks/tool-use", content=b"", headers={"Content-Type": "application/json"})

    assert response.status_code == 400
    assert response.json()["detail"] == "Request body is required"


def test_invalid_non_utf_hook_body_returns_400():
    """Malformed byte payloads should be treated as invalid JSON, not server errors."""
    app = create_app(session_manager=MagicMock(), output_monitor=_output_monitor(), config={})
    client = TestClient(app)

    response = client.post(
        "/hooks/context-usage",
        content=b"\x80\x81\x82",
        headers={"Content-Type": "application/json"},
    )

    assert response.status_code == 400
    assert response.json()["detail"] == "Invalid JSON body"
