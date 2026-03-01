"""Codex observability storage for tool and turn lifecycle events."""

from __future__ import annotations

import asyncio
import json
import logging
import sqlite3
import threading
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Optional

logger = logging.getLogger(__name__)


class CodexObservabilityLogger:
    """Persists codex observability events with bounded retention."""

    def __init__(
        self,
        db_path: str,
        retention_max_age_days: int = 14,
        retention_codex_fork_max_age_days: Optional[int] = None,
        retention_tool_events_per_session: int = 20000,
        retention_turn_events_per_session: int = 5000,
        payload_max_chars: int = 4000,
        prune_interval_seconds: int = 3600,
    ):
        self.db_path = Path(db_path).expanduser()
        self.db_path.parent.mkdir(parents=True, exist_ok=True)

        self.retention_max_age_days = max(1, int(retention_max_age_days))
        if retention_codex_fork_max_age_days is None:
            retention_codex_fork_max_age_days = retention_max_age_days
        self.retention_codex_fork_max_age_days = max(1, int(retention_codex_fork_max_age_days))
        self.retention_tool_events_per_session = max(1, int(retention_tool_events_per_session))
        self.retention_turn_events_per_session = max(1, int(retention_turn_events_per_session))
        self.payload_max_chars = max(200, int(payload_max_chars))
        self.prune_interval_seconds = max(60, int(prune_interval_seconds))

        self._conn: Optional[sqlite3.Connection] = None
        self._db_lock = threading.Lock()
        self._prune_task: Optional[asyncio.Task] = None

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
                CREATE TABLE IF NOT EXISTS codex_tool_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    thread_id TEXT,
                    turn_id TEXT,
                    item_id TEXT,
                    request_id TEXT,
                    event_type TEXT NOT NULL,
                    item_type TEXT,
                    phase TEXT,
                    command TEXT,
                    cwd TEXT,
                    exit_code INTEGER,
                    file_path TEXT,
                    diff_summary TEXT,
                    approval_decision TEXT,
                    latency_ms INTEGER,
                    final_status TEXT,
                    error_code TEXT,
                    error_message TEXT,
                    raw_payload_json TEXT,
                    provider TEXT NOT NULL DEFAULT 'codex-app',
                    schema_version INTEGER,
                    created_at TEXT NOT NULL
                )
                """
            )
            cursor.execute(
                """
                CREATE TABLE IF NOT EXISTS codex_turn_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    thread_id TEXT,
                    turn_id TEXT,
                    event_type TEXT NOT NULL,
                    status TEXT,
                    delta_chars INTEGER,
                    output_preview TEXT,
                    error_code TEXT,
                    error_message TEXT,
                    raw_payload_json TEXT,
                    provider TEXT NOT NULL DEFAULT 'codex-app',
                    schema_version INTEGER,
                    created_at TEXT NOT NULL
                )
                """
            )
            self._ensure_column(
                cursor=cursor,
                table="codex_tool_events",
                column="provider",
                definition="TEXT NOT NULL DEFAULT 'codex-app'",
            )
            self._ensure_column(
                cursor=cursor,
                table="codex_tool_events",
                column="schema_version",
                definition="INTEGER",
            )
            self._ensure_column(
                cursor=cursor,
                table="codex_turn_events",
                column="provider",
                definition="TEXT NOT NULL DEFAULT 'codex-app'",
            )
            self._ensure_column(
                cursor=cursor,
                table="codex_turn_events",
                column="schema_version",
                definition="INTEGER",
            )

            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_tool_events_session_created ON codex_tool_events(session_id, created_at, id)"
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_tool_events_event ON codex_tool_events(session_id, event_type, created_at)"
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_tool_events_turn ON codex_tool_events(turn_id, created_at)"
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_tool_events_session_call ON codex_tool_events(session_id, item_id, created_at, id)"
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_tool_events_session_turn_call ON codex_tool_events(session_id, turn_id, item_id, created_at, id)"
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_tool_events_provider_schema ON codex_tool_events(provider, schema_version, created_at, id)"
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_turn_events_session_created ON codex_turn_events(session_id, created_at, id)"
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_turn_events_turn ON codex_turn_events(turn_id, created_at)"
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_turn_events_provider_schema ON codex_turn_events(provider, schema_version, created_at, id)"
            )

            conn.commit()

        self.prune()

    def _ensure_column(self, *, cursor: sqlite3.Cursor, table: str, column: str, definition: str) -> None:
        cursor.execute(f"PRAGMA table_info({table})")
        columns = {row[1] for row in cursor.fetchall()}
        if column in columns:
            return
        cursor.execute(f"ALTER TABLE {table} ADD COLUMN {column} {definition}")

    def _bounded_payload_json(self, payload: Optional[dict[str, Any]]) -> Optional[str]:
        if payload is None:
            return None
        try:
            raw = json.dumps(payload, separators=(",", ":"), default=str)
        except Exception:
            raw = json.dumps({"raw": str(payload)})
        if len(raw) <= self.payload_max_chars:
            return raw
        excerpt = raw[: self.payload_max_chars]
        return json.dumps(
            {
                "truncated": True,
                "preview": excerpt,
                "original_chars": len(raw),
            },
            separators=(",", ":"),
        )

    def log_tool_event(
        self,
        *,
        session_id: str,
        event_type: str,
        thread_id: Optional[str] = None,
        turn_id: Optional[str] = None,
        item_id: Optional[str] = None,
        request_id: Optional[str] = None,
        item_type: Optional[str] = None,
        phase: Optional[str] = None,
        command: Optional[str] = None,
        cwd: Optional[str] = None,
        exit_code: Optional[int] = None,
        file_path: Optional[str] = None,
        diff_summary: Optional[str] = None,
        approval_decision: Optional[str] = None,
        latency_ms: Optional[int] = None,
        final_status: Optional[str] = None,
        error_code: Optional[str] = None,
        error_message: Optional[str] = None,
        provider: str = "codex-app",
        schema_version: Optional[int] = None,
        raw_payload: Optional[dict[str, Any]] = None,
        created_at: Optional[datetime] = None,
    ) -> None:
        ts = (created_at or datetime.now(timezone.utc)).astimezone(timezone.utc).isoformat()
        raw_payload_json = self._bounded_payload_json(raw_payload)
        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                INSERT INTO codex_tool_events (
                    session_id, thread_id, turn_id, item_id, request_id,
                    event_type, item_type, phase, command, cwd, exit_code, file_path,
                    diff_summary, approval_decision, latency_ms, final_status,
                    error_code, error_message, raw_payload_json, provider, schema_version, created_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    session_id,
                    thread_id,
                    turn_id,
                    item_id,
                    request_id,
                    event_type,
                    item_type,
                    phase,
                    command,
                    cwd,
                    exit_code,
                    file_path,
                    diff_summary,
                    approval_decision,
                    latency_ms,
                    final_status,
                    error_code,
                    error_message,
                    raw_payload_json,
                    provider,
                    schema_version,
                    ts,
                ),
            )
            conn.commit()

    def log_turn_event(
        self,
        *,
        session_id: str,
        turn_id: Optional[str],
        event_type: str,
        thread_id: Optional[str] = None,
        status: Optional[str] = None,
        delta_chars: Optional[int] = None,
        output_preview: Optional[str] = None,
        error_code: Optional[str] = None,
        error_message: Optional[str] = None,
        provider: str = "codex-app",
        schema_version: Optional[int] = None,
        raw_payload: Optional[dict[str, Any]] = None,
        created_at: Optional[datetime] = None,
    ) -> None:
        ts = (created_at or datetime.now(timezone.utc)).astimezone(timezone.utc).isoformat()
        raw_payload_json = self._bounded_payload_json(raw_payload)
        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                INSERT INTO codex_turn_events (
                    session_id, thread_id, turn_id, event_type, status, delta_chars,
                    output_preview, error_code, error_message, raw_payload_json, provider, schema_version, created_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    session_id,
                    thread_id,
                    turn_id,
                    event_type,
                    status,
                    delta_chars,
                    output_preview,
                    error_code,
                    error_message,
                    raw_payload_json,
                    provider,
                    schema_version,
                    ts,
                ),
            )
            conn.commit()

    def prune(self) -> dict[str, int]:
        """Apply age and per-session row-cap retention to tool and turn tables."""
        started = time.monotonic()
        deleted_tool_age = 0
        deleted_turn_age = 0
        deleted_tool_cap = 0
        deleted_turn_cap = 0

        cutoff_default_iso = (
            datetime.now(timezone.utc) - timedelta(days=self.retention_max_age_days)
        ).isoformat()
        cutoff_fork_iso = (
            datetime.now(timezone.utc) - timedelta(days=self.retention_codex_fork_max_age_days)
        ).isoformat()

        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()

            cursor.execute(
                "DELETE FROM codex_tool_events WHERE COALESCE(provider, 'codex-app') = 'codex-fork' AND created_at < ?",
                (cutoff_fork_iso,),
            )
            deleted_tool_age = cursor.rowcount
            cursor.execute(
                "DELETE FROM codex_tool_events WHERE COALESCE(provider, 'codex-app') != 'codex-fork' AND created_at < ?",
                (cutoff_default_iso,),
            )
            deleted_tool_age += cursor.rowcount

            cursor.execute(
                "DELETE FROM codex_turn_events WHERE COALESCE(provider, 'codex-app') = 'codex-fork' AND created_at < ?",
                (cutoff_fork_iso,),
            )
            deleted_turn_age = cursor.rowcount
            cursor.execute(
                "DELETE FROM codex_turn_events WHERE COALESCE(provider, 'codex-app') != 'codex-fork' AND created_at < ?",
                (cutoff_default_iso,),
            )
            deleted_turn_age += cursor.rowcount

            deleted_tool_cap = self._prune_table_by_session_cap(
                cursor=cursor,
                table="codex_tool_events",
                cap=self.retention_tool_events_per_session,
            )
            deleted_turn_cap = self._prune_table_by_session_cap(
                cursor=cursor,
                table="codex_turn_events",
                cap=self.retention_turn_events_per_session,
            )
            conn.commit()

        elapsed_ms = int((time.monotonic() - started) * 1000)
        logger.info(
            "codex observability prune completed: tool_age=%s turn_age=%s tool_cap=%s turn_cap=%s elapsed_ms=%s",
            deleted_tool_age,
            deleted_turn_age,
            deleted_tool_cap,
            deleted_turn_cap,
            elapsed_ms,
        )
        return {
            "tool_age": deleted_tool_age,
            "turn_age": deleted_turn_age,
            "tool_cap": deleted_tool_cap,
            "turn_cap": deleted_turn_cap,
            "elapsed_ms": elapsed_ms,
        }

    def _prune_table_by_session_cap(self, *, cursor: sqlite3.Cursor, table: str, cap: int) -> int:
        deleted = 0
        cursor.execute(
            f"""
            SELECT COALESCE(provider, 'codex-app') AS provider_name, session_id, COUNT(*)
            FROM {table}
            GROUP BY provider_name, session_id
            HAVING COUNT(*) > ?
            """,
            (cap,),
        )
        overflow_rows = cursor.fetchall()
        for provider_name, session_id, count in overflow_rows:
            overflow = int(count) - cap
            cursor.execute(
                f"""
                DELETE FROM {table}
                WHERE id IN (
                    SELECT id
                    FROM {table}
                    WHERE session_id = ? AND COALESCE(provider, 'codex-app') = ?
                    ORDER BY created_at ASC, id ASC
                    LIMIT ?
                )
                """,
                (session_id, provider_name, overflow),
            )
            deleted += cursor.rowcount
        return deleted

    async def start_periodic_prune(self):
        """Start periodic prune loop if not already active."""
        if self._prune_task and not self._prune_task.done():
            return
        self._prune_task = asyncio.create_task(self._periodic_prune_loop())

    async def stop_periodic_prune(self):
        """Stop periodic prune loop."""
        if not self._prune_task:
            return
        self._prune_task.cancel()
        try:
            await self._prune_task
        except asyncio.CancelledError:
            pass
        self._prune_task = None

    async def _periodic_prune_loop(self):
        while True:
            try:
                await asyncio.sleep(self.prune_interval_seconds)
                await asyncio.to_thread(self.prune)
            except asyncio.CancelledError:
                return
            except Exception as exc:
                logger.warning("codex observability periodic prune failed: %s", exc)

    def list_recent_tool_events(self, session_id: str, limit: int = 50) -> list[dict[str, Any]]:
        limit = max(1, min(int(limit), 500))
        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                SELECT session_id, thread_id, turn_id, item_id, request_id, event_type, item_type, phase,
                       command, cwd, exit_code, file_path, diff_summary, approval_decision,
                       latency_ms, final_status, error_code, error_message, raw_payload_json,
                       provider, schema_version, created_at
                FROM codex_tool_events
                WHERE session_id = ?
                ORDER BY id DESC
                LIMIT ?
                """,
                (session_id, limit),
            )
            rows = cursor.fetchall()
        rows.reverse()
        events = []
        for row in rows:
            events.append(
                {
                    "session_id": row[0],
                    "thread_id": row[1],
                    "turn_id": row[2],
                    "item_id": row[3],
                    "request_id": row[4],
                    "event_type": row[5],
                    "item_type": row[6],
                    "phase": row[7],
                    "command": row[8],
                    "cwd": row[9],
                    "exit_code": row[10],
                    "file_path": row[11],
                    "diff_summary": row[12],
                    "approval_decision": row[13],
                    "latency_ms": row[14],
                    "final_status": row[15],
                    "error_code": row[16],
                    "error_message": row[17],
                    "raw_payload_json": row[18],
                    "provider": row[19],
                    "schema_version": row[20],
                    "created_at": row[21],
                }
            )
        return events

    def list_recent_turn_events(self, session_id: str, limit: int = 50) -> list[dict[str, Any]]:
        limit = max(1, min(int(limit), 500))
        with self._db_lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                SELECT session_id, thread_id, turn_id, event_type, status, delta_chars,
                       output_preview, error_code, error_message, raw_payload_json,
                       provider, schema_version, created_at
                FROM codex_turn_events
                WHERE session_id = ?
                ORDER BY id DESC
                LIMIT ?
                """,
                (session_id, limit),
            )
            rows = cursor.fetchall()
        rows.reverse()
        events = []
        for row in rows:
            events.append(
                {
                    "session_id": row[0],
                    "thread_id": row[1],
                    "turn_id": row[2],
                    "event_type": row[3],
                    "status": row[4],
                    "delta_chars": row[5],
                    "output_preview": row[6],
                    "error_code": row[7],
                    "error_message": row[8],
                    "raw_payload_json": row[9],
                    "provider": row[10],
                    "schema_version": row[11],
                    "created_at": row[12],
                }
            )
        return events
