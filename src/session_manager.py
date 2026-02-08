"""Session registry and lifecycle management."""

import asyncio
import json
import logging
import subprocess
from datetime import datetime
from pathlib import Path
from typing import Optional, Callable, Awaitable

from .models import Session, SessionStatus, NotificationEvent, DeliveryResult
from .tmux_controller import TmuxController
from .codex_app_server import CodexAppServerSession, CodexAppServerConfig, CodexAppServerError

logger = logging.getLogger(__name__)


class SessionManager:
    """Manages the lifecycle of Claude Code sessions."""

    def __init__(
        self,
        log_dir: str = "/tmp/claude-sessions",
        state_file: str = "/tmp/claude-sessions/sessions.json",
        config: Optional[dict] = None,
    ):
        self.log_dir = Path(log_dir)
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self.state_file = Path(state_file)
        self.config = config or {}

        self.tmux = TmuxController(log_dir=log_dir)
        self.sessions: dict[str, Session] = {}
        self._event_handlers: list[Callable[[NotificationEvent], Awaitable[None]]] = []
        self.codex_sessions: dict[str, CodexAppServerSession] = {}
        self.codex_turns_in_flight: set[str] = set()
        self.hook_output_store: Optional[dict] = None

        codex_config = self.config.get("codex", {})
        codex_app_config = self.config.get("codex_app_server", codex_config)

        self.codex_cli_command = codex_config.get("command", "codex")
        self.codex_cli_args = codex_config.get("args", [])
        self.codex_default_model = codex_config.get("default_model")

        # App-server config (can be overridden by codex_app_server section)
        self.codex_config = CodexAppServerConfig(
            command=codex_app_config.get("command", self.codex_cli_command),
            args=codex_app_config.get("app_server_args", codex_app_config.get("args", [])),
            default_model=codex_app_config.get("default_model", self.codex_default_model),
            approval_policy=codex_app_config.get("approval_policy", "never"),
            sandbox=codex_app_config.get("sandbox", "workspace-write"),
            approval_decision=codex_app_config.get("approval_decision", "decline"),
            request_timeout_seconds=codex_app_config.get("request_timeout_seconds", 60),
            client_name=codex_app_config.get("client_name", "claude-session-manager"),
            client_title=codex_app_config.get("client_title", "Claude Session Manager"),
            client_version=codex_app_config.get("client_version", "0.1.0"),
        )

        # Message queue manager (set by main app)
        self.message_queue_manager = None

        # Child monitor (set by main app)
        self.child_monitor = None

        # Load existing sessions from state file
        self._load_state()

    def _load_state(self) -> bool:
        """
        Load session state from disk.

        Returns:
            True if state loaded successfully (or no state file exists),
            False if an error occurred during loading.
        """
        if self.state_file.exists():
            try:
                with open(self.state_file) as f:
                    data = json.load(f)
                legacy_codex_sessions: list[dict] = []
                cleaned_sessions: list[dict] = []
                for session_data in data.get("sessions", []):
                    raw_provider = session_data.get("provider")
                    raw_tmux_session = session_data.get("tmux_session")
                    raw_log_file = session_data.get("log_file")
                    raw_codex_thread_id = session_data.get("codex_thread_id")
                    is_legacy_codex_app = (
                        raw_provider == "codex"
                        and (
                            raw_codex_thread_id is not None
                            or (not raw_tmux_session and not raw_log_file)
                        )
                    )
                    if is_legacy_codex_app:
                        legacy_codex_sessions.append(session_data)
                        name = session_data.get("name") or session_data.get("id", "unknown")
                        logger.warning(
                            f"Dropping legacy codex app session from state: {name}"
                        )
                        continue
                    cleaned_sessions.append(session_data)
                    session = Session.from_dict(session_data)
                    # Codex app-server sessions are restored without tmux
                    if session.provider == "codex-app":
                        self.sessions[session.id] = session
                        logger.info(f"Restored codex app session: {session.name}")
                        continue

                    # Verify tmux session still exists (Claude/Codex CLI)
                    if self.tmux.session_exists(session.tmux_session):
                        self.sessions[session.id] = session
                        logger.info(f"Restored session: {session.name}")
                    else:
                        logger.warning(f"Session {session.name} no longer exists in tmux")
                if legacy_codex_sessions:
                    self._rewrite_state_raw(cleaned_sessions)
                return True
            except Exception as e:
                logger.error(f"CRITICAL: Failed to load state from {self.state_file}: {e}")
                logger.error(f"Session state may be lost! Please check {self.state_file}")
                return False
        return True  # No state file is not an error

    def _rewrite_state_raw(self, sessions_data: list[dict]) -> bool:
        """Rewrite state file with provided session data (used for one-time cleanup)."""
        try:
            data = {"sessions": sessions_data}
            state_path = Path(self.state_file)
            temp_file = state_path.with_suffix(".tmp")

            with open(temp_file, "w") as f:
                json.dump(data, f, indent=2)

            temp_file.rename(state_path)
            logger.info("State file rewritten to drop legacy codex app sessions.")
            return True
        except Exception as e:
            logger.error(f"CRITICAL: Failed to rewrite state file {self.state_file}: {e}")
            return False

    def _save_state(self) -> bool:
        """
        Save session state to disk using atomic file operations.

        Uses temp file + rename to ensure atomic writes and prevent race conditions
        when multiple async tasks call this method concurrently.

        Returns:
            True if state saved successfully, False if an error occurred.
        """
        try:
            data = {
                "sessions": [s.to_dict() for s in self.sessions.values()]
            }

            # Write to temporary file first
            state_path = Path(self.state_file)
            temp_file = state_path.with_suffix('.tmp')

            with open(temp_file, "w") as f:
                json.dump(data, f, indent=2)

            # Atomic rename (POSIX guarantees atomicity)
            temp_file.rename(state_path)
            return True

        except Exception as e:
            logger.error(f"CRITICAL: Failed to save state to {self.state_file}: {e}")
            logger.error(f"Session state NOT persisted! Data may be lost on restart.")
            # Clean up temp file if it exists
            try:
                temp_file = Path(self.state_file).with_suffix('.tmp')
                if temp_file.exists():
                    temp_file.unlink()
            except Exception:
                pass
            return False

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

    async def _get_git_remote_url_async(self, working_dir: str) -> Optional[str]:
        """
        Get the git remote URL for a working directory (async, non-blocking).

        Args:
            working_dir: Directory to check

        Returns:
            Git remote URL or None if not a git repo
        """
        try:
            working_path = Path(working_dir).expanduser().resolve()
            proc = await asyncio.create_subprocess_exec(
                "git", "config", "--get", "remote.origin.url",
                cwd=working_path,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=2)
            if proc.returncode == 0:
                return stdout.decode().strip()
            return None
        except Exception as e:
            logger.debug(f"Failed to get git remote for {working_dir}: {e}")
            return None

    def _get_git_remote_url(self, working_dir: str) -> Optional[str]:
        """
        Get the git remote URL for a working directory (sync wrapper).

        DEPRECATED: Use _get_git_remote_url_async() in async contexts.
        This sync version is kept for backward compatibility but should not be used.

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

    async def _create_session_common(
        self,
        working_dir: str,
        name: Optional[str] = None,
        friendly_name: Optional[str] = None,
        telegram_chat_id: Optional[int] = None,
        parent_session_id: Optional[str] = None,
        spawn_prompt: Optional[str] = None,
        model: Optional[str] = None,
        initial_prompt: Optional[str] = None,
        provider: str = "claude",
    ) -> Optional[Session]:
        """
        Common session creation logic (private method).

        Args:
            working_dir: Directory to run Claude in
            name: Optional session name (generated if not provided)
            friendly_name: Optional user-friendly name
            telegram_chat_id: Telegram chat to associate with session
            parent_session_id: Parent session ID (for child sessions)
            spawn_prompt: Initial prompt used to spawn (for child sessions)
            model: Model override (opus, sonnet, haiku)
            initial_prompt: Initial prompt to send after creation

        Returns:
            Created Session or None on failure
        """
        # Create session object with common fields
        session = Session(
            working_dir=working_dir,
            friendly_name=friendly_name,
            telegram_chat_id=telegram_chat_id,
            parent_session_id=parent_session_id,
            spawn_prompt=spawn_prompt,
            spawned_at=datetime.now() if parent_session_id else None,
            provider=provider,
        )

        # Set name if provided, otherwise __post_init__ generates claude-{id}
        if name:
            session.name = name

        # Detect git remote URL for repo matching (async to avoid blocking)
        session.git_remote_url = await self._get_git_remote_url_async(working_dir)

        # Set up log file path and tmux session for CLI providers
        if provider in ("claude", "codex"):
            session.log_file = str(self.log_dir / f"{session.name}.log")

            if provider == "claude":
                # Get Claude config
                claude_config = self.config.get("claude", {})
                command = claude_config.get("command", "claude")
                args = claude_config.get("args", [])
                default_model = claude_config.get("default_model", "sonnet")
            else:
                # Codex CLI config
                command = self.codex_cli_command
                args = self.codex_cli_args
                default_model = self.codex_default_model

            # Select model (override or default)
            selected_model = model or default_model

            # Create the tmux session with config args
            # NOTE: session.tmux_session is auto-set by __post_init__ to {provider}-{id}
            if not self.tmux.create_session_with_command(
                session.tmux_session,
                working_dir,
                session.log_file,
                session_id=session.id,
                command=command,
                args=args,
                model=selected_model if model else None,  # Only pass if explicitly set
                initial_prompt=initial_prompt,
            ):
                logger.error(f"Failed to create tmux session for {session.name}")
                return None
        elif provider == "codex-app":
            try:
                codex_session = CodexAppServerSession(
                    session_id=session.id,
                    working_dir=working_dir,
                    config=self.codex_config,
                    on_turn_complete=self._handle_codex_turn_complete,
                    on_turn_started=self._handle_codex_turn_started,
                    on_turn_delta=self._handle_codex_turn_delta,
                )
                thread_id = await codex_session.start(thread_id=session.codex_thread_id, model=model)
                session.codex_thread_id = thread_id
                if initial_prompt:
                    try:
                        await codex_session.send_user_turn(initial_prompt, model=model)
                        session.last_activity = datetime.now()
                    except Exception:
                        await codex_session.close()
                        raise
                self.codex_sessions[session.id] = codex_session
            except CodexAppServerError as e:
                logger.error(f"Failed to start Codex app-server session for {session.name}: {e}")
                return None
            except Exception as e:
                logger.error(f"Unexpected error starting Codex app session: {e}")
                return None
        else:
            logger.error(f"Unknown session provider: {provider}")
            return None

        # Mark as running and save
        if provider == "codex-app" and not initial_prompt:
            session.status = SessionStatus.IDLE
        else:
            session.status = SessionStatus.RUNNING
        self.sessions[session.id] = session
        self._save_state()

        if provider == "codex-app" and not initial_prompt and self.message_queue_manager:
            self.message_queue_manager.mark_session_idle(session.id)

        # Log creation
        if parent_session_id:
            logger.info(f"Spawned child session {session.name} (id={session.id}, parent={parent_session_id})")
        else:
            logger.info(f"Created session {session.name} (id={session.id})")

        return session

    async def create_session(
        self,
        working_dir: str,
        name: Optional[str] = None,
        telegram_chat_id: Optional[int] = None,
        provider: str = "claude",
    ) -> Optional[Session]:
        """
        Create a new Claude Code session (async, non-blocking).

        Args:
            working_dir: Directory to run Claude in
            name: Optional session name (generated if not provided)
            telegram_chat_id: Telegram chat to associate with session

        Returns:
            Created Session or None on failure
        """
        return await self._create_session_common(
            working_dir=working_dir,
            name=name,
            telegram_chat_id=telegram_chat_id,
            provider=provider,
        )

    async def spawn_child_session(
        self,
        parent_session_id: str,
        prompt: str,
        name: Optional[str] = None,
        wait: Optional[int] = None,
        model: Optional[str] = None,
        working_dir: Optional[str] = None,
        provider: Optional[str] = None,
    ) -> Optional[Session]:
        """
        Spawn a child agent session.

        Args:
            parent_session_id: Parent session ID
            prompt: Initial prompt for the child agent
            name: Friendly name for the child session
            wait: Monitor child and notify when complete or idle for N seconds
            model: Model override (opus, sonnet, haiku)
            working_dir: Working directory (defaults to parent's directory)

        Returns:
            Created child Session or None on failure
        """
        # Get parent session
        parent_session = self.sessions.get(parent_session_id)
        if not parent_session:
            logger.error(f"Parent session not found: {parent_session_id}")
            return None

        # Determine working directory
        child_working_dir = working_dir or parent_session.working_dir

        # Generate session name if not provided
        # Use friendly_name parameter, auto-generate session.name if needed
        # Take first 6 chars of parent ID for brevity (session IDs are 8-char UUIDs)
        session_name = f"child-{parent_session_id[:6]}" if not name else None

        # Select provider (default to parent)
        selected_provider = provider or parent_session.provider or "claude"

        # Create session using common logic
        session = await self._create_session_common(
            working_dir=child_working_dir,
            name=session_name,
            friendly_name=name,
            parent_session_id=parent_session_id,
            spawn_prompt=prompt,
            model=model,
            initial_prompt=prompt,
            provider=selected_provider,
        )

        if not session:
            return None

        # Register background monitoring if wait is specified
        if wait and self.child_monitor:
            self.child_monitor.register_child(
                child_session_id=session.id,
                parent_session_id=parent_session_id,
                wait_seconds=wait,
            )

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
        """Get session by Telegram thread (thread ID)."""
        for session in self.sessions.values():
            if session.telegram_chat_id == chat_id and session.telegram_thread_id == message_id:
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
            session.telegram_thread_id = message_id
            self._save_state()

    async def send_input(
        self,
        session_id: str,
        text: str,
        sender_session_id: Optional[str] = None,
        delivery_mode: str = "sequential",
        from_sm_send: bool = False,
        timeout_seconds: Optional[int] = None,
        notify_on_delivery: bool = False,
        notify_after_seconds: Optional[int] = None,
        notify_on_stop: bool = False,
        bypass_queue: bool = False,
    ) -> DeliveryResult:
        """
        Send input to a session with optional sender metadata and delivery mode.

        Args:
            session_id: Target session ID
            text: Text to send
            sender_session_id: Optional ID of sending session (for metadata)
            delivery_mode: Delivery mode (sequential, important, urgent)
            from_sm_send: True if called from sm send command (triggers notification)
            timeout_seconds: Drop message if not delivered in this time
            notify_on_delivery: Notify sender when delivered
            notify_after_seconds: Notify sender N seconds after delivery
            notify_on_stop: Notify sender when receiver's Stop hook fires
            bypass_queue: If True, send directly to tmux (for permission responses)

        Returns:
            DeliveryResult indicating whether message was DELIVERED, QUEUED, or FAILED
        """
        session = self.sessions.get(session_id)
        if not session:
            logger.error(f"Session not found: {session_id}")
            return DeliveryResult.FAILED

        # For permission responses, bypass queue and send directly
        if bypass_queue:
            logger.info(f"Bypassing queue for direct send to {session_id}: {text}")
            success = await self._deliver_direct(session, text)
            if success:
                session.last_activity = datetime.now()
            return DeliveryResult.DELIVERED if success else DeliveryResult.FAILED

        # Format message with sender metadata if provided
        sender_name = None
        if sender_session_id:
            sender_session = self.sessions.get(sender_session_id)
            if sender_session:
                sender_name = sender_session.friendly_name or sender_session.name or sender_session_id
                formatted_text = f"[Input from: {sender_name} ({sender_session_id[:8]}) via sm send]\n{text}"
            else:
                # Sender session not found, send without metadata
                formatted_text = text
        else:
            formatted_text = text

        # Send Telegram notification if from sm send
        # Note: notifier will be set by server when calling send_input
        if from_sm_send and sender_session_id and hasattr(self, 'notifier'):
            asyncio.create_task(self._notify_sm_send(
                sender_session_id=sender_session_id,
                recipient_session_id=session_id,
                text=text,
                delivery_mode=delivery_mode,
                notifier=self.notifier,
            ))

        # Handle delivery modes using the message queue manager
        if self.message_queue_manager:
            # Check if session is idle (will be delivered immediately)
            state = self.message_queue_manager.delivery_states.get(session_id)
            is_idle = state.is_idle if state else True  # Assume idle if no state yet

            # For sequential mode, always queue (queue manager handles idle detection)
            if delivery_mode == "sequential":
                self.message_queue_manager.queue_message(
                    target_session_id=session_id,
                    text=formatted_text,
                    sender_session_id=sender_session_id,
                    sender_name=sender_name,
                    delivery_mode=delivery_mode,
                    timeout_seconds=timeout_seconds,
                    notify_on_delivery=notify_on_delivery,
                    notify_after_seconds=notify_after_seconds,
                    notify_on_stop=notify_on_stop,
                )
                # Return DELIVERED if idle (will be delivered immediately), else QUEUED
                return DeliveryResult.DELIVERED if is_idle else DeliveryResult.QUEUED

            # For important/urgent, queue handles delivery logic
            if delivery_mode in ("important", "urgent"):
                self.message_queue_manager.queue_message(
                    target_session_id=session_id,
                    text=formatted_text,
                    sender_session_id=sender_session_id,
                    sender_name=sender_name,
                    delivery_mode=delivery_mode,
                    timeout_seconds=timeout_seconds,
                    notify_on_delivery=notify_on_delivery,
                    notify_after_seconds=notify_after_seconds,
                    notify_on_stop=notify_on_stop,
                )
                # Urgent always delivers (sends Escape first), important waits
                if delivery_mode == "urgent":
                    return DeliveryResult.DELIVERED
                return DeliveryResult.DELIVERED if is_idle else DeliveryResult.QUEUED

        # Fallback: send immediately (no queue manager or unknown mode)
        success = await self._deliver_direct(session, formatted_text)
        if success:
            session.last_activity = datetime.now()
            session.status = SessionStatus.RUNNING
            self._save_state()

        return DeliveryResult.DELIVERED if success else DeliveryResult.FAILED

    async def _notify_sm_send(
        self,
        sender_session_id: str,
        recipient_session_id: str,
        text: str,
        delivery_mode: str,
        notifier=None,
    ):
        """
        Send Telegram notification about sm send message.

        Args:
            sender_session_id: Sender session ID
            recipient_session_id: Recipient session ID
            text: Message text
            delivery_mode: Delivery mode (sequential, important, urgent)
            notifier: Notifier instance (passed from server)
        """
        recipient_session = self.sessions.get(recipient_session_id)
        sender_session = self.sessions.get(sender_session_id)

        if not recipient_session or not sender_session:
            return

        # Only notify if recipient has Telegram configured
        if not recipient_session.telegram_chat_id:
            return

        # Need notifier to send Telegram messages
        if not notifier:
            logger.warning(f"No notifier available for sm_send notification")
            return

        # Get sender friendly name
        sender_name = sender_session.friendly_name or sender_session.name or sender_session_id

        # Format delivery mode with icon
        mode_icons = {
            "sequential": "📨",
            "important": "❗",
            "urgent": "⚡",
        }
        icon = mode_icons.get(delivery_mode, "📨")

        # Format notification message
        notification_text = f"{icon} **From [{sender_name}]** ({delivery_mode}): {text}"

        # Send notification via notifier
        from .models import NotificationEvent
        event = NotificationEvent(
            session_id=recipient_session_id,
            event_type="sm_send",
            message=notification_text,
            context="",
            urgent=False,
        )

        await notifier.notify(event, recipient_session)

    def set_hook_output_store(self, store: dict):
        """Attach hook output store (used to cache last responses)."""
        self.hook_output_store = store

    async def _ensure_codex_session(self, session: Session, model: Optional[str] = None) -> Optional[CodexAppServerSession]:
        """Ensure a Codex app-server session is running for this session."""
        existing = self.codex_sessions.get(session.id)
        if existing:
            return existing

        try:
            codex_session = CodexAppServerSession(
                session_id=session.id,
                working_dir=session.working_dir,
                config=self.codex_config,
                on_turn_complete=self._handle_codex_turn_complete,
                on_turn_started=self._handle_codex_turn_started,
                on_turn_delta=self._handle_codex_turn_delta,
            )
            thread_id = await codex_session.start(thread_id=session.codex_thread_id, model=model)
            session.codex_thread_id = thread_id
            self.codex_sessions[session.id] = codex_session
            self._save_state()
            return codex_session
        except Exception as e:
            logger.error(f"Failed to ensure Codex session for {session.id}: {e}")
            return None

    async def _deliver_direct(self, session: Session, text: str, model: Optional[str] = None) -> bool:
        """Deliver a message directly to a session (no queue)."""
        if session.provider == "codex-app":
            codex_session = await self._ensure_codex_session(session, model=model)
            if not codex_session:
                return False
            try:
                await codex_session.send_user_turn(text, model=model)
                session.status = SessionStatus.RUNNING
                session.last_activity = datetime.now()
                self._save_state()
                if self.message_queue_manager:
                    self.message_queue_manager.mark_session_active(session.id)
                return True
            except Exception as e:
                logger.error(f"Codex app send failed for {session.id}: {e}")
                return False

        success = await self.tmux.send_input_async(session.tmux_session, text)
        if success and self.message_queue_manager:
            self.message_queue_manager.mark_session_active(session.id)
        return success

    async def _interrupt_codex(self, session: Session) -> bool:
        """Interrupt a Codex turn if one is in progress."""
        codex_session = await self._ensure_codex_session(session)
        if not codex_session:
            return False
        return await codex_session.interrupt_turn()

    async def _deliver_urgent(self, session: Session, text: str) -> bool:
        """Deliver an urgent message (interrupt if possible)."""
        if session.provider == "codex-app":
            codex_session = await self._ensure_codex_session(session)
            if not codex_session:
                return False
            await codex_session.interrupt_turn()
            return await self._deliver_direct(session, text)

        # Claude (tmux) urgent delivery handled in message queue directly
        return await self._deliver_direct(session, text)

    async def _handle_codex_turn_complete(self, session_id: str, text: str, status: str):
        """Handle Codex app-server turn completion."""
        session = self.sessions.get(session_id)
        if not session:
            return

        self.codex_turns_in_flight.discard(session_id)

        # Store last output (for /status, /last-message)
        if text and self.hook_output_store is not None:
            self.hook_output_store["latest"] = text
            self.hook_output_store[session_id] = text

        # Update session status and activity
        session.last_activity = datetime.now()
        session.status = SessionStatus.IDLE  # Session stopped, waiting for input
        self._save_state()

        # Mark idle for message queue delivery
        if self.message_queue_manager:
            self.message_queue_manager.mark_session_idle(session_id)

        # Send notification similar to Claude Stop hook
        if text and hasattr(self, "notifier") and self.notifier:
            if session.telegram_chat_id:
                event = NotificationEvent(
                    session_id=session.id,
                    event_type="response",
                    message="Codex responded",
                    context=text,
                    urgent=False,
                )
                await self.notifier.notify(event, session)

    async def _handle_codex_turn_started(self, session_id: str, turn_id: str):
        """Mark Codex turn as active and update activity timestamps."""
        self.codex_turns_in_flight.add(session_id)
        session = self.sessions.get(session_id)
        if not session:
            return
        session.status = SessionStatus.RUNNING
        session.last_activity = datetime.now()
        # Save on turn start (lower frequency)
        self._save_state()
        if self.message_queue_manager:
            self.message_queue_manager.mark_session_active(session_id)

    async def _handle_codex_turn_delta(self, session_id: str, turn_id: str, delta: str):
        """Update activity on Codex streaming deltas."""
        session = self.sessions.get(session_id)
        if not session:
            return
        session.last_activity = datetime.now()
        if self.message_queue_manager:
            self.message_queue_manager.mark_session_active(session_id)

    def is_codex_turn_active(self, session_id: str) -> bool:
        """Check if a Codex turn is currently in flight."""
        return session_id in self.codex_turns_in_flight

    async def clear_session(self, session_id: str, new_prompt: Optional[str] = None) -> bool:
        """Clear/reset a session's context (Claude: /clear, Codex: /new, Codex app: new thread)."""
        session = self.sessions.get(session_id)
        if not session:
            return False

        if session.provider == "codex-app":
            codex_session = await self._ensure_codex_session(session)
            if not codex_session:
                return False
            try:
                await codex_session.start_new_thread()
                session.codex_thread_id = codex_session.thread_id
                session.status = SessionStatus.IDLE
                session.last_activity = datetime.now()
                self._save_state()
                if new_prompt:
                    await codex_session.send_user_turn(new_prompt)
                    session.status = SessionStatus.RUNNING
                    session.last_activity = datetime.now()
                    self._save_state()
                    if self.message_queue_manager:
                        self.message_queue_manager.mark_session_active(session_id)
                elif self.message_queue_manager:
                    self.message_queue_manager.mark_session_idle(session_id)
                return True
            except Exception as e:
                logger.error(f"Failed to clear Codex app session {session_id}: {e}")
                return False

        if session.provider == "codex":
            return await self._clear_tmux_session(session, new_prompt, clear_command="/new")

        return await self._clear_tmux_session(session, new_prompt, clear_command="/clear")

    async def _clear_tmux_session(
        self,
        session: Session,
        new_prompt: Optional[str],
        clear_command: str,
    ) -> bool:
        """Send a clear command to a tmux session (async)."""
        tmux_session = session.tmux_session
        if not tmux_session:
            return False

        from src.models import CompletionStatus
        try:
            # If session is completed, wake it up first
            if session.completion_status == CompletionStatus.COMPLETED:
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "send-keys", "-t", tmux_session, "Enter",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await proc.communicate()
                await asyncio.sleep(1.5)

            # Interrupt any ongoing stream
            proc = await asyncio.create_subprocess_exec(
                "tmux", "send-keys", "-t", tmux_session, "Escape",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await proc.communicate()
            await asyncio.sleep(0.5)

            # Send clear command
            proc = await asyncio.create_subprocess_exec(
                "tmux", "send-keys", "-t", tmux_session, clear_command,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await proc.communicate()
            await asyncio.sleep(1.0)

            proc = await asyncio.create_subprocess_exec(
                "tmux", "send-keys", "-t", tmux_session, "Enter",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await proc.communicate()
            await asyncio.sleep(2.0)

            # Send new prompt if provided
            if new_prompt:
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "send-keys", "-t", tmux_session, new_prompt,
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await proc.communicate()
                await asyncio.sleep(1.0)
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "send-keys", "-t", tmux_session, "Enter",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await proc.communicate()
            return True
        except Exception as e:
            logger.error(f"Failed to clear tmux session {session.id}: {e}")
            return False

    def send_key(self, session_id: str, key: str) -> bool:
        """Send a key to a session (e.g., 'y', 'n')."""
        session = self.sessions.get(session_id)
        if not session:
            logger.error(f"Session not found: {session_id}")
            return False

        if session.provider == "codex-app":
            # Only support interrupt for Codex app sessions
            if key == "Escape":
                try:
                    loop = asyncio.get_running_loop()
                    loop.create_task(self._interrupt_codex(session))
                    return True
                except RuntimeError:
                    try:
                        asyncio.run(self._interrupt_codex(session))
                        return True
                    except Exception:
                        return False
                except Exception:
                    return False
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

        if session.provider == "codex-app":
            codex_session = self.codex_sessions.pop(session_id, None)
            if codex_session:
                try:
                    loop = asyncio.get_running_loop()
                    loop.create_task(codex_session.close())
                except RuntimeError:
                    asyncio.run(codex_session.close())
        else:
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

        if session.provider == "codex-app":
            logger.warning("Terminal open not supported for Codex app sessions")
            return False

        return self.tmux.open_in_terminal(session.tmux_session)

    def capture_output(self, session_id: str, lines: int = 50) -> Optional[str]:
        """Capture recent output from a session."""
        session = self.sessions.get(session_id)
        if not session:
            return None

        if session.provider == "codex-app":
            if self.hook_output_store:
                return self.hook_output_store.get(session_id)
            return None

        return self.tmux.capture_pane(session.tmux_session, lines)

    # cleanup_dead_sessions() removed - OutputMonitor now handles detection and cleanup automatically
