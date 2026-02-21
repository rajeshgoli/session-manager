"""Durable structured-request ledger for codex-app approval and user-input requests."""

from __future__ import annotations

import asyncio
import json
import logging
import sqlite3
import threading
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Optional

logger = logging.getLogger(__name__)


class CodexRequestLedger:
    """Persist codex structured requests and resolve them idempotently."""

    def __init__(self, db_path: str, process_generation: Optional[str] = None):
        self.db_path = Path(db_path).expanduser()
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self.process_generation = process_generation or uuid.uuid4().hex[:12]

        self._conn: Optional[sqlite3.Connection] = None
        self._db_lock = threading.Lock()

        self._pending_futures: dict[str, asyncio.Future] = {}
        self._expiry_tasks: dict[str, asyncio.Task] = {}

        self._init_db()

    def _get_conn(self) -> sqlite3.Connection:
        if self._conn is None:
            self._conn = sqlite3.connect(str(self.db_path), check_same_thread=False)
            self._conn.execute("PRAGMA journal_mode=WAL")
            self._conn.execute("PRAGMA busy_timeout=5000")
        return self._conn

    def _init_db(self):
        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                CREATE TABLE IF NOT EXISTS codex_pending_requests (
                    request_id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    process_generation TEXT NOT NULL,
                    rpc_request_id INTEGER,
                    thread_id TEXT,
                    turn_id TEXT,
                    item_id TEXT,
                    request_type TEXT NOT NULL,
                    request_method TEXT NOT NULL,
                    requested_at TEXT NOT NULL,
                    expires_at TEXT,
                    status TEXT NOT NULL,
                    request_payload_json TEXT,
                    resolved_payload_json TEXT,
                    resolved_at TEXT,
                    resolution_source TEXT,
                    error_code TEXT,
                    error_message TEXT
                )
                """
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_pending_session_status ON codex_pending_requests(session_id, status, requested_at)"
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_pending_generation ON codex_pending_requests(process_generation, status)"
            )
            # Startup reconciliation: unresolved requests from prior process runs are orphaned.
            now = datetime.now(timezone.utc).isoformat()
            cursor.execute(
                """
                UPDATE codex_pending_requests
                SET status = 'orphaned',
                    resolved_at = ?,
                    resolution_source = 'policy',
                    error_code = 'server_restarted',
                    error_message = 'server restarted before request resolution'
                WHERE status IN ('pending', 'expired') AND process_generation != ?
                """,
                (now, self.process_generation),
            )
            conn.commit()

    async def register_request(
        self,
        *,
        session_id: str,
        rpc_request_id: int,
        request_method: str,
        request_payload: dict[str, Any],
        thread_id: Optional[str],
        turn_id: Optional[str],
        item_id: Optional[str],
        request_type: str,
        timeout_seconds: int,
        policy_payload: dict[str, Any],
    ) -> dict[str, Any]:
        """Register pending request and arm timeout policy."""
        request_id = uuid.uuid4().hex[:16]
        now = datetime.now(timezone.utc)
        requested_at = now.isoformat()
        expires_at = (now + timedelta(seconds=max(1, timeout_seconds))).isoformat()

        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                INSERT INTO codex_pending_requests (
                    request_id, session_id, process_generation, rpc_request_id,
                    thread_id, turn_id, item_id, request_type, request_method,
                    requested_at, expires_at, status, request_payload_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)
                """,
                (
                    request_id,
                    session_id,
                    self.process_generation,
                    rpc_request_id,
                    thread_id,
                    turn_id,
                    item_id,
                    request_type,
                    request_method,
                    requested_at,
                    expires_at,
                    json.dumps(request_payload, separators=(",", ":"), default=str),
                ),
            )
            conn.commit()

        loop = asyncio.get_running_loop()
        self._pending_futures[request_id] = loop.create_future()
        self._expiry_tasks[request_id] = asyncio.create_task(
            self._expire_after_timeout(
                request_id=request_id,
                timeout_seconds=max(1, timeout_seconds),
                policy_payload=policy_payload,
            )
        )

        return {
            "request_id": request_id,
            "session_id": session_id,
            "request_type": request_type,
            "request_method": request_method,
            "requested_at": requested_at,
            "expires_at": expires_at,
            "status": "pending",
        }

    async def _expire_after_timeout(self, request_id: str, timeout_seconds: int, policy_payload: dict[str, Any]):
        try:
            await asyncio.sleep(timeout_seconds)
            transitioned = self._mark_expired(request_id)
            if not transitioned:
                return
            await self.resolve_request(
                request_id=request_id,
                response_payload=policy_payload,
                resolution_source="policy",
                error_code="request_expired",
                error_message="request expired before explicit response",
                allow_expired=True,
            )
        except asyncio.CancelledError:
            return

    async def wait_for_resolution(self, request_id: str) -> Optional[dict[str, Any]]:
        """Wait for request resolution and return response payload."""
        future = self._pending_futures.get(request_id)
        if future:
            return await future

        row = self.get_request(request_id)
        if row and row.get("status") == "resolved":
            return row.get("resolved_payload")
        return None

    async def resolve_request(
        self,
        *,
        request_id: str,
        response_payload: dict[str, Any],
        resolution_source: str,
        error_code: Optional[str] = None,
        error_message: Optional[str] = None,
        allow_expired: bool = False,
    ) -> dict[str, Any]:
        """Resolve one request idempotently."""
        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                SELECT request_id, session_id, request_type, request_method, status,
                       requested_at, expires_at, resolved_payload_json, resolved_at,
                       resolution_source, error_code, error_message
                FROM codex_pending_requests
                WHERE request_id = ?
                """,
                (request_id,),
            )
            row = cursor.fetchone()
            if not row:
                return {
                    "ok": False,
                    "http_status": 404,
                    "error_code": "request_not_found",
                    "error_message": "request id not found",
                }

            (
                req_id,
                session_id,
                request_type,
                request_method,
                status,
                requested_at,
                expires_at,
                resolved_payload_json,
                resolved_at,
                existing_resolution_source,
                existing_error_code,
                existing_error_message,
            ) = row

            if status in ("pending", "expired") and (status == "pending" or allow_expired):
                resolved_at_now = datetime.now(timezone.utc).isoformat()
                payload_json = json.dumps(response_payload, separators=(",", ":"), default=str)
                cursor.execute(
                    """
                    UPDATE codex_pending_requests
                    SET status = 'resolved',
                        resolved_payload_json = ?,
                        resolved_at = ?,
                        resolution_source = ?,
                        error_code = ?,
                        error_message = ?
                    WHERE request_id = ?
                    """,
                    (
                        payload_json,
                        resolved_at_now,
                        resolution_source,
                        error_code,
                        error_message,
                        request_id,
                    ),
                )
                conn.commit()
                result = {
                    "ok": True,
                    "idempotent": False,
                    "request": {
                        "request_id": req_id,
                        "session_id": session_id,
                        "request_type": request_type,
                        "request_method": request_method,
                        "status": "resolved",
                        "requested_at": requested_at,
                        "expires_at": expires_at,
                        "resolved_at": resolved_at_now,
                        "resolution_source": resolution_source,
                        "resolved_payload": response_payload,
                        "error_code": error_code,
                        "error_message": error_message,
                    },
                }
            elif status == "resolved":
                resolved_payload = json.loads(resolved_payload_json) if resolved_payload_json else None
                result = {
                    "ok": True,
                    "idempotent": True,
                    "request": {
                        "request_id": req_id,
                        "session_id": session_id,
                        "request_type": request_type,
                        "request_method": request_method,
                        "status": status,
                        "requested_at": requested_at,
                        "expires_at": expires_at,
                        "resolved_at": resolved_at,
                        "resolution_source": existing_resolution_source,
                        "resolved_payload": resolved_payload,
                        "error_code": existing_error_code,
                        "error_message": existing_error_message,
                    },
                }
            else:
                return {
                    "ok": False,
                    "http_status": 404,
                    "error_code": "request_unavailable",
                    "error_message": f"request is {status}",
                }

        # Notify waiter outside DB lock.
        waiter = self._pending_futures.pop(request_id, None)
        if waiter and not waiter.done():
            waiter.set_result(result["request"]["resolved_payload"])

        expiry_task = self._expiry_tasks.pop(request_id, None)
        if expiry_task and not expiry_task.done():
            expiry_task.cancel()

        return result

    def get_request(self, request_id: str) -> Optional[dict[str, Any]]:
        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                SELECT request_id, session_id, request_type, request_method, status,
                       requested_at, expires_at, resolved_payload_json, resolved_at,
                       resolution_source, error_code, error_message
                FROM codex_pending_requests
                WHERE request_id = ?
                """,
                (request_id,),
            )
            row = cursor.fetchone()

        if not row:
            return None

        (
            req_id,
            session_id,
            request_type,
            request_method,
            status,
            requested_at,
            expires_at,
            resolved_payload_json,
            resolved_at,
            resolution_source,
            error_code,
            error_message,
        ) = row

        return {
            "request_id": req_id,
            "session_id": session_id,
            "request_type": request_type,
            "request_method": request_method,
            "status": status,
            "requested_at": requested_at,
            "expires_at": expires_at,
            "resolved_payload": json.loads(resolved_payload_json) if resolved_payload_json else None,
            "resolved_at": resolved_at,
            "resolution_source": resolution_source,
            "error_code": error_code,
            "error_message": error_message,
        }

    def list_requests(self, session_id: str, include_orphaned: bool = False) -> list[dict[str, Any]]:
        """List structured requests for a session (pending by default)."""
        statuses = ["pending"]
        if include_orphaned:
            statuses.append("orphaned")

        placeholders = ", ".join("?" for _ in statuses)
        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                f"""
                SELECT request_id, session_id, request_type, request_method, status,
                       requested_at, expires_at, resolved_payload_json, resolved_at,
                       resolution_source, error_code, error_message
                FROM codex_pending_requests
                WHERE session_id = ? AND status IN ({placeholders})
                ORDER BY requested_at ASC
                """,
                (session_id, *statuses),
            )
            rows = cursor.fetchall()

        requests = []
        for row in rows:
            (
                req_id,
                s_id,
                request_type,
                request_method,
                status,
                requested_at,
                expires_at,
                resolved_payload_json,
                resolved_at,
                resolution_source,
                error_code,
                error_message,
            ) = row
            requests.append(
                {
                    "request_id": req_id,
                    "session_id": s_id,
                    "request_type": request_type,
                    "request_method": request_method,
                    "status": status,
                    "requested_at": requested_at,
                    "expires_at": expires_at,
                    "resolved_payload": json.loads(resolved_payload_json) if resolved_payload_json else None,
                    "resolved_at": resolved_at,
                    "resolution_source": resolution_source,
                    "error_code": error_code,
                    "error_message": error_message,
                }
            )
        return requests

    def has_pending_requests(self, session_id: str) -> bool:
        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                "SELECT 1 FROM codex_pending_requests WHERE session_id = ? AND status = 'pending' LIMIT 1",
                (session_id,),
            )
            return cursor.fetchone() is not None

    def oldest_pending_summary(self, session_id: str) -> Optional[dict[str, Any]]:
        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                SELECT request_id, request_type, requested_at
                FROM codex_pending_requests
                WHERE session_id = ? AND status = 'pending'
                ORDER BY requested_at ASC
                LIMIT 1
                """,
                (session_id,),
            )
            row = cursor.fetchone()

        if not row:
            return None

        return {
            "request_id": row[0],
            "request_type": row[1],
            "requested_at": row[2],
        }

    def _mark_expired(self, request_id: str) -> bool:
        """Transition request to expired only if it is still pending."""
        now = datetime.now(timezone.utc).isoformat()
        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                UPDATE codex_pending_requests
                SET status = 'expired',
                    resolved_at = ?,
                    resolution_source = 'policy',
                    error_code = 'request_expired',
                    error_message = 'request expired before explicit response'
                WHERE request_id = ? AND status = 'pending'
                """,
                (now, request_id),
            )
            conn.commit()
            return cursor.rowcount > 0

    def orphan_pending_for_session(self, session_id: str, error_code: str = "session_closed"):
        """Mark unresolved requests as orphaned and unblock waiters."""
        now = datetime.now(timezone.utc).isoformat()
        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                SELECT request_id
                FROM codex_pending_requests
                WHERE session_id = ? AND status IN ('pending', 'expired')
                """,
                (session_id,),
            )
            pending_ids = [row[0] for row in cursor.fetchall()]

            cursor.execute(
                """
                UPDATE codex_pending_requests
                SET status = 'orphaned',
                    resolved_at = ?,
                    resolution_source = 'policy',
                    error_code = ?,
                    error_message = 'session terminated before request resolution'
                WHERE session_id = ? AND status IN ('pending', 'expired')
                """,
                (now, error_code, session_id),
            )
            conn.commit()

        for request_id in pending_ids:
            future = self._pending_futures.pop(request_id, None)
            if future and not future.done():
                future.set_result(None)
            task = self._expiry_tasks.pop(request_id, None)
            if task and not task.done():
                task.cancel()
