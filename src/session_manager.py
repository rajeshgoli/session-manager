"""Session registry and lifecycle management."""

import asyncio
import json
import logging
import re
import subprocess
import uuid
from datetime import datetime
from pathlib import Path
from typing import Optional, Callable, Awaitable, Any

from .models import ActivityState, DeliveryResult, NotificationEvent, ReviewConfig, Session, SessionStatus
from .tmux_controller import TmuxController
from .codex_app_server import CodexAppServerSession, CodexAppServerConfig, CodexAppServerError
from .codex_activity_projection import CodexActivityProjection
from .codex_event_store import CodexEventStore
from .codex_observability_logger import CodexObservabilityLogger
from .codex_request_ledger import CodexRequestLedger
from .github_reviews import post_pr_review_comment, poll_for_codex_review, get_pr_repo_from_git

logger = logging.getLogger(__name__)

ROLE_KEYWORDS = (
    "engineer",
    "architect",
    "scout",
    "reviewer",
    "product",
    "director",
    "ux",
)


def _coerce_rollout_flag(value: Any, default: bool = True) -> bool:
    """Parse rollout config values robustly (supports bools and common string forms)."""
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    if isinstance(value, (int, float)):
        return value != 0
    if isinstance(value, str):
        normalized = value.strip().lower()
        if normalized in {"true", "1", "yes", "on"}:
            return True
        if normalized in {"false", "0", "no", "off"}:
            return False
    return default


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
        self.process_generation = uuid.uuid4().hex[:12]

        self.tmux = TmuxController(log_dir=log_dir)
        self.sessions: dict[str, Session] = {}
        self._event_handlers: list[Callable[[NotificationEvent], Awaitable[None]]] = []
        self.codex_sessions: dict[str, CodexAppServerSession] = {}
        self.codex_turns_in_flight: set[str] = set()
        self.codex_active_turn_ids: dict[str, str] = {}
        self.codex_last_delta_at: dict[str, datetime] = {}
        self.codex_wait_states: dict[str, tuple[str, datetime]] = {}
        self._codex_item_started_at: dict[tuple[str, str], datetime] = {}
        self.codex_working_delta_window_seconds = float(
            self.config.get("codex_events", {}).get("working_delta_window_seconds", 2.5)
        )
        self.hook_output_store: Optional[dict] = None
        self.output_monitor = None  # Set by main app for activity projection (#288)

        # Telegram topic auto-sync
        self.orphaned_topics: list[tuple[int, int]] = []  # (chat_id, thread_id) from dead sessions
        self.default_forum_chat_id: Optional[int] = self.config.get("telegram", {}).get("default_forum_chat_id")
        self._topic_creator: Optional[Callable[..., Awaitable[Optional[int]]]] = None

        codex_config = self.config.get("codex", {})
        codex_app_config = self.config.get("codex_app_server", codex_config)
        codex_rollout = self.config.get("codex_rollout", {})
        self.codex_rollout_flags = {
            "enable_durable_events": _coerce_rollout_flag(
                codex_rollout.get("enable_durable_events"), default=True
            ),
            "enable_structured_requests": _coerce_rollout_flag(
                codex_rollout.get("enable_structured_requests"), default=True
            ),
            "enable_observability_projection": _coerce_rollout_flag(
                codex_rollout.get("enable_observability_projection"), default=True
            ),
            "enable_codex_tui": _coerce_rollout_flag(
                codex_rollout.get("enable_codex_tui"), default=True
            ),
        }

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
            client_name=codex_app_config.get("client_name", "session-manager"),
            client_title=codex_app_config.get("client_title", "Claude Session Manager"),
            client_version=codex_app_config.get("client_version", "0.1.0"),
        )

        codex_events_config = self.config.get("codex_events", {})
        default_events_db = str(self.state_file.with_name("codex_events.db"))
        self.codex_event_store = CodexEventStore(
            db_path=codex_events_config.get("db_path", default_events_db),
            ring_size=codex_events_config.get("ring_size", 1000),
            retention_max_events_per_session=codex_events_config.get("retention_max_events_per_session", 5000),
            retention_max_age_days=codex_events_config.get("retention_max_age_days", 14),
            prune_every_writes=codex_events_config.get("prune_every_writes", 200),
            payload_preview_chars=codex_events_config.get("payload_preview_chars", 1500),
        )

        default_requests_db = str(self.state_file.with_name("codex_requests.db"))
        self.codex_request_ledger = CodexRequestLedger(
            db_path=self.config.get("codex_requests", {}).get("db_path", default_requests_db),
            process_generation=self.process_generation,
        )
        codex_observability_config = self.config.get("codex_observability", {})
        default_observability_db = str(self.state_file.with_name("codex_observability.db"))
        self.codex_observability_logger = CodexObservabilityLogger(
            db_path=codex_observability_config.get("db_path", default_observability_db),
            retention_max_age_days=codex_observability_config.get("retention_max_age_days", 14),
            retention_tool_events_per_session=codex_observability_config.get(
                "retention_tool_events_per_session", 20000
            ),
            retention_turn_events_per_session=codex_observability_config.get(
                "retention_turn_events_per_session", 5000
            ),
            payload_max_chars=codex_observability_config.get("payload_max_chars", 4000),
            prune_interval_seconds=codex_observability_config.get("prune_interval_seconds", 3600),
        )
        self.codex_activity_projection = CodexActivityProjection(self.codex_observability_logger)

        # Message queue manager (set by main app)
        self.message_queue_manager = None

        # Child monitor (set by main app)
        self.child_monitor = None

        # EM topic continuity (Fix B: sm#271): persisted across handoffs
        # Format: {"chat_id": int, "thread_id": int} or None
        self.em_topic: Optional[dict] = None

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
                        # Collect orphaned Telegram forum topics for cleanup at startup.
                        # Only collect if chat_id matches the known forum group —
                        # in non-forum chats, telegram_thread_id is a reply message_id,
                        # not a forum topic, so delete_forum_topic would fail.
                        if (
                            session.telegram_chat_id
                            and session.telegram_thread_id
                            and self.default_forum_chat_id
                            and session.telegram_chat_id == self.default_forum_chat_id
                        ):
                            self.orphaned_topics.append(
                                (session.telegram_chat_id, session.telegram_thread_id)
                            )
                            logger.info(
                                f"Collected orphaned topic: chat={session.telegram_chat_id}, "
                                f"thread={session.telegram_thread_id} from dead session {session.name}"
                            )
                if legacy_codex_sessions:
                    self._rewrite_state_raw(cleaned_sessions)

                # Load EM topic continuity field (backward compat: missing = None)
                self.em_topic = data.get("em_topic")

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
                "sessions": [s.to_dict() for s in self.sessions.values()],
                "em_topic": self.em_topic,
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
                    on_review_complete=self._handle_codex_review_complete,
                    on_server_request=self._handle_codex_server_request,
                    on_item_notification=self._handle_codex_item_notification,
                    on_stream_error=self._handle_codex_stream_error,
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

        # Auto-create Telegram topic for this session
        await self._ensure_telegram_topic(session, telegram_chat_id)

        if provider == "codex-app" and not initial_prompt and self.message_queue_manager:
            self.message_queue_manager.mark_session_idle(session.id)

        # Log creation
        if parent_session_id:
            logger.info(f"Spawned child session {session.name} (id={session.id}, parent={parent_session_id})")
        else:
            logger.info(f"Created session {session.name} (id={session.id})")

        return session

    def set_topic_creator(self, creator: Callable[..., Awaitable[Optional[int]]]):
        """Set the callback used to create Telegram forum topics.

        Signature: async (session_id, chat_id, topic_name) -> Optional[int]
        Returns the topic/thread ID on success, None on failure.
        """
        self._topic_creator = creator

    async def _ensure_telegram_topic(self, session: "Session", explicit_chat_id: Optional[int] = None):
        """Ensure a session has a Telegram forum topic, creating one if needed.

        Args:
            session: The session to ensure a topic for
            explicit_chat_id: Chat ID passed by the caller (e.g. from Telegram /new)
        """
        changed = False

        # 1. Ensure chat_id is set (explicit > existing > default)
        if not session.telegram_chat_id:
            chat_id = explicit_chat_id or self.default_forum_chat_id
            if chat_id:
                session.telegram_chat_id = chat_id
                changed = True

        # 2. Create topic if chat_id is set but thread_id is missing
        if session.telegram_chat_id and not session.telegram_thread_id and self._topic_creator:
            topic_name = f"{session.friendly_name or 'session'} [{session.id}]"
            try:
                thread_id = await self._topic_creator(
                    session.id, session.telegram_chat_id, topic_name
                )
                if thread_id:
                    session.telegram_thread_id = thread_id
                    self._save_state()  # Persist IMMEDIATELY — minimize race window
                    changed = False     # Already saved; prevent redundant outer save
                    logger.info(
                        f"Auto-created topic for session {session.id}: "
                        f"chat={session.telegram_chat_id}, thread={thread_id}"
                    )
            except Exception as e:
                logger.warning(f"Failed to auto-create topic for session {session.id}: {e}")

        if changed:
            self._save_state()

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

    def set_role(self, session_id: str, role: str) -> bool:
        """Set the role tag for a session."""
        session = self.sessions.get(session_id)
        if not session:
            return False
        session.role = role
        self._save_state()
        return True

    def clear_role(self, session_id: str) -> bool:
        """Clear the role tag for a session."""
        session = self.sessions.get(session_id)
        if not session:
            return False
        session.role = None
        self._save_state()
        return True

    def list_sessions(self, include_stopped: bool = False) -> list[Session]:
        """List all sessions."""
        sessions = list(self.sessions.values())
        if not include_stopped:
            sessions = [s for s in sessions if s.status != SessionStatus.STOPPED]
        return sessions

    @staticmethod
    def detect_role_from_prompt(text: str) -> Optional[str]:
        """Best-effort role detection from initial prompt text."""
        if not text:
            return None
        snippet = text[:200].lower()
        for keyword in ROLE_KEYWORDS:
            if re.search(rf"\bas\s+{re.escape(keyword)}\b", snippet):
                return keyword
        return None

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
        remind_soft_threshold: Optional[int] = None,
        remind_hard_threshold: Optional[int] = None,
        parent_session_id: Optional[str] = None,
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
            remind_soft_threshold: Seconds after delivery before soft remind fires (#188)
            remind_hard_threshold: Seconds after delivery before hard remind fires (#188)

        Returns:
            DeliveryResult indicating whether message was DELIVERED, QUEUED, or FAILED
        """
        session = self.sessions.get(session_id)
        if not session:
            logger.error(f"Session not found: {session_id}")
            return DeliveryResult.FAILED

        if session.role is None:
            detected_role = self.detect_role_from_prompt(text)
            if detected_role:
                session.role = detected_role
                self._save_state()

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

        # Directional notify-on-stop (#256): only EM→agent sends should enroll recipient.
        # Fail-closed: unknown sender treated as non-EM.
        if notify_on_stop and sender_session_id:
            if not sender_session or not sender_session.is_em:
                notify_on_stop = False

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

        # Handle steer delivery mode — direct Enter-based injection, bypasses queue
        if delivery_mode == "steer":
            if session.provider != "codex":
                logger.error(f"Steer delivery only supported for Codex CLI sessions, not {session.provider}")
                return DeliveryResult.FAILED
            success = await self.tmux.send_steer_text(session.tmux_session, text)
            return DeliveryResult.DELIVERED if success else DeliveryResult.FAILED

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
                    remind_soft_threshold=remind_soft_threshold,
                    remind_hard_threshold=remind_hard_threshold,
                    parent_session_id=parent_session_id,
                )
                # Record outgoing sm send for deferred stop notification suppression (#182)
                # Placed after queue_message to ensure message was persisted first.
                if from_sm_send and sender_session_id:
                    sender_state = self.message_queue_manager._get_or_create_state(sender_session_id)
                    sender_state.last_outgoing_sm_send_target = session_id
                    sender_state.last_outgoing_sm_send_at = datetime.now()
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
                    remind_soft_threshold=remind_soft_threshold,
                    remind_hard_threshold=remind_hard_threshold,
                    parent_session_id=parent_session_id,
                )
                # Record outgoing sm send for deferred stop notification suppression (#182)
                if from_sm_send and sender_session_id:
                    sender_state = self.message_queue_manager._get_or_create_state(sender_session_id)
                    sender_state.last_outgoing_sm_send_target = session_id
                    sender_state.last_outgoing_sm_send_at = datetime.now()
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

    async def start_background_tasks(self):
        """Start periodic maintenance tasks owned by SessionManager."""
        await self.codex_observability_logger.start_periodic_prune()

    async def stop_background_tasks(self):
        """Stop periodic maintenance tasks owned by SessionManager."""
        await self.codex_observability_logger.stop_periodic_prune()

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
                on_review_complete=self._handle_codex_review_complete,
                on_server_request=self._handle_codex_server_request,
                on_item_notification=self._handle_codex_item_notification,
                on_stream_error=self._handle_codex_stream_error,
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
        turn_id = self.codex_active_turn_ids.pop(session_id, None)
        self.codex_wait_states.pop(session_id, None)
        self.codex_last_delta_at.pop(session_id, None)

        self.codex_event_store.append_event(
            session_id=session_id,
            event_type="turn_completed",
            turn_id=turn_id,
            payload={
                "status": status,
                "output_preview": text[:400] if text else "",
                "output_chars": len(text or ""),
            },
        )
        self._safe_log_codex_turn_event(
            session_id=session_id,
            thread_id=self._thread_id_for_session(session_id),
            turn_id=turn_id,
            event_type="turn_completed",
            status=status,
            output_preview=text[:400] if text else "",
            raw_payload={"status": status, "output_chars": len(text or "")},
        )

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

                # If session has a review_config, emit review_complete
                if session.review_config:
                    try:
                        review_config = session.review_config
                        review_result = None
                        if review_config.mode == "pr" and review_config.pr_repo and review_config.pr_number:
                            from .github_reviews import fetch_latest_codex_review
                            from .review_parser import parse_github_review
                            codex_review = fetch_latest_codex_review(
                                review_config.pr_repo, review_config.pr_number
                            )
                            if codex_review:
                                review_result = parse_github_review(
                                    review_config.pr_repo,
                                    review_config.pr_number,
                                    codex_review,
                                )
                        else:
                            from .review_parser import parse_tui_output
                            review_result = parse_tui_output(text)
                        if review_result and review_result.findings:
                            review_event = NotificationEvent(
                                session_id=session.id,
                                event_type="review_complete",
                                message="Review complete",
                                context="",
                                urgent=False,
                            )
                            review_event.review_result = review_result
                            await self.notifier.notify(review_event, session)
                    except Exception as e:
                        logger.warning(f"Failed to emit review_complete: {e}")

    async def _handle_codex_turn_started(self, session_id: str, turn_id: str):
        """Mark Codex turn as active and update activity timestamps."""
        self.codex_turns_in_flight.add(session_id)
        self.codex_active_turn_ids[session_id] = turn_id
        self.codex_wait_states.pop(session_id, None)
        self.codex_last_delta_at.pop(session_id, None)
        self.codex_event_store.append_event(
            session_id=session_id,
            event_type="turn_started",
            turn_id=turn_id,
            payload={},
        )
        self._safe_log_codex_turn_event(
            session_id=session_id,
            thread_id=self._thread_id_for_session(session_id),
            turn_id=turn_id,
            event_type="turn_started",
            status="running",
            raw_payload={},
        )
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
        self.codex_last_delta_at[session_id] = datetime.now()
        self.codex_wait_states.pop(session_id, None)
        self.codex_event_store.append_event(
            session_id=session_id,
            event_type="turn_delta",
            turn_id=turn_id,
            payload={
                "delta_preview": delta[:240],
                "delta_chars": len(delta),
            },
        )
        self._safe_log_codex_turn_event(
            session_id=session_id,
            thread_id=self._thread_id_for_session(session_id),
            turn_id=turn_id,
            event_type="turn_delta",
            status="running",
            delta_chars=len(delta),
            output_preview=delta[:240],
            raw_payload={"delta_chars": len(delta)},
        )
        session = self.sessions.get(session_id)
        if not session:
            return
        session.last_activity = datetime.now()
        if self.message_queue_manager:
            self.message_queue_manager.mark_session_active(session_id)

    async def _handle_codex_review_complete(self, session_id: str, review_text: str):
        """Handle Codex app-server review completion (exitedReviewMode)."""
        session = self.sessions.get(session_id)
        if not session:
            return

        session.last_activity = datetime.now()
        session.status = SessionStatus.IDLE
        self._save_state()

        if self.message_queue_manager:
            self.message_queue_manager.mark_session_idle(session_id)

        # Store review output
        if review_text and self.hook_output_store is not None:
            self.hook_output_store["latest"] = review_text
            self.hook_output_store[session_id] = review_text

        self.codex_event_store.append_event(
            session_id=session_id,
            event_type="review_completed",
            turn_id=None,
            payload={
                "output_preview": review_text[:400] if review_text else "",
                "output_chars": len(review_text or ""),
            },
        )

        # Emit review_complete notification
        if review_text and session.review_config and hasattr(self, "notifier") and self.notifier:
            try:
                from .review_parser import parse_app_server_output
                review_result = parse_app_server_output(review_text)
                if review_result and review_result.findings:
                    review_event = NotificationEvent(
                        session_id=session.id,
                        event_type="review_complete",
                        message="Review complete",
                        context="",
                        urgent=False,
                    )
                    review_event.review_result = review_result
                    await self.notifier.notify(review_event, session)
            except Exception as e:
                logger.warning(f"Failed to emit review_complete for codex-app: {e}")

    def is_codex_turn_active(self, session_id: str) -> bool:
        """Check if a Codex turn is currently in flight."""
        return session_id in self.codex_turns_in_flight

    async def _handle_codex_server_request(
        self,
        session_id: str,
        request_id: int,
        method: str,
        params: dict,
    ) -> Optional[dict]:
        """Track codex-app server requests as lifecycle events for observability/activity state."""
        now = datetime.now()
        state_name = None
        event_type = "server_request"
        request_type = "server_request"
        policy_payload: Optional[dict] = None
        if method in ("item/commandExecution/requestApproval", "item/fileChange/requestApproval"):
            state_name = "waiting_permission"
            event_type = "request_approval"
            request_type = "request_approval"
            policy_payload = {"decision": "decline"}
        elif method == "item/tool/requestUserInput":
            state_name = "waiting_input"
            event_type = "request_user_input"
            request_type = "request_user_input"
            policy_payload = {"answers": {}}

        if state_name:
            self.codex_wait_states[session_id] = (state_name, now)

        codex_session = self.codex_sessions.get(session_id)
        thread_id = codex_session.thread_id if codex_session else None
        item = params.get("item", {}) if isinstance(params.get("item"), dict) else {}
        turn_id = params.get("turnId")
        item_id = item.get("id")

        if policy_payload is not None:
            pending = await self.codex_request_ledger.register_request(
                session_id=session_id,
                rpc_request_id=request_id,
                request_method=method,
                request_payload=params,
                thread_id=thread_id,
                turn_id=turn_id,
                item_id=item_id,
                request_type=request_type,
                timeout_seconds=self.codex_config.request_timeout_seconds,
                policy_payload=policy_payload,
            )
            request_ledger_id = pending["request_id"]
            item_type = "commandExecution" if "commandExecution" in method else "fileChange"
            if method == "item/tool/requestUserInput":
                item_type = "tool"
            self._safe_log_codex_tool_event(
                session_id=session_id,
                thread_id=thread_id,
                turn_id=turn_id,
                item_id=item_id,
                request_id=request_ledger_id,
                event_type=event_type,
                item_type=item_type,
                phase="pre",
                raw_payload=params,
            )
        else:
            request_ledger_id = None

        self.codex_event_store.append_event(
            session_id=session_id,
            event_type=event_type,
            turn_id=params.get("turnId"),
            payload={
                "request_id": request_id,
                "ledger_request_id": request_ledger_id,
                "method": method,
            },
        )

        if request_ledger_id:
            resolved = await self.codex_request_ledger.wait_for_resolution(request_ledger_id)
            self.codex_wait_states.pop(session_id, None)
            return resolved

        return None

    async def _handle_codex_item_notification(self, session_id: str, method: str, params: dict[str, Any]):
        """Ingest codex item lifecycle notifications into observability storage."""
        item = params.get("item", {}) if isinstance(params.get("item"), dict) else {}
        item_id = item.get("id")
        turn_id = params.get("turnId")
        thread_id = self._thread_id_for_session(session_id)
        now = datetime.now()

        if method == "item/started":
            item_type = item.get("type")
            if item_type in ("commandExecution", "fileChange", "tool"):
                if item_id:
                    self._codex_item_started_at[(session_id, item_id)] = now
                self._safe_log_codex_tool_event(
                    session_id=session_id,
                    thread_id=thread_id,
                    turn_id=turn_id,
                    item_id=item_id,
                    event_type="started",
                    item_type=item_type,
                    phase="running",
                    command=item.get("command"),
                    cwd=item.get("cwd"),
                    file_path=item.get("filePath") or item.get("path"),
                    diff_summary=item.get("diffSummary") or item.get("summary"),
                    raw_payload=params,
                )
            return

        if method in ("item/commandExecution/outputDelta", "item/fileChange/outputDelta"):
            item_type = "commandExecution" if "commandExecution" in method else "fileChange"
            delta = params.get("delta")
            delta_summary = str(delta)[:240] if delta is not None else None
            self._safe_log_codex_tool_event(
                session_id=session_id,
                thread_id=thread_id,
                turn_id=turn_id,
                item_id=item_id,
                event_type="output_delta",
                item_type=item_type,
                phase="running",
                diff_summary=delta_summary,
                raw_payload=params,
            )
            return

        if method == "item/completed":
            item_type = item.get("type")
            if item_type not in ("commandExecution", "fileChange", "tool"):
                return
            status = str(item.get("status", "completed")).lower()
            event_type_map = {
                "completed": "completed",
                "failed": "failed",
                "interrupted": "interrupted",
                "cancelled": "cancelled",
                "timeout": "timeout",
            }
            event_type = event_type_map.get(status)
            if event_type is None:
                if "interrupt" in status:
                    event_type = "interrupted"
                elif "cancel" in status:
                    event_type = "cancelled"
                elif "fail" in status:
                    event_type = "failed"
                elif "timeout" in status:
                    event_type = "timeout"
                else:
                    event_type = "completed"
            started_at = self._codex_item_started_at.pop((session_id, item_id), None) if item_id else None
            latency_ms = None
            if started_at is not None:
                latency_ms = int((now - started_at).total_seconds() * 1000)
            self._safe_log_codex_tool_event(
                session_id=session_id,
                thread_id=thread_id,
                turn_id=turn_id,
                item_id=item_id,
                event_type=event_type,
                item_type=item_type,
                phase="post",
                command=item.get("command"),
                cwd=item.get("cwd"),
                exit_code=item.get("exitCode"),
                file_path=item.get("filePath") or item.get("path"),
                diff_summary=item.get("diffSummary") or item.get("summary"),
                latency_ms=latency_ms,
                final_status=status,
                error_code=item.get("errorCode"),
                error_message=item.get("errorMessage"),
                raw_payload=params,
            )

    async def _handle_codex_stream_error(self, session_id: str, error_code: str, error_message: str):
        """Emit synthetic terminal observability events when app-server stream closes unexpectedly."""
        turn_id = self.codex_active_turn_ids.get(session_id)
        thread_id = self._thread_id_for_session(session_id)
        self._safe_log_codex_tool_event(
            session_id=session_id,
            thread_id=thread_id,
            turn_id=turn_id,
            event_type="failed",
            item_type="tool",
            phase="post",
            final_status="failed",
            error_code=error_code,
            error_message=error_message,
            raw_payload={"error_code": error_code, "error_message": error_message},
        )
        self._safe_log_codex_turn_event(
            session_id=session_id,
            thread_id=thread_id,
            turn_id=turn_id,
            event_type="turn_stream_error",
            status="failed",
            error_code=error_code,
            error_message=error_message,
            raw_payload={"error_code": error_code, "error_message": error_message},
        )

    def _thread_id_for_session(self, session_id: str) -> Optional[str]:
        codex_session = self.codex_sessions.get(session_id)
        if codex_session and codex_session.thread_id:
            return codex_session.thread_id
        session = self.sessions.get(session_id)
        return session.codex_thread_id if session else None

    def _safe_log_codex_tool_event(self, **kwargs: Any):
        try:
            self.codex_observability_logger.log_tool_event(**kwargs)
        except Exception as exc:
            logger.warning("Failed to log codex tool event for %s: %s", kwargs.get("session_id"), exc)

    def _safe_log_codex_turn_event(self, **kwargs: Any):
        try:
            self.codex_observability_logger.log_turn_event(**kwargs)
        except Exception as exc:
            logger.warning("Failed to log codex turn event for %s: %s", kwargs.get("session_id"), exc)

    def is_codex_rollout_enabled(self, flag_name: str) -> bool:
        """Read codex rollout feature gate (defaults to True for unknown flags)."""
        return bool(self.codex_rollout_flags.get(flag_name, True))

    def get_activity_state(self, session_or_id: Session | str) -> str:
        """Get computed activity state for API consumers."""
        session: Optional[Session]
        if isinstance(session_or_id, Session):
            session = session_or_id
        else:
            session = self.sessions.get(session_or_id)
            if not session:
                return ActivityState.STOPPED.value

        if session.status == SessionStatus.STOPPED:
            return ActivityState.STOPPED.value

        if session.provider == "codex-app":
            return self._compute_codex_app_activity(session)

        queue_mgr = self.message_queue_manager
        delivery_state = queue_mgr.delivery_states.get(session.id) if queue_mgr else None
        is_idle = delivery_state.is_idle if delivery_state is not None else None

        monitor_state = None
        if self.output_monitor:
            getter = getattr(self.output_monitor, "get_session_state", None)
            if callable(getter):
                monitor_state = getter(session.id)

        if monitor_state and monitor_state.last_pattern == "permission":
            return ActivityState.WAITING_PERMISSION.value

        if session.completion_status is not None:
            return ActivityState.WAITING_INPUT.value

        if is_idle is True:
            return ActivityState.IDLE.value

        if is_idle is False:
            if monitor_state and monitor_state.is_output_flowing:
                return ActivityState.WORKING.value
            return ActivityState.THINKING.value

        idle_seconds = (datetime.now() - session.last_activity).total_seconds()
        if idle_seconds < 30:
            return ActivityState.THINKING.value
        return ActivityState.IDLE.value

    def _compute_codex_app_activity(self, session: Session) -> str:
        """Compute activity state for codex-app sessions (no tmux/output monitor)."""
        if session.completion_status is not None:
            return ActivityState.WAITING_INPUT.value

        queue_mgr = self.message_queue_manager
        delivery_state = queue_mgr.delivery_states.get(session.id) if queue_mgr else None
        if delivery_state is not None:
            return ActivityState.IDLE.value if delivery_state.is_idle else ActivityState.WORKING.value

        idle_seconds = (datetime.now() - session.last_activity).total_seconds()
        if idle_seconds > 30:
            return ActivityState.IDLE.value
        return ActivityState.THINKING.value

    def get_codex_events(self, session_id: str, since_seq: Optional[int] = None, limit: int = 200) -> dict:
        """Read persisted codex event timeline for one session."""
        return self.codex_event_store.get_events(session_id=session_id, since_seq=since_seq, limit=limit)

    def get_codex_activity_actions(self, session_id: str, limit: int = 20) -> list[dict[str, Any]]:
        """Return provider-neutral projected codex-app actions for CLI surfaces."""
        return self.codex_activity_projection.recent_actions(session_id=session_id, limit=limit)

    def get_codex_latest_activity_action(self, session_id: str) -> Optional[dict[str, Any]]:
        """Return latest provider-neutral projected codex-app action summary."""
        return self.codex_activity_projection.latest_action(session_id=session_id)

    def list_codex_pending_requests(self, session_id: str, include_orphaned: bool = False) -> list[dict]:
        """List pending structured requests for a codex-app session."""
        return self.codex_request_ledger.list_requests(session_id=session_id, include_orphaned=include_orphaned)

    async def respond_codex_request(self, session_id: str, request_id: str, response_payload: dict) -> dict:
        """Resolve one structured request for a codex-app session."""
        request = self.codex_request_ledger.get_request(request_id)
        if not request or request.get("session_id") != session_id:
            return {
                "ok": False,
                "http_status": 404,
                "error_code": "request_not_found",
                "error_message": "request id not found for session",
            }
        result = await self.codex_request_ledger.resolve_request(
            request_id=request_id,
            response_payload=response_payload,
            resolution_source="api",
        )
        if result.get("ok"):
            event_type = "approval_decision" if "decision" in response_payload else "user_input_submitted"
            request_method = request.get("request_method", "")
            if "commandExecution" in request_method:
                item_type = "commandExecution"
            elif "fileChange" in request_method:
                item_type = "fileChange"
            else:
                item_type = "tool"
            self._safe_log_codex_tool_event(
                session_id=session_id,
                thread_id=request.get("thread_id"),
                turn_id=request.get("turn_id"),
                item_id=request.get("item_id"),
                request_id=request_id,
                event_type=event_type,
                item_type=item_type,
                phase="post",
                approval_decision=response_payload.get("decision"),
                raw_payload=response_payload,
            )
        return result

    def has_pending_codex_requests(self, session_id: str) -> bool:
        """Return True when unresolved structured codex requests block chat input."""
        return self.codex_request_ledger.has_pending_requests(session_id=session_id)

    def oldest_pending_codex_request(self, session_id: str) -> Optional[dict]:
        """Return oldest pending request summary for explicit input-gate error payloads."""
        return self.codex_request_ledger.oldest_pending_summary(session_id=session_id)

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
                self.codex_request_ledger.orphan_pending_for_session(session_id, error_code="thread_reset")
                for key in [k for k in self._codex_item_started_at if k[0] == session_id]:
                    self._codex_item_started_at.pop(key, None)
                self.codex_turns_in_flight.discard(session_id)
                self.codex_active_turn_ids.pop(session_id, None)
                self.codex_last_delta_at.pop(session_id, None)
                self.codex_wait_states.pop(session_id, None)
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
            self.codex_request_ledger.orphan_pending_for_session(session_id)
            for key in [k for k in self._codex_item_started_at if k[0] == session_id]:
                self._codex_item_started_at.pop(key, None)
            self.codex_turns_in_flight.discard(session_id)
            self.codex_active_turn_ids.pop(session_id, None)
            self.codex_last_delta_at.pop(session_id, None)
            self.codex_wait_states.pop(session_id, None)
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
                output = self.hook_output_store.get(session_id)
                if output is None:
                    return None
                if lines <= 0:
                    return ""
                chunks = output.splitlines()
                tail = chunks[-lines:] if chunks else []
                if not tail:
                    return ""
                # Preserve trailing newline semantics from tmux capture where possible.
                suffix = "\n" if output.endswith("\n") else ""
                return "\n".join(tail) + suffix
            return None

        return self.tmux.capture_pane(session.tmux_session, lines)

    # cleanup_dead_sessions() removed - OutputMonitor now handles detection and cleanup automatically

    async def start_review(
        self,
        session_id: str,
        mode: str,
        base_branch: Optional[str] = None,
        commit_sha: Optional[str] = None,
        custom_prompt: Optional[str] = None,
        steer_text: Optional[str] = None,
        wait: Optional[int] = None,
        watcher_session_id: Optional[str] = None,
    ) -> dict:
        """
        Start a Codex /review on an existing session.

        Args:
            session_id: Target session ID (must be a Codex CLI or codex-app session)
            mode: Review mode (branch, uncommitted, commit, custom)
            base_branch: Target branch for branch mode
            commit_sha: Target SHA for commit mode
            custom_prompt: Custom review text for custom mode
            steer_text: Instructions to inject after review starts
            wait: Seconds to watch for completion
            watcher_session_id: Session to notify when review completes

        Returns:
            Status dict with review info
        """
        session = self.sessions.get(session_id)
        if not session:
            return {"error": f"Session {session_id} not found"}

        if session.provider not in ("codex", "codex-app"):
            return {"error": "Review requires a Codex session (provider=codex or codex-app)"}

        # Validate session is idle before sending /review
        if self.message_queue_manager:
            state = self.message_queue_manager.delivery_states.get(session_id)
            if state and not state.is_idle:
                return {"error": "Session is busy. Wait for current work to complete or use sm clear first."}

        # Store ReviewConfig on session
        review_config = ReviewConfig(
            mode=mode,
            base_branch=base_branch,
            commit_sha=commit_sha,
            custom_prompt=custom_prompt,
            steer_text=steer_text,
            steer_delivered=False,
        )
        session.review_config = review_config

        # Reset idle baseline for ChildMonitor
        session.last_tool_call = datetime.now()
        self._save_state()

        # --- codex-app path: use review/start RPC ---
        if session.provider == "codex-app":
            codex_session = await self._ensure_codex_session(session)
            if not codex_session:
                if self.message_queue_manager:
                    self.message_queue_manager.mark_session_idle(session_id)
                return {"error": "Failed to connect to Codex app-server"}

            try:
                # Mark active just before dispatch (after all validation)
                if self.message_queue_manager:
                    self.message_queue_manager.mark_session_active(session_id)
                await codex_session.review_start(
                    mode=mode,
                    base_branch=base_branch,
                    commit_sha=commit_sha,
                    custom_prompt=custom_prompt,
                )
                session.status = SessionStatus.RUNNING
                session.last_activity = datetime.now()
                self._save_state()
            except CodexAppServerError as e:
                if self.message_queue_manager:
                    self.message_queue_manager.mark_session_idle(session_id)
                return {"error": f"review/start RPC failed: {e}"}

            # Register watch if requested
            if wait and watcher_session_id and self.message_queue_manager:
                await self.message_queue_manager.watch_session(
                    session_id, watcher_session_id, wait
                )

            return {
                "session_id": session_id,
                "review_mode": mode,
                "base_branch": base_branch,
                "commit_sha": commit_sha,
                "status": "started",
                "steer_queued": False,  # steer not applicable for app-server
            }

        # --- codex CLI path: tmux key sequence ---
        # Validate working dir is a git repo
        working_path = Path(session.working_dir).expanduser().resolve()
        try:
            result = subprocess.run(
                ["git", "rev-parse", "--git-dir"],
                cwd=working_path,
                capture_output=True,
                text=True,
                timeout=2,
            )
            if result.returncode != 0:
                return {"error": f"Working directory is not a git repo: {session.working_dir}"}
        except Exception as e:
            return {"error": f"Failed to check git repo: {e}"}

        # For branch mode, find branch position
        branch_position = None
        if mode == "branch" and base_branch:
            try:
                result = subprocess.run(
                    ["git", "branch", "--list"],
                    cwd=working_path,
                    capture_output=True,
                    text=True,
                    timeout=2,
                )
                if result.returncode != 0:
                    return {"error": "Failed to list git branches"}

                branches = []
                for line in result.stdout.strip().split("\n"):
                    # Strip leading whitespace and * marker for current branch
                    branch = line.strip().lstrip("* ").strip()
                    if branch:
                        branches.append(branch)

                if base_branch not in branches:
                    return {"error": f"Branch '{base_branch}' not found. Available: {', '.join(branches)}"}

                branch_position = branches.index(base_branch)
                logger.info(f"Branch '{base_branch}' at position {branch_position} in list: {branches}")
            except subprocess.TimeoutExpired:
                return {"error": "Timeout listing git branches"}

        # Get review timing config
        codex_config = self.config.get("codex", {})
        review_timing = codex_config.get("review", {})

        # Mark active just before dispatch (after all validation)
        if self.message_queue_manager:
            self.message_queue_manager.mark_session_active(session_id)

        # Send the review key sequence
        success = await self.tmux.send_review_sequence(
            session_name=session.tmux_session,
            mode=mode,
            base_branch=base_branch,
            commit_sha=commit_sha,
            custom_prompt=custom_prompt,
            branch_position=branch_position,
            config=review_timing,
        )

        if not success:
            # Roll back active state to avoid wedged session
            if self.message_queue_manager:
                self.message_queue_manager.mark_session_idle(session_id)
            return {"error": "Failed to send review sequence to tmux"}

        # Schedule steer injection if requested
        if steer_text:
            steer_delay = review_timing.get("steer_delay_seconds", 5.0)

            async def _inject_steer():
                await asyncio.sleep(steer_delay)
                steer_success = await self.tmux.send_steer_text(session.tmux_session, steer_text)
                if steer_success:
                    session.review_config.steer_delivered = True
                    self._save_state()
                    logger.info(f"Steer text injected for session {session_id}")
                else:
                    logger.error(f"Failed to inject steer text for session {session_id}")

            asyncio.create_task(_inject_steer())

        # Register watch if requested
        if wait and watcher_session_id and self.message_queue_manager:
            await self.message_queue_manager.watch_session(
                session_id, watcher_session_id, wait
            )

        return {
            "session_id": session_id,
            "review_mode": mode,
            "base_branch": base_branch,
            "commit_sha": commit_sha,
            "status": "started",
            "steer_queued": steer_text is not None,
        }

    async def spawn_review_session(
        self,
        parent_session_id: str,
        mode: str,
        base_branch: Optional[str] = None,
        commit_sha: Optional[str] = None,
        custom_prompt: Optional[str] = None,
        steer_text: Optional[str] = None,
        name: Optional[str] = None,
        wait: Optional[int] = None,
        model: Optional[str] = None,
        working_dir: Optional[str] = None,
    ) -> Optional[Session]:
        """
        Spawn a new Codex session and immediately start a review.

        Args:
            parent_session_id: Parent session ID
            mode: Review mode (branch, uncommitted, commit, custom)
            base_branch: Target branch for branch mode
            commit_sha: Target SHA for commit mode
            custom_prompt: Custom review text for custom mode
            steer_text: Instructions to inject after review starts
            name: Friendly name for the new session
            wait: Seconds to watch for completion
            model: Model override
            working_dir: Working directory override

        Returns:
            Created Session or None on failure
        """
        parent = self.sessions.get(parent_session_id)
        if not parent:
            logger.error(f"Parent session not found: {parent_session_id}")
            return None

        child_working_dir = working_dir or parent.working_dir

        # Spawn a Codex session with no initial prompt
        session = await self._create_session_common(
            working_dir=child_working_dir,
            name=f"child-{parent_session_id[:6]}" if not name else None,
            friendly_name=name,
            parent_session_id=parent_session_id,
            spawn_prompt=f"review:{mode}",
            model=model,
            initial_prompt=None,  # No prompt — we send /review instead
            provider="codex",
        )

        if not session:
            return None

        # Wait for Codex CLI to initialize
        tmux_timeouts = self.config.get("timeouts", {}).get("tmux", {})
        init_seconds = tmux_timeouts.get("claude_init_seconds", 3)
        await asyncio.sleep(init_seconds)

        # Start the review (wait/watcher handled by ChildMonitor below, not watch_session)
        result = await self.start_review(
            session_id=session.id,
            mode=mode,
            base_branch=base_branch,
            commit_sha=commit_sha,
            custom_prompt=custom_prompt,
            steer_text=steer_text,
            wait=None,
            watcher_session_id=None,
        )

        if result.get("error"):
            logger.error(f"Failed to start review on spawned session {session.id}: {result['error']}")
            # Clean up the leaked session to avoid orphans
            self.kill_session(session.id)
            return None

        # Register with ChildMonitor if wait specified
        if wait and self.child_monitor:
            self.child_monitor.register_child(
                child_session_id=session.id,
                parent_session_id=parent_session_id,
                wait_seconds=wait,
            )

        return session

    async def start_pr_review(
        self,
        pr_number: int,
        repo: Optional[str] = None,
        steer: Optional[str] = None,
        wait: Optional[int] = None,
        caller_session_id: Optional[str] = None,
    ) -> dict:
        """
        Trigger @codex review on a GitHub PR.

        No tmux session needed — posts a GitHub comment and optionally
        polls for the review to appear.

        Args:
            pr_number: GitHub PR number
            repo: GitHub repo (owner/repo). Inferred from working dir if None.
            steer: Focus instructions appended to the @codex review comment
            wait: Seconds to poll for Codex review completion
            caller_session_id: Session to store ReviewConfig on and notify

        Returns:
            Status dict with repo, pr_number, posted_at, comment_id, status
        """
        # 1. Resolve repo
        if not repo:
            # Try to infer from caller session's working dir, or cwd
            working_dir = None
            if caller_session_id:
                caller = self.sessions.get(caller_session_id)
                if caller:
                    working_dir = caller.working_dir
            if working_dir:
                repo = await asyncio.to_thread(get_pr_repo_from_git, working_dir)
            if not repo:
                return {"error": "Could not determine repo. Provide --repo or run from a git directory."}

        # 2. Validate PR exists
        try:
            result = await asyncio.to_thread(
                subprocess.run,
                ["gh", "pr", "view", str(pr_number), "--repo", repo, "--json", "state"],
                capture_output=True,
                text=True,
                timeout=10,
            )
            if result.returncode != 0:
                return {"error": f"PR #{pr_number} not found in {repo}: {result.stderr.strip()}"}
            pr_data = json.loads(result.stdout)
            if pr_data.get("state") != "OPEN":
                return {"error": f"PR #{pr_number} is {pr_data.get('state', 'unknown')}, not OPEN"}
        except Exception as e:
            return {"error": f"Failed to validate PR: {e}"}

        # 3. Store ReviewConfig on caller session (if provided)
        review_config = ReviewConfig(
            mode="pr",
            pr_number=pr_number,
            pr_repo=repo,
            steer_text=steer,
        )
        if caller_session_id:
            caller = self.sessions.get(caller_session_id)
            if caller:
                caller.review_config = review_config
                self._save_state()

        # 4. Post @codex review comment
        try:
            comment_result = await asyncio.to_thread(
                post_pr_review_comment, repo, pr_number, steer
            )
        except RuntimeError as e:
            return {"error": str(e)}

        # Store comment_id on ReviewConfig
        if caller_session_id:
            caller = self.sessions.get(caller_session_id)
            if caller and caller.review_config:
                caller.review_config.pr_comment_id = comment_result.get("comment_id")
                self._save_state()

        posted_at = comment_result["posted_at"]

        # 5. Start background poll if wait AND caller_session_id
        server_polling = False
        if wait and caller_session_id:
            server_polling = True

            async def _poll_and_notify():
                since = datetime.fromisoformat(posted_at)
                review = await asyncio.to_thread(
                    poll_for_codex_review, repo, pr_number, since, wait
                )
                if review:
                    msg = f"Review --pr {pr_number} ({repo}) completed: Codex posted review on PR #{pr_number}"
                else:
                    msg = f"Review --pr {pr_number} ({repo}) timed out after {wait}s"
                # Notify caller
                await self.send_input(
                    caller_session_id,
                    msg,
                    delivery_mode="important",
                )

            asyncio.create_task(_poll_and_notify())

        return {
            "repo": repo,
            "pr_number": pr_number,
            "posted_at": posted_at,
            "comment_id": comment_result.get("comment_id", 0),
            "comment_body": comment_result.get("body", ""),
            "status": "posted",
            "server_polling": server_polling,
        }

    async def recover_session(self, session: Session, graceful: bool = False) -> bool:
        """
        Recover a session from Claude Code harness crash.

        This handles JavaScript stack overflow crashes in the TUI harness.
        The agent (Anthropic backend) is unaffected - only the local harness crashed.

        Recovery flow (graceful=False, harness is dead):
        1. Pause message queue (prevent sm send going to bash)
        2. Send Ctrl-C twice to kill the crashed harness
        3. Parse resume UUID from Claude's exit output in the terminal
        4. Reset terminal with stty sane
        5. Resume Claude with --resume <uuid>
        6. Unpause message queue

        Recovery flow (graceful=True, harness survived):
        1. Pause message queue
        2. Send /exit + Enter to cleanly shut down the harness
        3. Parse resume UUID from Claude's exit output
        4. Resume Claude with --resume <uuid>
        5. Unpause message queue

        Args:
            session: Session to recover
            graceful: If True, use /exit instead of Ctrl-C (harness is still alive)

        Returns:
            True if recovery successful, False otherwise
        """
        if session.provider != "claude":
            logger.warning(f"Crash recovery only supported for Claude sessions, not {session.provider}")
            return False

        logger.info(f"Starting crash recovery for session {session.id}")

        # 1. Pause message queue
        if self.message_queue_manager:
            self.message_queue_manager.pause_session(session.id)

        try:
            # 2. Shut down the harness
            if graceful:
                # Harness survived the crash — use /exit for a clean shutdown
                logger.debug(f"Sending /exit to session {session.id} (graceful)")
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "send-keys", "-t", session.tmux_session, "Escape",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=5)
                await asyncio.sleep(0.3)
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "send-keys", "-t", session.tmux_session, "/exit", "Enter",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=5)
                await asyncio.sleep(3.0)
            else:
                # Harness is dead — Ctrl-C to force kill
                logger.debug(f"Sending C-c twice to session {session.id}")
                for _ in range(2):
                    proc = await asyncio.create_subprocess_exec(
                        "tmux", "send-keys", "-t", session.tmux_session, "C-c",
                        stdout=asyncio.subprocess.PIPE,
                        stderr=asyncio.subprocess.PIPE,
                    )
                    await asyncio.wait_for(proc.communicate(), timeout=5)
                    await asyncio.sleep(0.5)

                # Wait for Claude to print exit message (crash dump is large)
                await asyncio.sleep(3.0)

            # 4. Parse resume ID from Claude's exit output
            #    Claude prints: "To resume this conversation, run:\n  claude --resume <uuid>"
            resume_uuid = None
            proc = await asyncio.create_subprocess_exec(
                "tmux", "capture-pane", "-p", "-t", session.tmux_session, "-S", "-200",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=5)
            if proc.returncode == 0:
                import re
                # Match Claude's specific exit block:
                #   "To resume this conversation, run:\n  claude --resume <uuid>"
                match = re.search(
                    r'To resume this conversation.*?--resume\s+([0-9a-f-]{36})',
                    stdout.decode(),
                    re.DOTALL,
                )
                if match:
                    resume_uuid = match.group(1)
                    logger.info(f"Parsed resume UUID from terminal output: {resume_uuid}")

            if not resume_uuid:
                # Fallback to stored transcript_path
                if session.transcript_path:
                    resume_uuid = Path(session.transcript_path).stem
                    logger.warning(f"Could not parse resume UUID from output, falling back to transcript_path: {resume_uuid}")
                else:
                    logger.error(f"Cannot recover session {session.id}: no resume UUID found")
                    return False

            # 5. Reset terminal with stty sane (only needed for forceful Ctrl-C recovery)
            if not graceful:
                logger.debug(f"Sending stty sane to session {session.id}")
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "send-keys", "-t", session.tmux_session, "stty sane", "Enter",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=5)
                await asyncio.sleep(0.5)

            # 6. Unset CLAUDECODE to prevent nested-session detection
            #    (Claude Code exports this; it persists in the shell after the process dies)
            proc = await asyncio.create_subprocess_exec(
                "tmux", "send-keys", "-t", session.tmux_session,
                "unset CLAUDECODE", "Enter",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=5)
            await asyncio.sleep(0.3)

            # 7. Build resume command with config args
            claude_config = self.config.get("claude", {})
            command = claude_config.get("command", "claude")
            args = claude_config.get("args", [])

            # Build full command: claude [args] --resume <uuid>
            resume_cmd = f"{command}"
            if args:
                resume_cmd += " " + " ".join(args)
            resume_cmd += f" --resume {resume_uuid}"

            logger.debug(f"Sending resume command to session {session.id}: {resume_cmd}")
            proc = await asyncio.create_subprocess_exec(
                "tmux", "send-keys", "-t", session.tmux_session, resume_cmd, "Enter",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=5)

            # Wait for Claude to start
            await asyncio.sleep(3.0)

            # Update session state
            session.recovery_count += 1
            session.last_activity = datetime.now()
            session.status = SessionStatus.IDLE  # Claude starts idle after resume
            self._save_state()

            logger.info(
                f"Crash recovery complete for session {session.id} "
                f"(recovery count: {session.recovery_count})"
            )
            return True

        except asyncio.TimeoutError:
            logger.error(f"Timeout during crash recovery for session {session.id}")
            return False
        except Exception as e:
            logger.error(f"Crash recovery failed for session {session.id}: {e}")
            return False
        finally:
            # 6. Always unpause message queue (even on failure)
            if self.message_queue_manager:
                self.message_queue_manager.unpause_session(session.id)
