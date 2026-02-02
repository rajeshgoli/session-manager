"""Message queue manager for reliable inter-agent messaging (sm-send-v2)."""

import asyncio
import logging
import shlex
import sqlite3
import subprocess
from datetime import datetime, timedelta
from pathlib import Path
from typing import Optional, Dict, List, Callable, Awaitable

from .models import QueuedMessage, SessionDeliveryState

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

    def __init__(
        self,
        session_manager,
        db_path: str = "~/.local/share/claude-sessions/message_queue.db",
        config: Optional[dict] = None,
    ):
        """
        Initialize message queue manager.

        Args:
            session_manager: SessionManager instance
            db_path: Path to SQLite database
            config: Optional config dict with sm_send settings
        """
        self.session_manager = session_manager
        self.db_path = Path(db_path).expanduser()
        self.db_path.parent.mkdir(parents=True, exist_ok=True)

        # Configuration
        config = config or {}
        self.input_poll_interval = config.get("input_poll_interval", 5)  # seconds
        self.input_stale_timeout = config.get("input_stale_timeout", 120)  # seconds
        self.max_batch_size = config.get("max_batch_size", 10)
        self.urgent_delay_ms = config.get("urgent_delay_ms", 500)

        # In-memory state (not persisted - rebuilt from hooks)
        self.delivery_states: Dict[str, SessionDeliveryState] = {}

        # Background task
        self._running = False
        self._monitor_task: Optional[asyncio.Task] = None
        self._scheduled_tasks: Dict[str, asyncio.Task] = {}  # reminder_id -> task

        # Notification callback (set by main app)
        self._notify_callback: Optional[Callable] = None

        # Initialize database
        self._init_db()

    def _init_db(self):
        """Initialize SQLite database schema."""
        conn = sqlite3.connect(str(self.db_path))
        try:
            cursor = conn.cursor()
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
                    delivered_at TIMESTAMP
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
            conn.commit()
        finally:
            conn.close()
        logger.info(f"Message queue database initialized at {self.db_path}")

    def set_notify_callback(self, callback: Callable):
        """Set callback for delivery notifications."""
        self._notify_callback = callback

    async def start(self):
        """Start the queue monitoring service."""
        self._running = True
        self._monitor_task = asyncio.create_task(self._monitor_loop())
        # Recover pending reminders from database
        await self._recover_scheduled_reminders()
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
        logger.info("Message queue manager stopped")

    # =========================================================================
    # IDLE State Management (called by Stop hook handler)
    # =========================================================================

    def mark_session_idle(self, session_id: str):
        """
        Mark a session as idle (called when Stop hook fires).

        This triggers delivery check for any queued messages.
        """
        state = self._get_or_create_state(session_id)
        state.is_idle = True
        state.last_idle_at = datetime.now()
        logger.info(f"Session {session_id} marked idle")

        # Trigger async delivery check
        asyncio.create_task(self._try_deliver_messages(session_id))

    def mark_session_active(self, session_id: str):
        """Mark a session as active (not idle)."""
        state = self._get_or_create_state(session_id)
        state.is_idle = False
        logger.debug(f"Session {session_id} marked active")

    def is_session_idle(self, session_id: str) -> bool:
        """Check if a session is idle."""
        state = self.delivery_states.get(session_id)
        return state.is_idle if state else False

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
        )

        # Persist to database
        conn = sqlite3.connect(str(self.db_path))
        try:
            cursor = conn.cursor()
            cursor.execute("""
                INSERT INTO message_queue
                (id, target_session_id, sender_session_id, sender_name, text,
                 delivery_mode, queued_at, timeout_at, notify_on_delivery, notify_after_seconds)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
            ))
            conn.commit()
        finally:
            conn.close()

        queue_len = self.get_queue_length(target_session_id)
        logger.info(f"Queued message {msg.id} for {target_session_id} (mode={delivery_mode}, queue={queue_len})")

        # If urgent mode, trigger immediate delivery
        if delivery_mode == "urgent":
            asyncio.create_task(self._deliver_urgent(target_session_id, msg))
        # If important mode, trigger check (delivers when response complete)
        elif delivery_mode == "important":
            asyncio.create_task(self._try_deliver_messages(target_session_id, important_only=True))
        # For sequential mode, check if session is already idle and deliver immediately
        elif delivery_mode == "sequential":
            state = self.delivery_states.get(target_session_id)
            # Check in-memory idle state first
            if state and state.is_idle:
                logger.info(f"Session {target_session_id} already idle (in-memory), triggering immediate delivery")
                asyncio.create_task(self._try_deliver_messages(target_session_id))
            else:
                # Check actual session status - sessions with ERROR or IDLE status should receive messages
                session = self.session_manager.get_session(target_session_id)
                if session:
                    from .models import SessionStatus
                    if session.status in (SessionStatus.ERROR, SessionStatus.IDLE):
                        logger.info(f"Session {target_session_id} has status={session.status.value}, marking idle for delivery")
                        self.mark_session_idle(target_session_id)

        return msg

    def get_pending_messages(self, session_id: str) -> List[QueuedMessage]:
        """Get all pending (undelivered) messages for a session."""
        conn = sqlite3.connect(str(self.db_path))
        try:
            cursor = conn.cursor()
            cursor.execute("""
                SELECT id, target_session_id, sender_session_id, sender_name, text,
                       delivery_mode, queued_at, timeout_at, notify_on_delivery,
                       notify_after_seconds, delivered_at
                FROM message_queue
                WHERE target_session_id = ? AND delivered_at IS NULL
                ORDER BY queued_at ASC
            """, (session_id,))

            messages = []
            for row in cursor.fetchall():
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
                    delivered_at=datetime.fromisoformat(row[10]) if row[10] else None,
                )
                # Skip expired messages
                if msg.timeout_at and datetime.now() > msg.timeout_at:
                    self._mark_expired(msg.id)
                    continue
                messages.append(msg)
            return messages
        finally:
            conn.close()

    def get_queue_length(self, session_id: str) -> int:
        """Get the number of pending messages for a session."""
        return len(self.get_pending_messages(session_id))

    def _mark_delivered(self, message_id: str):
        """Mark a message as delivered in the database."""
        conn = sqlite3.connect(str(self.db_path))
        try:
            cursor = conn.cursor()
            cursor.execute("""
                UPDATE message_queue SET delivered_at = ? WHERE id = ?
            """, (datetime.now().isoformat(), message_id))
            conn.commit()
        finally:
            conn.close()

    def _mark_expired(self, message_id: str):
        """Mark a message as expired (delete it)."""
        conn = sqlite3.connect(str(self.db_path))
        try:
            cursor = conn.cursor()
            cursor.execute("DELETE FROM message_queue WHERE id = ?", (message_id,))
            conn.commit()
        finally:
            conn.close()
        logger.info(f"Message {message_id} expired and deleted")

    def _cleanup_messages_for_session(self, session_id: str):
        """Clean up all pending messages for a session that no longer exists."""
        conn = sqlite3.connect(str(self.db_path))
        try:
            cursor = conn.cursor()
            # First get the count for logging
            cursor.execute(
                "SELECT COUNT(*) FROM message_queue WHERE target_session_id = ? AND delivered_at IS NULL",
                (session_id,)
            )
            count = cursor.fetchone()[0]

            # Delete all pending messages for this session
            cursor.execute(
                "DELETE FROM message_queue WHERE target_session_id = ? AND delivered_at IS NULL",
                (session_id,)
            )
            conn.commit()
            logger.info(f"Cleaned up {count} pending message(s) for non-existent session {session_id}")
        finally:
            conn.close()

    # =========================================================================
    # User Input Detection and Management
    # =========================================================================

    def _get_pending_user_input(self, tmux_session: str) -> Optional[str]:
        """
        Check if user has typed something at the prompt.

        Returns the user's typed text if present, None otherwise.
        """
        try:
            result = subprocess.run(
                ["tmux", "capture-pane", "-p", "-t", tmux_session],
                capture_output=True,
                text=True,
                timeout=2,
            )
            if result.returncode != 0:
                return None

            output = result.stdout.strip()
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

    def _clear_user_input(self, tmux_session: str) -> bool:
        """Clear the current input line using Ctrl+U."""
        try:
            subprocess.run(
                ["tmux", "send-keys", "-t", tmux_session, "C-u"],
                check=True,
                timeout=2,
            )
            return True
        except Exception as e:
            logger.error(f"Error clearing user input: {e}")
            return False

    def _restore_user_input(self, tmux_session: str, text: str):
        """Restore previously saved user input (without pressing Enter)."""
        try:
            # Use list-based subprocess with "--" to handle text starting with "-"
            subprocess.run(
                ["tmux", "send-keys", "-t", tmux_session, "--", text],
                check=True,
                timeout=5,
            )
            logger.info(f"Restored user input: {text[:50]}...")
        except Exception as e:
            logger.error(f"Error restoring user input: {e}")

    # =========================================================================
    # Message Delivery
    # =========================================================================

    async def _monitor_loop(self):
        """Main monitoring loop - checks for stale user input."""
        try:
            while self._running:
                # Check each session with pending messages
                sessions_with_pending = self._get_sessions_with_pending()

                for session_id in sessions_with_pending:
                    await self._check_stale_input(session_id)

                await asyncio.sleep(self.input_poll_interval)
        except asyncio.CancelledError:
            logger.info("Monitor loop cancelled")
        except Exception as e:
            logger.error(f"Error in monitor loop: {e}")

    def _get_sessions_with_pending(self) -> List[str]:
        """Get list of session IDs with pending messages."""
        conn = sqlite3.connect(str(self.db_path))
        try:
            cursor = conn.cursor()
            cursor.execute("""
                SELECT DISTINCT target_session_id
                FROM message_queue
                WHERE delivered_at IS NULL
            """)
            return [row[0] for row in cursor.fetchall()]
        finally:
            conn.close()

    async def _check_stale_input(self, session_id: str):
        """Check if user input has become stale and trigger delivery."""
        state = self._get_or_create_state(session_id)

        # Only check if session is idle but has pending user input
        if not state.is_idle:
            return

        session = self.session_manager.get_session(session_id)
        if not session:
            return

        current_input = self._get_pending_user_input(session.tmux_session)

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
                        self._clear_user_input(session.tmux_session)
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

        Args:
            session_id: Target session ID
            important_only: Only deliver important mode messages
        """
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
        else:
            # For sequential, only deliver if session is idle
            if not state.is_idle:
                logger.debug(f"Session {session_id} not idle, skipping sequential delivery")
                return

        # Check for user input (final gate)
        current_input = self._get_pending_user_input(session.tmux_session)
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

        # Inject the message (use async version to avoid blocking event loop)
        logger.info(f"Delivering {len(batch)} message(s) to {session_id}")
        success = await self.session_manager.tmux.send_input_async(session.tmux_session, payload)

        if success:
            # Mark session as active
            state.is_idle = False

            # Mark messages as delivered
            for msg in batch:
                self._mark_delivered(msg.id)
                logger.info(f"Delivered message {msg.id}")

                # Handle delivery notifications
                if msg.notify_on_delivery and msg.sender_session_id:
                    await self._send_delivery_notification(msg)

                if msg.notify_after_seconds and msg.sender_session_id:
                    await self._schedule_followup_notification(msg)

            # Update session activity
            session.last_activity = datetime.now()
            from .models import SessionStatus
            session.status = SessionStatus.RUNNING
            self.session_manager._save_state()
        else:
            logger.error(f"Failed to deliver messages to {session_id}")

    async def _deliver_urgent(self, session_id: str, msg: QueuedMessage):
        """Deliver an urgent message immediately, interrupting Claude."""
        session = self.session_manager.get_session(session_id)
        if not session:
            logger.error(f"Session {session_id} not found for urgent delivery")
            return

        try:
            # Send Escape to interrupt any streaming
            subprocess.run(
                ["tmux", "send-keys", "-t", session.tmux_session, "Escape"],
                check=True,
                timeout=2,
            )

            # Brief delay for interrupt to process
            await asyncio.sleep(self.urgent_delay_ms / 1000)

            # Inject message directly (use async version to avoid blocking)
            success = await self.session_manager.tmux.send_input_async(session.tmux_session, msg.text)

            if success:
                self._mark_delivered(msg.id)
                state = self._get_or_create_state(session_id)
                state.is_idle = False
                logger.info(f"Urgent message {msg.id} delivered to {session_id}")

                # Handle notifications
                if msg.notify_on_delivery and msg.sender_session_id:
                    await self._send_delivery_notification(msg)
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

        # Restore the saved input
        self._restore_user_input(session.tmux_session, state.saved_user_input)

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

    async def _schedule_followup_notification(self, msg: QueuedMessage):
        """Schedule a follow-up notification after delivery."""
        if not msg.notify_after_seconds or not msg.sender_session_id:
            return

        async def send_followup():
            await asyncio.sleep(msg.notify_after_seconds)
            # Only notify if recipient is idle
            if self.is_session_idle(msg.target_session_id):
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
        import uuid
        reminder_id = uuid.uuid4().hex[:12]
        fire_at = datetime.now() + timedelta(seconds=delay_seconds)

        # Persist to database
        conn = sqlite3.connect(str(self.db_path))
        try:
            cursor = conn.cursor()
            cursor.execute("""
                INSERT INTO scheduled_reminders (id, target_session_id, message, fire_at, task_type)
                VALUES (?, ?, ?, ?, 'reminder')
            """, (reminder_id, session_id, message, fire_at.isoformat()))
            conn.commit()
        finally:
            conn.close()

        # Schedule async task
        task = asyncio.create_task(self._fire_reminder(reminder_id, session_id, message, delay_seconds))
        self._scheduled_tasks[reminder_id] = task

        logger.info(f"Scheduled reminder {reminder_id} for {session_id} in {delay_seconds}s")
        return reminder_id

    async def _fire_reminder(self, reminder_id: str, session_id: str, message: str, delay_seconds: int):
        """Fire a reminder after delay."""
        try:
            await asyncio.sleep(delay_seconds)

            # Queue the reminder with urgent delivery to actually wake the agent
            formatted_message = f"[sm] Scheduled reminder:\n{message}"
            self.queue_message(
                target_session_id=session_id,
                text=formatted_message,
                delivery_mode="urgent",
            )

            # Mark as fired in database
            conn = sqlite3.connect(str(self.db_path))
            try:
                cursor = conn.cursor()
                cursor.execute(
                    "UPDATE scheduled_reminders SET fired = 1 WHERE id = ?",
                    (reminder_id,)
                )
                conn.commit()
            finally:
                conn.close()

            logger.info(f"Reminder {reminder_id} fired for {session_id}")

        except asyncio.CancelledError:
            logger.info(f"Reminder {reminder_id} cancelled")
        finally:
            self._scheduled_tasks.pop(reminder_id, None)

    async def _recover_scheduled_reminders(self):
        """Recover unfired reminders on startup."""
        conn = sqlite3.connect(str(self.db_path))
        try:
            cursor = conn.cursor()
            cursor.execute("""
                SELECT id, target_session_id, message, fire_at
                FROM scheduled_reminders
                WHERE fired = 0 AND fire_at > ?
            """, (datetime.now().isoformat(),))

            for row in cursor.fetchall():
                reminder_id, session_id, message, fire_at_str = row
                fire_at = datetime.fromisoformat(fire_at_str)
                delay = (fire_at - datetime.now()).total_seconds()
                if delay > 0:
                    task = asyncio.create_task(
                        self._fire_reminder(reminder_id, session_id, message, delay)
                    )
                    self._scheduled_tasks[reminder_id] = task
                    logger.info(f"Recovered reminder {reminder_id}, fires in {delay:.0f}s")
        finally:
            conn.close()

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
