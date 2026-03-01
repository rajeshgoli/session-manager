"""Unit tests for codex-fork control-socket client integration."""

from __future__ import annotations

import asyncio
import json
import os
from unittest.mock import AsyncMock

import pytest

from src.models import Session, SessionStatus
from src.session_manager import SessionManager


pytestmark = pytest.mark.skipif(
    os.name == "nt",
    reason="unix domain sockets required for codex-fork control client tests",
)


@pytest.mark.asyncio
async def test_deliver_direct_uses_control_socket_primary_path(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="cfctl1",
        name="codex-fork-cfctl1",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.tmux.send_input_async = AsyncMock(return_value=True)

    socket_path = manager._codex_fork_control_socket_path(session)
    if socket_path.exists():
        socket_path.unlink()

    seen_commands: list[str] = []

    async def handle_client(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        raw = await reader.readline()
        request = json.loads(raw.decode("utf-8"))
        seen_commands.append(request["command"])
        if request["command"] == "get_epoch":
            response = {
                "request_id": request["request_id"],
                "ok": True,
                "epoch": "epoch-1",
                "result": {"epoch": "epoch-1"},
            }
        else:
            assert request["command"] == "submit_message"
            assert request["expected_epoch"] == "epoch-1"
            assert request["message"] == "hello world"
            response = {
                "request_id": request["request_id"],
                "ok": True,
                "epoch": "epoch-1",
                "result": {"status": "accepted"},
            }
        writer.write((json.dumps(response) + "\n").encode("utf-8"))
        await writer.drain()
        writer.close()
        await writer.wait_closed()

    server = await asyncio.start_unix_server(handle_client, path=str(socket_path))
    try:
        success = await manager._deliver_direct(session, "hello world")
        assert success is True
        assert seen_commands == ["get_epoch", "submit_message"]
        manager.tmux.send_input_async.assert_not_called()
        assert session.error_message is None
    finally:
        server.close()
        await server.wait_closed()
        if socket_path.exists():
            socket_path.unlink()


@pytest.mark.asyncio
async def test_deliver_direct_falls_back_to_tmux_when_control_unavailable(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="cfctl2",
        name="codex-fork-cfctl2",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.tmux.send_input_async = AsyncMock(return_value=True)

    success = await manager._deliver_direct(session, "fallback")
    assert success is True
    manager.tmux.send_input_async.assert_called_once()
    assert session.error_message is not None
    assert session.error_message.startswith("codex_fork_control_degraded:")


@pytest.mark.asyncio
async def test_deliver_direct_returns_failure_when_fallback_disabled(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    manager.codex_fork_control_tmux_fallback_enabled = False
    session = Session(
        id="cfctl3",
        name="codex-fork-cfctl3",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    manager.tmux.send_input_async = AsyncMock(return_value=True)

    success = await manager._deliver_direct(session, "no fallback")
    assert success is False
    manager.tmux.send_input_async.assert_not_called()
    assert session.error_message is not None
    assert session.error_message.startswith("codex_fork_control_degraded:")
