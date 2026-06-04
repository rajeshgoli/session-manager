"""Node-local codex-fork IPC bridge agent.

The agent dials the primary Session Manager over WebSocket, tails node-local
codex-fork event JSONL files, and relays control RPCs to node-local AF_UNIX
control sockets.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import json
import logging
import os
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional
from urllib.parse import urlparse, urlunparse

logger = logging.getLogger(__name__)


@dataclass
class ProviderCursor:
    session_epoch_key: Optional[str] = None
    seq: Optional[int] = None

    @classmethod
    def from_payload(cls, payload: Any) -> "ProviderCursor":
        if not isinstance(payload, dict):
            return cls()
        epoch_value = payload.get("session_epoch")
        epoch_key = payload.get("session_epoch_key")
        if not isinstance(epoch_key, str):
            epoch_key = _epoch_key(epoch_value) if epoch_value is not None else None
        seq = _coerce_seq(payload.get("seq"))
        return cls(session_epoch_key=epoch_key, seq=seq)

    def should_send(self, epoch_value: Any, seq: Optional[int]) -> bool:
        if seq is None:
            return True
        epoch_key = _epoch_key(epoch_value)
        if self.session_epoch_key is not None and epoch_key == self.session_epoch_key:
            return self.seq is None or seq > self.seq
        return True

    def note_sent(self, epoch_value: Any, seq: Optional[int]) -> Optional[dict[str, Any]]:
        if seq is None:
            return None
        epoch_key = _epoch_key(epoch_value)
        gap = None
        if self.session_epoch_key is not None and epoch_key == self.session_epoch_key:
            if self.seq is not None and seq > self.seq + 1:
                gap = {
                    "previous_seq": self.seq,
                    "next_seq": seq,
                    "session_epoch": epoch_value,
                }
        self.session_epoch_key = epoch_key
        self.seq = seq
        return gap


def _epoch_key(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def _coerce_seq(value: Any) -> Optional[int]:
    if isinstance(value, int):
        return value
    if isinstance(value, str) and value.isdigit():
        return int(value)
    return None


def _ws_url(primary_url: str) -> str:
    parsed = urlparse(primary_url)
    scheme = parsed.scheme
    if scheme == "http":
        scheme = "ws"
    elif scheme == "https":
        scheme = "wss"
    elif scheme not in {"ws", "wss"}:
        raise ValueError(f"Unsupported primary URL scheme: {parsed.scheme}")
    path = (parsed.path or "").rstrip("/") + "/nodes/agent"
    return urlunparse((scheme, parsed.netloc, path, "", "", ""))


class TailRegistration:
    def __init__(
        self,
        *,
        websocket: Any,
        session_id: str,
        event_stream_path: Path,
        control_socket_path: Path,
        cursor: ProviderCursor,
        poll_interval: float,
    ):
        self.websocket = websocket
        self.session_id = session_id
        self.event_stream_path = event_stream_path
        self.control_socket_path = control_socket_path
        self.cursor = cursor
        self.poll_interval = poll_interval
        self.offset = 0
        self.buffer = ""
        self.ready_sent = False
        self.task: Optional[asyncio.Task[Any]] = None

    def start(self) -> None:
        if self.task is not None and not self.task.done():
            return
        self.task = asyncio.create_task(self._tail_loop())

    async def stop(self) -> None:
        if self.task is None:
            return
        self.task.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await self.task

    async def _send(self, frame: dict[str, Any]) -> None:
        await self.websocket.send(json.dumps(frame, separators=(",", ":")))

    async def _tail_loop(self) -> None:
        while True:
            await self._report_ready_if_available()
            if self.event_stream_path.exists():
                try:
                    with self.event_stream_path.open("r", encoding="utf-8", errors="ignore") as handle:
                        handle.seek(self.offset)
                        chunk = handle.read()
                        self.offset = handle.tell()
                except OSError as exc:
                    logger.warning("Failed reading %s: %s", self.event_stream_path, exc)
                    chunk = ""
                if chunk:
                    await self._process_chunk(chunk)
            await asyncio.sleep(self.poll_interval)

    def control_ready(self) -> bool:
        return self.control_socket_path.exists() and self.control_socket_path.is_socket()

    async def _report_ready_if_available(self) -> None:
        if self.ready_sent:
            return
        if not self.event_stream_path.exists() or not self.control_ready():
            return
        self.ready_sent = True
        await self._send(
            {
                "type": "runtime_ready",
                "session_id": self.session_id,
                "event_stream_path": str(self.event_stream_path),
                "control_socket_path": str(self.control_socket_path),
            }
        )

    async def _process_chunk(self, chunk: str) -> None:
        self.buffer += chunk
        lines = self.buffer.splitlines()
        if self.buffer and not self.buffer.endswith("\n"):
            self.buffer = lines.pop() if lines else self.buffer
        else:
            self.buffer = ""

        for line in lines:
            raw = line.strip()
            if not raw:
                continue
            epoch_value = None
            seq = None
            try:
                parsed = json.loads(raw)
            except json.JSONDecodeError:
                parsed = None
            if isinstance(parsed, dict):
                epoch_value = parsed.get("session_epoch")
                seq = _coerce_seq(parsed.get("seq"))
            if not self.cursor.should_send(epoch_value, seq):
                continue
            gap = self.cursor.note_sent(epoch_value, seq)
            if gap is not None:
                await self._send({"type": "event_gap", "session_id": self.session_id, **gap})
            await self._send({"type": "event", "session_id": self.session_id, "line": raw})


class CodexForkNodeAgent:
    def __init__(
        self,
        *,
        node_id: str,
        primary_url: str,
        secret: Optional[str],
        poll_interval: float = 0.2,
        log_dir: Optional[str] = None,
    ):
        self.node_id = node_id
        self.primary_url = primary_url
        self.secret = secret
        self.poll_interval = poll_interval
        self.log_dir = Path(log_dir or "~/.local/share/claude-sessions").expanduser().resolve(strict=False)
        self.registrations: dict[str, TailRegistration] = {}

    async def run_forever(self) -> None:
        try:
            import websockets
        except ImportError as exc:
            raise RuntimeError("websockets package is required to run the node-agent") from exc

        url = _ws_url(self.primary_url)
        while True:
            try:
                async with websockets.connect(url) as websocket:
                    await self._run_connection(websocket)
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                logger.warning("node-agent connection failed: %s", exc)
                await asyncio.sleep(2)

    async def _run_connection(self, websocket: Any) -> None:
        await websocket.send(
            json.dumps(
                {
                    "type": "hello",
                    "node_id": self.node_id,
                    "secret": self.secret,
                },
                separators=(",", ":"),
            )
        )
        async for raw_message in websocket:
            try:
                frame = json.loads(raw_message)
            except json.JSONDecodeError:
                continue
            if not isinstance(frame, dict):
                continue
            await self._handle_frame(websocket, frame)

    async def _handle_frame(self, websocket: Any, frame: dict[str, Any]) -> None:
        frame_type = frame.get("type")
        if frame_type == "hello_ok":
            return
        if frame_type == "register":
            await self._register(websocket, frame)
            return
        if frame_type == "unregister":
            await self._unregister(str(frame.get("session_id") or ""))
            return
        if frame_type == "control":
            await self._control(websocket, frame)
            return

    async def _register(self, websocket: Any, frame: dict[str, Any]) -> None:
        session_id = str(frame.get("session_id") or "").strip()
        event_stream_path = str(frame.get("event_stream_path") or "").strip()
        control_socket_path = str(frame.get("control_socket_path") or "").strip()
        if not session_id or not event_stream_path or not control_socket_path:
            await self._send_register_failed(websocket, session_id, "session_id, event_stream_path, and control_socket_path are required")
            return
        event_path, event_error = self._resolve_under_log_dir(event_stream_path, "event_stream_path")
        if event_error:
            await self._send_register_failed(websocket, session_id, event_error)
            return
        control_path, control_error = self._resolve_under_log_dir(control_socket_path, "control_socket_path")
        if control_error:
            await self._send_register_failed(websocket, session_id, control_error)
            return
        await self._unregister(session_id)
        registration = TailRegistration(
            websocket=websocket,
            session_id=session_id,
            event_stream_path=event_path,
            control_socket_path=control_path,
            cursor=ProviderCursor.from_payload(frame.get("cursor")),
            poll_interval=self.poll_interval,
        )
        self.registrations[session_id] = registration
        registration.start()
        await websocket.send(
            json.dumps(
                {
                    "type": "registered",
                    "session_id": session_id,
                    "event_stream_path": str(event_path),
                    "control_socket_path": str(control_path),
                },
                separators=(",", ":"),
            )
        )

    def _resolve_under_log_dir(self, raw_path: str, label: str) -> tuple[Optional[Path], Optional[str]]:
        path = Path(raw_path).expanduser()
        if not path.is_absolute():
            return None, f"{label} must be absolute or ~-relative under node log_dir"
        resolved = path.resolve(strict=False)
        if not resolved.is_relative_to(self.log_dir):
            return None, f"{label} must be under node log_dir {self.log_dir}"
        return resolved, None

    async def _send_register_failed(self, websocket: Any, session_id: str, error: str) -> None:
        await websocket.send(
            json.dumps(
                {
                    "type": "register_failed",
                    "session_id": session_id,
                    "error": error,
                },
                separators=(",", ":"),
            )
        )

    async def _unregister(self, session_id: str) -> None:
        registration = self.registrations.pop(session_id, None)
        if registration is not None:
            await registration.stop()

    async def _send_control_error(
        self,
        websocket: Any,
        *,
        request_id: str,
        code: str,
        message: str,
    ) -> None:
        await websocket.send(
            json.dumps(
                {
                    "type": "control_result",
                    "request_id": request_id,
                    "ok": False,
                    "error": {"code": code, "message": message},
                },
                separators=(",", ":"),
            )
        )

    async def _control(self, websocket: Any, frame: dict[str, Any]) -> None:
        request_id = str(frame.get("request_id") or "").strip()
        session_id = str(frame.get("session_id") or "").strip()
        request_frame = frame.get("frame") if isinstance(frame.get("frame"), dict) else None
        registration = self.registrations.get(session_id)
        if not request_id or registration is None or request_frame is None:
            await self._send_control_error(
                websocket,
                request_id=request_id,
                code="not_registered",
                message="session not registered",
            )
            return
        if not registration.control_ready():
            await self._send_control_error(
                websocket,
                request_id=request_id,
                code="not_ready",
                message=f"control socket not ready: {registration.control_socket_path}",
            )
            return

        try:
            reader, writer = await asyncio.open_unix_connection(str(registration.control_socket_path))
            try:
                writer.write((json.dumps(request_frame, separators=(",", ":")) + "\n").encode("utf-8"))
                await writer.drain()
                line = await reader.readline()
                if not line:
                    raise RuntimeError("control socket closed without response")
            finally:
                writer.close()
                with contextlib.suppress(Exception):
                    await writer.wait_closed()
        except Exception as exc:
            await self._send_control_error(
                websocket,
                request_id=request_id,
                code="control_failed",
                message=str(exc),
            )
            return

        await websocket.send(
            json.dumps(
                {
                    "type": "control_result",
                    "request_id": request_id,
                    "ok": True,
                    "line": line.decode("utf-8", errors="replace").rstrip("\n"),
                },
                separators=(",", ":"),
            )
        )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Run a Session Manager codex-fork node-agent")
    parser.add_argument("--node-id", required=True, help="Node id as configured on the primary")
    parser.add_argument(
        "--primary-url",
        default=os.environ.get("SM_API_URL") or os.environ.get("SM_HOOK_BASE_URL") or "http://localhost:8420",
        help="Primary Session Manager HTTP(S)/WS URL",
    )
    parser.add_argument("--secret", default=os.environ.get("SM_NODE_TOKEN") or os.environ.get("SM_HOOK_SECRET"))
    parser.add_argument(
        "--log-dir",
        default=os.environ.get("SM_NODE_LOG_DIR") or "~/.local/share/claude-sessions",
        help="Node-local base directory for codex-fork event/control artifacts",
    )
    parser.add_argument("--poll-interval", type=float, default=0.2)
    parser.add_argument("--log-level", default="INFO")
    return parser


def main(argv: Optional[list[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    logging.basicConfig(level=getattr(logging, str(args.log_level).upper(), logging.INFO))
    agent = CodexForkNodeAgent(
        node_id=args.node_id,
        primary_url=args.primary_url,
        secret=args.secret,
        poll_interval=args.poll_interval,
        log_dir=args.log_dir,
    )
    try:
        asyncio.run(agent.run_forever())
    except KeyboardInterrupt:
        return 130
    except Exception as exc:
        print(f"node-agent failed: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
