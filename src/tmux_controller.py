"""tmux operations for spawning and controlling Claude Code sessions."""

import asyncio
import subprocess
import shutil
from pathlib import Path
from typing import Optional
import logging

logger = logging.getLogger(__name__)


class TmuxController:
    """Controls tmux sessions for Claude Code."""

    def __init__(self, log_dir: str = "/tmp/claude-sessions"):
        self.log_dir = Path(log_dir)
        self.log_dir.mkdir(parents=True, exist_ok=True)

    def _run_tmux(self, *args: str, check: bool = True) -> subprocess.CompletedProcess:
        """Run a tmux command."""
        cmd = ["tmux"] + list(args)
        logger.debug(f"Running tmux command: {' '.join(cmd)}")
        return subprocess.run(cmd, capture_output=True, text=True, check=check)

    def session_exists(self, session_name: str) -> bool:
        """Check if a tmux session exists."""
        result = self._run_tmux("has-session", "-t", session_name, check=False)
        return result.returncode == 0

    def set_status_bar(self, session_name: str, friendly_name: str) -> bool:
        """
        Update tmux status bar to show friendly name.

        Args:
            session_name: tmux session name
            friendly_name: User-friendly name to display

        Returns:
            True if successful
        """
        if not self.session_exists(session_name):
            logger.warning(f"Session {session_name} does not exist")
            return False

        try:
            # Set status-left to show friendly name
            self._run_tmux(
                "set-option",
                "-t", session_name,
                "status-left",
                f"[{friendly_name}] "
            )
            logger.info(f"Updated status bar for {session_name} to show '{friendly_name}'")
            return True
        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to set status bar: {e.stderr}")
            return False

    def create_session(
        self,
        session_name: str,
        working_dir: str,
        log_file: str,
        session_id: Optional[str] = None,
    ) -> bool:
        """
        Create a new tmux session with Claude Code running inside.

        Args:
            session_name: Name for the tmux session
            working_dir: Directory to start Claude in
            log_file: Path to pipe output to
            session_id: Session manager session ID to pass to Claude

        Returns:
            True if session created successfully
        """
        if self.session_exists(session_name):
            logger.warning(f"Session {session_name} already exists")
            return False

        working_path = Path(working_dir).expanduser().resolve()
        if not working_path.exists():
            logger.error(f"Working directory does not exist: {working_dir}")
            return False

        # Ensure log file parent directory exists
        log_path = Path(log_file)
        log_path.parent.mkdir(parents=True, exist_ok=True)

        # Touch the log file
        log_path.touch()

        try:
            # Create new detached tmux session
            self._run_tmux(
                "new-session",
                "-d",
                "-s", session_name,
                "-c", str(working_path),
            )

            # Set up pipe-pane to capture output to log file
            self._run_tmux(
                "pipe-pane",
                "-t", session_name,
                f"cat >> {log_file}",
            )

            # Set up environment variable first (persists in the shell)
            if session_id:
                # Export session ID so it persists even if user exits and restarts Claude
                self._run_tmux(
                    "send-keys",
                    "-t", session_name,
                    f"export CLAUDE_SESSION_MANAGER_ID={session_id}",
                    "Enter",
                )
                # Small delay to ensure export completes
                import time
                time.sleep(0.1)

            # Start Claude Code in the session
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                "claude",
                "Enter",
            )

            logger.info(f"Created session {session_name} (id={session_id}) in {working_dir}")
            return True

        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to create session: {e.stderr}")
            return False

    def send_input(self, session_name: str, text: str) -> bool:
        """
        Send input text to a tmux session.

        Args:
            session_name: Target session name
            text: Text to send (will add Enter at end)

        Returns:
            True if input sent successfully
        """
        if not self.session_exists(session_name):
            logger.error(f"Session {session_name} does not exist")
            return False

        try:
            import shlex
            # Run as shell command with sleep to avoid paste detection
            # Note: -l flag causes issues with Claude Code, so we don't use it
            escaped_text = shlex.quote(text)
            cmd = f'tmux send-keys -t {session_name} {escaped_text} && sleep 1 && tmux send-keys -t {session_name} Enter'
            logger.info(f"Running command: {cmd}")
            result = subprocess.run(cmd, shell=True, check=True, capture_output=True, text=True)
            logger.info(f"Command stdout: {result.stdout}, stderr: {result.stderr}")
            logger.info(f"Sent input to {session_name}: {text[:50]}...")
            return True

        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to send input: {e.stderr}")
            return False

    def send_key(self, session_name: str, key: str) -> bool:
        """
        Send a single key to a tmux session (e.g., 'y', 'n', 'Enter').

        Args:
            session_name: Target session name
            key: Key to send

        Returns:
            True if key sent successfully
        """
        if not self.session_exists(session_name):
            logger.error(f"Session {session_name} does not exist")
            return False

        try:
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                key,
            )
            logger.info(f"Sent key to {session_name}: {key}")
            return True

        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to send key: {e.stderr}")
            return False

    def kill_session(self, session_name: str) -> bool:
        """
        Kill a tmux session.

        Args:
            session_name: Session to kill

        Returns:
            True if session killed successfully
        """
        if not self.session_exists(session_name):
            logger.warning(f"Session {session_name} does not exist")
            return True  # Already gone

        try:
            self._run_tmux("kill-session", "-t", session_name)
            logger.info(f"Killed session {session_name}")
            return True

        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to kill session: {e.stderr}")
            return False

    def list_sessions(self) -> list[str]:
        """List all tmux sessions."""
        result = self._run_tmux("list-sessions", "-F", "#{session_name}", check=False)
        if result.returncode != 0:
            return []
        return [s.strip() for s in result.stdout.strip().split("\n") if s.strip()]

    def capture_pane(self, session_name: str, lines: int = 50) -> Optional[str]:
        """
        Capture recent output from a session's pane.

        Args:
            session_name: Session to capture from
            lines: Number of lines to capture

        Returns:
            Captured text or None on error
        """
        if not self.session_exists(session_name):
            return None

        try:
            result = self._run_tmux(
                "capture-pane",
                "-t", session_name,
                "-p",  # Print to stdout
                "-S", f"-{lines}",  # Start from N lines back
            )
            return result.stdout

        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to capture pane: {e.stderr}")
            return None

    def open_in_terminal(self, session_name: str) -> bool:
        """
        Open a tmux session in a new Terminal.app window (macOS only).

        Args:
            session_name: Session to open

        Returns:
            True if terminal opened successfully
        """
        if not self.session_exists(session_name):
            logger.error(f"Session {session_name} does not exist")
            return False

        if not shutil.which("osascript"):
            logger.error("osascript not found - not on macOS")
            return False

        # AppleScript to open new Terminal window and attach to tmux session
        script = f'''
        tell application "Terminal"
            activate
            do script "tmux attach-session -t {session_name}"
        end tell
        '''

        try:
            subprocess.run(["osascript", "-e", script], check=True, capture_output=True)
            logger.info(f"Opened Terminal window for session {session_name}")
            return True

        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to open Terminal: {e.stderr}")
            return False


async def test_controller():
    """Test the tmux controller."""
    controller = TmuxController()

    # List existing sessions
    sessions = controller.list_sessions()
    print(f"Existing sessions: {sessions}")

    # Create a test session
    test_name = "test-claude-session"
    log_file = f"/tmp/claude-sessions/{test_name}.log"

    if controller.create_session(test_name, "~", log_file):
        print(f"Created session: {test_name}")

        # Wait a moment for Claude to start
        await asyncio.sleep(2)

        # Capture output
        output = controller.capture_pane(test_name)
        print(f"Captured output:\n{output}")

        # Send a simple command
        controller.send_input(test_name, "/help")

        await asyncio.sleep(2)

        # Capture again
        output = controller.capture_pane(test_name)
        print(f"After /help:\n{output}")

        # Kill the session
        controller.kill_session(test_name)
        print("Session killed")


if __name__ == "__main__":
    logging.basicConfig(level=logging.DEBUG)
    asyncio.run(test_controller())
