"""FastAPI server for hooks and API endpoints."""

import logging
from typing import Optional

from fastapi import FastAPI, HTTPException, Body
from pydantic import BaseModel

from .models import Session, SessionStatus, NotificationChannel

logger = logging.getLogger(__name__)


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


class SendInputRequest(BaseModel):
    """Request to send input to a session."""
    text: str


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


def create_app(
    session_manager=None,
    notifier=None,
    output_monitor=None,
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

    # Store references to components
    app.state.session_manager = session_manager
    app.state.notifier = notifier
    app.state.output_monitor = output_monitor
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
        )

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
        )

    @app.post("/sessions/{session_id}/input")
    async def send_input(session_id: str, request: SendInputRequest):
        """Send input to a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        success = app.state.session_manager.send_input(session_id, request.text)

        if not success:
            raise HTTPException(status_code=500, detail="Failed to send input")

        # Update activity in monitor
        if app.state.output_monitor:
            app.state.output_monitor.update_activity(session_id)

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

        # Stop monitoring
        if app.state.output_monitor:
            await app.state.output_monitor.stop_monitoring(session_id)

        success = app.state.session_manager.kill_session(session_id)

        if not success:
            raise HTTPException(status_code=500, detail="Failed to kill session")

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

        # Read last assistant message from transcript file
        last_message = None
        if transcript_path:
            try:
                transcript_file = Path(transcript_path)
                if transcript_file.exists():
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
                                    last_message = "\n".join(texts)
                                    break
                        except json.JSONDecodeError:
                            continue
            except Exception as e:
                logger.error(f"Error reading transcript: {e}")

        # Store last message
        if last_message:
            app.state.last_claude_output["latest"] = last_message
            app.state.last_claude_output[claude_session_id or "default"] = last_message
            logger.info(f"Stored Claude output: {last_message[:100]}...")

        # Handle Stop hook - Claude finished responding
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

    return app
