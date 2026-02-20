"""Message queue manager for reliable inter-agent messaging (sm-send-v2)."""

import asyncio
import logging
import shlex
import sqlite3
import subprocess
import threading
import uuid
from datetime import datetime, timedelta
from pathlib import Path
from typing import Optional, Dict, List, Callable, Awaitable, Tuple

from .models import QueuedMessage, SessionDeliveryState, SessionStatus, RemindRegistration, ParentWakeRegistration

logger = logging.getLogger(__name__)


class MessageQueueManager:
    """
    Manages queued messages and delivers them reliably when sessions become idle.

    Key features:
    - SQLite persistence for crash recovery
    - IDLE_PROMPT detection via Stop hook
    - User input detection and save/restore
    - Batch message delivery
    - Delivery modes: sequential, important, urgent
    """

    _STOP_SUPPRESS_WINDOW_SECONDS = 10

    def __init__(
        self,
        session_manager,
        db_path: str = "~/.local/share/claude-sessions/message_queue.db",
        config: Optional[dict] = None,
        notifier=None,
    ):
        """
        Initialize message queue manager.

        Args:
            session_manager: SessionManager instance
            db_path: Path to SQLite database
            config: Optional config dict with sm_send settings
            notifier: Optional Notifier instance for Telegram mirroring
        """
        self.session_manager = session_manager
        self.notifier = notifier
        self.db_path = Path(db_path).expanduser()
        self.db_path.parent.mkdir(parents=True, exist_ok=True)

        # Configuration - full config with sm_send section
        config = config or {}
        sm_send_config = config.get("sm_send", {})
        self.input_poll_interval = sm_send_config.get("input_poll_interval", 5)  # seconds
        self.input_stale_timeout = sm_send_config.get("input_stale_timeout", 120)  # seconds
        self.max_batch_size = sm_send_config.get("max_batch_size", 10)
        self.urgent_delay_ms = sm_send_config.get("urgent_delay_ms", 500)

        # Remind configuration (#188)
        remind_config = config.get("remind", {})
        self.remind_soft_threshold_default = remind_config.get("soft_threshold_seconds", 180)
        self.remind_hard_gap_seconds = remind_config.get("hard_gap_seconds", 120)

        # Load timeout configuration with fallbacks
        timeouts = config.get("timeouts", {})
        mq_timeouts = timeouts.get("message_queue", {})
        self.subprocess_timeout = mq_timeouts.get("subprocess_timeout_seconds", 2)
        self.async_send_timeout = mq_timeouts.get("async_send_timeout_seconds", 5)
        self.initial_retry_delay = mq_timeouts.get("initial_retry_delay_seconds", 1.0)
        self.max_retry_delay = mq_timeouts.get("max_retry_delay_seconds", 30)
        self.watch_poll_interval = mq_timeouts.get("watch_poll_interval_seconds", 2)
        self.skip_fence_window_seconds = mq_timeouts.get("skip_fence_window_seconds", 8)  # sm#232

        # In-memory state (not persisted - rebuilt from hooks)
        self.delivery_states: Dict[str, SessionDeliveryState] = {}

        # Per-session delivery locks to prevent double-delivery race condition
        self._delivery_locks: Dict[str, asyncio.Lock] = {}

        # Sessions paused for recovery (delivery blocked until unpaused)
        self._paused_sessions: set[str] = set()

        # Background task
        self._running = False
        self._monitor_task: Optional[asyncio.Task] = None
        self._scheduled_tasks: Dict[str, asyncio.Task] = {}  # reminder_id -> task

        # Periodic remind registrations (#188): keyed by target_session_id (one-active-per-target)
        self._remind_registrations: Dict[str, RemindRegistration] = {}
        self._remind_tasks: Dict[str, asyncio.Task] = {}  # target_session_id -> task

        # Parent wake-up registrations (#225-C): keyed by child_session_id
        self._parent_wake_registrations: Dict[str, ParentWakeRegistration] = {}
        self._parent_wake_tasks: Dict[str, asyncio.Task] = {}  # child_session_id -> task

        # Recent stop notifications for suppressing redundant sm wait idle (#216)
        # Key: (recipient_session_id, sender_session_id) — (target, watcher)
        # Value: datetime when stop notification was sent
        self._recent_stop_notifications: Dict[Tuple[str, str], datetime] = {}

        # Notification callback (set by main app)
        self._notify_callback: Optional[Callable] = None

        # Persistent database connection with thread-safety
        self._db_conn: Optional[sqlite3.Connection] = None
        self._db_lock = threading.Lock()

        # Initialize database
        self._init_db()

    def _init_db(self):
        """Initialize SQLite database schema with persistent connection."""
        # Create persistent connection with thread-safety enabled
        self._db_conn = sqlite3.connect(str(self.db_path), check_same_thread=False)

        # Enable WAL mode for better concurrency
        self._db_conn.execute("PRAGMA journal_mode=WAL")

        # Create schema
        cursor = self._db_conn.cursor()
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS message_queue (
                id TEXT PRIMARY KEY,
                target_session_id TEXT NOT NULL,
                sender_session_id TEXT,
                sender_name TEXT,
                text TEXT NOT NULL,
                delivery_mode TEXT DEFAULT 'sequential',
                queued_at TIMESTAMP NOT NULL,
                timeout_at TIMESTAMP,
                notify_on_delivery INTEGER DEFAULT 0,
                notify_after_seconds INTEGER,
                notify_on_stop INTEGER DEFAULT 0,
                delivered_at TIMESTAMP,
                remind_soft_threshold INTEGER,
                remind_hard_threshold INTEGER,
                parent_session_id TEXT,
                message_category TEXT DEFAULT NULL
            )
        """)
        cursor.execute("""
            CREATE INDEX IF NOT EXISTS idx_pending
            ON message_queue(target_session_id, delivered_at)
            WHERE delivered_at IS NULL
        """)
        # Scheduled reminders table
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS scheduled_reminders (
                id TEXT PRIMARY KEY,
                target_session_id TEXT NOT NULL,
                message TEXT NOT NULL,
                fire_at TIMESTAMP NOT NULL,
                task_type TEXT DEFAULT 'reminder',
                fired INTEGER DEFAULT 0
            )
        """)
        # Migration: add new columns to message_queue if they don't exist
        cursor.execute("PRAGMA table_info(message_queue)")
        columns = [col[1] for col in cursor.fetchall()]
        if "notify_on_stop" not in columns:
            cursor.execute("ALTER TABLE message_queue ADD COLUMN notify_on_stop INTEGER DEFAULT 0")
            logger.info("Migrated message_queue: added notify_on_stop column")
        if "remind_soft_threshold" not in columns:
            cursor.execute("ALTER TABLE message_queue ADD COLUMN remind_soft_threshold INTEGER")
            logger.info("Migrated message_queue: added remind_soft_threshold column")
        if "remind_hard_threshold" not in columns:
            cursor.execute("ALTER TABLE message_queue ADD COLUMN remind_hard_threshold INTEGER")
            logger.info("Migrated message_queue: added remind_hard_threshold column")
        if "parent_session_id" not in columns:
            cursor.execute("ALTER TABLE message_queue ADD COLUMN parent_session_id TEXT")
            logger.info("Migrated message_queue: added parent_session_id column")
        if "message_category" not in columns:
            cursor.execute("ALTER TABLE message_queue ADD COLUMN message_category TEXT DEFAULT NULL")
            logger.info("Migrated message_queue: added message_category column")

        # Remind registrations table (#188)
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS remind_registrations (
                id TEXT PRIMARY KEY,
                target_session_id TEXT NOT NULL UNIQUE,
                soft_threshold_seconds INTEGER NOT NULL,
                hard_threshold_seconds INTEGER NOT NULL,
                registered_at TIMESTAMP NOT NULL,
                last_reset_at TIMESTAMP NOT NULL,
                soft_fired INTEGER DEFAULT 0,
                is_active INTEGER DEFAULT 1
            )
        """)

        # Parent wake-up registrations table (#225-C)
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS parent_wake_registrations (
                id TEXT PRIMARY KEY,
                child_session_id TEXT NOT NULL UNIQUE,
                parent_session_id TEXT NOT NULL,
                period_seconds INTEGER NOT NULL,
                registered_at TIMESTAMP NOT NULL,
                last_wake_at TIMESTAMP,
                last_status_at_prev_wake TIMESTAMP,
                escalated INTEGER DEFAULT 0,
                is_active INTEGER DEFAULT 1
            )
        """)

        self._db_conn.commit()
        logger.info(f"Message queue database initialized at {self.db_path} (WAL mode enabled)")

    def _execute(self, query: str, params=()) -> sqlite3.Cursor:
        """
        Execute a database query with thread-safety.

        Args:
            query: SQL query string
            params: Query parameters tuple

        Returns:
            Cursor object
        """
        with self._db_lock:
            cursor = self._db_conn.cursor()
            cursor.execute(query, params)
            self._db_conn.commit()
            return cursor

    def _execute_query(self, query: str, params=()) -> List:
        """
        Execute a SELECT query and return all results.

        Args:
            query: SQL query string
            params: Query parameters tuple

        Returns:
            List of rows
        """
        with self._db_lock:
            cursor = self._db_conn.cursor()
            cursor.execute(query, params)
            return cursor.fetchall()

    def set_notify_callback(self, callback: Callable):
        """Set callback for delivery notifications."""
        self._notify_callback = callback

    async def _mirror_to_telegram(self, text: str, session, event_type: str = "agent_comm"):
        """
        Mirror message to Telegram. Fire-and-forget: never blocks delivery.

        Args:
            text: Message text to mirror
            session: Session object (must have telegram_chat_id)
            event_type: Event type for logging/categorization
        """
        if not self.notifier or not session or not session.telegram_chat_id:
            return
        try:
            from .models import NotificationEvent
            event = NotificationEvent(
                session_id=session.id,
                event_type=event_type,
                message=text,
            )
            await self.notifier.notify(event, session)
        except Exception as e:
            logger.warning(f"Telegram mirror failed (non-fatal): {e}")

    async def start(self):
        """Start the queue monitoring service."""
        self._running = True
        self._monitor_task = asyncio.create_task(self._monitor_loop())
        # Recover pending reminders from database
        await self._recover_scheduled_reminders()
        # Recover active periodic remind registrations (#188)
        await self._recover_remind_registrations()
        # Recover active parent wake registrations (#225-C)
        await self._recover_parent_wake_registrations()
        # Recover pending messages - trigger delivery for sessions with queued messages
        await self._recover_pending_messages()
        logger.info("Message queue manager started")

    async def _recover_pending_messages(self):
        """
        Trigger delivery check for sessions with pending messages on startup.

        After a server restart, in-memory idle state is lost. This ensures
        messages queued before the restart get delivered promptly.
        """
        sessions_with_pending = self._get_sessions_with_pending()
        for session_id in sessions_with_pending:
            # Check if session still exists
            session = self.session_manager.get_session(session_id)
            if not session:
                count = self.get_queue_length(session_id)
                logger.warning(
                    f"Session {session_id} has {count} pending message(s) but session no longer exists. "
                    f"Messages will be cleaned up."
                )
                # Clean up messages for non-existent session
                self._cleanup_messages_for_session(session_id)
                continue

            count = self.get_queue_length(session_id)
            # Mark session as idle to trigger delivery
            # If Claude is actually busy, the next activity will mark it active
            self.mark_session_idle(session_id)
            logger.info(f"Recovered session {session_id} with {count} pending message(s), marked idle")

    async def stop(self):
        """Stop the queue monitoring service."""
        self._running = False
        if self._monitor_task:
            self._monitor_task.cancel()
            try:
                await self._monitor_task
            except asyncio.CancelledError:
                pass
        # Cancel all scheduled tasks
        for task in self._scheduled_tasks.values():
            task.cancel()
        self._scheduled_tasks.clear()
        # Cancel all remind tasks (#188)
        for task in self._remind_tasks.values():
            task.cancel()
        self._remind_tasks.clear()
        # Close database connection
        if self._db_conn:
            self._db_conn.close()
            self._db_conn = None
        logger.info("Message queue manager stopped")

    # =========================================================================
    # IDLE State Management (called by Stop hook handler)
    # =========================================================================

    def mark_session_idle(self, session_id: str, last_output: Optional[str] = None, from_stop_hook: bool = False):
        """
        Mark a session as idle (called when Stop hook fires).

        This triggers delivery check for any queued messages and
        sends stop notification to sender if requested.

        Args:
            session_id: Session that became idle
            last_output: Output from this specific Stop hook invocation
            from_stop_hook: True when called from the Stop hook handler;
                only Stop hook invocations may consume skip_count slots (#174)
        """
        state = self._get_or_create_state(session_id)
        logger.info(f"Session {session_id} marked idle")

        # Cancel periodic remind and parent wake on stop hook — agent completed their task (#188, #225-C)
        if from_stop_hook:
            self.cancel_remind(session_id)
            self.cancel_parent_wake(session_id)

        # Check for pending handoff — takes priority over all other Stop hook logic (#196).
        # Only execute on Stop hook calls; other callers (queue_message, recovery) must not trigger it.
        if from_stop_hook and getattr(state, "pending_handoff_path", None):
            file_path = state.pending_handoff_path
            state.pending_handoff_path = None  # Clear before execution
            state.is_idle = False  # Signal to server.py that handoff is in progress
            asyncio.create_task(self._execute_handoff(session_id, file_path))
            return  # Skip stop notification and queued message delivery

        # Absorb stop hooks generated by /clear commands (#174, sm#232).
        # Only Stop hook callers may consume skip slots — other callers
        # (queue_message sequential path, _recover_pending_messages) must not.
        # Time-bounded: only absorb if the fence was armed within the curl timeout window
        # (default 8s). If stale, reset the entire fence and fall through so a legitimate
        # Stop hook from the new task is not silently dropped.
        if from_stop_hook and state.stop_notify_skip_count > 0:
            armed_at = state.skip_count_armed_at
            if armed_at and (datetime.now() - armed_at).total_seconds() < self.skip_fence_window_seconds:
                # Within window: absorb this /clear Stop hook.
                # Do NOT set is_idle here — agent may already be processing new task.
                # Preserves is_idle=False if mark_session_active already ran.
                state.stop_notify_skip_count -= 1
                if state.stop_notify_skip_count == 0:
                    state.skip_count_armed_at = None  # hygiene: clear when fence fully consumed
                logger.debug(
                    f"Session {session_id}: skip_count decremented to {state.stop_notify_skip_count}; "
                    f"stop notification deferred (sender_id preserved: {state.stop_notify_sender_id})"
                )
                asyncio.create_task(self._try_deliver_messages(session_id))
                return
            else:
                # Stale arm (hook was lost): reset entire fence atomically and fall through
                # so the next real Stop hook sets is_idle=True correctly (sm#232).
                state.stop_notify_skip_count = 0
                state.skip_count_armed_at = None
                logger.warning(
                    f"Session {session_id}: skip fence was stale "
                    f"(armed >{self.skip_fence_window_seconds}s ago), resetting"
                )

        # Now safe to mark idle — skip check did not absorb this Stop hook
        state.is_idle = True
        state.last_idle_at = datetime.now()

        # Suppress redundant stop notification if agent recently sm-sent to the
        # same target that would receive the notification (#182)
        SUPPRESSION_WINDOW_SECONDS = 30
        if state.stop_notify_sender_id and state.last_outgoing_sm_send_target:
            if (state.stop_notify_sender_id == state.last_outgoing_sm_send_target
                    and state.last_outgoing_sm_send_at
                    and (datetime.now() - state.last_outgoing_sm_send_at).total_seconds()
                        < SUPPRESSION_WINDOW_SECONDS):
                logger.info(
                    f"Suppressing stop notification for {session_id}: "
                    f"agent sm-sent to {state.stop_notify_sender_id} "
                    f"{(datetime.now() - state.last_outgoing_sm_send_at).total_seconds():.1f}s ago (#182)"
                )
                state.stop_notify_sender_id = None
                state.stop_notify_sender_name = None
                state.last_outgoing_sm_send_target = None
                state.last_outgoing_sm_send_at = None

        # Send stop notification if a sender is waiting
        if state.stop_notify_sender_id:
            asyncio.create_task(self._send_stop_notification(
                recipient_session_id=session_id,
                sender_session_id=state.stop_notify_sender_id,
                sender_name=state.stop_notify_sender_name,
                last_output=last_output,
            ))
            # Clear after sending
            state.stop_notify_sender_id = None
            state.stop_notify_sender_name = None

        # Promote paste-buffered stop notification (sm#244).
        # If a message was pasted mid-turn (is_idle=False at paste time), the sender was
        # staged in paste_buffered to avoid a false notification on Task X's Stop hook.
        # On the first idle transition, promote to stop_notify_sender_id so the NEXT
        # Stop hook fires the notification (after Task Y completes).
        if state.paste_buffered_notify_sender_id:
            state.stop_notify_sender_id = state.paste_buffered_notify_sender_id
            state.stop_notify_sender_name = state.paste_buffered_notify_sender_name
            state.paste_buffered_notify_sender_id = None
            state.paste_buffered_notify_sender_name = None
            logger.debug(
                f"Session {session_id}: promoted paste-buffered stop-notify to "
                f"stop_notify_sender_id={state.stop_notify_sender_id} (sm#244)"
            )

        # Trigger async delivery check
        asyncio.create_task(self._try_deliver_messages(session_id))

    def mark_session_active(self, session_id: str):
        """Mark a session as active (not idle)."""
        state = self._get_or_create_state(session_id)
        state.is_idle = False
        # Sync session.status with in-memory state to prevent Phase 3 false positives (#191).
        # session.status stays IDLE after Stop hook; mark_session_active must clear it so
        # _watch_for_idle Phase 3 doesn't fire false idle immediately after urgent dispatch.
        session = self.session_manager.get_session(session_id)
        if session and session.status != SessionStatus.STOPPED:
            session.status = SessionStatus.RUNNING
        logger.debug(f"Session {session_id} marked active")

    def is_session_idle(self, session_id: str) -> bool:
        """Check if a session is idle."""
        state = self.delivery_states.get(session_id)
        return state.is_idle if state else False

    def pause_session(self, session_id: str):
        """
        Pause message delivery to a session (used during crash recovery).

        While paused, messages remain queued but delivery is blocked.
        This prevents sm send from going to bash during harness restart.
        """
        self._paused_sessions.add(session_id)
        logger.info(f"Session {session_id} paused for recovery")

    def unpause_session(self, session_id: str):
        """
        Resume message delivery to a session after recovery.

        Triggers delivery if pending messages exist.
        """
        self._paused_sessions.discard(session_id)
        logger.info(f"Session {session_id} unpaused after recovery")

        # Trigger delivery if pending messages exist.
        # Cannot rely on delivery_states.get() — if urgent delivery returned
        # early due to pause, no state entry was created (#154).
        pending = self.get_pending_messages(session_id)
        if pending:
            logger.info(f"Session {session_id} has {len(pending)} pending messages, scheduling delivery")
            asyncio.create_task(self._try_deliver_messages(session_id))

    def is_session_paused(self, session_id: str) -> bool:
        """Check if a session is paused for recovery."""
        return session_id in self._paused_sessions

    def _get_or_create_state(self, session_id: str) -> SessionDeliveryState:
        """Get or create delivery state for a session."""
        if session_id not in self.delivery_states:
            self.delivery_states[session_id] = SessionDeliveryState(session_id=session_id)
        return self.delivery_states[session_id]

    # =========================================================================
    # Message Queueing
    # =========================================================================

    def queue_message(
        self,
        target_session_id: str,
        text: str,
        sender_session_id: Optional[str] = None,
        sender_name: Optional[str] = None,
        delivery_mode: str = "sequential",
        timeout_seconds: Optional[int] = None,
        notify_on_delivery: bool = False,
        notify_after_seconds: Optional[int] = None,
        notify_on_stop: bool = False,
        remind_soft_threshold: Optional[int] = None,
        remind_hard_threshold: Optional[int] = None,
        parent_session_id: Optional[str] = None,
        message_category: Optional[str] = None,
    ) -> QueuedMessage:
        """
        Queue a message for delivery.

        Args:
            target_session_id: Target session ID
            text: Message text (already formatted with sender metadata)
            sender_session_id: Sender session ID
            sender_name: Sender friendly name
            delivery_mode: sequential, important, or urgent
            timeout_seconds: Drop message if not delivered in this time
            notify_on_delivery: Notify sender when delivered
            notify_after_seconds: Notify sender N seconds after delivery
            notify_on_stop: Notify sender when receiver's Stop hook fires
            remind_soft_threshold: Seconds after delivery before soft remind fires (#188)
            remind_hard_threshold: Seconds after delivery before hard remind fires (#188)
            parent_session_id: EM session to wake periodically after delivery (#225-C)
            message_category: Optional category tag, e.g. 'context_monitor', for scoped cancellation (#241)

        Returns:
            QueuedMessage with assigned ID
        """
        msg = QueuedMessage(
            target_session_id=target_session_id,
            sender_session_id=sender_session_id,
            sender_name=sender_name,
            text=text,
            delivery_mode=delivery_mode,
            queued_at=datetime.now(),
            timeout_at=datetime.now() + timedelta(seconds=timeout_seconds) if timeout_seconds else None,
            notify_on_delivery=notify_on_delivery,
            notify_after_seconds=notify_after_seconds,
            notify_on_stop=notify_on_stop,
            remind_soft_threshold=remind_soft_threshold,
            remind_hard_threshold=remind_hard_threshold,
            parent_session_id=parent_session_id,
            message_category=message_category,
        )

        # Persist to database
        self._execute("""
            INSERT INTO message_queue
            (id, target_session_id, sender_session_id, sender_name, text,
             delivery_mode, queued_at, timeout_at, notify_on_delivery, notify_after_seconds,
             notify_on_stop, remind_soft_threshold, remind_hard_threshold, parent_session_id,
             message_category)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """, (
            msg.id,
            msg.target_session_id,
            msg.sender_session_id,
            msg.sender_name,
            msg.text,
            msg.delivery_mode,
            msg.queued_at.isoformat(),
            msg.timeout_at.isoformat() if msg.timeout_at else None,
            1 if msg.notify_on_delivery else 0,
            msg.notify_after_seconds,
            1 if msg.notify_on_stop else 0,
            msg.remind_soft_threshold,
            msg.remind_hard_threshold,
            msg.parent_session_id,
            msg.message_category,
        ))

        queue_len = self.get_queue_length(target_session_id)
        logger.info(f"Queued message {msg.id} for {target_session_id} (mode={delivery_mode}, queue={queue_len})")

        # Codex CLI sessions have no hooks so idle detection never triggers.
        # Force immediate delivery for all non-urgent modes.
        session = self.session_manager.get_session(target_session_id)
        is_codex = session and getattr(session, "provider", "claude") == "codex"

        # If urgent mode, trigger immediate delivery
        if delivery_mode == "urgent":
            if target_session_id not in self._paused_sessions:
                self.mark_session_active(target_session_id)
            asyncio.create_task(self._deliver_urgent(target_session_id, msg))
        elif is_codex:
            # Codex: set idle flag and deliver immediately, but skip the
            # stop-notification side effects of mark_session_idle() since
            # this isn't a real stop event.
            # Reset any stale idle status from prior work cycle BEFORE setting is_idle=True.
            # Without this, _watch_for_idle Phase 3 can see session.status=IDLE from
            # OutputMonitor and fire a false idle during the delivery window. (#193)
            # Mirror the urgent path: skip mark_session_active if session is paused for recovery.
            if target_session_id not in self._paused_sessions:
                self.mark_session_active(target_session_id)
            state = self._get_or_create_state(target_session_id)
            state.is_idle = True  # re-set: mark_session_active clears is_idle; need True to gate delivery
            asyncio.create_task(self._try_deliver_messages(target_session_id))
        # If important mode, trigger delivery directly (tty buffer handles ordering, sm#244)
        elif delivery_mode == "important":
            asyncio.create_task(self._try_deliver_messages(target_session_id, important_only=True))
        # For sequential mode, deliver directly — no idle gate needed (sm#244)
        elif delivery_mode == "sequential":
            asyncio.create_task(self._try_deliver_messages(target_session_id))

        return msg

    def get_pending_messages(self, session_id: str) -> List[QueuedMessage]:
        """Get all pending (undelivered) messages for a session."""
        rows = self._execute_query("""
            SELECT id, target_session_id, sender_session_id, sender_name, text,
                   delivery_mode, queued_at, timeout_at, notify_on_delivery,
                   notify_after_seconds, notify_on_stop, delivered_at,
                   remind_soft_threshold, remind_hard_threshold, parent_session_id,
                   message_category
            FROM message_queue
            WHERE target_session_id = ? AND delivered_at IS NULL
            ORDER BY queued_at ASC
        """, (session_id,))

        messages = []
        for row in rows:
            msg = QueuedMessage(
                id=row[0],
                target_session_id=row[1],
                sender_session_id=row[2],
                sender_name=row[3],
                text=row[4],
                delivery_mode=row[5],
                queued_at=datetime.fromisoformat(row[6]),
                timeout_at=datetime.fromisoformat(row[7]) if row[7] else None,
                notify_on_delivery=bool(row[8]),
                notify_after_seconds=row[9],
                notify_on_stop=bool(row[10]),
                delivered_at=datetime.fromisoformat(row[11]) if row[11] else None,
                remind_soft_threshold=row[12],
                remind_hard_threshold=row[13],
                parent_session_id=row[14],
                message_category=row[15],
            )
            # Skip expired messages
            if msg.timeout_at and datetime.now() > msg.timeout_at:
                self._mark_expired(msg.id)
                continue
            messages.append(msg)
        return messages

    def get_queue_length(self, session_id: str) -> int:
        """Get the number of pending messages for a session."""
        return len(self.get_pending_messages(session_id))

    def _mark_delivered(self, message_id: str):
        """Mark a message as delivered in the database."""
        self._execute("""
            UPDATE message_queue SET delivered_at = ? WHERE id = ?
        """, (datetime.now().isoformat(), message_id))

    def _mark_expired(self, message_id: str):
        """Mark a message as expired (delete it)."""
        self._execute("DELETE FROM message_queue WHERE id = ?", (message_id,))
        logger.info(f"Message {message_id} expired and deleted")

    def _cleanup_messages_for_session(self, session_id: str):
        """Clean up all pending messages for a session that no longer exists."""
        # First get the count for logging
        rows = self._execute_query(
            "SELECT COUNT(*) FROM message_queue WHERE target_session_id = ? AND delivered_at IS NULL",
            (session_id,)
        )
        count = rows[0][0] if rows else 0

        # Delete all pending messages for this session
        self._execute(
            "DELETE FROM message_queue WHERE target_session_id = ? AND delivered_at IS NULL",
            (session_id,)
        )
        logger.info(f"Cleaned up {count} pending message(s) for non-existent session {session_id}")

    def cancel_context_monitor_messages_from(self, sender_session_id: str) -> int:
        """Cancel undelivered context-monitor notifications from sender_session_id.

        Called on sm clear to discard stale compaction/warning/critical alerts
        before they reach the parent EM (#241). Does NOT affect sm send traffic
        from the same sender (those have message_category=NULL).

        Returns:
            Number of messages cancelled.
        """
        rows = self._execute_query(
            "SELECT COUNT(*) FROM message_queue "
            "WHERE sender_session_id = ? AND message_category = 'context_monitor' AND delivered_at IS NULL",
            (sender_session_id,)
        )
        count = rows[0][0] if rows else 0
        if count:
            self._execute(
                "DELETE FROM message_queue "
                "WHERE sender_session_id = ? AND message_category = 'context_monitor' AND delivered_at IS NULL",
                (sender_session_id,)
            )
            logger.info(
                f"Cancelled {count} stale context-monitor message(s) from cleared session {sender_session_id}"
            )
        return count

    # =========================================================================
    # User Input Detection and Management
    # =========================================================================

    async def _get_pending_user_input_async(self, tmux_session: str) -> Optional[str]:
        """
        Check if user has typed something at the prompt (async, non-blocking).

        Returns the user's typed text if present, None otherwise.
        """
        try:
            proc = await asyncio.create_subprocess_exec(
                "tmux", "capture-pane", "-p", "-t", tmux_session,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)

            if proc.returncode != 0:
                return None

            output = stdout.decode().strip()
            if not output:
                return None

            # Get the last line
            lines = output.split('\n')
            last_line = lines[-1] if lines else ""

            # Check for Claude Code prompt pattern ("> ")
            if last_line.startswith('> '):
                user_text = last_line[2:]  # Remove "> "
                if user_text.strip():  # Has non-whitespace content
                    return user_text

            return None
        except Exception as e:
            logger.error(f"Error checking user input: {e}")
            return None

    async def _clear_user_input_async(self, tmux_session: str) -> bool:
        """Clear the current input line using Ctrl+U (async, non-blocking)."""
        try:
            proc = await asyncio.create_subprocess_exec(
                "tmux", "send-keys", "-t", tmux_session, "C-u",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)
            return proc.returncode == 0
        except Exception as e:
            logger.error(f"Error clearing user input: {e}")
            return False

    async def _restore_user_input_async(self, tmux_session: str, text: str):
        """Restore previously saved user input (async, non-blocking)."""
        try:
            # Use list-based subprocess with "--" to handle text starting with "-"
            proc = await asyncio.create_subprocess_exec(
                "tmux", "send-keys", "-t", tmux_session, "--", text,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=self.async_send_timeout)
            if proc.returncode == 0:
                logger.info(f"Restored user input: {text[:50]}...")
        except Exception as e:
            logger.error(f"Error restoring user input: {e}")

    # =========================================================================
    # Message Delivery
    # =========================================================================

    async def _monitor_loop(self):
        """
        Main monitoring loop - checks for stale user input.

        Automatically restarts on errors to ensure continuous monitoring.
        """
        retry_count = 0
        max_retries = 5
        retry_delay = self.initial_retry_delay

        while self._running:
            try:
                while self._running:
                    # Check each session with pending messages
                    sessions_with_pending = self._get_sessions_with_pending()

                    for session_id in sessions_with_pending:
                        await self._check_stale_input(session_id)

                    await asyncio.sleep(self.input_poll_interval)
                    retry_count = 0  # Reset on successful iteration
            except asyncio.CancelledError:
                logger.info("Monitor loop cancelled")
                break
            except Exception as e:
                retry_count += 1
                if retry_count <= max_retries:
                    logger.error(
                        f"CRITICAL: Error in monitor loop (retry {retry_count}/{max_retries}): {e}",
                        exc_info=True
                    )
                    logger.warning(f"Restarting monitor loop in {retry_delay}s...")
                    await asyncio.sleep(retry_delay)
                    retry_delay = min(retry_delay * 2, self.max_retry_delay)  # Exponential backoff
                else:
                    logger.error(
                        f"CRITICAL: Monitor loop failed {max_retries} times, giving up: {e}",
                        exc_info=True
                    )
                    logger.error("Message queue monitoring STOPPED! Messages may not be delivered.")
                    self._running = False
                    break

    def _get_sessions_with_pending(self) -> List[str]:
        """Get list of session IDs with pending messages."""
        rows = self._execute_query("""
            SELECT DISTINCT target_session_id
            FROM message_queue
            WHERE delivered_at IS NULL
        """)
        return [row[0] for row in rows]

    async def _check_stale_input(self, session_id: str):
        """Check if user input has become stale and trigger delivery."""
        state = self._get_or_create_state(session_id)

        session = self.session_manager.get_session(session_id)
        if not session:
            return
        if getattr(session, "provider", "claude") == "codex-app":
            # Codex app-server has no tmux input line to inspect
            return

        current_input = await self._get_pending_user_input_async(session.tmux_session)

        if current_input:
            # User has typed something
            if state.pending_user_input == current_input:
                # Same text - check if stale
                if state.pending_input_first_seen:
                    elapsed = (datetime.now() - state.pending_input_first_seen).total_seconds()
                    if elapsed >= self.input_stale_timeout:
                        logger.info(f"User input stale after {elapsed:.0f}s, saving and delivering")
                        # Save the input
                        state.saved_user_input = current_input
                        # Clear the line
                        await self._clear_user_input_async(session.tmux_session)
                        # Trigger delivery
                        await self._try_deliver_messages(session_id)
            else:
                # Text changed - reset timer
                state.pending_user_input = current_input
                state.pending_input_first_seen = datetime.now()
                logger.debug(f"User input detected, starting stale timer: {current_input[:30]}...")
        else:
            # No input - clear tracking
            state.pending_user_input = None
            state.pending_input_first_seen = None

    async def _try_deliver_messages(self, session_id: str, important_only: bool = False):
        """
        Attempt to deliver pending messages to a session.

        Uses per-session lock to prevent double-delivery when multiple Stop hooks
        fire rapidly and create concurrent delivery tasks.

        Args:
            session_id: Target session ID
            important_only: Only deliver important mode messages
        """
        # Skip delivery if session is paused for recovery
        if session_id in self._paused_sessions:
            logger.debug(f"Session {session_id} paused for recovery, skipping delivery")
            return

        # Acquire per-session lock to prevent concurrent delivery
        lock = self._delivery_locks.setdefault(session_id, asyncio.Lock())
        async with lock:
            state = self._get_or_create_state(session_id)
            session = self.session_manager.get_session(session_id)

            if not session:
                logger.warning(f"Session {session_id} not found, cannot deliver")
                return

            # Get pending messages
            messages = self.get_pending_messages(session_id)
            if not messages:
                return

            # Filter by mode if needed
            if important_only:
                messages = [m for m in messages if m.delivery_mode == "important"]
                if not messages:
                    return
            # No idle gate for sequential or important: tty buffer handles ordering (sm#244)

            # Check for user input (final gate)
            current_input = None
            if getattr(session, "provider", "claude") != "codex-app":
                current_input = await self._get_pending_user_input_async(session.tmux_session)
            if current_input and not state.saved_user_input:
                # User is typing - don't inject
                logger.debug(f"User typing detected at final gate, aborting delivery")
                return

            # Batch messages (up to max_batch_size)
            batch = messages[:self.max_batch_size]

            # Format batch payload
            if len(batch) == 1:
                payload = batch[0].text
            else:
                # Multiple messages - concatenate with headers
                parts = []
                for msg in batch:
                    parts.append(msg.text)
                payload = "\n\n".join(parts)

            # Capture idle state before delivery: determines notify_on_stop path (sm#244)
            was_idle = state.is_idle

            # Inject the message (use async version to avoid blocking event loop)
            logger.info(f"Delivering {len(batch)} message(s) to {session_id}")
            success = await self.session_manager._deliver_direct(session, payload)

            if success:
                # Mark session as active
                state.is_idle = False

                # Mark messages as delivered
                for msg in batch:
                    self._mark_delivered(msg.id)
                    logger.info(f"Delivered message {msg.id}")

                    # Mirror to Telegram (fire-and-forget)
                    if self.notifier:
                        sender_display = msg.sender_name or (msg.sender_session_id[:8] if msg.sender_session_id else "system")
                        # Truncate message text for display
                        text_preview = msg.text[:200] if len(msg.text) > 200 else msg.text
                        mirror_text = f"📨 [{sender_display}] {text_preview}"
                        await self._mirror_to_telegram(mirror_text, session, "message_delivered")

                    # Handle delivery notifications
                    if msg.notify_on_delivery and msg.sender_session_id:
                        await self._send_delivery_notification(msg)

                    if msg.notify_after_seconds and msg.sender_session_id:
                        await self._schedule_followup_notification(msg)

                    # Track sender for stop notification (last message with notify_on_stop wins).
                    # Two-phase promotion (sm#244): if pasted mid-turn (was_idle=False), stage in
                    # paste_buffered — Task X's Stop hook will promote to stop_notify_sender_id
                    # so the notification fires after Task Y (not falsely after Task X).
                    # If pasted when idle (was_idle=True), arm stop_notify_sender_id directly:
                    # the agent consumes the paste immediately, so the next Stop hook IS Task Y.
                    if msg.notify_on_stop and msg.sender_session_id:
                        if was_idle:
                            state.stop_notify_sender_id = msg.sender_session_id
                            state.stop_notify_sender_name = msg.sender_name
                        else:
                            state.paste_buffered_notify_sender_id = msg.sender_session_id
                            state.paste_buffered_notify_sender_name = msg.sender_name

                    # Start periodic remind if requested (#188)
                    if msg.remind_soft_threshold is not None:
                        self.register_periodic_remind(
                            target_session_id=msg.target_session_id,
                            soft_threshold=msg.remind_soft_threshold,
                            hard_threshold=msg.remind_hard_threshold or (msg.remind_soft_threshold + self.remind_hard_gap_seconds),
                        )

                    # Start parent wake-up if requested (#225-C)
                    if msg.parent_session_id and msg.remind_soft_threshold is not None:
                        self.register_parent_wake(msg.target_session_id, msg.parent_session_id)

                # Update session activity
                session.last_activity = datetime.now()
                from .models import SessionStatus
                session.status = SessionStatus.RUNNING
                self.session_manager._save_state()
            else:
                logger.error(f"Failed to deliver messages to {session_id}")

    async def _deliver_urgent(self, session_id: str, msg: QueuedMessage):
        """Deliver an urgent message immediately, interrupting Claude."""
        # Skip delivery if session is paused for recovery
        if session_id in self._paused_sessions:
            logger.debug(f"Session {session_id} paused for recovery, deferring urgent delivery")
            return

        session = self.session_manager.get_session(session_id)
        if not session:
            logger.error(f"Session {session_id} not found for urgent delivery")
            return

        try:
            if getattr(session, "provider", "claude") == "codex-app":
                success = await self.session_manager._deliver_urgent(session, msg.text)
                if success:
                    self._mark_delivered(msg.id)
                    state = self._get_or_create_state(session_id)
                    state.is_idle = False
                    logger.info(f"Urgent message {msg.id} delivered to {session_id} (codex-app)")

                    # Handle notifications
                    if msg.notify_on_delivery and msg.sender_session_id:
                        await self._send_delivery_notification(msg)

                    # Track sender for stop notification
                    if msg.notify_on_stop and msg.sender_session_id:
                        state.stop_notify_sender_id = msg.sender_session_id
                        state.stop_notify_sender_name = msg.sender_name

                    # Start periodic remind if requested (#188)
                    if msg.remind_soft_threshold is not None:
                        self.register_periodic_remind(
                            target_session_id=msg.target_session_id,
                            soft_threshold=msg.remind_soft_threshold,
                            hard_threshold=msg.remind_hard_threshold or (msg.remind_soft_threshold + self.remind_hard_gap_seconds),
                        )

                    # Start parent wake-up if requested (#225-C)
                    if msg.parent_session_id and msg.remind_soft_threshold is not None:
                        self.register_parent_wake(msg.target_session_id, msg.parent_session_id)
                else:
                    logger.error(f"Failed to deliver urgent message to {session_id} (codex-app)")
                return

            # Acquire delivery lock to prevent racing with _try_deliver_messages (#178).
            # Without this, a Stop hook firing during prompt polling can cause
            # _try_deliver_messages to deliver sequential messages before the urgent
            # message, producing out-of-order delivery.
            lock = self._delivery_locks.setdefault(session_id, asyncio.Lock())
            async with lock:
                # If session is completed, wake it up first (like cmd_clear does)
                from src.models import CompletionStatus
                if session.completion_status == CompletionStatus.COMPLETED:
                    logger.info(f"Session {session_id} is completed, sending Enter to wake up")
                    # Send Enter to wake up the completed session
                    proc = await asyncio.create_subprocess_exec(
                        "tmux", "send-keys", "-t", session.tmux_session, "Enter",
                        stdout=asyncio.subprocess.PIPE,
                        stderr=asyncio.subprocess.PIPE,
                    )
                    await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)

                    # Wait for Claude to show prompt after wake-up (#175)
                    await self._wait_for_claude_prompt_async(session.tmux_session)

                # Send Escape to interrupt any streaming (async, non-blocking)
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "send-keys", "-t", session.tmux_session, "Escape",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)

                # Wait for Claude to show idle prompt before sending payload (#175)
                await self._wait_for_claude_prompt_async(session.tmux_session)

                # Inject message directly (use async version to avoid blocking)
                success = await self.session_manager._deliver_direct(session, msg.text)

                if success:
                    self._mark_delivered(msg.id)
                    state = self._get_or_create_state(session_id)
                    state.is_idle = False
                    logger.info(f"Urgent message {msg.id} delivered to {session_id}")

                    # Handle notifications
                    if msg.notify_on_delivery and msg.sender_session_id:
                        await self._send_delivery_notification(msg)

                    # Track sender for stop notification
                    if msg.notify_on_stop and msg.sender_session_id:
                        state.stop_notify_sender_id = msg.sender_session_id
                        state.stop_notify_sender_name = msg.sender_name

                    # Start periodic remind if requested (#188)
                    if msg.remind_soft_threshold is not None:
                        self.register_periodic_remind(
                            target_session_id=msg.target_session_id,
                            soft_threshold=msg.remind_soft_threshold,
                            hard_threshold=msg.remind_hard_threshold or (msg.remind_soft_threshold + self.remind_hard_gap_seconds),
                        )

                    # Start parent wake-up if requested (#225-C)
                    if msg.parent_session_id and msg.remind_soft_threshold is not None:
                        self.register_parent_wake(msg.target_session_id, msg.parent_session_id)
                else:
                    logger.error(f"Failed to deliver urgent message to {session_id}")

        except Exception as e:
            logger.error(f"Error delivering urgent message: {e}")

    async def _restore_user_input_after_response(self, session_id: str):
        """
        Called when Claude finishes responding to restore saved user input.
        This is triggered by the Stop hook after message delivery.
        """
        state = self.delivery_states.get(session_id)
        if not state or not state.saved_user_input:
            return

        session = self.session_manager.get_session(session_id)
        if not session:
            return
        if getattr(session, "provider", "claude") == "codex-app":
            # Codex app-server has no tmux input to restore
            state.saved_user_input = None
            return

        # Restore the saved input
        await self._restore_user_input_async(session.tmux_session, state.saved_user_input)

        # Clear saved input
        state.saved_user_input = None
        logger.info(f"Restored user input for session {session_id}")

    # =========================================================================
    # Notifications
    # =========================================================================

    async def _send_delivery_notification(self, msg: QueuedMessage):
        """Send delivery notification to sender."""
        if not msg.sender_session_id:
            return

        # Format notification
        truncated = msg.text[:100] + "..." if len(msg.text) > 100 else msg.text
        notification = f'[sm] Message delivered to {msg.target_session_id}\nOriginal: "{truncated}"'

        # Queue notification to sender (as system message)
        self.queue_message(
            target_session_id=msg.sender_session_id,
            text=notification,
            delivery_mode="sequential",
        )
        logger.info(f"Sent delivery notification to {msg.sender_session_id}")

        # Mirror to Telegram (fire-and-forget)
        sender_session = self.session_manager.get_session(msg.sender_session_id)
        if sender_session:
            mirror_text = f"✅ {notification}"
            await self._mirror_to_telegram(mirror_text, sender_session, "delivery_confirm")

    async def _send_stop_notification(
        self,
        recipient_session_id: str,
        sender_session_id: str,
        sender_name: Optional[str] = None,
        last_output: Optional[str] = None,
    ):
        """
        Send notification to sender when recipient's Stop hook fires.

        Args:
            recipient_session_id: Session that completed (Stop hook fired)
            sender_session_id: Session to notify
            sender_name: Optional friendly name of sender
            last_output: Direct output from the Stop hook invocation (bypasses cache)
        """
        # Get recipient name for the notification
        recipient_session = self.session_manager.get_session(recipient_session_id)
        recipient_name = (
            recipient_session.friendly_name or recipient_session.name or recipient_session_id
            if recipient_session else recipient_session_id
        )

        # Build notification with last output if available
        if last_output:
            # Truncate if too long (keep it readable)
            truncated = last_output[:500] + "..." if len(last_output) > 500 else last_output
            notification = f"[sm] {recipient_name} stopped:\n{truncated}"
        else:
            notification = f"[sm] {recipient_name} ({recipient_session_id[:8]}) completed (Stop hook fired)"

        # Queue notification to sender (as system message)
        self.queue_message(
            target_session_id=sender_session_id,
            text=notification,
            delivery_mode="important",
        )
        logger.info(f"Sent stop notification to {sender_session_id} (recipient: {recipient_session_id})")

        # Record timestamp so _watch_for_idle can suppress the redundant idle notification (#216)
        key = (recipient_session_id, sender_session_id)
        self._recent_stop_notifications[key] = datetime.now()

        # Mirror to Telegram (fire-and-forget)
        sender_session = self.session_manager.get_session(sender_session_id)
        if sender_session:
            mirror_text = f"🛑 {notification}"
            await self._mirror_to_telegram(mirror_text, sender_session, "stop_notify")

    async def _schedule_followup_notification(self, msg: QueuedMessage):
        """Schedule a follow-up notification after delivery."""
        if not msg.notify_after_seconds or not msg.sender_session_id:
            return

        async def send_followup():
            await asyncio.sleep(msg.notify_after_seconds)
            # Send notification after N seconds regardless of recipient state
            truncated = msg.text[:100] + "..." if len(msg.text) > 100 else msg.text
            notification = (
                f'[sm] Reminder: {msg.notify_after_seconds}s since your message to '
                f'{msg.target_session_id} was delivered\n'
                f'Original: "{truncated}"\n'
                f'You can check status with: sm output {msg.target_session_id}'
            )
            self.queue_message(
                target_session_id=msg.sender_session_id,
                text=notification,
                delivery_mode="sequential",
            )
            logger.info(f"Sent follow-up notification to {msg.sender_session_id}")

        asyncio.create_task(send_followup())

    # =========================================================================
    # Scheduled Reminders (sm remind / sm wake)
    # =========================================================================

    async def schedule_reminder(
        self,
        session_id: str,
        delay_seconds: int,
        message: str,
    ) -> str:
        """
        Schedule a self-reminder.

        Args:
            session_id: Session to receive the reminder
            delay_seconds: Seconds until reminder fires
            message: Reminder message

        Returns:
            Reminder ID
        """
        reminder_id = uuid.uuid4().hex[:12]
        fire_at = datetime.now() + timedelta(seconds=delay_seconds)

        # Persist to database
        self._execute("""
            INSERT INTO scheduled_reminders (id, target_session_id, message, fire_at, task_type)
            VALUES (?, ?, ?, ?, 'reminder')
        """, (reminder_id, session_id, message, fire_at.isoformat()))

        # Schedule async task
        task = asyncio.create_task(self._fire_reminder(reminder_id, session_id, message, delay_seconds))
        self._scheduled_tasks[reminder_id] = task

        logger.info(f"Scheduled reminder {reminder_id} for {session_id} in {delay_seconds}s")
        return reminder_id

    async def _fire_reminder(self, reminder_id: str, session_id: str, message: str, delay_seconds: int):
        """Fire a reminder after delay.

        If the session is mid-compaction when the reminder fires, waits up to
        COMPACTION_WAIT_MAX seconds (polling every COMPACTION_POLL_INTERVAL) before
        delivering anyway — preserves the one-shot guarantee (#249).
        """
        COMPACTION_WAIT_MAX = 300     # seconds
        COMPACTION_POLL_INTERVAL = 5  # seconds
        try:
            await asyncio.sleep(delay_seconds)

            # Wait for compaction to complete before delivering (#249)
            waited = 0
            while waited < COMPACTION_WAIT_MAX:
                session = self.session_manager.get_session(session_id)
                if not session or not session._is_compacting:
                    break
                await asyncio.sleep(COMPACTION_POLL_INTERVAL)
                waited += COMPACTION_POLL_INTERVAL

            # Queue the reminder with urgent delivery to actually wake the agent
            formatted_message = f"[sm] Scheduled reminder:\n{message}"
            self.queue_message(
                target_session_id=session_id,
                text=formatted_message,
                delivery_mode="urgent",
            )

            # Mark as fired in database
            self._execute(
                "UPDATE scheduled_reminders SET fired = 1 WHERE id = ?",
                (reminder_id,)
            )

            logger.info(f"Reminder {reminder_id} fired for {session_id}")

        except asyncio.CancelledError:
            logger.info(f"Reminder {reminder_id} cancelled")
        finally:
            self._scheduled_tasks.pop(reminder_id, None)

    async def _recover_scheduled_reminders(self):
        """Recover unfired reminders on startup."""
        rows = self._execute_query("""
            SELECT id, target_session_id, message, fire_at
            FROM scheduled_reminders
            WHERE fired = 0 AND fire_at > ?
        """, (datetime.now().isoformat(),))

        for row in rows:
            reminder_id, session_id, message, fire_at_str = row
            fire_at = datetime.fromisoformat(fire_at_str)
            delay = (fire_at - datetime.now()).total_seconds()
            if delay > 0:
                task = asyncio.create_task(
                    self._fire_reminder(reminder_id, session_id, message, delay)
                )
                self._scheduled_tasks[reminder_id] = task
                logger.info(f"Recovered reminder {reminder_id}, fires in {delay:.0f}s")

    # =========================================================================
    # Periodic Remind (#188)
    # =========================================================================

    def register_periodic_remind(
        self,
        target_session_id: str,
        soft_threshold: int,
        hard_threshold: int,
    ) -> str:
        """
        Register (or replace) a periodic remind for a target session.

        One-active-per-target: if a registration already exists for this session,
        it is cancelled and replaced.

        Args:
            target_session_id: Session to remind
            soft_threshold: Seconds after last reset before soft (important) remind fires
            hard_threshold: Seconds after last reset before hard (urgent) remind fires

        Returns:
            Registration ID
        """
        # Cancel any existing registration for this target
        self.cancel_remind(target_session_id)

        reg_id = uuid.uuid4().hex[:12]
        now = datetime.now()
        reg = RemindRegistration(
            id=reg_id,
            target_session_id=target_session_id,
            soft_threshold_seconds=soft_threshold,
            hard_threshold_seconds=hard_threshold,
            registered_at=now,
            last_reset_at=now,
        )
        self._remind_registrations[target_session_id] = reg

        # Persist to DB
        self._execute("""
            INSERT OR REPLACE INTO remind_registrations
            (id, target_session_id, soft_threshold_seconds, hard_threshold_seconds,
             registered_at, last_reset_at, soft_fired, is_active)
            VALUES (?, ?, ?, ?, ?, ?, 0, 1)
        """, (
            reg_id,
            target_session_id,
            soft_threshold,
            hard_threshold,
            now.isoformat(),
            now.isoformat(),
        ))

        # Start async task
        task = asyncio.create_task(self._run_remind_task(target_session_id))
        self._remind_tasks[target_session_id] = task

        logger.info(
            f"Periodic remind registered for {target_session_id} "
            f"(soft={soft_threshold}s, hard={hard_threshold}s, id={reg_id})"
        )
        return reg_id

    def reset_remind(self, target_session_id: str):
        """
        Reset the remind timer for a session (called when agent reports sm status).

        Updates last_reset_at and clears soft_fired so the cycle restarts.
        """
        reg = self._remind_registrations.get(target_session_id)
        if not reg or not reg.is_active:
            return

        now = datetime.now()
        reg.last_reset_at = now
        reg.soft_fired = False

        self._update_remind_db(target_session_id, last_reset_at=now, soft_fired=False)
        logger.info(f"Remind timer reset for {target_session_id}")

    def cancel_remind(self, target_session_id: str):
        """
        Cancel the periodic remind registration for a session.

        Called on: stop hook, sm clear, sm kill, sm remind --stop.
        """
        reg = self._remind_registrations.pop(target_session_id, None)
        if reg:
            reg.is_active = False
            self._execute(
                "UPDATE remind_registrations SET is_active = 0 WHERE target_session_id = ?",
                (target_session_id,)
            )
            logger.info(f"Periodic remind cancelled for {target_session_id}")

        # Cancel async task
        task = self._remind_tasks.pop(target_session_id, None)
        if task:
            task.cancel()

    def _update_remind_db(self, target_session_id: str, **kwargs):
        """Update remind registration fields in the DB."""
        if not kwargs:
            return
        parts = []
        values = []
        for key, value in kwargs.items():
            parts.append(f"{key} = ?")
            if isinstance(value, datetime):
                values.append(value.isoformat())
            elif isinstance(value, bool):
                values.append(1 if value else 0)
            else:
                values.append(value)
        values.append(target_session_id)
        self._execute(
            f"UPDATE remind_registrations SET {', '.join(parts)} WHERE target_session_id = ?",
            tuple(values)
        )

    async def _run_remind_task(self, target_session_id: str):
        """
        Async loop that fires soft/hard remind messages for a target session.

        Polls every 5 seconds. Fires soft (important) when soft_threshold exceeded,
        hard (urgent) when hard_threshold exceeded. Hard fire resets the cycle.
        Skips delivery while session is compacting (#249).
        """
        CHECK_INTERVAL = 5  # seconds
        REMIND_PREFIX = "[sm remind]"
        try:
            while True:
                await asyncio.sleep(CHECK_INTERVAL)
                reg = self._remind_registrations.get(target_session_id)
                if not reg or not reg.is_active:
                    return

                # Skip this iteration if the session is mid-compaction (#249)
                session = self.session_manager.get_session(target_session_id)
                if session and session._is_compacting:
                    continue

                elapsed = (datetime.now() - reg.last_reset_at).total_seconds()

                # Soft threshold: fire important remind
                if not reg.soft_fired and elapsed >= reg.soft_threshold_seconds:
                    # Dedup guard: skip if a remind is already pending
                    pending = self.get_pending_messages(target_session_id)
                    has_pending_remind = any(m.text.startswith(REMIND_PREFIX) for m in pending)
                    if not has_pending_remind:
                        self.queue_message(
                            target_session_id=target_session_id,
                            text='[sm remind] Update your status: sm status "your current progress"',
                            delivery_mode="important",
                        )
                    reg.soft_fired = True
                    self._update_remind_db(target_session_id, soft_fired=True)

                # Hard threshold: fire urgent remind and reset cycle
                if elapsed >= reg.hard_threshold_seconds:
                    self.queue_message(
                        target_session_id=target_session_id,
                        text='[sm remind] Status overdue. Run: sm status "your current progress"',
                        delivery_mode="urgent",
                    )
                    # Reset cycle so it restarts
                    now = datetime.now()
                    reg.last_reset_at = now
                    reg.soft_fired = False
                    self._update_remind_db(
                        target_session_id,
                        last_reset_at=now,
                        soft_fired=False,
                    )

        except asyncio.CancelledError:
            logger.info(f"Remind task cancelled for {target_session_id}")
        finally:
            self._remind_tasks.pop(target_session_id, None)

    async def _recover_remind_registrations(self):
        """Recover active remind registrations on server restart."""
        rows = self._execute_query("""
            SELECT id, target_session_id, soft_threshold_seconds, hard_threshold_seconds,
                   registered_at, last_reset_at, soft_fired
            FROM remind_registrations
            WHERE is_active = 1
        """)

        for row in rows:
            reg_id, target_session_id, soft, hard, registered_at_str, last_reset_at_str, soft_fired = row
            last_reset_at = datetime.fromisoformat(last_reset_at_str)

            reg = RemindRegistration(
                id=reg_id,
                target_session_id=target_session_id,
                soft_threshold_seconds=soft,
                hard_threshold_seconds=hard,
                registered_at=datetime.fromisoformat(registered_at_str),
                last_reset_at=last_reset_at,
                soft_fired=bool(soft_fired),
                is_active=True,
            )
            self._remind_registrations[target_session_id] = reg

            # Restart async task
            task = asyncio.create_task(self._run_remind_task(target_session_id))
            self._remind_tasks[target_session_id] = task

            elapsed = (datetime.now() - last_reset_at).total_seconds()
            logger.info(
                f"Recovered remind registration {reg_id} for {target_session_id}, "
                f"elapsed={elapsed:.0f}s (soft={soft}s, hard={hard}s)"
            )

    # =========================================================================
    # Parent Wake-Up Registration (#225-C)
    # =========================================================================

    _PARENT_WAKE_DEFAULT_PERIOD = 600   # 10 min
    _PARENT_WAKE_ESCALATED_PERIOD = 300  # 5 min after no-progress
    _PARENT_WAKE_CHECK_INTERVAL = 10    # seconds between loop ticks

    def register_parent_wake(
        self,
        child_session_id: str,
        parent_session_id: str,
        period_seconds: int = _PARENT_WAKE_DEFAULT_PERIOD,
    ) -> str:
        """Register (or replace) a parent wake-up registration for a dispatched child.

        One-active-per-child. If a registration already exists it is cancelled and replaced.

        Args:
            child_session_id: Child session that was dispatched
            parent_session_id: Parent (EM) session to receive periodic digests
            period_seconds: Seconds between wake-ups (default 600)

        Returns:
            Registration ID
        """
        self.cancel_parent_wake(child_session_id)

        reg_id = uuid.uuid4().hex[:12]
        now = datetime.now()
        reg = ParentWakeRegistration(
            id=reg_id,
            child_session_id=child_session_id,
            parent_session_id=parent_session_id,
            period_seconds=period_seconds,
            registered_at=now,
            last_wake_at=None,
            last_status_at_prev_wake=None,
            escalated=False,
            is_active=True,
        )
        self._parent_wake_registrations[child_session_id] = reg

        self._execute("""
            INSERT OR REPLACE INTO parent_wake_registrations
            (id, child_session_id, parent_session_id, period_seconds, registered_at,
             last_wake_at, last_status_at_prev_wake, escalated, is_active)
            VALUES (?, ?, ?, ?, ?, NULL, NULL, 0, 1)
        """, (reg_id, child_session_id, parent_session_id, period_seconds, now.isoformat()))

        task = asyncio.create_task(self._run_parent_wake_task(child_session_id))
        self._parent_wake_tasks[child_session_id] = task

        logger.info(
            f"Parent wake registered: child={child_session_id}, parent={parent_session_id}, "
            f"period={period_seconds}s, id={reg_id}"
        )
        return reg_id

    def cancel_parent_wake(self, child_session_id: str):
        """Cancel the parent wake registration for a child session.

        Called on: stop hook, sm clear, sm kill.
        """
        reg = self._parent_wake_registrations.pop(child_session_id, None)
        if reg:
            reg.is_active = False
            self._execute(
                "UPDATE parent_wake_registrations SET is_active = 0 WHERE child_session_id = ?",
                (child_session_id,)
            )
            logger.info(f"Parent wake cancelled for child={child_session_id}")

        task = self._parent_wake_tasks.pop(child_session_id, None)
        if task:
            task.cancel()

    def _update_parent_wake_db(self, child_session_id: str, **kwargs):
        """Update parent wake registration fields in the DB."""
        if not kwargs:
            return
        parts = []
        values = []
        for key, value in kwargs.items():
            parts.append(f"{key} = ?")
            if isinstance(value, datetime):
                values.append(value.isoformat())
            elif isinstance(value, bool):
                values.append(1 if value else 0)
            else:
                values.append(value)
        values.append(child_session_id)
        self._execute(
            f"UPDATE parent_wake_registrations SET {', '.join(parts)} WHERE child_session_id = ?",
            tuple(values)
        )

    async def _run_parent_wake_task(self, child_session_id: str):
        """Async loop that sends periodic digest messages to the parent EM."""
        try:
            while True:
                reg = self._parent_wake_registrations.get(child_session_id)
                if not reg or not reg.is_active:
                    return

                await asyncio.sleep(reg.period_seconds)

                # Re-check after sleep — may have been cancelled
                reg = self._parent_wake_registrations.get(child_session_id)
                if not reg or not reg.is_active:
                    return

                # Assemble and queue digest
                digest = await self._assemble_parent_wake_digest(child_session_id, reg)
                self.queue_message(
                    target_session_id=reg.parent_session_id,
                    text=digest,
                    delivery_mode="important",
                )
                logger.info(
                    f"Parent wake digest queued: child={child_session_id}, parent={reg.parent_session_id}"
                )

                # Escalation check: if child's status timestamp hasn't changed since last wake
                now = datetime.now()
                child_session = self.session_manager.get_session(child_session_id)
                current_status_at = getattr(child_session, "agent_status_at", None) if child_session else None

                if (
                    reg.last_wake_at is not None  # not the first wake
                    and not reg.escalated
                    and reg.last_status_at_prev_wake is not None
                    and current_status_at == reg.last_status_at_prev_wake
                ):
                    reg.escalated = True
                    reg.period_seconds = self._PARENT_WAKE_ESCALATED_PERIOD
                    self._update_parent_wake_db(
                        child_session_id,
                        escalated=True,
                        period_seconds=self._PARENT_WAKE_ESCALATED_PERIOD,
                    )
                    logger.info(
                        f"Parent wake escalated for child={child_session_id}: "
                        f"period reduced to {self._PARENT_WAKE_ESCALATED_PERIOD}s"
                    )

                # Update tracking fields
                reg.last_wake_at = now
                reg.last_status_at_prev_wake = current_status_at
                self._update_parent_wake_db(
                    child_session_id,
                    last_wake_at=now,
                    last_status_at_prev_wake=current_status_at,
                )

        except asyncio.CancelledError:
            logger.info(f"Parent wake task cancelled for child={child_session_id}")
        finally:
            self._parent_wake_tasks.pop(child_session_id, None)

    async def _assemble_parent_wake_digest(
        self, child_session_id: str, reg: "ParentWakeRegistration"
    ) -> str:
        """Build the parent wake digest message.

        Includes: duration, child status text with age, last 5 tool events,
        and a no-progress flag if child hasn't called sm status since last wake.
        """
        child_session = self.session_manager.get_session(child_session_id)

        child_name = "<unknown>"
        child_id_short = child_session_id[:8]
        status_text = None
        status_age_str = ""
        no_progress = False

        if child_session:
            child_name = child_session.friendly_name or child_session.name or child_session_id
            child_id_short = child_session_id[:8]

            # Status text and age
            if child_session.agent_status_text:
                status_text = child_session.agent_status_text
                if child_session.agent_status_at:
                    age_secs = (datetime.now() - child_session.agent_status_at).total_seconds()
                    status_age_str = f" ({int(age_secs / 60)}m ago)"

            # No-progress check: status_at hasn't changed since last wake
            if (
                reg.last_wake_at is not None
                and reg.last_status_at_prev_wake is not None
                and child_session.agent_status_at == reg.last_status_at_prev_wake
            ):
                no_progress = True

        # Duration
        elapsed_secs = (datetime.now() - reg.registered_at).total_seconds()
        elapsed_min = int(elapsed_secs / 60)

        # Header
        if no_progress:
            header = f"[sm dispatch] Child update: {child_name} ({child_id_short}) — NO PROGRESS DETECTED"
        else:
            header = f"[sm dispatch] Child update: {child_name} ({child_id_short})"

        lines = [header, f"Duration: {elapsed_min}m running"]

        if status_text:
            lines.append(f"Status: \"{status_text}\"{status_age_str}")
        else:
            lines.append("Status: (no status reported)")

        if no_progress:
            lines.append(
                f"Warning: No status update since last wake-up. Hard remind was sent at {reg.last_status_at_prev_wake}."
            )

        # Recent activity from tool_usage.db
        tool_events = await asyncio.to_thread(self._read_child_tail, child_session_id, 5)
        if tool_events:
            lines.append("")
            lines.append("Recent activity:")
            now = datetime.now()
            for event in tool_events:
                tool = event.get("tool_name", "?")
                ts_str = event.get("timestamp", "")
                try:
                    ts = datetime.fromisoformat(ts_str.replace(" ", "T")) if ts_str else None
                    age = f" ({int((now - ts).total_seconds() / 60)}m ago)" if ts else ""
                except Exception:
                    age = ""
                target = event.get("target_file") or event.get("bash_command") or ""
                if target:
                    target = target[:60]
                    lines.append(f"  {tool}: {target}{age}")
                else:
                    lines.append(f"  {tool}{age}")
        elif child_session:
            lines.append("")
            lines.append("Recent activity: (no tool events recorded)")

        return "\n".join(lines)

    def _read_child_tail(self, child_session_id: str, n: int = 5) -> list:
        """Read the last N PreToolUse events from tool_usage.db for a child session.

        Returns a list of dicts with keys: tool_name, target_file, bash_command, timestamp.
        Returns empty list if DB not available.
        """
        db_path = Path("~/.local/share/claude-sessions/tool_usage.db").expanduser()
        if not db_path.exists():
            return []

        try:
            conn = sqlite3.connect(str(db_path))
            cursor = conn.cursor()
            cursor.execute("""
                SELECT tool_name, target_file, bash_command, timestamp
                FROM tool_usage
                WHERE session_id = ? AND hook_type = 'PreToolUse'
                ORDER BY timestamp DESC
                LIMIT ?
            """, (child_session_id, n))
            rows = cursor.fetchall()
            conn.close()
            # Return in chronological order (oldest first)
            return [
                {"tool_name": r[0], "target_file": r[1], "bash_command": r[2], "timestamp": r[3]}
                for r in reversed(rows)
            ]
        except Exception as e:
            logger.warning(f"Could not read tool_usage.db for {child_session_id}: {e}")
            return []

    async def _recover_parent_wake_registrations(self):
        """Recover active parent wake registrations on server restart."""
        rows = self._execute_query("""
            SELECT id, child_session_id, parent_session_id, period_seconds,
                   registered_at, last_wake_at, last_status_at_prev_wake, escalated
            FROM parent_wake_registrations
            WHERE is_active = 1
        """)

        for row in rows:
            (reg_id, child_session_id, parent_session_id, period_seconds,
             registered_at_str, last_wake_at_str, last_status_at_str, escalated) = row

            reg = ParentWakeRegistration(
                id=reg_id,
                child_session_id=child_session_id,
                parent_session_id=parent_session_id,
                period_seconds=period_seconds,
                registered_at=datetime.fromisoformat(registered_at_str),
                last_wake_at=datetime.fromisoformat(last_wake_at_str) if last_wake_at_str else None,
                last_status_at_prev_wake=datetime.fromisoformat(last_status_at_str) if last_status_at_str else None,
                escalated=bool(escalated),
                is_active=True,
            )
            self._parent_wake_registrations[child_session_id] = reg

            task = asyncio.create_task(self._run_parent_wake_task(child_session_id))
            self._parent_wake_tasks[child_session_id] = task

            logger.info(
                f"Recovered parent wake registration {reg_id}: child={child_session_id}, "
                f"parent={parent_session_id}, period={period_seconds}s"
            )

    # =========================================================================
    # Session Watching (sm wait async notification)
    # =========================================================================

    async def watch_session(
        self,
        target_session_id: str,
        watcher_session_id: str,
        timeout_seconds: int,
    ) -> str:
        """
        Watch a session and notify the watcher when it goes idle or timeout.

        Args:
            target_session_id: Session to watch
            watcher_session_id: Session to notify when target is idle
            timeout_seconds: Maximum seconds to wait

        Returns:
            Watch ID
        """
        watch_id = uuid.uuid4().hex[:12]

        # Schedule async watch task
        task = asyncio.create_task(
            self._watch_for_idle(watch_id, target_session_id, watcher_session_id, timeout_seconds)
        )
        self._scheduled_tasks[watch_id] = task

        logger.info(f"Watching {target_session_id} for {timeout_seconds}s, will notify {watcher_session_id}")
        return watch_id

    async def _wait_for_claude_prompt_async(
        self, tmux_session: str, timeout: float = 3.0, poll_interval: float = 0.1
    ) -> bool:
        """Poll capture-pane until Claude Code shows bare '>' prompt, or timeout.

        Uses asyncio.create_subprocess_exec (non-blocking) to avoid violating
        the no-blocking-IO-in-async constraint (issue #37).
        Returns True if prompt detected, False if timed out (caller proceeds anyway).
        """
        # Minimum floor sleep so function works in test environments
        # where no real tmux pane exists (#175 spec requirement)
        await asyncio.sleep(0.1)
        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            try:
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "capture-pane", "-p", "-t", tmux_session,
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                stdout, _ = await asyncio.wait_for(
                    proc.communicate(), timeout=self.subprocess_timeout
                )
                if proc.returncode == 0:
                    output = stdout.decode().rstrip('\n')
                    if output:
                        last_line = output.split('\n')[-1]
                        if last_line.rstrip() == '>':
                            return True
            except Exception:
                pass
            await asyncio.sleep(poll_interval)
        return False

    async def _execute_handoff(self, session_id: str, file_path: str):
        """Execute a deferred handoff: clear context and send handoff prompt (#196).

        Caller (mark_session_idle) has already set is_idle=False. On ANY failure,
        this method MUST restore idle state to prevent permanent stall.

        Acquires the per-session delivery lock to prevent interleaving with
        queued message delivery from _try_deliver_messages.
        """
        from pathlib import Path

        def _restore_idle():
            """Restore idle state and trigger queued delivery on failure."""
            state = self._get_or_create_state(session_id)
            state.is_idle = True
            state.last_idle_at = datetime.now()
            logger.warning(f"Handoff failed for {session_id}, restoring idle state")
            asyncio.create_task(self._try_deliver_messages(session_id))

        session = self.session_manager.sessions.get(session_id)
        if not session:
            logger.error(f"Handoff: session {session_id} not found")
            _restore_idle()
            return

        # Verify file still exists
        if not Path(file_path).exists():
            logger.error(f"Handoff: file {file_path} no longer exists, aborting")
            _restore_idle()
            return

        tmux_session = session.tmux_session
        if not tmux_session:
            logger.error(f"Handoff: session {session_id} has no tmux session")
            _restore_idle()
            return

        logger.info(f"Executing handoff for {session_id}: {file_path}")

        # Acquire delivery lock to prevent _try_deliver_messages from interleaving
        lock = self._delivery_locks.setdefault(session_id, asyncio.Lock())
        async with lock:
            try:
                # 1. Arm skip fence for /clear Stop hook + clear stale notification state
                state = self._get_or_create_state(session_id)
                state.stop_notify_skip_count += 1
                state.skip_count_armed_at = datetime.now()  # sm#232
                state.stop_notify_sender_id = None
                state.stop_notify_sender_name = None
                state.last_outgoing_sm_send_target = None
                state.last_outgoing_sm_send_at = None
                # Also clear server-side caches: stale last_claude_output or
                # pending_stop_notifications can cause the new context's Stop hook
                # to be misinterpreted (#196).
                if hasattr(self.session_manager, '_app') and self.session_manager._app:
                    _app = self.session_manager._app
                    _app.state.last_claude_output.pop(session_id, None)
                    _app.state.pending_stop_notifications.discard(session_id)

                # 2. Send Escape to ensure idle
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "send-keys", "-t", tmux_session, "Escape",
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)

                # 3. Wait for > prompt
                await self._wait_for_claude_prompt_async(tmux_session)

                # 4. Send /clear (with settle delay before Enter)
                clear_command = "/new" if session.provider == "codex" else "/clear"
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "send-keys", "-t", tmux_session, "--", clear_command,
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)
                await asyncio.sleep(0.3)  # Settle delay: allow paste mode to end before Enter
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "send-keys", "-t", tmux_session, "Enter",
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)

                # 5. Wait for clear to complete — extended timeout (5.0s vs default 3.0s)
                # because /clear rewrites the full terminal display and may take
                # longer than a normal turn ending.
                await self._wait_for_claude_prompt_async(tmux_session, timeout=5.0)

                # 6. Send handoff prompt (with settle delay before Enter)
                handoff_prompt = f"Read {file_path} and continue from where you left off."
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "send-keys", "-t", tmux_session, "--", handoff_prompt,
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)
                await asyncio.sleep(0.3)  # Settle delay
                proc = await asyncio.create_subprocess_exec(
                    "tmux", "send-keys", "-t", tmux_session, "Enter",
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)

                # 7. Mark session as active (new context is now processing)
                self.mark_session_active(session_id)

                # 8. Persist handoff path for post-compaction recovery (#203)
                # and re-arm context monitor flags for the new cycle.
                session.last_handoff_path = file_path
                self.session_manager._save_state()
                session._context_warning_sent = False
                session._context_critical_sent = False

                logger.info(f"Handoff complete for {session_id}")

            except Exception as e:
                logger.error(f"Handoff execution failed for {session_id}: {e}")
                _restore_idle()

    async def _check_idle_prompt(self, tmux_session: str) -> bool:
        """Check if CLI is showing the input prompt (idle). Works for both Claude Code and Codex CLI."""
        try:
            proc = await asyncio.create_subprocess_exec(
                "tmux", "capture-pane", "-p", "-t", tmux_session,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=self.subprocess_timeout)
            if proc.returncode != 0:
                return False
            output = stdout.decode().rstrip()
            if not output:
                return False
            last_line = output.split('\n')[-1]
            # Prompt is ">" with optional trailing whitespace, no user text
            return last_line.rstrip() == '>' or last_line.startswith('> ') and not last_line[2:].strip()
        except Exception:
            return False

    async def _watch_for_idle(
        self,
        watch_id: str,
        target_session_id: str,
        watcher_session_id: str,
        timeout_seconds: int,
    ):
        """Watch a session and notify when it goes idle or timeout."""
        try:
            start_time = datetime.now()
            poll_interval = self.watch_poll_interval
            elapsed = 0
            prompt_count = 0          # Phase 2: consecutive tmux prompt detections
            pending_idle_count = 0    # Phase 4: consecutive prompt detections with stuck pending msgs

            while elapsed < timeout_seconds:
                # Cache session object for this iteration
                session = self.session_manager.get_session(target_session_id)

                # Guard: session disappeared mid-loop (killed/cleaned up)
                if not session:
                    logger.warning(f"Watch {watch_id}: session {target_session_id} no longer exists")
                    notification = (
                        f"[sm wait] {target_session_id} no longer exists (waited {int(elapsed)}s)"
                    )
                    self.queue_message(
                        target_session_id=watcher_session_id,
                        text=notification,
                        delivery_mode="important",
                    )
                    return

                # Phase 1: Check in-memory idle state
                state = self.delivery_states.get(target_session_id)
                mem_idle = state.is_idle if state else False

                # Phase 2: If NOT idle per memory, try tmux prompt fallback
                # Handles RCA #1 (hook failure). Extends existing Codex fallback to Claude.
                if not mem_idle:
                    if session.tmux_session:
                        provider = getattr(session, "provider", "claude")
                        if provider in ("codex", "claude"):
                            prompt_visible = await self._check_idle_prompt(
                                session.tmux_session
                            )
                            if prompt_visible:
                                prompt_count += 1
                                if prompt_count >= 2:
                                    mem_idle = True
                            else:
                                prompt_count = 0
                    # Reset pending_idle_count when not idle per memory/tmux
                    if not mem_idle:
                        pending_idle_count = 0

                # Phase 3: Session.status fallback (weak — only catches in-memory corruption)
                if not mem_idle:
                    if session.status == SessionStatus.IDLE:
                        mem_idle = True

                # Phase 4: Pending-message validation with tmux tiebreaker
                # ALL idle sources go through this — no skip flags.
                is_idle = mem_idle
                if is_idle and self.get_pending_messages(target_session_id):
                    # Pending messages exist. Use tmux prompt as tiebreaker to
                    # distinguish stuck (delivery failed) from in-flight.
                    # Handles RCA #2 (is_idle=True + stuck pending messages).
                    if session.tmux_session:
                        prompt_visible = await self._check_idle_prompt(
                            session.tmux_session
                        )
                        if prompt_visible:
                            pending_idle_count += 1
                            if pending_idle_count >= 2:
                                pass       # 2 consecutive: truly idle, msgs stuck
                            else:
                                is_idle = False  # Need 2 consecutive to confirm
                        else:
                            pending_idle_count = 0
                            is_idle = False      # Not at prompt, delivery in-flight
                    else:
                        is_idle = False          # Can't verify, assume in-flight

                if is_idle:
                    # Log exact state at idle-fire time for root cause confirmation (#193)
                    _dbg_state = self.delivery_states.get(target_session_id)
                    logger.info(
                        f"Watch {watch_id}: idle detected at {elapsed:.1f}s — "
                        f"state.is_idle={_dbg_state.is_idle if _dbg_state else None}, "
                        f"session.status={session.status}, "
                        f"pending={len(self.get_pending_messages(target_session_id))}, "
                        f"prompt_count={prompt_count}, "
                        f"pending_idle_count={pending_idle_count}"
                    )
                    # Suppress if stop notification was already sent to this watcher <10s ago (#216)
                    stop_key = (target_session_id, watcher_session_id)
                    stop_at = self._recent_stop_notifications.get(stop_key)
                    if stop_at and (datetime.now() - stop_at).total_seconds() < self._STOP_SUPPRESS_WINDOW_SECONDS:
                        logger.info(
                            f"Watch {watch_id}: suppressing idle — stop notification already sent "
                            f"to {watcher_session_id} {(datetime.now() - stop_at).total_seconds():.1f}s ago (#216)"
                        )
                        self._recent_stop_notifications.pop(stop_key, None)
                        return
                    elif stop_at:
                        # Window expired — pop stale entry before proceeding
                        self._recent_stop_notifications.pop(stop_key, None)

                    # Target is idle - notify watcher
                    target_session = self.session_manager.get_session(target_session_id)
                    target_name = "unknown"
                    if target_session:
                        target_name = target_session.friendly_name or target_session.name or target_session_id

                    notification = (
                        f"[sm wait] {target_name} is now idle (waited {int(elapsed)}s)"
                    )
                    self.queue_message(
                        target_session_id=watcher_session_id,
                        text=notification,
                        delivery_mode="important",
                    )
                    logger.info(f"Watch {watch_id}: {target_session_id} idle after {elapsed:.0f}s")

                    # Mirror to Telegram (fire-and-forget)
                    watcher_session = self.session_manager.get_session(watcher_session_id)
                    if watcher_session:
                        mirror_text = f"💤 {notification}"
                        await self._mirror_to_telegram(mirror_text, watcher_session, "idle_notify")

                    return

                # Wait and check again
                await asyncio.sleep(poll_interval)
                elapsed = (datetime.now() - start_time).total_seconds()

            # Timeout reached - notify watcher
            target_session = self.session_manager.get_session(target_session_id)
            target_name = "unknown"
            if target_session:
                target_name = target_session.friendly_name or target_session.name or target_session_id

            notification = (
                f"[sm wait] Timeout: {target_name} still active after {timeout_seconds}s"
            )
            self.queue_message(
                target_session_id=watcher_session_id,
                text=notification,
                delivery_mode="important",
            )
            logger.info(f"Watch {watch_id}: {target_session_id} timeout after {timeout_seconds}s")

            # Mirror to Telegram (fire-and-forget)
            watcher_session = self.session_manager.get_session(watcher_session_id)
            if watcher_session:
                mirror_text = f"💤 {notification}"
                await self._mirror_to_telegram(mirror_text, watcher_session, "timeout_notify")

        except asyncio.CancelledError:
            logger.info(f"Watch {watch_id} cancelled")
        finally:
            self._scheduled_tasks.pop(watch_id, None)

    # =========================================================================
    # API Helpers
    # =========================================================================

    def get_queue_status(self, session_id: str) -> dict:
        """Get queue status for a session (for API)."""
        state = self.delivery_states.get(session_id, SessionDeliveryState(session_id=session_id))
        messages = self.get_pending_messages(session_id)

        return {
            "session_id": session_id,
            "is_idle": state.is_idle,
            "pending_count": len(messages),
            "pending_messages": [
                {
                    "id": m.id,
                    "sender": m.sender_name or m.sender_session_id,
                    "queued_at": m.queued_at.isoformat(),
                    "timeout_at": m.timeout_at.isoformat() if m.timeout_at else None,
                    "delivery_mode": m.delivery_mode,
                }
                for m in messages
            ],
            "saved_user_input": state.saved_user_input,
        }
