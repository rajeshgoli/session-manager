"""Report must-preserve state for Rust migration cutover.

This preflight is intentionally non-mutating. It resolves the durable stores
named by the Stage 5 ownership table, checks whether each path exists and is
copyable, and emits JSON/text evidence for later freeze, backup, and rollback
tooling.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import yaml


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_CONFIG = Path("config.yaml")
DEFAULT_STATE_FILE = "~/.local/share/claude-sessions/sessions.json"
DEFAULT_MESSAGE_QUEUE_DB = "~/.local/share/claude-sessions/message_queue.db"
DEFAULT_RESPONSE_RELAY_DB = "~/.local/share/claude-sessions/response_relay.db"
DEFAULT_TOOL_USAGE_DB = "~/.local/share/claude-sessions/tool_usage.db"
DEFAULT_TELEGRAM_TOPICS = "~/.local/share/claude-sessions/telegram_topics.json"
DEFAULT_QUEUE_RUNNER_STATE_DIR = "~/.local/share/claude-sessions/queue-runner"
DEFAULT_SERVER_LOG_FILE = "/tmp/session-manager.log"
DEFAULT_EMAIL_BRIDGE_CONFIG = "config/email_send.yaml"
DEFAULT_APP_ARTIFACTS_DIR = "data/apps"
DEFAULT_BUG_REPORTS_DB = "data/bug_reports.db"
DEFAULT_LOCAL_ENV = ".local/android-parity/values.env"
CLIENT_CONFIG_ENV = "SM_CLIENT_CONFIG"
CLIENT_CONFIG_SUBPATH = "session-manager/client.yaml"


@dataclass(frozen=True)
class StoreSpec:
    id: str
    label: str
    kind: str
    path: Path
    required: bool
    category: str
    source: str


def build_state_preflight_report(
    *,
    config_path: Path = DEFAULT_CONFIG,
    local_env_path: Path | None = None,
) -> dict[str, Any]:
    config_path = _resolve_path(config_path, base=Path.cwd())
    config = _load_yaml_config(config_path)
    state_file = _config_path(
        config,
        ("paths", "state_file"),
        DEFAULT_STATE_FILE,
        config_path=config_path,
    )
    local_env = (
        _resolve_path(local_env_path, base=config_path.parent)
        if local_env_path
        else _resolve_path(config_path.parent / DEFAULT_LOCAL_ENV, base=Path.cwd())
    )

    specs = _store_specs(config, config_path=config_path, state_file=state_file, local_env=local_env)
    rows = [_inspect_store(spec) for spec in specs]
    blockers = [
        issue
        for row in rows
        for issue in row["issues"]
        if issue["severity"] == "blocker"
    ]
    warnings = [
        issue
        for row in rows
        for issue in row["issues"]
        if issue["severity"] == "warning"
    ]
    existing = sum(1 for row in rows if row["exists"])
    return {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "status": "blocked" if blockers else "passed",
        "inputs": {
            "config": str(config_path),
            "config_exists": config_path.exists(),
            "local_env": str(local_env),
            "local_env_exists": local_env.exists(),
            "client_config": str(_client_config_path()),
        },
        "summary": {
            "stores": len(rows),
            "existing": existing,
            "missing": len(rows) - existing,
            "blockers": len(blockers),
            "warnings": len(warnings),
        },
        "stores": rows,
        "blockers": blockers,
        "warnings": warnings,
    }


def render_text_report(report: dict[str, Any]) -> str:
    summary = report["summary"]
    lines = [
        "Rust state ownership preflight",
        f"status: {report['status']}",
        (
            "stores: "
            f"{summary['stores']} total, {summary['existing']} existing, "
            f"{summary['missing']} missing"
        ),
        f"blockers: {summary['blockers']}",
        f"warnings: {summary['warnings']}",
        "",
        "Stores:",
    ]
    for row in report["stores"]:
        marker = "ok"
        if any(issue["severity"] == "blocker" for issue in row["issues"]):
            marker = "blocker"
        elif row["issues"]:
            marker = "warning"
        presence = row["actual_kind"] if row["exists"] else "missing"
        lines.append(
            f"  {marker:7} {row['id']}: {presence} {row['path']}"
        )
    if report["blockers"]:
        lines.extend(["", "Blockers:"])
        for issue in report["blockers"]:
            lines.append(f"  {issue['store_id']}: {issue['kind']} - {issue['detail']}")
    if report["warnings"]:
        lines.extend(["", "Warnings:"])
        for issue in report["warnings"]:
            lines.append(f"  {issue['store_id']}: {issue['kind']} - {issue['detail']}")
    return "\n".join(lines)


def _store_specs(
    config: dict[str, Any],
    *,
    config_path: Path,
    state_file: Path,
    local_env: Path,
) -> list[StoreSpec]:
    state_parent = state_file.parent
    queue_runner_configured = _get(config, ("queue_runner", "state_dir")) is not None
    queue_runner_default = (
        DEFAULT_QUEUE_RUNNER_STATE_DIR
        if queue_runner_configured or str(state_file) == str(Path(DEFAULT_STATE_FILE).expanduser())
        else str(state_parent / "queue-runner")
    )
    return [
        StoreSpec("config_yaml", "Config YAML", "file", config_path, True, "config", "config argument"),
        StoreSpec(
            "client_yaml",
            "Shared client config",
            "file",
            _client_config_path(),
            False,
            "config",
            "SM_CLIENT_CONFIG / XDG_CONFIG_HOME / ~/.config/session-manager/client.yaml",
        ),
        StoreSpec("local_env_overlay", "Local auth env overlay", "file", local_env, False, "config", "default local env overlay"),
        StoreSpec("sessions_state", "Session state", "file", state_file, True, "session", "paths.state_file"),
        StoreSpec(
            "message_queue_db",
            "Message queue DB",
            "file",
            _config_path(config, ("sm_send", "db_path"), DEFAULT_MESSAGE_QUEUE_DB, config_path=config_path),
            False,
            "queue",
            "sm_send.db_path",
        ),
        StoreSpec(
            "response_relay_db",
            "Response relay DB",
            "file",
            _config_path(config, ("response_relay", "db_path"), DEFAULT_RESPONSE_RELAY_DB, config_path=config_path),
            False,
            "relay",
            "response_relay.db_path",
        ),
        StoreSpec(
            "tool_usage_db",
            "Tool usage audit DB",
            "file",
            _config_path(config, ("tool_logging", "db_path"), DEFAULT_TOOL_USAGE_DB, config_path=config_path),
            False,
            "audit",
            "tool_logging.db_path",
        ),
        StoreSpec(
            "telegram_topics_json",
            "Telegram topic archive",
            "file",
            _config_path(config, ("telegram", "topic_registry", "path"), DEFAULT_TELEGRAM_TOPICS, config_path=config_path),
            False,
            "archive",
            "telegram.topic_registry.path",
        ),
        StoreSpec(
            "codex_events_db",
            "Codex events DB",
            "file",
            _config_path(config, ("codex_events", "db_path"), str(state_parent / "codex_events.db"), config_path=config_path),
            False,
            "codex",
            "codex_events.db_path",
        ),
        StoreSpec(
            "codex_requests_db",
            "Codex requests DB",
            "file",
            _config_path(config, ("codex_requests", "db_path"), str(state_parent / "codex_requests.db"), config_path=config_path),
            False,
            "codex",
            "codex_requests.db_path",
        ),
        StoreSpec(
            "codex_observability_db",
            "Codex observability DB",
            "file",
            _config_path(config, ("codex_observability", "db_path"), str(state_parent / "codex_observability.db"), config_path=config_path),
            False,
            "codex",
            "codex_observability.db_path",
        ),
        StoreSpec(
            "queue_runner_state_dir",
            "Queue runner state dir",
            "dir",
            _config_path(config, ("queue_runner", "state_dir"), queue_runner_default, config_path=config_path),
            False,
            "queue_runner",
            "queue_runner.state_dir",
        ),
        StoreSpec(
            "bug_reports_db",
            "Bug reports DB",
            "file",
            _config_path(config, ("paths", "bug_reports_db"), _get(config, ("bug_reports", "db_path"), DEFAULT_BUG_REPORTS_DB), config_path=config_path),
            False,
            "mobile",
            "paths.bug_reports_db",
        ),
        StoreSpec(
            "app_artifacts_dir",
            "App artifacts dir",
            "dir",
            _config_path(config, ("paths", "app_artifacts_dir"), _get(config, ("app_artifacts", "root_dir"), DEFAULT_APP_ARTIFACTS_DIR), config_path=config_path),
            False,
            "mobile",
            "paths.app_artifacts_dir",
        ),
        StoreSpec(
            "email_bridge_config",
            "Email bridge config",
            "file",
            _config_path(config, ("email", "bridge_config"), DEFAULT_EMAIL_BRIDGE_CONFIG, config_path=config_path),
            False,
            "email",
            "email.bridge_config",
        ),
        StoreSpec(
            "server_log_file",
            "Server log file",
            "file",
            _config_path(config, ("paths", "server_log_file"), _get(config, ("mobile_analytics", "server_log_file"), DEFAULT_SERVER_LOG_FILE), config_path=config_path),
            False,
            "logs",
            "paths.server_log_file",
        ),
        StoreSpec(
            "log_dir",
            "Runtime log dir",
            "dir",
            _config_path(config, ("paths", "log_dir"), "/tmp/claude-sessions", config_path=config_path),
            False,
            "logs",
            "paths.log_dir",
        ),
    ]


def _inspect_store(spec: StoreSpec) -> dict[str, Any]:
    path = spec.path
    row: dict[str, Any] = {
        "id": spec.id,
        "label": spec.label,
        "category": spec.category,
        "kind": spec.kind,
        "path": str(path),
        "required": spec.required,
        "source": spec.source,
        "exists": path.exists(),
        "actual_kind": None,
        "readable": False,
        "copyable": False,
        "size_bytes": None,
        "sha256": None,
        "file_count": None,
        "issues": [],
    }
    if not path.exists():
        severity = "blocker" if spec.required else "warning"
        row["issues"].append(
            _issue(spec, "missing", severity, f"path does not exist: {path}")
        )
        return row

    if path.is_file():
        row["actual_kind"] = "file"
        row["size_bytes"] = path.stat().st_size
        row["readable"], row["sha256"] = _file_hash(path)
        row["copyable"] = row["readable"]
    elif path.is_dir():
        row["actual_kind"] = "dir"
        row["readable"], row["size_bytes"], row["file_count"] = _dir_stats(path)
        row["copyable"] = row["readable"]
    else:
        row["actual_kind"] = "other"
        row["issues"].append(
            _issue(spec, "unsupported_type", "blocker", f"path is not a regular file or directory: {path}")
        )
        return row

    if row["actual_kind"] != spec.kind:
        row["issues"].append(
            _issue(
                spec,
                "wrong_kind",
                "blocker",
                f"expected {spec.kind}, got {row['actual_kind']}",
            )
        )
    if not row["readable"]:
        row["issues"].append(
            _issue(spec, "not_readable", "blocker", f"path cannot be read: {path}")
        )
    return row


def _file_hash(path: Path) -> tuple[bool, str | None]:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError:
        return False, None
    return True, digest.hexdigest()


def _dir_stats(path: Path) -> tuple[bool, int, int]:
    total_size = 0
    file_count = 0
    try:
        for child in path.rglob("*"):
            if child.is_file():
                file_count += 1
                total_size += child.stat().st_size
    except OSError:
        return False, total_size, file_count
    return True, total_size, file_count


def _issue(spec: StoreSpec, kind: str, severity: str, detail: str) -> dict[str, str]:
    return {
        "store_id": spec.id,
        "kind": kind,
        "severity": severity,
        "detail": detail,
    }


def _load_yaml_config(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    with path.open() as handle:
        loaded = yaml.safe_load(handle) or {}
    if not isinstance(loaded, dict):
        raise ValueError(f"config must be a mapping: {path}")
    return loaded


def _config_path(
    config: dict[str, Any],
    keys: tuple[str, ...],
    default: Any,
    *,
    config_path: Path,
) -> Path:
    raw = _get(config, keys, default)
    return _resolve_runtime_path(Path(str(raw)))


def _get(config: dict[str, Any], keys: tuple[str, ...], default: Any = None) -> Any:
    current: Any = config
    for key in keys:
        if not isinstance(current, dict) or key not in current:
            return default
        current = current[key]
    return current if current is not None else default


def _resolve_path(path: Path, *, base: Path) -> Path:
    expanded = Path(str(path)).expanduser()
    if expanded.is_absolute():
        return expanded
    candidate = (base / expanded).resolve()
    if candidate.exists() or base != Path.cwd():
        return candidate
    return (REPO_ROOT / expanded).resolve()


def _resolve_runtime_path(path: Path) -> Path:
    expanded = Path(str(path)).expanduser()
    if expanded.is_absolute():
        return expanded
    return (REPO_ROOT / expanded).resolve()


def _client_config_path() -> Path:
    override = os.environ.get(CLIENT_CONFIG_ENV)
    if override:
        return Path(override).expanduser()
    xdg_config_home = os.environ.get("XDG_CONFIG_HOME")
    if xdg_config_home:
        return Path(xdg_config_home).expanduser() / CLIENT_CONFIG_SUBPATH
    return Path.home() / ".config" / CLIENT_CONFIG_SUBPATH


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--local-env", type=Path, default=None)
    parser.add_argument("--json", action="store_true", help="Emit JSON report")
    parser.add_argument(
        "--fail-on-blockers",
        action="store_true",
        help="Exit nonzero when required or unreadable state blocks cutover",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    try:
        report = build_state_preflight_report(
            config_path=args.config,
            local_env_path=args.local_env,
        )
    except ValueError as exc:
        raise SystemExit(str(exc)) from exc
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render_text_report(report))
    if args.fail_on_blockers and report["blockers"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
