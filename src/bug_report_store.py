"""SQLite-backed spool for app-submitted bug reports."""

from __future__ import annotations

import json
import secrets
import sqlite3
import threading
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional


class BugReportStore:
    """Persist app bug reports in a bounded SQLite spool."""

    def __init__(self, db_path: str, max_reports: int = 30):
        self.db_path = Path(db_path).expanduser()
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self.max_reports = max(1, int(max_reports))
        self._conn: Optional[sqlite3.Connection] = None
        self._lock = threading.Lock()
        self._init_db()

    def _get_conn(self) -> sqlite3.Connection:
        if self._conn is None:
            self._conn = sqlite3.connect(str(self.db_path), check_same_thread=False)
            self._conn.execute("PRAGMA journal_mode=WAL")
            self._conn.execute("PRAGMA busy_timeout=5000")
            self._conn.execute("PRAGMA foreign_keys=ON")
            self._conn.row_factory = sqlite3.Row
        return self._conn

    def _init_db(self) -> None:
        with self._lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            cursor.execute(
                """
                CREATE TABLE IF NOT EXISTS bug_reports (
                    id TEXT PRIMARY KEY,
                    created_at TEXT NOT NULL,
                    reported_by TEXT,
                    report_text TEXT NOT NULL,
                    selected_session_id TEXT,
                    route TEXT,
                    app_version TEXT,
                    artifact_hash TEXT,
                    include_debug_state INTEGER NOT NULL,
                    client_state_json TEXT,
                    server_state_json TEXT,
                    status TEXT NOT NULL DEFAULT 'new',
                    maintainer_delivery_result TEXT
                )
                """
            )
            cursor.execute(
                """
                CREATE TABLE IF NOT EXISTS bug_report_attachments (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    bug_report_id TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    mime_type TEXT NOT NULL,
                    payload BLOB NOT NULL,
                    FOREIGN KEY (bug_report_id) REFERENCES bug_reports(id) ON DELETE CASCADE
                )
                """
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_bug_reports_created_at ON bug_reports(created_at, id)"
            )
            cursor.execute(
                "CREATE INDEX IF NOT EXISTS idx_bug_reports_selected_session ON bug_reports(selected_session_id, created_at)"
            )
            conn.commit()

    @staticmethod
    def _to_json(value: Optional[dict[str, Any]]) -> Optional[str]:
        if value is None:
            return None
        return json.dumps(value, separators=(",", ":"), default=str)

    @staticmethod
    def _bug_id(now: Optional[datetime] = None) -> str:
        current = now.astimezone(timezone.utc) if now else datetime.now(timezone.utc)
        return f"BR-{current.strftime('%Y%m%d-%H%M%S')}-{secrets.token_hex(3)}"

    def create_report(
        self,
        *,
        report_text: str,
        reported_by: Optional[str] = None,
        selected_session_id: Optional[str] = None,
        route: Optional[str] = None,
        app_version: Optional[str] = None,
        artifact_hash: Optional[str] = None,
        include_debug_state: bool = True,
        client_state: Optional[dict[str, Any]] = None,
        server_state: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        """Insert one report, prune the spool, and return the stored row summary."""
        created_at = datetime.now(timezone.utc).isoformat()
        bug_id = self._bug_id()

        with self._lock:
            conn = self._get_conn()
            cursor = conn.cursor()
            try:
                cursor.execute("BEGIN IMMEDIATE")
                cursor.execute(
                    """
                    INSERT INTO bug_reports (
                        id,
                        created_at,
                        reported_by,
                        report_text,
                        selected_session_id,
                        route,
                        app_version,
                        artifact_hash,
                        include_debug_state,
                        client_state_json,
                        server_state_json,
                        status,
                        maintainer_delivery_result
                    )
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'new', NULL)
                    """,
                    (
                        bug_id,
                        created_at,
                        reported_by,
                        report_text,
                        selected_session_id,
                        route,
                        app_version,
                        artifact_hash,
                        1 if include_debug_state else 0,
                        self._to_json(client_state if include_debug_state else None),
                        self._to_json(server_state if include_debug_state else None),
                    ),
                )
                self._prune_locked(cursor)
                conn.commit()
            except Exception:
                conn.rollback()
                raise

        return {
            "id": bug_id,
            "created_at": created_at,
            "reported_by": reported_by,
            "selected_session_id": selected_session_id,
            "route": route,
        }

    def update_delivery_result(self, bug_id: str, result: str) -> None:
        """Persist the maintainer delivery result for one report."""
        with self._lock:
            conn = self._get_conn()
            conn.execute(
                "UPDATE bug_reports SET maintainer_delivery_result = ?, status = 'submitted' WHERE id = ?",
                (result, bug_id),
            )
            conn.commit()

    def get_report(self, bug_id: str) -> Optional[dict[str, Any]]:
        """Return one stored bug report row as a dictionary."""
        with self._lock:
            cursor = self._get_conn().execute("SELECT * FROM bug_reports WHERE id = ?", (bug_id,))
            row = cursor.fetchone()
        if row is None:
            return None
        return dict(row)

    def count_reports(self) -> int:
        """Return the current report count."""
        with self._lock:
            cursor = self._get_conn().execute("SELECT COUNT(*) FROM bug_reports")
            value = cursor.fetchone()
        return int(value[0]) if value else 0

    def list_report_ids(self) -> list[str]:
        """Return report IDs ordered oldest-first."""
        with self._lock:
            cursor = self._get_conn().execute("SELECT id FROM bug_reports ORDER BY created_at ASC, id ASC")
            rows = cursor.fetchall()
        return [str(row[0]) for row in rows]

    def _prune_locked(self, cursor: sqlite3.Cursor) -> None:
        cursor.execute("SELECT COUNT(*) FROM bug_reports")
        total = int(cursor.fetchone()[0])
        excess = total - self.max_reports
        if excess <= 0:
            return

        cursor.execute(
            """
            SELECT id
            FROM bug_reports
            ORDER BY created_at ASC, id ASC
            LIMIT ?
            """,
            (excess,),
        )
        doomed_ids = [str(row[0]) for row in cursor.fetchall()]
        if not doomed_ids:
            return

        placeholders = ",".join("?" for _ in doomed_ids)
        cursor.execute(
            f"DELETE FROM bug_report_attachments WHERE bug_report_id IN ({placeholders})",
            doomed_ids,
        )
        cursor.execute(
            f"DELETE FROM bug_reports WHERE id IN ({placeholders})",
            doomed_ids,
        )
