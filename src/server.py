"""FastAPI server for hooks and API endpoints."""

import logging
import time
from typing import Optional

from fastapi import FastAPI, HTTPException, Body, Request
from pydantic import BaseModel
from starlette.middleware.base import BaseHTTPMiddleware

from .models import Session, SessionStatus, NotificationChannel, Subagent, SubagentStatus

logger = logging.getLogger(__name__)


class RequestTimingMiddleware(BaseHTTPMiddleware):
    """Log slow requests for debugging."""

    async def dispatch(self, request: Request, call_next):
        start = time.monotonic()
        response = await call_next(request)
        elapsed = time.monotonic() - start

        # Log slow requests (>1 second)
        if elapsed > 1.0:
            logger.warning(
                f"SLOW REQUEST: {request.method} {request.url.path} "
                f"took {elapsed:.2f}s"
            )
        elif elapsed > 0.1:
            logger.info(
                f"Request: {request.method} {request.url.path} "
                f"took {elapsed*1000:.0f}ms"
            )

        return response


class CreateSessionRequest(BaseModel):
    """Request to create a new session."""
    working_dir: str = "~"
    name: Optional[str] = None


class SessionResponse(BaseModel):
    """Response containing session info."""
    id: str
    name: str
    working_dir: str
    status: str
    created_at: str
    last_activity: str
    tmux_session: str
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


class KillSessionRequest(BaseModel):
    """Request to kill a session with ownership check."""
    requester_session_id: Optional[str] = None


def create_app(
    session_manager=None,
    notifier=None,
    output_monitor=None,
    child_monitor=None,
) -> FastAPI:
    """
    Create the FastAPI application.

    Args:
        session_manager: SessionManager instance
        notifier: Notifier instance
        output_monitor: OutputMonitor instance

    Returns:
        Configured FastAPI app
    """
    app = FastAPI(
        title="Claude Session Manager",
        description="Manage Claude Code sessions with Telegram/Email notifications",
        version="0.1.0",
    )

    # Add timing middleware for debugging
    app.add_middleware(RequestTimingMiddleware)

    # Store references to components
    app.state.session_manager = session_manager
    app.state.notifier = notifier
    app.state.output_monitor = output_monitor
    app.state.child_monitor = child_monitor
    app.state.last_claude_output = {}  # Store last output per session from hooks

    @app.get("/")
    async def root():
        """Health check endpoint."""
        return {"status": "ok", "service": "claude-session-manager"}

    @app.get("/health")
    async def health():
        """Health check endpoint."""
        return {"status": "healthy"}

    @app.post("/sessions", response_model=SessionResponse)
    async def create_session(request: CreateSessionRequest):
        """Create a new Claude Code session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.create_session(
            working_dir=request.working_dir,
            name=request.name,
        )

        if not session:
            raise HTTPException(status_code=500, detail="Failed to create session")

        # Start monitoring the session
        if app.state.output_monitor:
            await app.state.output_monitor.start_monitoring(session)

        return SessionResponse(
            id=session.id,
            name=session.name,
            working_dir=session.working_dir,
            status=session.status.value,
            created_at=session.created_at.isoformat(),
            last_activity=session.last_activity.isoformat(),
            tmux_session=session.tmux_session,
            friendly_name=session.friendly_name,
            current_task=session.current_task,
            git_remote_url=session.git_remote_url,
            parent_session_id=session.parent_session_id,
        )

    @app.post("/sessions/create")
    async def create_session_endpoint(working_dir: str):
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
        session = app.state.session_manager.create_session(
            working_dir=working_dir,
            telegram_chat_id=None,  # No Telegram association
        )

        if not session:
            raise HTTPException(status_code=500, detail="Failed to create session")

        # Start monitoring (same as Telegram /new does)
        if app.state.output_monitor:
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
            session.friendly_name = friendly_name
            app.state.session_manager._save_state()
            # Update tmux status bar
            app.state.session_manager.tmux.set_status_bar(session.tmux_session, friendly_name)
            # Update Telegram topic name if applicable
            if session.telegram_topic_id and app.state.notifier:
                await app.state.notifier.rename_session_topic(session, friendly_name)

        return SessionResponse(
            id=session.id,
            name=session.name,
            working_dir=session.working_dir,
            status=session.status.value,
            created_at=session.created_at.isoformat(),
            last_activity=session.last_activity.isoformat(),
            tmux_session=session.tmux_session,
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

        success = app.state.session_manager.send_input(
            session_id,
            request.text,
            sender_session_id=request.sender_session_id,
            delivery_mode=request.delivery_mode,
            from_sm_send=request.from_sm_send,
            timeout_seconds=request.timeout_seconds,
            notify_on_delivery=request.notify_on_delivery,
            notify_after_seconds=request.notify_after_seconds,
        )

        if not success:
            raise HTTPException(status_code=500, detail="Failed to send input")

        # Update activity in monitor
        if app.state.output_monitor:
            app.state.output_monitor.update_activity(session_id)

        # For queued messages, return queue info
        if request.delivery_mode == "sequential":
            queue_mgr = app.state.session_manager.message_queue_manager
            if queue_mgr and not queue_mgr.is_session_idle(session_id):
                queue_len = queue_mgr.get_queue_length(session_id)
                return {
                    "status": "queued",
                    "session_id": session_id,
                    "queue_position": queue_len,
                    "delivery_mode": request.delivery_mode,
                    "estimated_delivery": "waiting_for_idle",
                }

        return {"status": "sent", "session_id": session_id}

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
                    timeout=60
                )
                result_stdout = stdout.decode('utf-8')
                result_stderr = stderr.decode('utf-8')
                returncode = proc.returncode
            except asyncio.TimeoutError:
                proc.kill()
                await proc.wait()
                raise HTTPException(status_code=504, detail="Summary generation timed out (60s)")

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
                """Read transcript file synchronously (runs in thread pool)."""
                try:
                    transcript_file = Path(transcript_path)
                    if not transcript_file.exists():
                        return None
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
                                if texts:
                                    return "\n".join(texts)
                        except json.JSONDecodeError:
                            continue
                except Exception as e:
                    logger.error(f"Error reading transcript: {e}")
                return None

            try:
                last_message = await asyncio.to_thread(read_transcript)
            except Exception as e:
                logger.error(f"Error reading transcript in thread: {e}")

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

        if hook_event == "Stop" and last_message:
            # Send immediate notification to Telegram
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

            # Skip idle notifications - user doesn't want them (gets Stop hooks anyway)
            if notification_type == "idle_prompt":
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
        child_session = app.state.session_manager.spawn_child_session(
            parent_session_id=request.parent_session_id,
            prompt=request.prompt,
            name=request.name,
            wait=request.wait,
            model=request.model,
            working_dir=request.working_dir or parent_session.working_dir,
        )

        if not child_session:
            return {"error": "Failed to spawn child session"}

        # Start monitoring the child session
        if app.state.output_monitor:
            await app.state.output_monitor.start_monitoring(child_session)

        # Register for --wait monitoring if specified
        if request.wait and app.state.child_monitor:
            app.state.child_monitor.register_child(
                child_session_id=child_session.id,
                parent_session_id=request.parent_session_id,
                wait_seconds=request.wait,
            )

        return {
            "session_id": child_session.id,
            "name": child_session.name,
            "friendly_name": child_session.friendly_name,
            "working_dir": child_session.working_dir,
            "parent_session_id": child_session.parent_session_id,
            "tmux_session": child_session.tmux_session,
            "created_at": child_session.created_at.isoformat(),
        }

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
                children = [s for s in children if s.completion_status == "completed"]
            elif status == "error":
                children = [s for s in children if s.completion_status == "error"]

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
                    "completion_status": s.completion_status,
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
        if elapsed > 0.05:  # Log if > 50ms
            logger.debug(f"hook_tool_use: parse={parse_time*1000:.1f}ms total={elapsed*1000:.1f}ms tool={tool_name}")

        return {"status": "logged"}

    return app
