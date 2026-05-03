"""Main entry point for sm CLI tool."""

import argparse
import sys
import os
from typing import Optional

from .client import SessionManagerClient
from . import commands


def _looks_like_int_token(token: str) -> bool:
    """Return True when one argv token can be parsed as an integer."""
    try:
        int(token)
    except (TypeError, ValueError):
        return False
    return True


def _normalize_optional_track_args(argv: list[str]) -> list[str]:
    """
    Rewrite explicit `--track` integer values before argparse runs.

    Bare `--track` keeps the default 300-second behavior. Explicit integer forms
    (`--track 420` and `--track=420`) are rewritten to a hidden internal flag so
    argparse does not greedily consume following positional arguments.
    """
    if not argv:
        return argv

    if argv[0] not in {"send", "spawn"}:
        return argv

    normalized: list[str] = [argv[0]]
    index = 1
    while index < len(argv):
        token = argv[index]
        if token == "--":
            normalized.extend(argv[index:])
            break
        if token == "--track":
            next_token = argv[index + 1] if index + 1 < len(argv) else None
            if next_token is not None and _looks_like_int_token(next_token):
                normalized.extend(["--track-seconds", next_token])
                index += 2
                continue
            normalized.append(token)
            index += 1
            continue
        if token.startswith("--track="):
            maybe_value = token.split("=", 1)[1]
            if _looks_like_int_token(maybe_value):
                normalized.extend(["--track-seconds", maybe_value])
                index += 1
                continue
        normalized.append(token)
        index += 1

    return normalized


def _handle_dispatch(session_id: Optional[str]) -> int:
    """Handle 'sm dispatch' with two-phase argument parsing.

    Intercepts dispatch before the main argparse to support dynamic
    CLI flags derived from role templates. This keeps dynamic parsing
    completely isolated — existing commands retain strict validation.
    """
    from .dispatch import parse_dispatch_args

    agent_id, role, dry_run, no_clear, delivery_mode, notify_on_stop, dynamic_params = \
        parse_dispatch_args(sys.argv[2:])

    # em_id check: required for send mode, placeholder for dry-run
    em_id = session_id
    if not em_id and not dry_run:
        print(
            "Error: CLAUDE_SESSION_MANAGER_ID not set. "
            "Use --dry-run to test templates outside managed sessions.",
            file=sys.stderr,
        )
        return 1

    client = SessionManagerClient()
    return commands.cmd_dispatch(
        client, agent_id, role, dynamic_params, em_id,
        dry_run=dry_run, no_clear=no_clear,
        delivery_mode=delivery_mode, notify_on_stop=notify_on_stop,
    )


def main():
    """Main entry point for sm CLI."""
    # Pre-intercept: dispatch uses two-phase parsing for dynamic flags.
    # Must be handled before parser.parse_args() to avoid rejecting
    # role-specific flags like --issue, --spec, etc.
    if len(sys.argv) >= 2 and sys.argv[1] == "dispatch":
        session_id = os.environ.get("CLAUDE_SESSION_MANAGER_ID")
        sys.exit(_handle_dispatch(session_id))

    parser = argparse.ArgumentParser(
        prog="sm",
        description="Session Manager CLI - coordinate multiple Claude agents",
    )

    subparsers = parser.add_subparsers(dest="command", help="Command to execute")

    # sm dispatch — pre-intercepted above; stub registered here for visibility in sm --help
    subparsers.add_parser(
        "dispatch",
        help="Dispatch a role template to an agent (see .sm/dispatch_templates.yaml)",
    )

    # sm name <friendly-name> OR sm name <session> <friendly-name>
    name_parser = subparsers.add_parser("name", help="Set friendly name for self or a child session")
    name_parser.add_argument("name_or_session", help="Name for self, or session identifier to rename a child")
    name_parser.add_argument("new_name", nargs="?", help="New name when renaming a child session")

    # sm role <role> OR sm role --clear
    role_parser = subparsers.add_parser("role", help="Set or clear role tag for current session")
    role_parser.add_argument("role", nargs="?", default=None, help="Role tag (e.g., engineer)")
    role_parser.add_argument("--clear", action="store_true", help="Clear current role tag")

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

    # sm status [text]
    # With no args: system-wide status display (existing)
    # With text arg: self-report agent status and reset remind timer (#188)
    status_parser = subparsers.add_parser("status", help="Full status: you + others + lock (or report agent status)")
    status_parser.add_argument(
        "text",
        nargs="?",
        default=None,
        help='Self-report status text (e.g., sm status "investigating bug")',
    )

    # sm subagent-start (called by SubagentStart hook)
    subparsers.add_parser("subagent-start", help="Register subagent start (called by hook)")

    # sm subagent-stop (called by SubagentStop hook)
    subparsers.add_parser("subagent-stop", help="Register subagent stop (called by hook)")

    # sm subagents <session-id>
    subagents_parser = subparsers.add_parser("subagents", help="List subagents spawned by a session")
    subagents_parser.add_argument("session_id", help="Session ID")

    # sm send <session-id|human> "<text>"
    send_parser = subparsers.add_parser("send", help="Send input to a session or Telegram to a human recipient")
    send_parser.add_argument("session_id", help="Target session ID, friendly name, human alias, or comma-delimited list")
    send_parser.add_argument("text", help="Text to send")
    send_parser.add_argument("--sequential", action="store_true", help="Wait for idle before sending (default)")
    send_parser.add_argument("--important", action="store_true", help="Inject immediately, queue behind current work")
    send_parser.add_argument("--urgent", action="store_true", help="Interrupt immediately")
    send_parser.add_argument("--wait", type=int, metavar="SECONDS", help="Notify sender N seconds after delivery if recipient is idle")
    send_parser.add_argument("--steer", action="store_true", help="Inject via Enter-based mid-turn steering (for Codex reviews)")
    send_parser.add_argument("--no-notify-on-stop", action="store_true", help="Don't notify sender when receiver's Stop hook fires")
    send_parser.add_argument(
        "--track",
        action="store_const",
        const=300,
        default=None,
        help="Track the recipient with periodic remind until it replies (default: 300s; explicit seconds also supported)",
    )
    send_parser.add_argument("--track-seconds", dest="track", type=int, metavar="SECONDS", help=argparse.SUPPRESS)

    # sm telegram <human> "<text>" / sm tg <human> "<text>"
    telegram_parser = subparsers.add_parser(
        "telegram",
        aliases=["tg"],
        help="Send a Telegram message to a configured human recipient",
    )
    telegram_parser.add_argument("recipient", help="Configured human recipient or alias")
    telegram_parser.add_argument("text", help="Text to post into this session's Telegram topic")

    # sm email <human> "<text>" OR sm email <user[,user2]> --subject "..." [--body ... | --text file | --html file]
    email_parser = subparsers.add_parser("email", help="Send explicit email fallback to a human recipient or registered user")
    email_parser.add_argument("recipients", help="Human/registered user(s), comma-separated")
    email_parser.add_argument("message", nargs="?", help="Inline plain-text body")
    email_parser.add_argument("--subject", help="Email subject line")
    email_parser.add_argument("--body", help="Inline plain-text body")
    email_parser.add_argument("--text", help="Text/Markdown file for the body")
    email_parser.add_argument("--html", help="HTML file for the body")
    email_parser.add_argument("--cc", help="Additional registered user(s), comma-separated")

    # sm remind <delay> <message>              (one-shot self-reminder)
    # sm remind --recurring <delay> <message>  (recurring self-reminder)
    # sm remind cancel <reminder-id>           (cancel scheduled reminder)
    # sm remind <session-id> --stop            (cancel periodic remind)
    remind_parser = subparsers.add_parser(
        "remind",
        help="Schedule a self-reminder, recurring self-reminder, or cancel periodic remind / scheduled reminder",
    )
    remind_parser.add_argument(
        "first_arg",
        nargs="?",
        help="Delay in seconds, 'cancel', or session ID for --stop",
    )
    remind_parser.add_argument(
        "message",
        nargs="*",
        default=[],
        help="Reminder message or cancel target",
    )
    remind_parser.add_argument(
        "--recurring",
        action="store_true",
        help="Repeat the reminder using the same delay interval",
    )
    remind_parser.add_argument(
        "--stop",
        action="store_true",
        help="Cancel periodic remind for the specified session",
    )

    # sm wait <session-id> <seconds>
    wait_parser = subparsers.add_parser("wait", help="Wait for session to go idle (or timeout)")
    wait_parser.add_argument("session_id", help="Session ID to monitor")
    wait_parser.add_argument("seconds", type=int, help="Maximum seconds to wait")

    # sm watch-job add/list/cancel
    watch_job_parser = subparsers.add_parser("watch-job", help="Manage durable external job watches")
    watch_job_subparsers = watch_job_parser.add_subparsers(dest="watch_job_command")

    watch_job_add_parser = watch_job_subparsers.add_parser("add", help="Register a durable external job watch")
    watch_job_add_parser.add_argument("--target", help="Session ID or alias to wake (defaults to current session)")
    watch_job_add_parser.add_argument("--label", help="Human label for notifications")
    watch_job_add_parser.add_argument("--pid", type=int, help="PID to monitor for liveness")
    watch_job_add_parser.add_argument("--file", dest="file_path", help="Output/log file to inspect")
    watch_job_add_parser.add_argument("--progress-regex", help="Regex used to extract progress lines")
    watch_job_add_parser.add_argument("--done-regex", help="Regex used to detect completion from the file")
    watch_job_add_parser.add_argument("--error-regex", help="Regex used to detect errors from the file")
    watch_job_add_parser.add_argument(
        "--exit-code-file",
        help="File containing final process exit code; useful because SM cannot inspect arbitrary PID exit codes directly",
    )
    watch_job_add_parser.add_argument("--interval", dest="interval_seconds", type=int, default=300, help="Polling interval in seconds")
    watch_job_add_parser.add_argument("--tail-lines", type=int, default=200, help="How many trailing log lines to inspect")
    watch_job_add_parser.add_argument("--tail-on-error", type=int, default=10, help="How many trailing lines to include in error notifications")
    watch_job_add_parser.add_argument("--notify-every-poll", action="store_true", help="Send progress on every poll, not just when it changes")

    watch_job_list_parser = watch_job_subparsers.add_parser("list", help="List durable external job watches")
    watch_job_list_parser.add_argument("--target", help="Filter by target session ID or alias")
    watch_job_list_parser.add_argument("--all", action="store_true", help="List all watches instead of defaulting to current session")
    watch_job_list_parser.add_argument("--json", action="store_true", help="Output JSON")
    watch_job_list_parser.add_argument("--include-inactive", action="store_true", help="Include inactive watches from current process state")

    watch_job_cancel_parser = watch_job_subparsers.add_parser("cancel", help="Cancel a durable external job watch")
    watch_job_cancel_parser.add_argument("watch_id", help="Watch ID to cancel")

    # sm queue run/list/status/cancel
    queue_parser = subparsers.add_parser("queue", help="Manage local queue runner jobs")
    queue_subparsers = queue_parser.add_subparsers(dest="queue_command")

    queue_run_parser = queue_subparsers.add_parser("run", help="Submit a local command to the queue runner")
    queue_run_parser.add_argument("--type", dest="job_type", choices=["tests", "perf", "background"], default="tests")
    queue_run_parser.add_argument("--label", help="Human-readable job label")
    queue_run_parser.add_argument("--cwd", help="Working directory (defaults to current directory)")
    queue_run_parser.add_argument("--timeout", help="Timeout duration, e.g. 90s, 10m, 2h")
    queue_run_parser.add_argument("--env", dest="env_pairs", action="append", default=[], help="Environment override KEY=VALUE")
    queue_run_parser.add_argument("--notify", help="Session ID or registry role to notify")
    queue_run_parser.add_argument("--script-file", help="Script file to run, or - for stdin")
    queue_run_parser.add_argument("queue_argv", nargs=argparse.REMAINDER, help="Command argv after --")

    queue_list_parser = queue_subparsers.add_parser("list", help="List queue runner jobs")
    queue_list_parser.add_argument("--notify", help="Filter by notify session ID or registry role")
    queue_list_parser.add_argument("--all", action="store_true", help="List all jobs instead of current session active jobs")
    queue_list_parser.add_argument("--type", dest="job_type", choices=["tests", "perf", "background"])
    queue_list_parser.add_argument("--state", choices=["pending", "running", "succeeded", "failed", "cancelled", "timed_out", "displaced", "done"])
    queue_list_parser.add_argument("--json", action="store_true", help="Output JSON")

    queue_status_parser = queue_subparsers.add_parser("status", help="Show one queue runner job")
    queue_status_parser.add_argument("job_id", help="Queue job ID")
    queue_status_parser.add_argument("--json", action="store_true", help="Output JSON")

    queue_cancel_parser = queue_subparsers.add_parser("cancel", help="Cancel one queue runner job")
    queue_cancel_parser.add_argument("job_id", help="Queue job ID")

    queue_ci_run_parser = queue_subparsers.add_parser("ci-run", help="Submit a configured policy-controlled queue run")
    queue_ci_run_parser.add_argument("--policy", required=True, help="Configured queue policy name")
    queue_ci_run_parser.add_argument("--dedupe-token", help="Bounded dedupe token, such as a commit SHA")
    queue_ci_run_parser.add_argument("--label", help="Human-readable run label")
    queue_ci_run_parser.add_argument("--cwd", help="Working directory override")
    queue_ci_run_parser.add_argument("--timeout", help="Timeout duration override, e.g. 90s, 10m, 2h")
    queue_ci_run_parser.add_argument("--type", dest="job_type", choices=["tests", "perf", "background"], help="Queue workload type override")
    queue_ci_run_parser.add_argument("--env", dest="env_pairs", action="append", default=[], help="Environment override KEY=VALUE")
    queue_ci_run_parser.add_argument("--metadata", dest="metadata_pairs", action="append", default=[], help="Metadata KEY=VALUE")
    queue_ci_run_parser.add_argument("--script-file", help="Script file to run, or - for stdin")
    queue_ci_run_parser.add_argument("queue_argv", nargs=argparse.REMAINDER, help="Command argv after --")

    queue_ci_status_parser = queue_subparsers.add_parser("ci-status", help="Show one configured policy-controlled queue run")
    queue_ci_status_parser.add_argument("--policy", required=True, help="Configured queue policy name")
    queue_ci_status_parser.add_argument("--dedupe-token", help="Look up the latest admitted run by token")
    queue_ci_status_parser.add_argument("--id", dest="run_id", help="Look up a specific queue policy run ID")
    queue_ci_status_parser.add_argument("--json", action="store_true", help="Output JSON")

    queue_ci_history_parser = queue_subparsers.add_parser("ci-history", help="List configured policy-controlled queue runs")
    queue_ci_history_parser.add_argument("--policy", required=True, help="Configured queue policy name")
    queue_ci_history_parser.add_argument("--limit", type=int, default=50, help="Maximum runs to show")
    queue_ci_history_parser.add_argument("--include-suppressed", action="store_true", help="Include suppressed run attempts")
    queue_ci_history_parser.add_argument("--json", action="store_true", help="Output JSON")

    # sm request-codex-review <pr>|list|status|cancel
    request_codex_review_parser = subparsers.add_parser(
        "request-codex-review",
        help="Request and durably watch a Codex PR review",
    )
    request_codex_review_parser.add_argument(
        "action_or_pr",
        help="PR number to request, or one of: list, status, cancel",
    )
    request_codex_review_parser.add_argument(
        "request_id",
        nargs="?",
        help="Request ID for status/cancel",
    )
    request_codex_review_parser.add_argument("--repo", help="Repository in owner/repo or host/owner/repo format")
    request_codex_review_parser.add_argument("--notify", help="Session ID or registry role to notify")
    request_codex_review_parser.add_argument("--steer", help="Optional Codex review steer text")
    request_codex_review_parser.add_argument("--all", action="store_true", help="List/status across all notify targets")
    request_codex_review_parser.add_argument(
        "--inactive",
        action="store_true",
        help="Include inactive requests when listing a notify target; --all includes them by default",
    )
    request_codex_review_parser.add_argument("--json", action="store_true", help="Output JSON for list/status")
    request_codex_review_parser.add_argument("--pr", dest="status_pr", type=int, help="Filter status/list by PR number")
    request_codex_review_parser.add_argument("--poll-interval", dest="poll_interval_seconds", type=int, default=30, help="Polling cadence in seconds (default: 30)")
    request_codex_review_parser.add_argument("--retry-interval", dest="retry_interval_seconds", type=int, default=600, help="Re-ping cadence in seconds (default: 600)")

    # sm spawn "<prompt>"
    spawn_parser = subparsers.add_parser("spawn", help="Spawn a child agent session")
    spawn_parser.add_argument(
        "provider",
        choices=["claude", "codex", "codex-fork", "codex-app"],
        help="Provider for the child session",
    )
    spawn_parser.add_argument("prompt", help="Initial prompt for the child agent")
    spawn_parser.add_argument("--name", help="Friendly name for the child session")
    spawn_parser.add_argument("--wait", type=int, metavar="SECONDS", help="Monitor child and notify when complete or idle for N seconds")
    spawn_parser.add_argument(
        "--model",
        help=(
            "Override model (provider-aware: claude accepts opus|sonnet|haiku; "
            "codex/codex-fork/codex-app accept provider model IDs, e.g. codex-5.1)"
        ),
    )
    spawn_parser.add_argument("--working-dir", help="Override working directory (defaults to parent's directory)")
    spawn_parser.add_argument("--json", action="store_true", help="Output JSON")
    spawn_parser.add_argument(
        "--track",
        action="store_const",
        const=300,
        default=None,
        help="Track the child with periodic remind until stopped (default: 300s; explicit seconds also supported)",
    )
    spawn_parser.add_argument("--track-seconds", dest="track", type=int, metavar="SECONDS", help=argparse.SUPPRESS)

    # sm children [session]
    children_parser = subparsers.add_parser("children", help="List child sessions")
    children_parser.add_argument(
        "session_id",
        nargs="?",
        help="Parent session ID, friendly name, or registry alias (defaults to current)",
    )
    children_parser.add_argument("--recursive", action="store_true", help="Include grandchildren")
    children_parser.add_argument("--terminated", action="store_true", help="Include children retired via sm retire/sm kill")
    children_parser.add_argument("--status", choices=["running", "completed", "error", "all"], help="Filter by status")
    children_parser.add_argument("--json", action="store_true", help="Output JSON")
    children_parser.add_argument("--db-path", default=None, help="Override tool_usage.db path")

    # sm retire <session-id> / sm kill <session-id>
    kill_parser = subparsers.add_parser("kill", help="Retire a child session")
    kill_parser.add_argument("session_id", help="Session ID to retire")
    retire_parser = subparsers.add_parser("retire", help="Retire a child session")
    retire_parser.add_argument("session_id", help="Session ID to retire")

    # sm restore <session-id-or-name>
    restore_parser = subparsers.add_parser(
        "restore",
        aliases=["unkill"],
        help="Restore a stopped session",
    )
    restore_parser.add_argument("session", help="Session ID or friendly name to restore")

    # sm clean [--session-id ID ...]
    clean_parser = subparsers.add_parser(
        "clean",
        help="Close Telegram forum topics for idle/completed sessions (sm#271)"
    )
    clean_parser.add_argument(
        "--session-id",
        dest="session_ids",
        action="append",
        metavar="ID",
        help="Specific session ID(s) to clean (repeatable); omit for auto-mode (COMPLETED only)",
    )

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
        help="Create a new Codex session (codex-fork runtime) and attach to it"
    )
    parser_codex.add_argument(
        "working_dir",
        nargs="?",
        help="Working directory (defaults to current directory)"
    )

    # sm codex-legacy [working_dir]
    parser_codex_legacy = subparsers.add_parser(
        "codex-legacy",
        help="Create a legacy Codex tmux session and attach to it"
    )
    parser_codex_legacy.add_argument(
        "working_dir",
        nargs="?",
        help="Working directory (defaults to current directory)"
    )

    # sm codex-fork [working_dir]
    parser_codex_fork = subparsers.add_parser(
        "codex-fork",
        aliases=["codex_fork"],
        help="Create a new Codex-fork session and attach to it"
    )
    parser_codex_fork.add_argument(
        "working_dir",
        nargs="?",
        help="Working directory (defaults to current directory)"
    )

    # sm codex-2 [working_dir]
    parser_codex_2 = subparsers.add_parser(
        "codex-2",
        help="Create a new Codex-fork session and attach via sm attach flow"
    )
    parser_codex_2.add_argument(
        "working_dir",
        nargs="?",
        help="Working directory (defaults to current directory)"
    )

    # sm codex-app [working_dir]
    parser_codex_app = subparsers.add_parser(
        "codex-app",
        help="Create a new Codex app-server session (headless)"
    )
    parser_codex_app.add_argument(
        "working_dir",
        nargs="?",
        help="Working directory (defaults to current directory)"
    )

    # sm codex-server [working_dir] (removed entrypoint)
    parser_codex_server = subparsers.add_parser(
        "codex-server",
        help="Removed entrypoint (use sm codex-app or sm codex)"
    )
    parser_codex_server.add_argument(
        "working_dir",
        nargs="?",
        help="Working directory (ignored; command is removed)"
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

    # sm codex-tui <session> [--poll-interval N] [--event-limit N]
    codex_tui_parser = subparsers.add_parser(
        "codex-tui",
        help="Attach terminal UI for codex-app session state/events/requests",
    )
    codex_tui_parser.add_argument(
        "session",
        help="Session ID or friendly name"
    )
    codex_tui_parser.add_argument(
        "--poll-interval",
        type=float,
        default=1.0,
        help="Refresh interval seconds (default: 1.0)"
    )
    codex_tui_parser.add_argument(
        "--event-limit",
        type=int,
        default=100,
        help="Max event page size per poll (default: 100)"
    )

    # sm codex-fork-info [--json]
    codex_fork_info_parser = subparsers.add_parser(
        "codex-fork-info",
        help="Show codex-fork artifact pinning + schema metadata",
    )
    codex_fork_info_parser.add_argument(
        "--json",
        action="store_true",
        help="Output JSON",
    )

    # sm codex-rollout-gates [--json]
    codex_rollout_gates_parser = subparsers.add_parser(
        "codex-rollout-gates",
        help="Show codex launch/cutover gate status",
    )
    codex_rollout_gates_parser.add_argument(
        "--json",
        action="store_true",
        help="Output JSON",
    )

    # sm watch [--repo PATH] [--role ROLE] [--interval SECONDS]
    watch_parser = subparsers.add_parser(
        "watch",
        help="Interactive dashboard for sessions",
    )
    watch_parser.add_argument(
        "--repo",
        default=None,
        help="Filter sessions by repository/workdir root",
    )
    watch_parser.add_argument(
        "--role",
        default=None,
        help="Filter sessions by role tag",
    )
    watch_parser.add_argument(
        "--interval",
        type=float,
        default=2.0,
        help="Refresh interval seconds (default: 2.0)",
    )
    watch_parser.add_argument(
        "--restore",
        action="store_true",
        help="Browse restorable stopped sessions and restore the selected row",
    )
    watch_parser.add_argument(
        "--top-level",
        action="store_true",
        help="In --restore mode, start with only top-level stopped sessions expanded on demand",
    )
    watch_parser.add_argument(
        "--sort",
        choices=("retired", "last-active", "name"),
        default="retired",
        help="In --restore mode, initial sort order (default: retired)",
    )

    # sm tail <session> [-n N] [--raw] [--db-path PATH]
    tail_parser = subparsers.add_parser(
        "tail",
        help="Show recent agent activity (structured tool log or raw tmux output)"
    )
    tail_parser.add_argument(
        "session",
        help="Session ID or friendly name"
    )
    tail_parser.add_argument(
        "-n",
        type=int,
        default=10,
        help="Number of entries (structured) or lines (raw) to show (default: 10)"
    )
    tail_parser.add_argument(
        "--raw",
        action="store_true",
        help="Show raw tmux pane output with ANSI stripped"
    )
    tail_parser.add_argument(
        "--db-path",
        default=None,
        help="Override tool_usage.db path (default: ~/.local/share/claude-sessions/tool_usage.db)"
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

    # sm handoff <file_path>
    handoff_parser = subparsers.add_parser("handoff", help="Self-directed context rotation via handoff doc")
    handoff_parser.add_argument("file_path", help="Path to handoff document")

    # sm task-complete
    subparsers.add_parser("task-complete", help="Signal task completion: cancels remind + notifies EM")

    # sm turn-complete
    subparsers.add_parser("turn-complete", help="Signal turn completion: cancels periodic remind only")

    # sm context-monitor <enable|disable|status> [session-id]
    ctx_parser = subparsers.add_parser(
        "context-monitor",
        help="Manage context monitoring registration for a session",
    )
    ctx_parser.add_argument(
        "action",
        choices=["enable", "disable", "status"],
        help="enable: opt-in, disable: opt-out, status: list monitored sessions",
    )
    ctx_parser.add_argument(
        "target",
        nargs="?",
        default=None,
        help="Session ID to register/deregister; defaults to self",
    )

    # sm em [name]
    em_parser = subparsers.add_parser(
        "em",
        help="EM pre-flight: set name, enable context monitoring, register children",
    )
    em_parser.add_argument(
        "name",
        nargs="?",
        default=None,
        help="Name suffix (sets friendly name to em-<name>)",
    )

    # sm maintainer [--clear]
    maintainer_parser = subparsers.add_parser(
        "maintainer",
        help="Register this session as the durable maintainer alias",
    )
    maintainer_parser.add_argument(
        "--clear",
        action="store_true",
        help="Clear the maintainer alias from this session",
    )

    # sm register <role>
    register_parser = subparsers.add_parser(
        "register",
        help="Register this session under a durable agent registry role",
    )
    register_parser.add_argument(
        "role",
        help="Registry role to claim (example: reviewer, sm-maintainer)",
    )

    # sm unregister <role>
    unregister_parser = subparsers.add_parser(
        "unregister",
        help="Remove one durable agent registry role from this session",
    )
    unregister_parser.add_argument(
        "role",
        help="Registry role to release",
    )

    # sm lookup <role|human>
    lookup_parser = subparsers.add_parser(
        "lookup",
        help="Resolve a human recipient, durable registry role, or live session",
    )
    lookup_parser.add_argument(
        "role",
        help="Human alias, registry role, or session name to resolve",
    )

    # sm roster
    subparsers.add_parser(
        "roster",
        help="List live durable agent registry roles",
    )

    # sm adopt <session>
    adopt_parser = subparsers.add_parser(
        "adopt",
        help="Propose adopting an existing agent (requires approval in sm watch)",
    )
    adopt_parser.add_argument(
        "session",
        help="Session ID or friendly name to adopt",
    )

    # sm setup [--overwrite]
    setup_parser = subparsers.add_parser(
        "setup",
        help="Install default dispatch templates to ~/.sm/dispatch_templates.yaml",
    )
    setup_parser.add_argument(
        "--overwrite",
        action="store_true",
        help="Replace existing templates file",
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

    args = parser.parse_args(_normalize_optional_track_args(sys.argv[1:]))

    # Check for CLAUDE_SESSION_MANAGER_ID
    session_id = os.environ.get("CLAUDE_SESSION_MANAGER_ID")
    if args.command == "watch" and session_id:
        print(
            "Error: sm watch is operator-only. Run it from a non-managed shell "
            "(without CLAUDE_SESSION_MANAGER_ID).",
            file=sys.stderr,
        )
        sys.exit(1)

    # Commands that don't need session_id: lock, unlock, hooks, all, send, wait, what, subagents, children, kill/retire, restore, new, attach, output, clear
    no_session_needed = [
        "lock", "unlock", "subagent-start", "subagent-stop", "all", "send", "wait", "what",
        "subagents", "children", "kill", "retire", "restore", "unkill", "new", "claude", "codex", "codex-legacy", "codex-fork", "codex_fork",
        "codex-2", "codex-app", "codex-server",
        "attach", "output", "codex-tui", "codex-fork-info", "codex-rollout-gates", "watch", "tail", "clear", "review", "context-monitor", "remind", "setup", "lookup", "roster", "email", "request-codex-review", None
    ]
    # Commands that require session_id: self-directed managed-session actions
    requires_session_id = ["spawn", "adopt", "maintainer", "register", "unregister"]
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
    elif args.command == "role":
        sys.exit(commands.cmd_role(client, session_id, args.role, clear=args.clear))
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
        # With text arg: self-report agent status; without: system status display (#188)
        if getattr(args, "text", None):
            if not session_id:
                print("Error: CLAUDE_SESSION_MANAGER_ID not set (required to report status)", file=sys.stderr)
                sys.exit(2)
            sys.exit(commands.cmd_agent_status(client, session_id, args.text))
        else:
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
        sys.exit(commands.cmd_send(
            client, args.session_id, args.text, delivery_mode,
            wait_seconds=wait_seconds, notify_on_stop=notify_on_stop,
            track_seconds=getattr(args, "track", None),
        ))
    elif args.command in ("telegram", "tg"):
        sys.exit(commands.cmd_telegram(
            client,
            sender_session_id=session_id,
            recipient=args.recipient,
            text=args.text,
        ))
    elif args.command == "email":
        inline_body = getattr(args, "body", None)
        if inline_body is not None and getattr(args, "message", None) is not None:
            print("Error: use either positional message or --body, not both", file=sys.stderr)
            sys.exit(1)
        if inline_body is None:
            inline_body = getattr(args, "message", None)
        sys.exit(commands.cmd_email(
            client,
            sender_session_id=session_id,
            recipients_raw=args.recipients,
            subject=args.subject,
            body=inline_body,
            text_file=getattr(args, "text", None),
            html_file=getattr(args, "html", None),
            cc_raw=getattr(args, "cc", None),
        ))
    elif args.command == "remind":
        if args.stop:
            # sm remind <session-id> --stop: cancel periodic remind (#188)
            if not args.first_arg:
                print("Error: Expected session ID before --stop", file=sys.stderr)
                sys.exit(1)
            sys.exit(commands.cmd_remind_stop(client, args.first_arg))
        elif args.first_arg == "cancel":
            if len(args.message) != 1:
                print("Error: Expected reminder ID after 'cancel'", file=sys.stderr)
                sys.exit(1)
            sys.exit(commands.cmd_cancel_scheduled_reminder(client, args.message[0]))
        else:
            # sm remind <delay> <message>: one-shot or recurring self-reminder
            if not args.first_arg:
                print("Error: Expected delay in seconds", file=sys.stderr)
                sys.exit(1)
            if not session_id:
                print("Error: CLAUDE_SESSION_MANAGER_ID not set (required for self-reminder)", file=sys.stderr)
                sys.exit(2)
            try:
                delay_seconds = int(args.first_arg)
            except (TypeError, ValueError):
                print(f"Error: Expected integer delay (seconds), got: {args.first_arg!r}", file=sys.stderr)
                sys.exit(1)
            message = " ".join(args.message).strip() or "Reminder"
            sys.exit(commands.cmd_remind(
                client,
                session_id,
                delay_seconds,
                message,
                recurring=args.recurring,
            ))
    elif args.command == "wait":
        sys.exit(commands.cmd_wait(client, args.session_id, args.seconds))
    elif args.command == "watch-job":
        if args.watch_job_command == "add":
            sys.exit(commands.cmd_watch_job_add(
                client,
                current_session_id=session_id,
                target_identifier=args.target,
                label=args.label,
                pid=args.pid,
                file_path=args.file_path,
                progress_regex=args.progress_regex,
                done_regex=args.done_regex,
                error_regex=args.error_regex,
                exit_code_file=args.exit_code_file,
                interval_seconds=args.interval_seconds,
                tail_lines=args.tail_lines,
                tail_on_error=args.tail_on_error,
                notify_on_change=not args.notify_every_poll,
            ))
        if args.watch_job_command == "list":
            sys.exit(commands.cmd_watch_job_list(
                client,
                current_session_id=session_id,
                target_identifier=args.target,
                list_all=args.all,
                include_inactive=args.include_inactive,
                json_output=args.json,
            ))
        if args.watch_job_command == "cancel":
            sys.exit(commands.cmd_watch_job_cancel(client, args.watch_id))
        print("Error: watch-job subcommand required (add, list, cancel)", file=sys.stderr)
        sys.exit(2)
    elif args.command == "queue":
        if args.queue_command == "run":
            sys.exit(commands.cmd_queue_run(
                client,
                current_session_id=session_id,
                job_type=args.job_type,
                label=args.label,
                cwd=args.cwd,
                timeout=args.timeout,
                env_pairs=args.env_pairs,
                notify_target=args.notify,
                command=args.queue_argv,
                script_file=args.script_file,
            ))
        if args.queue_command == "list":
            sys.exit(commands.cmd_queue_list(
                client,
                current_session_id=session_id,
                notify_target=args.notify,
                list_all=args.all,
                job_type=args.job_type,
                state=args.state,
                json_output=args.json,
            ))
        if args.queue_command == "status":
            sys.exit(commands.cmd_queue_status(client, args.job_id, json_output=args.json))
        if args.queue_command == "cancel":
            sys.exit(commands.cmd_queue_cancel(client, args.job_id))
        if args.queue_command == "ci-run":
            sys.exit(commands.cmd_queue_ci_run(
                client,
                current_session_id=session_id,
                policy=args.policy,
                dedupe_token=args.dedupe_token,
                label=args.label,
                cwd=args.cwd,
                timeout=args.timeout,
                job_type=args.job_type,
                env_pairs=args.env_pairs,
                metadata_pairs=args.metadata_pairs,
                command=args.queue_argv,
                script_file=args.script_file,
            ))
        if args.queue_command == "ci-status":
            sys.exit(commands.cmd_queue_ci_status(
                client,
                policy=args.policy,
                dedupe_token=args.dedupe_token,
                run_id=args.run_id,
                json_output=args.json,
            ))
        if args.queue_command == "ci-history":
            sys.exit(commands.cmd_queue_ci_history(
                client,
                policy=args.policy,
                limit=args.limit,
                include_suppressed=args.include_suppressed,
                json_output=args.json,
            ))
        print("Error: queue subcommand required (run, list, status, cancel, ci-run, ci-status, ci-history)", file=sys.stderr)
        sys.exit(2)
    elif args.command == "request-codex-review":
        action = args.action_or_pr
        if action == "list":
            sys.exit(commands.cmd_request_codex_review_list(
                client,
                current_session_id=session_id,
                notify_target=args.notify,
                list_all=args.all,
                include_inactive=args.inactive or args.all,
                json_output=args.json,
                repo=args.repo,
                pr_number=args.status_pr,
            ))
        if action == "status":
            sys.exit(commands.cmd_request_codex_review_status(
                client,
                current_session_id=session_id,
                request_id=args.request_id,
                pr_number=args.status_pr,
                repo=args.repo,
                notify_target=args.notify,
                list_all=args.all,
                json_output=args.json,
            ))
        if action == "cancel":
            if not args.request_id:
                print("Error: request ID required for cancel", file=sys.stderr)
                sys.exit(1)
            sys.exit(commands.cmd_request_codex_review_cancel(client, args.request_id))
        try:
            pr_number = int(action)
        except ValueError:
            print("Error: first argument must be a PR number, list, status, or cancel", file=sys.stderr)
            sys.exit(1)
        sys.exit(commands.cmd_request_codex_review_create(
            client,
            current_session_id=session_id,
            pr_number=pr_number,
            repo=args.repo,
            steer=args.steer,
            notify_target=args.notify,
            poll_interval_seconds=args.poll_interval_seconds,
            retry_interval_seconds=args.retry_interval_seconds,
        ))
    elif args.command == "spawn":
        provider = "codex-fork" if args.provider == "codex" else args.provider
        sys.exit(commands.cmd_spawn(
            client,
            session_id,
            provider,
            args.prompt,
            args.name,
            args.wait,
            args.model,
            args.working_dir,
            args.json,
            getattr(args, "track", None),
        ))
    elif args.command == "children":
        if args.session_id:
            parent_id, _ = commands.resolve_session_id(client, args.session_id)
            if parent_id is None:
                sessions = client.list_sessions()
                if sessions is None:
                    print(commands.UNAVAILABLE_MESSAGE, file=sys.stderr)
                    sys.exit(2)
                print(f"Error: Session '{args.session_id}' not found", file=sys.stderr)
                sys.exit(1)
        else:
            parent_id = session_id

        sys.exit(
            commands.cmd_children(
                client,
                parent_id,
                args.recursive,
                args.status,
                args.terminated,
                args.json,
                getattr(args, "db_path", None),
            )
        )
    elif args.command in ("kill", "retire"):
        sys.exit(commands.cmd_kill(client, session_id, args.session_id))
    elif args.command in ("restore", "unkill"):
        sys.exit(commands.cmd_restore(client, args.session))
    elif args.command == "clean":
        sys.exit(commands.cmd_clean(client, session_ids=getattr(args, 'session_ids', None)))
    elif args.command == "claude":
        sys.exit(commands.cmd_new(client, args.working_dir, provider="claude", parent_session_id=session_id))
    elif args.command == "codex":
        sys.exit(commands.cmd_new(client, args.working_dir, provider="codex-fork", parent_session_id=session_id))
    elif args.command == "codex-legacy":
        sys.exit(commands.cmd_new(client, args.working_dir, provider="codex", parent_session_id=session_id))
    elif args.command in ("codex-fork", "codex_fork"):
        sys.exit(commands.cmd_new(client, args.working_dir, provider="codex-fork", parent_session_id=session_id))
    elif args.command == "codex-2":
        sys.exit(commands.cmd_codex_2(client, args.working_dir, parent_session_id=session_id))
    elif args.command == "codex-app":
        sys.exit(commands.cmd_new(client, args.working_dir, provider="codex-app", parent_session_id=session_id))
    elif args.command == "codex-server":
        sys.exit(commands.cmd_removed_entrypoint("codex-server"))
    elif args.command == "new":
        sys.exit(commands.cmd_new(client, args.working_dir, provider="claude", parent_session_id=session_id))
    elif args.command == "attach":
        sys.exit(commands.cmd_attach(client, args.session))
    elif args.command == "output":
        sys.exit(commands.cmd_output(client, args.session, args.lines))
    elif args.command == "codex-tui":
        sys.exit(commands.cmd_codex_tui(
            client,
            args.session,
            poll_interval=args.poll_interval,
            event_limit=args.event_limit,
        ))
    elif args.command == "codex-fork-info":
        sys.exit(commands.cmd_codex_fork_info(client, json_output=args.json))
    elif args.command == "codex-rollout-gates":
        sys.exit(commands.cmd_codex_rollout_gates(client, json_output=args.json))
    elif args.command == "watch":
        sys.exit(commands.cmd_watch(
            client,
            repo=args.repo,
            role=args.role,
            interval=args.interval,
            restore_mode=args.restore,
            top_level=args.top_level,
            restore_sort=args.sort,
        ))
    elif args.command == "tail":
        sys.exit(commands.cmd_tail(
            client, args.session, args.n, args.raw,
            db_path_override=getattr(args, 'db_path', None),
        ))
    elif args.command == "clear":
        sys.exit(commands.cmd_clear(client, session_id, args.session, args.prompt))
    elif args.command == "handoff":
        sys.exit(commands.cmd_handoff(client, session_id, args.file_path))
    elif args.command == "task-complete":
        sys.exit(commands.cmd_task_complete(client, session_id))
    elif args.command == "turn-complete":
        sys.exit(commands.cmd_turn_complete(client, session_id))
    elif args.command == "context-monitor":
        sys.exit(commands.cmd_context_monitor(client, session_id, args.action, args.target))
    elif args.command == "em":
        sys.exit(commands.cmd_em(client, session_id, args.name))
    elif args.command == "maintainer":
        sys.exit(commands.cmd_maintainer(client, session_id, clear=args.clear))
    elif args.command == "register":
        sys.exit(commands.cmd_register(client, session_id, args.role))
    elif args.command == "unregister":
        sys.exit(commands.cmd_unregister(client, session_id, args.role))
    elif args.command == "lookup":
        sys.exit(commands.cmd_lookup(client, args.role))
    elif args.command == "roster":
        sys.exit(commands.cmd_roster(client))
    elif args.command == "adopt":
        sys.exit(commands.cmd_adopt(client, session_id, args.session))
    elif args.command == "setup":
        sys.exit(commands.cmd_setup(overwrite=args.overwrite))
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
