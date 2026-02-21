"""Command implementations for sm CLI."""

import os
import re
import sqlite3
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path
from typing import Optional

_BASH_DISPLAY_WIDTH = 80

from .client import SessionManagerClient
from .formatting import format_session_line, format_relative_time, format_status_list
from ..lock_manager import LockManager

# Settle delay between tmux send-keys calls to avoid paste detection.
# Mirrors TmuxController.send_keys_settle_seconds (default 0.3s).
# Claude Code (Node.js TUI in raw mode) treats a rapid character burst as pasted
# text; Enter must arrive as a separate event after the paste mode ends.
_SEND_KEYS_SETTLE_SECONDS = 0.3


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
        seconds = int(duration_str)
        if seconds <= 0:
            raise ValueError("Duration must be positive")
        return seconds

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

    if total_seconds <= 0:
        raise ValueError("Duration must be positive")

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
    # Reject empty or blank identifiers
    if not identifier or not identifier.strip():
        return None, None

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


def validate_friendly_name(name: str) -> tuple[bool, str]:
    """
    Validate friendly name for shell compatibility.

    Args:
        name: The friendly name to validate

    Returns:
        Tuple of (is_valid, error_message)
        If valid, error_message is empty string
        If invalid, error_message describes the problem
    """
    if not name:
        return False, "Name cannot be empty"

    if len(name) > 32:
        return False, "Name too long (max 32 chars)"

    if not re.match(r'^[a-zA-Z0-9_-]+$', name):
        return False, "Name must be alphanumeric with - or _ only (no spaces)"

    return True, ""


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

        # Validate the name
        valid, error = validate_friendly_name(friendly_name)
        if not valid:
            print(f"Error: {error}", file=sys.stderr)
            return 1

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

    # Validate the name
    valid, error = validate_friendly_name(friendly_name)
    if not valid:
        print(f"Error: {error}", file=sys.stderr)
        return 1

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


def cmd_what(client: SessionManagerClient, identifier: str, lines: int, deep: bool = False) -> int:
    """
    Get AI-generated summary of what a session is doing.

    Exit codes:
        0: Success
        1: Session not found or summary unavailable
        2: Session manager unavailable
    """
    # Resolve identifier (could be session ID or friendly name)
    session_id, session = resolve_session_id(client, identifier)
    if session_id is None:
        sessions = client.list_sessions()
        if sessions is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        else:
            print("Error: Session not found", file=sys.stderr)
            return 1

    summary = client.get_summary(session_id, lines)

    if summary is None:
        print("Error: Summary unavailable", file=sys.stderr)
        return 1

    print(summary)

    # If --deep flag is set, show subagent activity
    if deep:
        subagents = client.list_subagents(session_id)
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


def cmd_clean(client: SessionManagerClient, session_ids: Optional[list] = None) -> int:
    """
    Close Telegram forum topics for idle/completed sessions (Fix C: sm#271).

    Without --session-id: closes topics for all sessions with completion_status=COMPLETED.
    With --session-id: closes topics for the specified session IDs (rejects running/em sessions).

    Exit codes:
        0: Success
        1: Operation failed or session manager error
        2: Session manager unavailable
    """
    result, unavailable = client.cleanup_idle_topics(session_ids=session_ids)

    if unavailable:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    if result is None:
        print("Error: Cleanup failed", file=sys.stderr)
        return 1

    if session_ids is not None:
        # Mode 2: explicit
        closed = result.get("closed", 0)
        rejected = result.get("rejected", [])
        print(f"Closed {closed} topic(s)")
        if rejected:
            print(f"Rejected {len(rejected)}:")
            for r in rejected:
                print(f"  {r['id']}: {r['reason']}")
    else:
        # Mode 1: automated
        closed = result.get("closed", 0)
        skipped = result.get("skipped", 0)
        print(f"Closed {closed} topic(s), skipped {skipped}")

    return 0


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


def cmd_dispatch(
    client: SessionManagerClient,
    agent_id: str,
    role: str,
    params: dict,
    em_id: Optional[str],
    dry_run: bool = False,
    no_clear: bool = False,
    delivery_mode: str = "sequential",
    notify_on_stop: bool = True,
) -> int:
    """Dispatch a template-expanded prompt to a target agent.

    Args:
        client: API client
        agent_id: Target session ID or friendly name
        role: Role name from template config
        params: Dynamic parameters (--issue, --spec, etc.)
        em_id: Sender's session ID
        dry_run: Print expanded template instead of sending
        no_clear: Skip clearing target session before dispatch
        delivery_mode: Delivery mode for sm send
        notify_on_stop: Notify on stop flag for sm send

    Exit codes:
        0: Success
        1: Template/param error or send failed
        2: Session manager unavailable
    """
    from .dispatch import load_template, expand_template, get_role_params, get_auto_remind_config, DispatchError

    try:
        config = load_template(os.getcwd())
    except DispatchError as e:
        print(f"Error: {e}", file=sys.stderr)
        return 1

    # Validate dynamic params against role's required/optional
    try:
        required, optional = get_role_params(config, role)
    except DispatchError as e:
        print(f"Error: {e}", file=sys.stderr)
        return 1

    all_known = set(required) | set(optional)
    for key in params:
        if key not in all_known:
            print(f"Error: Unknown parameter '--{key}' for role '{role}'", file=sys.stderr)
            return 1

    try:
        expanded = expand_template(config, role, params, em_id, dry_run=dry_run)
    except DispatchError as e:
        print(f"Error: {e}", file=sys.stderr)
        return 1

    if dry_run:
        print(expanded)
        return 0

    # Clear target before dispatch unless opted out (#234).
    if not no_clear:
        rc = cmd_clear(client, em_id, agent_id)
        if rc != 0:
            return rc

    # Auto-arm periodic remind and parent wake on every dispatch (#225-A, #225-C).
    soft_threshold, hard_threshold = get_auto_remind_config(os.getcwd())

    return cmd_send(
        client, agent_id, expanded, delivery_mode,
        notify_on_stop=notify_on_stop,
        remind_soft_threshold=soft_threshold,
        remind_hard_threshold=hard_threshold,
        parent_session_id=em_id,
    )


def cmd_agent_status(client: SessionManagerClient, session_id: str, text: str) -> int:
    """
    Self-report agent status and reset the remind timer.

    Args:
        client: API client
        session_id: Current session ID (self)
        text: Status text to report

    Exit codes:
        0: Success
        1: Failed to set status
        2: Session manager unavailable
    """
    success, unavailable = client.set_agent_status(session_id, text)

    if success:
        print(f"Status set: {text}")
        return 0
    elif unavailable:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2
    else:
        print("Error: Failed to set status", file=sys.stderr)
        return 1


def cmd_remind_stop(client: SessionManagerClient, target_identifier: str) -> int:
    """
    Cancel periodic remind for a target session.

    Args:
        client: API client
        target_identifier: Target session ID or friendly name

    Exit codes:
        0: Success
        1: Session not found or failed
        2: Session manager unavailable
    """
    target_session_id, session = resolve_session_id(client, target_identifier)
    if target_session_id is None:
        sessions = client.list_sessions()
        if sessions is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        else:
            print(f"Error: Session '{target_identifier}' not found", file=sys.stderr)
            return 1

    success, unavailable = client.cancel_remind(target_session_id)

    if success:
        name = session.get("friendly_name") or session.get("name") or target_session_id
        print(f"Remind cancelled for {name} ({target_session_id})")
        return 0
    elif unavailable:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2
    else:
        print("Error: Failed to cancel remind", file=sys.stderr)
        return 1


def cmd_send(
    client: SessionManagerClient,
    identifier: str,
    text: str,
    delivery_mode: str = "sequential",
    timeout_seconds: Optional[int] = None,
    notify_on_delivery: bool = False,
    notify_after_seconds: Optional[int] = None,
    wait_seconds: Optional[int] = None,
    notify_on_stop: bool = True,
    remind_soft_threshold: Optional[int] = None,
    remind_hard_threshold: Optional[int] = None,
    parent_session_id: Optional[str] = None,
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
        notify_on_stop: Notify sender when receiver's Stop hook fires (default True)
        remind_soft_threshold: Soft remind threshold in seconds; only set by sm dispatch (#225-A)
        remind_hard_threshold: Hard remind threshold in seconds; only set by sm dispatch (#225-A)
        parent_session_id: EM session to receive periodic wake digests; only set by sm dispatch (#225-C)

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
        notify_on_stop=notify_on_stop,
        remind_soft_threshold=remind_soft_threshold,
        remind_hard_threshold=remind_hard_threshold,
        parent_session_id=parent_session_id,
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
    if notify_on_stop:
        extras.append("notify-on-stop")
    if remind_soft_threshold:
        extras.append(f"remind={remind_soft_threshold}s soft"
                      + (f"/{remind_hard_threshold}s hard" if remind_hard_threshold else ""))
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
    provider: str,
    prompt: str,
    name: Optional[str] = None,
    wait: Optional[int] = None,
    model: Optional[str] = None,
    working_dir: Optional[str] = None,
    json_output: bool = False,
) -> int:
    """
    Spawn a child agent session.

    When the parent session has is_em=True, auto-registers the spawned child for:
    - remind (thresholds from config.yaml dispatch.auto_remind, default 210s soft / 420s hard → EM session)
    - context monitoring (alerts → EM session)
    - notify-on-stop pointing to EM session
    This mirrors what sm dispatch does (sm#277).

    Args:
        client: API client
        parent_session_id: Parent session ID (current session)
        provider: "claude", "codex", or "codex-app"
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
        provider=provider,
    )

    if result is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    if result.get("error"):
        print(f"Error: {result['error']}", file=sys.stderr)
        return 1

    # Success
    child_id = result["session_id"]
    child_name = result.get("friendly_name") or result["name"]
    provider = result.get("provider", "claude")

    if json_output:
        print(json_lib.dumps(result, indent=2))
    else:
        if provider == "codex-app":
            print(f"Spawned {child_name} ({child_id}) [codex-app]")
        else:
            print(f"Spawned {child_name} ({child_id}) in tmux session {result['tmux_session']}")

    # Auto-register EM monitoring when parent is EM (sm#277)
    parent_session = client.get_session(parent_session_id)
    if parent_session and parent_session.get("is_em"):
        from .dispatch import get_auto_remind_config
        soft_threshold, hard_threshold = get_auto_remind_config(os.getcwd())
        _register_em_monitoring(client, child_id, parent_session_id, soft_threshold, hard_threshold)

    return 0


def _register_em_monitoring(
    client: SessionManagerClient,
    child_id: str,
    em_session_id: str,
    soft_threshold: int,
    hard_threshold: int,
) -> None:
    """Register remind, context monitoring, and notify-on-stop for an EM-spawned child (sm#277).

    Args:
        client: API client
        child_id: Spawned child session ID
        em_session_id: EM parent session ID (is_em=True)
        soft_threshold: Soft remind threshold in seconds (from config.yaml or default)
        hard_threshold: Hard remind threshold in seconds (from config.yaml or default)
    """
    # Remind: thresholds from config.yaml (or defaults), alerts → EM
    remind_result = client.register_remind(child_id, soft_threshold=soft_threshold, hard_threshold=hard_threshold)
    if remind_result is None:
        print(f"  Warning: Failed to register remind for {child_id}", file=sys.stderr)

    # Context monitoring: enabled, alerts → EM
    _, cm_ok, _ = client.set_context_monitor(
        child_id,
        enabled=True,
        requester_session_id=em_session_id,
        notify_session_id=em_session_id,
    )
    if not cm_ok:
        print(f"  Warning: Failed to enable context monitoring for {child_id}", file=sys.stderr)

    # Notify-on-stop: fires → EM when child stops
    ns_ok, _ = client.arm_stop_notify(
        child_id,
        sender_session_id=em_session_id,
        requester_session_id=em_session_id,
    )
    if not ns_ok:
        print(f"  Warning: Failed to arm stop notification for {child_id}", file=sys.stderr)


_TOOL_DB_DEFAULT = "~/.local/share/claude-sessions/tool_usage.db"

# Sentinel returned by _query_last_tool when the DB is unavailable or locked.
# Distinct from None ("no data for this session") so cmd_children can warn once.
_DB_ERROR = object()


def _query_last_tool(session_id: str, db_path: str) -> Optional[dict]:
    """
    Query tool_usage.db for the most recent PreToolUse event for a session.

    Returns dict with: tool_name, target_file, bash_command, timestamp_str (UTC)
    Returns None if DB unavailable or no entries found.
    """
    try:
        conn = sqlite3.connect(db_path)
        try:
            cursor = conn.cursor()
            cursor.execute(
                """
                SELECT tool_name, target_file, bash_command, timestamp
                FROM tool_usage
                WHERE hook_type = 'PreToolUse' AND session_id = ?
                ORDER BY timestamp DESC
                LIMIT 1
                """,
                (session_id,),
            )
            row = cursor.fetchone()
        finally:
            conn.close()
    except sqlite3.Error:
        return _DB_ERROR  # type: ignore[return-value]

    if not row:
        return None

    return {
        "tool_name": row[0],
        "target_file": row[1],
        "bash_command": row[2],
        "timestamp_str": row[3],
    }


def _get_tmux_session_activity(tmux_session_name: str) -> Optional[int]:
    """
    Returns Unix epoch of last tmux session activity via:
      tmux display-message -p -t <name> '#{session_activity}'
    Returns None if session not found or tmux unavailable.
    """
    try:
        result = subprocess.run(
            ["tmux", "display-message", "-p", "-t", tmux_session_name, "#{session_activity}"],
            capture_output=True,
            text=True,
            timeout=3,
        )
        output = result.stdout.strip()
        if output:
            return int(output)
    except (subprocess.SubprocessError, ValueError, OSError):
        pass
    return None


def _format_thinking_duration(seconds: int) -> str:
    """Format thinking duration: 'Xm Ys' for >=1min, 'Xs' for <1min."""
    if seconds < 60:
        return f"{seconds}s"
    return f"{seconds // 60}m{seconds % 60:02d}s"


def cmd_children(
    client: SessionManagerClient,
    parent_session_id: str,
    recursive: bool = False,
    status_filter: Optional[str] = None,
    json_output: bool = False,
    db_path: Optional[str] = None,
) -> int:
    """
    List child sessions.

    Args:
        client: API client
        parent_session_id: Parent session ID
        recursive: Include grandchildren
        status_filter: Filter by status (running, completed, error, all)
        json_output: Output JSON format
        db_path: Override tool_usage.db path

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
        return 0  # No children is a valid result, not an error

    if json_output:
        print(json_lib.dumps(children, indent=2))
    else:
        # Resolve DB path once; warn at most once on failure
        resolved_db = str(Path(db_path or _TOOL_DB_DEFAULT).expanduser())
        db_warned = False

        def _seconds_since(ts: str) -> Optional[int]:
            try:
                parsed = datetime.fromisoformat(ts)
                if parsed.tzinfo is None:
                    now = datetime.utcnow()
                else:
                    now = datetime.now(parsed.tzinfo)
                return max(0, int((now - parsed).total_seconds()))
            except Exception:
                return None

        for child in children:
            name = child.get("friendly_name") or child["name"]
            child_id = child["id"]
            status = child.get("completion_status") or child["status"]
            last_activity = child.get("last_activity", "")
            completion_msg = child.get("completion_message", "")
            provider = child.get("provider", "claude")

            # Format last activity as relative time — pass ISO string (fix for pre-existing bug)
            elapsed = format_relative_time(last_activity) if last_activity else "unknown"

            # Format agent status if available (#188)
            agent_status_text = child.get("agent_status_text")
            agent_status_at = child.get("agent_status_at")
            status_age = ""
            if agent_status_at:
                status_age = f" ({format_relative_time(agent_status_at)})"

            # Build the base line
            line = f"{name} ({child_id}) | {status} | {elapsed}"

            # Thinking duration + last tool — only for running sessions
            raw_status = child.get("status", "")
            if raw_status == "running":
                thinking_str = None
                last_tool_str = None
                last_label = "last tool"

                if provider == "claude":
                    db_ok = Path(resolved_db).exists()
                    if not db_ok and not db_warned:
                        print(
                            f"Warning: tool_usage.db not found at {resolved_db} — skipping thinking/last-tool signals",
                            file=sys.stderr,
                        )
                        db_warned = True
                    elif db_ok:
                        row = _query_last_tool(child_id, resolved_db)
                        if row is _DB_ERROR:
                            if not db_warned:
                                print(
                                    f"Warning: tool_usage.db locked or unreadable at {resolved_db} — skipping thinking/last-tool signals",
                                    file=sys.stderr,
                                )
                                db_warned = True
                        elif row:
                            ts = row["timestamp_str"]
                            delta_s = _seconds_since(ts)
                            if delta_s is not None:
                                thinking_str = _format_thinking_duration(delta_s)
                                # Format last tool detail
                                tool_name = row["tool_name"]
                                if tool_name == "Bash" and row["bash_command"]:
                                    detail = row["bash_command"][:60].split("\n")[0]
                                    last_tool_str = f"Bash: {detail} ({_format_thinking_duration(delta_s)} ago)"
                                elif tool_name in ("Read", "Write", "Edit") and row["target_file"]:
                                    last_tool_str = f"{tool_name} {row['target_file']} ({_format_thinking_duration(delta_s)} ago)"
                                else:
                                    last_tool_str = f"{tool_name} ({_format_thinking_duration(delta_s)} ago)"

                elif provider == "codex":
                    tmux_name = f"codex-{child_id}"
                    epoch = _get_tmux_session_activity(tmux_name)
                    if epoch is not None:
                        delta_s = max(0, int(time.time() - epoch))
                        thinking_str = _format_thinking_duration(delta_s)
                    last_tool_str = "n/a (no hooks)"

                elif provider == "codex-app":
                    projection = child.get("activity_projection")
                    if isinstance(projection, dict):
                        summary = projection.get("summary_text")
                        ts = projection.get("ended_at") or projection.get("started_at")
                        if isinstance(summary, str) and summary:
                            delta_s = _seconds_since(ts) if isinstance(ts, str) else None
                            if delta_s is not None:
                                thinking_str = _format_thinking_duration(delta_s)
                                last_tool_str = f"{summary} ({_format_thinking_duration(delta_s)} ago)"
                            else:
                                last_tool_str = summary
                            last_label = "last action"

                if thinking_str is not None:
                    line += f" | thinking {thinking_str}"
                if last_tool_str is not None:
                    line += f" | {last_label}: {last_tool_str}"

            # Append agent status / completion (unchanged #188 logic)
            if agent_status_text:
                line += f' | "{agent_status_text}"{status_age}'
            elif completion_msg:
                line += f' | "{completion_msg}"'
            else:
                line += " | (no status)"

            print(line)

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


def cmd_new(client: SessionManagerClient, working_dir: Optional[str] = None, provider: str = "claude") -> int:
    """
    Create a new session (Claude/Codex attach; Codex app is headless).

    Args:
        client: API client
        working_dir: Working directory (optional, defaults to $PWD)
        provider: "claude", "codex", or "codex-app"

    Exit codes:
        0: Successfully created (and attached for Claude/Codex)
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
    session = client.create_session(working_dir, provider=provider)

    if session is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    session_id = session.get("id")
    provider = session.get("provider", provider)

    if provider == "codex-app":
        print(f"Codex app session created: {session_id}")
        print("No tmux attach for Codex app sessions.")
        return 0

    # Extract tmux session name
    tmux_session = session.get("tmux_session")
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

    provider = session.get("provider", "claude")
    if provider == "codex-app":
        message = client.get_last_message(session_id)
        if not message:
            print("No output available for this Codex app session", file=sys.stderr)
            return 1
        print(message)
        return 0

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


def cmd_tail(
    client: SessionManagerClient,
    identifier: str,
    n: int = 10,
    raw: bool = False,
    db_path_override: Optional[str] = None,
) -> int:
    """
    Show recent activity for a session.

    Structured mode (default): last N PreToolUse events from tool_usage.db.
    Raw mode (--raw): last N lines of tmux pane output with ANSI stripped.

    Exit codes:
        0: Success
        1: Session not found, DB not found, or capture error
        2: Session manager unavailable
    """
    # Validate -n
    if n < 1:
        print("Error: -n must be at least 1", file=sys.stderr)
        return 1

    # Resolve identifier to session ID
    session_id, session = resolve_session_id(client, identifier)
    if session_id is None:
        sessions = client.list_sessions()
        if sessions is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        else:
            print(f"Error: Session '{identifier}' not found", file=sys.stderr)
            return 1

    name = session.get("friendly_name") or session.get("name") or session_id
    provider = session.get("provider", "claude")

    def _relative_age(ts_str: Optional[str]) -> str:
        if not ts_str:
            return "?"
        try:
            ts = datetime.fromisoformat(ts_str)
            if ts.tzinfo is None:
                now = datetime.utcnow()
            else:
                now = datetime.now(ts.tzinfo)
            delta_s = int((now - ts).total_seconds())
            if delta_s < 60:
                return f"{delta_s}s"
            if delta_s < 3600:
                return f"{delta_s // 60}m{delta_s % 60:02d}s"
            return f"{delta_s // 3600}h{(delta_s % 3600) // 60}m"
        except Exception:
            return "?"

    # --- Raw mode ---
    if raw:
        if provider == "codex-app":
            message = client.get_last_message(session_id)
            if not message:
                print("No output available for this Codex app session", file=sys.stderr)
                return 1
            print(message)
            return 0

        tmux_session = session.get("tmux_session")
        if not tmux_session:
            print("Error: Session has no tmux session", file=sys.stderr)
            return 1

        from ..tmux_controller import TmuxController
        tmux = TmuxController()
        output = tmux.capture_pane(tmux_session, lines=n)
        if output is None:
            print(f"Error: Failed to capture output from {tmux_session}", file=sys.stderr)
            return 1

        # Strip ANSI escape codes
        ansi_escape = re.compile(r'\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])')
        clean = ansi_escape.sub('', output)
        print(clean, end="")
        return 0

    if provider == "codex-app":
        projected = client.get_activity_actions(session_id, limit=n)
        if projected is None:
            print("Error: Failed to fetch codex-app activity projection", file=sys.stderr)
            return 1
        actions = projected.get("actions", [])
        if not actions:
            print(f"No activity data for {name} ({session_id})")
            return 0

        print(f"Last {len(actions)} actions ({name} {session_id[:8]}):")
        for action in actions:
            elapsed = _relative_age(action.get("ended_at") or action.get("started_at"))
            summary = action.get("summary_text") or action.get("action_kind") or "activity"
            status = action.get("status")
            status_suffix = f" [{status}]" if status else ""
            print(f"  [{elapsed} ago] {summary}{status_suffix}")
        return 0

    # --- Structured mode: query tool_usage.db ---
    db_path = Path(db_path_override or _TOOL_DB_DEFAULT).expanduser()
    if not db_path.exists():
        print(f"No tool usage data available (DB not found: {db_path})", file=sys.stderr)
        return 1

    try:
        conn = sqlite3.connect(db_path)
        try:
            cursor = conn.cursor()
            cursor.execute("""
                SELECT timestamp, tool_name, target_file, bash_command
                FROM tool_usage
                WHERE session_id = ? AND hook_type = 'PreToolUse'
                ORDER BY timestamp DESC
                LIMIT ?
            """, (session_id, n))
            rows = cursor.fetchall()
        finally:
            conn.close()
    except sqlite3.Error as e:
        print(f"Error: Failed to query tool usage: {e}", file=sys.stderr)
        return 1

    if not rows:
        print(f"No tool usage data for {name} ({session_id})")
        print("(Hooks may not be active for this session, or it's a Codex agent)")
        return 0

    print(f"Last {len(rows)} actions ({name} {session_id[:8]}):")

    for ts_str, tool_name, target_file, bash_command in reversed(rows):
        elapsed = _relative_age(ts_str)

        # Format action description
        if tool_name == "Bash" and bash_command:
            desc = bash_command[:_BASH_DISPLAY_WIDTH].split('\n')[0]  # first line, truncated
            action = f"Bash: {desc}"
        elif tool_name in ("Read", "Write", "Edit", "Glob") and target_file:
            action = f"{tool_name}: {target_file}"
        elif tool_name == "Grep":
            action = "Grep: (search)"
        elif tool_name == "Task":
            action = "Task: (subagent)"
        else:
            action = tool_name

        print(f"  [{elapsed} ago] {action}")

    return 0


def cmd_wait(
    client: SessionManagerClient,
    identifier: str,
    timeout_seconds: int,
) -> int:
    """
    Watch a session and get notified asynchronously when it goes idle or timeout.

    Args:
        client: API client
        identifier: Target session ID or friendly name
        timeout_seconds: Maximum seconds to wait

    Exit codes:
        0: Watch registered successfully
        1: Failed to register watch
        2: Session manager unavailable or session not found
    """
    # Resolve identifier to session ID
    target_session_id, target_session = resolve_session_id(client, identifier)
    if target_session_id is None:
        # Check if it's unavailable or not found
        sessions = client.list_sessions()
        if sessions is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        else:
            print(f"Error: Session '{identifier}' not found", file=sys.stderr)
            return 2

    # Get watcher session ID (current session)
    watcher_session_id = client.session_id
    if not watcher_session_id:
        print("Error: No session context (CLAUDE_SESSION_MANAGER_ID not set)", file=sys.stderr)
        return 1

    # Register watch via API (async notification)
    result = client.watch_session(target_session_id, watcher_session_id, timeout_seconds)

    if result is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    if result.get("status") != "watching":
        print(f"Error: Failed to watch session", file=sys.stderr)
        return 1

    # Success - watching asynchronously
    target_name = result.get("target_name") or target_session_id
    print(f"Watching {target_name}, will notify after {timeout_seconds}s idle")
    return 0


def cmd_review(
    client: SessionManagerClient,
    parent_session_id: Optional[str],
    session: Optional[str] = None,
    base: Optional[str] = None,
    uncommitted: bool = False,
    commit: Optional[str] = None,
    custom: Optional[str] = None,
    new: bool = False,
    name: Optional[str] = None,
    wait: Optional[int] = None,
    model: Optional[str] = None,
    working_dir: Optional[str] = None,
    steer: Optional[str] = None,
    pr: Optional[int] = None,
    repo: Optional[str] = None,
) -> int:
    """
    Start a Codex code review on an existing or new session.

    Args:
        client: API client
        parent_session_id: Current session ID (from CLAUDE_SESSION_MANAGER_ID)
        session: Target session ID or friendly name
        base: Review against this base branch
        uncommitted: Review uncommitted changes
        commit: Review a specific commit SHA
        custom: Custom review instructions
        new: Spawn a new session for the review
        name: Friendly name (with --new)
        wait: Notify when review completes (seconds)
        model: Model override (with --new)
        working_dir: Working directory (with --new)
        steer: Instructions to inject after review starts
        pr: PR number (stub for Phase 1b)
        repo: Repository for PR (stub for Phase 1b)

    Exit codes:
        0: Success
        1: Validation error or failed
        2: Session manager unavailable
    """
    # --pr mode: GitHub PR review (mutually exclusive with TUI modes)
    if pr is not None:
        if session or new:
            print("Error: --pr is mutually exclusive with session/--new", file=sys.stderr)
            return 1
        if base or uncommitted or commit or custom:
            print("Error: --pr is mutually exclusive with --base/--uncommitted/--commit/--custom", file=sys.stderr)
            return 1

        # Default --wait to 600 when caller has session context
        if wait is None and parent_session_id:
            wait = 600

        result = client.start_pr_review(
            pr_number=pr,
            repo=repo,
            steer=steer,
            wait=wait,
            caller_session_id=parent_session_id,
        )

        if result is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2

        if result.get("error"):
            print(f"Error: {result['error']}", file=sys.stderr)
            return 1

        resolved_repo = result.get("repo", repo or "unknown")
        print(f"Posted @codex review on PR #{pr} ({resolved_repo})")
        if result.get("server_polling"):
            print(f"  Server polling for completion (timeout={wait}s)")
        elif wait and not parent_session_id:
            # CLI-side polling (standalone, no session context)
            from src.github_reviews import poll_for_codex_review as _poll
            from datetime import datetime as _dt
            since = _dt.fromisoformat(result["posted_at"])
            print(f"  Waiting for Codex review (timeout={wait}s)...")
            review = _poll(
                repo=resolved_repo,
                pr_number=pr,
                since=since,
                timeout=wait,
            )
            if review:
                print(f"Codex review posted on PR #{pr}: {review.get('state', 'unknown')}")
                return 0
            else:
                print(f"Timeout: no Codex review found after {wait}s")
                return 1

        return 0

    # Validate: exactly one TUI mode required
    modes = []
    if base:
        modes.append("base")
    if uncommitted:
        modes.append("uncommitted")
    if commit:
        modes.append("commit")
    if custom:
        modes.append("custom")

    if len(modes) == 0:
        print("Error: Must specify one of --base, --uncommitted, --commit, --custom, or --pr", file=sys.stderr)
        return 1
    if len(modes) > 1:
        print(f"Error: Modes are mutually exclusive. Got: {', '.join(modes)}", file=sys.stderr)
        return 1

    # Determine mode string
    mode = modes[0]
    if mode == "base":
        mode = "branch"

    # Validation: --new requires parent session context
    if new:
        if not parent_session_id:
            print("Error: --new requires session context (CLAUDE_SESSION_MANAGER_ID must be set)", file=sys.stderr)
            return 1
    elif not session:
        print("Error: Must specify a session or use --new", file=sys.stderr)
        return 1

    # Default --wait to 600 when caller has session context
    if wait is None and parent_session_id:
        wait = 600

    if new:
        # Spawn and review
        result = client.spawn_review(
            parent_session_id=parent_session_id,
            mode=mode,
            base_branch=base,
            commit_sha=commit,
            custom_prompt=custom,
            steer=steer,
            name=name,
            wait=wait,
            model=model,
            working_dir=working_dir,
        )

        if result is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2

        if result.get("error") or result.get("detail"):
            print(f"Error: {result.get('error') or result.get('detail')}", file=sys.stderr)
            return 1

        child_id = result.get("session_id", "unknown")
        child_name = result.get("friendly_name") or result.get("name", child_id)
        print(f"Review started on {child_name} ({child_id}) — mode={mode}")
        if wait:
            print(f"  Watching for completion (timeout={wait}s)")
        return 0
    else:
        # Resolve session
        session_id, session_info = resolve_session_id(client, session)
        if session_id is None:
            sessions = client.list_sessions()
            if sessions is None:
                print("Error: Session manager unavailable", file=sys.stderr)
                return 2
            else:
                print(f"Error: Session '{session}' not found", file=sys.stderr)
                return 1

        result = client.start_review(
            session_id=session_id,
            mode=mode,
            base_branch=base,
            commit_sha=commit,
            custom_prompt=custom,
            steer=steer,
            wait=wait,
            watcher_session_id=parent_session_id,
        )

        if result is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2

        if result.get("error") or result.get("detail"):
            print(f"Error: {result.get('error') or result.get('detail')}", file=sys.stderr)
            return 1

        session_name = session_info.get("friendly_name") or session_info.get("name") or session_id
        print(f"Review started on {session_name} ({session_id}) — mode={mode}")
        if steer:
            print(f"  Steer queued: {steer[:60]}...")
        if wait:
            print(f"  Watching for completion (timeout={wait}s)")
        return 0


def _wait_for_claude_prompt(
    tmux_session: str, timeout: float = 3.0, poll_interval: float = 0.1
) -> bool:
    """Poll capture-pane until Claude Code shows bare '>' prompt, or timeout.

    Blocking version for synchronous callers (e.g. cmd_clear).
    Returns True if prompt detected, False if timed out (caller proceeds anyway).
    """
    import subprocess
    import time

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            result = subprocess.run(
                ["tmux", "capture-pane", "-p", "-t", tmux_session],
                capture_output=True,
                text=True,
                timeout=2,
            )
            if result.returncode == 0:
                output = result.stdout.rstrip('\n')
                if output:
                    last_line = output.split('\n')[-1]
                    if last_line.rstrip() == '>':
                        return True
        except Exception:
            pass
        time.sleep(poll_interval)
    return False


def cmd_clear(
    client: SessionManagerClient,
    requester_session_id: Optional[str],
    target_identifier: str,
    new_prompt: Optional[str] = None,
) -> int:
    """
    Send /clear to a child Claude Code session to reset its context.
    For Codex CLI sessions, sends /new instead.
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

    provider = session.get("provider", "claude")
    if provider == "codex-app":
        success, unavailable = client.clear_session(target_session_id, new_prompt)
        if unavailable:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        if not success:
            print("Error: Failed to clear Codex app session", file=sys.stderr)
            return 1
        name = session.get("friendly_name") or session.get("name") or target_session_id
        if new_prompt:
            print(f"Cleared {name} ({target_session_id}) and sent new prompt")
        else:
            print(f"Cleared {name} ({target_session_id})")
        return 0

    # Extract tmux session name
    tmux_session = session.get("tmux_session")
    if not tmux_session:
        print(f"Error: Session {target_session_id} has no tmux session", file=sys.stderr)
        return 1

    clear_command = "/new" if provider == "codex" else "/clear"

    # Invalidate server-side caches and arm skip_count BEFORE tmux operations,
    # so the /clear Stop hook is absorbed even if it arrives late (#174).
    success, unavailable = client.invalidate_cache(target_session_id)
    if not success:
        if unavailable:
            print(
                f"Warning: Cache invalidation SKIPPED for {target_session_id}: server unavailable. "
                f"Skip fence not armed — stale stop notification possible if server recovers.",
                file=sys.stderr,
            )
        else:
            print(
                f"Warning: Cache invalidation failed for {target_session_id}; "
                f"stale output may affect next notification",
                file=sys.stderr,
            )

    # Send clear command
    try:
        # Check if session is in "completed" state
        # If so, we need to wake it up first (send Enter) before /clear will work
        completion_status = session.get("completion_status")
        if completion_status == "completed":
            # Wake up the session by sending Enter
            subprocess.run(
                ["tmux", "send-keys", "-t", tmux_session, "Enter"],
                check=True,
                capture_output=True,
                text=True,
            )
            # Wait for Claude to show prompt after wake-up (#175)
            _wait_for_claude_prompt(tmux_session)

        # First, send ESC to interrupt any ongoing stream
        subprocess.run(
            ["tmux", "send-keys", "-t", tmux_session, "Escape"],
            check=True,
            capture_output=True,
            text=True,
        )

        # Wait for Claude to show idle prompt before sending payload (#175)
        _wait_for_claude_prompt(tmux_session)

        # Send clear command, then Enter as a separate call after a settle delay (#178).
        # Sending text+"\r" atomically fails because Claude Code (Node.js TUI in raw
        # mode) treats the rapid burst as pasted text, in which \r is literal, not submit.
        subprocess.run(
            ["tmux", "send-keys", "-t", tmux_session, "--", clear_command],
            check=True,
            capture_output=True,
            text=True,
        )
        time.sleep(_SEND_KEYS_SETTLE_SECONDS)  # Allow paste mode to end before Enter arrives
        subprocess.run(
            ["tmux", "send-keys", "-t", tmux_session, "Enter"],
            check=True,
            capture_output=True,
            text=True,
        )

        # Wait for clear to finish and prompt to reappear (#175)
        _wait_for_claude_prompt(tmux_session, timeout=5.0)

        # Send new prompt if provided
        if new_prompt:
            subprocess.run(
                ["tmux", "send-keys", "-t", tmux_session, "--", new_prompt],
                check=True,
                capture_output=True,
                text=True,
            )
            time.sleep(_SEND_KEYS_SETTLE_SECONDS)  # Allow paste mode to end before Enter arrives
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

        # Best-effort clear of stale agent status for codex tmux sessions,
        # which have no context_reset hook (#283). Non-critical — /new already succeeded.
        if provider == "codex":
            client.clear_agent_status(target_session_id)

        return 0

    except subprocess.CalledProcessError as e:
        print(f"Error: Failed to send clear command: {e.stderr}", file=sys.stderr)
        return 1
    except FileNotFoundError:
        print("Error: tmux not found. Is tmux installed?", file=sys.stderr)
        return 1


def cmd_handoff(client: SessionManagerClient, session_id: str, file_path: str) -> int:
    """
    Schedule a self-directed context rotation via handoff doc.

    Agent writes handoff state to a doc, then calls sm handoff. The server
    executes /clear + prompt injection after the current turn completes (Stop hook).

    Args:
        client: API client
        session_id: Current session ID (from CLAUDE_SESSION_MANAGER_ID)
        file_path: Path to the handoff document

    Exit codes:
        0: Handoff scheduled successfully
        1: File not found or server rejected
        2: Session manager unavailable or no session context
    """
    # 1. Verify session_id is set (handoff is self-only)
    if not session_id:
        print(
            "Error: CLAUDE_SESSION_MANAGER_ID not set. sm handoff can only be called from within a session.",
            file=sys.stderr,
        )
        return 2

    # 2. Resolve file_path to absolute path
    abs_path = os.path.abspath(file_path)

    # 3. Verify file exists
    if not os.path.isfile(abs_path):
        print(f"Error: File not found: {abs_path}", file=sys.stderr)
        return 1

    # 4. Call server API
    result = client.schedule_handoff(session_id, abs_path)
    if result is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2
    if result.get("error") or result.get("detail"):
        print(f"Error: {result.get('error') or result.get('detail')}", file=sys.stderr)
        return 1

    print("Handoff scheduled — will execute after current turn completes")
    return 0


def cmd_task_complete(client: SessionManagerClient, session_id: str) -> int:
    """
    Signal that the calling agent has completed its task.

    Cancels the periodic remind and parent-wake loop for this session, then
    sends a one-time important notification to the dispatching EM.

    Args:
        client: API client
        session_id: Current session ID (from CLAUDE_SESSION_MANAGER_ID)

    Exit codes:
        0: Task marked complete
        1: Server rejected the request
        2: Session manager unavailable or no session context
    """
    if not session_id:
        print(
            "Error: CLAUDE_SESSION_MANAGER_ID not set. sm task-complete can only be called from within a session.",
            file=sys.stderr,
        )
        return 2

    success, unavailable, em_notified = client.task_complete(session_id)

    if unavailable:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2
    if not success:
        print("Error: Failed to mark task complete", file=sys.stderr)
        return 1

    if em_notified:
        print("Task complete. Remind cancelled. EM notified.")
    else:
        print("Task complete. Remind cancelled. (No EM registered — no notification sent.)")
    return 0


def cmd_context_monitor(
    client: SessionManagerClient,
    session_id: Optional[str],
    action: str,
    target: Optional[str],
) -> int:
    """
    Enable, disable, or show status for context monitoring.

    Args:
        client: API client
        session_id: Caller's session ID (from CLAUDE_SESSION_MANAGER_ID)
        action: "enable", "disable", or "status"
        target: Optional target session ID; defaults to self when action is enable/disable
    """
    if action == "status":
        monitored = client.get_context_monitor_status()
        if monitored is None:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        if not monitored:
            print("No sessions currently registered for context monitoring.")
            return 0
        print(f"{'Session':<12} {'Name':<24} {'Notify Target'}")
        print("-" * 52)
        for entry in monitored:
            name = entry.get("friendly_name") or ""
            notify = entry.get("notify_session_id") or "(none)"
            print(f"{entry['session_id']:<12} {name:<24} {notify}")
        return 0

    if action in ("enable", "disable"):
        # enable/disable require being inside a managed session (need session_id as requester)
        if not session_id:
            print(
                "Error: sm context-monitor enable/disable requires a managed session "
                "(CLAUDE_SESSION_MANAGER_ID not set)",
                file=sys.stderr,
            )
            return 2

        # Determine target session
        resolved_target = target or session_id

        enabled = (action == "enable")
        # notify_session_id: when enabling, notify the CALLER (self), not the target
        notify_session_id = session_id if enabled else None

        data, success, unavailable = client.set_context_monitor(
            resolved_target, enabled, session_id, notify_session_id=notify_session_id
        )
        if unavailable:
            print("Error: Session manager unavailable", file=sys.stderr)
            return 2
        if not success:
            err = (data or {}).get("detail", "Unknown error")
            print(f"Error: {err}", file=sys.stderr)
            return 1

        if enabled:
            if target and target != session_id:
                print(f"Context monitoring enabled for {target} — notifications → {session_id}")
            else:
                print(f"Context monitoring enabled — notifications → self ({session_id})")
        else:
            print(f"Context monitoring disabled for {resolved_target}")
        return 0

    print(f"Error: Unknown action '{action}'. Use: enable, disable, status", file=sys.stderr)
    return 1


def cmd_em(
    client: SessionManagerClient,
    session_id: Optional[str],
    name_suffix: Optional[str],
) -> int:
    """
    EM pre-flight: set name, enable context monitoring for self and all children,
    register periodic remind for all children.

    Args:
        client: API client
        session_id: Caller's session ID (must be set)
        name_suffix: Optional suffix for friendly name (sets to em-<suffix>)

    Exit codes:
        0: Success (even if some child steps partially failed)
        1: Invalid name
        2: Session manager unavailable or no session ID
    """
    if not session_id:
        print("Error: sm em requires a managed session (CLAUDE_SESSION_MANAGER_ID not set)", file=sys.stderr)
        return 2

    results = []
    child_success = 0
    child_fail = 0

    # Step 1: Validate and set name
    friendly_name = f"em-{name_suffix}" if name_suffix else "em"
    valid, error = validate_friendly_name(friendly_name)
    if not valid:
        print(f"Error: {error}", file=sys.stderr)
        return 1

    success, unavailable = client.update_friendly_name(session_id, friendly_name)
    if unavailable:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2
    if success:
        results.append(f"  Name set: {friendly_name} ({session_id})")
    else:
        results.append(f"  Warning: Failed to set name to {friendly_name}")

    # Step 1b: Register EM role server-side (#256)
    success, unavailable = client.set_em_role(session_id)
    if unavailable:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2
    if success:
        results.append("  EM role: registered")
    else:
        results.append("  Warning: Failed to register EM role")

    # Step 2: Enable self context-monitoring
    data, success, unavailable = client.set_context_monitor(
        session_id, enabled=True, requester_session_id=session_id,
        notify_session_id=session_id,
    )
    if unavailable:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2
    if success:
        results.append("  Context monitoring: enabled (notifications → self)")
    else:
        results.append("  Warning: Failed to enable self context monitoring")

    # Step 3: List and register children
    children_data = client.list_children(session_id)
    if children_data is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    children = children_data.get("children", [])

    if not children:
        results.append("  No existing children found.")
    else:
        child_lines = []
        for child in children:
            child_id = child["id"]
            child_name = child.get("friendly_name") or child.get("name") or child_id
            line_parts = []
            ok = True

            _, cm_ok, _ = client.set_context_monitor(
                child_id, enabled=True, requester_session_id=session_id,
                notify_session_id=session_id,
            )
            if cm_ok:
                line_parts.append("context monitoring enabled")
            else:
                line_parts.append("Warning: context monitoring failed")
                ok = False

            # Fixed policy: matches server defaults (soft=180s, hard=soft+gap=180+120=300s).
            # Client cannot read server config — sm#233 spec / Remind Threshold Policy.
            remind_result = client.register_remind(child_id, soft_threshold=180, hard_threshold=300)
            if remind_result is not None:
                line_parts.append("remind registered (soft=180s, hard=300s)")
            else:
                line_parts.append("remind registration failed")
                ok = False

            if ok:
                child_success += 1
            else:
                child_fail += 1
            child_lines.append(f"    {child_name} ({child_id}) → {'; '.join(line_parts)}")

        results.append(f"  Children processed: {len(children)} ({child_success} succeeded, {child_fail} failed)")
        results.extend(child_lines)

    print("EM pre-flight complete:")
    for line in results:
        print(line)
    return 0


def cmd_setup(overwrite: bool = False) -> int:
    """Copy default dispatch templates to ~/.sm/dispatch_templates.yaml.

    Installs the bundled default_dispatch_templates.yaml to the user's global
    ~/.sm/dispatch_templates.yaml. Never overwrites an existing file unless
    overwrite=True.

    Args:
        overwrite: If True, replace an existing file. Default False.

    Returns:
        0 on success, 1 on error.
    """
    import shutil
    from pathlib import Path

    src = Path(__file__).parent / "default_dispatch_templates.yaml"
    dest = Path.home() / ".sm" / "dispatch_templates.yaml"

    if not src.is_file():
        print(f"Error: Default template file not found: {src}", file=sys.stderr)
        return 1

    if dest.exists() and not overwrite:
        print(f"Templates already installed at {dest}")
        print("Use --overwrite to replace.")
        return 0

    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, dest)
    print(f"Installed dispatch templates to {dest}")
    return 0
