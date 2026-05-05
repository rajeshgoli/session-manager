"""tmux operations for spawning and controlling Claude Code sessions."""

import logging
import asyncio
import os
import shlex
import shutil
import subprocess
from pathlib import Path
from typing import Optional

logger = logging.getLogger(__name__)


class TmuxController:
    """Controls tmux sessions for Claude Code."""

    SERVER_ANCHOR_SESSION = "__sm_server_anchor"

    def __init__(self, log_dir: str = "/tmp/claude-sessions", config: Optional[dict] = None):
        self.log_dir = Path(log_dir)
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self.config = config or {}
        self.last_error_message: Optional[str] = None

        tmux_config = self.config.get("tmux", {}) if isinstance(self.config, dict) else {}
        raw_socket_name = str(tmux_config.get("socket_name", "") or "").strip()
        self.socket_name: Optional[str] = raw_socket_name or None
        self.native_scrollback = self._coerce_bool(
            tmux_config.get("native_scrollback"),
            default=bool(self.socket_name),
        )
        self.history_limit = self._coerce_positive_int(
            tmux_config.get("history_limit"),
            default=100000,
        )

        # Load timeout configuration with fallbacks
        timeouts = self.config.get("timeouts", {})
        tmux_timeouts = timeouts.get("tmux", {})

        self.shell_export_settle_seconds = tmux_timeouts.get("shell_export_settle_seconds", 0.1)
        self.claude_init_seconds = tmux_timeouts.get("claude_init_seconds", 3)
        self.claude_init_no_prompt_seconds = tmux_timeouts.get("claude_init_no_prompt_seconds", 1)
        self.send_keys_timeout_seconds = tmux_timeouts.get("send_keys_timeout_seconds", 5)
        self.send_keys_settle_seconds = tmux_timeouts.get("send_keys_settle_seconds", 0.3)
        self.send_keys_settle_max_seconds = tmux_timeouts.get("send_keys_settle_max_seconds", 0.9)
        self.send_keys_settle_per_ki_chars = tmux_timeouts.get("send_keys_settle_per_ki_chars", 0.06)
        self.send_keys_settle_per_extra_line = tmux_timeouts.get("send_keys_settle_per_extra_line", 0.015)
        self.send_keys_max_chunk_chars = int(tmux_timeouts.get("send_keys_max_chunk_chars", 4096))
        self.submit_verify_seconds = tmux_timeouts.get("submit_verify_seconds", 0.6)
        self.submit_retry_seconds = tmux_timeouts.get("submit_retry_seconds", 0.6)
        self.shell_fd_limit = int(tmux_timeouts.get("shell_fd_limit", 65536))

    @staticmethod
    def _coerce_bool(value: object, default: bool = False) -> bool:
        """Parse bool config values with common string/int forms."""
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

    @staticmethod
    def _coerce_positive_int(value: object, default: int) -> int:
        """Parse a positive integer config value."""
        try:
            parsed = int(value)
        except (TypeError, ValueError):
            return default
        return parsed if parsed > 0 else default

    @staticmethod
    def _normalize_session_target(session_name: str) -> str:
        """Return the tmux session portion of a target that may include window/pane suffixes."""
        return str(session_name or "").split(":", 1)[0].split(".", 1)[0]

    def tmux_cmd(self, *args: str, socket_name: Optional[str] = "__primary__") -> list[str]:
        """Build one tmux command using the configured SM socket unless overridden."""
        effective_socket = self.socket_name if socket_name == "__primary__" else socket_name
        cmd = ["tmux"]
        if effective_socket:
            cmd.extend(["-L", effective_socket])
        cmd.extend(args)
        return cmd

    def tmux_cmd_for_session(self, session_name: str, *args: str) -> list[str]:
        """Build a tmux command for an existing target, falling back for legacy sessions."""
        return self.tmux_cmd(*args, socket_name=self._resolve_socket_for_session(session_name))

    def _session_exists_on_socket(self, session_name: str, socket_name: Optional[str]) -> bool:
        target = self._normalize_session_target(session_name)
        if not target:
            return False
        result = subprocess.run(
            self.tmux_cmd("has-session", "-t", target, socket_name=socket_name),
            capture_output=True,
            text=True,
            check=False,
        )
        return result.returncode == 0

    def _resolve_socket_for_session(self, session_name: str) -> Optional[str]:
        """Resolve which tmux server owns a session, supporting legacy default-server panes."""
        target = self._normalize_session_target(session_name)
        if not target:
            return self.socket_name
        if self.socket_name and self._session_exists_on_socket(target, self.socket_name):
            return self.socket_name
        if self.socket_name and self._session_exists_on_socket(target, None):
            return None
        return self.socket_name

    def _ensure_server_options(self) -> None:
        """Apply SM-owned tmux server options that are safe only on the configured socket."""
        if not self.socket_name:
            return
        self._run_tmux(
            "set-option",
            "-g",
            "focus-events",
            "on",
            check=False,
        )
        if not self.native_scrollback:
            return
        current = self._run_tmux(
            "show-options",
            "-gqv",
            "terminal-overrides",
            check=False,
        )
        if "smcup@:rmcup@" in (current.stdout or ""):
            return
        self._run_tmux(
            "set-option",
            "-as",
            "terminal-overrides",
            ",*:smcup@:rmcup@",
        )

    def _ensure_server_anchor(self) -> None:
        """Start the configured tmux server under a neutral SM-owned session.

        tmux server processes keep the argv from the command that first created
        the server. If the first command is an agent session, `ps` can later
        make the server itself look like an orphaned agent runtime after that
        session is retired. Keep a neutral anchor so process-grep cleanup cannot
        accidentally kill every managed session on the socket.
        """
        if not self.socket_name:
            return
        if self._session_exists_on_socket(self.SERVER_ANCHOR_SESSION, self.socket_name):
            return
        result = self._run_tmux(
            "new-session",
            "-d",
            "-s",
            self.SERVER_ANCHOR_SESSION,
            "-n",
            "anchor",
            "-c",
            str(self.log_dir),
            "sleep 315360000",
            check=False,
        )
        if result.returncode == 0:
            logger.info(
                "Created tmux server anchor %s on socket %s",
                self.SERVER_ANCHOR_SESSION,
                self.socket_name,
            )
        elif "duplicate session" not in (result.stderr or "").lower():
            logger.warning("Could not create tmux server anchor: %s", result.stderr)

    def _enable_exit_diagnostics(self, session_name: str) -> None:
        """Keep dead panes around long enough for the monitor to read exit status."""
        try:
            self._run_tmux(
                "set-window-option",
                "-t",
                f"{session_name}:main",
                "remain-on-exit",
                "on",
                check=False,
            )
            pane_id = (
                self._run_tmux(
                    "display-message",
                    "-p",
                    "-t",
                    session_name,
                    "#{pane_id}",
                    check=False,
                ).stdout
                or ""
            ).strip()
            if pane_id:
                self._run_tmux(
                    "set-option",
                    "-t",
                    session_name,
                    "@sm_main_pane_id",
                    pane_id,
                    check=False,
                )
        except Exception:
            # Exit diagnostics are best-effort. Spawn should not fail if tmux
            # lacks this option or the pane disappeared during setup.
            logger.debug("Could not enable remain-on-exit for %s", session_name, exc_info=True)

    def _list_sessions_on_socket(self, socket_name: Optional[str]) -> list[str]:
        """Return tmux session names on one socket without using fallback resolution."""
        result = self._run_tmux(
            "list-sessions",
            "-F",
            "#{session_name}",
            check=False,
            socket_name=socket_name,
        )
        if result.returncode != 0:
            return []
        return [
            line.strip()
            for line in (result.stdout or "").splitlines()
            if line.strip() and line.strip() != self.SERVER_ANCHOR_SESSION
        ]

    def get_session_exit_diagnostics(self, session_name: str) -> dict[str, object]:
        """Collect tmux lifecycle diagnostics for a managed session.

        If the session still exists with a dead pane, this captures the provider
        exit status. If tmux has already removed it, this returns socket-level
        snapshots so logs can distinguish a missing SM socket session from a
        legacy/default-socket mismatch.
        """
        target = self._normalize_session_target(session_name)
        socket_name = self._resolve_socket_for_session(target)
        exists = self._session_exists_on_socket(target, socket_name)
        if not exists and self.socket_name:
            legacy_exists = self._session_exists_on_socket(target, None)
            if legacy_exists:
                socket_name = None
                exists = True

        diagnostics: dict[str, object] = {
            "session_name": target,
            "socket_name": socket_name,
            "configured_socket_name": self.socket_name,
            "exists": exists,
            "pane_dead": False,
            "panes": [],
            "sessions_on_configured_socket": self._list_sessions_on_socket(self.socket_name),
            "sessions_on_default_socket": self._list_sessions_on_socket(None),
        }

        if not exists:
            return diagnostics

        main_pane_id = (
            self._run_tmux(
                "show-options",
                "-qv",
                "-t",
                target,
                "@sm_main_pane_id",
                check=False,
                socket_name=socket_name,
            ).stdout
            or ""
        ).strip() or None
        diagnostics["main_pane_id"] = main_pane_id

        result = self._run_tmux(
            "list-panes",
            "-t",
            target,
            "-F",
            "#{pane_id}\t#{pane_dead}\t#{pane_dead_status}\t#{pane_dead_signal}\t#{pane_current_command}\t#{pane_pid}\t#{pane_tty}\t#{pane_active}\t#{pane_title}",
            check=False,
            socket_name=socket_name,
        )
        if result.returncode != 0:
            diagnostics["pane_error"] = (result.stderr or "").strip()
            return diagnostics

        panes: list[dict[str, object]] = []
        for line in (result.stdout or "").splitlines():
            parts = line.split("\t")
            parts += [""] * (9 - len(parts))
            pane = {
                "pane_id": parts[0],
                "pane_dead": parts[1] == "1",
                "pane_dead_status": parts[2] or None,
                "pane_dead_signal": parts[3] or None,
                "pane_current_command": parts[4] or None,
                "pane_pid": parts[5] or None,
                "pane_tty": parts[6] or None,
                "pane_active": parts[7] == "1",
                "pane_title": parts[8] or None,
            }
            panes.append(pane)

        diagnostics["panes"] = panes
        dead_panes = [pane for pane in panes if pane.get("pane_dead")]
        diagnostics["dead_panes"] = dead_panes
        if main_pane_id:
            dead_pane = next(
                (
                    pane
                    for pane in dead_panes
                    if pane.get("pane_id") == main_pane_id
                ),
                None,
            )
        else:
            live_panes = [pane for pane in panes if not pane.get("pane_dead")]
            dead_pane = dead_panes[0] if dead_panes and not live_panes else None
        if dead_pane:
            diagnostics["pane_dead"] = True
            diagnostics.update(dead_pane)
        elif panes:
            main_pane = next(
                (pane for pane in panes if pane.get("pane_id") == main_pane_id),
                None,
            )
            diagnostics.update(main_pane or panes[0])
        return diagnostics

    def _resolve_launch_command(
        self,
        command: str,
        *,
        working_path: Path,
    ) -> tuple[Optional[str], Optional[str]]:
        """Resolve and validate one CLI launch command before creating tmux state."""
        raw_command = str(command or "").strip()
        if not raw_command:
            return None, "Launch command is empty"

        if raw_command.startswith("~") or "/" in raw_command:
            candidate = Path(raw_command).expanduser()
            if not candidate.is_absolute():
                candidate = (working_path / candidate).resolve()
            else:
                candidate = candidate.resolve()
            if not candidate.exists():
                return None, f"Launch command does not exist: {candidate}"
            if not candidate.is_file():
                return None, f"Launch command is not a file: {candidate}"
            if not os.access(candidate, os.X_OK):
                return None, f"Launch command is not executable: {candidate}"
            return str(candidate), None

        if shutil.which(raw_command) is None:
            return None, f"Launch command not found on PATH: {raw_command}"
        return raw_command, None

    def _run_tmux(
        self,
        *args: str,
        check: bool = True,
        timeout: Optional[float] = None,
        socket_name: Optional[str] = "__primary__",
    ) -> subprocess.CompletedProcess:
        """Run a tmux command."""
        cmd = self.tmux_cmd(*args, socket_name=socket_name)
        logger.debug(f"Running tmux command: {' '.join(cmd)}")
        return subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            check=check,
            timeout=timeout,
        )

    def _run_tmux_for_session(
        self,
        session_name: str,
        *args: str,
        check: bool = True,
        timeout: Optional[float] = None,
    ) -> subprocess.CompletedProcess:
        """Run a tmux command against an existing target, with legacy fallback."""
        cmd = self.tmux_cmd_for_session(session_name, *args)
        logger.debug(f"Running tmux command: {' '.join(cmd)}")
        return subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            check=check,
            timeout=timeout,
        )

    def _compute_settle_delay_seconds(self, text: str) -> float:
        """Compute adaptive settle delay before Enter to reduce paste-mode races."""
        base = float(self.send_keys_settle_seconds)
        max_delay = max(base, float(self.send_keys_settle_max_seconds))
        text_len = len(text or "")
        line_count = (text or "").count("\n") + 1
        if text_len <= 512 and line_count <= 1:
            return base
        extra = (
            (max(0, text_len - 512) / 1024.0) * float(self.send_keys_settle_per_ki_chars)
            + max(0, line_count - 1) * float(self.send_keys_settle_per_extra_line)
        )
        return max(base, min(max_delay, base + extra))

    def _split_send_text_chunks(self, text: str) -> list[str]:
        """Split long send-keys payloads into bounded chunks to avoid argv limits."""
        payload = text or ""
        max_chunk_chars = max(1, int(self.send_keys_max_chunk_chars or 1))
        if len(payload) <= max_chunk_chars:
            return [payload]

        chunks: list[str] = []
        remaining = payload
        while remaining:
            if len(remaining) <= max_chunk_chars:
                chunks.append(remaining)
                break

            split_at = max_chunk_chars
            newline_idx = remaining.rfind("\n", 0, max_chunk_chars)
            if newline_idx >= max_chunk_chars // 2:
                split_at = newline_idx + 1

            chunks.append(remaining[:split_at])
            remaining = remaining[split_at:]

        return chunks

    def _get_pane_in_mode(self, session_name: str) -> Optional[int]:
        """Return tmux pane_in_mode (1 copy-mode, 0 normal) for active pane."""
        try:
            result = self._run_tmux_for_session(
                session_name,
                "display-message", "-p", "-t", session_name, "#{pane_in_mode}",
                check=False,
            )
            if result.returncode != 0:
                return None
            raw = (result.stdout or "").strip()
            return int(raw) if raw in {"0", "1"} else None
        except Exception:
            return None

    def get_pane_title(self, session_name: str) -> Optional[str]:
        """Return the active pane title for one tmux session."""
        try:
            result = self._run_tmux_for_session(
                session_name,
                "display-message", "-p", "-t", session_name, "#{pane_title}",
                check=False,
            )
            if result.returncode != 0:
                return None
            title = (result.stdout or "").strip()
            return title or None
        except Exception:
            return None

    def _initialize_pane_title(self, session_name: str) -> None:
        """Seed a fresh pane with a neutral title before provider-specific updates arrive."""
        try:
            self._run_tmux(
                "select-pane",
                "-t", session_name,
                "-T", session_name,
                check=False,
            )
        except Exception:
            # Non-fatal: pane title is best-effort only.
            pass

    def _prepare_managed_shell(self, session_name: str, session_id: Optional[str]) -> None:
        """Set shell state inherited by Claude/Codex before launching the provider."""
        if self.shell_fd_limit > 0:
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                f"ulimit -n {self.shell_fd_limit}",
                "Enter",
            )

        self._run_tmux(
            "send-keys",
            "-t", session_name,
            "unset NO_COLOR",
            "Enter",
        )
        color_env = {
            "TERM_PROGRAM": os.environ.get("TERM_PROGRAM"),
            "TERM_PROGRAM_VERSION": os.environ.get("TERM_PROGRAM_VERSION"),
            "COLORTERM": os.environ.get("COLORTERM"),
            "CLICOLOR": os.environ.get("CLICOLOR"),
            "CLICOLOR_FORCE": os.environ.get("CLICOLOR_FORCE"),
            "FORCE_COLOR": os.environ.get("FORCE_COLOR"),
        }
        for name, value in color_env.items():
            if value:
                color_cmd = f"export {name}={shlex.quote(value)}"
            else:
                color_cmd = f"unset {name}"
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                color_cmd,
                "Enter",
            )

        # Claude Code can leave this behind after exits; managed tmux panes are independent
        # launches, so nested-session detection should not apply.
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
            # Export session ID so it persists even if user exits and restarts the provider.
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                f"export CLAUDE_SESSION_MANAGER_ID={session_id}",
                "Enter",
            )

    def _exit_copy_mode_if_needed(self, session_name: str) -> tuple[Optional[int], Optional[int]]:
        """Exit tmux copy-mode on active pane when present."""
        before = self._get_pane_in_mode(session_name)
        if before != 1:
            return before, before
        try:
            self._run_tmux_for_session(session_name, "send-keys", "-t", session_name, "-X", "cancel", check=False)
        except Exception:
            # Non-fatal: we still attempt delivery.
            pass
        after = self._get_pane_in_mode(session_name)
        return before, after

    def _clear_pending_input(self, session_name: str) -> bool:
        """Best-effort clear of partially typed input after a failed send."""
        try:
            subprocess.run(
                self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "C-u"),
                check=True,
                capture_output=True,
                text=True,
                timeout=self.send_keys_timeout_seconds,
            )
            return True
        except Exception as exc:
            logger.warning("Failed to clear pending input for %s after send failure: %s", session_name, exc)
            return False

    async def _get_pane_in_mode_async(self, session_name: str) -> Optional[int]:
        """Async variant of pane mode query."""
        try:
            proc = await asyncio.create_subprocess_exec(
                *self.tmux_cmd_for_session(session_name, "display-message", "-p", "-t", session_name, "#{pane_in_mode}"),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(
                proc.communicate(), timeout=self.send_keys_timeout_seconds
            )
            if proc.returncode != 0:
                return None
            raw = stdout.decode(errors="ignore").strip()
            return int(raw) if raw in {"0", "1"} else None
        except Exception:
            return None

    async def _exit_copy_mode_if_needed_async(self, session_name: str) -> tuple[Optional[int], Optional[int]]:
        """Async variant of copy-mode exit."""
        before = await self._get_pane_in_mode_async(session_name)
        if before != 1:
            return before, before
        try:
            proc = await asyncio.create_subprocess_exec(
                *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "-X", "cancel"),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=self.send_keys_timeout_seconds)
        except Exception:
            # Non-fatal: continue with send path.
            pass
        after = await self._get_pane_in_mode_async(session_name)
        return before, after

    async def _clear_pending_input_async(self, session_name: str) -> bool:
        """Best-effort clear of partially typed input after a failed send."""
        try:
            proc = await asyncio.create_subprocess_exec(
                *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "C-u"),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=self.send_keys_timeout_seconds)
            return proc.returncode == 0
        except Exception as exc:
            logger.warning("Failed to clear pending input for %s after send failure: %s", session_name, exc)
            return False

    async def _capture_pane_async(self, session_name: str) -> Optional[str]:
        """Capture the full active tmux pane asynchronously."""
        try:
            proc = await asyncio.create_subprocess_exec(
                *self.tmux_cmd_for_session(session_name, "capture-pane", "-p", "-J", "-S", "-200", "-t", session_name),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(
                proc.communicate(), timeout=self.send_keys_timeout_seconds
            )
            if proc.returncode != 0:
                return None
            return stdout.decode(errors="ignore")
        except Exception:
            return None

    def _extract_active_claude_composer_text(self, pane_text: Optional[str]) -> Optional[str]:
        """Extract the current Claude composer text from the bottom prompt block."""
        if not pane_text:
            return None

        lines = pane_text.splitlines()
        separator_indexes = [
            i for i, line in enumerate(lines)
            if line.strip() and set(line.strip()) == {"─"}
        ]
        if len(separator_indexes) < 2:
            return None

        block = lines[separator_indexes[-2] + 1:separator_indexes[-1]]
        if not block:
            return None

        prompt_index = None
        for idx, line in enumerate(block):
            stripped = line.lstrip()
            if stripped.startswith("❯") or stripped.startswith(">"):
                prompt_index = idx
                break
        if prompt_index is None:
            return None

        prompt_line = block[prompt_index].lstrip()
        if prompt_line.startswith("❯"):
            current = prompt_line[1:].lstrip()
        else:
            current = prompt_line[1:].lstrip()

        continuation: list[str] = []
        for line in block[prompt_index + 1:]:
            if line.lstrip().startswith("❯") or line.lstrip().startswith(">"):
                break
            continuation.append(line.strip())

        parts = [current] if current else []
        parts.extend(part for part in continuation if part)
        composer = " ".join(parts).strip()
        return composer or None

    def _looks_like_queued_message_placeholder(self, composer_text: Optional[str]) -> bool:
        """Return True for Claude's local queued-message editor prompt."""
        if not composer_text:
            return False
        lowered = composer_text.lower()
        return "queued messages" in lowered or "queued message" in lowered

    def _looks_like_codex_deferred_send_banner(self, pane_text: Optional[str]) -> bool:
        """Return True when Codex parked a message behind its deferred-send banner."""
        if not pane_text:
            return False
        lowered = pane_text.lower()
        return "submitted after next tool call" in lowered

    def _looks_like_codex_rename_prompt(self, pane_text: Optional[str]) -> bool:
        """Return True when Codex is asking for a thread name."""
        if not pane_text:
            return False
        lowered = pane_text.lower()
        return (
            ("name thread" in lowered or "rename thread" in lowered)
            and "press enter to confirm" in lowered
        )

    def _extract_active_codex_region(self, pane_text: Optional[str]) -> Optional[str]:
        """Return the active bottom-of-pane Codex region near the live prompt/banner."""
        if not pane_text:
            return None

        lines = pane_text.splitlines()
        if not lines:
            return None

        prompt_indexes = [index for index, line in enumerate(lines) if line.lstrip().startswith("›")]
        if prompt_indexes:
            prompt_index = prompt_indexes[-1]
            start = max(0, prompt_index - 8)
            end = min(len(lines), prompt_index + 4)
            region = "\n".join(lines[start:end]).strip()
            return region or None

        region = "\n".join(lines[-16:]).strip()
        return region or None

    def _extract_active_codex_prompt_region(self, pane_text: Optional[str]) -> Optional[str]:
        """Return only the live Codex prompt/dialog region, excluding stale scrollback."""
        if not pane_text:
            return None

        lines = pane_text.splitlines()
        if not lines:
            return None

        prompt_indexes = [index for index, line in enumerate(lines) if line.lstrip().startswith("›")]
        if prompt_indexes:
            prompt_index = prompt_indexes[-1]
            end = min(len(lines), prompt_index + 4)
            region = "\n".join(lines[prompt_index:end]).strip()
            return region or None

        region = "\n".join(lines[-8:]).strip()
        return region or None

    def _normalize_for_compare(self, text: str) -> str:
        """Collapse whitespace for approximate pane-vs-payload comparison."""
        return " ".join((text or "").split())

    async def _verify_claude_submit_async(self, session_name: str, text: str) -> bool:
        """Retry Enter once if Claude still shows the unsent payload in the composer."""
        normalized_text = self._normalize_for_compare(text)
        if not normalized_text:
            return False

        async def _capture_matching_composer() -> Optional[str]:
            pane = await self._capture_pane_async(session_name)
            composer = self._extract_active_claude_composer_text(pane)
            if not composer or self._looks_like_queued_message_placeholder(composer):
                return None
            normalized_composer = self._normalize_for_compare(composer)
            if (
                normalized_composer
                and (
                    normalized_text.startswith(normalized_composer)
                    or normalized_composer in normalized_text
                )
            ):
                return composer
            return None

        await asyncio.sleep(self.submit_verify_seconds)
        first_match = await _capture_matching_composer()
        if not first_match:
            return False

        await asyncio.sleep(self.submit_retry_seconds)
        second_match = await _capture_matching_composer()
        if not second_match:
            return False

        proc = await asyncio.create_subprocess_exec(
            *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Enter"),
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await asyncio.wait_for(
            proc.communicate(), timeout=self.send_keys_timeout_seconds
        )
        if proc.returncode != 0:
            logger.error(f"Failed to resend Enter during Claude submit verification: {stderr.decode()}")
            return False

        logger.warning(
            "Claude submit verification resent Enter for %s after composer stayed populated",
            session_name,
        )
        return True

    async def _verify_codex_submit_async(self, session_name: str, text: str) -> bool:
        """Interrupt once if Codex parked the payload behind its deferred-send banner."""
        normalized_text = self._normalize_for_compare(text)
        if not normalized_text:
            return False
        normalized_preview = normalized_text[:120]

        async def _capture_deferred_payload() -> bool:
            pane = await self._capture_pane_async(session_name)
            active_region = self._extract_active_codex_region(pane)
            if not self._looks_like_codex_deferred_send_banner(active_region):
                return False
            normalized_pane = self._normalize_for_compare(active_region or "")
            return normalized_preview in normalized_pane

        await asyncio.sleep(self.submit_verify_seconds)
        first_match = await _capture_deferred_payload()
        if not first_match:
            return False

        await asyncio.sleep(self.submit_retry_seconds)
        second_match = await _capture_deferred_payload()
        if not second_match:
            return False

        proc = await asyncio.create_subprocess_exec(
            *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Escape"),
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await asyncio.wait_for(
            proc.communicate(), timeout=self.send_keys_timeout_seconds
        )
        if proc.returncode != 0:
            logger.error(f"Failed to send Escape during Codex submit verification: {stderr.decode()}")
            return False

        logger.warning(
            "Codex submit verification sent Escape for %s after deferred-send banner persisted",
            session_name,
        )
        return True

    def session_exists(self, session_name: str) -> bool:
        """Check if a tmux session exists."""
        if self._session_exists_on_socket(session_name, self.socket_name):
            return True
        if self.socket_name and self._session_exists_on_socket(session_name, None):
            return True
        return False

    def get_history_limit(self, session_name: str) -> Optional[int]:
        """Return the active pane history limit for a tmux session."""
        try:
            result = self._run_tmux_for_session(
                session_name,
                "display-message",
                "-p",
                "-t", session_name,
                "#{history_limit}",
                check=False,
            )
            if result.returncode != 0:
                return None
            raw = (result.stdout or "").strip()
            return int(raw) if raw.isdigit() else None
        except Exception:
            return None

    def set_status_bar(
        self,
        session_name: str,
        friendly_name: str,
        *,
        timeout_seconds: Optional[float] = None,
    ) -> bool:
        """
        Update tmux status bar to show friendly name.

        Args:
            session_name: tmux session name
            friendly_name: User-friendly name to display
            timeout_seconds: Optional tmux command timeout

        Returns:
            True if successful
        """
        if not self.session_exists(session_name):
            logger.warning(f"Session {session_name} does not exist")
            return False

        try:
            # Set status-left to show friendly name
            self._run_tmux_for_session(
                session_name,
                "set-option",
                "-t", session_name,
                "status-left",
                f"[{friendly_name}] ",
                timeout=timeout_seconds,
            )
            logger.info(f"Updated status bar for {session_name} to show '{friendly_name}'")
            return True
        except subprocess.TimeoutExpired:
            logger.warning(
                "Timed out setting status bar for %s after %.2fs",
                session_name,
                timeout_seconds if timeout_seconds is not None else 0.0,
            )
            return False
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
        self.last_error_message = None
        if self.session_exists(session_name):
            logger.warning(f"Session {session_name} already exists")
            self.last_error_message = f"Tmux session already exists: {session_name}"
            return False

        working_path = Path(working_dir).expanduser().resolve()
        if not working_path.exists():
            self.last_error_message = f"Working directory does not exist: {working_dir}"
            logger.error(self.last_error_message)
            return False

        launch_command, command_error = self._resolve_launch_command(
            "claude",
            working_path=working_path,
        )
        if command_error:
            self.last_error_message = command_error
            logger.error(command_error)
            return False

        # Ensure log file parent directory exists
        log_path = Path(log_file)
        log_path.parent.mkdir(parents=True, exist_ok=True)

        # Touch the log file
        log_path.touch()

        try:
            self._ensure_server_anchor()
            # Create bootstrap session, then create provider window after history-limit is set.
            self._run_tmux(
                "new-session",
                "-d",
                "-s", session_name,
                "-c", str(working_path),
                "-n", "__sm_bootstrap",
            )
            self._ensure_server_options()
            self._run_tmux("set-option", "-t", session_name, "history-limit", str(self.history_limit))
            self._run_tmux("new-window", "-d", "-t", session_name, "-n", "main", "-c", str(working_path))
            self._run_tmux("kill-window", "-t", f"{session_name}:__sm_bootstrap")
            self._run_tmux("select-window", "-t", f"{session_name}:main")
            self._enable_exit_diagnostics(session_name)
            self._initialize_pane_title(session_name)

            # Set up pipe-pane to capture output to log file
            self._run_tmux(
                "pipe-pane",
                "-t", session_name,
                f"cat >> {log_file}",
            )

            self._prepare_managed_shell(session_name, session_id)

            # Small delay to ensure exports complete
            import time
            time.sleep(self.shell_export_settle_seconds)

            # Start Claude Code in the session
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                launch_command,
                "Enter",
            )

            self.last_error_message = None
            logger.info(f"Created session {session_name} (id={session_id}) in {working_dir}")
            return True

        except subprocess.CalledProcessError as e:
            self.last_error_message = f"Failed to create tmux session: {e.stderr}"
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
        self.last_error_message = None
        if self.session_exists(session_name):
            logger.warning(f"Session {session_name} already exists")
            self.last_error_message = f"Tmux session already exists: {session_name}"
            return False

        working_path = Path(working_dir).expanduser().resolve()
        if not working_path.exists():
            self.last_error_message = f"Working directory does not exist: {working_dir}"
            logger.error(self.last_error_message)
            return False

        launch_command, command_error = self._resolve_launch_command(
            command,
            working_path=working_path,
        )
        if command_error:
            self.last_error_message = command_error
            logger.error(command_error)
            return False

        # Ensure log file parent directory exists
        log_path = Path(log_file)
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_path.touch()

        try:
            self._ensure_server_anchor()
            # Create bootstrap session, then create provider window after history-limit is set.
            self._run_tmux(
                "new-session",
                "-d",
                "-s", session_name,
                "-c", str(working_path),
                "-n", "__sm_bootstrap",
            )
            self._ensure_server_options()
            self._run_tmux("set-option", "-t", session_name, "history-limit", str(self.history_limit))
            self._run_tmux("new-window", "-d", "-t", session_name, "-n", "main", "-c", str(working_path))
            self._run_tmux("kill-window", "-t", f"{session_name}:__sm_bootstrap")
            self._run_tmux("select-window", "-t", f"{session_name}:main")
            self._enable_exit_diagnostics(session_name)
            self._initialize_pane_title(session_name)

            # Set up pipe-pane to capture output to log file
            self._run_tmux(
                "pipe-pane",
                "-t", session_name,
                f"cat >> {log_file}",
            )

            self._prepare_managed_shell(session_name, session_id)

            # Small delay to ensure exports complete
            import time
            time.sleep(self.shell_export_settle_seconds)

            # Build Claude command with args and model
            cmd_parts = [launch_command]
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

            launch_command = " ".join(cmd_parts)
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                "--",
                launch_command,
            )
            time.sleep(self._compute_settle_delay_seconds(launch_command))
            self._run_tmux(
                "send-keys",
                "-t", session_name,
                "Enter",
            )

            if initial_prompt:
                logger.info(f"Created session with CLI prompt for {session_name} (prompt_len={len(initial_prompt)})")
            else:
                import time
                time.sleep(self.claude_init_no_prompt_seconds)

            # Log command without prompt payload to avoid leaking sensitive content
            log_parts = [p for p in cmd_parts if p != "--" and p != shlex.quote(initial_prompt)] if initial_prompt else cmd_parts
            self.last_error_message = None
            logger.info(f"Created child session {session_name} (id={session_id}) with command {' '.join(log_parts)}")
            return True

        except subprocess.CalledProcessError as e:
            self.last_error_message = f"Failed to create tmux session: {e.stderr}"
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
            mode_before, mode_after = self._exit_copy_mode_if_needed(session_name)
            settle_delay = self._compute_settle_delay_seconds(text)
            chunks = self._split_send_text_chunks(text)
            text_injected = False
            # Use subprocess with list arguments to prevent shell injection
            for chunk in chunks:
                subprocess.run(
                    self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "-l", "--", chunk),
                    check=True,
                    capture_output=True,
                    text=True,
                    timeout=self.send_keys_timeout_seconds
                )
                text_injected = True
            time.sleep(settle_delay)
            subprocess.run(
                self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Enter"),
                check=True,
                capture_output=True,
                text=True,
                timeout=self.send_keys_timeout_seconds
            )
            line_count = (text or "").count("\n") + 1
            if mode_before == 1 or settle_delay > self.send_keys_settle_seconds:
                logger.info(
                    "Sent input to %s (copy_mode %s->%s, settle=%.3fs, len=%d, lines=%d): %s...",
                    session_name,
                    mode_before,
                    mode_after,
                    settle_delay,
                    len(text or ""),
                    line_count,
                    (text or "")[:50],
                )
            else:
                logger.debug(f"Sent input to {session_name}: {(text or '')[:50]}...")
            return True

        except subprocess.CalledProcessError as e:
            if "text_injected" in locals() and text_injected:
                self._clear_pending_input(session_name)
            logger.error(f"Failed to send input: {e.stderr}")
            return False
        except subprocess.TimeoutExpired:
            if "text_injected" in locals() and text_injected:
                self._clear_pending_input(session_name)
            logger.error(f"Timeout sending input to {session_name}")
            return False

    async def send_input_async(
        self,
        session_name: str,
        text: str,
        verify_claude_submit: bool = False,
        verify_codex_submit: bool = False,
    ) -> bool:
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
            mode_before, mode_after = await self._exit_copy_mode_if_needed_async(session_name)
            settle_delay = self._compute_settle_delay_seconds(text)
            chunks = self._split_send_text_chunks(text)
            text_injected = False

            for chunk in chunks:
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "-l", "--", chunk),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                stdout, stderr = await asyncio.wait_for(
                    proc.communicate(), timeout=self.send_keys_timeout_seconds
                )
                if proc.returncode != 0:
                    if text_injected:
                        await self._clear_pending_input_async(session_name)
                    logger.error(f"Failed to send text: {stderr.decode()}")
                    return False
                text_injected = True

            # Settle delay to avoid paste detection (#178)
            # Claude Code (Node.js TUI in raw mode) treats a rapid character burst
            # as pasted text, in which \r is a literal byte not a submit command.
            # The gap lets paste mode end before Enter arrives as a separate event.
            await asyncio.sleep(settle_delay)

            # Send Enter as a separate keystroke
            proc = await asyncio.create_subprocess_exec(
                *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Enter"),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await asyncio.wait_for(
                proc.communicate(), timeout=self.send_keys_timeout_seconds
            )
            if proc.returncode != 0:
                if text_injected:
                    await self._clear_pending_input_async(session_name)
                logger.error(f"Failed to send Enter: {stderr.decode()}")
                return False

            if verify_claude_submit:
                await self._verify_claude_submit_async(session_name, text)
            if verify_codex_submit:
                await self._verify_codex_submit_async(session_name, text)

            line_count = (text or "").count("\n") + 1
            if mode_before == 1 or settle_delay > self.send_keys_settle_seconds:
                logger.info(
                    "Sent input (async) to %s (copy_mode %s->%s, settle=%.3fs, len=%d, lines=%d): %s...",
                    session_name,
                    mode_before,
                    mode_after,
                    settle_delay,
                    len(text or ""),
                    line_count,
                    (text or "")[:50],
                )
            else:
                logger.debug(f"Sent input (async) to {session_name}: {(text or '')[:50]}...")
            return True

        except asyncio.TimeoutError:
            if "text_injected" in locals() and text_injected:
                await self._clear_pending_input_async(session_name)
            logger.error(f"Timeout sending input to {session_name}")
            return False
        except Exception as e:
            if "text_injected" in locals() and text_injected:
                await self._clear_pending_input_async(session_name)
            logger.error(f"Failed to send input: {e}")
            return False

    async def send_key_async(self, session_name: str, key: str) -> bool:
        """
        Send a single key to a tmux session asynchronously.

        Args:
            session_name: Target session name
            key: Key to send, e.g. "Escape" or "C-b"

        Returns:
            True if key sent successfully
        """
        try:
            proc = await asyncio.create_subprocess_exec(
                *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, key),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            _, stderr = await asyncio.wait_for(
                proc.communicate(), timeout=self.send_keys_timeout_seconds
            )
            if proc.returncode != 0:
                logger.error(f"Failed to send key async: {stderr.decode()}")
                return False
            logger.info(f"Sent key to {session_name} (async): {key}")
            return True
        except asyncio.TimeoutError:
            logger.error(f"Timeout sending key {key} to {session_name}")
            return False
        except Exception as e:
            logger.error(f"Failed to send key async: {e}")
            return False

    async def background_claude_task_async(self, session_name: str) -> bool:
        """Send Claude's background-task keybinding to a tmux session."""
        return await self.send_key_async(session_name, "C-b")

    async def rename_codex_thread_async(self, session_name: str, friendly_name: str) -> bool:
        """Drive Codex's interactive /rename dialog using tmux keystrokes."""
        if not self.session_exists(session_name):
            logger.error(f"Session {session_name} does not exist")
            return False

        try:
            await self._exit_copy_mode_if_needed_async(session_name)
            if not await self.send_key_async(session_name, "C-u"):
                return False
            if not await self.send_input_async(session_name, "/rename"):
                return False

            prompt_seen = False
            deadline = asyncio.get_running_loop().time() + 5.0
            while asyncio.get_running_loop().time() < deadline:
                pane_text = await self._capture_pane_async(session_name)
                active_region = self._extract_active_codex_prompt_region(pane_text)
                if self._looks_like_codex_rename_prompt(active_region):
                    prompt_seen = True
                    break
                await asyncio.sleep(0.2)
            if not prompt_seen:
                logger.error(f"Codex rename prompt did not appear for {session_name}")
                return False

            if not await self.send_key_async(session_name, "C-u"):
                return False
            if not await self.send_input_async(session_name, friendly_name):
                return False
            return True
        except Exception as exc:
            logger.error(f"Failed to rename Codex thread for {session_name}: {exc}")
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
            self._run_tmux_for_session(session_name, "kill-session", "-t", session_name)
            logger.info(f"Killed session {session_name}")
            return True

        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to kill session: {e.stderr}")
            return False

    def list_sessions(self) -> list[str]:
        """List all tmux sessions."""
        sessions: set[str] = set()
        result = self._run_tmux("list-sessions", "-F", "#{session_name}", check=False)
        if result.returncode != 0:
            result = None
        if result is not None:
            sessions.update(
                s.strip()
                for s in result.stdout.strip().split("\n")
                if s.strip() and s.strip() != self.SERVER_ANCHOR_SESSION
            )
        if self.socket_name:
            legacy = self._run_tmux(
                "list-sessions",
                "-F",
                "#{session_name}",
                check=False,
                socket_name=None,
            )
            if legacy.returncode == 0:
                sessions.update(
                    s.strip()
                    for s in legacy.stdout.strip().split("\n")
                    if s.strip() and s.strip() != self.SERVER_ANCHOR_SESSION
                )
        return sorted(sessions)

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
            result = self._run_tmux_for_session(
                session_name,
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
                    *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "--", review_text),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
                await asyncio.sleep(self.send_keys_settle_seconds)
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Enter"),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
                logger.info(f"Sent custom review to {session_name}")
                return True

            # All other modes: send /review + Enter, then navigate menu
            proc = await asyncio.create_subprocess_exec(
                *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "--", "/review"),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
            await asyncio.sleep(self.send_keys_settle_seconds)

            proc = await asyncio.create_subprocess_exec(
                *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Enter"),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)

            # Wait for menu to appear
            await asyncio.sleep(menu_settle)

            if mode == "branch":
                # 1st menu item — just press Enter
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Enter"),
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
                            *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Down"),
                            stdout=asyncio.subprocess.PIPE,
                            stderr=asyncio.subprocess.PIPE,
                        )
                        await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)

                # Confirm branch selection
                await asyncio.sleep(self.send_keys_settle_seconds)
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Enter"),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
                logger.info(f"Sent branch review to {session_name} (position={branch_position})")

            elif mode == "uncommitted":
                # 2nd menu item — Down then Enter
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Down"),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
                await asyncio.sleep(self.send_keys_settle_seconds)
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Enter"),
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
                        *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Down"),
                        stdout=asyncio.subprocess.PIPE,
                        stderr=asyncio.subprocess.PIPE,
                    )
                    await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
                await asyncio.sleep(self.send_keys_settle_seconds)
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Enter"),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)

                # Wait for commit picker
                await asyncio.sleep(branch_settle)

                # Select the first commit (most recent)
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Enter"),
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
                *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Enter"),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
            await asyncio.sleep(self.send_keys_settle_seconds)

            # Send the steer text
            proc = await asyncio.create_subprocess_exec(
                *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "--", text),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.wait(), timeout=self.send_keys_timeout_seconds)
            await asyncio.sleep(self.send_keys_settle_seconds)

            # Press Enter to submit
            proc = await asyncio.create_subprocess_exec(
                *self.tmux_cmd_for_session(session_name, "send-keys", "-t", session_name, "Enter"),
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
        script = f'''
        tell application "Terminal"
            activate
            do script "{' '.join(shlex.quote(part) for part in self.tmux_cmd_for_session(session_name, 'attach-session', '-t', session_name))}"
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
