"""Output formatting utilities for CLI."""

from datetime import datetime
from typing import Optional


def format_relative_time(timestamp_str: str) -> str:
    """
    Format timestamp as relative time (e.g., '2min ago', '5min ago').

    Args:
        timestamp_str: ISO format timestamp string

    Returns:
        Relative time string
    """
    try:
        timestamp = datetime.fromisoformat(timestamp_str)
        now = datetime.now()
        delta = now - timestamp

        # Convert to minutes
        minutes = int(delta.total_seconds() / 60)

        if minutes == 0:
            return "just now"
        elif minutes == 1:
            return "1min ago"
        elif minutes < 60:
            return f"{minutes}min ago"
        else:
            hours = minutes // 60
            if hours == 1:
                return "1hr ago"
            elif hours < 24:
                return f"{hours}hr ago"
            else:
                days = hours // 24
                if days == 1:
                    return "1day ago"
                else:
                    return f"{days}days ago"
    except Exception:
        return "unknown"


def format_session_line(
    session: dict,
    show_working_dir: bool = False,
    show_summary: bool = False,
    summary: Optional[str] = None,
    index: Optional[int] = None
) -> str:
    """
    Format a session as a single line.

    Args:
        session: Session dict from API
        show_working_dir: Show working directory instead of relative time
        show_summary: Show summary on next line
        summary: Optional summary text
        index: Optional menu index number

    Returns:
        Formatted session line(s)
    """
    # Get display name (friendly_name or just ID)
    friendly_name = session.get("friendly_name")
    name = session.get("name", session.get("id", "unknown"))
    session_id = session.get("id", "unknown")
    status = session.get("status", "unknown")
    last_activity = session.get("last_activity")

    # Display name: prefer friendly_name, fall back to name
    display_name = friendly_name or name

    # Format last activity
    if last_activity:
        time_str = format_relative_time(last_activity)
    else:
        time_str = "unknown"

    # Build line
    parts = []

    if index is not None:
        parts.append(f"{index}.")

    parts.append(display_name)
    parts.append(f"[{session_id}]")
    parts.append(f"- {status}")
    parts.append(f"- {time_str}")
    node = str(session.get("node") or "primary")
    if node != "primary":
        parts.append(f"- node={node}")

    if show_working_dir:
        working_dir = session.get("working_dir", "")
        if working_dir:
            parts.append(f"({working_dir})")

    # Format main line
    line = " ".join(parts)

    # Add summary if requested
    if show_summary and summary:
        # Indent summary
        summary_lines = summary.split('\n')
        summary_text = '\n'.join(f"  → {line}" for line in summary_lines)
        line = f"{line}\n{summary_text}"

    return line


def format_status_list(sessions: list, current_session_id: str) -> str:
    """
    Format list of sessions for status output.

    Args:
        sessions: List of session dicts
        current_session_id: ID of current session

    Returns:
        Formatted status text
    """
    lines = []

    # Current session
    current = next((s for s in sessions if s["id"] == current_session_id), None)
    if current:
        lines.append("You: " + format_session_line(current, show_working_dir=True))
    else:
        lines.append("You: Session not found")

    # Other sessions in same workspace
    if current:
        working_dir = current["working_dir"]
        others = [
            s for s in sessions
            if s["id"] != current_session_id
            and s["working_dir"] == working_dir
            and s["status"] in ["running", "waiting_permission"]
        ]

        if others:
            lines.append("")
            lines.append("Others in this workspace:")
            for session in others:
                lines.append("  " + format_session_line(session))
        else:
            lines.append("")
            lines.append("Others in this workspace: none")

    return '\n'.join(lines)
