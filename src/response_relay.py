"""Durable turn-bound assistant response relay state."""

from __future__ import annotations

import hashlib
import json
import logging
import sqlite3
import threading
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Optional

logger = logging.getLogger(__name__)


def _utc_now() -> datetime:
    return datetime.now(timezone.utc)


def _coerce_utc(value: Optional[datetime]) -> datetime:
    if value is None:
        return _utc_now()
    if value.tzinfo is None:
        return value.replace(tzinfo=timezone.utc)
    return value.astimezone(timezone.utc)


def _parse_datetime(value: Optional[str]) -> Optional[datetime]:
    if not value:
        return None
    try:
        normalized = value.replace("Z", "+00:00")
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        return None
    return _coerce_utc(parsed)


def _hash_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


@dataclass(frozen=True)
class InboundTurn:
    """A user/operator input delivered to one managed session."""

    inbound_id: str
    session_id: str
    source: str
    provider: Optional[str]
    delivered_at: datetime
    transcript_path: Optional[str] = None
    transcript_offset: Optional[int] = None
    provider_turn_id: Optional[str] = None
    text_hash: Optional[str] = None


@dataclass(frozen=True)
class ClaudeAssistantOutput:
    """Visible Claude assistant text parsed from a transcript."""

    assistant_message_id: str
    text: str
    completed_at: datetime
    line_start_offset: int
    line_number: int


class ResponseRelayLedger:
    """SQLite ledger for inbound turn boundaries and relayed assistant outputs."""

    def __init__(self, db_path: str = "~/.local/share/claude-sessions/response_relay.db"):
        self.db_path = Path(db_path).expanduser()
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._lock = threading.Lock()
        self._conn = sqlite3.connect(str(self.db_path), check_same_thread=False)
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute("PRAGMA busy_timeout=5000")
        self._init_db()

    def close(self) -> None:
        with self._lock:
            self._conn.close()

    def _init_db(self) -> None:
        with self._lock:
            cursor = self._conn.cursor()
            cursor.execute(
                """
                CREATE TABLE IF NOT EXISTS inbound_turns (
                    inbound_id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    source TEXT NOT NULL,
                    provider TEXT,
                    delivered_at TEXT NOT NULL,
                    transcript_path TEXT,
                    transcript_offset INTEGER,
                    provider_turn_id TEXT,
                    text_hash TEXT,
                    superseded_at TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )
                """
            )
            cursor.execute(
                """
                CREATE INDEX IF NOT EXISTS idx_inbound_turns_active
                ON inbound_turns(session_id, delivered_at)
                WHERE superseded_at IS NULL
                """
            )
            cursor.execute(
                """
                CREATE TABLE IF NOT EXISTS assistant_outputs (
                    session_id TEXT NOT NULL,
                    inbound_id TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    provider_turn_id TEXT,
                    assistant_message_id TEXT NOT NULL,
                    completed_at TEXT NOT NULL,
                    text_hash TEXT NOT NULL,
                    text_preview TEXT,
                    relay_claimed_at TEXT,
                    relayed_at TEXT,
                    telegram_thread_id INTEGER,
                    PRIMARY KEY (session_id, inbound_id, provider, assistant_message_id)
                )
                """
            )
            cursor.execute(
                """
                CREATE INDEX IF NOT EXISTS idx_assistant_outputs_relayed
                ON assistant_outputs(session_id, inbound_id, relayed_at)
                """
            )
            self._conn.commit()

    @staticmethod
    def capture_transcript_offset(transcript_path: Optional[str]) -> Optional[int]:
        """Return the current transcript byte size, if a transcript exists."""
        if not transcript_path:
            return None
        try:
            return Path(transcript_path).expanduser().stat().st_size
        except FileNotFoundError:
            return None
        except OSError as exc:
            logger.debug("Could not stat transcript %s for relay boundary: %s", transcript_path, exc)
            return None

    def record_inbound_turn(
        self,
        *,
        session_id: str,
        inbound_id: str,
        source: str,
        provider: Optional[str] = None,
        delivered_at: Optional[datetime] = None,
        transcript_path: Optional[str] = None,
        transcript_offset: Optional[int] = None,
        provider_turn_id: Optional[str] = None,
        text: str = "",
    ) -> InboundTurn:
        """Persist a delivered input turn and supersede older active turns."""
        delivered = _coerce_utc(delivered_at)
        now = _utc_now().isoformat()
        delivered_iso = delivered.isoformat()
        text_hash = _hash_text(text) if text else None
        with self._lock:
            cursor = self._conn.cursor()
            cursor.execute(
                """
                UPDATE inbound_turns
                SET superseded_at = ?, updated_at = ?
                WHERE session_id = ?
                  AND inbound_id != ?
                  AND superseded_at IS NULL
                """,
                (now, now, session_id, inbound_id),
            )
            cursor.execute(
                """
                INSERT INTO inbound_turns
                (inbound_id, session_id, source, provider, delivered_at, transcript_path,
                 transcript_offset, provider_turn_id, text_hash, superseded_at, created_at, updated_at)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)
                ON CONFLICT(inbound_id) DO UPDATE SET
                    session_id = excluded.session_id,
                    source = excluded.source,
                    provider = excluded.provider,
                    delivered_at = excluded.delivered_at,
                    transcript_path = COALESCE(excluded.transcript_path, inbound_turns.transcript_path),
                    transcript_offset = COALESCE(excluded.transcript_offset, inbound_turns.transcript_offset),
                    provider_turn_id = COALESCE(excluded.provider_turn_id, inbound_turns.provider_turn_id),
                    text_hash = COALESCE(excluded.text_hash, inbound_turns.text_hash),
                    superseded_at = NULL,
                    updated_at = excluded.updated_at
                """,
                (
                    inbound_id,
                    session_id,
                    source,
                    provider,
                    delivered_iso,
                    transcript_path,
                    transcript_offset,
                    provider_turn_id,
                    text_hash,
                    now,
                    now,
                ),
            )
            self._conn.commit()
        return InboundTurn(
            inbound_id=inbound_id,
            session_id=session_id,
            source=source,
            provider=provider,
            delivered_at=delivered,
            transcript_path=transcript_path,
            transcript_offset=transcript_offset,
            provider_turn_id=provider_turn_id,
            text_hash=text_hash,
        )

    def update_inbound_boundary(
        self,
        inbound_id: str,
        *,
        transcript_path: Optional[str] = None,
        transcript_offset: Optional[int] = None,
        provider_turn_id: Optional[str] = None,
    ) -> None:
        """Fill missing provider boundary metadata without moving the turn."""
        now = _utc_now().isoformat()
        with self._lock:
            self._conn.execute(
                """
                UPDATE inbound_turns
                SET transcript_path = COALESCE(?, transcript_path),
                    transcript_offset = COALESCE(?, transcript_offset),
                    provider_turn_id = COALESCE(?, provider_turn_id),
                    updated_at = ?
                WHERE inbound_id = ?
                """,
                (transcript_path, transcript_offset, provider_turn_id, now, inbound_id),
            )
            self._conn.commit()

    def get_latest_active_turn(self, session_id: str) -> Optional[InboundTurn]:
        with self._lock:
            cursor = self._conn.cursor()
            cursor.execute(
                """
                SELECT inbound_id, session_id, source, provider, delivered_at,
                       transcript_path, transcript_offset, provider_turn_id, text_hash
                FROM inbound_turns
                WHERE session_id = ?
                  AND superseded_at IS NULL
                ORDER BY delivered_at DESC, updated_at DESC
                LIMIT 1
                """,
                (session_id,),
            )
            row = cursor.fetchone()
        if not row:
            return None
        delivered_at = _parse_datetime(row[4]) or _utc_now()
        return InboundTurn(
            inbound_id=row[0],
            session_id=row[1],
            source=row[2],
            provider=row[3],
            delivered_at=delivered_at,
            transcript_path=row[5],
            transcript_offset=row[6],
            provider_turn_id=row[7],
            text_hash=row[8],
        )

    def claim_assistant_output(
        self,
        *,
        session_id: str,
        inbound_id: str,
        provider: str,
        assistant_message_id: str,
        text: str,
        completed_at: Optional[datetime] = None,
        provider_turn_id: Optional[str] = None,
        claim_timeout_seconds: int = 120,
    ) -> bool:
        """Claim one assistant output for relay, returning False for duplicates."""
        now = _utc_now()
        now_iso = now.isoformat()
        completed_iso = _coerce_utc(completed_at).isoformat()
        text_hash = _hash_text(text)
        preview = text[:400]
        stale_before = now - timedelta(seconds=claim_timeout_seconds)
        with self._lock:
            cursor = self._conn.cursor()
            cursor.execute(
                """
                SELECT relayed_at, relay_claimed_at
                FROM assistant_outputs
                WHERE session_id = ?
                  AND inbound_id = ?
                  AND provider = ?
                  AND assistant_message_id = ?
                """,
                (session_id, inbound_id, provider, assistant_message_id),
            )
            row = cursor.fetchone()
            if row:
                relayed_at, claimed_at = row
                if relayed_at:
                    return False
                claimed_dt = _parse_datetime(claimed_at)
                if claimed_dt and claimed_dt > stale_before:
                    return False
                cursor.execute(
                    """
                    UPDATE assistant_outputs
                    SET relay_claimed_at = ?,
                        completed_at = ?,
                        text_hash = ?,
                        text_preview = ?,
                        provider_turn_id = COALESCE(?, provider_turn_id)
                    WHERE session_id = ?
                      AND inbound_id = ?
                      AND provider = ?
                      AND assistant_message_id = ?
                    """,
                    (
                        now_iso,
                        completed_iso,
                        text_hash,
                        preview,
                        provider_turn_id,
                        session_id,
                        inbound_id,
                        provider,
                        assistant_message_id,
                    ),
                )
            else:
                cursor.execute(
                    """
                    INSERT INTO assistant_outputs
                    (session_id, inbound_id, provider, provider_turn_id, assistant_message_id,
                     completed_at, text_hash, text_preview, relay_claimed_at, relayed_at, telegram_thread_id)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL)
                    """,
                    (
                        session_id,
                        inbound_id,
                        provider,
                        provider_turn_id,
                        assistant_message_id,
                        completed_iso,
                        text_hash,
                        preview,
                        now_iso,
                    ),
                )
            self._conn.commit()
        return True

    def mark_assistant_output_relayed(
        self,
        *,
        session_id: str,
        inbound_id: str,
        provider: str,
        assistant_message_id: str,
        telegram_thread_id: Optional[int] = None,
        relayed_at: Optional[datetime] = None,
    ) -> None:
        relay_ts = _coerce_utc(relayed_at).isoformat()
        with self._lock:
            self._conn.execute(
                """
                UPDATE assistant_outputs
                SET relayed_at = ?,
                    telegram_thread_id = ?
                WHERE session_id = ?
                  AND inbound_id = ?
                  AND provider = ?
                  AND assistant_message_id = ?
                """,
                (relay_ts, telegram_thread_id, session_id, inbound_id, provider, assistant_message_id),
            )
            self._conn.commit()

    def release_assistant_output_claim(
        self,
        *,
        session_id: str,
        inbound_id: str,
        provider: str,
        assistant_message_id: str,
    ) -> None:
        """Release a claim after notifier rejection so a later hook may retry."""
        with self._lock:
            self._conn.execute(
                """
                UPDATE assistant_outputs
                SET relay_claimed_at = NULL
                WHERE session_id = ?
                  AND inbound_id = ?
                  AND provider = ?
                  AND assistant_message_id = ?
                  AND relayed_at IS NULL
                """,
                (session_id, inbound_id, provider, assistant_message_id),
            )
            self._conn.commit()


def _extract_visible_message_text(entry: dict) -> str:
    message = entry.get("message") if isinstance(entry.get("message"), dict) else {}
    content = message.get("content", entry.get("content", []))
    if isinstance(content, str):
        return content.strip()
    texts: list[str] = []
    if isinstance(content, list):
        for item in content:
            if isinstance(item, str):
                texts.append(item)
            elif isinstance(item, dict) and item.get("type") == "text":
                text = item.get("text")
                if isinstance(text, str):
                    texts.append(text)
    return "\n".join(texts).strip()


def _extract_visible_assistant_text(entry: dict) -> str:
    return _extract_visible_message_text(entry)


def _assistant_message_id(entry: dict, line_start_offset: int, text: str) -> str:
    message = entry.get("message") if isinstance(entry.get("message"), dict) else {}
    for value in (
        entry.get("uuid"),
        entry.get("message_id"),
        entry.get("id"),
        message.get("id"),
    ):
        if value:
            return str(value)
    return f"transcript:{line_start_offset}:{_hash_text(text)[:16]}"


def find_claude_inbound_turn_boundary_offset(
    transcript_path: str,
    turn: InboundTurn,
) -> Optional[int]:
    """Find the byte offset immediately after the matching inbound user line."""
    if not turn.text_hash:
        return None

    path = Path(transcript_path).expanduser()
    if not path.exists():
        return None

    try:
        data = path.read_bytes()
    except OSError as exc:
        logger.warning("Could not read Claude transcript for inbound boundary %s: %s", transcript_path, exc)
        return None

    offset = 0
    matched_offset: Optional[int] = None
    for raw_line in data.splitlines(keepends=True):
        offset += len(raw_line)
        if not raw_line.strip():
            continue
        try:
            entry = json.loads(raw_line.decode("utf-8").strip())
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            logger.debug("Skipping malformed Claude transcript line for inbound boundary: %s", exc)
            continue
        if not isinstance(entry, dict) or entry.get("type") != "user":
            continue
        text = _extract_visible_message_text(entry)
        if text and _hash_text(text) == turn.text_hash:
            matched_offset = offset

    return matched_offset


def collect_claude_assistant_outputs_after_turn(
    transcript_path: str,
    turn: InboundTurn,
) -> list[ClaudeAssistantOutput]:
    """Collect visible assistant messages provably after an inbound turn boundary."""
    path = Path(transcript_path).expanduser()
    if not path.exists():
        return []

    try:
        data = path.read_bytes()
    except OSError as exc:
        logger.warning("Could not read Claude transcript for relay %s: %s", transcript_path, exc)
        return []

    start_offset = turn.transcript_offset
    if start_offset is not None:
        if start_offset > len(data):
            return []
        scan_data = data[start_offset:]
        base_offset = start_offset
        require_timestamp = False
    else:
        scan_data = data
        base_offset = 0
        require_timestamp = True

    outputs: list[ClaudeAssistantOutput] = []
    line_start = 0
    for line_number, raw_line in enumerate(scan_data.splitlines(), start=1):
        absolute_offset = base_offset + line_start
        line_start += len(raw_line) + 1
        if not raw_line.strip():
            continue
        try:
            entry = json.loads(raw_line.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            logger.debug("Skipping malformed Claude transcript line for relay: %s", exc)
            continue
        if not isinstance(entry, dict) or entry.get("type") != "assistant":
            continue

        completed_at = _parse_datetime(str(entry.get("timestamp")) if entry.get("timestamp") else None)
        if require_timestamp:
            if completed_at is None or completed_at < turn.delivered_at:
                continue
        text = _extract_visible_assistant_text(entry)
        if not text:
            continue
        outputs.append(
            ClaudeAssistantOutput(
                assistant_message_id=_assistant_message_id(entry, absolute_offset, text),
                text=text,
                completed_at=completed_at or _utc_now(),
                line_start_offset=absolute_offset,
                line_number=line_number,
            )
        )
    return outputs
