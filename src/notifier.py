"""Routes notifications to appropriate channels (Telegram/Email)."""

import asyncio
import logging
import re
from typing import Optional

from .models import Session, NotificationEvent, NotificationChannel
from .telegram_bot import TelegramBot, escape_markdown_v2, create_permission_keyboard
from .email_handler import EmailHandler

logger = logging.getLogger(__name__)

# Regex to match ANSI escape codes (comprehensive)
ANSI_ESCAPE_RE = re.compile(
    r'\x1b\[[0-9;?]*[a-zA-Z]|'  # CSI sequences (including private modes like ?2026h)
    r'\x1b\][^\x07]*\x07|'       # OSC sequences (title, etc.)
    r'\x1b[PX^_].*?\x1b\\|'      # DCS, SOS, PM, APC sequences
    r'\x1b[\(\)][AB012]|'        # Character set selection
    r'\x1b[=>]|'                 # Keypad modes
    r'\x1b[78]|'                 # Save/restore cursor
    r'\x1b[DMEHc]|'              # Various single-char commands
    r'\x1b\[[\d;]*[Hf]|'         # Cursor positioning
    r'[\x00-\x08\x0b\x0c\x0e-\x1f]'  # Other control characters
)


def strip_ansi(text: str) -> str:
    """Remove ANSI escape codes and control characters from text."""
    # First pass: regex
    text = ANSI_ESCAPE_RE.sub('', text)
    # Second pass: remove any remaining escape sequences we might have missed
    text = re.sub(r'\x1b[^a-zA-Z]*[a-zA-Z]', '', text)
    # Clean up multiple blank lines
    text = re.sub(r'\n{3,}', '\n\n', text)
    return text


class Notifier:
    """Routes notifications to Telegram and/or Email."""

    def __init__(
        self,
        telegram_bot: Optional[TelegramBot] = None,
        email_handler: Optional[EmailHandler] = None,
        default_channel: NotificationChannel = NotificationChannel.TELEGRAM,
    ):
        self.telegram = telegram_bot
        self.email = email_handler
        self.default_channel = default_channel
        # Track last response message ID per session (for idle replies)
        self._last_response_msg: dict[str, tuple[int, int]] = {}  # session_id -> (chat_id, msg_id)

    async def notify(
        self,
        event: NotificationEvent,
        session: Optional[Session] = None,
    ) -> bool:
        """
        Send a notification for an event.

        Args:
            event: The notification event
            session: Optional session for context (chat IDs, etc.)

        Returns:
            True if notification sent successfully
        """
        # Determine channel
        channel = event.channel or self.default_channel

        # Format the message
        message = self._format_message(event, session)

        success = False

        if channel == NotificationChannel.TELEGRAM:
            # Use markdown formatting for response events
            use_markdown = event.event_type == "response"
            success = await self._notify_telegram(event, session, message, use_markdown)
        elif channel == NotificationChannel.EMAIL:
            success = await self._notify_email(event, message)

        # If urgent and Telegram notification succeeded, also consider email
        if event.urgent and success and channel == NotificationChannel.TELEGRAM:
            # For urgent events, we might want to also send email
            # (Claude can request this explicitly via the API)
            pass

        return success

    async def _notify_telegram(
        self,
        event: NotificationEvent,
        session: Optional[Session],
        message: str,
        use_markdown: bool = False,
    ) -> bool:
        """Send notification via Telegram."""
        if not self.telegram:
            logger.warning("Telegram not configured")
            return False

        chat_id = None
        reply_to = None
        topic_id = None

        # Get chat ID and thread info from session
        if session:
            chat_id = session.telegram_chat_id
            topic_id = session.telegram_thread_id  # Can be forum topic or reply thread
            reply_to = session.telegram_thread_id  # Same field, used for both
        else:
            # Try to get from session thread registry
            thread_info = self.telegram.get_session_thread(event.session_id)
            if thread_info:
                chat_id, reply_to = thread_info

        if not chat_id:
            logger.warning(f"No Telegram chat ID for session {event.session_id}")
            return False

        # For idle notifications in non-topic mode, reply to the last response
        if event.event_type == "idle" and not topic_id:
            last_response = self._last_response_msg.get(event.session_id)
            if last_response:
                chat_id, reply_to = last_response

        # Use MarkdownV2 for response events (Claude's output is markdown)
        parse_mode = "MarkdownV2" if use_markdown else None

        # Create inline keyboard for permission prompts
        reply_markup = None
        if event.event_type == "permission_prompt":
            reply_markup = create_permission_keyboard(event.session_id)

        # In topic mode, don't use reply_to (just post to the topic)
        msg_id = await self.telegram.send_notification(
            chat_id=chat_id,
            message=message,
            reply_to_message_id=reply_to if not topic_id else None,
            message_thread_id=topic_id,
            parse_mode=parse_mode,
            reply_markup=reply_markup,
        )

        # Store message ID for response notifications (so idle can reply to it)
        if msg_id and event.event_type == "response":
            self._last_response_msg[event.session_id] = (chat_id, msg_id)
            # Delete the "Input sent" message now that response arrived
            await self.telegram.delete_pending_input_msg(event.session_id)

        return msg_id is not None

    async def _notify_email(self, event: NotificationEvent, message: str) -> bool:
        """Send notification via Email."""
        if not self.email:
            logger.warning("Email not configured")
            return False

        if not self.email.is_available():
            logger.warning("Email harness not available")
            return False

        return await self.email.send_notification(
            session_id=event.session_id,
            message=message,
            urgent=event.urgent,
        )

    def _format_message(self, event: NotificationEvent, session: Optional[Session] = None) -> str:
        """Format notification event as message text."""
        lines = []

        # Build session label: "friendly-name [id]" or just "[id]"
        session_id = event.session_id
        friendly_name = session.friendly_name if session else None

        # For response events, use markdown formatting
        if event.event_type == "response":
            # Escape for markdown
            session_id_escaped = session_id.replace('-', '\\-').replace('.', '\\.')
            if friendly_name:
                name_escaped = friendly_name.replace('-', '\\-').replace('.', '\\.')
                lines.append(f"{name_escaped} \\[{session_id_escaped}\\] *Claude:*")
            else:
                lines.append(f"\\[{session_id_escaped}\\] *Claude:*")

            if event.context:
                # Strip ANSI codes
                context = strip_ansi(event.context)

                # Convert markdown headings (##, ###, etc) to bold since MarkdownV2 doesn't support headings
                context = re.sub(r'^#{1,6}\s+(.+)$', r'*\1*', context, flags=re.MULTILINE)

                # Escape for MarkdownV2
                context = escape_markdown_v2(context)
                lines.append(context)
        else:
            # Non-response events: plain text formatting (no escaping needed)
            if event.event_type == "idle":
                # Simple idle message - just notify that Claude is idle
                lines.append("Claude is idle.")
                lines.append("Waiting for your input.")
            else:
                lines.append(f"[{event.event_type.upper()}] {event.message}")
                lines.append(f"Session: {event.session_id}")

                if event.context:
                    context = strip_ansi(event.context)
                    lines.append("")
                    lines.append("Recent output:")
                    lines.append("---")
                    lines.append(context)
                    lines.append("---")

                if event.event_type == "permission_prompt":
                    lines.append("")
                    lines.append("Reply with: y/n/yes/no or custom input")

        return "\n".join(lines)

    async def rename_session_topic(
        self,
        session: Session,
        new_name: str,
    ) -> bool:
        """
        Rename the Telegram topic for a session.

        Args:
            session: Session with telegram_chat_id and telegram_thread_id
            new_name: New friendly name for the topic

        Returns:
            True if renamed successfully
        """
        if not self.telegram:
            return False

        if not session.telegram_chat_id or not session.telegram_thread_id:
            return False

        # Format topic name same way as when created
        topic_name = f"{new_name} [{session.id}]"

        return await self.telegram.rename_forum_topic(
            chat_id=session.telegram_chat_id,
            topic_id=session.telegram_thread_id,
            name=topic_name,
        )

    async def request_email_notification(
        self,
        session_id: str,
        message: str,
        urgent: bool = False,
    ) -> bool:
        """
        Send an email notification on request (e.g., from Claude).

        This is used when Claude explicitly requests email notification
        via the /notify API endpoint.

        Args:
            session_id: Session ID
            message: Message to send
            urgent: Whether to also send SMS

        Returns:
            True if sent successfully
        """
        if not self.email or not self.email.is_available():
            logger.warning("Email not available for requested notification")
            return False

        return await self.email.send_notification(
            session_id=session_id,
            message=message,
            urgent=urgent,
        )
