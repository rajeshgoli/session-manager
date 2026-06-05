import asyncio
import json
import shutil
import subprocess
import tempfile
import time
from pathlib import Path
from unittest.mock import AsyncMock, Mock, patch

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


def test_node_agent_websocket_reregisters_active_sessions_after_hello_ok(tmp_path):
    manager = _manager(tmp_path)
    session = Session(
        id="active-remote",
        name="codex-fork-active-remote",
        working_dir=str(tmp_path),
        provider="codex-fork",
        node="worker",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session
    client = TestClient(create_app(session_manager=manager))

    with client.websocket_connect("/nodes/agent") as websocket:
        websocket.send_json({"type": "hello", "node_id": "worker", "secret": "node-secret"})
        assert websocket.receive_json() == {"type": "hello_ok", "node_id": "worker"}

        frame = websocket.receive_json()
        assert frame["type"] == "register"
        assert frame["session_id"] == session.id
        assert frame["event_stream_path"] == "/tmp/worker-sm/active-remote.codex-fork.events.jsonl"
        assert frame["control_socket_path"] == "/tmp/worker-sm/active-remote.codex-fork.control.sock"

        websocket.send_json({"type": "registered", "session_id": session.id})
        deadline = time.monotonic() + 1.0
        while time.monotonic() < deadline:
            if manager.codex_fork_node_agents._session_nodes.get(session.id) == "worker":
                break
            time.sleep(0.01)

        assert manager.codex_fork_node_agents._session_nodes.get(session.id) == "worker"


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
async def test_node_agent_connection_requests_restore_inventory():
    class FakeWebSocket:
        def __init__(self):
            self.sent: list[dict] = []

        async def send_json(self, frame: dict):
            self.sent.append(frame)

    websocket = FakeWebSocket()
    connection = NodeAgentConnection("worker", websocket)

    inventory_task = asyncio.create_task(connection.restore_inventory(timeout=1.0))
    await asyncio.sleep(0)

    request = websocket.sent[0]
    assert request["type"] == "restore_inventory"
    await connection.handle_frame(
        {
            "type": "restore_inventory_result",
            "request_id": request["request_id"],
            "ok": True,
            "node_id": "worker",
            "sessions": [{"id": "old12345", "status": "stopped"}],
        }
    )

    result = await inventory_task
    assert result["sessions"] == [{"id": "old12345", "status": "stopped"}]


@pytest.mark.asyncio
async def test_node_agent_restore_inventory_reads_stopped_state_records(tmp_path):
    state_file = tmp_path / "sessions.json"
    stopped = Session(
        id="old12345",
        name="codex-fork-old12345",
        working_dir=str(tmp_path),
        tmux_session="codex-fork-old12345",
        log_file=str(tmp_path / "old12345.log"),
        provider="codex-fork",
        status=SessionStatus.STOPPED,
        provider_resume_id="resume-old12345",
    )
    running = Session(
        id="live1234",
        name="codex-fork-live1234",
        working_dir=str(tmp_path),
        tmux_session="codex-fork-live1234",
        log_file=str(tmp_path / "live1234.log"),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    state_file.write_text(
        json.dumps(
            {
                "sessions": [stopped.to_dict(), running.to_dict()],
                "agent_registrations": [],
                "adoption_proposals": [],
            }
        )
    )

    class FakeWebSocket:
        def __init__(self):
            self.sent: list[str] = []

        async def send(self, frame: str):
            self.sent.append(frame)

    websocket = FakeWebSocket()
    agent = CodexForkNodeAgent(
        node_id="worker",
        primary_url="http://primary:8420",
        secret="node-secret",
        state_file=str(state_file),
    )

    await agent._restore_inventory(websocket, {"type": "restore_inventory", "request_id": "req1"})

    payload = json.loads(websocket.sent[0])
    assert payload["type"] == "restore_inventory_result"
    assert payload["ok"] is True
    assert [session["id"] for session in payload["sessions"]] == ["old12345"]
    candidate = payload["sessions"][0]
    assert candidate["node"] == "worker"
    assert candidate["origin_node"] == "worker"
    assert candidate["source_session_id"] == "old12345"
    assert candidate["provider_resume_id"] == "resume-old12345"


@pytest.mark.asyncio
async def test_node_agent_restore_inventory_falls_back_to_legacy_state_file(tmp_path):
    default_state_file = tmp_path / "new" / "sessions.json"
    legacy_state_file = tmp_path / "legacy" / "sessions.json"
    legacy_state_file.parent.mkdir(parents=True)
    stopped = Session(
        id="legacy123",
        name="claude-legacy123",
        working_dir=str(tmp_path),
        tmux_session="claude-legacy123",
        log_file=str(tmp_path / "legacy123.log"),
        provider="claude",
        status=SessionStatus.STOPPED,
        provider_resume_id="resume-legacy123",
    )
    legacy_state_file.write_text(
        json.dumps(
            {
                "sessions": [stopped.to_dict()],
                "agent_registrations": [],
                "adoption_proposals": [],
            }
        )
    )

    class FakeWebSocket:
        def __init__(self):
            self.sent: list[str] = []

        async def send(self, frame: str):
            self.sent.append(frame)

    with (
        patch("src.node_agent.DEFAULT_NODE_STATE_FILE", str(default_state_file)),
        patch("src.node_agent.LEGACY_NODE_STATE_FILE", str(legacy_state_file)),
    ):
        agent = CodexForkNodeAgent(
            node_id="worker",
            primary_url="http://primary:8420",
            secret="node-secret",
        )

    websocket = FakeWebSocket()
    await agent._restore_inventory(websocket, {"type": "restore_inventory", "request_id": "req1"})

    payload = json.loads(websocket.sent[0])
    assert payload["ok"] is True
    assert payload["state_file"] == str(legacy_state_file)
    assert [session["id"] for session in payload["sessions"]] == ["legacy123"]
    assert payload["sessions"][0]["provider_resume_id"] == "resume-legacy123"


@pytest.mark.asyncio
async def test_session_manager_imports_node_restore_candidate(tmp_path):
    manager = _manager(tmp_path)
    stopped = Session(
        id="importme",
        name="codex-fork-importme",
        working_dir=str(tmp_path),
        tmux_session="codex-fork-importme",
        log_file=str(tmp_path / "importme.log"),
        provider="codex-fork",
        node="worker",
        status=SessionStatus.STOPPED,
        provider_resume_id="resume-importme",
    )

    class InventoryConnection:
        def is_healthy(self):
            return True

        async def restore_inventory(self, timeout: float = 5.0):
            return {
                "ok": True,
                "node_id": "worker",
                "sessions": [stopped.to_dict()],
            }

    manager.codex_fork_node_agents._connections["worker"] = InventoryConnection()

    ok, candidates, error = await manager.list_node_restore_candidates("worker")
    assert ok is True
    assert error is None
    assert [candidate["id"] for candidate in candidates] == ["importme"]

    ok, imported, error = await manager.import_node_restore_candidate("worker", "importme")
    assert ok is True
    assert error is None
    assert imported is not None
    assert imported.id == "importme"
    assert imported.node == "worker"
    assert imported.provider_resume_id == "resume-importme"
    assert manager.get_session("importme") is imported


@pytest.mark.asyncio
async def test_session_manager_force_refreshes_node_restore_candidate_import(tmp_path):
    manager = _manager(tmp_path)
    stopped = Session(
        id="staleone",
        name="codex-fork-staleone",
        working_dir=str(tmp_path),
        tmux_session="codex-fork-staleone",
        log_file=str(tmp_path / "staleone.log"),
        provider="codex-fork",
        node="worker",
        status=SessionStatus.STOPPED,
    )

    class InventoryConnection:
        def __init__(self):
            self.sessions = [stopped.to_dict()]
            self.calls = 0

        def is_healthy(self):
            return True

        async def restore_inventory(self, timeout: float = 5.0):
            self.calls += 1
            return {
                "ok": True,
                "node_id": "worker",
                "sessions": list(self.sessions),
            }

    connection = InventoryConnection()
    manager.codex_fork_node_agents._connections["worker"] = connection

    ok, candidates, error = await manager.list_node_restore_candidates("worker")
    assert ok is True
    assert error is None
    assert [candidate["id"] for candidate in candidates] == ["staleone"]
    assert connection.calls == 1

    connection.sessions = []
    ok, imported, error = await manager.import_node_restore_candidate(
        "worker",
        "staleone",
        force_refresh=True,
    )

    assert ok is False
    assert imported is None
    assert error == "Session not found in node restore inventory"
    assert connection.calls == 2
    assert manager.get_session("staleone") is None


@pytest.mark.asyncio
async def test_session_manager_revalidates_cached_node_restore_inventory(tmp_path):
    manager = _manager(tmp_path)
    stopped = Session(
        id="cachedlive",
        name="codex-fork-cachedlive",
        working_dir=str(tmp_path),
        tmux_session="codex-fork-cachedlive",
        log_file=str(tmp_path / "cachedlive.log"),
        provider="codex-fork",
        node="worker",
        status=SessionStatus.STOPPED,
    )

    class InventoryConnection:
        def __init__(self):
            self.calls = 0

        def is_healthy(self):
            return True

        async def restore_inventory(self, timeout: float = 5.0):
            self.calls += 1
            return {
                "ok": True,
                "node_id": "worker",
                "sessions": [stopped.to_dict()],
            }

    connection = InventoryConnection()
    manager.codex_fork_node_agents._connections["worker"] = connection

    ok, candidates, error = await manager.list_node_restore_candidates("worker")
    assert ok is True
    assert error is None
    assert [candidate["id"] for candidate in candidates] == ["cachedlive"]
    assert connection.calls == 1

    running = Session.from_dict(stopped.to_dict())
    running.status = SessionStatus.RUNNING
    manager.sessions[running.id] = running
    ok, candidates, error = await manager.list_node_restore_candidates("worker")

    assert ok is True
    assert error is None
    assert candidates == []
    assert connection.calls == 1


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


@pytest.mark.asyncio
async def test_node_agent_reregister_suppresses_stale_tail_task_failure(tmp_path):
    class FakeWebSocket:
        def __init__(self):
            self.sent: list[str] = []

        async def send(self, frame: str):
            self.sent.append(frame)

    async def failed_tail():
        raise RuntimeError("stale websocket closed")

    log_dir = tmp_path / "node-log"
    log_dir.mkdir()
    event_path = log_dir / "session1.codex-fork.events.jsonl"
    control_path = log_dir / "session1.codex-fork.control.sock"
    websocket = FakeWebSocket()
    stale_registration = TailRegistration(
        websocket=websocket,
        session_id="session1",
        event_stream_path=event_path,
        control_socket_path=control_path,
        cursor=ProviderCursor(),
        poll_interval=0.01,
    )
    stale_registration.task = asyncio.create_task(failed_tail())
    await asyncio.sleep(0)

    agent = CodexForkNodeAgent(
        node_id="worker",
        primary_url="http://primary",
        secret="secret",
        log_dir=str(log_dir),
    )
    agent.registrations["session1"] = stale_registration

    await agent._register(
        websocket,
        {
            "type": "register",
            "session_id": "session1",
            "event_stream_path": str(event_path),
            "control_socket_path": str(control_path),
        },
    )
    await agent._unregister("session1")

    frame = json.loads(websocket.sent[-1])
    assert frame["type"] == "registered"
    assert frame["session_id"] == "session1"
    assert stale_registration.task is None


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


@pytest.mark.asyncio
async def test_remote_codex_fork_create_rejects_legacy_codex_fallback(tmp_path):
    manager = _manager(tmp_path)

    class HealthyConnection:
        def is_healthy(self):
            return True

    manager.codex_fork_node_agents._connections["worker"] = HealthyConnection()
    manager.node_runner.command_available = Mock(side_effect=[True, False])
    manager.tmux.create_session_with_command = Mock(return_value=True)

    session = await manager.create_session(
        working_dir=str(tmp_path),
        provider="codex-fork",
        node="worker",
    )

    assert session is None
    assert manager.last_create_error is not None
    assert "falling back to codex" in manager.last_create_error
    manager.tmux.create_session_with_command.assert_not_called()
