"""Curses dashboard for sm watch (#289)."""

from __future__ import annotations

import curses
import os
import subprocess
import time
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Optional

SPINNER_FRAMES = ["|", "/", "-", "\\"]


@dataclass
class WatchRow:
    """A render row in sm watch."""
    kind: str  # repo, session, status
    text: str
    session_id: Optional[str] = None


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
    try:
        return datetime.fromisoformat(ts)
    except Exception:
        return None


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


def build_watch_rows(sessions: list[dict], spinner_index: int = 0) -> tuple[list[WatchRow], list[str], int]:
    """Build grouped tree rows and selectable session IDs."""
    rows: list[WatchRow] = []
    selectable: list[str] = []
    groups: dict[str, list[dict]] = {}

    for session in sessions:
        key = _repo_key(session.get("working_dir", ""))
        groups.setdefault(key, []).append(session)

    for repo_key in sorted(groups.keys()):
        group_sessions = groups[repo_key]
        repo_header = _repo_label(repo_key) if repo_key != "unknown" else "unknown/"
        if repo_key not in ("unknown",):
            repo_header = f"{repo_header} ({repo_key})"
        rows.append(WatchRow(kind="repo", text=repo_header))
        by_id = {s["id"]: s for s in group_sessions}
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
            activity_state = session.get("activity_state", "idle")
            state_label = _state_label(activity_state, spinner_index)
            age = _format_age(session.get("last_activity"), activity_state)
            line = (
                f"{tree_prefix}{_session_name(session)} "
                f"[{session.get('id', '')}] {role:<10} {state_label:<18} {age}"
            )
            session_id = session.get("id")
            rows.append(WatchRow(kind="session", text=line, session_id=session_id))
            if session_id:
                selectable.append(session_id)

            status_text = session.get("agent_status_text")
            if status_text:
                status_indent = " " * (len(tree_prefix) + 1)
                rows.append(WatchRow(kind="status", text=f'{status_indent}"{status_text}"'))

            kid_sessions = children.get(session.get("id", ""), [])
            for idx, child in enumerate(kid_sessions):
                walk(child, ancestors_last + [is_last], idx == len(kid_sessions) - 1)

        for idx, root in enumerate(roots):
            walk(root, [], idx == len(roots) - 1)

    return rows, selectable, len(groups)


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


def _render(
    stdscr,
    rows: list[WatchRow],
    selected_session_id: Optional[str],
    scroll_offset: int,
    total_sessions: int,
    repo_count: int,
    filter_text: Optional[str],
    flash_message: Optional[str],
):
    stdscr.erase()
    height, width = stdscr.getmaxyx()
    header = f"sm watch  {total_sessions} agents - {repo_count} repos"
    if filter_text:
        header += f" - filter: {filter_text}"
    stdscr.addnstr(0, 0, header, max(0, width - 1), curses.A_BOLD)

    max_rows = max(0, height - 3)
    display_rows = rows[scroll_offset:scroll_offset + max_rows]
    y = 1
    for row in display_rows:
        if row.kind == "session":
            is_selected = row.session_id == selected_session_id
            marker = ">" if is_selected else " "
            attr = curses.A_REVERSE if is_selected else curses.A_NORMAL
            stdscr.addnstr(y, 0, f"{marker} {row.text}", max(0, width - 1), attr)
        elif row.kind == "repo":
            stdscr.addnstr(y, 0, f"  {row.text}", max(0, width - 1), curses.A_BOLD)
        else:
            stdscr.addnstr(y, 0, f"  {row.text}", max(0, width - 1))
        y += 1

    if flash_message and height >= 2:
        stdscr.addnstr(height - 2, 0, flash_message, max(0, width - 1), curses.A_BOLD)

    footer = "j/k or arrows: move  Enter: attach  s: send  K: kill child  /: filter  r: refresh  q: quit"
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

        selected_session_id: Optional[str] = None
        text_filter: Optional[str] = None
        flash_message: Optional[str] = None
        flash_until = 0.0
        rows: list[WatchRow] = []
        selectable: list[str] = []
        latest_sessions: list[dict] = []
        repo_count = 0
        total_sessions = 0
        spinner_index = 0
        scroll_offset = 0
        next_refresh = 0.0

        while True:
            now = time.monotonic()
            if now >= next_refresh:
                latest_sessions = client.list_sessions() or []
                latest_sessions = filter_sessions(
                    latest_sessions,
                    repo_filter=repo_filter,
                    role_filter=role_filter,
                    text_filter=text_filter,
                )
                total_sessions = len(latest_sessions)
                rows, selectable, repo_count = build_watch_rows(latest_sessions, spinner_index=spinner_index)
                spinner_index += 1
                if selected_session_id not in selectable:
                    selected_session_id = selectable[0] if selectable else None
                next_refresh = now + max(0.2, interval)

            if flash_message and now >= flash_until:
                flash_message = None

            # Keep selected session visible in viewport when row count exceeds screen height.
            max_rows = max(0, stdscr.getmaxyx()[0] - 3)
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
                next_refresh = 0.0
                continue

            selected = None
            if selected_session_id:
                selected = next((s for s in latest_sessions if s.get("id") == selected_session_id), None)

            if key in (ord("/"),):
                entered = _prompt_input(stdscr, "filter (blank=clear): ")
                text_filter = entered or None
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
                if not selected:
                    flash_message = "No session selected"
                    flash_until = time.monotonic() + 2.0
                    continue
                if not selected.get("parent_session_id"):
                    flash_message = "Kill is only allowed for child sessions"
                    flash_until = time.monotonic() + 2.5
                    continue
                if client.session_id and selected.get("parent_session_id") != client.session_id:
                    flash_message = "Kill is only allowed for your child sessions"
                    flash_until = time.monotonic() + 2.5
                    continue
                confirm = _prompt_input(stdscr, f"Kill {selected_session_id}? type yes: ")
                if confirm.lower() != "yes":
                    flash_message = "Kill canceled"
                    flash_until = time.monotonic() + 2.0
                    continue
                result = client.kill_session(client.session_id, selected_session_id)
                if result and result.get("status") == "killed":
                    flash_message = f"Killed {selected_session_id}"
                elif result is None:
                    flash_message = "Session manager unavailable"
                else:
                    flash_message = "Failed to kill session"
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

    curses.wrapper(_loop)
    return 0
