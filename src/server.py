"""FastAPI server for hooks and API endpoints."""

import json
import logging
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional, Dict, Any, Literal

from fastapi import FastAPI, HTTPException, Body, Request
from pydantic import BaseModel
from starlette.middleware.base import BaseHTTPMiddleware

from .models import Session, SessionStatus, NotificationChannel, Subagent, SubagentStatus, DeliveryResult
from .cli.commands import validate_friendly_name

logger = logging.getLogger(__name__)


def _normalize_provider(provider: Optional[str]) -> str:
    """Normalize/validate provider string."""
    if not provider:
        return "claude"
    provider = provider.lower()
    if provider in ("codex-app", "codex_app", "codex-server", "codex-app-server"):
        return "codex-app"
    if provider in ("claude", "codex"):
        return provider
    raise HTTPException(status_code=400, detail=f"Invalid provider: {provider}")


class RequestTimingMiddleware(BaseHTTPMiddleware):
    """Log slow requests for debugging."""

    def __init__(self, app, config: Optional[dict] = None):
        super().__init__(app)
        self.config = config or {}

        # Load timing thresholds from config
        timeouts = self.config.get("timeouts", {})
        server_timeouts = timeouts.get("server", {})
        self.slow_threshold = server_timeouts.get("slow_request_threshold_seconds", 1.0)
        self.timing_threshold = server_timeouts.get("request_timing_threshold_seconds", 0.1)

    async def dispatch(self, request: Request, call_next):
        start = time.monotonic()
        response = await call_next(request)
        elapsed = time.monotonic() - start

        # Log slow requests
        if elapsed > self.slow_threshold:
            logger.warning(
                f"SLOW REQUEST: {request.method} {request.url.path} "
                f"took {elapsed:.2f}s"
            )
        elif elapsed > self.timing_threshold:
            logger.info(
                f"Request: {request.method} {request.url.path} "
                f"took {elapsed*1000:.0f}ms"
            )

        return response


class CreateSessionRequest(BaseModel):
    """Request to create a new session."""
    working_dir: str = "~"
    name: Optional[str] = None
    provider: Optional[str] = "claude"


class SessionResponse(BaseModel):
    """Response containing session info."""
    id: str
    name: str
    working_dir: str
    status: str
    created_at: str
    last_activity: str
    tmux_session: str
    provider: Optional[str] = "claude"
    friendly_name: Optional[str] = None
    current_task: Optional[str] = None
    git_remote_url: Optional[str] = None
    parent_session_id: Optional[str] = None


class SendInputRequest(BaseModel):
    """Request to send input to a session."""
    text: str
    sender_session_id: Optional[str] = None  # Optional sender identification
    delivery_mode: str = "sequential"  # sequential, important, urgent
    from_sm_send: bool = False  # True if called from sm send command
    timeout_seconds: Optional[int] = None  # Drop message if not delivered in time
    notify_on_delivery: bool = False  # Notify sender when delivered
    notify_after_seconds: Optional[int] = None  # Notify sender N seconds after delivery
    notify_on_stop: bool = False  # Notify sender when receiver's Stop hook fires


class ClearSessionRequest(BaseModel):
    """Request to clear/reset a session context."""
    prompt: Optional[str] = None


class NotifyRequest(BaseModel):
    """Request to send a notification."""
    message: str
    channel: Optional[str] = None  # "telegram" or "email"
    urgent: bool = False


class HookPayload(BaseModel):
    """Payload from Claude Code hooks."""
    # Claude Code hook fields
    hook_type: Optional[str] = None  # "Stop", "Notification", etc.
    session_id: Optional[str] = None
    transcript: Optional[list] = None
    stop_hook_active: Optional[bool] = None
    # For backward compatibility
    event: Optional[str] = None
    data: Optional[dict] = None

    class Config:
        extra = "allow"  # Allow additional fields from Claude


class SubagentStartRequest(BaseModel):
    """Request to register subagent start."""
    agent_id: str
    agent_type: str
    transcript_path: Optional[str] = None


class SubagentStopRequest(BaseModel):
    """Request to register subagent stop."""
    summary: Optional[str] = None


class SubagentResponse(BaseModel):
    """Response containing subagent info."""
    agent_id: str
    agent_type: str
    parent_session_id: str
    started_at: str
    stopped_at: Optional[str] = None
    status: str
    summary: Optional[str] = None


class SpawnChildRequest(BaseModel):
    """Request to spawn a child agent session."""
    parent_session_id: str
    prompt: str
    name: Optional[str] = None
    wait: Optional[int] = None
    model: Optional[str] = None
    working_dir: Optional[str] = None
    provider: Optional[str] = None


class KillSessionRequest(BaseModel):
    """Request to kill a session with ownership check."""
    requester_session_id: Optional[str] = None


class StartReviewRequest(BaseModel):
    """Start a review on an existing session."""
    mode: str = "branch"
    base_branch: Optional[str] = None
    commit_sha: Optional[str] = None
    custom_prompt: Optional[str] = None
    steer: Optional[str] = None
    wait: Optional[int] = None
    watcher_session_id: Optional[str] = None


class SpawnReviewRequest(BaseModel):
    """Spawn a new session and start a review."""
    parent_session_id: str
    mode: str = "branch"
    base_branch: Optional[str] = None
    commit_sha: Optional[str] = None
    custom_prompt: Optional[str] = None
    steer: Optional[str] = None
    name: Optional[str] = None
    wait: Optional[int] = None
    model: Optional[str] = None
    working_dir: Optional[str] = None


class PRReviewRequest(BaseModel):
    """Trigger @codex review on a GitHub PR."""
    pr_number: int
    repo: Optional[str] = None
    steer: Optional[str] = None
    wait: Optional[int] = None
    caller_session_id: Optional[str] = None


# Health check response models
class HealthCheckResult(BaseModel):
    """Result of a single health check."""
    status: Literal["ok", "warning", "error"]
    message: Optional[str] = None
    details: Optional[Dict[str, Any]] = None


class HealthCheckResponse(BaseModel):
    """Response from detailed health check endpoint."""
    status: Literal["healthy", "degraded", "unhealthy"]
    checks: Dict[str, HealthCheckResult]
    resources: Dict[str, Any]
    timestamp: str


def _invalidate_session_cache(app: FastAPI, session_id: str) -> None:
    """Clear server-side caches for a session after a context reset.

    Prevents stale cached output and notification state from a previous
    task from leaking into stop-hook notifications for the next task (#167).
    """
    app.state.last_claude_output.pop(session_id, None)
    app.state.pending_stop_notifications.discard(session_id)

    queue_mgr = (
        app.state.session_manager.message_queue_manager
        if app.state.session_manager
        else None
    )
    if queue_mgr:
        state = queue_mgr.delivery_states.get(session_id)
        if state:
            state.stop_notify_sender_id = None
            state.stop_notify_sender_name = None


def create_app(
    session_manager=None,
    notifier=None,
    output_monitor=None,
    child_monitor=None,
    config: Optional[dict] = None,
    lifespan=None,
) -> FastAPI:
    """
    Create the FastAPI application.

    Args:
        session_manager: SessionManager instance
        notifier: Notifier instance
        output_monitor: OutputMonitor instance
        child_monitor: ChildMonitor instance
        config: Configuration dictionary
        lifespan: Optional ASGI lifespan context manager

    Returns:
        Configured FastAPI app
    """
    app = FastAPI(
        title="Claude Session Manager",
        description="Manage Claude Code sessions with Telegram/Email notifications",
        version="0.1.0",
        lifespan=lifespan,
    )

    # Store config first so middleware can access it
    app.state.config = config or {}

    # Add timing middleware for debugging (with config)
    app.add_middleware(RequestTimingMiddleware, config=config)

    # Store references to components
    app.state.session_manager = session_manager
    app.state.notifier = notifier
    app.state.output_monitor = output_monitor
    app.state.child_monitor = child_monitor
    app.state.last_claude_output = {}  # Store last output per session from hooks
    app.state.pending_stop_notifications = set()  # Sessions where Stop hook had empty transcript

    @app.get("/")
    async def root():
        """Health check endpoint."""
        return {"status": "ok", "service": "session-manager"}

    @app.get("/health")
    async def health():
        """Health check endpoint."""
        return {"status": "healthy"}

    @app.get("/health/detailed", response_model=HealthCheckResponse)
    async def health_detailed():
        """
        Detailed health check endpoint for monitoring and debugging.

        Performs comprehensive checks on:
        - State file integrity
        - Session consistency (memory vs tmux)
        - Message queue health
        - Component status (telegram, monitors)
        - Resource usage
        """
        checks: Dict[str, HealthCheckResult] = {}
        resources: Dict[str, Any] = {}

        # Track overall status (starts healthy, degrades based on check results)
        overall_status: Literal["healthy", "degraded", "unhealthy"] = "healthy"

        def update_status(check_status: str):
            nonlocal overall_status
            if check_status == "error":
                overall_status = "unhealthy"
            elif check_status == "warning" and overall_status == "healthy":
                overall_status = "degraded"

        # 1. State File Integrity Check
        state_file_check = await _check_state_file(app)
        checks["state_file"] = state_file_check
        update_status(state_file_check.status)

        # 2. Session Consistency Check (memory vs tmux)
        session_check = await _check_session_consistency(app)
        checks["tmux_sessions"] = session_check
        update_status(session_check.status)

        # 3. Message Queue Health Check
        mq_check = await _check_message_queue(app)
        checks["message_queue"] = mq_check
        update_status(mq_check.status)

        # 4. Component Status Checks
        telegram_check = await _check_telegram(app)
        checks["telegram"] = telegram_check
        update_status(telegram_check.status)

        monitor_check = await _check_monitors(app)
        checks["monitors"] = monitor_check
        update_status(monitor_check.status)

        # 5. Resource Usage
        resources = _get_resource_usage(app)

        return HealthCheckResponse(
            status=overall_status,
            checks=checks,
            resources=resources,
            timestamp=datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        )

    async def _check_state_file(app) -> HealthCheckResult:
        """Check state file integrity."""
        if not app.state.session_manager:
            return HealthCheckResult(
                status="error",
                message="Session manager not configured",
            )

        state_file = app.state.session_manager.state_file
        try:
            if not state_file.exists():
                return HealthCheckResult(
                    status="ok",
                    message="No state file (fresh start)",
                    details={"exists": False},
                )

            with open(state_file) as f:
                data = json.load(f)

            # Validate required structure
            if not isinstance(data, dict):
                return HealthCheckResult(
                    status="error",
                    message="State file is not a valid JSON object",
                )

            sessions = data.get("sessions", [])
            if not isinstance(sessions, list):
                return HealthCheckResult(
                    status="error",
                    message="State file sessions field is not a list",
                )

            return HealthCheckResult(
                status="ok",
                message="State file valid",
                details={
                    "exists": True,
                    "sessions_in_file": len(sessions),
                    "file_size_bytes": state_file.stat().st_size,
                },
            )

        except json.JSONDecodeError as e:
            return HealthCheckResult(
                status="error",
                message=f"State file contains invalid JSON: {e}",
            )
        except Exception as e:
            return HealthCheckResult(
                status="error",
                message=f"Failed to read state file: {e}",
            )

    async def _check_session_consistency(app) -> HealthCheckResult:
        """Check that sessions in memory match tmux sessions."""
        if not app.state.session_manager:
            return HealthCheckResult(
                status="error",
                message="Session manager not configured",
            )

        sm = app.state.session_manager
        memory_sessions = list(sm.sessions.values())

        # Get tmux sessions managed by us (those starting with "claude-" or "codex-")
        try:
            all_tmux_sessions = set(sm.tmux.list_sessions())
            our_tmux_sessions = {
                s for s in all_tmux_sessions
                if s.startswith("claude-") or s.startswith("codex-")
            }
        except Exception as e:
            return HealthCheckResult(
                status="error",
                message=f"Failed to list tmux sessions: {e}",
            )

        # Check for sessions in memory that don't exist in tmux (tmux providers only)
        missing_in_tmux = []
        for session in memory_sessions:
            if getattr(session, "provider", "claude") == "codex-app":
                continue
            if session.status not in (SessionStatus.STOPPED,) and session.tmux_session not in all_tmux_sessions:
                missing_in_tmux.append(session.id)

        # Check for orphaned tmux sessions (in tmux but not in memory)
        memory_tmux_names = {
            s.tmux_session for s in memory_sessions
            if getattr(s, "provider", "claude") != "codex-app"
        }
        orphaned_tmux = list(our_tmux_sessions - memory_tmux_names)

        # Check for duplicate session IDs (should never happen)
        session_ids = [s.id for s in memory_sessions]
        duplicates = [sid for sid in session_ids if session_ids.count(sid) > 1]

        if missing_in_tmux or duplicates:
            return HealthCheckResult(
                status="error",
                message="Session consistency issues found",
                details={
                    "sessions_in_memory": len(memory_sessions),
                    "our_tmux_sessions": len(our_tmux_sessions),
                    "missing_in_tmux": missing_in_tmux,
                    "orphaned_tmux": orphaned_tmux,
                    "duplicate_ids": list(set(duplicates)),
                },
            )

        if orphaned_tmux:
            return HealthCheckResult(
                status="warning",
                message=f"Found {len(orphaned_tmux)} orphaned tmux sessions",
                details={
                    "sessions_in_memory": len(memory_sessions),
                    "our_tmux_sessions": len(our_tmux_sessions),
                    "orphaned_tmux": orphaned_tmux,
                },
            )

        return HealthCheckResult(
            status="ok",
            message="Sessions consistent",
            details={
                "sessions_in_memory": len(memory_sessions),
                "our_tmux_sessions": len(our_tmux_sessions),
                "orphaned_tmux": 0,
            },
        )

    async def _check_message_queue(app) -> HealthCheckResult:
        """Check message queue health."""
        sm = app.state.session_manager
        if not sm or not sm.message_queue_manager:
            return HealthCheckResult(
                status="warning",
                message="Message queue not configured",
            )

        mq = sm.message_queue_manager

        try:
            # Check if database is accessible
            db_path = mq.db_path
            if not db_path.exists():
                return HealthCheckResult(
                    status="warning",
                    message="Message queue database does not exist yet",
                    details={"db_exists": False},
                )

            # Count pending and potentially stuck messages
            import sqlite3
            conn = sqlite3.connect(str(db_path))
            cursor = conn.cursor()

            # Count total pending
            cursor.execute("SELECT COUNT(*) FROM messages WHERE delivered_at IS NULL")
            pending_count = cursor.fetchone()[0]

            # Count stuck messages (queued > 1 hour ago and not delivered)
            cursor.execute("""
                SELECT COUNT(*) FROM messages
                WHERE delivered_at IS NULL
                AND datetime(queued_at) < datetime('now', '-1 hour')
            """)
            stuck_count = cursor.fetchone()[0]

            # Count expired messages still in queue
            cursor.execute("""
                SELECT COUNT(*) FROM messages
                WHERE delivered_at IS NULL
                AND timeout_at IS NOT NULL
                AND datetime(timeout_at) < datetime('now')
            """)
            expired_count = cursor.fetchone()[0]

            conn.close()

            if stuck_count > 0 or expired_count > 0:
                return HealthCheckResult(
                    status="warning",
                    message=f"Found {stuck_count} stuck and {expired_count} expired messages",
                    details={
                        "db_exists": True,
                        "pending": pending_count,
                        "stuck": stuck_count,
                        "expired": expired_count,
                    },
                )

            return HealthCheckResult(
                status="ok",
                message="Message queue healthy",
                details={
                    "db_exists": True,
                    "pending": pending_count,
                    "stuck": 0,
                    "expired": 0,
                },
            )

        except Exception as e:
            return HealthCheckResult(
                status="error",
                message=f"Failed to check message queue: {e}",
            )

    async def _check_telegram(app) -> HealthCheckResult:
        """Check Telegram bot status."""
        notifier = app.state.notifier
        if not notifier:
            return HealthCheckResult(
                status="ok",
                message="Notifier not configured",
                details={"configured": False},
            )

        telegram_bot = getattr(notifier, 'telegram', None)
        if not telegram_bot:
            return HealthCheckResult(
                status="ok",
                message="Telegram not configured",
                details={"configured": False},
            )

        # Check if bot is initialized and running
        bot = getattr(telegram_bot, 'bot', None)
        application = getattr(telegram_bot, 'application', None)

        if not bot or not application:
            return HealthCheckResult(
                status="warning",
                message="Telegram bot not fully initialized",
                details={
                    "configured": True,
                    "bot_initialized": bot is not None,
                    "application_initialized": application is not None,
                },
            )

        # Try to check if bot is running
        is_running = application.running if hasattr(application, 'running') else None

        return HealthCheckResult(
            status="ok",
            message="Telegram bot running",
            details={
                "configured": True,
                "bot_initialized": True,
                "application_running": is_running,
                "tracked_sessions": len(telegram_bot._session_threads),
                "tracked_topics": len(telegram_bot._topic_sessions),
            },
        )

    async def _check_monitors(app) -> HealthCheckResult:
        """Check output and child monitors status."""
        output_monitor = app.state.output_monitor
        child_monitor = app.state.child_monitor

        output_status = {
            "configured": output_monitor is not None,
            "active_tasks": 0,
        }

        if output_monitor:
            output_status["active_tasks"] = len(output_monitor._tasks)

        child_status = {
            "configured": child_monitor is not None,
            "running": False,
        }

        if child_monitor:
            child_status["running"] = getattr(child_monitor, '_running', False)

        # Determine overall status
        if not output_monitor:
            return HealthCheckResult(
                status="warning",
                message="Output monitor not configured",
                details={
                    "output_monitor": output_status,
                    "child_monitor": child_status,
                },
            )

        # Check if active sessions are being monitored
        sm = app.state.session_manager
        if sm:
            active_sessions = [
                s for s in sm.sessions.values()
                if s.status not in (SessionStatus.STOPPED,)
                and getattr(s, "provider", "claude") != "codex-app"
            ]
            monitored = len(output_monitor._tasks)
            if len(active_sessions) > monitored:
                return HealthCheckResult(
                    status="warning",
                    message=f"{len(active_sessions) - monitored} active sessions not being monitored",
                    details={
                        "output_monitor": output_status,
                        "child_monitor": child_status,
                        "active_sessions": len(active_sessions),
                        "monitored_sessions": monitored,
                    },
                )

        return HealthCheckResult(
            status="ok",
            message="Monitors running",
            details={
                "output_monitor": output_status,
                "child_monitor": child_status,
            },
        )

    def _get_resource_usage(app) -> Dict[str, Any]:
        """Get resource usage statistics."""
        resources = {
            "active_sessions": 0,
            "output_cache_size": 0,
            "scheduled_tasks": 0,
            "monitor_tasks": 0,
        }

        # Active sessions
        sm = app.state.session_manager
        if sm:
            resources["active_sessions"] = len([
                s for s in sm.sessions.values()
                if s.status not in (SessionStatus.STOPPED,)
            ])
            resources["total_sessions"] = len(sm.sessions)

        # Output cache size
        if hasattr(app.state, 'last_claude_output'):
            resources["output_cache_size"] = len(app.state.last_claude_output)

        # Scheduled tasks in message queue
        if sm and sm.message_queue_manager:
            mq = sm.message_queue_manager
            if hasattr(mq, '_scheduled_tasks'):
                resources["scheduled_tasks"] = len(mq._scheduled_tasks)

        # Monitor tasks
        if app.state.output_monitor:
            resources["monitor_tasks"] = len(app.state.output_monitor._tasks)

        return resources

    @app.post("/sessions", response_model=SessionResponse)
    async def create_session(request: CreateSessionRequest):
        """Create a new Claude Code session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        provider = _normalize_provider(request.provider)
        session = await app.state.session_manager.create_session(
            working_dir=request.working_dir,
            name=request.name,
            provider=provider,
        )

        if not session:
            raise HTTPException(status_code=500, detail="Failed to create session")

        # Start monitoring the session (tmux providers only)
        if app.state.output_monitor and getattr(session, "provider", "claude") != "codex-app":
            await app.state.output_monitor.start_monitoring(session)

        return SessionResponse(
            id=session.id,
            name=session.name,
            working_dir=session.working_dir,
            status=session.status.value,
            created_at=session.created_at.isoformat(),
            last_activity=session.last_activity.isoformat(),
            tmux_session=session.tmux_session,
            provider=getattr(session, "provider", "claude"),
            friendly_name=session.friendly_name,
            current_task=session.current_task,
            git_remote_url=session.git_remote_url,
            parent_session_id=session.parent_session_id,
        )

    @app.post("/sessions/create")
    async def create_session_endpoint(working_dir: str, provider: str = "claude"):
        """
        Create a new Claude Code session.

        Args:
            working_dir: Absolute path to working directory

        Returns:
            Session object dict
        """
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        # Create session using config settings
        provider = _normalize_provider(provider)
        session = await app.state.session_manager.create_session(
            working_dir=working_dir,
            telegram_chat_id=None,  # No Telegram association
            provider=provider,
        )

        if not session:
            raise HTTPException(status_code=500, detail="Failed to create session")

        # Start monitoring (tmux providers only)
        if app.state.output_monitor and getattr(session, "provider", "claude") != "codex-app":
            await app.state.output_monitor.start_monitoring(session)

        return session.to_dict()

    @app.get("/sessions")
    async def list_sessions():
        """List all active sessions."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        sessions = app.state.session_manager.list_sessions()

        return {
            "sessions": [
                SessionResponse(
                    id=s.id,
                    name=s.name,
                    working_dir=s.working_dir,
                    status=s.status.value,
                    created_at=s.created_at.isoformat(),
                    last_activity=s.last_activity.isoformat(),
                    tmux_session=s.tmux_session,
                    provider=getattr(s, "provider", "claude"),
                    friendly_name=s.friendly_name,
                    current_task=s.current_task,
                    git_remote_url=s.git_remote_url,
                    parent_session_id=s.parent_session_id,
                )
                for s in sessions
            ]
        }

    @app.get("/sessions/{session_id}", response_model=SessionResponse)
    async def get_session(session_id: str):
        """Get session details."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)

        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        return SessionResponse(
            id=session.id,
            name=session.name,
            working_dir=session.working_dir,
            status=session.status.value,
            created_at=session.created_at.isoformat(),
            last_activity=session.last_activity.isoformat(),
            tmux_session=session.tmux_session,
            provider=getattr(session, "provider", "claude"),
            friendly_name=session.friendly_name,
            current_task=session.current_task,
            git_remote_url=session.git_remote_url,
            parent_session_id=session.parent_session_id,
        )

    @app.patch("/sessions/{session_id}", response_model=SessionResponse)
    async def update_session(
        session_id: str,
        friendly_name: Optional[str] = Body(None, embed=True)
    ):
        """Update session metadata (currently only friendly_name)."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        if friendly_name is not None:
            # Validate friendly name
            valid, error = validate_friendly_name(friendly_name)
            if not valid:
                raise HTTPException(status_code=400, detail=error)

            session.friendly_name = friendly_name
            app.state.session_manager._save_state()
            # Update tmux status bar
            if getattr(session, "provider", "claude") != "codex-app":
                app.state.session_manager.tmux.set_status_bar(session.tmux_session, friendly_name)
            # Update Telegram topic name if applicable
            if session.telegram_thread_id and app.state.notifier:
                success = await app.state.notifier.rename_session_topic(session, friendly_name)
                if not success:
                    logger.warning(f"Failed to rename Telegram topic for session {session_id}")

        return SessionResponse(
            id=session.id,
            name=session.name,
            working_dir=session.working_dir,
            status=session.status.value,
            created_at=session.created_at.isoformat(),
            last_activity=session.last_activity.isoformat(),
            tmux_session=session.tmux_session,
            provider=getattr(session, "provider", "claude"),
            friendly_name=session.friendly_name,
            current_task=session.current_task,
            git_remote_url=session.git_remote_url,
            parent_session_id=session.parent_session_id,
        )

    @app.put("/sessions/{session_id}/task")
    async def update_task(session_id: str, task: str = Body(..., embed=True)):
        """Register what the session is currently working on."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        session.current_task = task
        app.state.session_manager._save_state()

        return {"session_id": session_id, "task": task}

    @app.post("/sessions/{session_id}/input")
    async def send_input(session_id: str, request: SendInputRequest):
        """Send input to a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        result = await app.state.session_manager.send_input(
            session_id,
            request.text,
            sender_session_id=request.sender_session_id,
            delivery_mode=request.delivery_mode,
            from_sm_send=request.from_sm_send,
            timeout_seconds=request.timeout_seconds,
            notify_on_delivery=request.notify_on_delivery,
            notify_after_seconds=request.notify_after_seconds,
            notify_on_stop=request.notify_on_stop,
        )

        if result == DeliveryResult.FAILED:
            raise HTTPException(status_code=500, detail="Failed to send input")

        # Update activity in monitor
        if app.state.output_monitor:
            app.state.output_monitor.update_activity(session_id)

        # Return delivery result with queue info if queued
        response = {
            "status": result.value,  # "delivered", "queued", or "failed"
            "session_id": session_id,
            "delivery_mode": request.delivery_mode,
        }

        if result == DeliveryResult.QUEUED:
            queue_mgr = app.state.session_manager.message_queue_manager
            if queue_mgr:
                queue_len = queue_mgr.get_queue_length(session_id)
                response["queue_position"] = queue_len
                response["estimated_delivery"] = "waiting_for_idle"

        return response

    @app.post("/sessions/{session_id}/key")
    async def send_key(session_id: str, key: str = Body(..., embed=True)):
        """Send a single key to a session (e.g., 'y', 'n')."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        success = app.state.session_manager.send_key(session_id, key)

        if not success:
            raise HTTPException(status_code=500, detail="Failed to send key")

        if app.state.output_monitor:
            app.state.output_monitor.update_activity(session_id)

        return {"status": "sent", "session_id": session_id, "key": key}

    @app.post("/sessions/{session_id}/clear")
    async def clear_session(session_id: str, request: ClearSessionRequest):
        """Clear/reset a session's context."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        success = await app.state.session_manager.clear_session(session_id, request.prompt)
        if not success:
            raise HTTPException(status_code=500, detail="Failed to clear session")

        # Invalidate server-side caches so stale state from the previous task
        # doesn't leak into stop-hook notifications for the next task (#167)
        _invalidate_session_cache(app, session_id)

        return {"status": "cleared", "session_id": session_id}

    @app.post("/sessions/{session_id}/invalidate-cache")
    async def invalidate_session_cache(session_id: str):
        """Invalidate server-side caches for a session.

        Called by the CLI after tmux-level clear operations so that stale
        cached output and notification state from a previous task don't
        leak into the next task's stop-hook notifications (#167).
        """
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        _invalidate_session_cache(app, session_id)

        return {"status": "invalidated", "session_id": session_id}

    @app.delete("/sessions/{session_id}")
    async def kill_session(session_id: str):
        """Kill a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        # Kill tmux session
        success = app.state.session_manager.kill_session(session_id)

        if not success:
            raise HTTPException(status_code=500, detail="Failed to kill session")

        # Perform full cleanup (Telegram, monitoring, state)
        if app.state.output_monitor:
            await app.state.output_monitor.cleanup_session(session)

        return {"status": "killed", "session_id": session_id}

    @app.post("/sessions/{session_id}/open")
    async def open_terminal(session_id: str):
        """Open a session in Terminal.app."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        success = app.state.session_manager.open_terminal(session_id)

        if not success:
            raise HTTPException(status_code=500, detail="Failed to open Terminal")

        return {"status": "opened", "session_id": session_id}

    @app.get("/sessions/{session_id}/output")
    async def capture_output(session_id: str, lines: int = 50):
        """Capture recent output from a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        output = app.state.session_manager.capture_output(session_id, lines)

        return {"session_id": session_id, "output": output}

    @app.get("/sessions/{session_id}/last-message")
    async def get_last_message(session_id: str):
        """Get the last Claude message from hooks (structured output)."""
        output = app.state.last_claude_output.get(session_id)
        if not output:
            # Try "latest" as fallback
            output = app.state.last_claude_output.get("latest")
        return {"session_id": session_id, "message": output}

    @app.get("/sessions/{session_id}/summary")
    async def get_summary(session_id: str, lines: int = 100):
        """
        Generate AI-powered summary of session activity.

        Args:
            session_id: Session to summarize
            lines: Number of lines of tmux output to analyze (default 100)

        Returns:
            JSON with summary text
        """
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        try:
            # Get tmux output
            tmux_output = app.state.session_manager.capture_output(session_id, lines)

            if not tmux_output:
                raise HTTPException(status_code=404, detail="No output available for session")

            # Strip ANSI codes
            from .notifier import strip_ansi
            clean_output = strip_ansi(tmux_output)

            # Prepare prompt for Claude Haiku
            prompt = f"""You are analyzing terminal output from a Claude Code session. Based on the output below, write a 2-3 line summary of what the session is currently working on. Focus on:
- What task/problem they're solving
- Current status (working, waiting, error, etc)
- Key details (files, commands, progress)

Output from session:
{clean_output}

Provide ONLY the summary, no preamble or questions."""

            logger.info(f"Generating summary for session {session_id}, input length: {len(prompt)} chars")

            # Get timeout from config
            config = app.state.config or {}
            timeouts = config.get("timeouts", {})
            server_timeouts = timeouts.get("server", {})
            summary_timeout = server_timeouts.get("summary_generation_timeout_seconds", 60)

            # Use asyncio.create_subprocess_exec for non-blocking execution
            import asyncio
            proc = await asyncio.create_subprocess_exec(
                '/opt/homebrew/bin/claude', '--model', 'haiku', '--print',
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE
            )

            try:
                stdout, stderr = await asyncio.wait_for(
                    proc.communicate(input=prompt.encode('utf-8')),
                    timeout=summary_timeout
                )
                result_stdout = stdout.decode('utf-8')
                result_stderr = stderr.decode('utf-8')
                returncode = proc.returncode
            except asyncio.TimeoutError:
                proc.kill()
                await proc.wait()
                raise HTTPException(status_code=504, detail=f"Summary generation timed out ({summary_timeout}s)")

            logger.info(f"Summary generated, return code: {returncode}, output length: {len(result_stdout)}")

            if returncode != 0:
                logger.error(f"Claude command failed: {result_stderr}")
                raise HTTPException(status_code=500, detail=f"Error generating summary: {result_stderr[:200]}")

            summary = result_stdout.strip()

            if not summary:
                raise HTTPException(status_code=500, detail="Summary was empty")

            return {"session_id": session_id, "summary": summary}

        except HTTPException:
            raise
        except Exception as e:
            logger.error(f"Error generating summary for session {session_id}: {e}", exc_info=True)
            raise HTTPException(status_code=500, detail=str(e))

    @app.post("/sessions/{session_id}/subagents", response_model=SubagentResponse)
    async def register_subagent_start(session_id: str, request: SubagentStartRequest):
        """Register a new subagent spawned by this session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        # Create new subagent
        from datetime import datetime
        subagent = Subagent(
            agent_id=request.agent_id,
            agent_type=request.agent_type,
            parent_session_id=session_id,
            transcript_path=request.transcript_path,
            started_at=datetime.now(),
            status=SubagentStatus.RUNNING,
        )

        # Add to session
        session.subagents.append(subagent)
        app.state.session_manager._save_state()

        logger.info(f"Registered subagent {request.agent_id} ({request.agent_type}) for session {session_id}")

        return SubagentResponse(
            agent_id=subagent.agent_id,
            agent_type=subagent.agent_type,
            parent_session_id=subagent.parent_session_id,
            started_at=subagent.started_at.isoformat(),
            stopped_at=None,
            status=subagent.status.value,
            summary=None,
        )

    @app.post("/sessions/{session_id}/subagents/{agent_id}/stop")
    async def register_subagent_stop(session_id: str, agent_id: str, request: SubagentStopRequest):
        """Register subagent completion."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        # Find the subagent
        subagent = None
        for sa in session.subagents:
            if sa.agent_id == agent_id:
                subagent = sa
                break

        if not subagent:
            raise HTTPException(status_code=404, detail=f"Subagent {agent_id} not found")

        # Update subagent
        from datetime import datetime
        subagent.stopped_at = datetime.now()
        subagent.status = SubagentStatus.COMPLETED
        if request.summary:
            subagent.summary = request.summary

        app.state.session_manager._save_state()

        logger.info(f"Stopped subagent {agent_id} for session {session_id}")

        return {
            "session_id": session_id,
            "agent_id": agent_id,
            "status": "stopped",
            "summary": subagent.summary,
        }

    @app.get("/sessions/{session_id}/subagents")
    async def list_subagents(session_id: str):
        """List all subagents for a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        return {
            "session_id": session_id,
            "subagents": [
                SubagentResponse(
                    agent_id=sa.agent_id,
                    agent_type=sa.agent_type,
                    parent_session_id=sa.parent_session_id,
                    started_at=sa.started_at.isoformat(),
                    stopped_at=sa.stopped_at.isoformat() if sa.stopped_at else None,
                    status=sa.status.value,
                    summary=sa.summary,
                )
                for sa in session.subagents
            ],
        }

    @app.post("/notify")
    async def send_notification(request: NotifyRequest):
        """
        Send a notification (for use by Claude Code hooks).

        This endpoint allows Claude to request notifications to be sent
        to the user via Telegram or Email.
        """
        if not app.state.notifier:
            raise HTTPException(status_code=503, detail="Notifier not configured")

        channel = None
        if request.channel:
            try:
                channel = NotificationChannel(request.channel)
            except ValueError:
                raise HTTPException(status_code=400, detail=f"Invalid channel: {request.channel}")

        # Use email handler directly for explicit email requests
        if channel == NotificationChannel.EMAIL:
            success = await app.state.notifier.request_email_notification(
                session_id="api",
                message=request.message,
                urgent=request.urgent,
            )
        else:
            # For Telegram, we need a chat ID - this should come from an active session
            # For now, just log it
            logger.info(f"Notification requested: {request.message}")
            success = True

        return {"status": "sent" if success else "failed"}

    @app.post("/hooks/claude")
    async def claude_hook(payload: dict = Body(...)):
        """
        Webhook endpoint for Claude Code hooks.

        Receives structured data from Claude Code Stop/Notification hooks.
        """
        import json
        from pathlib import Path

        hook_event = payload.get("hook_event_name", "unknown")
        logger.info(f"Hook received: {hook_event}")
        logger.debug(f"Hook payload keys: {list(payload.keys())}")

        transcript_path = payload.get("transcript_path")
        claude_session_id = payload.get("session_id")
        # This will be set by the environment variable we pass when launching Claude
        session_manager_id = payload.get("session_manager_id") or payload.get("CLAUDE_SESSION_MANAGER_ID")

        # Read last assistant message from transcript file (in thread pool to avoid blocking)
        last_message = None
        if transcript_path:
            import asyncio

            def read_transcript():
                """
                Read transcript file synchronously (runs in thread pool).

                Returns:
                    Tuple of (success: bool, message: str | None)
                """
                try:
                    transcript_file = Path(transcript_path)
                    if not transcript_file.exists():
                        logger.warning(f"Transcript file does not exist: {transcript_path}")
                        return (False, None)
                    # JSONL file - read last lines and find last assistant message
                    lines = transcript_file.read_text().strip().split('\n')
                    for line in reversed(lines):
                        try:
                            entry = json.loads(line)
                            if entry.get("type") == "assistant":
                                # Extract text from message content
                                message = entry.get("message", {})
                                content = message.get("content", [])
                                texts = []
                                for item in content:
                                    if isinstance(item, dict) and item.get("type") == "text":
                                        texts.append(item.get("text", ""))
                                full_text = "\n".join(texts).strip()
                                if full_text:
                                    return (True, full_text)
                                # Newest assistant message exists but has no
                                # visible text (whitespace-only / not flushed).
                                # Stop here — do NOT fall back to older entries
                                # which would surface a stale message.
                                return (True, None)
                        except json.JSONDecodeError as e:
                            logger.debug(f"Skipping malformed JSON line in transcript: {e}")
                            continue
                    # No assistant message found
                    return (True, None)
                except Exception as e:
                    logger.error(f"CRITICAL: Error reading transcript {transcript_path}: {e}")
                    logger.error(f"Claude output will not be available for this hook event")
                    return (False, None)

            try:
                success, last_message = await asyncio.to_thread(read_transcript)
                if not success:
                    logger.warning(f"Failed to read transcript for hook event: {hook_event}")
            except Exception as e:
                logger.error(f"CRITICAL: Error reading transcript in thread: {e}")
                last_message = None

        # Store last message
        if last_message:
            app.state.last_claude_output["latest"] = last_message
            # If we have the session manager ID, store under that immediately
            if session_manager_id:
                app.state.last_claude_output[session_manager_id] = last_message
                logger.info(f"Stored Claude output for session {session_manager_id}: {last_message[:100]}...")
            else:
                app.state.last_claude_output[claude_session_id or "default"] = last_message
                logger.warning(f"No session_manager_id in hook - stored as {claude_session_id or 'default'}")

        # Handle Stop hook - Claude finished responding
        # Mark session as idle for message queue delivery
        if hook_event == "Stop" and session_manager_id:
            queue_mgr = app.state.session_manager.message_queue_manager if app.state.session_manager else None
            if queue_mgr:
                queue_mgr.mark_session_idle(session_manager_id)
                # Restore any saved user input
                import asyncio
                asyncio.create_task(queue_mgr._restore_user_input_after_response(session_manager_id))

            # Keep session.status in sync with delivery_state.is_idle
            if app.state.session_manager:
                target_session = app.state.session_manager.get_session(session_manager_id)
                if target_session and target_session.status != SessionStatus.STOPPED:
                    app.state.session_manager.update_session_status(
                        session_manager_id, SessionStatus.IDLE
                    )

            # Always keep transcript_path up to date (needed for crash recovery)
            if transcript_path and app.state.session_manager:
                target = app.state.session_manager.get_session(session_manager_id)
                if target and target.transcript_path != transcript_path:
                    target.transcript_path = transcript_path
                    app.state.session_manager._save_state()

            # Auto-release locks and check for cleanup
            if app.state.session_manager:
                session = app.state.session_manager.get_session(session_manager_id)
                if session:
                    # Import lock manager functions
                    from .lock_manager import LockManager, is_worktree, get_worktree_status_hash

                    # Release all locks (silent)
                    for repo_root in session.touched_repos:
                        lock_mgr = LockManager(working_dir=repo_root)
                        lock_mgr.release_lock(repo_root, session_manager_id)
                        logger.info(f"Released lock on {repo_root} for session {session_manager_id}")

                    # Check for worktrees with uncommitted changes
                    cleanup_needed = []
                    prompt_state_changed = False
                    for repo_root in session.touched_repos:
                        if not is_worktree(repo_root):
                            continue

                        status_hash = get_worktree_status_hash(repo_root)
                        if status_hash is None:
                            # Clean worktree: clear any prior prompt state
                            if repo_root in session.cleanup_prompted:
                                del session.cleanup_prompted[repo_root]
                                prompt_state_changed = True
                            continue

                        if session.cleanup_prompted.get(repo_root) == status_hash:
                            continue

                        cleanup_needed.append((repo_root, status_hash))

                    # Inject cleanup prompt if needed
                    if cleanup_needed:
                        paths_str = "\n".join(f"  - {p}" for p, _ in cleanup_needed)
                        cleanup_prompt = f"""You have uncommitted changes in worktree(s):
{paths_str}

Please choose:
1. Push to branch and create PR: git push -u origin HEAD && gh pr create
2. Push branch only: git push -u origin HEAD
3. Abandon changes: git worktree remove <path>

Or continue working if not done yet."""

                        # Send cleanup prompt to session
                        await app.state.session_manager.send_input(
                            session_manager_id,
                            cleanup_prompt,
                            delivery_mode="important"
                        )
                        for repo_root, status_hash in cleanup_needed:
                            session.cleanup_prompted[repo_root] = status_hash
                        app.state.session_manager._save_state()
                        logger.info(f"Sent cleanup prompt for {len(cleanup_needed)} worktree(s)")
                    elif prompt_state_changed:
                        app.state.session_manager._save_state()

        if hook_event == "Stop" and not last_message and session_manager_id:
            # Transcript was empty/whitespace-only at Stop time (race condition:
            # file not flushed yet). Track this so we can send a deferred
            # notification when the idle_prompt Notification hook arrives with
            # the real content ~60s later.
            app.state.pending_stop_notifications.add(session_manager_id)
            logger.info(f"Stop hook for {session_manager_id} had empty transcript, deferring notification")

        if hook_event == "Stop" and last_message:
            # Send immediate notification to Telegram
            app.state.pending_stop_notifications.discard(session_manager_id)
            if app.state.notifier and app.state.session_manager:
                # Try to find the session this hook belongs to
                target_session = None

                # First try: match by session_manager_id (set when session manager launches Claude)
                # This is the most reliable - set via CLAUDE_SESSION_MANAGER_ID env var
                if session_manager_id:
                    target_session = app.state.session_manager.get_session(session_manager_id)
                    if target_session:
                        logger.info(f"Matched hook to session {session_manager_id} via environment variable")

                # If we have a reliable match, don't try other methods
                if target_session:
                    pass  # Use this session
                else:
                    # Second try: match by transcript path - but only if it hasn't been set yet
                    # (to avoid matching wrong session if transcript paths get reused)
                    if transcript_path:
                        sessions = app.state.session_manager.list_sessions()
                        for session in sessions:
                            # Only match if session has this transcript path already recorded
                            if session.transcript_path == transcript_path:
                                target_session = session
                                logger.info(f"Matched hook to session {session.id} via existing transcript path")
                                break

                    # Third try: match by claude_session_id (if provided)
                    if not target_session and claude_session_id:
                        target_session = app.state.session_manager.get_session(claude_session_id)
                        if target_session:
                            logger.info(f"Matched hook to session {claude_session_id} via claude_session_id")

                # If we found a matching session, send the notification
                if target_session and target_session.telegram_chat_id:
                    # Store transcript path for /name command (only set it once, don't overwrite)
                    if transcript_path and not target_session.transcript_path:
                        target_session.transcript_path = transcript_path
                        app.state.session_manager._save_state()

                    # Store last output under our session ID for /status
                    app.state.last_claude_output[target_session.id] = last_message

                    # Update session's last activity timestamp
                    from datetime import datetime
                    target_session.last_activity = datetime.now()
                    app.state.session_manager._save_state()

                    from .models import NotificationEvent
                    event = NotificationEvent(
                        session_id=target_session.id,
                        event_type="response",
                        message="Claude responded",
                        context=last_message,
                        urgent=False,
                    )
                    await app.state.notifier.notify(event, target_session)
                    # Mark response sent (starts idle cooldown)
                    if app.state.output_monitor:
                        app.state.output_monitor.mark_response_sent(target_session.id)

                    # If session has a review_config, emit review_complete notification
                    if target_session.review_config:
                        try:
                            from .review_parser import parse_tui_output, parse_github_review
                            review_config = target_session.review_config

                            review_result = None
                            if review_config.mode == "pr" and review_config.pr_repo and review_config.pr_number:
                                # PR mode: fetch from GitHub (async)
                                import asyncio
                                from .github_reviews import fetch_latest_codex_review
                                codex_review = await asyncio.to_thread(
                                    fetch_latest_codex_review,
                                    review_config.pr_repo,
                                    review_config.pr_number,
                                )
                                if codex_review:
                                    review_result = await asyncio.to_thread(
                                        parse_github_review,
                                        review_config.pr_repo,
                                        review_config.pr_number,
                                        codex_review,
                                    )
                            else:
                                # TUI mode: parse from last message
                                review_result = parse_tui_output(last_message)

                            if review_result and review_result.findings:
                                review_event = NotificationEvent(
                                    session_id=target_session.id,
                                    event_type="review_complete",
                                    message="Review complete",
                                    context="",
                                    urgent=False,
                                )
                                review_event.review_result = review_result
                                await app.state.notifier.notify(review_event, target_session)
                        except Exception as e:
                            logger.warning(f"Failed to emit review_complete notification: {e}")
                else:
                    # Couldn't find matching session
                    logger.warning(
                        f"Stop hook: Could not find matching session for "
                        f"claude_session_id={claude_session_id}, "
                        f"transcript_path={transcript_path}"
                    )

        # Handle Notification hook (permission prompts, idle, etc.)
        elif hook_event == "Notification":
            notification_type = payload.get("notification_type")
            message = payload.get("message", "")
            logger.info(f"Claude notification: {notification_type} - {message}")

            # idle_prompt: normally skip, but send deferred response if the
            # preceding Stop hook had an empty transcript (race condition).
            if notification_type == "idle_prompt":
                sid = session_manager_id
                if sid and sid in app.state.pending_stop_notifications and last_message:
                    app.state.pending_stop_notifications.discard(sid)
                    logger.info(f"Sending deferred response notification for {sid} (idle_prompt had content)")
                    if app.state.notifier and app.state.session_manager:
                        target_session = app.state.session_manager.get_session(sid)
                        if target_session and target_session.telegram_chat_id:
                            # Persist transcript path (same as immediate Stop path)
                            if transcript_path and not target_session.transcript_path:
                                target_session.transcript_path = transcript_path
                            app.state.last_claude_output[target_session.id] = last_message
                            from datetime import datetime
                            target_session.last_activity = datetime.now()
                            app.state.session_manager._save_state()
                            from .models import NotificationEvent
                            event = NotificationEvent(
                                session_id=target_session.id,
                                event_type="response",
                                message="Claude responded",
                                context=last_message,
                                urgent=False,
                            )
                            await app.state.notifier.notify(event, target_session)
                            if app.state.output_monitor:
                                app.state.output_monitor.mark_response_sent(target_session.id)
                else:
                    # Only clear pending state if the session is NOT awaiting
                    # a deferred notification (i.e. it was never pending, or
                    # was pending but last_message is empty — keep it pending
                    # so a later hook can still deliver the content).
                    if sid and sid not in app.state.pending_stop_notifications:
                        logger.debug(f"Skipping idle_prompt notification for {sid} (filtered out)")
                    elif sid and sid in app.state.pending_stop_notifications:
                        logger.info(f"idle_prompt for {sid} had empty transcript, keeping deferred state")
                    else:
                        logger.debug(f"Skipping idle_prompt notification (filtered out)")
                return {"status": "received", "hook_event": hook_event}

            # Send notification to Telegram for permission prompts and errors
            if app.state.notifier and app.state.session_manager:
                # Try to find the session
                target_session = None

                if session_manager_id:
                    target_session = app.state.session_manager.get_session(session_manager_id)
                    if target_session:
                        logger.info(f"Found session {session_manager_id} for notification")

                if not target_session and claude_session_id:
                    target_session = app.state.session_manager.get_session(claude_session_id)
                    if target_session:
                        logger.info(f"Found session {claude_session_id} for notification")

                # Send notification if we found a session
                if target_session and target_session.telegram_chat_id:
                    from .models import NotificationEvent
                    event = NotificationEvent(
                        session_id=target_session.id,
                        event_type=notification_type,  # "permission_prompt", "idle_prompt", etc.
                        message=message,
                        context=last_message or "",
                        urgent=notification_type in ["permission_prompt", "error"],
                    )
                    await app.state.notifier.notify(event, target_session)
                else:
                    logger.warning(
                        f"Notification hook: Could not find matching session for "
                        f"session_manager_id={session_manager_id}, "
                        f"claude_session_id={claude_session_id}"
                    )

        return {"status": "received", "hook_event": hook_event}

    @app.post("/sessions/spawn")
    async def spawn_child_session(request: SpawnChildRequest):
        """Spawn a child agent session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        # Get parent session
        parent_session = app.state.session_manager.get_session(request.parent_session_id)
        if not parent_session:
            return {"error": "Parent session not found"}

        # Spawn child session
        provider = _normalize_provider(request.provider) if request.provider else None
        child_session = await app.state.session_manager.spawn_child_session(
            parent_session_id=request.parent_session_id,
            prompt=request.prompt,
            name=request.name,
            wait=request.wait,
            model=request.model,
            working_dir=request.working_dir or parent_session.working_dir,
            provider=provider,
        )

        if not child_session:
            return {"error": "Failed to spawn child session"}

        # Start monitoring the child session (tmux providers only)
        if app.state.output_monitor and getattr(child_session, "provider", "claude") != "codex-app":
            await app.state.output_monitor.start_monitoring(child_session)

        # Note: --wait monitoring is already registered by session_manager.spawn_child_session()

        return {
            "session_id": child_session.id,
            "name": child_session.name,
            "friendly_name": child_session.friendly_name,
            "working_dir": child_session.working_dir,
            "parent_session_id": child_session.parent_session_id,
            "tmux_session": child_session.tmux_session,
            "provider": getattr(child_session, "provider", "claude"),
            "created_at": child_session.created_at.isoformat(),
        }

    @app.get("/sessions/{session_id}/review-results")
    async def get_review_results(session_id: str):
        """Get parsed review results for a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        if not session.review_config:
            raise HTTPException(status_code=404, detail="No review configured for this session")

        from .review_parser import parse_github_review, parse_tui_output
        from .github_reviews import fetch_latest_codex_review

        review_config = session.review_config

        if review_config.mode == "pr" and review_config.pr_repo and review_config.pr_number:
            # GitHub PR mode: fetch review from GitHub API
            import asyncio
            repo = review_config.pr_repo
            pr_number = review_config.pr_number

            try:
                codex_review = await asyncio.to_thread(
                    fetch_latest_codex_review, repo, pr_number
                )
                if not codex_review:
                    raise HTTPException(status_code=404, detail="No Codex review found on PR")

                review_result = await asyncio.to_thread(
                    parse_github_review, repo, pr_number, codex_review
                )
                return review_result.to_dict()

            except HTTPException:
                raise
            except Exception as e:
                raise HTTPException(status_code=500, detail=f"Failed to fetch review: {e}")
        else:
            # TUI mode: capture tmux pane output
            output = app.state.session_manager.capture_output(session_id, lines=500)
            if not output:
                raise HTTPException(status_code=404, detail="No output available from session")

            review_result = parse_tui_output(output)
            return review_result.to_dict()

    @app.post("/sessions/{session_id}/review")
    async def start_review(session_id: str, request: StartReviewRequest):
        """Start a Codex review on an existing session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        result = await app.state.session_manager.start_review(
            session_id=session_id,
            mode=request.mode,
            base_branch=request.base_branch,
            commit_sha=request.commit_sha,
            custom_prompt=request.custom_prompt,
            steer_text=request.steer,
            wait=request.wait,
            watcher_session_id=request.watcher_session_id,
        )

        if result.get("error"):
            return result  # Return 200 with error payload (matches spawn flow pattern)

        return result

    @app.post("/sessions/review")
    async def spawn_review(request: SpawnReviewRequest):
        """Spawn a new Codex session and start a review."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        parent = app.state.session_manager.get_session(request.parent_session_id)
        if not parent:
            raise HTTPException(status_code=404, detail="Parent session not found")

        session = await app.state.session_manager.spawn_review_session(
            parent_session_id=request.parent_session_id,
            mode=request.mode,
            base_branch=request.base_branch,
            commit_sha=request.commit_sha,
            custom_prompt=request.custom_prompt,
            steer_text=request.steer,
            name=request.name,
            wait=request.wait,
            model=request.model,
            working_dir=request.working_dir,
        )

        if not session:
            return {"error": "Failed to spawn review session"}

        # Start monitoring
        if app.state.output_monitor:
            await app.state.output_monitor.start_monitoring(session)

        return {
            "session_id": session.id,
            "name": session.name,
            "friendly_name": session.friendly_name,
            "review_mode": request.mode,
            "base_branch": request.base_branch,
            "status": "started",
        }

    @app.post("/reviews/pr")
    async def start_pr_review(request: PRReviewRequest):
        """Trigger @codex review on a GitHub PR."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        result = await app.state.session_manager.start_pr_review(
            pr_number=request.pr_number,
            repo=request.repo,
            steer=request.steer,
            wait=request.wait,
            caller_session_id=request.caller_session_id,
        )

        if result.get("error"):
            return result  # Return 200 with error payload (matches spawn flow pattern)

        return result

    @app.get("/sessions/{parent_session_id}/children")
    async def list_children_sessions(
        parent_session_id: str,
        recursive: bool = False,
        status: Optional[str] = None,
    ):
        """List child sessions of a parent."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        # Get all sessions and filter by parent_session_id
        all_sessions = app.state.session_manager.list_sessions(include_stopped=True)
        children = [s for s in all_sessions if s.parent_session_id == parent_session_id]

        # Filter by status if specified
        if status and status != "all":
            if status == "running":
                children = [s for s in children if s.status == SessionStatus.RUNNING]
            elif status == "completed":
                from src.models import CompletionStatus
                children = [s for s in children if s.completion_status == CompletionStatus.COMPLETED]
            elif status == "error":
                from src.models import CompletionStatus
                children = [s for s in children if s.completion_status == CompletionStatus.ERROR]

        # Handle recursive
        if recursive:
            all_descendants = []
            for child in children:
                all_descendants.append(child)
                # Get grandchildren
                grandchildren = [s for s in all_sessions if s.parent_session_id == child.id]
                all_descendants.extend(grandchildren)
            children = all_descendants

        return {
            "parent_session_id": parent_session_id,
            "children": [
                {
                    "id": s.id,
                    "name": s.name,
                    "friendly_name": s.friendly_name,
                    "status": s.status.value,
                    "completion_status": s.completion_status.value if s.completion_status else None,
                    "completion_message": s.completion_message,
                    "last_activity": s.last_activity.isoformat(),
                    "spawned_at": s.spawned_at.isoformat() if s.spawned_at else None,
                    "completed_at": s.completed_at.isoformat() if s.completed_at else None,
                }
                for s in children
            ],
        }

    @app.post("/sessions/{target_session_id}/kill")
    async def kill_session_with_check(target_session_id: str, request: KillSessionRequest):
        """Kill a session with parent-child ownership check."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        # Get target session
        target_session = app.state.session_manager.get_session(target_session_id)
        if not target_session:
            return {"error": f"Session {target_session_id} not found"}

        # Check ownership if requester provided
        if request.requester_session_id:
            # Requester must be the parent
            if target_session.parent_session_id != request.requester_session_id:
                return {"error": f"Cannot kill session {target_session_id} - not your child session"}

        # Kill the session
        success = app.state.session_manager.kill_session(target_session_id)

        if not success:
            return {"error": "Failed to kill session"}

        # Perform full cleanup (Telegram, monitoring, state)
        if app.state.output_monitor:
            await app.state.output_monitor.cleanup_session(target_session)

        return {"status": "killed", "session_id": target_session_id}

    @app.get("/sessions/{session_id}/send-queue")
    async def get_send_queue(session_id: str):
        """Get pending messages for a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            return {"session_id": session_id, "is_idle": False, "pending_count": 0, "pending_messages": []}

        return queue_mgr.get_queue_status(session_id)

    @app.post("/scheduler/remind")
    async def schedule_reminder(
        session_id: str,
        delay_seconds: int,
        message: str,
    ):
        """Schedule a self-reminder for a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            raise HTTPException(status_code=503, detail="Message queue not configured")

        reminder_id = await queue_mgr.schedule_reminder(session_id, delay_seconds, message)

        return {
            "status": "scheduled",
            "reminder_id": reminder_id,
            "session_id": session_id,
            "fires_in_seconds": delay_seconds,
        }

    @app.post("/sessions/{target_session_id}/watch")
    async def watch_session(
        target_session_id: str,
        watcher_session_id: str,
        timeout_seconds: int,
    ):
        """Watch a session and notify when it goes idle or timeout."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        # Verify both sessions exist
        target_session = app.state.session_manager.get_session(target_session_id)
        if not target_session:
            raise HTTPException(status_code=404, detail="Target session not found")

        watcher_session = app.state.session_manager.get_session(watcher_session_id)
        if not watcher_session:
            raise HTTPException(status_code=404, detail="Watcher session not found")

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            raise HTTPException(status_code=503, detail="Message queue not configured")

        watch_id = await queue_mgr.watch_session(target_session_id, watcher_session_id, timeout_seconds)

        target_name = target_session.friendly_name or target_session.name or target_session_id

        return {
            "status": "watching",
            "watch_id": watch_id,
            "target_session_id": target_session_id,
            "target_name": target_name,
            "watcher_session_id": watcher_session_id,
            "timeout_seconds": timeout_seconds,
        }

    @app.post("/hooks/tool-use")
    async def hook_tool_use(request: Request):
        """
        Receive tool usage events from Claude Code hooks.
        """
        import asyncio

        start = time.monotonic()
        data = await request.json()
        parse_time = time.monotonic() - start

        # Our session ID (injected by hook script)
        session_manager_id = data.get("session_manager_id")

        # Claude Code's native fields
        claude_session_id = data.get("session_id")  # Claude's internal ID
        hook_type = data.get("hook_event_name")  # PreToolUse, PostToolUse, SubagentStart, SubagentStop
        tool_name = data.get("tool_name") or hook_type  # Fall back to hook_type for non-tool events
        tool_input = data.get("tool_input", {})
        tool_response = data.get("tool_response")  # Only for PostToolUse
        tool_use_id = data.get("tool_use_id")  # For Pre/Post correlation
        cwd = data.get("cwd")  # Working directory

        # Subagent context (if present)
        agent_id = data.get("agent_id")  # From SubagentStart context

        # Get session info if available
        session = None
        if session_manager_id and app.state.session_manager:
            session = app.state.session_manager.get_session(session_manager_id)

        # Auto-acquire lock on file write (PreToolUse for Edit/Write/NotebookEdit)
        if hook_type == "PreToolUse" and tool_name in ("Edit", "Write", "NotebookEdit"):
            file_path = tool_input.get("file_path", "")

            if file_path:
                # Resolve to absolute path
                if file_path.startswith("/"):
                    abs_path = file_path
                else:
                    abs_path = str(Path(cwd) / file_path) if cwd else file_path

                # Import lock manager functions
                from .lock_manager import get_git_root, LockManager

                # Find git repo root for this file
                repo_root = get_git_root(abs_path)

                if repo_root:
                    # Try to acquire lock
                    lock_mgr = LockManager(working_dir=repo_root)
                    lock_result = lock_mgr.try_acquire(repo_root, session_manager_id)

                    if lock_result.locked_by_other:
                        # Get the other session's friendly name
                        other_session = None
                        if app.state.session_manager:
                            other_session = app.state.session_manager.get_session(lock_result.owner_session_id)

                        other_name = other_session.friendly_name if other_session and other_session.friendly_name else lock_result.owner_session_id

                        return {
                            "status": "error",
                            "error": f"⚠️  {repo_root} is locked by session [{other_name}].\n\n"
                                     f"Work in a separate worktree:\n"
                                     f"  git worktree add ../my-feature feature-branch\n"
                                     f"  Then edit ../my-feature/{Path(abs_path).relative_to(repo_root)}"
                        }

                    # Lock acquired - track this repo
                    if session:
                        session.touched_repos.add(repo_root)
                        app.state.session_manager._save_state()

        # Track worktree creation (PreToolUse for Bash)
        if hook_type == "PreToolUse" and tool_name == "Bash" and session:
            command = tool_input.get("command", "")

            # Detect worktree creation
            if "git worktree add" in command:
                import re
                # Parse: git worktree add <path> [<branch>]
                match = re.search(r'git\s+worktree\s+add\s+([^\s]+)', command)
                if match:
                    worktree_path = match.group(1)
                    # Resolve to absolute path
                    abs_worktree = str((Path(cwd) / worktree_path).resolve()) if cwd else worktree_path
                    session.worktrees.append(abs_worktree)
                    app.state.session_manager._save_state()
                    logger.info(f"Tracked worktree creation: {abs_worktree}")

        # Log to database (fire and forget - don't block response)
        if hasattr(app.state, 'tool_logger') and app.state.tool_logger:
            # Create task but don't await - let it run in background
            asyncio.create_task(app.state.tool_logger.log(
                session_id=session_manager_id,
                claude_session_id=claude_session_id,
                session_name=session.friendly_name if session else None,
                parent_session_id=session.parent_session_id if session else None,
                hook_type=hook_type,
                tool_name=tool_name,
                tool_input=tool_input,
                tool_response=tool_response,
                tool_use_id=tool_use_id,
                cwd=cwd,
                agent_id=agent_id,
            ))

        elapsed = time.monotonic() - start
        # Get hook timing threshold from config
        config = app.state.config or {}
        hook_threshold = config.get("timeouts", {}).get("server", {}).get("hook_timing_threshold_seconds", 0.05)
        if elapsed > hook_threshold:
            logger.debug(f"hook_tool_use: parse={parse_time*1000:.1f}ms total={elapsed*1000:.1f}ms tool={tool_name}")

        return {"status": "logged"}

    return app
