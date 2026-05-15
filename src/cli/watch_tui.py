"""Curses dashboard for sm watch (#309)."""

from __future__ import annotations

import curses
import os
import queue
import re
import subprocess
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Optional

SPINNER_FRAMES = ["|", "/", "-", "\\"]
ANSI_RE = re.compile(r"\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])")

# (title + column header + flash + footer)
_RESERVED_SCREEN_ROWS = 4
_RETIRE_CONFIRM_SECONDS = 5.0

# name, min_width, weight, align
_COLUMN_SPECS = [
    ("Session", 16, 3, "left"),
    ("ID", 8, 0, "left"),
    ("Parent", 18, 2, "left"),
    ("Role", 8, 1, "left"),
    ("Provider", 10, 1, "left"),
    ("Activity", 11, 1, "left"),
    ("Status", 8, 1, "left"),
    ("Last", 24, 3, "left"),
    ("Age", 6, 0, "right"),
]
_COLUMN_ORDER = [name for name, _, _, _ in _COLUMN_SPECS]
_COLUMN_SEP = "  "
_COLUMN_FLOORS = {
    "Session": 8,
    "ID": 4,
    "Parent": 8,
    "Role": 4,
    "Provider": 6,
    "Activity": 7,
    "Status": 5,
    "Last": 8,
    "Age": 3,
}
_DYNAMIC_COLUMN_CAPS = {
    "ID": 8,
    "Parent": 36,
    "Role": 16,
    "Provider": 10,
    "Activity": 18,
    "Status": 10,
    "Last": 36,
    "Age": 6,
}

_RESTORE_COLUMN_SPECS = [
    ("Session", 16, 4, "left"),
    ("ID", 8, 0, "left"),
    ("Parent", 18, 2, "left"),
    ("Role", 8, 1, "left"),
    ("Provider", 10, 1, "left"),
    ("Repo", 12, 2, "left"),
    ("Last Active", 11, 1, "left"),
    ("Retired", 8, 1, "left"),
    ("Restore", 10, 1, "left"),
]
_RESTORE_COLUMN_FLOORS = {
    "Session": 8,
    "ID": 4,
    "Parent": 8,
    "Role": 4,
    "Provider": 6,
    "Repo": 6,
    "Last Active": 5,
    "Retired": 5,
    "Restore": 5,
}
_RESTORE_DYNAMIC_COLUMN_CAPS = {
    "ID": 8,
    "Parent": 36,
    "Role": 16,
    "Provider": 10,
    "Repo": 28,
    "Last Active": 12,
    "Retired": 12,
    "Restore": 12,
}


@dataclass
class WatchRow:
    """A render row in sm watch."""

    kind: str  # repo, session, status, detail, repo_ref
    text: str = ""
    session_id: Optional[str] = None
    repo_key: Optional[str] = None
    activity_state: str = "idle"
    columns: dict[str, str] = field(default_factory=dict)


@dataclass
class DetailSnapshot:
    """Cached detail payload for one session."""

    action_lines: list[str]
    tail_lines: list[str]
    fetched_at: float
    loading: bool = False
    last_error: Optional[str] = None


@dataclass
class RetireConfirmation:
    """Armed retire confirmation for one selected session."""

    session_id: str
    expires_at: float


def _retire_confirmation_matches(
    confirmation: Optional[RetireConfirmation],
    selected_session_id: Optional[str],
    now: float,
) -> bool:
    return bool(
        confirmation
        and selected_session_id
        and confirmation.session_id == selected_session_id
        and now <= confirmation.expires_at
    )


def _arm_retire_confirmation(session_id: str, now: float) -> RetireConfirmation:
    return RetireConfirmation(session_id=session_id, expires_at=now + _RETIRE_CONFIRM_SECONDS)


class DetailFetchWorker:
    """Background worker for non-blocking detail reads."""

    def __init__(self, client, codex_projection_enabled: bool):
        self.client = client
        self.codex_projection_enabled = codex_projection_enabled
        self._queue: queue.Queue[dict] = queue.Queue(maxsize=256)
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._queued_ids: set[str] = set()
        self._cache: dict[str, DetailSnapshot] = {}
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self):
        self._stop.set()
        self._thread.join(timeout=1.0)

    def request(self, session: dict):
        session_id = session.get("id")
        if not session_id:
            return
        with self._lock:
            if session_id in self._queued_ids:
                return
            self._queued_ids.add(session_id)
            existing = self._cache.get(session_id)
            if existing is None:
                self._cache[session_id] = DetailSnapshot(
                    action_lines=[],
                    tail_lines=[],
                    fetched_at=0.0,
                    loading=True,
                )
            else:
                existing.loading = True

        payload = {"session_id": session_id, "session": session}
        try:
            self._queue.put_nowait(payload)
        except queue.Full:
            with self._lock:
                self._queued_ids.discard(session_id)

    def get(self, session_id: str) -> Optional[DetailSnapshot]:
        with self._lock:
            return self._cache.get(session_id)

    def _run(self):
        while not self._stop.is_set():
            try:
                payload = self._queue.get(timeout=0.1)
            except queue.Empty:
                continue

            session_id = payload["session_id"]
            session = payload["session"]
            with self._lock:
                self._queued_ids.discard(session_id)

            try:
                snapshot = self._fetch(session)
            except Exception as exc:
                with self._lock:
                    existing = self._cache.get(session_id)
                    if existing is None:
                        existing = DetailSnapshot([], [], time.monotonic())
                        self._cache[session_id] = existing
                    existing.loading = False
                    existing.last_error = str(exc)
                    existing.fetched_at = time.monotonic()
                continue

            with self._lock:
                self._cache[session_id] = snapshot

    def _fetch(self, session: dict) -> DetailSnapshot:
        session_id = session.get("id")
        provider = session.get("provider", "claude")

        action_lines = self._fetch_actions(session_id, provider)
        tail_lines = self._fetch_tail(session_id)

        return DetailSnapshot(
            action_lines=action_lines,
            tail_lines=tail_lines,
            fetched_at=time.monotonic(),
            loading=False,
            last_error=None,
        )

    def _fetch_actions(self, session_id: str, provider: str) -> list[str]:
        if provider == "codex":
            return ["n/a (no hooks)"]

        if provider == "codex-fork":
            payload = self.client.get_tool_calls(session_id, limit=10, timeout=2)
            if payload is None:
                return ["n/a (unavailable)"]

            calls = payload.get("tool_calls") or []
            if not calls:
                return ["-"]

            lines: list[str] = []
            for row in calls[:10]:
                tool = row.get("tool_name") or "-"
                age = _age_from_iso(row.get("timestamp"))
                if age != "-":
                    lines.append(f"{tool} ({age})")
                else:
                    lines.append(tool)
            return lines

        if provider == "codex-app":
            if not self.codex_projection_enabled:
                return ["n/a (projection disabled)"]
            payload = self.client.get_activity_actions(session_id, limit=10)
            if payload is None:
                return ["n/a (projection disabled)"]

            actions = payload.get("actions") or []
            if not actions:
                return ["-"]

            lines: list[str] = []
            for action in actions[:10]:
                summary = action.get("summary_text") or action.get("action_kind") or "action"
                status = action.get("status")
                when = action.get("ended_at") or action.get("started_at")
                age = _age_from_iso(when)
                suffix = f" [{status}]" if status else ""
                age_suffix = f" ({age})" if age != "-" else ""
                lines.append(f"{summary}{suffix}{age_suffix}")
            return lines

        payload = self.client.get_tool_calls(session_id, limit=10, timeout=2)
        if payload is None:
            return ["n/a (unavailable)"]

        calls = payload.get("tool_calls") or []
        if not calls:
            return ["-"]

        lines: list[str] = []
        for row in calls[:10]:
            tool = row.get("tool_name") or "-"
            age = _age_from_iso(row.get("timestamp"))
            if age != "-":
                lines.append(f"{tool} ({age})")
            else:
                lines.append(tool)
        return lines

    def _fetch_tail(self, session_id: str) -> list[str]:
        payload = self.client.get_output(session_id, lines=10, timeout=2)
        if payload is None:
            return ["n/a (unavailable)"]

        output = payload.get("output") or ""
        if not output:
            return ["-"]

        clean = ANSI_RE.sub("", output)
        lines = clean.splitlines()
        if not lines and clean.strip():
            return [clean.strip()]
        return lines[-10:] if lines else ["-"]


def _session_name(session: dict) -> str:
    return session.get("friendly_name") or session.get("name") or session.get("id", "unknown")


def _parent_label(session: dict, sessions_by_id: dict[str, dict]) -> str:
    parent_id = session.get("parent_session_id")
    if not parent_id:
        return "-"
    parent = sessions_by_id.get(parent_id)
    if not parent:
        return parent_id
    parent_name = _session_name(parent)
    if parent_name == parent_id:
        return parent_id
    return f"{parent_name} [{parent_id}]"


def _repo_label(working_dir: str) -> str:
    if not working_dir:
        return "unknown/"
    normalized = os.path.normpath(working_dir)
    return f"{os.path.basename(normalized) or normalized}/"


def _repo_key(working_dir: str) -> str:
    if not working_dir:
        return "unknown"
    try:
        return str(Path(working_dir).expanduser().resolve())
    except Exception:
        return os.path.normpath(working_dir)


def _parse_iso(ts: Optional[str]) -> Optional[datetime]:
    if not ts:
        return None
    parsed = str(ts).replace("Z", "+00:00")
    try:
        return datetime.fromisoformat(parsed)
    except Exception:
        return None


def _elapsed_label(seconds: int) -> str:
    if seconds < 60:
        return f"{seconds}s"
    if seconds < 3600:
        return f"{seconds // 60}m"
    return f"{seconds // 3600}h"


def _age_from_iso(ts: Optional[str]) -> str:
    parsed = _parse_iso(ts)
    if not parsed:
        return "-"
    now = datetime.now(parsed.tzinfo) if parsed.tzinfo else datetime.now()
    seconds = max(0, int((now - parsed).total_seconds()))
    return _elapsed_label(seconds)


def _status_line(session: dict) -> Optional[str]:
    status_text = session.get("agent_status_text")
    if not status_text:
        return None
    status_at = session.get("agent_status_at")
    age_suffix = f" ({_age_from_iso(status_at)})" if status_at else ""
    return f'status: "{status_text}"{age_suffix}'


def _task_completion_line(session: dict) -> Optional[str]:
    completed_at = session.get("agent_task_completed_at")
    if not completed_at:
        return None
    return f"task: completed ({_age_from_iso(completed_at)})"


def _pending_adoption_lines(session: dict) -> list[str]:
    proposals = session.get("pending_adoption_proposals") or []
    lines: list[str] = []
    for proposal in proposals:
        if proposal.get("status") != "pending":
            continue
        proposer_name = proposal.get("proposer_name") or proposal.get("proposer_session_id") or "unknown"
        proposer_id = proposal.get("proposer_session_id") or "unknown"
        created_at = proposal.get("created_at")
        age = _age_from_iso(created_at)
        age_suffix = f" ({age})" if age != "-" else ""
        lines.append(
            f"adopt: pending from {proposer_name} [{proposer_id}]{age_suffix}  [A accept / X reject]"
        )
    return lines


def _format_age(last_activity: Optional[str], activity_state: str) -> str:
    parsed = _parse_iso(last_activity)
    if not parsed:
        return "-"
    now = datetime.now(parsed.tzinfo) if parsed.tzinfo else datetime.now()
    seconds = max(0, int((now - parsed).total_seconds()))
    if activity_state in ("working", "thinking"):
        return f"{seconds}s"
    return f"{seconds // 60}m"


def _state_label(activity_state: str, spinner_index: int) -> str:
    if activity_state == "working":
        return "working *"
    if activity_state == "thinking":
        return f"thinking {SPINNER_FRAMES[spinner_index % len(SPINNER_FRAMES)]}"
    return activity_state


def _last_column(session: dict, codex_projection_enabled: bool) -> str:
    provider = session.get("provider", "claude")

    if provider == "codex":
        return "n/a (no hooks)"

    if provider == "codex-app":
        if not codex_projection_enabled:
            return "n/a (projection disabled)"
        summary = session.get("last_action_summary")
        at = session.get("last_action_at")
        if summary and at:
            return f"{summary} ({_age_from_iso(at)})"
        if summary:
            return summary
        return "-"

    tool_name = session.get("last_tool_name")
    tool_at = session.get("last_tool_call")
    if tool_name and tool_at:
        return f"{tool_name} ({_age_from_iso(tool_at)})"
    if tool_name:
        return str(tool_name)
    if tool_at:
        return f"tool ({_age_from_iso(tool_at)})"
    return "-"


def _thinking_duration(session: dict, codex_projection_enabled: bool) -> str:
    provider = session.get("provider", "claude")
    ts: Optional[str]

    if provider == "codex-app":
        ts = session.get("last_action_at") if codex_projection_enabled else None
        if not ts:
            ts = session.get("last_activity")
    elif provider == "claude":
        ts = session.get("last_tool_call") or session.get("last_activity")
    else:
        ts = session.get("last_activity")

    return _age_from_iso(ts)


def can_attach_session(session: dict) -> bool:
    """Return True if Enter attach should be enabled for a session."""
    return session.get("provider", "claude") != "codex-app"


def filter_sessions(
    sessions: list[dict],
    repo_filter: Optional[str] = None,
    role_filter: Optional[str] = None,
    text_filter: Optional[str] = None,
) -> list[dict]:
    """Apply repo/role/text filters for watch rows."""
    filtered_ids: list[str] = []
    repo_root = None
    if repo_filter:
        try:
            repo_root = str(Path(repo_filter).expanduser().resolve())
        except Exception:
            repo_root = None
    role_filter_norm = role_filter.lower() if role_filter else None
    text_filter_norm = text_filter.lower() if text_filter else None

    sessions_by_id = {session.get("id"): session for session in sessions if session.get("id")}
    children_by_parent: dict[str, list[str]] = {}
    for session in sessions:
        session_id = session.get("id")
        parent_id = session.get("parent_session_id")
        if session_id and parent_id:
            children_by_parent.setdefault(parent_id, []).append(session_id)

    for session in sessions:
        working_dir = session.get("working_dir") or ""
        if repo_root:
            try:
                resolved = str(Path(working_dir).expanduser().resolve())
            except Exception:
                continue
            if resolved != repo_root and not resolved.startswith(repo_root + os.sep):
                continue

        role = (session.get("role") or "").strip()
        if role_filter_norm and role.lower() != role_filter_norm:
            continue

        if text_filter_norm:
            parent = sessions_by_id.get(session.get("parent_session_id"))
            aliases = session.get("aliases") or []
            if isinstance(aliases, str):
                aliases = [aliases]
            haystack = " ".join(
                [
                    _session_name(session),
                    session.get("id", ""),
                    session.get("role", "") or "",
                    session.get("provider", "") or "",
                    session.get("status", "") or "",
                    session.get("working_dir", "") or "",
                    session.get("current_task", "") or "",
                    _session_name(parent) if parent else "",
                    session.get("parent_session_id", "") or "",
                    " ".join(str(alias) for alias in aliases),
                ]
            ).lower()
            if text_filter_norm not in haystack:
                continue

        session_id = session.get("id")
        if session_id:
            filtered_ids.append(session_id)

    if not filtered_ids:
        return []

    filtered_id_set = set(filtered_ids)
    if not repo_filter or role_filter_norm or text_filter_norm:
        return [session for session in sessions if session.get("id") in filtered_id_set]

    included_ids = set(filtered_id_set)

    # Pure repo-scoped watch views should preserve tree context so cross-worktree
    # children remain visibly attached to their parent session.
    for session_id in filtered_ids:
        parent_id = sessions_by_id.get(session_id, {}).get("parent_session_id")
        while parent_id:
            if parent_id in included_ids:
                break
            parent = sessions_by_id.get(parent_id)
            if not parent:
                break
            included_ids.add(parent_id)
            parent_id = parent.get("parent_session_id")

    stack = list(filtered_ids)
    while stack:
        current_id = stack.pop()
        for child_id in children_by_parent.get(current_id, []):
            if child_id in included_ids:
                continue
            included_ids.add(child_id)
            stack.append(child_id)

    return [session for session in sessions if session.get("id") in included_ids]


def _detail_lines(
    session: dict,
    detail: Optional[DetailSnapshot],
    codex_projection_enabled: bool,
) -> list[str]:
    activity_state = session.get("activity_state", "idle")
    name = _session_name(session)
    lines = [
        (
            f"meta: {name} [{session.get('id', '')}] "
            f"provider={session.get('provider', 'claude')} "
            f"activity={activity_state} status={session.get('status', '-') or '-'} "
            f"role={session.get('role') or '-'}"
        ),
        f"thinking duration: {_thinking_duration(session, codex_projection_enabled)}",
    ]

    if session.get("context_monitor_enabled"):
        tokens = int(session.get("tokens_used") or 0)
        lines.append(f"context size: {tokens:,} tokens")
    else:
        lines.append("context size: n/a (monitor off)")

    lines.append("last 10 tool calls/actions:")
    if detail is None or detail.loading:
        lines.append("  loading...")
    else:
        if detail.last_error:
            lines.append(f"  warning: {detail.last_error}")
        for item in detail.action_lines[:10]:
            lines.append(f"  {item}")

    lines.append("last 10 tail lines:")
    if detail is None or detail.loading:
        lines.append("  loading...")
    else:
        for item in detail.tail_lines[:10]:
            lines.append(f"  {item}")

    return lines


def build_watch_rows(
    sessions: list[dict],
    spinner_index: int = 0,
    expanded_session_ids: Optional[set[str]] = None,
    detail_cache: Optional[dict[str, DetailSnapshot]] = None,
    codex_projection_enabled: bool = True,
) -> tuple[list[WatchRow], list[str], int]:
    """Build grouped rows and selectable session IDs."""
    rows: list[WatchRow] = []
    selectable: list[str] = []
    expanded = expanded_session_ids or set()
    sessions_by_id = {session["id"]: session for session in sessions if session.get("id")}
    groups: dict[str, list[dict]] = {}
    roots_by_repo: dict[str, list[dict]] = {}
    same_repo_children: dict[str, list[dict]] = {}
    cross_repo_children: dict[str, dict[str, list[dict]]] = {}

    for session in sessions:
        key = _repo_key(session.get("working_dir", ""))
        groups.setdefault(key, []).append(session)
        parent_id = session.get("parent_session_id")
        if not parent_id:
            roots_by_repo.setdefault(key, []).append(session)
            continue

        parent = sessions_by_id.get(parent_id)
        if parent is None:
            roots_by_repo.setdefault(key, []).append(session)
            continue

        parent_repo_key = _repo_key(parent.get("working_dir", ""))
        if parent_repo_key == key:
            same_repo_children.setdefault(parent_id, []).append(session)
            continue

        cross_repo_children.setdefault(parent_id, {}).setdefault(key, []).append(session)

    sort_key = lambda s: (_session_name(s).lower(), s.get("id", ""))
    for root_list in roots_by_repo.values():
        root_list.sort(key=sort_key)
    for child_list in same_repo_children.values():
        child_list.sort(key=sort_key)
    for repo_map in cross_repo_children.values():
        for child_list in repo_map.values():
            child_list.sort(key=sort_key)

    def _tree_prefix(ancestors_last: list[bool], is_last: bool) -> str:
        connector = "`-" if is_last else "|-"
        prefix_parts = []
        for ancestor_is_last in ancestors_last:
            prefix_parts.append("   " if ancestor_is_last else "|  ")
        return "".join(prefix_parts) + connector

    def _status_prefix(ancestors_last: list[bool]) -> str:
        prefix_parts = []
        for ancestor_is_last in ancestors_last:
            prefix_parts.append("   " if ancestor_is_last else "|  ")
        return "".join(prefix_parts) + "  "

    def render_session(session: dict, ancestors_last: list[bool], is_last: bool):
        tree_prefix = _tree_prefix(ancestors_last, is_last)
        status_prefix = _status_prefix(ancestors_last)

        role = session.get("role") or "-"
        provider = session.get("provider", "claude")
        activity_state = session.get("activity_state", "idle")
        status = session.get("status") or "-"

        columns = {
            "Session": f"{tree_prefix}{_session_name(session)}",
            "ID": session.get("id", ""),
            "Parent": _parent_label(session, sessions_by_id),
            "Role": role,
            "Provider": provider,
            "Activity": _state_label(activity_state, spinner_index),
            "Status": status,
            "Last": _last_column(session, codex_projection_enabled),
            "Age": _format_age(session.get("last_activity"), activity_state),
        }

        session_id = session.get("id")
        rows.append(
            WatchRow(
                kind="session",
                session_id=session_id,
                activity_state=activity_state,
                columns=columns,
            )
        )
        if session_id:
            selectable.append(session_id)

        status_line = _status_line(session)
        if status_line:
            rows.append(
                WatchRow(
                    kind="status",
                    text=f"{status_prefix}{status_line}",
                    session_id=session_id,
                )
            )

        task_line = _task_completion_line(session)
        if task_line:
            rows.append(
                WatchRow(
                    kind="status",
                    text=f"{status_prefix}{task_line}",
                    session_id=session_id,
                )
            )

        for adoption_line in _pending_adoption_lines(session):
            rows.append(
                WatchRow(
                    kind="status",
                    text=f"{status_prefix}{adoption_line}",
                    session_id=session_id,
                )
            )

        if session_id and session_id in expanded:
            detail = detail_cache.get(session_id) if detail_cache else None
            for line in _detail_lines(session, detail, codex_projection_enabled):
                rows.append(WatchRow(kind="detail", text=line, session_id=session_id))

        child_entries: list[tuple[str, object, str]] = []
        for child in same_repo_children.get(session.get("id", ""), []):
            child_entries.append(("session", child, _session_name(child).lower()))
        for child_repo_key in sorted(cross_repo_children.get(session.get("id", ""), {}).keys()):
            child_entries.append(("repo", child_repo_key, _repo_label(child_repo_key).lower()))
        child_entries.sort(key=lambda item: (item[2], item[0]))

        for idx, (entry_kind, entry_payload, _) in enumerate(child_entries):
            entry_is_last = idx == len(child_entries) - 1
            if entry_kind == "session":
                render_session(entry_payload, ancestors_last + [is_last], entry_is_last)
            else:
                render_cross_repo_group(
                    parent_session_id=session.get("id", ""),
                    repo_key=entry_payload,
                    ancestors_last=ancestors_last + [is_last],
                    is_last=entry_is_last,
                )

    def render_cross_repo_group(
        parent_session_id: str,
        repo_key: str,
        ancestors_last: list[bool],
        is_last: bool,
    ):
        tree_prefix = _tree_prefix(ancestors_last, is_last)
        repo_label = _repo_label(repo_key) if repo_key != "unknown" else "unknown/"
        if repo_key not in ("unknown",):
            repo_label = f"{repo_label} ({repo_key})"
        rows.append(WatchRow(kind="repo_ref", text=f"{tree_prefix}{repo_label}"))

        remote_children = cross_repo_children.get(parent_session_id, {}).get(repo_key, [])
        for idx, child in enumerate(remote_children):
            render_session(child, ancestors_last + [is_last], idx == len(remote_children) - 1)

    for repo_key in sorted(groups.keys()):
        top_level_roots = roots_by_repo.get(repo_key, [])
        if not top_level_roots:
            continue

        repo_header = _repo_label(repo_key) if repo_key != "unknown" else "unknown/"
        if repo_key not in ("unknown",):
            repo_header = f"{repo_header} ({repo_key})"
        rows.append(WatchRow(kind="repo", text=repo_header))

        for idx, root in enumerate(top_level_roots):
            render_session(root, [], idx == len(top_level_roots) - 1)

    return rows, selectable, len(groups)



def _restore_status(session: dict) -> str:
    provider = session.get("provider", "claude")
    if provider == "codex-app":
        return "headless"
    return "ready" if session.get("tmux_session") else "no-tmux"


def _restore_retired_age(session: dict) -> str:
    return _age_from_iso(session.get("stopped_at") or session.get("completed_at") or session.get("last_activity"))


def build_restore_rows(
    stopped_sessions: list[dict],
    all_sessions: Optional[list[dict]] = None,
    expanded_session_ids: Optional[set[str]] = None,
    collapsed_session_ids: Optional[set[str]] = None,
    collapsed_repo_keys: Optional[set[str]] = None,
    top_level_only: bool = False,
    sort_mode: str = "retired",
) -> tuple[list[WatchRow], list[str], int]:
    """Build grouped restore-browser rows for stopped sessions."""
    rows: list[WatchRow] = []
    selectable: list[str] = []
    expanded = expanded_session_ids or set()
    collapsed = collapsed_session_ids or set()
    collapsed_repos = collapsed_repo_keys or set()
    stopped_by_id = {session["id"]: session for session in stopped_sessions if session.get("id")}
    sessions_by_id = {
        session["id"]: session
        for session in (all_sessions or stopped_sessions)
        if session.get("id")
    }
    groups: dict[str, list[dict]] = {}
    roots_by_repo: dict[str, list[dict]] = {}
    children_by_parent: dict[str, list[dict]] = {}

    for session in stopped_sessions:
        if session.get("status") != "stopped":
            continue
        session_id = session.get("id")
        if not session_id:
            continue
        key = _repo_key(session.get("working_dir", ""))
        groups.setdefault(key, []).append(session)
        parent_id = session.get("parent_session_id")
        if parent_id and parent_id in stopped_by_id:
            children_by_parent.setdefault(parent_id, []).append(session)
        else:
            roots_by_repo.setdefault(key, []).append(session)

    def _timestamp_sort_value(ts: Optional[str]) -> float:
        parsed = _parse_iso(ts)
        if not parsed:
            return float("-inf")
        try:
            return parsed.timestamp()
        except Exception:
            return float("-inf")

    def restore_sort_key(session: dict):
        name_key = (_session_name(session).lower(), session.get("id", ""))
        if sort_mode == "last-active":
            return (-_timestamp_sort_value(session.get("last_activity")), *name_key)
        if sort_mode == "retired":
            return (-_timestamp_sort_value(session.get("stopped_at") or session.get("completed_at") or session.get("last_activity")), *name_key)
        return name_key

    for root_list in roots_by_repo.values():
        root_list.sort(key=restore_sort_key)
    for child_list in children_by_parent.values():
        child_list.sort(key=restore_sort_key)

    def _tree_prefix(ancestors_last: list[bool], is_last: bool) -> str:
        connector = "`-" if is_last else "|-"
        prefix_parts = []
        for ancestor_is_last in ancestors_last:
            prefix_parts.append("   " if ancestor_is_last else "|  ")
        return "".join(prefix_parts) + connector

    def render_session(session: dict, ancestors_last: list[bool], is_last: bool):
        tree_prefix = _tree_prefix(ancestors_last, is_last)
        session_id = session.get("id")
        repo_label = _repo_label(session.get("working_dir") or "")
        columns = {
            "Session": f"{tree_prefix}{_session_name(session)}",
            "ID": session_id or "",
            "Parent": _parent_label(session, sessions_by_id),
            "Role": session.get("role") or "-",
            "Provider": session.get("provider", "claude"),
            "Repo": repo_label,
            "Last Active": _age_from_iso(session.get("last_activity")),
            "Retired": _restore_retired_age(session),
            "Restore": _restore_status(session),
        }
        rows.append(
            WatchRow(
                kind="session",
                session_id=session_id,
                repo_key=_repo_key(session.get("working_dir", "")),
                activity_state="idle",
                columns=columns,
            )
        )
        if session_id:
            selectable.append(session_id)

        if (top_level_only and session_id not in expanded) or session_id in collapsed:
            return
        children = children_by_parent.get(session_id or "", [])
        for idx, child in enumerate(children):
            render_session(child, ancestors_last + [is_last], idx == len(children) - 1)

    for repo_key in sorted(groups.keys()):
        top_level_roots = roots_by_repo.get(repo_key, [])
        if not top_level_roots:
            continue
        repo_header = _repo_label(repo_key) if repo_key != "unknown" else "unknown/"
        if repo_key not in ("unknown",):
            repo_header = f"{repo_header} ({repo_key})"
        if repo_key in collapsed_repos:
            rows.append(
                WatchRow(
                    kind="repo",
                    text=f"[+] {repo_header} ({len(groups.get(repo_key, []))} hidden)",
                    repo_key=repo_key,
                )
            )
            continue

        rows.append(WatchRow(kind="repo", text=repo_header, repo_key=repo_key))
        for idx, root in enumerate(top_level_roots):
            render_session(root, [], idx == len(top_level_roots) - 1)

    return rows, selectable, len(groups)

def _truncate(text: str, width: int, align: str = "left") -> str:
    if width <= 0:
        return ""
    value = text or ""
    if len(value) > width:
        if width <= 3:
            value = value[:width]
        else:
            value = value[: width - 3] + "..."
    if align == "right":
        return value.rjust(width)
    return value.ljust(width)


def _content_len(text: str) -> int:
    return len(ANSI_RE.sub("", text or ""))


def _compute_static_column_widths(
    content_width: int,
    column_specs: Optional[list[tuple[str, int, int, str]]] = None,
    column_floors: Optional[dict[str, int]] = None,
) -> dict[str, int]:
    specs = column_specs or _COLUMN_SPECS
    floors = column_floors or _COLUMN_FLOORS
    if content_width <= 10:
        return {name: 1 for name, _, _, _ in specs}

    min_widths = {name: minimum for name, minimum, _, _ in specs}
    widths = dict(min_widths)
    sep_total = len(_COLUMN_SEP) * (len(specs) - 1)
    min_total = sum(min_widths.values()) + sep_total

    if content_width >= min_total:
        extra = content_width - min_total
        weighted = [entry for entry in specs if entry[2] > 0]
        total_weight = sum(weight for _, _, weight, _ in weighted) or 1
        for name, _, weight, _ in weighted:
            add = (extra * weight) // total_weight
            widths[name] += add
        used = sum(widths.values()) + sep_total
        remainder = content_width - used
        idx = 0
        while remainder > 0 and weighted:
            name = weighted[idx % len(weighted)][0]
            widths[name] += 1
            remainder -= 1
            idx += 1
        return widths

    deficit = min_total - content_width
    shrink_order = [name for name, _, _, _ in reversed(specs)]
    while deficit > 0:
        changed = False
        for name in shrink_order:
            if widths[name] > floors.get(name, 1) and deficit > 0:
                widths[name] -= 1
                deficit -= 1
                changed = True
        if not changed:
            break

    return widths


def _compute_column_widths(
    content_width: int,
    rows: Optional[list[WatchRow]] = None,
    column_specs: Optional[list[tuple[str, int, int, str]]] = None,
    column_floors: Optional[dict[str, int]] = None,
    dynamic_caps: Optional[dict[str, int]] = None,
) -> dict[str, int]:
    specs = column_specs or _COLUMN_SPECS
    floors = column_floors or _COLUMN_FLOORS
    caps = dynamic_caps or _DYNAMIC_COLUMN_CAPS
    if not rows:
        return _compute_static_column_widths(content_width, specs, floors)
    if content_width <= 10:
        return {name: 1 for name, _, _, _ in specs}

    session_rows = [row for row in rows if row.kind == "session"]
    if not session_rows:
        return _compute_static_column_widths(content_width, specs, floors)

    widths: dict[str, int] = {}
    for name, _, _, _ in specs:
        if name == "ID":
            widths[name] = caps.get("ID", 8)
            continue
        desired = len(name)
        for row in session_rows:
            desired = max(desired, _content_len(row.columns.get(name, "")))
        cap = caps.get(name)
        if cap is not None:
            desired = min(desired, max(len(name), cap))
        widths[name] = max(1, desired)

    sep_total = len(_COLUMN_SEP) * (len(specs) - 1)
    used = sum(widths.values()) + sep_total
    if used <= content_width:
        if "Session" in widths:
            widths["Session"] += content_width - used
        return widths

    deficit = used - content_width
    shrink_order = [name for name, _, _, _ in reversed(specs)]
    while deficit > 0:
        changed = False
        for name in shrink_order:
            if widths[name] > floors.get(name, 1) and deficit > 0:
                widths[name] -= 1
                deficit -= 1
                changed = True
        if not changed:
            break

    return widths


def _header_line(
    widths: dict[str, int],
    column_specs: Optional[list[tuple[str, int, int, str]]] = None,
) -> str:
    parts = []
    for name, _, _, align in (column_specs or _COLUMN_SPECS):
        parts.append(_truncate(name, widths[name], align=align))
    return _COLUMN_SEP.join(parts)


def _session_line(
    row: WatchRow,
    widths: dict[str, int],
    column_specs: Optional[list[tuple[str, int, int, str]]] = None,
) -> str:
    parts = []
    for name, _, _, align in (column_specs or _COLUMN_SPECS):
        parts.append(_truncate(row.columns.get(name, ""), widths[name], align=align))
    return _COLUMN_SEP.join(parts)

def _prompt_input(stdscr, prompt: str) -> str:
    height, width = stdscr.getmaxyx()
    curses.echo()
    curses.curs_set(1)
    stdscr.nodelay(False)
    stdscr.move(height - 1, 0)
    stdscr.clrtoeol()
    stdscr.addnstr(height - 1, 0, prompt, max(0, width - 1))
    stdscr.refresh()
    max_len = max(1, width - len(prompt) - 1)
    raw = stdscr.getstr(height - 1, min(len(prompt), max(0, width - 1)), max_len)
    text = raw.decode("utf-8", errors="ignore").strip() if raw else ""
    curses.noecho()
    curses.curs_set(0)
    stdscr.nodelay(True)
    return text


def _default_create_working_dir(
    selected_session: Optional[dict],
    repo_filter: Optional[str],
) -> str:
    """Choose the default working directory for watch-side session creation."""
    candidate: Optional[str] = None
    if selected_session:
        working_dir = selected_session.get("working_dir")
        if working_dir:
            candidate = str(working_dir)
    if candidate is None and repo_filter:
        candidate = str(repo_filter)
    if candidate is None:
        candidate = os.getcwd()

    normalized, _ = _normalize_create_working_dir(candidate)
    return normalized or candidate


def _normalize_create_working_dir(working_dir: str) -> tuple[Optional[str], Optional[str]]:
    """Resolve a watch create path into a validated absolute directory."""
    raw_value = (working_dir or "").strip()
    if not raw_value:
        return None, "Working dir is required"

    try:
        candidate = Path(raw_value).expanduser()
        if not candidate.is_absolute():
            candidate = (Path.cwd() / candidate).resolve(strict=False)
        else:
            candidate = candidate.resolve(strict=False)
    except Exception as exc:
        return None, f"Invalid working dir: {exc}"

    if not candidate.exists():
        return None, f"Working dir does not exist: {candidate}"
    if not candidate.is_dir():
        return None, f"Working dir is not a directory: {candidate}"
    return str(candidate), None


def _resolve_create_provider(choice: str) -> Optional[str]:
    """Map watch create prompt input to a concrete provider."""
    normalized = (choice or "").strip().lower()
    if normalized in ("", "codex", "codex-fork", "co", "2"):
        return "codex-fork"
    if normalized in ("claude", "cl", "1"):
        return "claude"
    return None


def _create_watch_session(
    client,
    provider: str,
    working_dir: str,
) -> tuple[Optional[dict], Optional[str], Optional[str]]:
    """Create a session from sm watch and return its attach target."""
    result = client.create_session_result(
        working_dir,
        provider=provider,
        parent_session_id=getattr(client, "session_id", None),
    )
    if result.get("unavailable"):
        return None, None, "Session manager unavailable"
    if not result.get("ok"):
        return None, None, result.get("detail") or "Failed to create session"

    session = result.get("data")
    if not isinstance(session, dict):
        return None, None, "Failed to create session"

    session_id = session.get("id")
    descriptor = client.get_attach_descriptor(session_id) if session_id else None
    if descriptor and not descriptor.get("attach_supported", True):
        message = descriptor.get("message") or "Attach not supported for this session"
        return session, None, message

    tmux_session = (descriptor or {}).get("tmux_session") or session.get("tmux_session")
    if not tmux_session:
        return session, None, "Created session has no tmux target"
    if descriptor and descriptor.get("tmux_socket_name"):
        session["tmux_socket_name"] = descriptor.get("tmux_socket_name")

    return session, tmux_session, None


def _fork_watch_session(
    client,
    session_id: str,
) -> tuple[Optional[dict], Optional[dict], Optional[str]]:
    """Fork a session from sm watch and return (source, fork, error)."""
    fork_result = client.fork_session_result(
        session_id,
        requester_session_id=getattr(client, "session_id", None),
    )
    if fork_result.get("unavailable"):
        return None, None, "Session manager unavailable"
    if not fork_result.get("ok"):
        return None, None, fork_result.get("detail") or "Failed to fork session"

    data = fork_result.get("data") or {}
    source = data.get("source_session")
    fork_session = data.get("fork_session")
    if not isinstance(source, dict) or not isinstance(fork_session, dict):
        return None, None, "Failed to fork session"
    return source, fork_session, None


def _resolve_tmux_attach_target(
    client,
    session: dict,
) -> tuple[Optional[str], Optional[str], Optional[str]]:
    """Resolve tmux target/socket from the attach descriptor when available."""
    session_id = session.get("id")
    descriptor = None
    descriptor_getter = getattr(client, "get_attach_descriptor", None)
    if session_id and callable(descriptor_getter):
        descriptor = descriptor_getter(session_id)

    if descriptor and not descriptor.get("attach_supported", True):
        message = descriptor.get("message") or "Attach not supported for this session"
        return None, None, message

    tmux_session = (descriptor or {}).get("tmux_session") or session.get("tmux_session")
    if not tmux_session:
        return None, None, "Selected session has no tmux target"

    tmux_socket_name = (descriptor or {}).get("tmux_socket_name") or session.get("tmux_socket_name")
    if descriptor:
        session["tmux_session"] = tmux_session
        if tmux_socket_name:
            session["tmux_socket_name"] = tmux_socket_name

    return tmux_session, tmux_socket_name, None


def _tmux_attach_command(tmux_session: str, tmux_socket_name: str | None = None) -> list[str]:
    cmd = ["tmux"]
    if tmux_socket_name:
        cmd.extend(["-L", tmux_socket_name])
    cmd.extend(["attach-session", "-t", tmux_session])
    return cmd


def _attach_tmux(stdscr, tmux_session: str, tmux_socket_name: str | None = None):
    curses.def_prog_mode()
    curses.endwin()
    try:
        subprocess.run(_tmux_attach_command(tmux_session, tmux_socket_name), check=False)
    finally:
        curses.reset_prog_mode()
        curses.curs_set(0)
        stdscr.nodelay(True)
        stdscr.clear()


def _init_colors() -> dict[str, int]:
    palette = {
        "header": 0,
        "repo": 0,
        "working": 0,
        "thinking": 0,
        "waiting": 0,
        "idle": 0,
        "stopped": 0,
        "flash_success": 0,
        "flash_warn": 0,
        "flash_error": 0,
    }

    if not curses.has_colors():
        return palette

    try:
        curses.start_color()
        curses.use_default_colors()
        curses.init_pair(1, curses.COLOR_CYAN, -1)
        curses.init_pair(2, curses.COLOR_GREEN, -1)
        curses.init_pair(3, curses.COLOR_YELLOW, -1)
        curses.init_pair(4, curses.COLOR_RED, -1)
        curses.init_pair(5, curses.COLOR_WHITE, -1)

        palette["header"] = curses.color_pair(1)
        palette["repo"] = curses.color_pair(1)
        palette["working"] = curses.color_pair(2)
        palette["thinking"] = curses.color_pair(3)
        palette["waiting"] = curses.color_pair(4)
        palette["idle"] = curses.color_pair(5)
        palette["stopped"] = curses.color_pair(4)
        palette["flash_success"] = curses.color_pair(2)
        palette["flash_warn"] = curses.color_pair(3)
        palette["flash_error"] = curses.color_pair(4)
    except curses.error:
        return {k: 0 for k in palette}

    return palette


def _activity_attr(activity_state: str, palette: dict[str, int]) -> int:
    if activity_state == "working":
        return palette["working"]
    if activity_state == "thinking":
        return palette["thinking"]
    if activity_state == "waiting_permission":
        return palette["waiting"]
    if activity_state == "stopped":
        return palette["stopped"]
    if activity_state == "idle":
        return palette["idle"]
    return curses.A_NORMAL


def _flash_attr(flash_message: str, palette: dict[str, int]) -> int:
    lowered = flash_message.lower()
    if "failed" in lowered or "error" in lowered:
        return palette["flash_error"] | curses.A_BOLD
    if "canceled" in lowered or "cancel" in lowered or "warning" in lowered:
        return palette["flash_warn"] | curses.A_BOLD
    return palette["flash_success"] | curses.A_BOLD


def _render_columns(total_width: int, start_col: int, reserve_last_cell: bool = False) -> int:
    """Return the number of visible columns available from start_col."""
    usable = total_width - start_col
    if reserve_last_cell:
        usable -= 1
    return max(0, usable)


def _render(
    stdscr,
    rows: list[WatchRow],
    selected_session_id: Optional[str],
    scroll_offset: int,
    total_sessions: int,
    repo_count: int,
    filter_text: Optional[str],
    flash_message: Optional[str],
    palette: dict[str, int],
    restore_mode: bool = False,
    restore_sort: str = "retired",
):
    stdscr.erase()
    height, width = stdscr.getmaxyx()

    title = f"sm watch{' --restore' if restore_mode else ''}  {total_sessions} agents - {repo_count} repos"
    if filter_text:
        title += f" - filter: {filter_text}"
    stdscr.addnstr(0, 0, title, _render_columns(width, 0), curses.A_BOLD | palette["header"])

    content_width = max(1, _render_columns(width, 2))
    column_specs = _RESTORE_COLUMN_SPECS if restore_mode else _COLUMN_SPECS
    column_floors = _RESTORE_COLUMN_FLOORS if restore_mode else _COLUMN_FLOORS
    dynamic_caps = _RESTORE_DYNAMIC_COLUMN_CAPS if restore_mode else _DYNAMIC_COLUMN_CAPS
    widths = _compute_column_widths(content_width, rows, column_specs, column_floors, dynamic_caps)
    stdscr.addnstr(1, 2, _header_line(widths, column_specs), _render_columns(width, 2), curses.A_BOLD | palette["header"])

    max_rows = max(0, height - _RESERVED_SCREEN_ROWS)
    display_rows = rows[scroll_offset: scroll_offset + max_rows]

    y = 2
    for row in display_rows:
        if y >= height - 2:
            break

        if row.kind == "session":
            is_selected = row.session_id == selected_session_id
            marker = ">" if is_selected else " "
            base_attr = _activity_attr(row.activity_state, palette)
            attr = base_attr
            if is_selected:
                attr = base_attr | curses.A_REVERSE | curses.A_BOLD
            stdscr.addnstr(y, 0, f"{marker} {_session_line(row, widths, column_specs)}", _render_columns(width, 0), attr)
        elif row.kind == "repo":
            stdscr.addnstr(y, 2, row.text, _render_columns(width, 2), curses.A_BOLD | palette["repo"])
        elif row.kind == "repo_ref":
            stdscr.addnstr(y, 4, row.text, _render_columns(width, 4), curses.A_BOLD)
        elif row.kind == "status":
            stdscr.addnstr(y, 4, row.text, _render_columns(width, 4), curses.A_NORMAL)
        else:
            stdscr.addnstr(y, 4, row.text, _render_columns(width, 4), curses.A_NORMAL)

        y += 1

    if flash_message and height >= 2:
        stdscr.addnstr(
            height - 2,
            0,
            flash_message,
            _render_columns(width, 0),
            _flash_attr(flash_message, palette),
        )

    if restore_mode:
        footer = f"j/k: move  Enter: restore+attach  o: sort={restore_sort}  R: hide repo  U: show repos  Tab: expand/collapse  E: expand all  C: collapse all  /: search  r: refresh  q: quit"
    else:
        footer = "j/k: move  +: create  F: fork  Enter: attach  s: send  K,K: retire  n: rename  A/X: adopt  Tab: details  /: filter  r: refresh  q: quit"
    stdscr.addnstr(height - 1, 0, footer, _render_columns(width, 0, reserve_last_cell=True))
    stdscr.refresh()


def run_watch_tui(
    client,
    repo_filter: Optional[str] = None,
    role_filter: Optional[str] = None,
    interval: float = 2.0,
    restore_mode: bool = False,
    top_level: bool = False,
    restore_sort: str = "retired",
) -> int:
    """Run the sm watch curses UI."""

    def _loop(stdscr):
        curses.curs_set(0)
        stdscr.nodelay(True)
        stdscr.keypad(True)
        palette = _init_colors()

        rollout_flags = client.get_rollout_flags()
        codex_projection_enabled = bool(
            rollout_flags and rollout_flags.get("enable_observability_projection", True)
        )

        detail_worker = None if restore_mode else DetailFetchWorker(client=client, codex_projection_enabled=codex_projection_enabled)

        try:
            selected_session_id: Optional[str] = None
            text_filter: Optional[str] = None
            flash_message: Optional[str] = None
            flash_until = 0.0
            rows: list[WatchRow] = []
            selectable: list[str] = []
            latest_sessions: list[dict] = []
            latest_by_id: dict[str, dict] = {}
            expanded_session_ids: set[str] = set()
            collapsed_session_ids: set[str] = set()
            collapsed_repo_keys: set[str] = set()
            restore_tree_collapsed = top_level
            restore_sort_mode = restore_sort
            repo_count = 0
            total_sessions = 0
            spinner_index = 0
            scroll_offset = 0
            next_refresh = 0.0
            retire_confirmation: Optional[RetireConfirmation] = None

            while True:
                now = time.monotonic()
                if now >= next_refresh:
                    listed = client.list_sessions(include_stopped=restore_mode)
                    if listed is None:
                        flash_message = "Session manager unavailable"
                        flash_until = now + 2.5
                    else:
                        filtered = filter_sessions(
                            listed,
                            repo_filter=repo_filter,
                            role_filter=role_filter,
                            text_filter=text_filter,
                        )
                        if restore_mode:
                            latest_sessions = [s for s in filtered if s.get("status") == "stopped"]
                            latest_by_id = {s.get("id", ""): s for s in latest_sessions if s.get("id")}
                            total_sessions = len(latest_sessions)
                            rows, selectable, repo_count = build_restore_rows(
                                latest_sessions,
                                all_sessions=listed,
                                expanded_session_ids=expanded_session_ids,
                                collapsed_session_ids=collapsed_session_ids,
                                collapsed_repo_keys=collapsed_repo_keys,
                                top_level_only=restore_tree_collapsed,
                                sort_mode=restore_sort_mode,
                            )
                        else:
                            latest_sessions = filtered
                            latest_by_id = {s.get("id", ""): s for s in latest_sessions if s.get("id")}
                            total_sessions = len(latest_sessions)
                            detail_cache = {
                                sid: detail_worker.get(sid)
                                for sid in expanded_session_ids
                                if sid in latest_by_id
                            } if detail_worker else {}
                            rows, selectable, repo_count = build_watch_rows(
                                latest_sessions,
                                spinner_index=spinner_index,
                                expanded_session_ids=expanded_session_ids,
                                detail_cache=detail_cache,
                                codex_projection_enabled=codex_projection_enabled,
                            )
                            spinner_index += 1
                            # Prune stale expanded IDs and enqueue refresh for active details.
                            expanded_session_ids.intersection_update(set(selectable))
                            for sid in expanded_session_ids:
                                session = latest_by_id.get(sid)
                                if session and detail_worker:
                                    detail_worker.request(session)
                        if selected_session_id not in selectable:
                            selected_session_id = selectable[0] if selectable else None

                    next_refresh = now + max(0.2, interval)

                if flash_message and now >= flash_until:
                    flash_message = None
                if retire_confirmation and now > retire_confirmation.expires_at:
                    retire_confirmation = None

                max_rows = max(0, stdscr.getmaxyx()[0] - _RESERVED_SCREEN_ROWS)
                selected_row_idx = None
                if selected_session_id:
                    for idx, row in enumerate(rows):
                        if row.kind == "session" and row.session_id == selected_session_id:
                            selected_row_idx = idx
                            break
                if selected_row_idx is not None and max_rows > 0:
                    if selected_row_idx < scroll_offset:
                        scroll_offset = selected_row_idx
                    elif selected_row_idx >= scroll_offset + max_rows:
                        scroll_offset = selected_row_idx - max_rows + 1
                    max_offset = max(0, len(rows) - max_rows)
                    scroll_offset = max(0, min(scroll_offset, max_offset))
                else:
                    scroll_offset = 0

                _render(
                    stdscr,
                    rows=rows,
                    selected_session_id=selected_session_id,
                    scroll_offset=scroll_offset,
                    total_sessions=total_sessions,
                    repo_count=repo_count,
                    filter_text=text_filter,
                    flash_message=flash_message,
                    palette=palette,
                    restore_mode=restore_mode,
                    restore_sort=restore_sort_mode,
                )

                key = stdscr.getch()
                if key == -1:
                    time.sleep(0.05)
                    continue

                if key in (ord("q"), 27):
                    break

                if key != ord("K"):
                    retire_confirmation = None

                if key in (ord("j"), curses.KEY_DOWN):
                    if selectable:
                        current_idx = selectable.index(selected_session_id) if selected_session_id in selectable else 0
                        selected_session_id = selectable[min(current_idx + 1, len(selectable) - 1)]
                    continue

                if key in (ord("k"), curses.KEY_UP):
                    if selectable:
                        current_idx = selectable.index(selected_session_id) if selected_session_id in selectable else 0
                        selected_session_id = selectable[max(current_idx - 1, 0)]
                    continue

                if key in (ord("r"),):
                    if detail_worker:
                        for sid in expanded_session_ids:
                            session = latest_by_id.get(sid)
                            if session:
                                detail_worker.request(session)
                    next_refresh = 0.0
                    continue

                selected = None
                if selected_session_id:
                    selected = latest_by_id.get(selected_session_id)

                if key in (ord("/"),):
                    entered = _prompt_input(stdscr, "filter (blank=clear): ")
                    text_filter = entered or None
                    next_refresh = 0.0
                    continue

                if key == 9:  # Tab
                    if not selected_session_id:
                        continue
                    if restore_mode:
                        if restore_tree_collapsed:
                            if selected_session_id in expanded_session_ids:
                                expanded_session_ids.remove(selected_session_id)
                            else:
                                expanded_session_ids.add(selected_session_id)
                        else:
                            if selected_session_id in collapsed_session_ids:
                                collapsed_session_ids.remove(selected_session_id)
                            else:
                                collapsed_session_ids.add(selected_session_id)
                        next_refresh = 0.0
                        continue
                    if selected_session_id in expanded_session_ids:
                        expanded_session_ids.remove(selected_session_id)
                    else:
                        expanded_session_ids.add(selected_session_id)
                        if selected and detail_worker:
                            detail_worker.request(selected)
                    next_refresh = 0.0
                    continue

                if restore_mode and key in (ord("C"),):
                    restore_tree_collapsed = True
                    expanded_session_ids.clear()
                    collapsed_session_ids.clear()
                    next_refresh = 0.0
                    continue

                if restore_mode and key in (ord("E"),):
                    restore_tree_collapsed = False
                    expanded_session_ids.clear()
                    collapsed_session_ids.clear()
                    next_refresh = 0.0
                    continue

                if restore_mode and key in (ord("o"),):
                    order = ["retired", "last-active", "name"]
                    current_idx = order.index(restore_sort_mode) if restore_sort_mode in order else 0
                    restore_sort_mode = order[(current_idx + 1) % len(order)]
                    next_refresh = 0.0
                    continue

                if restore_mode and key in (ord("R"),):
                    if selected:
                        collapsed_repo_keys.add(_repo_key(selected.get("working_dir", "")))
                        selected_session_id = None
                        next_refresh = 0.0
                    continue

                if restore_mode and key in (ord("U"),):
                    collapsed_repo_keys.clear()
                    next_refresh = 0.0
                    continue

                if restore_mode and key in (ord("s"), ord("+"), ord("F"), ord("K"), ord("n"), ord("A"), ord("X")):
                    flash_message = "Not available in restore mode"
                    flash_until = time.monotonic() + 2.0
                    continue

                if key in (ord("s"),):
                    if not selected_session_id:
                        flash_message = "No session selected"
                        flash_until = time.monotonic() + 2.0
                        continue
                    message = _prompt_input(stdscr, "send> ")
                    if not message:
                        flash_message = "Send canceled"
                        flash_until = time.monotonic() + 2.0
                        continue
                    success, unavailable = client.send_input(
                        selected_session_id,
                        message,
                        sender_session_id=client.session_id,
                        delivery_mode="sequential",
                        from_sm_send=True,
                    )
                    if success:
                        flash_message = f"Sent to {selected_session_id}"
                    elif unavailable:
                        flash_message = "Session manager unavailable"
                    else:
                        flash_message = "Failed to send"
                    flash_until = time.monotonic() + 2.5
                    next_refresh = 0.0
                    continue

                if key in (ord("F"),):
                    if not selected_session_id:
                        flash_message = "No session selected"
                        flash_until = time.monotonic() + 2.0
                        continue
                    source, fork_session, error = _fork_watch_session(client, selected_session_id)
                    if error:
                        flash_message = error
                        flash_until = time.monotonic() + 2.5
                        next_refresh = 0.0
                        continue
                    fork_session_id = fork_session.get("id") if fork_session else None
                    if not fork_session_id:
                        flash_message = "Failed to fork session"
                        flash_until = time.monotonic() + 2.5
                        next_refresh = 0.0
                        continue
                    source_name = (source or {}).get("friendly_name") or (source or {}).get("name") or selected_session_id
                    fork_name = fork_session.get("friendly_name") or fork_session.get("name") or fork_session_id
                    selected_session_id = fork_session_id
                    flash_message = f"Forked {source_name} ({source.get('id') if source else ''}) -> {fork_name} ({fork_session_id})"
                    flash_until = time.monotonic() + 2.5
                    next_refresh = 0.0
                    continue

                if key in (ord("+"),):
                    provider_choice = _prompt_input(
                        stdscr,
                        "provider [codex/claude] (blank=codex, cancel=cancel): ",
                    )
                    if provider_choice.strip().lower() in ("cancel", "q", "quit"):
                        flash_message = "Create canceled"
                        flash_until = time.monotonic() + 2.0
                        continue

                    provider = _resolve_create_provider(provider_choice)
                    if provider is None:
                        flash_message = "Invalid provider (use codex or claude)"
                        flash_until = time.monotonic() + 2.5
                        continue

                    default_dir = _default_create_working_dir(selected, repo_filter)
                    working_dir_prompt = f"working dir (blank={default_dir}, cancel=cancel): "
                    working_dir_input = _prompt_input(stdscr, working_dir_prompt)
                    if working_dir_input.strip().lower() in ("cancel", "q", "quit"):
                        flash_message = "Create canceled"
                        flash_until = time.monotonic() + 2.0
                        continue

                    working_dir = working_dir_input or default_dir
                    normalized_working_dir, working_dir_error = _normalize_create_working_dir(working_dir)
                    if working_dir_error:
                        flash_message = working_dir_error
                        flash_until = time.monotonic() + 2.5
                        continue

                    session, tmux_session, error = _create_watch_session(
                        client,
                        provider=provider,
                        working_dir=normalized_working_dir,
                    )
                    if error:
                        flash_message = error
                        flash_until = time.monotonic() + 2.5
                        next_refresh = 0.0
                        continue

                    selected_session_id = session.get("id") or selected_session_id
                    next_refresh = 0.0
                    _attach_tmux(stdscr, tmux_session, session.get("tmux_socket_name"))
                    flash_message = f"Created {selected_session_id}"
                    flash_until = time.monotonic() + 2.5
                    next_refresh = 0.0
                    continue

                if key in (ord("K"),):
                    if not selected_session_id:
                        retire_confirmation = None
                        flash_message = "No session selected"
                        flash_until = time.monotonic() + 2.0
                        continue
                    now = time.monotonic()
                    if not _retire_confirmation_matches(retire_confirmation, selected_session_id, now):
                        retire_confirmation = _arm_retire_confirmation(selected_session_id, now)
                        flash_message = f"Press K again to retire {selected_session_id}"
                        flash_until = retire_confirmation.expires_at
                        continue
                    retire_confirmation = None
                    result = client.kill_session(None, selected_session_id)
                    if result and result.get("status") == "killed":
                        flash_message = f"Retired {selected_session_id}"
                    elif result is None:
                        flash_message = "Session manager unavailable"
                    elif isinstance(result, dict) and result.get("error"):
                        flash_message = str(result.get("error"))
                    elif isinstance(result, dict) and result.get("detail"):
                        flash_message = str(result.get("detail"))
                    else:
                        flash_message = "Failed to retire session"
                    flash_until = time.monotonic() + 2.5
                    next_refresh = 0.0
                    continue

                if key in (ord("n"),):
                    if not selected_session_id:
                        flash_message = "No session selected"
                        flash_until = time.monotonic() + 2.0
                        continue
                    new_name = _prompt_input(stdscr, "name> ")
                    if not new_name:
                        flash_message = "Rename canceled"
                        flash_until = time.monotonic() + 2.0
                        continue
                    success, unavailable = client.update_friendly_name(selected_session_id, new_name)
                    if success:
                        flash_message = f"Renamed {selected_session_id} -> {new_name}"
                    elif unavailable:
                        flash_message = "Session manager unavailable"
                    else:
                        flash_message = "Failed to rename session"
                    flash_until = time.monotonic() + 2.5
                    next_refresh = 0.0
                    continue

                if key in (ord("A"), ord("X")):
                    if not selected:
                        flash_message = "No session selected"
                        flash_until = time.monotonic() + 2.0
                        continue

                    proposals = [
                        proposal
                        for proposal in (selected.get("pending_adoption_proposals") or [])
                        if proposal.get("status") == "pending"
                    ]
                    if not proposals:
                        flash_message = "No pending adoption proposal"
                        flash_until = time.monotonic() + 2.0
                        continue

                    proposal = proposals[0]
                    proposal_id = proposal.get("id")
                    if not proposal_id:
                        flash_message = "Pending adoption proposal is missing an id"
                        flash_until = time.monotonic() + 2.0
                        continue

                    if key == ord("A"):
                        result = client.accept_adoption_proposal(proposal_id)
                        action = "accepted"
                    else:
                        result = client.reject_adoption_proposal(proposal_id)
                        action = "rejected"

                    if result.get("unavailable"):
                        flash_message = "Session manager unavailable"
                    elif result.get("ok"):
                        flash_message = f"Adoption {action} for {selected_session_id}"
                    else:
                        flash_message = str(result.get("detail") or f"Failed to {action} adoption proposal")
                    flash_until = time.monotonic() + 2.5
                    next_refresh = 0.0
                    continue

                if key in (10, 13, curses.KEY_ENTER):
                    if not selected:
                        flash_message = "No session selected"
                        flash_until = time.monotonic() + 2.0
                        continue
                    if restore_mode:
                        result = client.restore_session_result(selected_session_id)
                        if result.get("unavailable"):
                            flash_message = "Session manager unavailable"
                        elif result.get("ok"):
                            restored = result.get("data") or selected
                            selected_session_id = restored.get("id") or selected_session_id
                            if can_attach_session(restored):
                                tmux_session, tmux_socket_name, attach_error = _resolve_tmux_attach_target(
                                    client,
                                    restored,
                                )
                            else:
                                tmux_session, tmux_socket_name, attach_error = None, None, None
                            if can_attach_session(restored) and tmux_session:
                                _attach_tmux(stdscr, tmux_session, tmux_socket_name)
                                flash_message = f"Restored {selected_session_id}"
                            else:
                                flash_message = attach_error or f"Restored {selected_session_id} (headless)"
                        else:
                            flash_message = str(result.get("detail") or result.get("error") or "Failed to restore session")
                        flash_until = time.monotonic() + 2.5
                        next_refresh = 0.0
                        continue
                    if not can_attach_session(selected):
                        flash_message = "no terminal (use s to send)"
                        flash_until = time.monotonic() + 2.5
                        continue
                    tmux_session, tmux_socket_name, attach_error = _resolve_tmux_attach_target(client, selected)
                    if attach_error:
                        flash_message = attach_error
                        flash_until = time.monotonic() + 2.5
                        continue
                    _attach_tmux(stdscr, tmux_session, tmux_socket_name)
                    next_refresh = 0.0
        finally:
            if detail_worker:
                detail_worker.stop()

    curses.wrapper(_loop)
    return 0
