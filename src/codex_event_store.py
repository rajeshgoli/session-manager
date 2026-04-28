"""Durable Codex app lifecycle event storage with cursor replay semantics."""

from __future__ import annotations

import json
import logging
import sqlite3
import threading
from collections import defaultdict, deque
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Optional

logger = logging.getLogger(__name__)


class CodexEventStore:
    """Persists codex-app lifecycle events and serves cursor-based history pages."""

    def __init__(
        self,
        db_path: str,
        ring_size: int = 1000,
        retention_max_events_per_session: int = 5000,
        retention_max_age_days: int = 14,
        prune_every_writes: int = 200,
        payload_preview_chars: int = 1500,
        startup_maintenance: bool = True,
    ):
        self.db_path = Path(db_path).expanduser()
        self.db_path.parent.mkdir(parents=True, exist_ok=True)

        self.ring_size = max(100, ring_size)
        self.retention_max_events_per_session = max(1, retention_max_events_per_session)
        self.retention_max_age_days = max(1, retention_max_age_days)
        self.prune_every_writes = max(1, prune_every_writes)
        self.payload_preview_chars = max(200, payload_preview_chars)
        self.prune_batch_size = 500

        self._lock = threading.Lock()
        self._conn: Optional[sqlite3.Connection] = None
        self._ring_events: dict[str, deque[dict[str, Any]]] = defaultdict(
            lambda: deque(maxlen=self.ring_size)
        )
        self._persistence_degraded: set[str] = set()
        self._persisted_writes = 0
        self._prune_index_ready = False
        self._maintenance_running = False
        self._maintenance_lock = threading.Lock()
        self._pending_overflow_prune_sessions: set[str] = set()

        self._init_db()
        if startup_maintenance:
            self._start_startup_maintenance()

    def _get_conn(self) -> sqlite3.Connection:
        if self._conn is None:
            self._conn = sqlite3.connect(str(self.db_path), check_same_thread=False)
            self._conn.execute("PRAGMA journal_mode=WAL")
            self._conn.execute("PRAGMA busy_timeout=5000")
        return self._conn

    def _init_db(self):
        with self._lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                CREATE TABLE IF NOT EXISTS codex_session_events (
                    session_id TEXT NOT NULL,
                    seq INTEGER NOT NULL,
                    timestamp TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    turn_id TEXT,
                    payload_preview_json TEXT,
                    PRIMARY KEY (session_id, seq)
                )
                """
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_session_events_ts ON codex_session_events(session_id, timestamp)"
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_session_events_event_type ON codex_session_events(event_type)"
            )
            conn.commit()

    def _start_startup_maintenance(self) -> None:
        with self._maintenance_lock:
            if self._maintenance_running:
                return
            self._maintenance_running = True
        thread = threading.Thread(
            target=self._run_startup_maintenance,
            name="codex-event-store-maintenance",
            daemon=True,
        )
        thread.start()

    def _run_startup_maintenance(self) -> None:
        conn: Optional[sqlite3.Connection] = None
        try:
            conn = sqlite3.connect(str(self.db_path), check_same_thread=False)
            conn.execute("PRAGMA journal_mode=WAL")
            conn.execute("PRAGMA busy_timeout=5000")
            cursor = conn.cursor()
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_codex_session_events_timestamp ON codex_session_events(timestamp)"
            )
            conn.commit()
            with self._lock:
                self._prune_index_ready = True

            pending_overflow_sessions = self._pop_pending_overflow_prune_sessions()
            pruned = self._prune_incremental(conn, pending_overflow_sessions)
            logger.info("Codex event store startup maintenance completed")
            if pruned:
                logger.info("Codex event store pruned %s stale event(s)", pruned)
        except Exception as exc:
            if conn is not None:
                try:
                    conn.rollback()
                except Exception:
                    pass
            logger.warning("Codex event store startup maintenance failed: %s", exc)
        finally:
            if conn is not None:
                try:
                    conn.close()
                except Exception:
                    pass
            with self._maintenance_lock:
                self._maintenance_running = False

    def append_event(
        self,
        session_id: str,
        event_type: str,
        turn_id: Optional[str] = None,
        payload: Optional[dict[str, Any]] = None,
        timestamp: Optional[datetime] = None,
    ) -> dict[str, Any]:
        """Append one event. Falls back to in-memory-only event on persistence failure."""
        event_ts = timestamp.astimezone(timezone.utc) if timestamp else datetime.now(timezone.utc)
        payload_preview = self._serialize_payload_preview(payload)

        with self._lock:
            conn: Optional[sqlite3.Connection] = None
            cursor: Optional[sqlite3.Cursor] = None
            persisted_events: list[dict[str, Any]] = []
            try:
                conn = self._get_conn()
                cursor = conn.cursor()
                cursor.execute("BEGIN IMMEDIATE")
                latest_seq = self._latest_seq_locked(cursor, session_id)

                if session_id in self._persistence_degraded:
                    latest_seq += 1
                    marker = {
                        "session_id": session_id,
                        "seq": latest_seq,
                        "timestamp": datetime.now(timezone.utc).isoformat(),
                        "event_type": "event_persist_recovered",
                        "turn_id": None,
                        "payload_preview": {"reason": "persistence_recovered"},
                        "persisted": True,
                    }
                    cursor.execute(
                        """
                        INSERT INTO codex_session_events
                        (session_id, seq, timestamp, event_type, turn_id, payload_preview_json)
                        VALUES (?, ?, ?, ?, ?, ?)
                        """,
                        (
                            marker["session_id"],
                            marker["seq"],
                            marker["timestamp"],
                            marker["event_type"],
                            marker["turn_id"],
                            json.dumps(marker["payload_preview"], separators=(",", ":")),
                        ),
                    )
                    persisted_events.append(marker)
                    self._persistence_degraded.discard(session_id)

                latest_seq += 1
                event = {
                    "session_id": session_id,
                    "seq": latest_seq,
                    "timestamp": event_ts.isoformat(),
                    "event_type": event_type,
                    "turn_id": turn_id,
                    "payload_preview": payload_preview,
                    "persisted": True,
                }
                cursor.execute(
                    """
                    INSERT INTO codex_session_events
                    (session_id, seq, timestamp, event_type, turn_id, payload_preview_json)
                    VALUES (?, ?, ?, ?, ?, ?)
                    """,
                    (
                        event["session_id"],
                        event["seq"],
                        event["timestamp"],
                        event["event_type"],
                        event["turn_id"],
                        json.dumps(payload_preview, separators=(",", ":")) if payload_preview else None,
                    ),
                )
                persisted_events.append(event)
                conn.commit()

                for item in persisted_events:
                    self._append_ring_event_locked(item)

                self._persisted_writes += len(persisted_events)
                if latest_seq > self.retention_max_events_per_session:
                    self._mark_session_pending_overflow_prune(session_id)
                if self._persisted_writes % self.prune_every_writes == 0:
                    if not self._prune_index_ready:
                        self._start_startup_maintenance()
                    elif not self._maintenance_running:
                        self._start_startup_maintenance()

                return event

            except Exception as exc:
                try:
                    if conn is not None:
                        conn.rollback()
                except Exception:
                    pass
                self._persistence_degraded.add(session_id)
                logger.warning("Failed to persist codex event for %s: %s", session_id, exc)
                fallback_event = {
                    "session_id": session_id,
                    "seq": None,
                    "timestamp": event_ts.isoformat(),
                    "event_type": event_type,
                    "turn_id": turn_id,
                    "payload_preview": payload_preview,
                    "persisted": False,
                }
                self._append_ring_event_locked(fallback_event)
                return fallback_event

    def get_events(self, session_id: str, since_seq: Optional[int] = None, limit: int = 200) -> dict[str, Any]:
        """Read persisted events for a session using sequence cursor semantics."""
        limit = max(1, min(limit, 500))

        with self._lock:
            conn = self._get_conn()
            cursor = conn.cursor()

            cursor.execute(
                "SELECT MIN(seq), MAX(seq) FROM codex_session_events WHERE session_id = ?",
                (session_id,),
            )
            earliest_seq, latest_seq = cursor.fetchone()

            history_gap = False
            gap_reason: Optional[str] = None
            events: list[dict[str, Any]] = []

            if earliest_seq is None or latest_seq is None:
                next_seq = (since_seq + 1) if since_seq is not None else 1
                if session_id in self._persistence_degraded:
                    history_gap = True
                    gap_reason = "persistence_error"
                return {
                    "events": events,
                    "earliest_seq": None,
                    "latest_seq": None,
                    "next_seq": next_seq,
                    "history_gap": history_gap,
                    "gap_reason": gap_reason,
                }

            if since_seq is None:
                start_seq = max(earliest_seq, latest_seq - limit + 1)
            else:
                if since_seq < (earliest_seq - 1):
                    history_gap = True
                    gap_reason = "retention"
                    start_seq = earliest_seq
                else:
                    start_seq = since_seq + 1

            cursor.execute(
                """
                SELECT seq, timestamp, event_type, turn_id, payload_preview_json
                FROM codex_session_events
                WHERE session_id = ? AND seq >= ?
                ORDER BY seq ASC
                LIMIT ?
                """,
                (session_id, start_seq, limit),
            )
            rows = cursor.fetchall()

            for seq, ts, event_type, turn_id, payload_json in rows:
                payload_preview = None
                if payload_json:
                    try:
                        payload_preview = json.loads(payload_json)
                    except Exception:
                        payload_preview = {"raw": payload_json[: self.payload_preview_chars]}
                events.append(
                    {
                        "session_id": session_id,
                        "seq": seq,
                        "timestamp": ts,
                        "event_type": event_type,
                        "turn_id": turn_id,
                        "payload_preview": payload_preview,
                        "persisted": True,
                    }
                )

            if events:
                next_seq = events[-1]["seq"] + 1
            elif since_seq is not None:
                next_seq = since_seq + 1
            else:
                next_seq = earliest_seq

            if session_id in self._persistence_degraded and not history_gap:
                history_gap = True
                gap_reason = "persistence_error"

            return {
                "events": events,
                "earliest_seq": earliest_seq,
                "latest_seq": latest_seq,
                "next_seq": next_seq,
                "history_gap": history_gap,
                "gap_reason": gap_reason,
            }

    def get_ring_events(self, session_id: str, limit: int = 200) -> list[dict[str, Any]]:
        """Get recent in-memory events, including non-persisted fallback events."""
        limit = max(1, min(limit, self.ring_size))
        with self._lock:
            ring = self._ring_events.get(session_id)
            if not ring:
                return []
            return list(ring)[-limit:]

    def _latest_seq_locked(self, cursor: sqlite3.Cursor, session_id: str) -> int:
        cursor.execute(
            "SELECT COALESCE(MAX(seq), 0) FROM codex_session_events WHERE session_id = ?",
            (session_id,),
        )
        row = cursor.fetchone()
        return int(row[0]) if row else 0

    def _append_ring_event_locked(self, event: dict[str, Any]):
        session_id = event["session_id"]
        self._ring_events[session_id].append(event)

    def _serialize_payload_preview(self, payload: Optional[dict[str, Any]]) -> Optional[dict[str, Any]]:
        if payload is None:
            return None

        serialized = json.dumps(payload, separators=(",", ":"), default=str)
        if len(serialized) <= self.payload_preview_chars:
            return payload

        excerpt = serialized[: self.payload_preview_chars]
        return {
            "truncated": True,
            "preview": excerpt,
            "original_chars": len(serialized),
        }

    def _prune_locked(self, cursor: sqlite3.Cursor):
        cutoff = datetime.now(timezone.utc) - timedelta(days=self.retention_max_age_days)
        cutoff_iso = cutoff.isoformat()
        cursor.execute(
            "DELETE FROM codex_session_events WHERE timestamp < ?",
            (cutoff_iso,),
        )

        cursor.execute(
            "SELECT session_id, MAX(seq) FROM codex_session_events GROUP BY session_id"
        )
        for session_id, max_seq in cursor.fetchall():
            if max_seq is None:
                continue
            min_keep_seq = int(max_seq) - self.retention_max_events_per_session + 1
            if min_keep_seq > 1:
                cursor.execute(
                    "DELETE FROM codex_session_events WHERE session_id = ? AND seq < ?",
                    (session_id, min_keep_seq),
                )

    def _mark_session_pending_overflow_prune(self, session_id: str) -> None:
        with self._maintenance_lock:
            self._pending_overflow_prune_sessions.add(session_id)

    def _pop_pending_overflow_prune_sessions(self) -> list[str]:
        with self._maintenance_lock:
            sessions = sorted(self._pending_overflow_prune_sessions)
            self._pending_overflow_prune_sessions.clear()
            return sessions

    def _prune_incremental(
        self,
        conn: sqlite3.Connection,
        pending_overflow_sessions: Optional[list[str]] = None,
    ) -> int:
        """Delete at most one small retention batch so foreground writes are not starved."""
        cursor = conn.cursor()
        cutoff = datetime.now(timezone.utc) - timedelta(days=self.retention_max_age_days)
        cutoff_iso = cutoff.isoformat()

        cursor.execute(
            """
            SELECT rowid
            FROM codex_session_events
            WHERE timestamp < ?
            ORDER BY timestamp
            LIMIT ?
            """,
            (cutoff_iso, self.prune_batch_size),
        )
        rowids = [row[0] for row in cursor.fetchall()]
        if rowids:
            return self._delete_rowids_batch(conn, rowids)

        for session_id in pending_overflow_sessions or []:
            cursor.execute(
                "SELECT MAX(seq) FROM codex_session_events WHERE session_id = ?",
                (session_id,),
            )
            row = cursor.fetchone()
            max_seq = int(row[0] or 0) if row else 0
            min_keep_seq = max_seq - self.retention_max_events_per_session + 1
            if min_keep_seq <= 1:
                continue
            rowids = self._overflow_rowids_for_session(cursor, session_id, min_keep_seq)
            if rowids:
                return self._delete_rowids_batch(conn, rowids)
        return 0

    def _overflow_rowids_for_session(
        self,
        cursor: sqlite3.Cursor,
        session_id: str,
        min_keep_seq: int,
    ) -> list[int]:
        cursor.execute(
            """
            SELECT rowid
            FROM codex_session_events
            WHERE session_id = ? AND seq < ?
            ORDER BY seq
            LIMIT ?
            """,
            (session_id, int(min_keep_seq), self.prune_batch_size),
        )
        return [row[0] for row in cursor.fetchall()]

    def _delete_rowids_batch(self, conn: sqlite3.Connection, rowids: list[int]) -> int:
        cursor = conn.cursor()
        try:
            cursor.execute("BEGIN IMMEDIATE")
            cursor.executemany(
                "DELETE FROM codex_session_events WHERE rowid = ?",
                [(rowid,) for rowid in rowids],
            )
            conn.commit()
            return len(rowids)
        except Exception:
            conn.rollback()
            raise
