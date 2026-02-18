"""tmux operations for spawning and controlling Claude Code sessions."""

import asyncio
import shlex
import subprocess
import shutil
from pathlib import Path
from typing import Optional
import logging

logger = logging.getLogger(__name__)


class TmuxController:
    """Controls tmux sessions for Claude Code."""

    def __init__(self, log_dir: str = "/tmp/claude-sessions", config: Optional[dict] = None):
        self.log_dir = Path(log_dir)
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self.config = config or {}

        # Load timeout configuration with fallbacks
        timeouts = self.config.get("timeouts", {})
        tmux_timeouts = timeouts.get("tmux", {})

        self.shell_export_settle_seconds = tmux_timeouts.get("shell_export_settle_seconds", 0.1)
        self.claude_init_seconds = tmux_timeouts.get("claude_init_seconds", 3)
        self.claude_init_no_prompt_seconds = tmux_timeouts.get("claude_init_no_prompt_seconds", 1)
        self.send_keys_timeout_seconds = tmux_timeouts.get("send_keys_timeout_seconds", 5)
        self.send_keys_settle_seconds = tmux_timeouts.get("send_keys_settle_seconds", 0.3)

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

            # Set up environment variables first (persists in the shell)
            # Unset CLAUDECODE to allow spawning Claude Code in child sessions
            # (Claude Code sets this to detect nested sessions, but our tmux sessions are independent)
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                "unset CLAUDECODE",
                "Enter",
            )
            # Workaround for Claude Code bug: ToolSearch infinite loop (issues #20329, #20468, #20982)
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                "export ENABLE_TOOL_SEARCH=false",
                "Enter",
            )

            if session_id:
                # Export session ID so it persists even if user exits and restarts Claude
                self._run_tmux(
                    "send-keys",
                    "-t", session_name,
                    f"export CLAUDE_SESSION_MANAGER_ID={session_id}",
                    "Enter",
                )

            # Small delay to ensure exports complete
            import time
            time.sleep(self.shell_export_settle_seconds)

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

    def create_session_with_command(
        self,
        session_name: str,
        working_dir: str,
        log_file: str,
        session_id: Optional[str] = None,
        command: str = "claude",
        args: list[str] = None,
        model: Optional[str] = None,
        initial_prompt: Optional[str] = None,
    ) -> bool:
        """
        Create a new tmux session with custom Claude Code command.

        Args:
            session_name: Name for the tmux session
            working_dir: Directory to start Claude in
            log_file: Path to pipe output to
            session_id: Session manager session ID to pass to Claude
            command: Claude command (e.g., 'claude')
            args: Additional command-line arguments
            model: Model to use (opus, sonnet, haiku)
            initial_prompt: Initial prompt to send to Claude

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

            # Set up environment variables first (persists in the shell)
            # Unset CLAUDECODE to allow spawning Claude Code in child sessions
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                "unset CLAUDECODE",
                "Enter",
            )
            # Workaround for Claude Code bug: ToolSearch infinite loop (issues #20329, #20468, #20982)
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                "export ENABLE_TOOL_SEARCH=false",
                "Enter",
            )

            if session_id:
                # Export session ID so it persists
                self._run_tmux(
                    "send-keys",
                    "-t", session_name,
                    f"export CLAUDE_SESSION_MANAGER_ID={session_id}",
                    "Enter",
                )

            # Small delay to ensure exports complete
            import time
            time.sleep(self.shell_export_settle_seconds)

            # Build Claude command with args and model
            cmd_parts = [command]
            if args:
                cmd_parts.extend(args)
            if model:
                # Add model flag (e.g., --model sonnet)
                cmd_parts.extend(["--model", model])

            # Pass initial prompt as a CLI positional argument instead of typing
            # it via send-keys after startup. This avoids timing issues where
            # Claude Code hasn't finished initializing when the prompt arrives.
            if initial_prompt:
                cmd_parts.append("--")
                cmd_parts.append(shlex.quote(initial_prompt))

            # Start Claude Code in the session
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                " ".join(cmd_parts),
                "Enter",
            )

            if initial_prompt:
                logger.info(f"Created session with CLI prompt for {session_name} (prompt_len={len(initial_prompt)})")
            else:
                import time
                time.sleep(self.claude_init_no_prompt_seconds)

            # Log command without prompt payload to avoid leaking sensitive content
            log_parts = [p for p in cmd_parts if p != "--" and p != shlex.quote(initial_prompt)] if initial_prompt else cmd_parts
            logger.info(f"Created child session {session_name} (id={session_id}) with command {' '.join(log_parts)}")
            return True

        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to create session: {e.stderr}")
            return False

    def send_input(self, session_name: str, text: str) -> bool:
        """
        Send input text to a tmux session (SYNCHRONOUS - blocks event loop).

        WARNING: This method blocks for ~0.3 seconds. Use send_input_async() in async contexts.

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
            import time
            # Use subprocess with list arguments to prevent shell injection
            # Note: -l flag causes issues with Claude Code, so we don't use it
            # Small delay between send-keys calls to avoid paste detection
            subprocess.run(
                ["tmux", "send-keys", "-t", session_name, "--", text],
                check=True,
                capture_output=True,
                text=True,
                timeout=self.send_keys_timeout_seconds
            )
            time.sleep(self.send_keys_settle_seconds)
            subprocess.run(
                ["tmux", "send-keys", "-t", session_name, "Enter"],
                check=True,
                capture_output=True,
                text=True,
                timeout=self.send_keys_timeout_seconds
            )
            logger.info(f"Sent input to {session_name}: {text[:50]}...")
            return True

        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to send input: {e.stderr}")
            return False
        except subprocess.TimeoutExpired:
            logger.error(f"Timeout sending input to {session_name}")
            return False

    async def send_input_async(self, session_name: str, text: str) -> bool:
        """
        Send input text to a tmux session (ASYNC - non-blocking).

        Use this in async contexts to avoid blocking the event loop.

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
            # Send text first
            proc = await asyncio.create_subprocess_exec(
                'tmux', 'send-keys', '-t', session_name, '--', text,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await asyncio.wait_for(
                proc.communicate(), timeout=self.send_keys_timeout_seconds
            )
            if proc.returncode != 0:
                logger.error(f"Failed to send text: {stderr.decode()}")
                return False

            # Settle delay to avoid paste detection (#178)
            # Claude Code (Node.js TUI in raw mode) treats a rapid character burst
            # as pasted text, in which \r is a literal byte not a submit command.
            # The gap lets paste mode end before Enter arrives as a separate event.
            await asyncio.sleep(self.send_keys_settle_seconds)

            # Send Enter as a separate keystroke
            proc = await asyncio.create_subprocess_exec(
                'tmux', 'send-keys', '-t', session_name, 'Enter',
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await asyncio.wait_for(
                proc.communicate(), timeout=self.send_keys_timeout_seconds
            )
            if proc.returncode != 0:
                logger.error(f"Failed to send Enter: {stderr.decode()}")
                return False

            logger.info(f"Sent input (async) to {session_name}: {text[:50]}...")
            return True

        except asyncio.TimeoutError:
            logger.error(f"Timeout sending input to {session_name}")
            return False
        except Exception as e:
            logger.error(f"Failed to send input: {e}")
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

    async def send_review_sequence(
        self,
        session_name: str,
        mode: str,
        base_branch: Optional[str] = None,
        commit_sha: Optional[str] = None,
        custom_prompt: Optional[str] = None,
        branch_position: Optional[int] = None,
        config: Optional[dict] = None,
    ) -> bool:
        """
        Send /review slash command and navigate the interactive menu.

        Args:
            session_name: Target tmux session
            mode: Review mode (branch, uncommitted, commit, custom)
            base_branch: Target branch for branch mode
            commit_sha: Target SHA for commit mode
            custom_prompt: Custom review text for custom mode
            branch_position: Pre-computed position in branch list (0-indexed)
            config: Review timing config (menu_settle_seconds, branch_settle_seconds)

        Returns:
            True if sequence sent successfully
        """
        if not self.session_exists(session_name):
            logger.error(f"Session {session_name} does not exist")
            return False

        cfg = config or {}
        menu_settle = cfg.get("menu_settle_seconds", 1.0)
        branch_settle = cfg.get("branch_settle_seconds", 1.0)

        try:
            if mode == "custom":
                # Custom mode: send /review <text> directly, bypasses menu
                review_text = f"/review {custom_prompt}"
                proc = await asyncio.create_subprocess_exec(
                    'tmux', 'send-keys', '-t', session_name, '--', review_text,
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
                await asyncio.sleep(self.send_keys_settle_seconds)
                proc = await asyncio.create_subprocess_exec(
                    'tmux', 'send-keys', '-t', session_name, 'Enter',
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
                logger.info(f"Sent custom review to {session_name}")
                return True

            # All other modes: send /review + Enter, then navigate menu
            proc = await asyncio.create_subprocess_exec(
                'tmux', 'send-keys', '-t', session_name, '--', '/review',
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
            await asyncio.sleep(self.send_keys_settle_seconds)

            proc = await asyncio.create_subprocess_exec(
                'tmux', 'send-keys', '-t', session_name, 'Enter',
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)

            # Wait for menu to appear
            await asyncio.sleep(menu_settle)

            if mode == "branch":
                # 1st menu item — just press Enter
                proc = await asyncio.create_subprocess_exec(
                    'tmux', 'send-keys', '-t', session_name, 'Enter',
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)

                # Wait for branch picker
                await asyncio.sleep(branch_settle)

                # Navigate to target branch
                if branch_position and branch_position > 0:
                    for _ in range(branch_position):
                        proc = await asyncio.create_subprocess_exec(
                            'tmux', 'send-keys', '-t', session_name, 'Down',
                            stdout=asyncio.subprocess.PIPE,
                            stderr=asyncio.subprocess.PIPE,
                        )
                        await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)

                # Confirm branch selection
                await asyncio.sleep(self.send_keys_settle_seconds)
                proc = await asyncio.create_subprocess_exec(
                    'tmux', 'send-keys', '-t', session_name, 'Enter',
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
                logger.info(f"Sent branch review to {session_name} (position={branch_position})")

            elif mode == "uncommitted":
                # 2nd menu item — Down then Enter
                proc = await asyncio.create_subprocess_exec(
                    'tmux', 'send-keys', '-t', session_name, 'Down',
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
                await asyncio.sleep(self.send_keys_settle_seconds)
                proc = await asyncio.create_subprocess_exec(
                    'tmux', 'send-keys', '-t', session_name, 'Enter',
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
                logger.info(f"Sent uncommitted review to {session_name}")

            elif mode == "commit":
                if commit_sha:
                    logger.error("Commit mode SHA navigation not yet implemented; use --custom as a workaround")
                    return False

                # 3rd menu item — Down Down then Enter
                for _ in range(2):
                    proc = await asyncio.create_subprocess_exec(
                        'tmux', 'send-keys', '-t', session_name, 'Down',
                        stdout=asyncio.subprocess.PIPE,
                        stderr=asyncio.subprocess.PIPE,
                    )
                    await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
                await asyncio.sleep(self.send_keys_settle_seconds)
                proc = await asyncio.create_subprocess_exec(
                    'tmux', 'send-keys', '-t', session_name, 'Enter',
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)

                # Wait for commit picker
                await asyncio.sleep(branch_settle)

                # Select the first commit (most recent)
                proc = await asyncio.create_subprocess_exec(
                    'tmux', 'send-keys', '-t', session_name, 'Enter',
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
                logger.info(f"Sent commit review to {session_name}")

            return True

        except asyncio.TimeoutError:
            logger.error(f"Timeout sending review sequence to {session_name}")
            return False
        except Exception as e:
            logger.error(f"Failed to send review sequence to {session_name}: {e}")
            return False

    async def send_steer_text(self, session_name: str, text: str) -> bool:
        """
        Inject steer text into an active Codex turn via Enter key.

        Sends: Enter (open steer field) -> text -> Enter (submit).

        Args:
            session_name: Target tmux session
            text: Steer instructions to inject

        Returns:
            True if steer text sent successfully
        """
        if not self.session_exists(session_name):
            logger.error(f"Session {session_name} does not exist")
            return False

        try:
            # Press Enter to open steer input field
            proc = await asyncio.create_subprocess_exec(
                'tmux', 'send-keys', '-t', session_name, 'Enter',
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
            await asyncio.sleep(self.send_keys_settle_seconds)

            # Send the steer text
            proc = await asyncio.create_subprocess_exec(
                'tmux', 'send-keys', '-t', session_name, '--', text,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
            await asyncio.sleep(self.send_keys_settle_seconds)

            # Press Enter to submit
            proc = await asyncio.create_subprocess_exec(
                'tmux', 'send-keys', '-t', session_name, 'Enter',
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)

            logger.info(f"Sent steer text to {session_name}: {text[:50]}...")
            return True

        except asyncio.TimeoutError:
            logger.error(f"Timeout sending steer text to {session_name}")
            return False
        except Exception as e:
            logger.error(f"Failed to send steer text to {session_name}: {e}")
            return False

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
        # Escape session_name for AppleScript (prevent injection)
        import shlex
        escaped_session = shlex.quote(session_name)
        script = f'''
        tell application "Terminal"
            activate
            do script "tmux attach-session -t {escaped_session}"
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
