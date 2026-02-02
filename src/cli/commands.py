"""Command implementations for sm CLI."""

import re
import sys
from typing import Optional

from .client import SessionManagerClient
from .formatting import format_session_line, format_relative_time, format_status_list
from ..lock_manager import LockManager


def parse_duration(duration_str: str) -> int:
    """
    Parse a duration string into seconds.

    Supports formats: 30s, 5m, 1h, 2h30m, etc.

    Args:
        duration_str: Duration string (e.g., "5m", "30s", "1h", "2h30m")

    Returns:
        Duration in seconds

    Raises:
        ValueError: If format is invalid
    """
    if not duration_str:
        raise ValueError("Empty duration string")

    # Try pure integer (assume seconds)
    if duration_str.isdigit():
        return int(duration_str)

    total_seconds = 0
    pattern = re.compile(r'(\d+)([smhd])', re.IGNORECASE)
    matches = pattern.findall(duration_str)

    if not matches:
        raise ValueError(f"Invalid duration format: {duration_str}")

    for value, unit in matches:
        value = int(value)
        unit = unit.lower()
        if unit == 's':
            total_seconds += value
        elif unit == 'm':
            total_seconds += value * 60
        elif unit == 'h':
            total_seconds += value * 3600
        elif unit == 'd':
            total_seconds += value * 86400

    return total_seconds


def resolve_session_id(client: SessionManagerClient, identifier: str) -> tuple[Optional[str], Optional[dict]]:
    """
    Resolve a session identifier (ID or friendly name) to session ID and details.

    Args:
        client: API client
        identifier: Session ID or friendly name

    Returns:
        Tuple of (session_id, session_dict) or (None, None) if not found/unavailable
    """
    # Try as session ID first
    session = client.get_session(identifier)
    if session:
        return identifier, session

    # Not found by ID, try as friendly name
    sessions = client.list_sessions()
    if sessions is None:
        return None, None  # Session manager unavailable

    # Search by friendly_name
    for s in sessions:
        if s.get("friendly_name") == identifier:
            return s["id"], s

    return None, None  # Not found


def cmd_name(client: SessionManagerClient, session_id: str, name_or_session: str, new_name: Optional[str] = None) -> int:
    """
    Set friendly name for current session or a child session.

    Args:
        client: API client
        session_id: Current session ID
        name_or_session: Name for self, or session identifier when renaming a child
        new_name: New name when renaming a child session (None when renaming self)

    Exit codes:
        0: Success
        1: Failed to set or not authorized
        2: Session manager unavailable
    """
    # Case 1: Rename self (sm name <name>)
    if new_name is None:
        friendly_name = name_or_session
        success, unavailable = client.update_friendly_name(session_id, friendly_name)

        if success:
            print(f"Name set: {friendly_name} ({session_id})")
            return 0
        elif unavailable:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        else:
            print("Error: Failed to set name", file=sys.stderr)
            return 1

    # Case 2: Rename child session (sm name <session> <name>)
    target_identifier = name_or_session
    friendly_name = new_name

    # Resolve identifier to session ID and get session details
    target_session_id, target_session = resolve_session_id(client, target_identifier)
    if target_session_id is None:
        # Check if it's unavailable or not found
        sessions = client.list_sessions()
        if sessions is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        else:
            print(f"Error: Session '{target_identifier}' not found", file=sys.stderr)
            return 1

    # Check parent-child ownership
    # Only allow renaming if current session is the parent of the target session
    parent_id = target_session.get("parent_session_id")
    if parent_id != session_id:
        print(f"Error: Not authorized. You can only rename your child sessions.", file=sys.stderr)
        print(f"Target session parent: {parent_id or 'none'}", file=sys.stderr)
        return 1

    # Update the child session's friendly name
    success, unavailable = client.update_friendly_name(target_session_id, friendly_name)

    if success:
        print(f"Name set: {friendly_name} ({target_session_id})")
        return 0
    elif unavailable:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2
    else:
        print("Error: Failed to set name", file=sys.stderr)
        return 1


def cmd_me(client: SessionManagerClient, session_id: str) -> int:
    """
    Show current session info.

    Exit codes:
        0: Success
        1: Session manager unavailable or session not found
    """
    session = client.get_session(session_id)

    if session is None:
        print("Error: Session manager unavailable or session not found", file=sys.stderr)
        return 1

    print(format_session_line(session, show_working_dir=True))
    return 0


def cmd_who(client: SessionManagerClient, session_id: str) -> int:
    """
    List other active sessions in the same workspace.

    Exit codes:
        0: Other agents found
        1: No other agents (you're alone)
        2: Session manager unavailable
    """
    # Get current session to find working_dir
    current = client.get_session(session_id)
    if current is None:
        lock_manager = LockManager()
        lock = lock_manager.check_lock()
        if lock and not lock.is_stale():
            print(f"{lock.session_id} | locked | {lock.task}")
            return 0
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    # List all sessions
    sessions = client.list_sessions()
    if sessions is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    # Filter: same working_dir, not current session, active status
    working_dir = current["working_dir"]
    others = [
        s for s in sessions
        if s["id"] != session_id
        and s["working_dir"] == working_dir
        and s["status"] in ["running", "waiting_permission", "idle"]
    ]

    if not others:
        return 1  # Silent exit, no other agents

    for session in others:
        print(format_session_line(session))

    return 0


def cmd_what(client: SessionManagerClient, target_session_id: str, lines: int, deep: bool = False) -> int:
    """
    Get AI-generated summary of what a session is doing.

    Exit codes:
        0: Success
        1: Session not found or summary unavailable
        2: Session manager unavailable
    """
    summary = client.get_summary(target_session_id, lines)

    if summary is None:
        # Check if it's a connection issue
        session = client.get_session(target_session_id)
        if session is None:
            # Could be unavailable or not found
            sessions = client.list_sessions()
            if sessions is None:
                print("Error: Session manager unavailable", file=sys.stderr)
                return 2
            else:
                print("Error: Session not found", file=sys.stderr)
                return 1
        else:
            print("Error: Summary unavailable", file=sys.stderr)
            return 1

    print(summary)

    # If --deep flag is set, show subagent activity
    if deep:
        subagents = client.list_subagents(target_session_id)
        if subagents:
            print()
            print("Subagents:")
            for sa in subagents:
                elapsed = format_relative_time(sa["started_at"])
                status_icon = "✓" if sa["status"] == "completed" else "→"
                print(f"  {status_icon} {sa['agent_type']} ({sa['agent_id'][:6]}) | {sa['status']} | {elapsed}")
                if sa.get("summary"):
                    print(f"     {sa['summary']}")

    return 0


def cmd_others(client: SessionManagerClient, session_id: str, include_repo: bool) -> int:
    """
    List other agents + what they're doing.

    Exit codes:
        0: Other agents found
        1: No other agents
        2: Session manager unavailable
    """
    # Get current session
    current = client.get_session(session_id)
    if current is None:
        lock_manager = LockManager()
        lock = lock_manager.check_lock()
        if lock and not lock.is_stale():
            print(f"{lock.session_id} | locked")
            print(f"  → {lock.task}")
            return 0
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    # List all sessions
    sessions = client.list_sessions()
    if sessions is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    # Filter based on --repo flag
    if include_repo:
        # Match by git remote URL
        git_remote = current.get("git_remote_url")
        if not git_remote:
            # Fall back to working_dir matching
            others = [
                s for s in sessions
                if s["id"] != session_id
                and s["working_dir"] == current["working_dir"]
                and s["status"] in ["running", "waiting_permission", "idle"]
            ]
        else:
            others = [
                s for s in sessions
                if s["id"] != session_id
                and s.get("git_remote_url") == git_remote
                and s["status"] in ["running", "waiting_permission", "idle"]
            ]
    else:
        # Match by working_dir (same workspace)
        working_dir = current["working_dir"]
        others = [
            s for s in sessions
            if s["id"] != session_id
            and s["working_dir"] == working_dir
            and s["status"] in ["running", "waiting_permission", "idle"]
        ]

    if not others:
        return 1  # Silent exit

    # Get summaries for each
    for session in others:
        summary = client.get_summary(session["id"], lines=100)
        print(format_session_line(session, show_summary=True, summary=summary))
        print()  # Blank line between sessions

    return 0


def cmd_all(client: SessionManagerClient, include_summaries: bool) -> int:
    """
    List all sessions system-wide across all workspaces.

    Exit codes:
        0: Sessions found
        1: No sessions
        2: Session manager unavailable
    """
    # List all sessions
    sessions = client.list_sessions()
    if sessions is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    if not sessions:
        print("No active sessions")
        return 1

    # Show sessions with optional summaries
    if include_summaries:
        for session in sessions:
            summary = client.get_summary(session["id"], lines=100)
            print(format_session_line(session, show_summary=True, summary=summary))
            print()  # Blank line between sessions
    else:
        for session in sessions:
            print(format_session_line(session, show_working_dir=True))

    return 0


def cmd_alone(client: SessionManagerClient, session_id: str) -> int:
    """
    Check if you're the only active agent (silent, for scripting).

    Exit codes:
        0: You're alone
        1: Other agents are active
        2: Session manager unavailable (conservative: not alone)
    """
    # Get current session
    current = client.get_session(session_id)
    if current is None:
        # Conservative: treat unavailable as not alone
        lock_manager = LockManager()
        if lock_manager.is_locked():
            return 1
        return 2

    # List all sessions
    sessions = client.list_sessions()
    if sessions is None:
        return 2

    # Check for others in same workspace
    working_dir = current["working_dir"]
    others = [
        s for s in sessions
        if s["id"] != session_id
        and s["working_dir"] == working_dir
        and s["status"] in ["running", "waiting_permission", "idle"]
    ]

    return 0 if not others else 1


def cmd_task(client: SessionManagerClient, session_id: str, description: str) -> int:
    """
    Register what you're currently working on.

    Exit codes:
        0: Success
        1: Failed to register
        2: Session manager unavailable
    """
    success, unavailable = client.update_task(session_id, description)

    if success:
        print(f"Task registered for session {session_id}")
        return 0
    elif unavailable:
        # Fallback to lock file when session manager unavailable
        lock_manager = LockManager()
        if lock_manager.acquire_lock(session_id, description):
            print(f"Task registered in lock file (session manager unavailable)")
            return 0
        else:
            print("Error: Failed to register task", file=sys.stderr)
            return 1
    else:
        # API error (not unavailable, but failed)
        print("Error: Failed to register task", file=sys.stderr)
        return 1


def cmd_lock(session_id: Optional[str], description: str) -> int:
    """
    Acquire workspace lock (file-based fallback).

    Exit codes:
        0: Lock acquired
        1: Lock exists (another agent has it)
    """
    # Use session_id if available, otherwise generate one
    if not session_id:
        import uuid
        session_id = uuid.uuid4().hex[:8]

    lock_manager = LockManager()
    success = lock_manager.acquire_lock(session_id, description)

    if success:
        print(f"Lock written to .claude/workspace.lock")
        return 0
    else:
        lock = lock_manager.check_lock()
        if lock and not lock.is_stale():
            print(f"Error: Lock held by session {lock.session_id}", file=sys.stderr)
            return 1
        else:
            print("Error: Failed to acquire lock", file=sys.stderr)
            return 1


def cmd_unlock(session_id: Optional[str]) -> int:
    """
    Release workspace lock.

    Exit codes:
        0: Success (or no lock existed)
    """
    lock_manager = LockManager()
    lock_manager.release_lock(session_id)
    print("Lock removed")
    return 0


def cmd_status(client: SessionManagerClient, session_id: str) -> int:
    """
    Full status: your session + others + lock file state.

    Exit codes:
        0: Success
        2: Session manager unavailable (will still show lock file status)
    """
    # Get current session
    current = client.get_session(session_id)
    sessions = client.list_sessions()

    if current is None or sessions is None:
        # Session manager unavailable, show lock file only
        print("You: Session manager unavailable")
        print()
        lock_manager = LockManager()
        lock = lock_manager.check_lock()
        if lock:
            if lock.is_stale():
                print(f"Lock file: {lock.session_id} (stale - {lock.task})")
            else:
                print(f"Lock file: {lock.session_id} - {lock.task}")
        else:
            print("Lock file: none")
        return 2

    # Show status
    print(format_status_list(sessions, session_id))
    print()

    # Show lock file status
    lock_manager = LockManager()
    lock = lock_manager.check_lock()
    if lock:
        if lock.is_stale():
            print(f"Lock file: {lock.session_id} (stale - {lock.task})")
        else:
            print(f"Lock file: {lock.session_id} - {lock.task}")
    else:
        print("Lock file: none")

    return 0


def cmd_subagent_start(client: SessionManagerClient, session_id: str) -> int:
    """
    Register subagent start (called by SubagentStart hook).

    Reads JSON payload from stdin with fields:
    - agent_id: Subagent identifier
    - agent_type: Role (engineer, architect, etc.)
    - agent_transcript_path: Path to subagent transcript

    Exit codes:
        0: Success
        1: Failed to register
        2: Session manager unavailable
    """
    import json

    # If no CLAUDE_SESSION_MANAGER_ID, this session isn't managed by us
    # Skip entirely - no need to track subagents for unmanaged sessions
    if not session_id:
        # Still need to consume stdin to avoid broken pipe
        sys.stdin.read()
        return 0

    # Read hook payload from stdin
    try:
        payload = json.loads(sys.stdin.read())
    except Exception as e:
        print(f"Error: Failed to parse hook payload: {e}", file=sys.stderr)
        return 1

    agent_id = payload.get("agent_id")
    agent_type = payload.get("agent_type", payload.get("subagent_type", "unknown"))
    transcript_path = payload.get("agent_transcript_path")

    if not agent_id:
        print("Error: Missing agent_id in hook payload", file=sys.stderr)
        return 1

    # Use OUR session_id from CLAUDE_SESSION_MANAGER_ID, not Claude's internal UUID
    hook_session_id = session_id

    success, unavailable = client.register_subagent_start(
        hook_session_id, agent_id, agent_type, transcript_path
    )

    if success:
        return 0
    elif unavailable:
        # Silent failure when session manager is unavailable
        return 2
    else:
        return 1


def cmd_subagent_stop(client: SessionManagerClient, session_id: str) -> int:
    """
    Register subagent stop (called by SubagentStop hook).

    Reads JSON payload from stdin with fields:
    - agent_id: Subagent identifier
    - agent_transcript_path: Path to subagent transcript (for generating summary)

    Exit codes:
        0: Success
        1: Failed to register
        2: Session manager unavailable
    """
    import json

    # If no CLAUDE_SESSION_MANAGER_ID, this session isn't managed by us
    # Skip entirely - no need to track subagents for unmanaged sessions
    if not session_id:
        # Still need to consume stdin to avoid broken pipe
        sys.stdin.read()
        return 0

    # Read hook payload from stdin
    try:
        payload = json.loads(sys.stdin.read())
    except Exception as e:
        print(f"Error: Failed to parse hook payload: {e}", file=sys.stderr)
        return 1

    agent_id = payload.get("agent_id")
    transcript_path = payload.get("agent_transcript_path")

    if not agent_id:
        print("Error: Missing agent_id in hook payload", file=sys.stderr)
        return 1

    # Use OUR session_id from CLAUDE_SESSION_MANAGER_ID, not Claude's internal UUID
    hook_session_id = session_id

    # TODO: Generate summary from transcript_path using Haiku
    # For now, just register the stop without a summary
    summary = None

    success, unavailable = client.register_subagent_stop(hook_session_id, agent_id, summary)

    if success:
        return 0
    elif unavailable:
        return 2
    else:
        return 1


def cmd_subagents(client: SessionManagerClient, target_session_id: str) -> int:
    """
    List subagents spawned by a session.

    Exit codes:
        0: Success (may have 0 subagents)
        1: Session not found
        2: Session manager unavailable
    """
    # First check if session exists
    session = client.get_session(target_session_id)
    if session is None:
        sessions = client.list_sessions()
        if sessions is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        else:
            print("Error: Session not found", file=sys.stderr)
            return 1

    # Get subagents
    subagents = client.list_subagents(target_session_id)

    if subagents is None:
        print("Error: Failed to list subagents", file=sys.stderr)
        return 1

    if not subagents:
        print(f"{session.get('friendly_name', target_session_id)} has no subagents")
        return 0

    # Display subagents
    name = session.get("friendly_name", target_session_id)
    print(f"{name} ({target_session_id}) subagents:")
    for sa in subagents:
        elapsed = format_relative_time(sa["started_at"])
        status_icon = "✓" if sa["status"] == "completed" else "→"
        print(f"  {status_icon} {sa['agent_type']} ({sa['agent_id'][:6]}) | {sa['status']} | {elapsed}")
        if sa.get("summary"):
            print(f"     {sa['summary']}")

    return 0


def cmd_send(
    client: SessionManagerClient,
    identifier: str,
    text: str,
    delivery_mode: str = "sequential",
    timeout_seconds: Optional[int] = None,
    notify_on_delivery: bool = False,
    notify_after_seconds: Optional[int] = None,
    wait_seconds: Optional[int] = None,
) -> int:
    """
    Send input text to a session.

    Args:
        client: API client
        identifier: Target session ID or friendly name
        text: Text to send
        delivery_mode: Delivery mode (sequential, important, urgent)
        timeout_seconds: Drop message if not delivered in this time
        notify_on_delivery: Notify sender when delivered
        notify_after_seconds: Notify sender N seconds after delivery
        wait_seconds: Notify sender N seconds after delivery if recipient is idle (alias for notify_after_seconds)

    Exit codes:
        0: Success
        1: Session not found or send failed
        2: Session manager unavailable
    """
    # Resolve identifier to session ID and get session details
    session_id, session = resolve_session_id(client, identifier)
    if session_id is None:
        # Check if it's unavailable or not found
        sessions = client.list_sessions()
        if sessions is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        else:
            print(f"Error: Session '{identifier}' not found", file=sys.stderr)
            return 1

    # Get sender session ID from environment (if available)
    sender_session_id = client.session_id  # Set from CLAUDE_SESSION_MANAGER_ID in __init__

    # Use wait_seconds if provided, otherwise use notify_after_seconds
    effective_notify_after = wait_seconds if wait_seconds is not None else notify_after_seconds

    # Send input with sender metadata, delivery mode, and sm send flag
    success, unavailable = client.send_input(
        session_id,
        text,
        sender_session_id=sender_session_id,
        delivery_mode=delivery_mode,
        from_sm_send=True,  # This is from sm send command
        timeout_seconds=timeout_seconds,
        notify_on_delivery=notify_on_delivery,
        notify_after_seconds=effective_notify_after,
    )

    if unavailable:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    if not success:
        print(f"Error: Failed to send input to session {session_id}", file=sys.stderr)
        return 1

    # Success - show different message based on delivery mode
    name = session.get("friendly_name") or session.get("name") or session_id
    if delivery_mode == "sequential":
        print(f"Queued for {name} ({session_id}) (will inject when idle)")
    elif delivery_mode == "urgent":
        print(f"Input sent to {name} ({session_id}) (interrupted)")
    else:  # important
        print(f"Input sent to {name} ({session_id})")

    # Show additional options if used
    extras = []
    if timeout_seconds:
        extras.append(f"timeout={timeout_seconds}s")
    if notify_on_delivery:
        extras.append("notify-on-delivery")
    if effective_notify_after:
        extras.append(f"wait={effective_notify_after}s")
    if extras:
        print(f"  Options: {', '.join(extras)}")

    return 0


def cmd_remind(client: SessionManagerClient, session_id: str, delay_seconds: int, message: str) -> int:
    """
    Schedule a self-reminder.

    Args:
        client: API client
        session_id: Current session ID (to receive the reminder)
        delay_seconds: Seconds until reminder fires
        message: Reminder message

    Exit codes:
        0: Success
        1: Failed to schedule
        2: Session manager unavailable
    """
    result = client.schedule_reminder(session_id, delay_seconds, message)

    if result is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    if result.get("status") == "scheduled":
        reminder_id = result.get("reminder_id", "unknown")
        # Format delay for display
        if delay_seconds >= 3600:
            delay_str = f"{delay_seconds // 3600}h{(delay_seconds % 3600) // 60}m"
        elif delay_seconds >= 60:
            delay_str = f"{delay_seconds // 60}m{delay_seconds % 60}s"
        else:
            delay_str = f"{delay_seconds}s"
        print(f"Reminder scheduled ({reminder_id}): fires in {delay_str}")
        return 0
    else:
        print(f"Error: Failed to schedule reminder", file=sys.stderr)
        return 1


def cmd_queue(client: SessionManagerClient, session_id: str) -> int:
    """
    Show pending message queue for a session.

    Args:
        client: API client
        session_id: Session ID to check

    Exit codes:
        0: Success
        1: Session not found
        2: Session manager unavailable
    """
    result = client.get_queue_status(session_id)

    if result is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    is_idle = result.get("is_idle", False)
    pending_count = result.get("pending_count", 0)
    messages = result.get("pending_messages", [])
    saved_input = result.get("saved_user_input")

    # Print status
    status = "idle" if is_idle else "active"
    print(f"Session {session_id}: {status}")
    print(f"Pending messages: {pending_count}")

    if saved_input:
        print(f"Saved user input: {saved_input[:50]}...")

    if messages:
        print()
        for i, msg in enumerate(messages, 1):
            sender = msg.get("sender") or "unknown"
            mode = msg.get("delivery_mode", "sequential")
            queued = msg.get("queued_at", "")[:19]  # Trim to datetime
            timeout = msg.get("timeout_at")
            timeout_str = f" (expires {timeout[:19]})" if timeout else ""
            print(f"  {i}. from {sender} [{mode}] queued {queued}{timeout_str}")

    return 0


def cmd_spawn(
    client: SessionManagerClient,
    parent_session_id: str,
    prompt: str,
    name: Optional[str] = None,
    wait: Optional[int] = None,
    model: Optional[str] = None,
    working_dir: Optional[str] = None,
    json_output: bool = False,
) -> int:
    """
    Spawn a child agent session.

    Args:
        client: API client
        parent_session_id: Parent session ID (current session)
        prompt: Initial prompt for the child agent
        name: Friendly name for the child session
        wait: Monitor child and notify when complete or idle for N seconds
        model: Model override (opus, sonnet, haiku)
        working_dir: Working directory override
        json_output: Output JSON format

    Exit codes:
        0: Success
        1: Failed to spawn
        2: Session manager unavailable
    """
    import json as json_lib

    # Spawn child session
    result = client.spawn_child(
        parent_session_id=parent_session_id,
        prompt=prompt,
        name=name,
        wait=wait,
        model=model,
        working_dir=working_dir,
    )

    if result is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    if result.get("error"):
        print(f"Error: {result['error']}", file=sys.stderr)
        return 1

    # Success
    if json_output:
        print(json_lib.dumps(result, indent=2))
    else:
        child_id = result["session_id"]
        child_name = result.get("friendly_name") or result["name"]
        print(f"Spawned {child_name} ({child_id}) in tmux session {result['tmux_session']}")

    return 0


def cmd_children(
    client: SessionManagerClient,
    parent_session_id: str,
    recursive: bool = False,
    status_filter: Optional[str] = None,
    json_output: bool = False,
) -> int:
    """
    List child sessions.

    Args:
        client: API client
        parent_session_id: Parent session ID
        recursive: Include grandchildren
        status_filter: Filter by status (running, completed, error, all)
        json_output: Output JSON format

    Exit codes:
        0: Success (children found)
        1: No children or error
        2: Session manager unavailable
    """
    import json as json_lib

    # Get children
    result = client.list_children(
        parent_session_id=parent_session_id,
        recursive=recursive,
        status_filter=status_filter,
    )

    if result is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    children = result.get("children", [])
    if not children:
        if not json_output:
            print("No child sessions")
        return 1

    if json_output:
        print(json_lib.dumps(children, indent=2))
    else:
        for child in children:
            name = child.get("friendly_name") or child["name"]
            child_id = child["id"]
            status = child.get("completion_status") or child["status"]
            last_activity = child.get("last_activity", "")
            completion_msg = child.get("completion_message", "")

            # Format last activity as relative time
            if last_activity:
                from datetime import datetime
                try:
                    activity_time = datetime.fromisoformat(last_activity)
                    elapsed = format_relative_time(activity_time)
                except:
                    elapsed = "unknown"
            else:
                elapsed = "unknown"

            # Print child info
            status_icon = "✓" if status == "completed" else "●" if status == "running" else "✗"
            print(f"{name} ({child_id}) | {status} | {elapsed}", end="")
            if completion_msg:
                print(f' | "{completion_msg}"')
            else:
                print()

    return 0


def cmd_kill(
    client: SessionManagerClient,
    requester_session_id: Optional[str],
    target_identifier: str,
) -> int:
    """
    Kill a child session (with parent-child ownership check).

    Args:
        client: API client
        requester_session_id: Requesting session ID (must be parent)
        target_identifier: Target session ID or friendly name

    Exit codes:
        0: Success
        1: Not authorized or failed
        2: Session manager unavailable
    """
    # Resolve identifier to session ID and get session details
    target_session_id, session = resolve_session_id(client, target_identifier)
    if target_session_id is None:
        # Check if it's unavailable or not found
        sessions = client.list_sessions()
        if sessions is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        else:
            print(f"Error: Session '{target_identifier}' not found", file=sys.stderr)
            return 1

    # Kill session with ownership check
    result = client.kill_session(
        requester_session_id=requester_session_id,
        target_session_id=target_session_id,
    )

    if result is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    if result.get("error"):
        print(f"Error: {result['error']}", file=sys.stderr)
        return 1

    # Success - show friendly name if available
    name = session.get("friendly_name") or session.get("name") or target_session_id
    print(f"Session {name} ({target_session_id}) terminated")
    return 0


def cmd_new(client: SessionManagerClient, working_dir: Optional[str] = None) -> int:
    """
    Create a new Claude Code session and attach to it.

    Args:
        client: API client
        working_dir: Working directory (optional, defaults to $PWD)

    Exit codes:
        0: Successfully created and attached
        1: Failed to create session
        2: Session manager unavailable
    """
    import os
    import subprocess
    import time
    from pathlib import Path

    # Resolve working directory
    if working_dir is None:
        working_dir = os.getcwd()

    # Expand and resolve path
    try:
        path = Path(working_dir).expanduser().resolve()
        if not path.exists():
            print(f"Error: Directory does not exist: {working_dir}", file=sys.stderr)
            return 1
        if not path.is_dir():
            print(f"Error: Not a directory: {working_dir}", file=sys.stderr)
            return 1
        working_dir = str(path)
    except Exception as e:
        print(f"Error: Invalid path: {e}", file=sys.stderr)
        return 1

    # Create session via API
    print(f"Creating session in {working_dir}...")
    session = client.create_session(working_dir)

    if session is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    # Extract tmux session name
    tmux_session = session.get("tmux_session")
    session_id = session.get("id")

    if not tmux_session:
        print("Error: Failed to get tmux session name", file=sys.stderr)
        return 1

    print(f"Session created: {session_id}")
    print(f"Attaching to {tmux_session}...")

    # Wait briefly for Claude to initialize
    time.sleep(1)

    # Attach to tmux session (blocks until detach)
    try:
        subprocess.run(["tmux", "attach", "-t", tmux_session], check=True)
        return 0
    except subprocess.CalledProcessError:
        print(f"Error: Failed to attach to tmux session {tmux_session}", file=sys.stderr)
        return 1
    except FileNotFoundError:
        print("Error: tmux not found. Is tmux installed?", file=sys.stderr)
        return 1


def cmd_attach(client: SessionManagerClient, identifier: Optional[str] = None) -> int:
    """
    Attach to an existing Claude Code session.

    Args:
        client: API client
        identifier: Session ID or friendly name (optional, shows menu if omitted)

    Exit codes:
        0: Successfully attached
        1: No sessions available or invalid selection
        2: Session manager unavailable
    """
    import subprocess

    # Case 1: Direct attach with identifier
    if identifier:
        # Resolve identifier to session
        session_id, session = resolve_session_id(client, identifier)

        if session_id is None:
            sessions = client.list_sessions()
            if sessions is None:
                print("Error: Session manager unavailable", file=sys.stderr)
                return 2
            else:
                print(f"Error: Session '{identifier}' not found", file=sys.stderr)
                return 1

        # Extract tmux session name
        tmux_session = session.get("tmux_session")
        if not tmux_session:
            print("Error: Session has no tmux session", file=sys.stderr)
            return 1

        # Attach
        try:
            subprocess.run(["tmux", "attach", "-t", tmux_session], check=True)
            return 0
        except subprocess.CalledProcessError:
            print(f"Error: Failed to attach to tmux session {tmux_session}", file=sys.stderr)
            return 1
        except FileNotFoundError:
            print("Error: tmux not found. Is tmux installed?", file=sys.stderr)
            return 1

    # Case 2: Interactive menu
    sessions = client.list_sessions()

    if sessions is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    # Filter out stopped sessions only (error status is still attachable)
    active_sessions = [
        s for s in sessions
        if s.get("status") != "stopped"
    ]

    if not active_sessions:
        print("No sessions available")
        return 1

    # Single session - attach directly
    if len(active_sessions) == 1:
        session = active_sessions[0]
        tmux_session = session.get("tmux_session")

        try:
            subprocess.run(["tmux", "attach", "-t", tmux_session], check=True)
            return 0
        except subprocess.CalledProcessError:
            print(f"Error: Failed to attach to tmux session {tmux_session}", file=sys.stderr)
            return 1

    # Multiple sessions - show menu
    print("Available sessions:")
    for i, session in enumerate(active_sessions, start=1):
        print(format_session_line(session, index=i))

    print()

    # Get user selection
    try:
        selection = input("Select session (number or name): ").strip()
    except KeyboardInterrupt:
        print("\nCancelled")
        return 1
    except EOFError:
        print("\nCancelled")
        return 1

    if not selection:
        print("No selection made")
        return 1

    # Try as number first
    if selection.isdigit():
        index = int(selection)
        if index < 1 or index > len(active_sessions):
            print(f"Error: Invalid selection. Must be between 1 and {len(active_sessions)}", file=sys.stderr)
            return 1
        session = active_sessions[index - 1]
    else:
        # Try as session ID or friendly name
        session_id, session = resolve_session_id(client, selection)
        if session_id is None:
            print(f"Error: Session '{selection}' not found", file=sys.stderr)
            return 1

    # Attach to selected session
    tmux_session = session.get("tmux_session")
    if not tmux_session:
        print("Error: Session has no tmux session", file=sys.stderr)
        return 1

    try:
        subprocess.run(["tmux", "attach", "-t", tmux_session], check=True)
        return 0
    except subprocess.CalledProcessError:
        print(f"Error: Failed to attach to tmux session {tmux_session}", file=sys.stderr)
        return 1
    except FileNotFoundError:
        print("Error: tmux not found. Is tmux installed?", file=sys.stderr)
        return 1


def cmd_output(client: SessionManagerClient, identifier: str, lines: int) -> int:
    """
    Show recent tmux output from a session.

    Args:
        client: API client
        identifier: Session ID or friendly name
        lines: Number of lines to capture

    Exit codes:
        0: Success
        1: Session not found or no tmux session
        2: Session manager unavailable
    """
    from ..tmux_controller import TmuxController

    # Resolve identifier to session ID and get session details
    session_id, session = resolve_session_id(client, identifier)
    if session_id is None:
        # Check if it's unavailable or not found
        sessions = client.list_sessions()
        if sessions is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        else:
            print(f"Error: Session '{identifier}' not found", file=sys.stderr)
            return 1

    # Extract tmux session name
    tmux_session = session.get("tmux_session")
    if not tmux_session:
        print(f"Error: Session has no tmux session", file=sys.stderr)
        return 1

    # Capture pane output
    tmux_controller = TmuxController()
    output = tmux_controller.capture_pane(tmux_session, lines=lines)

    if output is None:
        print(f"Error: Failed to capture output from {tmux_session}", file=sys.stderr)
        return 1

    # Print output
    print(output, end="")
    return 0


def cmd_wait(
    client: SessionManagerClient,
    identifier: str,
    timeout_seconds: int,
) -> int:
    """
    Wait for a session to go idle or timeout.

    Args:
        client: API client
        identifier: Target session ID or friendly name
        timeout_seconds: Maximum seconds to wait

    Exit codes:
        0: Session went idle
        1: Timeout reached (session still active)
        2: Session manager unavailable or session not found
    """
    import time

    # Resolve identifier to session ID
    session_id, session = resolve_session_id(client, identifier)
    if session_id is None:
        # Check if it's unavailable or not found
        sessions = client.list_sessions()
        if sessions is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        else:
            print(f"Error: Session '{identifier}' not found", file=sys.stderr)
            return 2

    # Poll interval (check every 2 seconds)
    poll_interval = 2
    elapsed = 0
    start_time = time.time()

    while elapsed < timeout_seconds:
        # Check if session is idle
        result = client.get_queue_status(session_id)

        if result is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2

        is_idle = result.get("is_idle", False)

        if is_idle:
            # Session is idle
            name = session.get("friendly_name") or session.get("name") or session_id
            print(f"{name} is idle (waited {int(elapsed)}s)")
            return 0

        # Sleep and continue
        time.sleep(poll_interval)
        elapsed = time.time() - start_time

    # Timeout reached
    name = session.get("friendly_name") or session.get("name") or session_id
    print(f"Timeout: {name} still active after {timeout_seconds}s")
    return 1


def cmd_clear(
    client: SessionManagerClient,
    requester_session_id: Optional[str],
    target_identifier: str,
    new_prompt: Optional[str] = None,
) -> int:
    """
    Send /clear to a child Claude Code session to reset its context.
    Requires parent-child ownership (requester must be parent of target).

    Args:
        client: API client
        requester_session_id: Requesting session ID (must be parent)
        target_identifier: Target session ID or friendly name
        new_prompt: Optional prompt to send after clearing

    Exit codes:
        0: Success
        1: Not authorized or clear failed
        2: Session manager unavailable
    """
    import subprocess
    import time

    # Resolve identifier to session ID and get session details
    target_session_id, session = resolve_session_id(client, target_identifier)
    if target_session_id is None:
        # Check if it's unavailable or not found
        sessions = client.list_sessions()
        if sessions is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        else:
            print(f"Error: Session '{target_identifier}' not found", file=sys.stderr)
            return 1

    # Check parent-child ownership
    # Only allow clearing if:
    # 1. Requester is the parent of the target session
    # 2. Or requester is None (called from outside a session, like from shell)
    parent_id = session.get("parent_session_id")

    if requester_session_id is not None:
        # Called from within a session - must be parent
        if parent_id != requester_session_id:
            print(f"Error: Not authorized. You can only clear your child sessions.", file=sys.stderr)
            print(f"Target session parent: {parent_id or 'none'}", file=sys.stderr)
            return 1
    else:
        # Called from shell (no CLAUDE_SESSION_MANAGER_ID)
        # Only allow if target is a child session (has a parent)
        if parent_id is None:
            print(f"Error: Can only clear child sessions. Target session has no parent.", file=sys.stderr)
            return 1

    # Extract tmux session name
    tmux_session = session.get("tmux_session")
    if not tmux_session:
        print(f"Error: Session {target_session_id} has no tmux session", file=sys.stderr)
        return 1

    # Send /clear command
    try:
        import shlex

        # First, send ESC to interrupt any ongoing stream
        subprocess.run(
            ["tmux", "send-keys", "-t", tmux_session, "Escape"],
            check=True,
            capture_output=True,
            text=True,
        )

        # Wait for interrupt to process
        time.sleep(0.5)

        # Now send /clear using the same approach as send_input
        subprocess.run(
            ["tmux", "send-keys", "-t", tmux_session, "/clear"],
            check=True,
            capture_output=True,
            text=True,
        )
        time.sleep(1)
        subprocess.run(
            ["tmux", "send-keys", "-t", tmux_session, "Enter"],
            check=True,
            capture_output=True,
            text=True,
        )

        # Wait for clear to process
        time.sleep(2)

        # Send new prompt if provided
        if new_prompt:
            subprocess.run(
                ["tmux", "send-keys", "-t", tmux_session, new_prompt],
                check=True,
                capture_output=True,
                text=True,
            )
            time.sleep(1)
            subprocess.run(
                ["tmux", "send-keys", "-t", tmux_session, "Enter"],
                check=True,
                capture_output=True,
                text=True,
            )

            name = session.get("friendly_name") or session.get("name") or target_session_id
            print(f"Cleared {name} ({target_session_id}) and sent new prompt")
        else:
            name = session.get("friendly_name") or session.get("name") or target_session_id
            print(f"Cleared {name} ({target_session_id})")

        return 0

    except subprocess.CalledProcessError as e:
        print(f"Error: Failed to send clear command: {e.stderr}", file=sys.stderr)
        return 1
    except FileNotFoundError:
        print("Error: tmux not found. Is tmux installed?", file=sys.stderr)
        return 1
