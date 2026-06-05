"""Primary-side remote codex-fork node-agent connection registry."""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import uuid
from abc import ABC, abstractmethod
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional

logger = logging.getLogger(__name__)


@dataclass(frozen=True)
class CodexForkBridgePaths:
    """Node-local codex-fork artifact paths registered with a node-agent."""

    event_stream_path: str
    control_socket_path: str


class CodexForkTransport(ABC):
    """Raw codex-fork IPC transport selected by the owning session's node."""

    @abstractmethod
    async def register_event_stream(self) -> Optional[asyncio.Queue[Optional[str]]]:
        """Prepare the event source for monitoring and return a queue for remote streams."""

    @abstractmethod
    async def control_roundtrip(self, request: dict[str, Any]) -> dict[str, Any]:
        """Perform one codex-fork control protocol request/response round-trip."""


class LocalCodexForkTransport(CodexForkTransport):
    """Primary-local transport using the existing filesystem tail and AF_UNIX socket."""

    def __init__(
        self,
        *,
        socket_path: Path,
        roundtrip: Callable[[Path, dict[str, Any]], Awaitable[dict[str, Any]]],
    ):
        self.socket_path = socket_path
        self._roundtrip = roundtrip

    async def register_event_stream(self) -> Optional[asyncio.Queue[Optional[str]]]:
        return None

    async def control_roundtrip(self, request: dict[str, Any]) -> dict[str, Any]:
        return await self._roundtrip(self.socket_path, request)


class RemoteCodexForkTransport(CodexForkTransport):
    """Remote transport over a node-agent WebSocket."""

    def __init__(
        self,
        *,
        registry: "NodeAgentRegistry",
        node_id: str,
        session_id: str,
        event_stream_path: str,
        control_socket_path: str,
        cursor: Optional[dict[str, Any]],
        control_timeout: float,
        registration_timeout: float = 5.0,
    ):
        self.registry = registry
        self.node_id = node_id
        self.session_id = session_id
        self.event_stream_path = event_stream_path
        self.control_socket_path = control_socket_path
        self.cursor = cursor
        self.control_timeout = control_timeout
        self.registration_timeout = registration_timeout

    async def register_event_stream(self) -> Optional[asyncio.Queue[Optional[str]]]:
        return await self.registry.register_session(
            node_id=self.node_id,
            session_id=self.session_id,
            event_stream_path=self.event_stream_path,
            control_socket_path=self.control_socket_path,
            cursor=self.cursor,
            timeout=self.registration_timeout,
        )

    async def control_roundtrip(self, request: dict[str, Any]) -> dict[str, Any]:
        return await self.registry.control_roundtrip(
            node_id=self.node_id,
            session_id=self.session_id,
            frame=request,
            timeout=self.control_timeout,
        )


class NodeAgentConnection:
    """One live node-initiated WebSocket bridge for codex-fork IPC."""

    def __init__(self, node_id: str, websocket: Any):
        self.node_id = node_id
        self.websocket = websocket
        self.connected = True
        self._send_lock = asyncio.Lock()
        self._register_lock = asyncio.Lock()
        self._event_queues: dict[str, asyncio.Queue[Optional[str]]] = {}
        self._pending_control: dict[str, asyncio.Future[dict[str, Any]]] = {}
        self._pending_register: dict[str, asyncio.Future[None]] = {}
        self._pending_restore_inventory: dict[str, asyncio.Future[dict[str, Any]]] = {}
        self._registered_paths: dict[str, CodexForkBridgePaths] = {}

    def is_healthy(self) -> bool:
        return self.connected

    def event_queue(self, session_id: str) -> asyncio.Queue[Optional[str]]:
        queue = self._event_queues.get(session_id)
        if queue is None:
            queue = asyncio.Queue()
            self._event_queues[session_id] = queue
        return queue

    async def send(self, frame: dict[str, Any]) -> None:
        if not self.connected:
            raise RuntimeError(f"Node-agent {self.node_id} is not connected")
        async with self._send_lock:
            await self.websocket.send_json(frame)

    async def register_session(
        self,
        *,
        session_id: str,
        event_stream_path: str,
        control_socket_path: str,
        cursor: Optional[dict[str, Any]] = None,
        timeout: float = 5.0,
    ) -> asyncio.Queue[Optional[str]]:
        async with self._register_lock:
            queue = self.event_queue(session_id)
            paths = CodexForkBridgePaths(
                event_stream_path=event_stream_path,
                control_socket_path=control_socket_path,
            )
            loop = asyncio.get_running_loop()
            future: asyncio.Future[None] = loop.create_future()
            self._pending_register[session_id] = future
            try:
                await self.send(
                    {
                        "type": "register",
                        "session_id": session_id,
                        "event_stream_path": event_stream_path,
                        "control_socket_path": control_socket_path,
                        "cursor": cursor,
                    }
                )
                await asyncio.wait_for(future, timeout=timeout)
                self._registered_paths[session_id] = paths
            except Exception:
                self._registered_paths.pop(session_id, None)
                raise
            finally:
                self._pending_register.pop(session_id, None)
            return queue

    async def unregister_session(self, session_id: str) -> None:
        self._registered_paths.pop(session_id, None)
        self._event_queues.pop(session_id, None)
        if self.connected:
            with contextlib.suppress(Exception):
                await self.send({"type": "unregister", "session_id": session_id})

    async def control_roundtrip(
        self,
        *,
        session_id: str,
        frame: dict[str, Any],
        timeout: float,
    ) -> dict[str, Any]:
        bridge_request_id = uuid.uuid4().hex
        loop = asyncio.get_running_loop()
        future: asyncio.Future[dict[str, Any]] = loop.create_future()
        self._pending_control[bridge_request_id] = future
        try:
            await self.send(
                {
                    "type": "control",
                    "request_id": bridge_request_id,
                    "session_id": session_id,
                    "frame": frame,
                    "timeout": timeout,
                }
            )
            return await asyncio.wait_for(future, timeout=timeout)
        finally:
            self._pending_control.pop(bridge_request_id, None)

    async def restore_inventory(self, *, timeout: float = 5.0) -> dict[str, Any]:
        request_id = uuid.uuid4().hex
        loop = asyncio.get_running_loop()
        future: asyncio.Future[dict[str, Any]] = loop.create_future()
        self._pending_restore_inventory[request_id] = future
        try:
            await self.send({"type": "restore_inventory", "request_id": request_id})
            return await asyncio.wait_for(future, timeout=timeout)
        finally:
            self._pending_restore_inventory.pop(request_id, None)

    async def handle_frame(self, frame: dict[str, Any]) -> None:
        frame_type = frame.get("type")
        if frame_type == "event":
            session_id = str(frame.get("session_id") or "").strip()
            line = frame.get("line")
            if not session_id or not isinstance(line, str):
                logger.debug("Skipping malformed node-agent event frame from %s", self.node_id)
                return
            await self.event_queue(session_id).put(line)
            return

        if frame_type == "event_gap":
            logger.warning(
                "Remote codex-fork event gap on node %s session %s: %s",
                self.node_id,
                frame.get("session_id"),
                frame,
            )
            return

        if frame_type == "registered":
            session_id = str(frame.get("session_id") or "").strip()
            future = self._pending_register.get(session_id)
            if future is not None and not future.done():
                future.set_result(None)
            return

        if frame_type == "register_failed":
            session_id = str(frame.get("session_id") or "").strip()
            future = self._pending_register.get(session_id)
            if future is not None and not future.done():
                future.set_exception(RuntimeError(str(frame.get("error") or "node-agent registration failed")))
            return

        if frame_type == "control_result":
            request_id = str(frame.get("request_id") or "").strip()
            future = self._pending_control.get(request_id)
            if future is None or future.done():
                return
            if frame.get("ok") is False:
                error = frame.get("error") or "remote control failed"
                if isinstance(error, dict):
                    future.set_result({"ok": False, "error": error})
                    return
                future.set_exception(RuntimeError(str(error)))
                return
            response = frame.get("response")
            if isinstance(response, dict):
                future.set_result(response)
                return
            line = frame.get("line")
            if isinstance(line, str):
                try:
                    future.set_result(json.loads(line))
                except json.JSONDecodeError as exc:
                    future.set_exception(RuntimeError(f"control socket returned invalid JSON: {exc}"))
                return
            future.set_exception(RuntimeError("remote control returned no response"))
            return

        if frame_type == "restore_inventory_result":
            request_id = str(frame.get("request_id") or "").strip()
            future = self._pending_restore_inventory.get(request_id)
            if future is None or future.done():
                return
            if frame.get("ok") is False:
                future.set_exception(RuntimeError(str(frame.get("error") or "restore inventory failed")))
                return
            future.set_result(frame)
            return

        if frame_type in {"runtime_ready", "pong"}:
            return

        logger.debug("Ignoring unknown node-agent frame from %s: %s", self.node_id, frame_type)

    async def close(self) -> None:
        self.connected = False
        for future in list(self._pending_control.values()):
            if not future.done():
                future.set_exception(RuntimeError(f"Node-agent {self.node_id} disconnected"))
        self._pending_control.clear()
        for future in list(self._pending_restore_inventory.values()):
            if not future.done():
                future.set_exception(RuntimeError(f"Node-agent {self.node_id} disconnected"))
        self._pending_restore_inventory.clear()
        for future in list(self._pending_register.values()):
            if not future.done():
                future.set_exception(RuntimeError(f"Node-agent {self.node_id} disconnected"))
        self._pending_register.clear()
        for queue in list(self._event_queues.values()):
            await queue.put(None)


class NodeAgentRegistry:
    """Tracks connected node-agent WebSockets and per-session bridges."""

    def __init__(self):
        self._connections: dict[str, NodeAgentConnection] = {}
        self._session_nodes: dict[str, str] = {}

    def is_connected(self, node_id: str) -> bool:
        connection = self._connections.get(node_id)
        return bool(connection and connection.is_healthy())

    def get(self, node_id: str) -> Optional[NodeAgentConnection]:
        connection = self._connections.get(node_id)
        if connection and connection.is_healthy():
            return connection
        return None

    async def attach(self, connection: NodeAgentConnection) -> None:
        previous = self._connections.get(connection.node_id)
        if previous is not None and previous is not connection:
            await previous.close()
        self._connections[connection.node_id] = connection

    async def detach(self, connection: NodeAgentConnection) -> None:
        current = self._connections.get(connection.node_id)
        if current is connection:
            self._connections.pop(connection.node_id, None)
        await connection.close()

    async def register_session(
        self,
        *,
        node_id: str,
        session_id: str,
        event_stream_path: str,
        control_socket_path: str,
        cursor: Optional[dict[str, Any]] = None,
        timeout: float = 5.0,
    ) -> asyncio.Queue[Optional[str]]:
        connection = self.get(node_id)
        if connection is None:
            raise RuntimeError(f"Node-agent for {node_id} is not connected")
        queue = await connection.register_session(
            session_id=session_id,
            event_stream_path=event_stream_path,
            control_socket_path=control_socket_path,
            cursor=cursor,
            timeout=timeout,
        )
        self._session_nodes[session_id] = node_id
        return queue

    async def unregister_session(self, session_id: str) -> None:
        node_id = self._session_nodes.pop(session_id, None)
        if not node_id:
            return
        connection = self.get(node_id)
        if connection is not None:
            await connection.unregister_session(session_id)

    async def control_roundtrip(
        self,
        *,
        node_id: str,
        session_id: str,
        frame: dict[str, Any],
        timeout: float,
    ) -> dict[str, Any]:
        connection = self.get(node_id)
        if connection is None:
            raise RuntimeError(f"Node-agent for {node_id} is not connected")
        return await connection.control_roundtrip(
            session_id=session_id,
            frame=frame,
            timeout=timeout,
        )

    async def restore_inventory(self, *, node_id: str, timeout: float = 5.0) -> dict[str, Any]:
        connection = self.get(node_id)
        if connection is None:
            raise RuntimeError(f"Node-agent for {node_id} is not connected")
        return await connection.restore_inventory(timeout=timeout)
