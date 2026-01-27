"""Main entry point - orchestrates all components."""

import asyncio
import logging
import signal
import sys
from pathlib import Path
from typing import Optional

import yaml
import uvicorn

from .models import Session, SessionStatus, NotificationEvent, UserInput
from .session_manager import SessionManager
from .output_monitor import OutputMonitor
from .telegram_bot import TelegramBot
from .email_handler import EmailHandler
from .notifier import Notifier
from .server import create_app

logger = logging.getLogger(__name__)


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
        )

        monitor_config = config.get("monitor", {})
        notify_config = monitor_config.get("notify", {})
        self.output_monitor = OutputMonitor(
            idle_timeout=monitor_config.get("idle_timeout", 300),
            poll_interval=monitor_config.get("poll_interval", 1.0),
            notify_errors=notify_config.get("errors", False),
            notify_permission_prompts=notify_config.get("permission_prompts", True),
            notify_completion=notify_config.get("completion", False),
            notify_idle=notify_config.get("idle", True),
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

        # Wire up output monitor callbacks
        self.output_monitor.set_event_callback(self._handle_monitor_event)
        self.output_monitor.set_status_callback(self._handle_status_change)
        self.output_monitor.set_save_state_callback(self.session_manager._save_state)
        self.output_monitor.set_session_manager(self.session_manager)

        # Create FastAPI app
        self.app = create_app(
            session_manager=self.session_manager,
            notifier=self.notifier,
            output_monitor=self.output_monitor,
        )

        # Connect output monitor to hook output storage
        self.output_monitor.set_hook_output_store(self.app.state.last_claude_output)

        self._shutdown_event = asyncio.Event()

    def _setup_telegram_handlers(self):
        """Wire up Telegram bot handlers to session manager."""
        if not self.telegram_bot:
            return

        async def on_new_session(chat_id: int, working_dir: str) -> Optional[Session]:
            session = self.session_manager.create_session(
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

        async def on_session_input(user_input: UserInput) -> bool:
            success = self.session_manager.send_input(user_input.session_id, user_input.text)
            if success:
                self.output_monitor.update_activity(user_input.session_id)
            return success

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
                session.telegram_topic_id = topic_id
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
            return self.session_manager.send_key(session_id, "Escape")

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

    try:
        await app.start()
    except KeyboardInterrupt:
        await app.stop()


def run():
    """Entry point for console script."""
    asyncio.run(main())


if __name__ == "__main__":
    run()
