"""Summary analytics for the Android client."""

from __future__ import annotations

import sqlite3
from collections import Counter, defaultdict
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Iterable, Optional

from .models import Session, SessionStatus


_MESSAGE_QUEUE_DB_DEFAULT = Path("~/.local/share/claude-sessions/message_queue.db").expanduser()
_SERVER_LOG_DEFAULT = Path("/tmp/session-manager.log")


def _utc_now() -> datetime:
    return datetime.now(timezone.utc)


def _parse_any_datetime(value: datetime | str | None) -> Optional[datetime]:
    if not value:
        return None
    if isinstance(value, datetime):
        if value.tzinfo is None:
            return value.replace(tzinfo=timezone.utc)
        return value.astimezone(timezone.utc)
    value = value.strip()
    if not value:
        return None
    try:
        parsed = datetime.fromisoformat(value)
    except ValueError:
        try:
            parsed = datetime.strptime(value, "%Y-%m-%d %H:%M:%S")
        except ValueError:
            return None
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def _parse_log_timestamp(line: str) -> Optional[datetime]:
    prefix = line[:23]
    try:
        parsed = datetime.strptime(prefix, "%Y-%m-%d %H:%M:%S,%f")
    except ValueError:
        return None
    return parsed.replace(tzinfo=timezone.utc)


def _bucket_start(dt: datetime, bucket_hours: int) -> datetime:
    dt = dt.astimezone(timezone.utc)
    hour = (dt.hour // bucket_hours) * bucket_hours
    return dt.replace(hour=hour, minute=0, second=0, microsecond=0)


def _series_points(
    timestamps: Iterable[datetime],
    *,
    window_start: datetime,
    window_end: datetime,
    bucket_hours: int,
) -> list[tuple[datetime, int]]:
    bucket_count = int((window_end - window_start).total_seconds() // (bucket_hours * 3600))
    buckets = {
        window_start + timedelta(hours=index * bucket_hours): 0
        for index in range(bucket_count)
    }
    for ts in timestamps:
        if ts < window_start or ts >= window_end:
            continue
        start = _bucket_start(ts, bucket_hours)
        if start < window_start:
            start = window_start
        if start in buckets:
            buckets[start] += 1
    return list(buckets.items())


def _delta_pct(current: int, previous: int) -> Optional[float]:
    if previous <= 0:
        return None
    return round(((current - previous) / previous) * 100.0, 1)


def _repo_label(working_dir: str) -> str:
    normalized = working_dir.strip() or "unknown"
    return normalized.rstrip("/").split("/")[-1] or normalized


def _safe_int(value: Any) -> int:
    try:
        return int(value)
    except Exception:
        return 0


@dataclass
class MobileAnalyticsBuilder:
    session_manager: Any
    config: Optional[dict] = None

    def __post_init__(self) -> None:
        self.config = self.config or {}
        paths = self.config.get("paths", {})
        self.message_queue_db_path = Path(paths.get("message_queue_db", str(_MESSAGE_QUEUE_DB_DEFAULT))).expanduser()
        self.server_log_path = Path(paths.get("server_log_file", str(_SERVER_LOG_DEFAULT))).expanduser()

    def build_summary(self) -> dict[str, Any]:
        now = _utc_now()
        current_start = now - timedelta(hours=24)
        previous_start = now - timedelta(hours=48)
        sessions = list(self.session_manager.list_sessions()) if self.session_manager else []

        send_times_current, send_times_previous = self._read_send_timestamps(current_start, previous_start, now)
        track_times_current = self._read_track_remind_timestamps(current_start, now)
        active_tracks, overdue_tracks = self._read_track_registration_counts()
        spawn_times_current, spawn_times_previous, restart_count, self_heal_count = self._read_log_metrics(
            current_start=current_start,
            previous_start=previous_start,
            now=now,
        )

        sends_series = _series_points(send_times_current, window_start=current_start, window_end=now, bucket_hours=2)
        spawn_series = _series_points(spawn_times_current, window_start=current_start, window_end=now, bucket_hours=2)
        track_series = _series_points(track_times_current, window_start=current_start, window_end=now, bucket_hours=2)

        active_states = Counter(self._activity_state(session) for session in sessions)
        provider_counts = Counter((getattr(session, "provider", None) or "claude") for session in sessions)
        repo_counts: dict[str, dict[str, Any]] = defaultdict(lambda: {"session_count": 0, "tokens_used": 0})
        total_tokens_live = 0
        longest_running = []
        for session in sessions:
            working_dir = str(getattr(session, "working_dir", "") or "")
            repo = _repo_label(working_dir)
            repo_counts[repo]["session_count"] += 1
            tokens_used = _safe_int(getattr(session, "tokens_used", 0))
            repo_counts[repo]["tokens_used"] += tokens_used
            total_tokens_live += tokens_used

            created_at = _parse_any_datetime(getattr(session, "created_at", None))
            if created_at:
                age_hours = round((now - created_at).total_seconds() / 3600.0, 1)
            else:
                age_hours = 0.0
            longest_running.append(
                {
                    "id": getattr(session, "id", ""),
                    "name": self._display_name(session),
                    "repo": repo,
                    "provider": getattr(session, "provider", None) or "claude",
                    "age_hours": age_hours,
                }
            )

        total_sessions = len(sessions)
        provider_distribution = [
            {
                "key": provider,
                "label": provider,
                "count": count,
                "share_pct": round((count / total_sessions) * 100.0, 1) if total_sessions else 0.0,
            }
            for provider, count in provider_counts.most_common()
        ]
        repo_distribution = sorted(
            (
                {
                    "key": repo,
                    "label": repo,
                    "session_count": payload["session_count"],
                    "tokens_used": payload["tokens_used"],
                    "share_pct": round((payload["session_count"] / total_sessions) * 100.0, 1) if total_sessions else 0.0,
                }
                for repo, payload in repo_counts.items()
            ),
            key=lambda item: (-item["session_count"], item["label"]),
        )[:6]

        summary = {
            "generated_at": now.isoformat(),
            "window_hours": 24,
            "kpis": {
                "active_sessions": {
                    "label": "Active sessions",
                    "value": total_sessions,
                },
                "sends_24h": {
                    "label": "Sends",
                    "value": len(send_times_current),
                    "delta_pct": _delta_pct(len(send_times_current), len(send_times_previous)),
                },
                "spawns_24h": {
                    "label": "Dispatches",
                    "value": len(spawn_times_current),
                    "delta_pct": _delta_pct(len(spawn_times_current), len(spawn_times_previous)),
                },
                "active_tracks": {
                    "label": "Tracks active",
                    "value": active_tracks,
                },
                "overdue_tracks": {
                    "label": "Overdue tracks",
                    "value": overdue_tracks,
                },
                "incidents_24h": {
                    "label": "Incidents",
                    "value": restart_count + self_heal_count,
                },
            },
            "throughput": [
                {
                    "bucket_start": bucket.isoformat(),
                    "bucket_label": bucket.strftime("%H:%M"),
                    "sends": send_count,
                    "spawns": dict(spawn_series).get(bucket, 0),
                    "track_reminders": dict(track_series).get(bucket, 0),
                }
                for bucket, send_count in sends_series
            ],
            "state_distribution": [
                {"key": key, "label": key.replace("_", " "), "count": active_states.get(key, 0)}
                for key in ("working", "thinking", "waiting", "idle")
            ],
            "provider_distribution": provider_distribution,
            "repo_distribution": repo_distribution,
            "longest_running": sorted(longest_running, key=lambda item: (-item["age_hours"], item["name"]))[:5],
            "reliability": {
                "restart_count_24h": restart_count,
                "self_heal_count_24h": self_heal_count,
            },
            "totals": {
                "tokens_live": total_tokens_live,
                "track_reminders_24h": len(track_times_current),
            },
        }
        return summary

    def _activity_state(self, session: Session) -> str:
        getter = getattr(self.session_manager, "get_activity_state", None)
        if callable(getter):
            state = str(getter(session) or "").strip().lower()
            if state in {"waiting_permission", "waiting_input"}:
                return "waiting"
            if state:
                return state
        status = getattr(session, "status", SessionStatus.IDLE)
        if status == SessionStatus.RUNNING:
            return "working"
        return "idle"

    def _display_name(self, session: Session) -> str:
        getter = getattr(self.session_manager, "get_effective_session_name", None)
        if callable(getter):
            value = str(getter(session) or "").strip()
            if value:
                return value
        return str(getattr(session, "friendly_name", None) or getattr(session, "name", "") or getattr(session, "id", ""))

    def _read_send_timestamps(
        self,
        current_start: datetime,
        previous_start: datetime,
        now: datetime,
    ) -> tuple[list[datetime], list[datetime]]:
        if not self.message_queue_db_path.exists():
            return [], []
        query = """
            SELECT queued_at
            FROM message_queue
            WHERE from_sm_send = 1
              AND queued_at >= ?
              AND queued_at < ?
        """
        try:
            with sqlite3.connect(str(self.message_queue_db_path)) as conn:
                rows = conn.execute(query, (previous_start.isoformat(), now.isoformat())).fetchall()
        except sqlite3.Error:
            return [], []
        previous: list[datetime] = []
        current: list[datetime] = []
        for (raw_ts,) in rows:
            parsed = _parse_any_datetime(raw_ts)
            if parsed is None:
                continue
            if parsed >= current_start:
                current.append(parsed)
            else:
                previous.append(parsed)
        return current, previous

    def _read_track_remind_timestamps(self, current_start: datetime, now: datetime) -> list[datetime]:
        if not self.message_queue_db_path.exists():
            return []
        query = """
            SELECT queued_at
            FROM message_queue
            WHERE message_category = 'track_remind'
              AND queued_at >= ?
              AND queued_at < ?
        """
        try:
            with sqlite3.connect(str(self.message_queue_db_path)) as conn:
                rows = conn.execute(query, (current_start.isoformat(), now.isoformat())).fetchall()
        except sqlite3.Error:
            return []
        return [parsed for (raw_ts,) in rows if (parsed := _parse_any_datetime(raw_ts)) is not None]

    def _read_track_registration_counts(self) -> tuple[int, int]:
        if not self.message_queue_db_path.exists():
            return 0, 0
        query = """
            SELECT
                SUM(CASE WHEN is_active = 1 AND cancel_on_reply_session_id IS NOT NULL AND TRIM(cancel_on_reply_session_id) != '' THEN 1 ELSE 0 END),
                SUM(CASE WHEN is_active = 1 AND soft_fired = 1 AND cancel_on_reply_session_id IS NOT NULL AND TRIM(cancel_on_reply_session_id) != '' THEN 1 ELSE 0 END)
            FROM remind_registrations
        """
        try:
            with sqlite3.connect(str(self.message_queue_db_path)) as conn:
                row = conn.execute(query).fetchone()
        except sqlite3.Error:
            return 0, 0
        if not row:
            return 0, 0
        return _safe_int(row[0]), _safe_int(row[1])

    def _read_log_metrics(
        self,
        *,
        current_start: datetime,
        previous_start: datetime,
        now: datetime,
    ) -> tuple[list[datetime], list[datetime], int, int]:
        if not self.server_log_path.exists():
            return [], [], 0, 0
        current_spawns: list[datetime] = []
        previous_spawns: list[datetime] = []
        restart_count = 0
        self_heal_count = 0
        for line in self.server_log_path.read_text(errors="ignore").splitlines():
            timestamp = _parse_log_timestamp(line)
            if timestamp is None or timestamp < previous_start or timestamp >= now:
                continue
            if "Created session " in line and "Created session with CLI prompt" not in line:
                if timestamp >= current_start:
                    current_spawns.append(timestamp)
                else:
                    previous_spawns.append(timestamp)
            if "Starting Claude Session Manager..." in line and timestamp >= current_start:
                restart_count += 1
            if "Recovered " in line and timestamp >= current_start:
                self_heal_count += 1
        return current_spawns, previous_spawns, restart_count, self_heal_count
