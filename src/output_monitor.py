"""Async log file tailing and pattern detection for Claude sessions."""

import asyncio
import re
import logging
from datetime import datetime, timedelta
from pathlib import Path
from typing import Callable, Awaitable, Optional

from .models import Session, SessionStatus, NotificationEvent

logger = logging.getLogger(__name__)


# Patterns that indicate Claude is waiting for permission
PERMISSION_PATTERNS = [
    r'\[Y/n\]',
    r'\[y/N\]',
    r'\[Yes/no\]',
    r'Allow .+\?',
    r'Do you want to proceed\?',
    r'Permission required',
    r'Press Enter to continue',
    r'Approve\?',
    r'Run command\?',
    r'Allow once\?',
    r'\(y\)es',
    r'\(n\)o',
]

# Patterns that indicate errors
ERROR_PATTERNS = [
    r'Error:',
    r'ERROR:',
    r'error:',
    r'Failed to',
    r'Exception:',
    r'Traceback \(most recent call last\)',
    r'command not found',
    r'Permission denied',
]

# Patterns that indicate completion
COMPLETION_PATTERNS = [
    r'Task complete',
    r'Done\.',
    r'Finished\.',
    r'All tests passed',
]

# Compiled patterns for efficiency
_permission_re = re.compile('|'.join(PERMISSION_PATTERNS), re.IGNORECASE)
_error_re = re.compile('|'.join(ERROR_PATTERNS))
_completion_re = re.compile('|'.join(COMPLETION_PATTERNS), re.IGNORECASE)


class OutputMonitor:
    """Monitors Claude session output for patterns that require notification."""

    def __init__(
        self,
        idle_timeout: int = 300,  # 5 minutes
        poll_interval: float = 1.0,
        context_lines: int = 20,
        notify_errors: bool = False,
        notify_permission_prompts: bool = True,
        notify_completion: bool = False,
        notify_idle: bool = True,
    ):
        self.idle_timeout = idle_timeout
        self.poll_interval = poll_interval
        self.context_lines = context_lines
        self.notify_errors = notify_errors
        self.notify_permission_prompts = notify_permission_prompts
        self.notify_completion = notify_completion
        self.notify_idle = notify_idle

        self._event_callback: Optional[Callable[[NotificationEvent], Awaitable[None]]] = None
        self._status_callback: Optional[Callable[[str, SessionStatus], Awaitable[None]]] = None
        self._save_state_callback: Optional[Callable[[], None]] = None
        self._session_manager = None  # Reference to SessionManager for looking up sessions
        self._running = False
        self._tasks: dict[str, asyncio.Task] = {}
        self._file_positions: dict[str, int] = {}
        self._last_activity: dict[str, datetime] = {}
        self._notified_permissions: dict[str, datetime] = {}  # Debounce
        self._last_response_sent: dict[str, datetime] = {}  # Track when we sent response notifications
        self._hook_output_store: Optional[dict] = None  # Reference to hook output storage
        self._idle_cooldown = 300  # Don't send idle notifications within 5 mins of response

    def set_event_callback(self, callback: Callable[[NotificationEvent], Awaitable[None]]):
        """Set the callback for notification events."""
        self._event_callback = callback

    def set_status_callback(self, callback: Callable[[str, SessionStatus], Awaitable[None]]):
        """Set the callback for status updates."""
        self._status_callback = callback

    def set_hook_output_store(self, store: dict):
        """Set reference to hook output storage (from server)."""
        self._hook_output_store = store

    def set_save_state_callback(self, callback: Callable[[], None]):
        """Set callback to save session state."""
        self._save_state_callback = callback

    def set_session_manager(self, session_manager):
        """Set reference to SessionManager for session lookups."""
        self._session_manager = session_manager

    async def start_monitoring(self, session: Session, is_restored: bool = False):
        """Start monitoring a session's output."""
        if session.id in self._tasks:
            logger.warning(f"Already monitoring session {session.id}")
            return

        self._last_activity[session.id] = datetime.now()
        self._file_positions[session.id] = 0

        # For restored sessions, set a grace period before sending idle notifications
        # This prevents spamming idle notifications after server restart
        if is_restored:
            self._last_response_sent[session.id] = datetime.now()

        # Get initial file position (end of file)
        log_path = Path(session.log_file)
        if log_path.exists():
            self._file_positions[session.id] = log_path.stat().st_size

        task = asyncio.create_task(self._monitor_loop(session))
        self._tasks[session.id] = task
        logger.info(f"Started monitoring session {session.id}")

    async def stop_monitoring(self, session_id: str):
        """Stop monitoring a session."""
        if session_id in self._tasks:
            self._tasks[session_id].cancel()
            try:
                await self._tasks[session_id]
            except asyncio.CancelledError:
                pass
            del self._tasks[session_id]
            logger.info(f"Stopped monitoring session {session_id}")

        # Clean up state
        self._file_positions.pop(session_id, None)
        self._last_activity.pop(session_id, None)
        self._notified_permissions.pop(session_id, None)

    async def stop_all(self):
        """Stop all monitoring tasks."""
        self._running = False
        for session_id in list(self._tasks.keys()):
            await self.stop_monitoring(session_id)

    async def _monitor_loop(self, session: Session):
        """Main monitoring loop for a session."""
        log_path = Path(session.log_file)

        while True:
            try:
                await asyncio.sleep(self.poll_interval)

                # Check if log file exists
                if not log_path.exists():
                    continue

                # Read new content
                current_size = log_path.stat().st_size
                last_pos = self._file_positions.get(session.id, 0)

                if current_size > last_pos:
                    # New content available
                    with open(log_path, 'r', errors='ignore') as f:
                        f.seek(last_pos)
                        new_content = f.read()

                    self._file_positions[session.id] = current_size
                    now = datetime.now()
                    self._last_activity[session.id] = now
                    # Also update the Session model's last_activity
                    session.last_activity = now
                    # Save state to persist the update
                    if self._save_state_callback:
                        self._save_state_callback()
                    # Clear idle notification flag on new activity
                    self._notified_permissions.pop(f"{session.id}_idle", None)

                    # Analyze the new content
                    await self._analyze_content(session, new_content)

                else:
                    # No new content - check for idle
                    await self._check_idle(session)

            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Monitor error for session {session.id}: {e}")
                await asyncio.sleep(5)  # Back off on error

    async def _analyze_content(self, session: Session, content: str):
        """Analyze new content for patterns."""
        # Check for permission prompts
        if _permission_re.search(content):
            await self._handle_permission_prompt(session, content)

        # Check for errors
        if _error_re.search(content):
            await self._handle_error(session, content)

        # Check for completion
        if _completion_re.search(content):
            await self._handle_completion(session, content)

    async def _handle_permission_prompt(self, session: Session, content: str):
        """Handle detected permission prompt."""
        # Debounce - don't notify twice within 30 seconds
        last_notified = self._notified_permissions.get(session.id)
        if last_notified and datetime.now() - last_notified < timedelta(seconds=30):
            return

        self._notified_permissions[session.id] = datetime.now()

        # Update status
        if self._status_callback:
            await self._status_callback(session.id, SessionStatus.WAITING_PERMISSION)

        # Only send notification if enabled
        if not self.notify_permission_prompts:
            logger.debug(f"Permission prompt detected but notifications disabled for session {session.id}")
            return

        # Get context (last few lines)
        context = self._get_context(content)

        # Emit event
        if self._event_callback:
            event = NotificationEvent(
                session_id=session.id,
                event_type="permission_prompt",
                message="Claude is waiting for permission",
                context=context,
                urgent=True,
            )
            await self._event_callback(event)

        logger.info(f"Permission prompt detected in session {session.id}")

    async def _handle_error(self, session: Session, content: str):
        """Handle detected error."""
        if self._status_callback:
            await self._status_callback(session.id, SessionStatus.ERROR)

        # Only send notification if enabled
        if not self.notify_errors:
            logger.debug(f"Error detected but notifications disabled for session {session.id}")
            return

        context = self._get_context(content)

        if self._event_callback:
            event = NotificationEvent(
                session_id=session.id,
                event_type="error",
                message="Error detected in session",
                context=context,
                urgent=False,
            )
            await self._event_callback(event)

        logger.warning(f"Error detected in session {session.id}")

    async def _handle_completion(self, session: Session, content: str):
        """Handle detected completion."""
        # Only send notification if enabled
        if not self.notify_completion:
            logger.debug(f"Completion detected but notifications disabled for session {session.id}")
            return

        context = self._get_context(content)

        if self._event_callback:
            event = NotificationEvent(
                session_id=session.id,
                event_type="complete",
                message="Task appears to be complete",
                context=context,
                urgent=False,
            )
            await self._event_callback(event)

        logger.info(f"Completion detected in session {session.id}")

    async def _check_idle(self, session: Session):
        """Check if session has been idle too long."""
        last_activity = self._last_activity.get(session.id)
        if not last_activity:
            logger.debug(f"No last_activity for session {session.id}")
            return

        idle_duration = datetime.now() - last_activity
        logger.debug(f"Session {session.id} idle for {idle_duration.total_seconds()}s (timeout: {self.idle_timeout}s)")
        if idle_duration > timedelta(seconds=self.idle_timeout):
            # Only notify once per idle period (until activity resets it)
            notified_key = f"{session.id}_idle"
            if self._notified_permissions.get(notified_key):
                return  # Already notified, wait for activity

            # Don't send idle notification if we recently sent a response notification
            last_response = self._last_response_sent.get(session.id)
            if last_response:
                time_since_response = datetime.now() - last_response
                if time_since_response < timedelta(seconds=self._idle_cooldown):
                    logger.debug(f"Skipping idle notification - response sent {time_since_response.total_seconds()}s ago")
                    return

            self._notified_permissions[notified_key] = True

            if self._status_callback:
                await self._status_callback(session.id, SessionStatus.IDLE)

            # Only send notification if enabled
            if not self.notify_idle:
                logger.debug(f"Session {session.id} is idle but notifications disabled")
                return

            if self._event_callback:
                # Don't send context - user already got the last message via response hook
                # Including context would duplicate the entire message
                event = NotificationEvent(
                    session_id=session.id,
                    event_type="idle",
                    message=f"Session has been idle for {int(idle_duration.total_seconds())} seconds",
                    context="",  # No context to avoid duplicating the response
                    urgent=False,
                )
                await self._event_callback(event)

            logger.info(f"Session {session.id} is idle")

    def _get_context(self, content: str) -> str:
        """Extract recent context from content."""
        lines = content.strip().split('\n')
        context_lines = lines[-self.context_lines:]
        return '\n'.join(context_lines)

    def update_activity(self, session_id: str):
        """Manually update last activity time (e.g., when input is sent)."""
        now = datetime.now()
        self._last_activity[session_id] = now
        # Also update Session model if we have access to it
        if self._session_manager:
            session = self._session_manager.get_session(session_id)
            if session:
                session.last_activity = now
                if self._save_state_callback:
                    self._save_state_callback()
        # Clear idle notification flag
        notified_key = f"{session_id}_idle"
        self._notified_permissions.pop(notified_key, None)

    def mark_response_sent(self, session_id: str):
        """Mark that we sent a response notification (for idle cooldown)."""
        self._last_response_sent[session_id] = datetime.now()
        # Also update activity and clear idle flag
        self._last_activity[session_id] = datetime.now()
        notified_key = f"{session_id}_idle"
        self._notified_permissions.pop(notified_key, None)
