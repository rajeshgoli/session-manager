"""Codex app-server integration (JSON-RPC over stdio)."""

from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable, Optional

logger = logging.getLogger(__name__)


@dataclass
class CodexAppServerConfig:
    """Configuration for Codex app-server sessions."""
    command: str = "codex"
    args: list[str] = field(default_factory=list)
    default_model: Optional[str] = None
    approval_policy: Optional[str] = "never"  # AskForApproval enum
    sandbox: Optional[str] = "workspace-write"  # SandboxMode enum
    approval_decision: str = "decline"  # accept | acceptForSession | decline | cancel
    request_timeout_seconds: int = 60
    client_name: str = "session-manager"
    client_title: Optional[str] = "Claude Session Manager"
    client_version: str = "0.1.0"


class CodexAppServerError(RuntimeError):
    """Raised for Codex app-server protocol errors."""


class CodexAppServerSession:
    """
    Manages a single Codex app-server process and thread.

    Notes:
    - app-server notifications include streaming deltas via item/agentMessage/delta.
    - turn/completed does NOT include items, so we aggregate deltas by turnId.
    """

    def __init__(
        self,
        session_id: str,
        working_dir: str,
        config: CodexAppServerConfig,
        on_turn_complete: Callable[[str, str, str], Awaitable[None]],
        on_turn_started: Optional[Callable[[str, str], Awaitable[None]]] = None,
        on_turn_delta: Optional[Callable[[str, str, str], Awaitable[None]]] = None,
        on_review_complete: Optional[Callable[[str, str], Awaitable[None]]] = None,
        on_server_request: Optional[Callable[[str, int, str, dict[str, Any]], Awaitable[Optional[dict[str, Any]]]]] = None,
    ):
        self.session_id = session_id
        self.working_dir = working_dir
        self.config = config
        self.on_turn_complete = on_turn_complete
        self.on_turn_started = on_turn_started
        self.on_turn_delta = on_turn_delta
        self.on_review_complete = on_review_complete
        self.on_server_request = on_server_request

        self._proc: Optional[asyncio.subprocess.Process] = None
        self._reader_task: Optional[asyncio.Task] = None
        self._stderr_task: Optional[asyncio.Task] = None
        self._pending: dict[Any, asyncio.Future] = {}
        self._id_counter = 0

        self.thread_id: Optional[str] = None
        self._current_turn_id: Optional[str] = None
        self._turn_buffers: dict[str, list[str]] = {}
        self._review_in_progress: bool = False
        self._review_id: Optional[str] = None

    async def start(self, thread_id: Optional[str] = None, model: Optional[str] = None) -> str:
        """Start app-server and create or resume a thread. Returns thread_id."""
        if self._proc:
            return self.thread_id or ""

        cmd = [self.config.command, "app-server", *self.config.args]
        self._proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

        self._reader_task = asyncio.create_task(self._read_loop())
        self._stderr_task = asyncio.create_task(self._read_stderr())

        await self._initialize()

        if thread_id:
            result = await self._request("thread/resume", {"threadId": thread_id})
        else:
            params: dict[str, Any] = {
                "cwd": self.working_dir,
            }
            if model or self.config.default_model:
                params["model"] = model or self.config.default_model
            if self.config.approval_policy:
                params["approvalPolicy"] = self.config.approval_policy
            if self.config.sandbox:
                params["sandbox"] = self.config.sandbox
            result = await self._request("thread/start", params)

        thread = result.get("thread") if isinstance(result, dict) else None
        if not thread or not thread.get("id"):
            raise CodexAppServerError("Failed to start/resume Codex thread (missing id)")

        self.thread_id = thread["id"]
        logger.info(f"Codex app-server started for session {self.session_id} (thread={self.thread_id})")
        return self.thread_id

    async def start_new_thread(self, model: Optional[str] = None) -> str:
        """Start a new Codex thread on the existing app-server process."""
        if not self._proc:
            return await self.start(thread_id=None, model=model)

        params: dict[str, Any] = {
            "cwd": self.working_dir,
        }
        if model or self.config.default_model:
            params["model"] = model or self.config.default_model
        if self.config.approval_policy:
            params["approvalPolicy"] = self.config.approval_policy
        if self.config.sandbox:
            params["sandbox"] = self.config.sandbox

        result = await self._request("thread/start", params)
        thread = result.get("thread") if isinstance(result, dict) else None
        if not thread or not thread.get("id"):
            raise CodexAppServerError("Failed to start new Codex thread (missing id)")
        self.thread_id = thread["id"]
        logger.info(f"Codex new thread started for session {self.session_id} (thread={self.thread_id})")
        return self.thread_id

    async def send_user_turn(self, text: str, model: Optional[str] = None) -> str:
        """Send a user turn to Codex. Returns turn_id."""
        if not self.thread_id:
            raise CodexAppServerError("Codex thread not initialized")

        params: dict[str, Any] = {
            "threadId": self.thread_id,
            "input": [
                {
                    "type": "text",
                    "text": text,
                }
            ],
        }

        # Optional per-turn overrides
        if model:
            params["model"] = model

        result = await self._request("turn/start", params)
        turn = result.get("turn") if isinstance(result, dict) else None
        turn_id = turn.get("id") if turn else None
        if not turn_id:
            raise CodexAppServerError("turn/start response missing turn id")
        self._current_turn_id = turn_id
        self._turn_buffers.setdefault(turn_id, [])
        return turn_id

    async def interrupt_turn(self) -> bool:
        """Interrupt the current turn if one is active."""
        if not self.thread_id or not self._current_turn_id:
            return False

        try:
            await self._request(
                "turn/interrupt",
                {"threadId": self.thread_id, "turnId": self._current_turn_id},
            )
            return True
        except Exception as e:
            logger.warning(f"Failed to interrupt turn for session {self.session_id}: {e}")
            return False

    async def review_start(
        self,
        mode: str,
        base_branch: Optional[str] = None,
        commit_sha: Optional[str] = None,
        custom_prompt: Optional[str] = None,
        delivery: str = "inline",
    ) -> dict:
        """Start a code review via review/start RPC.

        Args:
            mode: Review mode (branch, uncommitted, commit, custom)
            base_branch: Target branch for branch mode
            commit_sha: Target SHA for commit mode
            custom_prompt: Free-form text for custom mode
            delivery: 'inline' (stream) or 'detached' (background)

        Returns:
            RPC response dict
        """
        if not self.thread_id:
            raise CodexAppServerError("Codex thread not initialized")

        # Build target object
        if mode == "branch":
            target: dict[str, Any] = {"type": "baseBranch", "branch": base_branch or "main"}
        elif mode == "uncommitted":
            target = {"type": "uncommittedChanges"}
        elif mode == "commit":
            if not commit_sha:
                raise CodexAppServerError("commit_sha required for commit review mode")
            target = {"type": "commit", "sha": commit_sha}
        elif mode == "custom":
            target = {"type": "custom"}
            if custom_prompt:
                target["prompt"] = custom_prompt
        else:
            raise CodexAppServerError(f"Unknown review mode: {mode}")

        params: dict[str, Any] = {
            "threadId": self.thread_id,
            "target": target,
            "delivery": delivery,
        }

        self._review_in_progress = True
        try:
            result = await self._request("review/start", params)
        except Exception:
            self._review_in_progress = False
            raise
        return result

    async def close(self):
        """Stop the app-server process."""
        if self._reader_task:
            self._reader_task.cancel()
        if self._stderr_task:
            self._stderr_task.cancel()
        if self._proc and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=3)
            except asyncio.TimeoutError:
                self._proc.kill()
        self._proc = None

    # -----------------------
    # JSON-RPC helpers
    # -----------------------
    async def _initialize(self):
        """Send initialize + initialized handshake."""
        params = {
            "clientInfo": {
                "name": self.config.client_name,
                "title": self.config.client_title,
                "version": self.config.client_version,
            }
        }
        await self._request("initialize", params)
        await self._notify("initialized", {})

    async def _notify(self, method: str, params: dict[str, Any]):
        msg = {"jsonrpc": "2.0", "method": method, "params": params}
        await self._send(msg)

    async def _request(self, method: str, params: dict[str, Any]) -> dict:
        self._id_counter += 1
        request_id = self._id_counter
        fut: asyncio.Future = asyncio.get_running_loop().create_future()
        self._pending[request_id] = fut

        msg = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        await self._send(msg)

        try:
            result = await asyncio.wait_for(fut, timeout=self.config.request_timeout_seconds)
            return result
        finally:
            self._pending.pop(request_id, None)

    async def _send(self, msg: dict[str, Any]):
        if not self._proc or not self._proc.stdin:
            raise CodexAppServerError("app-server process not running")
        data = json.dumps(msg) + "\n"
        self._proc.stdin.write(data.encode("utf-8"))
        await self._proc.stdin.drain()

    async def _read_loop(self):
        assert self._proc and self._proc.stdout
        while True:
            line = await self._proc.stdout.readline()
            if not line:
                break
            try:
                message = json.loads(line.decode("utf-8"))
            except json.JSONDecodeError:
                logger.debug("Codex app-server: invalid JSON line")
                continue

            # JSON-RPC response
            if "id" in message and ("result" in message or "error" in message):
                req_id = message.get("id")
                fut = self._pending.get(req_id)
                if fut and not fut.done():
                    if "error" in message:
                        fut.set_exception(CodexAppServerError(str(message["error"])))
                    else:
                        fut.set_result(message.get("result", {}))
                continue

            # JSON-RPC server request (requires a response)
            if "id" in message and "method" in message:
                await self._handle_server_request(message)
                continue

            # Notification
            if "method" in message:
                await self._handle_notification(message)

    async def _read_stderr(self):
        assert self._proc and self._proc.stderr
        while True:
            line = await self._proc.stderr.readline()
            if not line:
                break
            logger.debug(f"codex app-server stderr: {line.decode('utf-8').rstrip()}")

    async def _handle_notification(self, message: dict[str, Any]):
        method = message.get("method")
        params = message.get("params", {})

        if method == "turn/started":
            turn = params.get("turn", {})
            self._current_turn_id = turn.get("id")
            if self._current_turn_id:
                self._turn_buffers.setdefault(self._current_turn_id, [])
                if self.on_turn_started:
                    await self.on_turn_started(self.session_id, self._current_turn_id)
            return

        if method == "item/agentMessage/delta":
            turn_id = params.get("turnId")
            delta = params.get("delta", "")
            if turn_id:
                self._turn_buffers.setdefault(turn_id, []).append(delta)
                if self.on_turn_delta:
                    await self.on_turn_delta(self.session_id, turn_id, delta)
            return

        if method == "turn/completed":
            turn = params.get("turn", {})
            turn_id = turn.get("id")
            status = turn.get("status", "completed")
            text = ""
            if turn_id:
                text = "".join(self._turn_buffers.pop(turn_id, []))
                if self._current_turn_id == turn_id:
                    self._current_turn_id = None
            await self.on_turn_complete(self.session_id, text, status)
            return

        # Review lifecycle: item/started with enteredReviewMode
        if method == "item/started":
            item = params.get("item", {})
            item_type = item.get("type")
            if item_type == "enteredReviewMode":
                self._review_in_progress = True
                self._review_id = item.get("id")
                label = item.get("review", "")
                logger.info(f"Review started for session {self.session_id}: {label}")
            return

        # Review lifecycle: item/completed with exitedReviewMode
        if method == "item/completed":
            item = params.get("item", {})
            item_type = item.get("type")
            if item_type == "exitedReviewMode":
                review_text = item.get("review", "")
                self._review_in_progress = False
                self._review_id = None
                logger.info(f"Review completed for session {self.session_id}")
                if self.on_review_complete:
                    await self.on_review_complete(self.session_id, review_text)
            return

    async def _handle_server_request(self, message: dict[str, Any]):
        req_id = message.get("id")
        method = message.get("method")
        params = message.get("params", {})

        if self.on_server_request and isinstance(req_id, int):
            response = await self.on_server_request(self.session_id, req_id, method, params)
            if response is not None:
                await self._send({"jsonrpc": "2.0", "id": req_id, "result": response})
                return

        # Unknown / unsupported request: return error
        error = {
            "code": -32601,
            "message": f"Unsupported server request: {method}",
        }
        await self._send({"jsonrpc": "2.0", "id": req_id, "error": error})
