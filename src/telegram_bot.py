"""Telegram bot for controlling Claude sessions."""

import asyncio
import logging
from typing import Optional, Callable, Awaitable
import httpx

from telegram import Update, Bot, InlineKeyboardButton, InlineKeyboardMarkup
from telegram.ext import (
    Application,
    CommandHandler,
    MessageHandler,
    CallbackQueryHandler,
    ContextTypes,
    filters,
)

from .models import Session, UserInput, NotificationChannel, DeliveryResult

logger = logging.getLogger(__name__)


def create_permission_keyboard(session_id: str) -> InlineKeyboardMarkup:
    """Create inline keyboard for permission prompts.

    Claude Code permission prompts use numbered options:
    1. Yes, allow once
    2. Yes, always allow for this session
    3. No
    4. Don't ask again for this session
    """
    keyboard = [
        [
            InlineKeyboardButton("✓ Yes (once)", callback_data=f"perm:{session_id}:1"),
            InlineKeyboardButton("✓ Always", callback_data=f"perm:{session_id}:2"),
        ],
        [
            InlineKeyboardButton("✗ No", callback_data=f"perm:{session_id}:3"),
            InlineKeyboardButton("✗ Never", callback_data=f"perm:{session_id}:4"),
        ],
    ]
    return InlineKeyboardMarkup(keyboard)


def escape_markdown_v2(text: str) -> str:
    """Escape special characters for Telegram MarkdownV2, preserving formatting."""
    # Characters that need escaping in MarkdownV2 (outside of code blocks)
    # We'll escape conservatively to avoid breaking formatting
    escape_chars = r'\_[]()~`>#+-=|{}.!'

    result = []
    i = 0
    while i < len(text):
        char = text[i]

        # Handle code blocks (```) - don't escape inside
        if text[i:i+3] == '```':
            end = text.find('```', i + 3)
            if end != -1:
                result.append(text[i:end+3])
                i = end + 3
                continue

        # Handle inline code (`) - don't escape inside
        if char == '`':
            end = text.find('`', i + 1)
            if end != -1:
                result.append(text[i:end+1])
                i = end + 1
                continue

        # Handle bold (**text**)
        if text[i:i+2] == '**':
            end = text.find('**', i + 2)
            if end != -1:
                inner = escape_markdown_v2(text[i+2:end])
                result.append(f'*{inner}*')  # Telegram uses single * for bold
                i = end + 2
                continue

        # Handle links [text](url)
        if char == '[':
            close_bracket = text.find(']', i)
            if close_bracket != -1 and text[close_bracket:close_bracket+2] == '](':
                close_paren = text.find(')', close_bracket + 2)
                if close_paren != -1:
                    link_text = text[i+1:close_bracket]
                    url = text[close_bracket+2:close_paren]
                    # Escape special chars in link text but not the brackets/parens
                    escaped_text = escape_markdown_v2(link_text)
                    result.append(f'[{escaped_text}]({url})')
                    i = close_paren + 1
                    continue

        # Escape special characters
        if char in escape_chars:
            result.append('\\' + char)
        else:
            result.append(char)
        i += 1

    return ''.join(result)


class TelegramBot:
    """Telegram bot for Claude session management."""

    def __init__(
        self,
        token: str,
        allowed_chat_ids: Optional[list[int]] = None,
        allowed_user_ids: Optional[list[int]] = None,
        office_automate_url: Optional[str] = None,
    ):
        """
        Initialize the Telegram bot.

        Args:
            token: Telegram bot token from BotFather
            allowed_chat_ids: List of chat IDs allowed to use the bot (None = allow all)
            allowed_user_ids: List of user IDs allowed to use the bot (None = allow all)
            office_automate_url: URL to office-automate service for utilities like /password
        """
        self.token = token
        self.allowed_chat_ids = set(allowed_chat_ids) if allowed_chat_ids else None
        self.allowed_user_ids = set(allowed_user_ids) if allowed_user_ids else None
        self.office_automate_url = office_automate_url or "http://192.168.5.140:8080"
        self.application: Optional[Application] = None
        self.bot: Optional[Bot] = None

        # Callbacks for session operations
        self._on_new_session: Optional[Callable[[int, str], Awaitable[Optional[Session]]]] = None
        self._on_list_sessions: Optional[Callable[[], Awaitable[list[Session]]]] = None
        self._on_kill_session: Optional[Callable[[str], Awaitable[bool]]] = None
        self._on_session_input: Optional[Callable[[UserInput], Awaitable[DeliveryResult]]] = None
        self._on_session_status: Optional[Callable[[str], Awaitable[Optional[Session]]]] = None
        self._on_open_terminal: Optional[Callable[[str], Awaitable[bool]]] = None
        self._on_update_thread: Optional[Callable[[str, int, int], Awaitable[None]]] = None
        self._on_set_name: Optional[Callable[[str, str], Awaitable[bool]]] = None
        self._on_get_last_output: Optional[Callable[[str], Awaitable[Optional[str]]]] = None
        self._on_get_last_message: Optional[Callable[[str], Awaitable[Optional[str]]]] = None
        self._on_get_tmux_output: Optional[Callable[[str, int], Awaitable[Optional[str]]]] = None
        self._on_interrupt_session: Optional[Callable[[str], Awaitable[bool]]] = None
        self._on_update_topic: Optional[Callable[[str, int, int], Awaitable[None]]] = None
        self._on_get_subagents: Optional[Callable[[str], Awaitable[Optional[list]]]] = None

        # Track message threads for sessions
        self._session_threads: dict[str, tuple[int, int]] = {}  # session_id -> (chat_id, message_id)
        # Track topic -> session mapping (for forum groups)
        self._topic_sessions: dict[tuple[int, int], str] = {}  # (chat_id, topic_id) -> session_id
        # Track "Input sent" messages to delete when response arrives
        self._pending_input_msgs: dict[str, tuple[int, int]] = {}  # session_id -> (chat_id, msg_id)
        # Track sessions that have completed (for progress monitoring)
        self._completed_sessions: set[str] = set()

    def load_session_threads(self, sessions: list[Session]):
        """Load thread and topic mappings from existing sessions (call on startup)."""
        for session in sessions:
            if session.telegram_chat_id:
                # Load thread/topic mapping if available
                if session.telegram_thread_id:
                    # Store in both mappings for backward compatibility
                    self._topic_sessions[(session.telegram_chat_id, session.telegram_thread_id)] = session.id
                    self._session_threads[session.id] = (
                        session.telegram_chat_id,
                        session.telegram_thread_id,
                    )
                    logger.info(f"Restored thread mapping for session {session.id}")

    def set_new_session_handler(self, handler: Callable[[int, str], Awaitable[Optional[Session]]]):
        """Set handler for creating new sessions. Handler receives (chat_id, working_dir)."""
        self._on_new_session = handler

    def set_list_sessions_handler(self, handler: Callable[[], Awaitable[list[Session]]]):
        """Set handler for listing sessions."""
        self._on_list_sessions = handler

    def set_kill_session_handler(self, handler: Callable[[str], Awaitable[bool]]):
        """Set handler for killing sessions. Handler receives session_id."""
        self._on_kill_session = handler

    def set_session_input_handler(self, handler: Callable[[UserInput], Awaitable[bool]]):
        """Set handler for session input. Handler receives UserInput."""
        self._on_session_input = handler

    def set_session_status_handler(self, handler: Callable[[str], Awaitable[Optional[Session]]]):
        """Set handler for session status. Handler receives session_id."""
        self._on_session_status = handler

    def set_open_terminal_handler(self, handler: Callable[[str], Awaitable[bool]]):
        """Set handler for opening terminal. Handler receives session_id."""
        self._on_open_terminal = handler

    def set_update_thread_handler(self, handler: Callable[[str, int, int], Awaitable[None]]):
        """Set handler for updating telegram thread. Handler receives (session_id, chat_id, message_id)."""
        self._on_update_thread = handler

    def set_name_handler(self, handler: Callable[[str, str], Awaitable[bool]]):
        """Set handler for setting session name. Handler receives (session_id, name)."""
        self._on_set_name = handler

    def set_get_last_output_handler(self, handler: Callable[[str], Awaitable[Optional[str]]]):
        """Set handler for getting last Claude output. Handler receives session_id."""
        self._on_get_last_output = handler

    def set_get_last_message_handler(self, handler: Callable[[str], Awaitable[Optional[str]]]):
        """Set handler for getting last Claude message. Handler receives session_id."""
        self._on_get_last_message = handler

    def set_get_tmux_output_handler(self, handler: Callable[[str, int], Awaitable[Optional[str]]]):
        """Set handler for getting tmux output. Handler receives session_id and line count."""
        self._on_get_tmux_output = handler

    def set_interrupt_handler(self, handler: Callable[[str], Awaitable[bool]]):
        """Set handler for interrupting a session. Handler receives session_id."""
        self._on_interrupt_session = handler

    def set_update_topic_handler(self, handler: Callable[[str, int, int], Awaitable[None]]):
        """Set handler for updating session topic. Handler receives (session_id, chat_id, topic_id)."""
        self._on_update_topic = handler

    def set_get_subagents_handler(self, handler: Callable[[str], Awaitable[Optional[list]]]):
        """Set handler for getting subagents. Handler receives session_id."""
        self._on_get_subagents = handler

    def _format_subagents(self, subagents: list) -> str:
        """Format subagents list for display."""
        from datetime import datetime

        if not subagents:
            return ""

        lines = ["\nSubagents:"]
        for sa in subagents:
            # Calculate elapsed time
            started_at = datetime.fromisoformat(sa["started_at"])
            elapsed_seconds = (datetime.now() - started_at).total_seconds()

            if elapsed_seconds < 60:
                elapsed_str = f"{int(elapsed_seconds)}s ago"
            elif elapsed_seconds < 3600:
                elapsed_str = f"{int(elapsed_seconds // 60)}m ago"
            else:
                elapsed_str = f"{int(elapsed_seconds // 3600)}h ago"

            # Status icon
            status = sa["status"]
            if status == "completed":
                icon = "✓"
            elif status == "error":
                icon = "✗"
            else:
                icon = "→"

            # Format agent line
            agent_id = sa["agent_id"][:6] if len(sa["agent_id"]) > 6 else sa["agent_id"]
            lines.append(f"  {icon} {sa['agent_type']} ({agent_id}) | {status} | {elapsed_str}")

            # Add summary if available
            if sa.get("summary"):
                lines.append(f"     {sa['summary']}")

        return "\n".join(lines)

    def _is_allowed(self, chat_id: int, user_id: Optional[int] = None) -> bool:
        """Check if a chat/user is allowed to use the bot."""
        # Check user allowlist first (if configured)
        if self.allowed_user_ids is not None:
            if user_id is None or user_id not in self.allowed_user_ids:
                return False

        # Check chat allowlist (if configured)
        if self.allowed_chat_ids is not None:
            if chat_id not in self.allowed_chat_ids:
                return False

        return True

    async def _cmd_start(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /start command."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            logger.warning(f"Unauthorized: chat_id={update.effective_chat.id}, user_id={update.effective_user.id}")
            await update.message.reply_text("Unauthorized.")
            return

        await update.message.reply_text(
            "Claude Session Manager Bot\n\n"
            "Commands:\n"
            "/new [path] - Create new session (defaults to fractal-market-simulator)\n"
            "/session - Pick a project and create a session\n"
            "/follow [session] - Create forum topic for existing session (shows buttons if no args)\n"
            "/list - List active sessions\n"
            "/status - What is Claude doing? (reply to session)\n"
            "/subagents - List spawned subagents (reply to session)\n"
            "/message - Get last Claude message (reply to session)\n"
            "/summary - AI summary of session activity (reply to session)\n"
            "/stop - Interrupt Claude (reply to session)\n"
            "/kill [id] - Kill a session\n"
            "/open [id] - Open session in Terminal.app\n"
            "/name <name> - Set friendly name (reply to session)\n"
            "/password - Get LocalTunnel password\n"
            "/help - Show this message\n\n"
            "Reply to a session thread to send input."
        )

    async def _cmd_help(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /help command."""
        await self._cmd_start(update, context)

    async def _cmd_new(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /new command to create a new session."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not self._on_new_session:
            await update.message.reply_text("Session creation not configured.")
            return

        # If no args, default to fractal-market-simulator
        if not context.args:
            working_dir = "~/Desktop/fractal-market-simulator"
        else:
            # Get working directory from args
            working_dir = " ".join(context.args)

        chat_id = update.effective_chat.id
        is_forum = update.effective_chat.is_forum

        await update.message.reply_text(f"Creating session in {working_dir}...")

        try:
            session = await self._on_new_session(chat_id, working_dir)

            if session:
                topic_id = None

                # In forum groups, create a dedicated topic for this session
                if is_forum:
                    topic_name = f"{session.friendly_name or 'session'} [{session.id}]"
                    topic_id = await self.create_forum_topic(chat_id, topic_name)

                    if topic_id:
                        # Register topic -> session mapping
                        self.register_topic_session(chat_id, topic_id, session.id)

                        # Send welcome message in the new topic
                        msg = await self.bot.send_message(
                            chat_id=chat_id,
                            message_thread_id=topic_id,
                            text=f"Session created: {session.name}\n"
                                 f"ID: {session.id}\n"
                                 f"Directory: {session.working_dir}\n\n"
                                 "Send messages here to interact with Claude."
                        )

                        # Update session with topic info
                        if self._on_update_topic:
                            await self._on_update_topic(session.id, chat_id, topic_id)

                        await update.message.reply_text(f"Created topic for session [{session.id}]")
                    else:
                        await update.message.reply_text("Failed to create topic. Using reply mode.")
                        is_forum = False

                # Non-forum mode: use reply chains
                if not is_forum or not topic_id:
                    msg = await update.message.reply_text(
                        f"Session created: {session.name}\n"
                        f"ID: {session.id}\n"
                        f"Directory: {session.working_dir}\n"
                        f"Status: {session.status.value}\n\n"
                        "Reply to this message to send input to Claude."
                    )
                    # Track this message as the thread root
                    self._session_threads[session.id] = (chat_id, msg.message_id)

                    if self._on_update_thread:
                        await self._on_update_thread(session.id, chat_id, msg.message_id)

            else:
                await update.message.reply_text("Failed to create session.")

        except Exception as e:
            logger.error(f"Error creating session: {e}")
            await update.message.reply_text(f"Error: {e}")

    async def _cmd_list(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /list command."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not self._on_list_sessions:
            await update.message.reply_text("Session listing not configured.")
            return

        try:
            sessions = await self._on_list_sessions()

            if not sessions:
                await update.message.reply_text("No active sessions.")
                return

            lines = ["Active Sessions:\n"]
            for s in sessions:
                lines.append(f"- {s.name} ({s.id}): {s.status.value}")

            await update.message.reply_text("\n".join(lines))

        except Exception as e:
            logger.error(f"Error listing sessions: {e}")
            await update.message.reply_text(f"Error: {e}")

    async def _cmd_status(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /status command - shows what Claude is doing in a session."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not self._on_session_status:
            await update.message.reply_text("Status check not configured.")
            return

        from datetime import datetime

        # Find session from topic or reply context
        session_id = self._get_session_from_context(update)

        if not session_id:
            await update.message.reply_text(
                "Could not identify session. Use /status in a session topic or reply to a session message."
            )
            return

        try:
            session = await self._on_session_status(session_id)

            if session:
                # Calculate idle time
                idle_seconds = (datetime.now() - session.last_activity).total_seconds()
                if idle_seconds < 60:
                    idle_str = f"{int(idle_seconds)}s"
                else:
                    idle_str = f"{int(idle_seconds // 60)}m"

                # Determine if working or idle
                # Use the session status directly - it's more reliable than guessing from idle time
                if session.status.value == "running":
                    status_emoji = "Working"
                elif session.status.value in ("waiting_input", "waiting_permission"):
                    status_emoji = session.status.value.replace("_", " ").title()
                elif session.status.value == "idle":
                    status_emoji = "Idle"
                else:
                    status_emoji = session.status.value.title()

                name_str = f"{session.friendly_name} " if session.friendly_name else ""

                lines = [
                    f"{name_str}[{session.id}]",
                    f"Status: {status_emoji} (idle {idle_str})",
                    f"Dir: {session.working_dir}",
                ]

                # Get last output if available
                if self._on_get_last_output:
                    last_output = await self._on_get_last_output(session_id)
                    if last_output:
                        # Truncate for display
                        if len(last_output) > 300:
                            last_output = last_output[:300] + "..."
                        lines.append(f"\nLast output:\n{last_output}")

                # Get subagents if available
                if self._on_get_subagents:
                    subagents = await self._on_get_subagents(session_id)
                    if subagents:
                        subagent_info = self._format_subagents(subagents)
                        if subagent_info:
                            lines.append(subagent_info)

                await update.message.reply_text("\n".join(lines))
            else:
                await update.message.reply_text(f"Session not found: {session_id}")

        except Exception as e:
            logger.error(f"Error getting status: {e}")
            await update.message.reply_text(f"Error: {e}")

    async def _cmd_subagents(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /subagents command - lists spawned subagents."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not self._on_get_subagents:
            await update.message.reply_text("Subagent tracking not configured.")
            return

        # Find session from topic or reply context
        session_id = self._get_session_from_context(update)

        if not session_id:
            await update.message.reply_text(
                "Could not identify session. Use /subagents in a session topic or reply to a session message."
            )
            return

        try:
            # Get session info for name
            session = None
            if self._on_session_status:
                session = await self._on_session_status(session_id)

            # Get subagents
            subagents = await self._on_get_subagents(session_id)

            if not subagents:
                name_str = f"{session.friendly_name} " if session and session.friendly_name else ""
                await update.message.reply_text(f"{name_str}[{session_id}] has no subagents")
                return

            # Format and send
            name_str = f"{session.friendly_name} " if session and session.friendly_name else ""
            header = f"{name_str}[{session_id}] subagents:"
            subagent_info = self._format_subagents(subagents)

            # Remove the "Subagents:" header from format_subagents output since we have our own
            subagent_lines = subagent_info.split('\n')[1:]  # Skip first line
            message = header + "\n" + "\n".join(subagent_lines)

            await update.message.reply_text(message)

        except Exception as e:
            logger.error(f"Error getting subagents: {e}")
            await update.message.reply_text(f"Error: {e}")

    async def _cmd_message(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /message command - retrieves the last Claude message."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not self._on_get_last_message:
            await update.message.reply_text("Last message retrieval not configured.")
            return

        # Find session from topic or reply context
        session_id = self._get_session_from_context(update)

        if not session_id:
            await update.message.reply_text(
                "Could not identify session. Use /message in a session topic or reply to a session message."
            )
            return

        try:
            last_message = await self._on_get_last_message(session_id)

            if last_message:
                # Send the full message with markdown formatting
                session_id_escaped = session_id.replace('-', '\\-').replace('.', '\\.')
                header = f"\\[{session_id_escaped}\\] *Last Claude message:*\n\n"
                full_message = header + escape_markdown_v2(last_message)

                await update.message.reply_text(full_message, parse_mode="MarkdownV2")
            else:
                await update.message.reply_text(f"No message found for session {session_id}")

        except Exception as e:
            logger.error(f"Error getting last message: {e}")
            await update.message.reply_text(f"Error: {e}")

    async def _cmd_summary(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /summary command - AI-generated summary of what session is doing."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not self._on_get_tmux_output:
            await update.message.reply_text("Summary not configured.")
            return

        # Find session from topic or reply context
        session_id = self._get_session_from_context(update)

        if not session_id:
            await update.message.reply_text(
                "Could not identify session. Use /summary in a session topic or reply to a session message."
            )
            return

        try:
            # Get tmux output (last 100 lines)
            tmux_output = await self._on_get_tmux_output(session_id, 100)

            if not tmux_output:
                await update.message.reply_text(f"No output available for session {session_id}")
                return

            # Strip ANSI codes
            from .notifier import strip_ansi
            clean_output = strip_ansi(tmux_output)

            # Send to claude haiku for summary
            import subprocess
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
                raise subprocess.TimeoutExpired(cmd='/opt/homebrew/bin/claude', timeout=60)

            logger.info(f"Summary generated, return code: {returncode}, output length: {len(result_stdout)}")

            if returncode != 0:
                logger.error(f"Claude command failed: {result_stderr}")
                await update.message.reply_text(f"Error generating summary: {result_stderr[:200]}")
                return

            summary = result_stdout.strip()

            if not summary:
                await update.message.reply_text("Summary was empty")
                return

            # Send summary
            await update.message.reply_text(f"📋 *Summary:*\n{summary}", parse_mode="Markdown")

        except subprocess.TimeoutExpired as e:
            logger.error(f"Summary generation timed out for session {session_id} after 60s")
            await update.message.reply_text("Summary generation timed out (60s) - try /status instead")
        except Exception as e:
            logger.error(f"Error generating summary for session {session_id}: {e}", exc_info=True)
            await update.message.reply_text(f"Error: {e}")

    async def _cmd_kill(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /kill command."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not self._on_kill_session:
            await update.message.reply_text("Session killing not configured.")
            return

        if not context.args:
            await update.message.reply_text("Usage: /kill <session_id>")
            return

        session_id = context.args[0]

        try:
            success = await self._on_kill_session(session_id)

            if success:
                await update.message.reply_text(f"Session {session_id} killed.")
            else:
                await update.message.reply_text(f"Failed to kill session {session_id}.")

        except Exception as e:
            logger.error(f"Error killing session: {e}")
            await update.message.reply_text(f"Error: {e}")

    async def _cmd_stop(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /stop command to interrupt Claude."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not self._on_interrupt_session:
            await update.message.reply_text("Interrupt not configured.")
            return

        # Find session from topic or reply context
        session_id = self._get_session_from_context(update)

        if not session_id:
            await update.message.reply_text(
                "Could not identify session. Use /stop in a session topic or reply to a session message."
            )
            return

        try:
            success = await self._on_interrupt_session(session_id)

            if success:
                await update.message.reply_text(f"[{session_id}] Interrupted.")
            else:
                await update.message.reply_text(f"[{session_id}] Failed to interrupt.")

        except Exception as e:
            logger.error(f"Error interrupting session: {e}")
            await update.message.reply_text(f"Error: {e}")

    async def _cmd_force(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /force command to interrupt Claude and deliver message immediately."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not self._on_session_input:
            await update.message.reply_text("Input handler not configured.")
            return

        # Find session from topic or reply context
        session_id = self._get_session_from_context(update)

        if not session_id:
            await update.message.reply_text(
                "Could not identify session. Use /force in a session topic or reply to a session message."
            )
            return

        # Get the message text (everything after /force)
        if not context.args:
            await update.message.reply_text(
                "Usage: /force <message>\n"
                "Interrupts Claude and delivers your message immediately."
            )
            return

        text = " ".join(context.args)
        chat_id = update.effective_chat.id

        try:
            # Create UserInput with urgent delivery mode
            user_input = UserInput(
                session_id=session_id,
                text=text,
                source=NotificationChannel.TELEGRAM,
                chat_id=chat_id,
                message_id=update.message.message_id,
                delivery_mode="urgent",  # Sends Escape first, then delivers
            )

            result = await self._on_session_input(user_input)

            if result == DeliveryResult.DELIVERED:
                msg = await update.message.reply_text(f"[{session_id}] ⚡ Interrupted & delivered")
                # Track this message for deletion when response arrives
                self._pending_input_msgs[session_id] = (chat_id, msg.message_id)
            elif result == DeliveryResult.QUEUED:
                # Shouldn't happen with urgent mode, but handle it
                await update.message.reply_text(f"[{session_id}] ⏳ Queued (unexpected)")
            else:
                await update.message.reply_text(f"[{session_id}] ❌ Failed to force-deliver")

        except Exception as e:
            logger.error(f"Error force-delivering message: {e}")
            await update.message.reply_text(f"Error: {e}")

    async def _cmd_open(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /open command to open session in Terminal."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not self._on_open_terminal:
            await update.message.reply_text("Terminal opening not configured.")
            return

        if not context.args:
            await update.message.reply_text("Usage: /open <session_id>")
            return

        session_id = context.args[0]

        try:
            success = await self._on_open_terminal(session_id)

            if success:
                await update.message.reply_text(f"Opened Terminal for session {session_id}.")
            else:
                await update.message.reply_text(f"Failed to open Terminal for session {session_id}.")

        except Exception as e:
            logger.error(f"Error opening terminal: {e}")
            await update.message.reply_text(f"Error: {e}")

    async def _cmd_name(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /name command to set a friendly name for a session."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not self._on_set_name:
            await update.message.reply_text("Name setting not configured.")
            return

        if not context.args:
            await update.message.reply_text("Usage: /name <friendly-name>")
            return

        name = "-".join(context.args).lower()

        # Find session from topic or reply context
        session_id = self._get_session_from_context(update)

        if not session_id:
            await update.message.reply_text(
                "Could not identify session. Use /name in a session topic or reply to a session message."
            )
            return

        try:
            success = await self._on_set_name(session_id, name)

            if success:
                # Also rename the forum topic if we're in one
                topic_id = update.message.message_thread_id
                if topic_id:
                    chat_id = update.effective_chat.id
                    await self.rename_forum_topic(chat_id, topic_id, name)
                await update.message.reply_text(f"Session renamed: {name}")
            else:
                await update.message.reply_text(f"Failed to rename session {session_id}")

        except Exception as e:
            logger.error(f"Error setting name: {e}")
            await update.message.reply_text(f"Error: {e}")

    async def _handle_message(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle regular messages (replies to session threads or in forum topics)."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            return

        if not self._on_session_input:
            return

        chat_id = update.effective_chat.id

        # Find the session from topic or reply context
        session_id = self._get_session_from_context(update)

        if not session_id:
            logger.warning(f"Could not identify session for message in chat {chat_id}, topic {update.message.message_thread_id}")
            # In forum mode, only respond in session topics
            if update.effective_chat.is_forum:
                # Show error in forum topics (except General topic which is usually None or 1)
                if update.message.message_thread_id and update.message.message_thread_id != 1:
                    await update.message.reply_text(
                        "Could not identify session for this topic. The session may have been created before the server started."
                    )
                return  # Silently ignore in General topic
            await update.message.reply_text(
                "Could not identify session. Reply to a message containing [session_id]."
            )
            return

        # Send input to the session
        user_input = UserInput(
            session_id=session_id,
            text=update.message.text,
            source=NotificationChannel.TELEGRAM,
            chat_id=chat_id,
            message_id=update.message.message_id,
        )

        try:
            result = await self._on_session_input(user_input)

            if result == DeliveryResult.DELIVERED:
                msg = await update.message.reply_text(f"[{session_id}] ✓ Delivered")
                # Track this message so we can delete it when response arrives
                self._pending_input_msgs[session_id] = (chat_id, msg.message_id)
                # Start progress monitoring for delivered messages
                asyncio.create_task(self._monitor_progress(session_id, chat_id, msg.message_id))
            elif result == DeliveryResult.QUEUED:
                msg = await update.message.reply_text(
                    f"[{session_id}] ⏳ Queued (session working)\n"
                    f"Reply /force to deliver immediately"
                )
                # Track queued message for potential /force promotion
                self._pending_input_msgs[session_id] = (chat_id, msg.message_id)
            else:
                await update.message.reply_text(f"[{session_id}] ❌ Failed to send input")

        except Exception as e:
            logger.error(f"Error sending input: {e}")
            await update.message.reply_text(f"[{session_id}] Error: {e}")

    async def send_notification(
        self,
        chat_id: int,
        message: str,
        reply_to_message_id: Optional[int] = None,
        message_thread_id: Optional[int] = None,
        parse_mode: Optional[str] = None,
        reply_markup: Optional[InlineKeyboardMarkup] = None,
    ) -> Optional[int]:
        """
        Send a notification message.

        Args:
            chat_id: Chat to send to
            message: Message text
            reply_to_message_id: Optional message to reply to (for threading)
            message_thread_id: Optional forum topic ID
            parse_mode: Optional parse mode ("MarkdownV2", "HTML", or None for plain text)
            reply_markup: Optional inline keyboard markup for buttons

        Returns:
            Message ID of sent message, or None on failure
        """
        if not self.bot:
            logger.error("Bot not initialized")
            return None

        try:
            msg = await self.bot.send_message(
                chat_id=chat_id,
                text=message,
                reply_to_message_id=reply_to_message_id,
                message_thread_id=message_thread_id,
                parse_mode=parse_mode,
                reply_markup=reply_markup,
            )
            return msg.message_id

        except Exception as e:
            # If markdown parsing fails, retry without parse_mode
            if parse_mode:
                logger.warning(f"Markdown parsing failed, retrying as plain text: {e}")
                try:
                    # Strip markdown escape chars for plain text fallback
                    plain_message = message.replace('\\', '')
                    msg = await self.bot.send_message(
                        chat_id=chat_id,
                        text=plain_message,
                        reply_to_message_id=reply_to_message_id,
                        message_thread_id=message_thread_id,
                        reply_markup=reply_markup,
                    )
                    return msg.message_id
                except Exception as e2:
                    logger.error(f"Failed to send plain text message: {e2}")
                    return None
            logger.error(f"Failed to send Telegram message: {e}")
            return None

    def register_session_thread(self, session_id: str, chat_id: int, message_id: int):
        """Register a session's root message for threading."""
        self._session_threads[session_id] = (chat_id, message_id)

    def get_session_thread(self, session_id: str) -> Optional[tuple[int, int]]:
        """Get the (chat_id, message_id) for a session's thread."""
        return self._session_threads.get(session_id)

    async def delete_pending_input_msg(self, session_id: str):
        """Delete the 'Input sent' message for a session (called when response arrives)."""
        # Mark session as completed to stop progress monitoring
        self._completed_sessions.add(session_id)

        pending = self._pending_input_msgs.pop(session_id, None)
        if pending and self.bot:
            chat_id, msg_id = pending
            try:
                await self.bot.delete_message(chat_id=chat_id, message_id=msg_id)
            except Exception as e:
                logger.debug(f"Could not delete input msg: {e}")

    async def _monitor_progress(self, session_id: str, chat_id: int, msg_id: int):
        """
        Update message with Claude's progress every 5 seconds.

        Runs until the Stop hook fires (session added to _completed_sessions)
        or a timeout is reached (60 seconds).
        """
        # Clear any previous completion flag for this session
        self._completed_sessions.discard(session_id)

        last_content = ""
        max_iterations = 12  # 60 seconds total (5s * 12)

        for _ in range(max_iterations):
            await asyncio.sleep(5)

            # Check if Stop hook fired (response complete)
            if session_id in self._completed_sessions:
                self._completed_sessions.discard(session_id)
                return

            # Check if message was removed from tracking (e.g., replaced by response)
            if session_id not in self._pending_input_msgs:
                return

            # Get current tmux output
            if not self._on_get_tmux_output:
                return

            try:
                output = await self._on_get_tmux_output(session_id, 20)
                if not output or output == last_content:
                    continue

                last_content = output

                # Strip ANSI codes and truncate for display
                from .notifier import strip_ansi
                clean_output = strip_ansi(output)

                # Truncate if too long
                if len(clean_output) > 400:
                    clean_output = "..." + clean_output[-400:]

                # Update the message with progress
                if self.bot:
                    await self.bot.edit_message_text(
                        chat_id=chat_id,
                        message_id=msg_id,
                        text=f"[{session_id}] ⏳ Working...\n```\n{clean_output}\n```",
                        parse_mode="Markdown",
                    )
            except Exception as e:
                # Message might be deleted or rate limited
                logger.debug(f"Could not update progress message: {e}")

    async def create_forum_topic(self, chat_id: int, name: str) -> Optional[int]:
        """Create a forum topic and return its ID."""
        if not self.bot:
            return None
        try:
            topic = await self.bot.create_forum_topic(chat_id=chat_id, name=name)
            return topic.message_thread_id
        except Exception as e:
            logger.error(f"Failed to create forum topic: {e}")
            return None

    async def rename_forum_topic(self, chat_id: int, topic_id: int, name: str) -> bool:
        """Rename a forum topic."""
        if not self.bot:
            return False
        try:
            await self.bot.edit_forum_topic(chat_id=chat_id, message_thread_id=topic_id, name=name)
            return True
        except Exception as e:
            logger.error(f"Failed to rename forum topic: {e}")
            return False

    def register_topic_session(self, chat_id: int, topic_id: int, session_id: str):
        """Register a topic -> session mapping."""
        self._topic_sessions[(chat_id, topic_id)] = session_id

    def get_session_from_topic(self, chat_id: int, topic_id: int) -> Optional[str]:
        """Get session ID from a topic."""
        return self._topic_sessions.get((chat_id, topic_id))

    def is_forum_topic(self, update) -> bool:
        """Check if the message is in a forum topic."""
        return update.message.message_thread_id is not None

    def _get_session_from_context(self, update) -> Optional[str]:
        """Get session ID from topic or reply context."""
        import re

        chat_id = update.effective_chat.id
        topic_id = update.message.message_thread_id

        logger.debug(f"Getting session from context: chat_id={chat_id}, topic_id={topic_id}")

        # First, check if we're in a forum topic
        if topic_id:
            session_id = self.get_session_from_topic(chat_id, topic_id)
            logger.debug(f"Topic lookup: (chat_id={chat_id}, topic_id={topic_id}) -> session_id={session_id}")
            if session_id:
                return session_id

        # Second, check if this is a reply to a session message
        reply_to = update.message.reply_to_message
        if reply_to:
            reply_text = reply_to.text or ""
            match = re.search(r'\[([a-f0-9]{8})\]|ID:\s*([a-f0-9]{8})', reply_text)
            if match:
                session_id = match.group(1) or match.group(2)
                # Check if session exists in either thread registry or topic registry
                if session_id in self._session_threads:
                    return session_id
                # Also check topic registry
                for (cid, tid), sid in self._topic_sessions.items():
                    if sid == session_id and cid == chat_id:
                        return session_id

        # Third, if only one session in this chat, use it
        # Check both thread sessions and topic sessions
        chat_sessions = []

        # Add sessions from reply threads
        chat_sessions.extend([
            sid for sid, (cid, mid) in self._session_threads.items()
            if cid == chat_id
        ])

        # Add sessions from forum topics
        chat_sessions.extend([
            sid for (cid, tid), sid in self._topic_sessions.items()
            if cid == chat_id
        ])

        # Remove duplicates and return if exactly one
        chat_sessions = list(set(chat_sessions))
        logger.debug(f"Fallback: found {len(chat_sessions)} sessions in chat {chat_id}: {chat_sessions}")
        if len(chat_sessions) == 1:
            return chat_sessions[0]

        logger.debug(f"Could not identify session from context")
        logger.debug(f"Topic sessions: {self._topic_sessions}")
        logger.debug(f"Thread sessions: {self._session_threads}")
        return None

    async def _cmd_session(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /session command to pick a project and create a session."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not self._on_new_session:
            await update.message.reply_text("Session creation not configured.")
            return

        # Show project selection buttons
        projects = [
            ("fractal-market-simulator", "~/Desktop/fractal-market-simulator"),
            ("automation (session-mgr)", "~/Desktop/automation/claude-session-manager"),
            ("office-automate", "~/Desktop/automation/office-automate"),
            ("financial-analysis", "~/Desktop/automation/financial-analysis"),
        ]

        keyboard = [
            [InlineKeyboardButton(name, callback_data=f"new_project:{path}")]
            for name, path in projects
        ]
        keyboard.append([InlineKeyboardButton("Custom path", callback_data="new_project:custom")])

        await update.message.reply_text(
            "Select a project:",
            reply_markup=InlineKeyboardMarkup(keyboard)
        )

    async def _cmd_password(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /password command to get LocalTunnel password."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        try:
            async with httpx.AsyncClient() as client:
                url = f"{self.office_automate_url}/localtunnel/password"
                response = await client.get(url, timeout=5)
                response.raise_for_status()

                data = response.json()
                password = data.get("password") or str(data)

                await update.message.reply_text(f"🔐 LocalTunnel Password:\n`{password}`", parse_mode="MarkdownV2")
        except Exception as e:
            logger.error(f"Error fetching password: {e}")
            await update.message.reply_text(f"Failed to fetch password: {e}")

    async def _cmd_follow(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle /follow command to create a forum topic for an existing session."""
        if not self._is_allowed(update.effective_chat.id, update.effective_user.id):
            await update.message.reply_text("Unauthorized.")
            return

        if not update.effective_chat.is_forum:
            await update.message.reply_text("This command only works in forum groups.")
            return

        # If no args provided, show inline keyboard with eligible sessions
        if not context.args:
            if not self._on_list_sessions:
                await update.message.reply_text("Session listing not configured.")
                return

            # Get all sessions
            sessions = await self._on_list_sessions()

            # Filter for eligible sessions (no telegram_thread_id AND status != stopped)
            eligible = [
                s for s in sessions
                if not s.telegram_thread_id and s.status.value != "stopped"
            ]

            if not eligible:
                await update.message.reply_text(
                    "No eligible sessions to follow.\n"
                    "All sessions either already have topics or are stopped."
                )
                return

            # Create inline keyboard
            keyboard = []
            for session in eligible:
                # Display name shows friendly name if available, otherwise session ID
                display_name = session.friendly_name or session.id
                button_text = f"{display_name} [{session.id}]"
                keyboard.append([
                    InlineKeyboardButton(button_text, callback_data=f"follow:{session.id}")
                ])

            await update.message.reply_text(
                "Select a session to follow:",
                reply_markup=InlineKeyboardMarkup(keyboard)
            )
            return

        identifier = " ".join(context.args)
        chat_id = update.effective_chat.id

        # Resolve session by ID or friendly name
        session = None

        # Try as session ID first
        if self._on_session_status:
            session = await self._on_session_status(identifier)

        # If not found by ID, try as friendly name
        if not session and self._on_list_sessions:
            sessions = await self._on_list_sessions()
            for s in sessions:
                if s.friendly_name == identifier:
                    session = s
                    break

        if not session:
            await update.message.reply_text(f"Session not found: {identifier}")
            return

        # Check if session already has a topic
        if session.telegram_thread_id:
            await update.message.reply_text(
                f"Session [{session.id}] already has a topic (ID: {session.telegram_thread_id})"
            )
            return

        # Create forum topic for the session
        topic_name = f"{session.friendly_name or 'session'} [{session.id}]"
        topic_id = await self.create_forum_topic(chat_id, topic_name)

        if not topic_id:
            await update.message.reply_text("Failed to create forum topic.")
            return

        # Register topic -> session mapping
        self.register_topic_session(chat_id, topic_id, session.id)

        # Send welcome message in the new topic
        await self.bot.send_message(
            chat_id=chat_id,
            message_thread_id=topic_id,
            text=f"Now following session: {session.name}\n"
                 f"ID: {session.id}\n"
                 f"Directory: {session.working_dir}\n\n"
                 "Send messages here to interact with Claude."
        )

        # Update session with topic info
        if self._on_update_topic:
            await self._on_update_topic(session.id, chat_id, topic_id)

        await update.message.reply_text(
            f"Created topic for session [{session.id}]\n"
            f"You can now interact with it in the new topic."
        )

    async def _handle_new_project(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle project selection button press."""
        query = update.callback_query
        await query.answer()

        # Extract project path from callback data
        callback_data = query.data  # Format: "new_project:path/to/project"
        _, path = callback_data.split(":", 1)

        if path == "custom":
            await query.edit_message_text(
                "Send the path to your project:\n(or reply to this message with `/new /path/to/project`)"
            )
            return

        # Create session in selected project
        chat_id = query.message.chat_id
        is_forum = query.message.chat.is_forum

        await query.edit_message_text(f"Creating session in {path}...")

        try:
            session = await self._on_new_session(chat_id, path)

            if session:
                topic_id = None

                # In forum groups, create a dedicated topic for this session
                if is_forum:
                    topic_name = f"{session.friendly_name or 'session'} [{session.id}]"
                    topic_id = await self.create_forum_topic(chat_id, topic_name)

                    if topic_id:
                        # Register topic -> session mapping
                        self.register_topic_session(chat_id, topic_id, session.id)

                        # Send welcome message in the new topic
                        msg = await self.bot.send_message(
                            chat_id=chat_id,
                            message_thread_id=topic_id,
                            text=f"Session created: {session.name}\n"
                                 f"ID: {session.id}\n"
                                 f"Directory: {session.working_dir}\n\n"
                                 "Send messages here to interact with Claude."
                        )

                        # Update session with topic info
                        if self._on_update_topic:
                            await self._on_update_topic(session.id, chat_id, topic_id)

                        await query.edit_message_text(f"Created topic for session [{session.id}]")
                    else:
                        await query.edit_message_text("Failed to create topic. Using reply mode.")
                        is_forum = False

                # Non-forum mode: use reply chains
                if not is_forum or not topic_id:
                    msg = await query.edit_message_text(
                        f"Session created: {session.name}\n"
                        f"ID: {session.id}\n"
                        f"Directory: {session.working_dir}\n"
                        f"Status: {session.status.value}\n\n"
                        "Reply to this message to send input to Claude."
                    )
                    # Track this message as the thread root
                    self._session_threads[session.id] = (chat_id, msg.message_id)

                    if self._on_update_thread:
                        await self._on_update_thread(session.id, chat_id, msg.message_id)

            else:
                await query.edit_message_text("Failed to create session.")

        except Exception as e:
            logger.error(f"Error creating session: {e}")
            await query.edit_message_text(f"Error: {e}")

    async def _handle_permission_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle permission button press (Yes/No/Always/Never)."""
        query = update.callback_query
        await query.answer()

        logger.info(f"Permission callback received: {query.data}")

        # Extract session_id and response from callback data
        # Format: "perm:session_id:response" (e.g., "perm:a4af4272:1")
        parts = query.data.split(":", 2)
        if len(parts) != 3:
            logger.error(f"Invalid permission callback data: {query.data}")
            await query.edit_message_text("Invalid button data.")
            return

        _, session_id, response = parts
        chat_id = query.message.chat_id
        logger.info(f"Permission callback: session={session_id}, response={response}")

        # Map button responses to display names
        # Claude Code uses numbered options: 1=Yes once, 2=Always, 3=No, 4=Never
        response_names = {
            "1": "Yes (once)",
            "2": "Always",
            "3": "No",
            "4": "Never",
        }
        response_name = response_names.get(response, response)

        if not self._on_session_input:
            logger.error("Session input handler not configured!")
            await query.edit_message_text("Session input handler not configured.")
            return

        try:
            # Send the response to the session (bypass queue for immediate delivery)
            user_input = UserInput(
                session_id=session_id,
                text=response,
                source=NotificationChannel.TELEGRAM,
                chat_id=chat_id,
                message_id=query.message.message_id,
                is_permission_response=True,
            )

            logger.info(f"Sending permission response to session {session_id}: {response}")
            success = await self._on_session_input(user_input)
            logger.info(f"Permission response sent, success={success}")

            if success:
                # Update the message to show the response was sent
                # Keep the original message but update to show what was selected
                original_text = query.message.text or ""
                # Remove the "Reply with:" line since we handled it
                lines = original_text.split("\n")
                lines = [l for l in lines if not l.startswith("Reply with:")]
                lines.append(f"\n✓ Sent: {response_name}")
                await query.edit_message_text("\n".join(lines))
            else:
                await query.edit_message_text(
                    f"{query.message.text}\n\n✗ Failed to send response."
                )

        except Exception as e:
            logger.error(f"Error handling permission callback: {e}")
            await query.edit_message_text(f"Error: {e}")

    async def _handle_follow_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle follow button press."""
        query = update.callback_query
        await query.answer()

        # Extract session_id from callback data
        callback_data = query.data  # Format: "follow:session_id"
        _, session_id = callback_data.split(":", 1)

        chat_id = query.message.chat_id

        await query.edit_message_text(f"Creating topic for session [{session_id}]...")

        try:
            # Get session details
            if not self._on_session_status:
                await query.edit_message_text("Session status check not configured.")
                return

            session = await self._on_session_status(session_id)

            if not session:
                await query.edit_message_text(f"Session not found: {session_id}")
                return

            # Check if session already has a topic
            if session.telegram_thread_id:
                await query.edit_message_text(
                    f"Session [{session.id}] already has a topic (ID: {session.telegram_thread_id})"
                )
                return

            # Create forum topic for the session
            topic_name = f"{session.friendly_name or 'session'} [{session.id}]"
            topic_id = await self.create_forum_topic(chat_id, topic_name)

            if not topic_id:
                await query.edit_message_text("Failed to create forum topic.")
                return

            # Register topic -> session mapping
            self.register_topic_session(chat_id, topic_id, session.id)

            # Send welcome message in the new topic
            await self.bot.send_message(
                chat_id=chat_id,
                message_thread_id=topic_id,
                text=f"Now following session: {session.name}\n"
                     f"ID: {session.id}\n"
                     f"Directory: {session.working_dir}\n\n"
                     "Send messages here to interact with Claude."
            )

            # Update session with topic info
            if self._on_update_topic:
                await self._on_update_topic(session.id, chat_id, topic_id)

            await query.edit_message_text(
                f"✓ Created topic for session [{session.id}]\n"
                f"You can now interact with it in the new topic."
            )

        except Exception as e:
            logger.error(f"Error creating topic for session: {e}")
            await query.edit_message_text(f"Error: {e}")

    async def start(self):
        """Start the bot."""
        self.application = (
            Application.builder()
            .token(self.token)
            .build()
        )

        self.bot = self.application.bot

        # Register handlers
        self.application.add_handler(CommandHandler("start", self._cmd_start))
        self.application.add_handler(CommandHandler("help", self._cmd_help))
        self.application.add_handler(CommandHandler("new", self._cmd_new))
        self.application.add_handler(CommandHandler("session", self._cmd_session))
        self.application.add_handler(CommandHandler("list", self._cmd_list))
        self.application.add_handler(CommandHandler("status", self._cmd_status))
        self.application.add_handler(CommandHandler("subagents", self._cmd_subagents))
        self.application.add_handler(CommandHandler("message", self._cmd_message))
        self.application.add_handler(CommandHandler("summary", self._cmd_summary))
        self.application.add_handler(CommandHandler("kill", self._cmd_kill))
        self.application.add_handler(CommandHandler("stop", self._cmd_stop))
        self.application.add_handler(CommandHandler("force", self._cmd_force))
        self.application.add_handler(CommandHandler("open", self._cmd_open))
        self.application.add_handler(CommandHandler("name", self._cmd_name))
        self.application.add_handler(CommandHandler("password", self._cmd_password))
        self.application.add_handler(CommandHandler("follow", self._cmd_follow))

        # Handle button presses for project selection
        self.application.add_handler(CallbackQueryHandler(self._handle_new_project, pattern="^new_project:"))

        # Handle button presses for follow command
        self.application.add_handler(CallbackQueryHandler(self._handle_follow_callback, pattern="^follow:"))

        # Handle button presses for permission prompts
        self.application.add_handler(CallbackQueryHandler(self._handle_permission_callback, pattern="^perm:"))

        # Handle regular messages (for replies) - include commands so /clear, /compact, etc can be sent as input
        self.application.add_handler(MessageHandler(filters.TEXT, self._handle_message))

        # Start polling
        await self.application.initialize()
        await self.application.start()
        await self.application.updater.start_polling()

        logger.info("Telegram bot started")

    async def stop(self):
        """Stop the bot."""
        if self.application:
            await self.application.updater.stop()
            await self.application.stop()
            await self.application.shutdown()
            logger.info("Telegram bot stopped")
