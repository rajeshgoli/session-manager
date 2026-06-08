"""Opt-in Rust shadow comparison support for the migration cutover."""

from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import logging
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

import httpx
from starlette.types import ASGIApp, Message, Receive, Scope, Send

logger = logging.getLogger(__name__)

DEFAULT_SHADOW_ENDPOINT = "http://127.0.0.1:8421/__shadow/http"
DEFAULT_LEDGER_PATH = "~/.local/share/claude-sessions/rust_shadow.jsonl"
DEFAULT_MAX_BODY_BYTES = 64 * 1024
DEFAULT_TIMEOUT_SECONDS = 0.5

SENSITIVE_HEADER_NAMES = {
    "authorization",
    "cookie",
    "set-cookie",
    "x-email-worker-secret",
    "x-sm-device-signature",
    "x-sm-hook-secret",
    "x-sm-node-token",
}

SAFE_HEADER_NAMES = {
    "accept",
    "content-type",
    "host",
    "user-agent",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-sm-shadow-mode",
}

BODY_CONTENT_TYPE_PREFIXES = (
    "application/json",
    "application/x-www-form-urlencoded",
    "text/",
)


class RustShadowMiddleware:
    """Mirror completed Python HTTP requests to Rust without changing authority.

    Python remains the only writer. The Rust side receives a bounded, sanitized
    request/response envelope at a dedicated shadow endpoint and returns a
    comparison classification. This middleware records only hashes and metadata
    in the local ledger, not raw request or response bodies.
    """

    def __init__(self, app: ASGIApp, config: Optional[dict[str, Any]] = None):
        self.app = app
        shadow_config = (config or {}).get("rust_shadow") or {}
        self.enabled = bool(shadow_config.get("enabled", False))
        self.endpoint = str(shadow_config.get("endpoint") or DEFAULT_SHADOW_ENDPOINT)
        self.secret = str(shadow_config.get("secret") or "")
        self.ledger_path = Path(
            str(shadow_config.get("ledger_path") or DEFAULT_LEDGER_PATH)
        ).expanduser()
        self.timeout_seconds = float(
            shadow_config.get("timeout_seconds", DEFAULT_TIMEOUT_SECONDS)
        )
        self.max_body_bytes = int(
            shadow_config.get("max_body_bytes", DEFAULT_MAX_BODY_BYTES)
        )
        self.await_completion_for_tests = bool(
            shadow_config.get("await_completion_for_tests", False)
        )

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if not self.enabled or scope["type"] != "http":
            await self.app(scope, receive, send)
            return

        method = str(scope.get("method") or "").upper()
        path = str(scope.get("path") or "")
        query_string = _decode_bytes(scope.get("query_string", b""))
        request_headers = _headers_from_scope(scope)
        request_capture = _CaptureBuffer(self.max_body_bytes)
        response_capture = _CaptureBuffer(self.max_body_bytes)
        response_status: Optional[int] = None
        response_headers: dict[str, str] = {}

        async def receive_wrapper() -> Message:
            message = await receive()
            if message["type"] == "http.request":
                request_capture.append(message.get("body", b""))
            return message

        async def send_wrapper(message: Message) -> None:
            nonlocal response_status, response_headers
            if message["type"] == "http.response.start":
                response_status = int(message["status"])
                response_headers = _headers_from_message(message)
            elif message["type"] == "http.response.body":
                response_capture.append(message.get("body", b""))

            await send(message)

            if message["type"] == "http.response.body" and not message.get(
                "more_body", False
            ):
                envelope = self._build_envelope(
                    method=method,
                    path=path,
                    query_string=query_string,
                    request_headers=request_headers,
                    request_capture=request_capture,
                    response_status=response_status,
                    response_headers=response_headers,
                    response_capture=response_capture,
                )
                if envelope is not None:
                    await self._dispatch_shadow(envelope)

        await self.app(scope, receive_wrapper, send_wrapper)

    def _build_envelope(
        self,
        *,
        method: str,
        path: str,
        query_string: str,
        request_headers: dict[str, str],
        request_capture: "_CaptureBuffer",
        response_status: Optional[int],
        response_headers: dict[str, str],
        response_capture: "_CaptureBuffer",
    ) -> Optional[dict[str, Any]]:
        if response_status is None or not _is_shadowable_http(
            method=method,
            path=path,
            request_headers=request_headers,
            response_headers=response_headers,
        ):
            return None

        body_content_type = request_headers.get("content-type", "")
        include_request_body = _is_shadowable_body_content_type(body_content_type)
        request_body = request_capture.bytes if include_request_body else b""

        return {
            "schema_version": 1,
            "observed_at": _now_iso(),
            "request": {
                "method": method,
                "path": path,
                "query_string": query_string,
                "headers": _sanitize_headers(request_headers),
                "body_sha256": request_capture.sha256_hex,
                "body_base64": base64.b64encode(request_body).decode("ascii")
                if request_body and not request_capture.truncated
                else None,
                "body_truncated": request_capture.truncated,
                "body_omitted": not include_request_body or request_capture.truncated,
            },
            "python_response": {
                "status": response_status,
                "headers": _sanitize_headers(response_headers),
                "body_sha256": response_capture.sha256_hex,
                "body_truncated": response_capture.truncated,
            },
        }

    async def _dispatch_shadow(self, envelope: dict[str, Any]) -> None:
        if self.await_completion_for_tests:
            await self._post_and_record(envelope)
            return
        asyncio.create_task(self._post_and_record(envelope))

    async def _post_and_record(self, envelope: dict[str, Any]) -> None:
        started_at = asyncio.get_running_loop().time()
        record: dict[str, Any] = {
            "schema_version": 1,
            "observed_at": envelope["observed_at"],
            "method": envelope["request"]["method"],
            "path": envelope["request"]["path"],
            "query_string": envelope["request"]["query_string"],
            "python_status": envelope["python_response"]["status"],
            "python_body_sha256": envelope["python_response"]["body_sha256"],
        }
        try:
            headers = {}
            if self.secret:
                headers["x-sm-rust-shadow-secret"] = self.secret
            async with httpx.AsyncClient(timeout=self.timeout_seconds) as client:
                response = await client.post(self.endpoint, json=envelope, headers=headers)
            record["rust_http_status"] = response.status_code
            try:
                record["rust_result"] = response.json()
            except ValueError:
                record["rust_result"] = {
                    "comparison": "shadow_endpoint_non_json",
                    "body_preview": response.text[:200],
                }
        except Exception as exc:  # pragma: no cover - exact transport errors vary
            logger.debug("Rust shadow request failed: %s", exc)
            record["shadow_error"] = type(exc).__name__
            record["shadow_error_message"] = str(exc)[:200]
        finally:
            elapsed_ms = (asyncio.get_running_loop().time() - started_at) * 1000
            record["shadow_elapsed_ms"] = round(elapsed_ms, 3)
            await asyncio.to_thread(self._append_ledger, record)

    def _append_ledger(self, record: dict[str, Any]) -> None:
        self.ledger_path.parent.mkdir(parents=True, exist_ok=True)
        with self.ledger_path.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(record, sort_keys=True, separators=(",", ":")))
            handle.write("\n")


class _CaptureBuffer:
    def __init__(self, max_bytes: int):
        self.max_bytes = max(0, max_bytes)
        self._buffer = bytearray()
        self._hasher = hashlib.sha256()
        self.truncated = False

    @property
    def bytes(self) -> bytes:
        return bytes(self._buffer)

    @property
    def sha256_hex(self) -> str:
        return self._hasher.hexdigest()

    def append(self, chunk: bytes) -> None:
        self._hasher.update(chunk)
        if not chunk or self.max_bytes == 0:
            self.truncated = self.truncated or bool(chunk)
            return
        remaining = self.max_bytes - len(self._buffer)
        if remaining <= 0:
            self.truncated = True
            return
        self._buffer.extend(chunk[:remaining])
        if len(chunk) > remaining:
            self.truncated = True


def _is_shadowable_http(
    *,
    method: str,
    path: str,
    request_headers: dict[str, str],
    response_headers: dict[str, str],
) -> bool:
    if method not in {"GET", "POST", "PUT", "PATCH", "DELETE"}:
        return False
    if path.startswith("/__shadow"):
        return False
    if path == "/events" or path.startswith("/client/terminal"):
        return False
    request_content_type = request_headers.get("content-type", "")
    response_content_type = response_headers.get("content-type", "")
    if request_content_type.startswith("multipart/"):
        return False
    if response_content_type.startswith("text/event-stream"):
        return False
    return True


def _is_shadowable_body_content_type(content_type: str) -> bool:
    if not content_type:
        return False
    normalized = content_type.split(";", 1)[0].strip().lower()
    return any(
        normalized == prefix.rstrip("/") or normalized.startswith(prefix)
        for prefix in BODY_CONTENT_TYPE_PREFIXES
    )


def _headers_from_scope(scope: Scope) -> dict[str, str]:
    headers: dict[str, str] = {}
    for raw_name, raw_value in scope.get("headers", []):
        name = _decode_bytes(raw_name).lower()
        if name not in headers:
            headers[name] = _decode_bytes(raw_value)
    return headers


def _headers_from_message(message: Message) -> dict[str, str]:
    headers: dict[str, str] = {}
    for raw_name, raw_value in message.get("headers", []):
        name = _decode_bytes(raw_name).lower()
        if name not in headers:
            headers[name] = _decode_bytes(raw_value)
    return headers


def _sanitize_headers(headers: dict[str, str]) -> dict[str, str]:
    sanitized: dict[str, str] = {}
    for name, value in headers.items():
        normalized = name.lower()
        if normalized in SENSITIVE_HEADER_NAMES:
            continue
        if normalized in SAFE_HEADER_NAMES:
            sanitized[normalized] = value[:512]
    return sanitized


def _decode_bytes(value: bytes | str) -> str:
    if isinstance(value, str):
        return value
    return value.decode("latin-1", errors="replace")


def _now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()
