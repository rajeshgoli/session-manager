import asyncio
import json
import shutil
import subprocess
import tempfile
from pathlib import Path
from unittest.mock import AsyncMock, Mock

import pytest
from fastapi.testclient import TestClient

from src.codex_fork_remote import LocalCodexForkTransport, NodeAgentConnection, RemoteCodexForkTransport
from src.models import Session, SessionStatus
from src.node_agent import CodexForkNodeAgent, ProviderCursor, TailRegistration
from src.server import create_app
from src.session_manager import SessionManager


def _manager(tmp_path):
    return SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "state.json"),
        config={
            "nodes": {
                "registry": {
                    "worker": {
                        "ssh": "dev@example",
                        "node_token": "node-secret",
                        "log_dir": "/tmp/worker-sm",
                    }
                }
            }
        },
    )


def test_node_agent_websocket_authenticates_and_marks_node_healthy(tmp_path):
    manager = _manager(tmp_path)
    client = TestClient(create_app(session_manager=manager))

    with client.websocket_connect("/nodes/agent") as websocket:
        websocket.send_json({"type": "hello", "node_id": "worker", "secret": "node-secret"})
        assert websocket.receive_json() == {"type": "hello_ok", "node_id": "worker"}
        assert manager.is_codex_fork_node_agent_healthy("worker") is True

    assert manager.is_codex_fork_node_agent_healthy("worker") is False


def test_node_agent_websocket_rejects_invalid_secret(tmp_path):
    manager = _manager(tmp_path)
    client = TestClient(create_app(session_manager=manager))

    with client.websocket_connect("/nodes/agent") as websocket:
        websocket.send_json({"type": "hello", "node_id": "worker", "secret": "wrong"})
        assert websocket.receive_json() == {"type": "error", "message": "Invalid node-agent secret"}

    assert manager.is_codex_fork_node_agent_healthy("worker") is False


@pytest.mark.asyncio
async def test_node_agent_connection_waits_for_registered_ack_before_returning_queue():
    class FakeWebSocket:
        def __init__(self):
            self.sent: list[dict] = []

        async def send_json(self, frame: dict):
            self.sent.append(frame)

    websocket = FakeWebSocket()
    connection = NodeAgentConnection("worker", websocket)

    register_task = asyncio.create_task(
        connection.register_session(
            session_id="session1",
            event_stream_path="/tmp/session1.events.jsonl",
            control_socket_path="/tmp/session1.control.sock",
            timeout=1.0,
        )
    )
    await asyncio.sleep(0)

    assert websocket.sent == [
        {
            "type": "register",
            "session_id": "session1",
            "event_stream_path": "/tmp/session1.events.jsonl",
            "control_socket_path": "/tmp/session1.control.sock",
            "cursor": None,
        }
    ]
    assert register_task.done() is False

    await connection.handle_frame({"type": "registered", "session_id": "session1"})
    queue = await register_task

    assert queue is connection.event_queue("session1")


@pytest.mark.asyncio
async def test_node_agent_connection_surfaces_register_failure():
    class FakeWebSocket:
        def __init__(self):
            self.sent: list[dict] = []

        async def send_json(self, frame: dict):
            self.sent.append(frame)

    websocket = FakeWebSocket()
    connection = NodeAgentConnection("worker", websocket)

    register_task = asyncio.create_task(
        connection.register_session(
            session_id="session1",
            event_stream_path="/tmp/session1.events.jsonl",
            control_socket_path="/tmp/session1.control.sock",
            timeout=1.0,
        )
    )
    await asyncio.sleep(0)

    await connection.handle_frame(
        {
            "type": "register_failed",
            "session_id": "session1",
            "error": "event_stream_path must be under node log_dir /tmp/worker-sm",
        }
    )

    with pytest.raises(RuntimeError, match="under node log_dir"):
        await register_task


@pytest.mark.asyncio
async def test_node_agent_connection_forwards_control_timeout():
    class FakeWebSocket:
        def __init__(self):
            self.sent: list[dict] = []

        async def send_json(self, frame: dict):
            self.sent.append(frame)

    websocket = FakeWebSocket()
    connection = NodeAgentConnection("worker", websocket)

    control_task = asyncio.create_task(
        connection.control_roundtrip(
            session_id="session1",
            frame={"command": "get_epoch"},
            timeout=0.75,
        )
    )
    await asyncio.sleep(0)

    request = websocket.sent[0]
    assert request["type"] == "control"
    assert request["session_id"] == "session1"
    assert request["timeout"] == 0.75

    await connection.handle_frame(
        {
            "type": "control_result",
            "request_id": request["request_id"],
            "ok": True,
            "line": '{"ok":true}',
        }
    )

    assert await control_task == {"ok": True}


@pytest.mark.asyncio
async def test_stale_node_agent_detach_does_not_mark_reconnected_sessions_unreachable(tmp_path):
    class FakeWebSocket:
        async def send_json(self, frame: dict):
            return None

    manager = _manager(tmp_path)
    session = Session(
        id="reconnect1",
        name="codex-fork-reconnect1",
        working_dir=str(tmp_path),
        provider="codex-fork",
        node="worker",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    old_connection = NodeAgentConnection("worker", FakeWebSocket())
    new_connection = NodeAgentConnection("worker", FakeWebSocket())

    await manager.attach_codex_fork_node_agent(old_connection)
    await manager.attach_codex_fork_node_agent(new_connection)
    await manager.detach_codex_fork_node_agent(old_connection)

    assert manager.is_codex_fork_node_agent_healthy("worker") is True
    assert manager.is_session_node_unreachable(session.id) is False


@pytest.mark.asyncio
async def test_node_agent_rejects_registered_paths_outside_log_dir(tmp_path):
    class FakeWebSocket:
        def __init__(self):
            self.sent: list[str] = []

        async def send(self, frame: str):
            self.sent.append(frame)

    log_dir = tmp_path / "node-log"
    websocket = FakeWebSocket()
    agent = CodexForkNodeAgent(
        node_id="worker",
        primary_url="http://primary",
        secret="secret",
        log_dir=str(log_dir),
    )

    await agent._register(
        websocket,
        {
            "type": "register",
            "session_id": "session1",
            "event_stream_path": str(tmp_path / "outside.events.jsonl"),
            "control_socket_path": str(log_dir / "session1.control.sock"),
        },
    )

    assert "session1" not in agent.registrations
    frame = json.loads(websocket.sent[-1])
    assert frame["type"] == "register_failed"
    assert frame["session_id"] == "session1"
    assert "under node log_dir" in frame["error"]


@pytest.mark.asyncio
async def test_node_agent_control_before_socket_ready_returns_not_ready(tmp_path):
    class FakeWebSocket:
        def __init__(self):
            self.sent: list[str] = []

        async def send(self, frame: str):
            self.sent.append(frame)

    log_dir = tmp_path / "node-log"
    log_dir.mkdir()
    event_path = log_dir / "session1.codex-fork.events.jsonl"
    control_path = log_dir / "session1.codex-fork.control.sock"
    websocket = FakeWebSocket()
    agent = CodexForkNodeAgent(
        node_id="worker",
        primary_url="http://primary",
        secret="secret",
        log_dir=str(log_dir),
    )

    await agent._register(
        websocket,
        {
            "type": "register",
            "session_id": "session1",
            "event_stream_path": str(event_path),
            "control_socket_path": str(control_path),
        },
    )
    await agent._control(
        websocket,
        {
            "type": "control",
            "request_id": "request1",
            "session_id": "session1",
            "frame": {"command": "get_epoch"},
        },
    )
    await agent._unregister("session1")

    frame = json.loads(websocket.sent[-1])
    assert frame["type"] == "control_result"
    assert frame["ok"] is False
    assert frame["error"]["code"] == "not_ready"
    assert "control socket not ready" in frame["error"]["message"]


@pytest.mark.asyncio
async def test_node_agent_control_timeout_closes_unresponsive_socket():
    class FakeWebSocket:
        def __init__(self):
            self.sent: list[str] = []

        async def send(self, frame: str):
            self.sent.append(frame)

    log_dir = Path(tempfile.mkdtemp(prefix="smna-", dir="/tmp"))
    event_path = log_dir / "session1.codex-fork.events.jsonl"
    control_path = log_dir / "s.sock"
    client_closed = asyncio.Event()

    async def handle_control(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        try:
            await reader.readline()
            if await reader.read() == b"":
                client_closed.set()
        finally:
            writer.close()
            await writer.wait_closed()

    server = await asyncio.start_unix_server(handle_control, path=str(control_path))
    websocket = FakeWebSocket()
    agent = CodexForkNodeAgent(
        node_id="worker",
        primary_url="http://primary",
        secret="secret",
        log_dir=str(log_dir),
        control_timeout=0.05,
    )
    agent.registrations["session1"] = TailRegistration(
        websocket=websocket,
        session_id="session1",
        event_stream_path=event_path,
        control_socket_path=control_path,
        cursor=ProviderCursor(),
        poll_interval=0.01,
    )

    try:
        await asyncio.wait_for(
            agent._control(
                websocket,
                {
                    "type": "control",
                    "request_id": "request1",
                    "session_id": "session1",
                    "frame": {"command": "get_epoch"},
                    "timeout": 0.05,
                },
            ),
            timeout=2.0,
        )
        await asyncio.wait_for(client_closed.wait(), timeout=1.0)
    finally:
        server.close()
        await server.wait_closed()
        shutil.rmtree(log_dir, ignore_errors=True)

    frame = json.loads(websocket.sent[-1])
    assert frame["type"] == "control_result"
    assert frame["ok"] is False
    assert frame["error"]["code"] == "control_failed"
    assert "timed out" in frame["error"]["message"]


def test_codex_fork_transport_selection_by_node(tmp_path):
    manager = _manager(tmp_path)
    local_session = Session(
        id="localtransport",
        name="codex-fork-localtransport",
        working_dir=str(tmp_path),
        provider="codex-fork",
        node="primary",
    )
    remote_session = Session(
        id="remotetransport",
        name="codex-fork-remotetransport",
        working_dir=str(tmp_path),
        provider="codex-fork",
        node="worker",
    )

    assert isinstance(manager._codex_fork_transport_for_session(local_session), LocalCodexForkTransport)
    assert isinstance(manager._codex_fork_transport_for_session(remote_session), RemoteCodexForkTransport)


@pytest.mark.asyncio
async def test_node_agent_buffers_partial_event_lines(tmp_path):
    class FakeWebSocket:
        def __init__(self):
            self.sent: list[str] = []

        async def send(self, frame: str):
            self.sent.append(frame)

    registration = TailRegistration(
        websocket=FakeWebSocket(),
        session_id="session1",
        event_stream_path=tmp_path / "session1.events.jsonl",
        control_socket_path=tmp_path / "session1.control.sock",
        cursor=ProviderCursor(),
        poll_interval=0.01,
    )

    await registration._process_chunk('{"session_epoch":"epoch1",')
    assert registration.websocket.sent == []

    await registration._process_chunk('"seq":1,"event_type":"turn_started"}\n')
    assert [json.loads(frame)["type"] for frame in registration.websocket.sent] == ["event"]
    event_frame = json.loads(registration.websocket.sent[0])
    assert event_frame["session_id"] == "session1"
    assert json.loads(event_frame["line"])["seq"] == 1


@pytest.mark.asyncio
async def test_restore_remote_codex_fork_registers_bridge_before_launch_and_uses_node_paths(tmp_path):
    manager = _manager(tmp_path)
    session = Session(
        id="restoreremote1",
        name="codex-fork-restoreremote1",
        working_dir=str(tmp_path),
        provider="codex-fork",
        node="worker",
        status=SessionStatus.STOPPED,
        provider_resume_id="resume-restoreremote1",
    )
    manager.sessions[session.id] = session

    class HealthyConnection:
        def is_healthy(self):
            return True

    manager.codex_fork_node_agents._connections["worker"] = HealthyConnection()
    manager.node_runner.command_available = Mock(return_value=True)
    manager.node_runner.run = Mock(
        return_value=subprocess.CompletedProcess(args=[], returncode=0, stdout="", stderr="")
    )
    manager._register_codex_fork_remote_bridge = AsyncMock()
    manager._start_codex_fork_event_monitor = Mock()
    manager.tmux.session_exists = Mock(return_value=False)
    manager.tmux.create_session_with_command = Mock(return_value=True)

    success, restored, error = await manager.restore_session(session.id)

    assert success is True
    assert restored is session
    assert error is None
    manager._register_codex_fork_remote_bridge.assert_awaited_once_with(session)
    manager._start_codex_fork_event_monitor.assert_called_once_with(session)
    manager.tmux.create_session_with_command.assert_called_once()
    _, kwargs = manager.tmux.create_session_with_command.call_args
    assert kwargs["node"] == "worker"
    args = kwargs["args"]
    event_stream_path = args[args.index("--event-stream") + 1]
    control_socket_path = args[args.index("--control-socket") + 1]
    assert event_stream_path == "/tmp/worker-sm/restoreremote1.codex-fork.events.jsonl"
    assert control_socket_path == "/tmp/worker-sm/restoreremote1.codex-fork.control.sock"
