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

# name, min_width, weight, align
_COLUMN_SPECS = [
    ("Session", 22, 3, "left"),
    ("Role", 8, 1, "left"),
    ("Provider", 10, 1, "left"),
    ("Activity", 11, 1, "left"),
    ("Status", 8, 1, "left"),
    ("Last", 24, 3, "left"),
    ("Age", 6, 0, "right"),
]
_COLUMN_ORDER = [name for name, _, _, _ in _COLUMN_SPECS]
_COLUMN_SEP = "  "


@dataclass
class WatchRow:
    """A render row in sm watch."""

    kind: str  # repo, session, status, detail
    text: str = ""
    session_id: Optional[str] = None
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
    filtered = []
    repo_root = None
    if repo_filter:
        try:
            repo_root = str(Path(repo_filter).expanduser().resolve())
        except Exception:
            repo_root = None
    role_filter_norm = role_filter.lower() if role_filter else None
    text_filter_norm = text_filter.lower() if text_filter else None

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
            haystack = " ".join(
                [
                    _session_name(session),
                    session.get("id", ""),
                    session.get("role", "") or "",
                ]
            ).lower()
            if text_filter_norm not in haystack:
                continue

        filtered.append(session)

    return filtered


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
    groups: dict[str, list[dict]] = {}
    expanded = expanded_session_ids or set()

    for session in sessions:
        key = _repo_key(session.get("working_dir", ""))
        groups.setdefault(key, []).append(session)

    for repo_key in sorted(groups.keys()):
        group_sessions = groups[repo_key]
        repo_header = _repo_label(repo_key) if repo_key != "unknown" else "unknown/"
        if repo_key not in ("unknown",):
            repo_header = f"{repo_header} ({repo_key})"
        rows.append(WatchRow(kind="repo", text=repo_header))

        by_id = {s["id"]: s for s in group_sessions if s.get("id")}
        children: dict[str, list[dict]] = {}
        roots: list[dict] = []

        for session in group_sessions:
            parent_id = session.get("parent_session_id")
            if parent_id and parent_id in by_id:
                children.setdefault(parent_id, []).append(session)
            else:
                roots.append(session)

        sort_key = lambda s: (_session_name(s).lower(), s.get("id", ""))
        roots.sort(key=sort_key)
        for kid_list in children.values():
            kid_list.sort(key=sort_key)

        def walk(session: dict, ancestors_last: list[bool], is_last: bool):
            connector = "`-" if is_last else "|-"
            prefix_parts = []
            for ancestor_is_last in ancestors_last:
                prefix_parts.append("   " if ancestor_is_last else "|  ")
            tree_prefix = "".join(prefix_parts) + connector

            role = session.get("role") or "-"
            provider = session.get("provider", "claude")
            activity_state = session.get("activity_state", "idle")
            status = session.get("status") or "-"

            columns = {
                "Session": f"{tree_prefix}{_session_name(session)} [{session.get('id', '')}]",
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

            status_text = session.get("agent_status_text")
            if status_text:
                rows.append(WatchRow(kind="status", text=f'"{status_text}"', session_id=session_id))

            if session_id and session_id in expanded:
                detail = detail_cache.get(session_id) if detail_cache else None
                for line in _detail_lines(session, detail, codex_projection_enabled):
                    rows.append(WatchRow(kind="detail", text=line, session_id=session_id))

            kid_sessions = children.get(session.get("id", ""), [])
            for idx, child in enumerate(kid_sessions):
                walk(child, ancestors_last + [is_last], idx == len(kid_sessions) - 1)

        for idx, root in enumerate(roots):
            walk(root, [], idx == len(roots) - 1)

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


def _compute_column_widths(content_width: int) -> dict[str, int]:
    if content_width <= 10:
        return {name: 1 for name, _, _, _ in _COLUMN_SPECS}

    min_widths = {name: minimum for name, minimum, _, _ in _COLUMN_SPECS}
    widths = dict(min_widths)
    sep_total = len(_COLUMN_SEP) * (len(_COLUMN_SPECS) - 1)
    min_total = sum(min_widths.values()) + sep_total

    if content_width >= min_total:
        extra = content_width - min_total
        weighted = [entry for entry in _COLUMN_SPECS if entry[2] > 0]
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
    shrink_order = ["Last", "Session", "Activity", "Role", "Provider", "Status", "Age"]
    floor = {"Session": 8, "Last": 8, "Age": 3, "Role": 4, "Provider": 6, "Activity": 7, "Status": 5}
    while deficit > 0:
        changed = False
        for name in shrink_order:
            if widths[name] > floor[name] and deficit > 0:
                widths[name] -= 1
                deficit -= 1
                changed = True
        if not changed:
            break

    return widths


def _header_line(widths: dict[str, int]) -> str:
    parts = []
    for name, _, _, align in _COLUMN_SPECS:
        parts.append(_truncate(name, widths[name], align=align))
    return _COLUMN_SEP.join(parts)


def _session_line(row: WatchRow, widths: dict[str, int]) -> str:
    parts = []
    for name, _, _, align in _COLUMN_SPECS:
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


def _attach_tmux(stdscr, tmux_session: str):
    curses.def_prog_mode()
    curses.endwin()
    try:
        subprocess.run(["tmux", "attach-session", "-t", tmux_session], check=False)
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
        return palette["stopped"] | curses.A_DIM
    if activity_state == "idle":
        return palette["idle"] | curses.A_DIM
    return curses.A_NORMAL


def _flash_attr(flash_message: str, palette: dict[str, int]) -> int:
    lowered = flash_message.lower()
    if "failed" in lowered or "error" in lowered:
        return palette["flash_error"] | curses.A_BOLD
    if "canceled" in lowered or "cancel" in lowered or "warning" in lowered:
        return palette["flash_warn"] | curses.A_BOLD
    return palette["flash_success"] | curses.A_BOLD


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
):
    stdscr.erase()
    height, width = stdscr.getmaxyx()

    title = f"sm watch  {total_sessions} agents - {repo_count} repos"
    if filter_text:
        title += f" - filter: {filter_text}"
    stdscr.addnstr(0, 0, title, max(0, width - 1), curses.A_BOLD | palette["header"])

    content_width = max(1, width - 2)
    widths = _compute_column_widths(content_width)
    stdscr.addnstr(1, 2, _header_line(widths), max(0, width - 3), curses.A_BOLD | palette["header"])

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
            stdscr.addnstr(y, 0, f"{marker} {_session_line(row, widths)}", max(0, width - 1), attr)
        elif row.kind == "repo":
            stdscr.addnstr(y, 2, row.text, max(0, width - 3), curses.A_BOLD | palette["repo"])
        elif row.kind == "status":
            stdscr.addnstr(y, 4, row.text, max(0, width - 5), curses.A_DIM)
        else:
            stdscr.addnstr(y, 4, row.text, max(0, width - 5), curses.A_DIM)

        y += 1

    if flash_message and height >= 2:
        stdscr.addnstr(
            height - 2,
            0,
            flash_message,
            max(0, width - 1),
            _flash_attr(flash_message, palette),
        )

    footer = "j/k: move  Enter: attach  s: send  K: kill  n: rename  Tab: details  /: filter  r: refresh  q: quit"
    stdscr.addnstr(height - 1, 0, footer, max(0, width - 1))
    stdscr.refresh()


def run_watch_tui(
    client,
    repo_filter: Optional[str] = None,
    role_filter: Optional[str] = None,
    interval: float = 2.0,
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

        detail_worker = DetailFetchWorker(client=client, codex_projection_enabled=codex_projection_enabled)

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
            repo_count = 0
            total_sessions = 0
            spinner_index = 0
            scroll_offset = 0
            next_refresh = 0.0

            while True:
                now = time.monotonic()
                if now >= next_refresh:
                    listed = client.list_sessions()
                    if listed is None:
                        flash_message = "Session manager unavailable"
                        flash_until = now + 2.5
                    else:
                        latest_sessions = filter_sessions(
                            listed,
                            repo_filter=repo_filter,
                            role_filter=role_filter,
                            text_filter=text_filter,
                        )
                        latest_by_id = {s.get("id", ""): s for s in latest_sessions if s.get("id")}
                        total_sessions = len(latest_sessions)
                        detail_cache = {
                            sid: detail_worker.get(sid)
                            for sid in expanded_session_ids
                            if sid in latest_by_id
                        }
                        rows, selectable, repo_count = build_watch_rows(
                            latest_sessions,
                            spinner_index=spinner_index,
                            expanded_session_ids=expanded_session_ids,
                            detail_cache=detail_cache,
                            codex_projection_enabled=codex_projection_enabled,
                        )
                        spinner_index += 1
                        if selected_session_id not in selectable:
                            selected_session_id = selectable[0] if selectable else None
                        # Prune stale expanded IDs and enqueue refresh for active details.
                        expanded_session_ids.intersection_update(set(selectable))
                        for sid in expanded_session_ids:
                            session = latest_by_id.get(sid)
                            if session:
                                detail_worker.request(session)

                    next_refresh = now + max(0.2, interval)

                if flash_message and now >= flash_until:
                    flash_message = None

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
                )

                key = stdscr.getch()
                if key == -1:
                    time.sleep(0.05)
                    continue

                if key in (ord("q"), 27):
                    break

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
                    if selected_session_id in expanded_session_ids:
                        expanded_session_ids.remove(selected_session_id)
                    else:
                        expanded_session_ids.add(selected_session_id)
                        if selected:
                            detail_worker.request(selected)
                    next_refresh = 0.0
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

                if key in (ord("K"),):
                    if not selected_session_id:
                        flash_message = "No session selected"
                        flash_until = time.monotonic() + 2.0
                        continue
                    confirm = _prompt_input(stdscr, f"Kill {selected_session_id}? type yes: ")
                    if confirm.lower() != "yes":
                        flash_message = "Kill canceled"
                        flash_until = time.monotonic() + 2.0
                        continue
                    result = client.kill_session(None, selected_session_id)
                    if result and result.get("status") == "killed":
                        flash_message = f"Killed {selected_session_id}"
                    elif result is None:
                        flash_message = "Session manager unavailable"
                    else:
                        flash_message = "Failed to kill session"
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

                if key in (10, 13, curses.KEY_ENTER):
                    if not selected:
                        flash_message = "No session selected"
                        flash_until = time.monotonic() + 2.0
                        continue
                    if not can_attach_session(selected):
                        flash_message = "no terminal (use s to send)"
                        flash_until = time.monotonic() + 2.5
                        continue
                    tmux_session = selected.get("tmux_session")
                    if not tmux_session:
                        flash_message = "Selected session has no tmux target"
                        flash_until = time.monotonic() + 2.5
                        continue
                    _attach_tmux(stdscr, tmux_session)
                    next_refresh = 0.0
        finally:
            detail_worker.stop()

    curses.wrapper(_loop)
    return 0
