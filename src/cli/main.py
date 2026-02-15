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

    # sm name <friendly-name> OR sm name <session> <friendly-name>
    name_parser = subparsers.add_parser("name", help="Set friendly name for self or a child session")
    name_parser.add_argument("name_or_session", help="Name for self, or session identifier to rename a child")
    name_parser.add_argument("new_name", nargs="?", help="New name when renaming a child session")

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

    # sm all
    all_parser = subparsers.add_parser("all", help="List all sessions system-wide")
    all_parser.add_argument("--summaries", action="store_true", help="Include AI-generated summaries")

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

    # sm send <session-id> "<text>"
    send_parser = subparsers.add_parser("send", help="Send input to a session")
    send_parser.add_argument("session_id", help="Target session ID")
    send_parser.add_argument("text", help="Text to send to the session")
    send_parser.add_argument("--sequential", action="store_true", help="Wait for idle before sending (default)")
    send_parser.add_argument("--important", action="store_true", help="Inject immediately, queue behind current work")
    send_parser.add_argument("--urgent", action="store_true", help="Interrupt immediately")
    send_parser.add_argument("--wait", type=int, metavar="SECONDS", help="Notify sender N seconds after delivery if recipient is idle")
    send_parser.add_argument("--steer", action="store_true", help="Inject via Enter-based mid-turn steering (for Codex reviews)")
    send_parser.add_argument("--no-notify-on-stop", action="store_true", help="Don't notify sender when receiver's Stop hook fires")

    # sm wait <session-id> <seconds>
    wait_parser = subparsers.add_parser("wait", help="Wait for session to go idle (or timeout)")
    wait_parser.add_argument("session_id", help="Session ID to monitor")
    wait_parser.add_argument("seconds", type=int, help="Maximum seconds to wait")

    # sm spawn "<prompt>"
    spawn_parser = subparsers.add_parser("spawn", help="Spawn a child agent session")
    spawn_parser.add_argument(
        "provider",
        choices=["claude", "codex", "codex-app"],
        help="Provider for the child session",
    )
    spawn_parser.add_argument("prompt", help="Initial prompt for the child agent")
    spawn_parser.add_argument("--name", help="Friendly name for the child session")
    spawn_parser.add_argument("--wait", type=int, metavar="SECONDS", help="Monitor child and notify when complete or idle for N seconds")
    spawn_parser.add_argument("--model", choices=["opus", "sonnet", "haiku"], help="Override default model")
    spawn_parser.add_argument("--working-dir", help="Override working directory (defaults to parent's directory)")
    spawn_parser.add_argument("--json", action="store_true", help="Output JSON")

    # sm children [session-id]
    children_parser = subparsers.add_parser("children", help="List child sessions")
    children_parser.add_argument("session_id", nargs="?", help="Parent session ID (defaults to current)")
    children_parser.add_argument("--recursive", action="store_true", help="Include grandchildren")
    children_parser.add_argument("--status", choices=["running", "completed", "error", "all"], help="Filter by status")
    children_parser.add_argument("--json", action="store_true", help="Output JSON")

    # sm kill <session-id>
    kill_parser = subparsers.add_parser("kill", help="Terminate a child session")
    kill_parser.add_argument("session_id", help="Session ID to terminate")

    # sm claude [working_dir]
    parser_claude = subparsers.add_parser(
        "claude",
        help="Create a new Claude session and attach to it"
    )
    parser_claude.add_argument(
        "working_dir",
        nargs="?",
        help="Working directory (defaults to current directory)"
    )

    # sm codex [working_dir]
    parser_codex = subparsers.add_parser(
        "codex",
        help="Create a new Codex session and attach to it"
    )
    parser_codex.add_argument(
        "working_dir",
        nargs="?",
        help="Working directory (defaults to current directory)"
    )

    # sm codex-app [working_dir]
    parser_codex_app = subparsers.add_parser(
        "codex-app",
        aliases=["codex-server"],
        help="Create a new Codex app-server session (headless)"
    )
    parser_codex_app.add_argument(
        "working_dir",
        nargs="?",
        help="Working directory (defaults to current directory)"
    )

    # sm new (deprecated alias)
    parser_new = subparsers.add_parser(
        "new",
        help="Create a new Claude session (deprecated: use `sm claude` or `sm codex`)"
    )
    parser_new.add_argument(
        "working_dir",
        nargs="?",
        help="Working directory (defaults to current directory)"
    )

    # sm attach [session]
    parser_attach = subparsers.add_parser(
        "attach",
        help="Attach to an existing session"
    )
    parser_attach.add_argument(
        "session",
        nargs="?",
        help="Session ID or friendly name (shows menu if omitted)"
    )

    # sm output <session> [--lines N]
    parser_output = subparsers.add_parser(
        "output",
        help="View recent tmux output from a session"
    )
    parser_output.add_argument(
        "session",
        help="Session ID or friendly name"
    )
    parser_output.add_argument(
        "--lines",
        type=int,
        default=30,
        help="Number of lines to capture (default: 30)"
    )

    # sm clear <session> [prompt]
    parser_clear = subparsers.add_parser(
        "clear",
        help="Send /clear to reset session context"
    )
    parser_clear.add_argument(
        "session",
        help="Session ID or friendly name"
    )
    parser_clear.add_argument(
        "prompt",
        nargs="?",
        help="Optional new prompt to send after clearing"
    )

    # sm review [session] --base|--uncommitted|--commit|--custom [options]
    review_parser = subparsers.add_parser("review", help="Start a Codex code review")
    review_parser.add_argument("session", nargs="?", help="Session ID or name to review on")
    review_parser.add_argument("--base", help="Review against this base branch")
    review_parser.add_argument("--uncommitted", action="store_true", help="Review uncommitted changes")
    review_parser.add_argument("--commit", help="Review a specific commit SHA")
    review_parser.add_argument("--custom", help="Custom review instructions")
    review_parser.add_argument("--new", action="store_true", help="Spawn a new session for the review")
    review_parser.add_argument("--name", help="Friendly name (with --new)")
    review_parser.add_argument("--wait", type=int, default=None, help="Notify when review completes (seconds; defaults to 600 when in managed session)")
    review_parser.add_argument("--model", help="Model override (with --new)")
    review_parser.add_argument("--working-dir", help="Working directory (with --new)")
    review_parser.add_argument("--steer", help="Instructions to inject after review starts")
    review_parser.add_argument("--pr", type=int, help="PR number to review (Phase 1b)")
    review_parser.add_argument("--repo", help="Repository for PR review (Phase 1b)")

    args = parser.parse_args()

    # Check for CLAUDE_SESSION_MANAGER_ID
    session_id = os.environ.get("CLAUDE_SESSION_MANAGER_ID")
    # Commands that don't need session_id: lock, unlock, hooks, all, send, wait, what, subagents, children, kill, new, attach, output, clear
    no_session_needed = [
        "lock", "unlock", "subagent-start", "subagent-stop", "all", "send", "wait", "what",
        "subagents", "children", "kill", "new", "claude", "codex", "codex-app", "codex-server",
        "attach", "output", "clear", "review", None
    ]
    # Commands that require session_id: spawn (needs to set parent_session_id)
    requires_session_id = ["spawn"]
    if not session_id and args.command in requires_session_id:
        print("Error: CLAUDE_SESSION_MANAGER_ID environment variable not set", file=sys.stderr)
        print("This tool must be run inside a Claude Code session managed by Session Manager", file=sys.stderr)
        sys.exit(2)
    if not session_id and args.command not in no_session_needed and args.command not in requires_session_id:
        print("Error: CLAUDE_SESSION_MANAGER_ID environment variable not set", file=sys.stderr)
        print("This tool must be run inside a Claude Code session managed by Session Manager", file=sys.stderr)
        sys.exit(2)

    # Create client
    client = SessionManagerClient()

    # Dispatch to command handler
    if args.command == "name":
        sys.exit(commands.cmd_name(client, session_id, args.name_or_session, args.new_name))
    elif args.command == "me":
        sys.exit(commands.cmd_me(client, session_id))
    elif args.command == "who":
        sys.exit(commands.cmd_who(client, session_id))
    elif args.command == "what":
        sys.exit(commands.cmd_what(client, args.session_id, args.lines, args.deep))
    elif args.command == "others":
        sys.exit(commands.cmd_others(client, session_id, args.repo))
    elif args.command == "all":
        sys.exit(commands.cmd_all(client, args.summaries))
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
    elif args.command == "send":
        # Determine delivery mode (precedence: urgent > important > steer > sequential)
        delivery_mode = "sequential"  # default
        if args.urgent:
            delivery_mode = "urgent"
        elif args.important:
            delivery_mode = "important"
        elif args.steer:
            delivery_mode = "steer"
        # Extract wait parameter
        wait_seconds = args.wait if hasattr(args, 'wait') else None
        # notify_on_stop defaults to True unless --no-notify-on-stop is passed
        notify_on_stop = not getattr(args, 'no_notify_on_stop', False)
        sys.exit(commands.cmd_send(client, args.session_id, args.text, delivery_mode, wait_seconds=wait_seconds, notify_on_stop=notify_on_stop))
    elif args.command == "wait":
        sys.exit(commands.cmd_wait(client, args.session_id, args.seconds))
    elif args.command == "spawn":
        sys.exit(commands.cmd_spawn(client, session_id, args.provider, args.prompt, args.name, args.wait, args.model, args.working_dir, args.json))
    elif args.command == "children":
        # Use current session if not specified
        parent_id = args.session_id if args.session_id else session_id
        sys.exit(commands.cmd_children(client, parent_id, args.recursive, args.status, args.json))
    elif args.command == "kill":
        sys.exit(commands.cmd_kill(client, session_id, args.session_id))
    elif args.command == "claude":
        sys.exit(commands.cmd_new(client, args.working_dir, provider="claude"))
    elif args.command == "codex":
        sys.exit(commands.cmd_new(client, args.working_dir, provider="codex"))
    elif args.command in ("codex-app", "codex-server"):
        sys.exit(commands.cmd_new(client, args.working_dir, provider="codex-app"))
    elif args.command == "new":
        sys.exit(commands.cmd_new(client, args.working_dir, provider="claude"))
    elif args.command == "attach":
        sys.exit(commands.cmd_attach(client, args.session))
    elif args.command == "output":
        sys.exit(commands.cmd_output(client, args.session, args.lines))
    elif args.command == "clear":
        sys.exit(commands.cmd_clear(client, session_id, args.session, args.prompt))
    elif args.command == "review":
        sys.exit(commands.cmd_review(
            client,
            parent_session_id=session_id,
            session=args.session,
            base=args.base,
            uncommitted=args.uncommitted,
            commit=args.commit,
            custom=args.custom,
            new=args.new,
            name=args.name,
            wait=args.wait,
            model=args.model,
            working_dir=getattr(args, 'working_dir', None),
            steer=args.steer,
            pr=args.pr,
            repo=args.repo,
        ))
    else:
        parser.print_help()
        sys.exit(0)


if __name__ == "__main__":
    main()
