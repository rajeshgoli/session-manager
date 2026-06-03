"""Node registry and command routing for local/remote session execution."""

from __future__ import annotations

import asyncio
import os
import shlex
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Optional


PRIMARY_NODE = "primary"


@dataclass(frozen=True)
class NodeConfig:
    """Configuration for one Session Manager execution node."""

    id: str
    ssh: Optional[str] = None
    ssh_proxy_command: Optional[str] = None
    control_path: Optional[str] = None
    api_url: Optional[str] = None
    hook_base_url: Optional[str] = None
    hook_secret: Optional[str] = None
    projects_root: Optional[str] = None

    @property
    def is_primary(self) -> bool:
        return self.id == PRIMARY_NODE


class NodeRegistry:
    """Validated node registry loaded from config.yaml."""

    def __init__(self, nodes: dict[str, NodeConfig], default_node: str = PRIMARY_NODE):
        self._nodes = dict(nodes)
        self._nodes.setdefault(PRIMARY_NODE, NodeConfig(id=PRIMARY_NODE))
        self.default_node = default_node if default_node in self._nodes else PRIMARY_NODE

    @classmethod
    def from_config(cls, config: Optional[dict]) -> "NodeRegistry":
        raw_nodes = (config or {}).get("nodes") or {}
        if not isinstance(raw_nodes, dict):
            raw_nodes = {}

        raw_registry = raw_nodes.get("registry") or {}
        if not isinstance(raw_registry, dict):
            raw_registry = {}

        nodes: dict[str, NodeConfig] = {PRIMARY_NODE: NodeConfig(id=PRIMARY_NODE)}
        for raw_id, raw_value in raw_registry.items():
            node_id = str(raw_id or "").strip()
            if not node_id:
                continue
            value = raw_value if isinstance(raw_value, dict) else {}
            nodes[node_id] = NodeConfig(
                id=node_id,
                ssh=_clean_optional(value.get("ssh")),
                ssh_proxy_command=_clean_optional(value.get("ssh_proxy_command")),
                control_path=_expand_user_optional(value.get("control_path")),
                api_url=_clean_optional(value.get("api_url")),
                hook_base_url=_clean_optional(value.get("hook_base_url")),
                hook_secret=_clean_optional(value.get("hook_secret")),
                projects_root=_clean_optional(value.get("projects_root")),
            )

        default_node = _clean_optional(raw_nodes.get("default")) or PRIMARY_NODE
        return cls(nodes=nodes, default_node=default_node)

    def get(self, node_id: Optional[str]) -> Optional[NodeConfig]:
        normalized = normalize_node_id(node_id)
        return self._nodes.get(normalized)

    def require(self, node_id: Optional[str]) -> NodeConfig:
        normalized = normalize_node_id(node_id)
        node = self._nodes.get(normalized)
        if node is None:
            raise ValueError(f"Unknown node: {normalized}")
        return node

    def has(self, node_id: Optional[str]) -> bool:
        return normalize_node_id(node_id) in self._nodes

    def ids(self) -> list[str]:
        return sorted(self._nodes)

    def as_list(self) -> list[dict[str, object]]:
        return [
            {
                "id": node.id,
                "primary": node.is_primary,
                "ssh": node.ssh,
                "api_url": node.api_url,
                "hook_base_url": node.hook_base_url,
                "projects_root": node.projects_root,
            }
            for node in sorted(self._nodes.values(), key=lambda item: item.id)
        ]


class NodeRunner:
    """Run commands on the primary host or on a registered SSH node."""

    def __init__(self, registry: NodeRegistry):
        self.registry = registry

    def is_primary(self, node_id: Optional[str]) -> bool:
        return normalize_node_id(node_id) == PRIMARY_NODE

    def command(
        self,
        node_id: Optional[str],
        argv: Iterable[object],
        *,
        cwd: Optional[str] = None,
        tty: bool = False,
    ) -> list[str]:
        """Return a local argv or ssh-routed argv for one node."""
        node = self.registry.require(node_id)
        local_argv = [str(part) for part in argv]
        if node.is_primary:
            return local_argv

        if not node.ssh:
            raise ValueError(f"Remote node {node.id} is missing ssh target")

        remote = shlex.join(local_argv)
        if cwd:
            remote = f"cd {shlex.quote(str(cwd))} && {remote}"

        cmd = ["ssh"]
        if tty:
            cmd.append("-tt")
        cmd.extend(self._ssh_options(node))
        cmd.append(node.ssh)
        # OpenSSH concatenates post-destination args into a remote command string.
        # Quote the shell payload so the remote shell passes it as one sh -lc arg.
        cmd.extend(["/bin/sh", "-lc", shlex.quote(remote)])
        return cmd

    def attach_command(self, node_id: Optional[str], argv: Iterable[object]) -> list[str]:
        """Return an argv suitable for Popen under a local PTY."""
        return self.command(node_id, argv, tty=not self.is_primary(node_id))

    def run(
        self,
        node_id: Optional[str],
        argv: Iterable[object],
        *,
        cwd: Optional[str] = None,
        check: bool = True,
        timeout: Optional[float] = None,
        capture_output: bool = True,
        text: bool = True,
    ) -> subprocess.CompletedProcess:
        cmd = self.command(node_id, argv, cwd=cwd)
        run_kwargs: dict[str, object] = {
            "check": check,
            "timeout": timeout,
            "capture_output": capture_output,
            "text": text,
        }
        if self.is_primary(node_id) and cwd:
            run_kwargs["cwd"] = cwd
        return subprocess.run(cmd, **run_kwargs)

    async def run_async(
        self,
        node_id: Optional[str],
        argv: Iterable[object],
        *,
        cwd: Optional[str] = None,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> subprocess.CompletedProcess:
        cmd = self.command(node_id, argv, cwd=cwd)
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            cwd=cwd if self.is_primary(node_id) and cwd else None,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        try:
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        except asyncio.TimeoutError:
            proc.kill()
            await proc.wait()
            raise
        result = subprocess.CompletedProcess(
            cmd,
            proc.returncode,
            stdout.decode(errors="replace"),
            stderr.decode(errors="replace"),
        )
        if check and result.returncode != 0:
            raise subprocess.CalledProcessError(
                result.returncode,
                cmd,
                output=result.stdout,
                stderr=result.stderr,
            )
        return result

    def ping(self, node_id: Optional[str], timeout: float = 5.0) -> bool:
        result = self.run(node_id, ["true"], check=False, timeout=timeout)
        return result.returncode == 0

    def path_is_dir(self, node_id: Optional[str], path: str, timeout: float = 5.0) -> bool:
        return self.resolve_directory(node_id, path, timeout=timeout) is not None

    def resolve_directory(self, node_id: Optional[str], path: str, timeout: float = 5.0) -> Optional[str]:
        """Return the node-local physical directory path, or None if it is not a directory."""
        if self.is_primary(node_id):
            candidate = Path(path).expanduser().resolve()
            return str(candidate) if candidate.is_dir() else None

        script = """
path=$1
case "$path" in
  "~") path=${HOME:-~} ;;
  "~/"*) path="${HOME%/}/${path#\\~/}" ;;
esac
CDPATH= cd -- "$path" 2>/dev/null && pwd -P
"""
        result = self.run(
            node_id,
            ["/bin/sh", "-lc", script, "sh", path],
            check=False,
            timeout=timeout,
        )
        if result.returncode != 0:
            return None
        resolved = [line.strip() for line in (result.stdout or "").splitlines() if line.strip()]
        return resolved[0] if resolved else None

    def ensure_file(self, node_id: Optional[str], path: str, timeout: float = 5.0) -> bool:
        if self.is_primary(node_id):
            local_path = Path(path).expanduser()
            local_path.parent.mkdir(parents=True, exist_ok=True)
            local_path.touch()
            return True

        script = """
path=$1
case "$path" in
  "~") path=${HOME:-~} ;;
  "~/"*) path="${HOME%/}/${path#\\~/}" ;;
esac
parent=$(dirname "$path")
mkdir -p "$parent" && : >> "$path"
"""
        result = self.run(
            node_id,
            ["/bin/sh", "-lc", script, "sh", path],
            check=False,
            timeout=timeout,
        )
        return result.returncode == 0

    def command_available(
        self,
        node_id: Optional[str],
        command: str,
        *,
        cwd: Optional[str] = None,
        timeout: float = 5.0,
    ) -> bool:
        if "/" in command or command.startswith("~"):
            payload = """
path=$1
case "$path" in
  "~") path=${HOME:-~} ;;
  "~/"*) path="${HOME%/}/${path#\\~/}" ;;
esac
test -f "$path" && test -x "$path"
"""
            argv = ["/bin/sh", "-lc", payload, "sh", command]
        else:
            payload = f"command -v {shlex.quote(command)} >/dev/null 2>&1"
            argv = ["/bin/sh", "-lc", payload]
        result = self.run(
            node_id,
            argv,
            cwd=cwd,
            check=False,
            timeout=timeout,
        )
        return result.returncode == 0

    def read_text(self, node_id: Optional[str], path: str, timeout: float = 5.0) -> Optional[str]:
        result = self.run(node_id, ["cat", path], check=False, timeout=timeout)
        if result.returncode != 0:
            return None
        return result.stdout

    def _ssh_options(self, node: NodeConfig) -> list[str]:
        opts = [
            "-o",
            "ControlMaster=auto",
            "-o",
            "ControlPersist=600",
            "-o",
            "ConnectTimeout=5",
        ]
        if node.control_path:
            opts.extend(["-S", node.control_path])
        if node.ssh_proxy_command:
            opts.extend(["-o", f"ProxyCommand={node.ssh_proxy_command}"])
        return opts


def normalize_node_id(value: Optional[str]) -> str:
    normalized = str(value or "").strip()
    return normalized or PRIMARY_NODE


def _clean_optional(value: object) -> Optional[str]:
    if value is None:
        return None
    text = str(value).strip()
    return text or None


def _expand_user_optional(value: object) -> Optional[str]:
    text = _clean_optional(value)
    if not text:
        return None
    return os.path.expanduser(text)
