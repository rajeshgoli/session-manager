"""Background supervisor for local sidecar infrastructure."""

from __future__ import annotations

import copy
import logging
import os
import shutil
import socket
import subprocess
import threading
import time
from pathlib import Path
from typing import Any, Optional

logger = logging.getLogger(__name__)


class InfrastructureSupervisor:
    """Best-effort repair loop for local sidecars SM depends on."""

    def __init__(self, config: Optional[dict] = None):
        self.config = config or {}
        supervisor_config = self.config.get("infra_supervisor", {})
        self.enabled = bool(supervisor_config.get("enabled", True))
        self.check_interval = max(10, int(supervisor_config.get("check_interval_seconds", 30)))
        self._stop_event = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._lock = threading.Lock()
        self._results: dict[str, dict[str, Any]] = {}

        self._home = Path.home()
        self._uid = os.getuid()

        self._android_sshd_label = str(
            supervisor_config.get("android_sshd", {}).get("launch_agent_label", "com.rajesh.sm-android-sshd")
        )
        self._android_sshd_plist = Path(
            supervisor_config.get("android_sshd", {}).get(
                "launch_agent_plist",
                str(self._home / "Library/LaunchAgents/com.rajesh.sm-android-sshd.plist"),
            )
        ).expanduser()
        self._android_sshd_config = Path(
            supervisor_config.get("android_sshd", {}).get(
                "config_path",
                str(self._home / ".local/share/session-manager/android-sshd/sshd_config"),
            )
        ).expanduser()

        self._caffeinate_label = str(
            supervisor_config.get("ac_caffeinate", {}).get("launch_agent_label", "com.rajesh.sm-ac-caffeinate")
        )
        self._caffeinate_plist = Path(
            supervisor_config.get("ac_caffeinate", {}).get(
                "launch_agent_plist",
                str(self._home / "Library/LaunchAgents/com.rajesh.sm-ac-caffeinate.plist"),
            )
        ).expanduser()

        self._tmux_base_session = str(
            supervisor_config.get("tmux", {}).get("base_session", "base")
        ).strip() or "base"

    def start(self) -> None:
        """Start the repair loop and run an immediate pass."""
        if not self.enabled:
            logger.info("Infrastructure supervisor disabled")
            return
        self.ensure_now()
        self._thread = threading.Thread(target=self._loop, daemon=True, name="infra-supervisor")
        self._thread.start()
        logger.info("Infrastructure supervisor started (check every %ss)", self.check_interval)

    def stop(self) -> None:
        """Stop the repair loop."""
        self._stop_event.set()
        if self._thread:
            self._thread.join(timeout=5)

    def snapshot(self) -> dict[str, dict[str, Any]]:
        """Return the latest status snapshot."""
        with self._lock:
            return copy.deepcopy(self._results)

    def ensure_now(self) -> dict[str, dict[str, Any]]:
        """Run one repair pass immediately."""
        results = {
            "android_sshd": self._ensure_android_sshd(),
            "tmux_base": self._ensure_tmux_base(),
            "ac_caffeinate": self._ensure_ac_caffeinate(),
        }
        with self._lock:
            self._results = copy.deepcopy(results)
        return results

    def get_check(self, name: str) -> Optional[dict[str, Any]]:
        """Get the latest result for one check."""
        with self._lock:
            result = self._results.get(name)
            return copy.deepcopy(result) if result else None

    def _loop(self) -> None:
        while not self._stop_event.wait(self.check_interval):
            try:
                self.ensure_now()
            except Exception:
                logger.exception("Infrastructure supervisor pass failed")

    def _ensure_android_sshd(self) -> dict[str, Any]:
        public_ssh_host = str(
            ((self.config.get("external_access") or {}).get("public_ssh_host") or "")
        ).strip()
        if not public_ssh_host:
            return self._result("ok", "external ssh attach is not configured", attach_ready=False)
        if not self._android_sshd_config.exists():
            return self._result(
                "warning",
                "android attach sshd config is missing",
                attach_ready=False,
                config_path=str(self._android_sshd_config),
            )

        listener_targets = self._parse_sshd_listener_targets(self._android_sshd_config)
        if listener_targets and all(self._tcp_listening(host, port) for host, port in listener_targets):
            return self._result(
                "ok",
                "android attach sshd is listening",
                attach_ready=True,
                listeners=self._format_targets(listener_targets),
            )

        actions = self._repair_launch_agent(self._android_sshd_label, self._android_sshd_plist)
        listener_targets = self._parse_sshd_listener_targets(self._android_sshd_config)
        if listener_targets and all(self._tcp_listening(host, port) for host, port in listener_targets):
            logger.warning("Recovered android attach sshd via launchctl (%s)", ", ".join(actions) or "no-op")
            return self._result(
                "warning",
                "android attach sshd was down and was restarted",
                attach_ready=True,
                actions=actions,
                listeners=self._format_targets(listener_targets),
            )

        return self._result(
            "error",
            "android attach sshd is unavailable",
            attach_ready=False,
            actions=actions,
            listeners=self._format_targets(listener_targets),
        )

    def _ensure_tmux_base(self) -> dict[str, Any]:
        tmux_bin = shutil.which("tmux") or next(
            (candidate for candidate in ("/opt/homebrew/bin/tmux", "/usr/local/bin/tmux") if Path(candidate).exists()),
            None,
        )
        if not tmux_bin:
            return self._result("warning", "tmux binary is not installed")

        has_base = subprocess.run(
            [tmux_bin, "has-session", "-t", self._tmux_base_session],
            capture_output=True,
            text=True,
        )
        if has_base.returncode == 0:
            return self._result("ok", "tmux base session is present", session=self._tmux_base_session)

        create = subprocess.run(
            [tmux_bin, "new-session", "-d", "-s", self._tmux_base_session],
            capture_output=True,
            text=True,
        )
        if create.returncode == 0:
            logger.warning("Recovered missing tmux base session")
            return self._result(
                "warning",
                "tmux base session was missing and was recreated",
                session=self._tmux_base_session,
            )

        return self._result(
            "error",
            "tmux base session is unavailable",
            session=self._tmux_base_session,
            stderr=(create.stderr or "").strip() or None,
        )

    def _ensure_ac_caffeinate(self) -> dict[str, Any]:
        if not self._on_ac_power():
            return self._result("ok", "AC caffeinate skipped while on battery")
        if not self._caffeinate_plist.exists():
            return self._result("warning", "AC caffeinate launch agent is missing", plist=str(self._caffeinate_plist))

        if self._launch_agent_running(self._caffeinate_label):
            return self._result("ok", "AC caffeinate is running")

        actions = self._repair_launch_agent(self._caffeinate_label, self._caffeinate_plist)
        if self._launch_agent_running(self._caffeinate_label):
            logger.warning("Recovered AC caffeinate via launchctl (%s)", ", ".join(actions) or "no-op")
            return self._result("warning", "AC caffeinate was down and was restarted", actions=actions)

        return self._result("error", "AC caffeinate is unavailable on AC power", actions=actions)

    def _repair_launch_agent(self, label: str, plist_path: Path) -> list[str]:
        actions: list[str] = []
        if not plist_path.exists():
            return actions

        bootstrap = subprocess.run(
            ["launchctl", "bootstrap", f"gui/{self._uid}", str(plist_path)],
            capture_output=True,
            text=True,
        )
        bootstrap_err = (bootstrap.stderr or "").strip()
        if bootstrap.returncode == 0:
            actions.append("bootstrap")
        elif "service already loaded" in bootstrap_err.lower():
            actions.append("bootstrap-already-loaded")
        elif bootstrap_err:
            actions.append(f"bootstrap-failed:{bootstrap_err}")

        kickstart = subprocess.run(
            ["launchctl", "kickstart", "-k", f"gui/{self._uid}/{label}"],
            capture_output=True,
            text=True,
        )
        kickstart_err = (kickstart.stderr or "").strip()
        if kickstart.returncode == 0:
            actions.append("kickstart")
        elif kickstart_err:
            actions.append(f"kickstart-failed:{kickstart_err}")

        time.sleep(1)
        return actions

    def _launch_agent_running(self, label: str) -> bool:
        result = subprocess.run(
            ["launchctl", "print", f"gui/{self._uid}/{label}"],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            return False
        output = result.stdout or ""
        return "state = running" in output or "state = xpcproxy" in output

    def _on_ac_power(self) -> bool:
        try:
            result = subprocess.run(
                ["pmset", "-g", "batt"],
                capture_output=True,
                text=True,
            )
        except FileNotFoundError:
            logger.warning("pmset is unavailable; skipping AC power detection")
            return True
        if result.returncode != 0:
            return True
        return "AC Power" in (result.stdout or "")

    @staticmethod
    def _parse_sshd_listener_targets(config_path: Path) -> list[tuple[str, int]]:
        port = 22
        listen_addresses: list[tuple[str, Optional[int]]] = []
        for raw_line in config_path.read_text().splitlines():
            line = raw_line.strip()
            if not line or line.startswith("#"):
                continue
            parts = line.split()
            if len(parts) < 2:
                continue
            key = parts[0].lower()
            value = parts[1]
            if key == "port":
                try:
                    port = int(value)
                except ValueError:
                    continue
            elif key == "listenaddress":
                listen_port: Optional[int] = None
                host = value
                if value.startswith("[") and "]:" in value:
                    host_part, port_part = value.rsplit("]:", 1)
                    host = host_part[1:]
                    try:
                        listen_port = int(port_part)
                    except ValueError:
                        listen_port = None
                elif value.count(":") == 1:
                    host_part, port_part = value.rsplit(":", 1)
                    try:
                        listen_port = int(port_part)
                        host = host_part
                    except ValueError:
                        host = value
                listen_addresses.append((host, listen_port))
        if not listen_addresses:
            listen_addresses = [("127.0.0.1", None)]
        return [(address, listen_port or port) for address, listen_port in listen_addresses]

    @staticmethod
    def _format_targets(targets: list[tuple[str, int]]) -> list[str]:
        return [f"{host}:{port}" for host, port in targets]

    @staticmethod
    def _tcp_listening(host: str, port: int) -> bool:
        target_host = "127.0.0.1" if host in {"0.0.0.0", "::", "*"} else host
        try:
            with socket.create_connection((target_host, port), timeout=1.5):
                return True
        except OSError:
            return False

    @staticmethod
    def _result(status: str, message: str, **details: Any) -> dict[str, Any]:
        payload = {
            "status": status,
            "message": message,
            "checked_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        }
        if details:
            payload["details"] = details
        return payload
