"""Session registry and lifecycle management."""

import asyncio
import json
import logging
import subprocess
from datetime import datetime
from pathlib import Path
from typing import Optional, Callable, Awaitable

from .models import Session, SessionStatus, NotificationEvent
from .tmux_controller import TmuxController

logger = logging.getLogger(__name__)


class SessionManager:
    """Manages the lifecycle of Claude Code sessions."""

    def __init__(
        self,
        log_dir: str = "/tmp/claude-sessions",
        state_file: str = "/tmp/claude-sessions/sessions.json",
    ):
        self.log_dir = Path(log_dir)
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self.state_file = Path(state_file)

        self.tmux = TmuxController(log_dir=log_dir)
        self.sessions: dict[str, Session] = {}
        self._event_handlers: list[Callable[[NotificationEvent], Awaitable[None]]] = []

        # Load existing sessions from state file
        self._load_state()

    def _load_state(self):
        """Load session state from disk."""
        if self.state_file.exists():
            try:
                with open(self.state_file) as f:
                    data = json.load(f)
                for session_data in data.get("sessions", []):
                    session = Session.from_dict(session_data)
                    # Verify tmux session still exists
                    if self.tmux.session_exists(session.tmux_session):
                        self.sessions[session.id] = session
                        logger.info(f"Restored session: {session.name}")
                    else:
                        logger.warning(f"Session {session.name} no longer exists in tmux")
            except Exception as e:
                logger.error(f"Failed to load state: {e}")

    def _save_state(self):
        """Save session state to disk."""
        try:
            data = {
                "sessions": [s.to_dict() for s in self.sessions.values()]
            }
            with open(self.state_file, "w") as f:
                json.dump(data, f, indent=2)
        except Exception as e:
            logger.error(f"Failed to save state: {e}")

    def add_event_handler(self, handler: Callable[[NotificationEvent], Awaitable[None]]):
        """Register a handler for session events."""
        self._event_handlers.append(handler)

    async def _emit_event(self, event: NotificationEvent):
        """Emit an event to all registered handlers."""
        for handler in self._event_handlers:
            try:
                await handler(event)
            except Exception as e:
                logger.error(f"Event handler error: {e}")

    def _get_git_remote_url(self, working_dir: str) -> Optional[str]:
        """
        Get the git remote URL for a working directory.

        Args:
            working_dir: Directory to check

        Returns:
            Git remote URL or None if not a git repo
        """
        try:
            working_path = Path(working_dir).expanduser().resolve()
            result = subprocess.run(
                ["git", "config", "--get", "remote.origin.url"],
                cwd=working_path,
                capture_output=True,
                text=True,
                timeout=2,
            )
            if result.returncode == 0:
                return result.stdout.strip()
            return None
        except Exception as e:
            logger.debug(f"Failed to get git remote for {working_dir}: {e}")
            return None

    def create_session(
        self,
        working_dir: str,
        name: Optional[str] = None,
        telegram_chat_id: Optional[int] = None,
    ) -> Optional[Session]:
        """
        Create a new Claude Code session.

        Args:
            working_dir: Directory to run Claude in
            name: Optional session name (generated if not provided)
            telegram_chat_id: Telegram chat to associate with session

        Returns:
            Created Session or None on failure
        """
        session = Session(
            working_dir=working_dir,
            telegram_chat_id=telegram_chat_id,
        )

        # Detect git remote URL for repo matching
        session.git_remote_url = self._get_git_remote_url(working_dir)

        if name:
            session.name = name
            session.tmux_session = name

        # Set up log file path
        session.log_file = str(self.log_dir / f"{session.name}.log")

        # Create the tmux session (pass session ID so Claude hooks can identify it)
        if not self.tmux.create_session(
            session.tmux_session,
            working_dir,
            session.log_file,
            session_id=session.id,
        ):
            logger.error(f"Failed to create tmux session for {session.name}")
            return None

        session.status = SessionStatus.RUNNING
        self.sessions[session.id] = session
        self._save_state()

        logger.info(f"Created session {session.name} (id={session.id})")
        return session

    def get_session(self, session_id: str) -> Optional[Session]:
        """Get a session by ID."""
        return self.sessions.get(session_id)

    def get_session_by_name(self, name: str) -> Optional[Session]:
        """Get a session by name."""
        for session in self.sessions.values():
            if session.name == name:
                return session
        return None

    def get_session_by_telegram_chat(self, chat_id: int) -> list[Session]:
        """Get all sessions associated with a Telegram chat."""
        return [s for s in self.sessions.values() if s.telegram_chat_id == chat_id]

    def get_session_by_telegram_thread(self, chat_id: int, message_id: int) -> Optional[Session]:
        """Get session by Telegram thread (root message ID)."""
        for session in self.sessions.values():
            if session.telegram_chat_id == chat_id and session.telegram_root_msg_id == message_id:
                return session
        return None

    def list_sessions(self, include_stopped: bool = False) -> list[Session]:
        """List all sessions."""
        sessions = list(self.sessions.values())
        if not include_stopped:
            sessions = [s for s in sessions if s.status != SessionStatus.STOPPED]
        return sessions

    def update_session_status(self, session_id: str, status: SessionStatus, error_message: Optional[str] = None):
        """Update a session's status."""
        session = self.sessions.get(session_id)
        if session:
            session.status = status
            session.last_activity = datetime.now()
            if error_message:
                session.error_message = error_message
            self._save_state()

    def update_telegram_thread(self, session_id: str, chat_id: int, message_id: int):
        """Associate a Telegram thread with a session."""
        session = self.sessions.get(session_id)
        if session:
            session.telegram_chat_id = chat_id
            session.telegram_root_msg_id = message_id
            self._save_state()

    def send_input(self, session_id: str, text: str) -> bool:
        """Send input to a session."""
        session = self.sessions.get(session_id)
        if not session:
            logger.error(f"Session not found: {session_id}")
            return False

        success = self.tmux.send_input(session.tmux_session, text)
        if success:
            session.last_activity = datetime.now()
            session.status = SessionStatus.RUNNING
            self._save_state()

        return success

    def send_key(self, session_id: str, key: str) -> bool:
        """Send a key to a session (e.g., 'y', 'n')."""
        session = self.sessions.get(session_id)
        if not session:
            logger.error(f"Session not found: {session_id}")
            return False

        success = self.tmux.send_key(session.tmux_session, key)
        if success:
            session.last_activity = datetime.now()
            session.status = SessionStatus.RUNNING
            self._save_state()

        return success

    def kill_session(self, session_id: str) -> bool:
        """Kill a session."""
        session = self.sessions.get(session_id)
        if not session:
            logger.error(f"Session not found: {session_id}")
            return False

        self.tmux.kill_session(session.tmux_session)
        session.status = SessionStatus.STOPPED
        self._save_state()

        logger.info(f"Killed session {session.name}")
        return True

    def open_terminal(self, session_id: str) -> bool:
        """Open a session in Terminal.app."""
        session = self.sessions.get(session_id)
        if not session:
            logger.error(f"Session not found: {session_id}")
            return False

        return self.tmux.open_in_terminal(session.tmux_session)

    def capture_output(self, session_id: str, lines: int = 50) -> Optional[str]:
        """Capture recent output from a session."""
        session = self.sessions.get(session_id)
        if not session:
            return None

        return self.tmux.capture_pane(session.tmux_session, lines)

    def cleanup_dead_sessions(self):
        """Remove sessions that no longer exist in tmux."""
        dead_sessions = []
        for session_id, session in self.sessions.items():
            if not self.tmux.session_exists(session.tmux_session):
                dead_sessions.append(session_id)

        for session_id in dead_sessions:
            session = self.sessions[session_id]
            session.status = SessionStatus.STOPPED
            logger.info(f"Marked dead session as stopped: {session.name}")

        if dead_sessions:
            self._save_state()
