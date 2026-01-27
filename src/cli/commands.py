"""Command implementations for sm CLI."""

import sys
from typing import Optional

from .client import SessionManagerClient
from .formatting import format_session_line, format_relative_time, format_status_list
from ..lock_manager import LockManager


def cmd_name(client: SessionManagerClient, session_id: str, friendly_name: str) -> int:
    """
    Set friendly name for current session.

    Exit codes:
        0: Success
        1: Failed to set
        2: Session manager unavailable
    """
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
        and s["status"] in ["running", "waiting_permission"]
    ]

    if not others:
        return 1  # Silent exit, no other agents

    for session in others:
        print(format_session_line(session))

    return 0


def cmd_what(client: SessionManagerClient, target_session_id: str, lines: int) -> int:
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
                and s["status"] in ["running", "waiting_permission"]
            ]
        else:
            others = [
                s for s in sessions
                if s["id"] != session_id
                and s.get("git_remote_url") == git_remote
                and s["status"] in ["running", "waiting_permission"]
            ]
    else:
        # Match by working_dir (same workspace)
        working_dir = current["working_dir"]
        others = [
            s for s in sessions
            if s["id"] != session_id
            and s["working_dir"] == working_dir
            and s["status"] in ["running", "waiting_permission"]
        ]

    if not others:
        return 1  # Silent exit

    # Get summaries for each
    for session in others:
        summary = client.get_summary(session["id"], lines=100)
        print(format_session_line(session, show_summary=True, summary=summary))
        print()  # Blank line between sessions

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
        and s["status"] in ["running", "waiting_permission"]
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
