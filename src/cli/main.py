"""Main entry point for sm CLI tool."""

import argparse
import sys
import os

from .client import SessionManagerClient
from . import commands


def main():
    """Main entry point for sm CLI."""
    parser = argparse.ArgumentParser(
        prog="sm",
        description="Session Manager CLI - coordinate multiple Claude agents",
    )

    subparsers = parser.add_subparsers(dest="command", help="Command to execute")

    # sm name <friendly-name>
    name_parser = subparsers.add_parser("name", help="Set friendly name for this session")
    name_parser.add_argument("friendly_name", help="Friendly name (e.g., 'scout-epic987')")

    # sm me
    subparsers.add_parser("me", help="Show current session info")

    # sm who
    subparsers.add_parser("who", help="List other active sessions in this workspace")

    # sm what <session-id>
    what_parser = subparsers.add_parser("what", help="Get summary of what a session is doing")
    what_parser.add_argument("session_id", help="Session ID")
    what_parser.add_argument("--lines", type=int, default=100, help="Lines to analyze (default: 100)")
    what_parser.add_argument("--deep", action="store_true", help="Include subagent activity")

    # sm others
    others_parser = subparsers.add_parser("others", help="List others + what they're doing")
    others_parser.add_argument("--repo", action="store_true", help="Include sessions in other worktrees of same repo")

    # sm alone
    subparsers.add_parser("alone", help="Check if you're the only active agent (for scripting)")

    # sm task "<description>"
    task_parser = subparsers.add_parser("task", help="Register what you're working on")
    task_parser.add_argument("description", help="Task description")

    # sm lock "<description>"
    lock_parser = subparsers.add_parser("lock", help="Acquire workspace lock (fallback)")
    lock_parser.add_argument("description", help="Lock description")

    # sm unlock
    subparsers.add_parser("unlock", help="Release workspace lock")

    # sm status
    subparsers.add_parser("status", help="Full status: you + others + lock")

    # sm subagent-start (called by SubagentStart hook)
    subparsers.add_parser("subagent-start", help="Register subagent start (called by hook)")

    # sm subagent-stop (called by SubagentStop hook)
    subparsers.add_parser("subagent-stop", help="Register subagent stop (called by hook)")

    # sm subagents <session-id>
    subagents_parser = subparsers.add_parser("subagents", help="List subagents spawned by a session")
    subagents_parser.add_argument("session_id", help="Session ID")

    args = parser.parse_args()

    # Check for CLAUDE_SESSION_MANAGER_ID
    session_id = os.environ.get("CLAUDE_SESSION_MANAGER_ID")
    if not session_id and args.command not in ["lock", "unlock", "subagent-start", "subagent-stop", None]:
        print("Error: CLAUDE_SESSION_MANAGER_ID environment variable not set", file=sys.stderr)
        print("This tool must be run inside a Claude Code session managed by Session Manager", file=sys.stderr)
        sys.exit(2)

    # Create client
    client = SessionManagerClient()

    # Dispatch to command handler
    if args.command == "name":
        sys.exit(commands.cmd_name(client, session_id, args.friendly_name))
    elif args.command == "me":
        sys.exit(commands.cmd_me(client, session_id))
    elif args.command == "who":
        sys.exit(commands.cmd_who(client, session_id))
    elif args.command == "what":
        sys.exit(commands.cmd_what(client, args.session_id, args.lines, args.deep))
    elif args.command == "others":
        sys.exit(commands.cmd_others(client, session_id, args.repo))
    elif args.command == "alone":
        sys.exit(commands.cmd_alone(client, session_id))
    elif args.command == "task":
        sys.exit(commands.cmd_task(client, session_id, args.description))
    elif args.command == "lock":
        sys.exit(commands.cmd_lock(session_id, args.description))
    elif args.command == "unlock":
        sys.exit(commands.cmd_unlock(session_id))
    elif args.command == "status":
        sys.exit(commands.cmd_status(client, session_id))
    elif args.command == "subagent-start":
        sys.exit(commands.cmd_subagent_start(client, session_id))
    elif args.command == "subagent-stop":
        sys.exit(commands.cmd_subagent_stop(client, session_id))
    elif args.command == "subagents":
        sys.exit(commands.cmd_subagents(client, args.session_id))
    else:
        parser.print_help()
        sys.exit(0)


if __name__ == "__main__":
    main()
