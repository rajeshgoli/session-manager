"""Main entry point - orchestrates all components."""

import asyncio
import logging
import os
import signal
import sys
import threading
import time
from pathlib import Path
from typing import Optional

import yaml
import uvicorn

from .models import Session, SessionStatus, NotificationEvent, UserInput, DeliveryResult
from .session_manager import SessionManager
from .output_monitor import OutputMonitor
from .telegram_bot import TelegramBot
from .email_handler import EmailHandler
from .notifier import Notifier
from .server import create_app
from .child_monitor import ChildMonitor
from .message_queue import MessageQueueManager
from .tool_logger import ToolLogger

logger = logging.getLogger(__name__)


class EventLoopWatchdog:
    """
    Watchdog that monitors the asyncio event loop health.
    If the event loop becomes unresponsive, it kills the process to allow
    the process supervisor (launchd) to restart it.
    """

    def __init__(self, loop: asyncio.AbstractEventLoop, check_interval: int = 30, timeout: int = 10):
        """
        Args:
            loop: The asyncio event loop to monitor
            check_interval: Seconds between health checks
            timeout: Seconds to wait for event loop response before considering it frozen
        """
        self.loop = loop
        self.check_interval = check_interval
        self.timeout = timeout
        self._stop_event = threading.Event()
        self._thread: Optional[threading.Thread] = None

    def start(self):
        """Start the watchdog in a background thread."""
        self._thread = threading.Thread(target=self._watchdog_loop, daemon=True)
        self._thread.start()
        logger.info(f"Event loop watchdog started (check every {self.check_interval}s, timeout {self.timeout}s)")

    def stop(self):
        """Stop the watchdog."""
        self._stop_event.set()
        if self._thread:
            self._thread.join(timeout=5)

    def _watchdog_loop(self):
        """Main watchdog loop running in a separate thread."""
        while not self._stop_event.wait(self.check_interval):
            if not self._check_event_loop_health():
                logger.error("Event loop is frozen! Killing process for restart...")
                # Give a moment for the log to flush
                time.sleep(0.5)
                # Kill ourselves - launchd will restart
                os._exit(1)

    def _check_event_loop_health(self) -> bool:
        """
        Check if the event loop is responsive.
        Returns True if healthy, False if frozen.
        """
        response_event = threading.Event()

        def set_response():
            response_event.set()

        try:
            # Schedule a callback on the event loop
            self.loop.call_soon_threadsafe(set_response)

            # Wait for it to execute
            if response_event.wait(timeout=self.timeout):
                return True
            else:
                logger.warning(f"Event loop did not respond within {self.timeout}s")
                return False
        except Exception as e:
            logger.error(f"Error checking event loop health: {e}")
            return False


def load_config(config_path: str = "config.yaml") -> dict:
    """Load configuration from YAML file."""
    path = Path(config_path)

    if not path.exists():
        logger.warning(f"Config file not found: {config_path}, using defaults")
        return {}

    with open(path) as f:
        return yaml.safe_load(f) or {}


class SessionManagerApp:
    """Main application orchestrator."""

    def __init__(self, config: dict):
        self.config = config

        # Server config
        self.host = config.get("server", {}).get("host", "127.0.0.1")
        self.port = config.get("server", {}).get("port", 8420)

        # Paths
        self.log_dir = config.get("paths", {}).get("log_dir", "/tmp/claude-sessions")
        self.state_file = config.get("paths", {}).get("state_file", "/tmp/claude-sessions/sessions.json")

        # Initialize components
        self.session_manager = SessionManager(
            log_dir=self.log_dir,
            state_file=self.state_file,
            config=config,
        )

        # Will set notifier reference after creating notifier below
        self.session_manager.notifier = None

        monitor_config = config.get("monitor", {})
        notify_config = monitor_config.get("notify", {})
        self.output_monitor = OutputMonitor(
            idle_timeout=monitor_config.get("idle_timeout", 300),
            poll_interval=monitor_config.get("poll_interval", 1.0),
            notify_errors=notify_config.get("errors", False),
            notify_permission_prompts=notify_config.get("permission_prompts", True),
            notify_completion=notify_config.get("completion", False),
            notify_idle=notify_config.get("idle", True),
            config=config,  # Pass full config for timeout settings
        )

        # Email handler (uses existing harness)
        email_config = config.get("email", {})
        self.email_handler = EmailHandler(
            email_config=email_config.get("smtp_config", ""),
            imap_config=email_config.get("imap_config", ""),
        )

        # Telegram bot (optional)
        telegram_config = config.get("telegram", {})
        self.telegram_bot: Optional[TelegramBot] = None

        if telegram_config.get("token"):
            services_config = self.config.get("services", {})
            self.telegram_bot = TelegramBot(
                token=telegram_config["token"],
                allowed_chat_ids=telegram_config.get("allowed_chat_ids"),
                allowed_user_ids=telegram_config.get("allowed_user_ids"),
                office_automate_url=services_config.get("office_automate_url"),
            )
            self._setup_telegram_handlers()

        # Notifier
        self.notifier = Notifier(
            telegram_bot=self.telegram_bot,
            email_handler=self.email_handler,
        )

        # Set notifier reference in session manager for sm_send notifications
        self.session_manager.notifier = self.notifier

        # Wire up output monitor callbacks
        self.output_monitor.set_event_callback(self._handle_monitor_event)
        self.output_monitor.set_status_callback(self._handle_status_change)
        self.output_monitor.set_save_state_callback(self.session_manager._save_state)
        self.output_monitor.set_session_manager(self.session_manager)

        # Child monitor for --wait functionality
        self.child_monitor = ChildMonitor(self.session_manager)
        # Pass child monitor to session manager
        self.session_manager.child_monitor = self.child_monitor

        # Message queue manager for reliable inter-agent messaging (sm-send-v2)
        sm_send_config = config.get("sm_send", {})
        self.message_queue = MessageQueueManager(
            self.session_manager,
            db_path=sm_send_config.get("db_path", "~/.local/share/claude-sessions/message_queue.db"),
            config=config,  # Pass full config for timeout settings
        )
        # Pass message queue to session manager
        self.session_manager.message_queue_manager = self.message_queue

        # Tool logger for security audit
        tool_logging_config = config.get("tool_logging", {})
        db_path = tool_logging_config.get("db_path", "~/.local/share/claude-sessions/tool_usage.db")
        self.tool_logger = ToolLogger(db_path=db_path)

        # Create FastAPI app
        self.app = create_app(
            session_manager=self.session_manager,
            notifier=self.notifier,
            output_monitor=self.output_monitor,
            child_monitor=self.child_monitor,
            config=config,  # Pass config for server timeout settings
        )

        # Attach tool logger to app state
        self.app.state.tool_logger = self.tool_logger

        # Connect output monitor to hook output storage
        self.output_monitor.set_hook_output_store(self.app.state.last_claude_output)

        self._shutdown_event = asyncio.Event()

    def _setup_telegram_handlers(self):
        """Wire up Telegram bot handlers to session manager."""
        if not self.telegram_bot:
            return

        async def on_new_session(chat_id: int, working_dir: str) -> Optional[Session]:
            session = await self.session_manager.create_session(
                working_dir=working_dir,
                telegram_chat_id=chat_id,
            )
            if session:
                await self.output_monitor.start_monitoring(session)
            return session

        async def on_list_sessions() -> list[Session]:
            return self.session_manager.list_sessions()

        async def on_kill_session(session_id: str) -> bool:
            await self.output_monitor.stop_monitoring(session_id)
            return self.session_manager.kill_session(session_id)

        async def on_session_input(user_input: UserInput) -> DeliveryResult:
            result = await self.session_manager.send_input(
                user_input.session_id,
                user_input.text,
                bypass_queue=user_input.is_permission_response,
                delivery_mode=getattr(user_input, 'delivery_mode', 'sequential'),
            )
            if result != DeliveryResult.FAILED:
                self.output_monitor.update_activity(user_input.session_id)
            return result

        async def on_session_status(session_id: str) -> Optional[Session]:
            return self.session_manager.get_session(session_id)

        async def on_open_terminal(session_id: str) -> bool:
            return self.session_manager.open_terminal(session_id)

        async def on_update_thread(session_id: str, chat_id: int, message_id: int) -> None:
            self.session_manager.update_telegram_thread(session_id, chat_id, message_id)

        async def on_update_topic(session_id: str, chat_id: int, topic_id: int) -> None:
            session = self.session_manager.get_session(session_id)
            if session:
                session.telegram_chat_id = chat_id
                session.telegram_thread_id = topic_id
                self.session_manager._save_state()

        async def on_set_name(session_id: str, name: str) -> bool:
            """Set a friendly name for a session."""
            session = self.session_manager.get_session(session_id)
            if not session:
                return False

            session.friendly_name = name
            self.session_manager._save_state()

            # Update tmux status bar to show friendly name
            self.session_manager.tmux.set_status_bar(session.tmux_session, name)

            return True

        async def on_get_last_output(session_id: str) -> Optional[str]:
            """Get last Claude output for a session."""
            # Only return session-specific output - don't fall back to "latest"
            # (fallback would show output from wrong session)
            return self.app.state.last_claude_output.get(session_id)

        async def on_get_last_message(session_id: str) -> Optional[str]:
            """Get last Claude message for a session (full message from hooks)."""
            # Only return session-specific message - don't fall back to "latest"
            # (fallback would show message from wrong session)
            return self.app.state.last_claude_output.get(session_id)

        async def on_get_tmux_output(session_id: str, lines: int) -> Optional[str]:
            """Get tmux output for a session."""
            return self.session_manager.capture_output(session_id, lines)

        async def on_interrupt_session(session_id: str) -> bool:
            """Send Escape to interrupt Claude."""
            session = self.session_manager.get_session(session_id)
            if not session:
                return False
            success = self.session_manager.send_key(session_id, "Escape")

            if success:
                # After interrupt, Claude won't fire Stop hook
                # Mark idle after delay to allow message delivery
                async def mark_idle_after_delay():
                    await asyncio.sleep(1.0)  # Give Claude time to process interrupt
                    if self.message_queue:
                        self.message_queue.mark_session_idle(session_id)
                asyncio.create_task(mark_idle_after_delay())

            return success

        async def on_get_subagents(session_id: str) -> Optional[list]:
            """Get subagents for a session."""
            import httpx
            try:
                async with httpx.AsyncClient() as client:
                    response = await client.get(
                        f"http://127.0.0.1:{self.config['server']['port']}/sessions/{session_id}/subagents",
                        timeout=5.0
                    )
                    if response.status_code == 200:
                        data = response.json()
                        return data.get("subagents", [])
            except Exception as e:
                logger.error(f"Error getting subagents: {e}")
            return None

        self.telegram_bot.set_new_session_handler(on_new_session)
        self.telegram_bot.set_list_sessions_handler(on_list_sessions)
        self.telegram_bot.set_kill_session_handler(on_kill_session)
        self.telegram_bot.set_session_input_handler(on_session_input)
        self.telegram_bot.set_session_status_handler(on_session_status)
        self.telegram_bot.set_open_terminal_handler(on_open_terminal)
        self.telegram_bot.set_update_thread_handler(on_update_thread)
        self.telegram_bot.set_update_topic_handler(on_update_topic)
        self.telegram_bot.set_name_handler(on_set_name)
        self.telegram_bot.set_get_last_output_handler(on_get_last_output)
        self.telegram_bot.set_get_last_message_handler(on_get_last_message)
        self.telegram_bot.set_get_tmux_output_handler(on_get_tmux_output)
        self.telegram_bot.set_interrupt_handler(on_interrupt_session)
        self.telegram_bot.set_get_subagents_handler(on_get_subagents)

    async def _handle_monitor_event(self, event: NotificationEvent):
        """Handle events from the output monitor."""
        session = self.session_manager.get_session(event.session_id)
        await self.notifier.notify(event, session)

    async def _handle_status_change(self, session_id: str, status: SessionStatus):
        """Handle status changes from the output monitor."""
        self.session_manager.update_session_status(session_id, status)

    async def start(self):
        """Start all components."""
        logger.info("Starting Claude Session Manager...")

        # Start child monitor
        await self.child_monitor.start()
        logger.info("Child monitor started")

        # Start message queue manager
        await self.message_queue.start()
        logger.info("Message queue manager started")

        # Start Telegram bot if configured
        if self.telegram_bot:
            await self.telegram_bot.start()
            # Restore thread mappings from persisted sessions
            self.telegram_bot.load_session_threads(self.session_manager.list_sessions())
            logger.info("Telegram bot started")

        # Restore monitoring for existing sessions
        for session in self.session_manager.list_sessions():
            if session.status not in (SessionStatus.STOPPED, SessionStatus.ERROR):
                await self.output_monitor.start_monitoring(session, is_restored=True)
                logger.info(f"Restored monitoring for session {session.name}")

                # Update tmux status bar if friendly name exists
                if session.friendly_name:
                    self.session_manager.tmux.set_status_bar(session.tmux_session, session.friendly_name)

        # Start the web server
        config = uvicorn.Config(
            self.app,
            host=self.host,
            port=self.port,
            log_level="info",
        )
        server = uvicorn.Server(config)

        logger.info(f"Starting server on http://{self.host}:{self.port}")

        # Run until shutdown
        await server.serve()

    async def stop(self):
        """Stop all components."""
        logger.info("Stopping Claude Session Manager...")

        # Stop output monitor
        await self.output_monitor.stop_all()

        # Stop child monitor
        await self.child_monitor.stop()

        # Stop message queue manager
        await self.message_queue.stop()

        # Stop Telegram bot
        if self.telegram_bot:
            await self.telegram_bot.stop()

        logger.info("Shutdown complete")


def setup_signal_handlers(app: SessionManagerApp):
    """Set up signal handlers for graceful shutdown."""
    def signal_handler(sig, frame):
        logger.info(f"Received signal {sig}, shutting down...")
        asyncio.create_task(app.stop())
        sys.exit(0)

    signal.signal(signal.SIGINT, signal_handler)
    signal.signal(signal.SIGTERM, signal_handler)


async def main():
    """Main entry point."""
    # Setup logging
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s - %(name)s - %(levelname)s - %(message)s",
    )

    # Load config
    config = load_config("config.yaml")

    # Create and start app
    app = SessionManagerApp(config)
    setup_signal_handlers(app)

    # Start event loop watchdog
    watchdog_config = config.get("watchdog", {})
    watchdog = EventLoopWatchdog(
        loop=asyncio.get_running_loop(),
        check_interval=watchdog_config.get("check_interval", 30),
        timeout=watchdog_config.get("timeout", 10),
    )
    watchdog.start()

    try:
        await app.start()
    except KeyboardInterrupt:
        await app.stop()
    finally:
        watchdog.stop()


def run():
    """Entry point for console script."""
    asyncio.run(main())


if __name__ == "__main__":
    run()
