"""Create a stopped-service final Rust migration state backup.

This tool is the MVP cutover version of the write-admission freeze gate: Python
must be stopped before the final rollback restore point is copied. By default it
is a dry run. With ``--execute`` it fails closed unless the configured Python
health URL is connection-refused, then delegates to the existing state backup
tool and can append final-backup evidence to the migration ledger.
"""

from __future__ import annotations

import argparse
import errno
import hashlib
import json
import socket
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable

from scripts.rust_migration.state_backup import build_backup_plan, render_text_report
from scripts.rust_migration.state_preflight import DEFAULT_CONFIG, _get, _load_yaml_config


DEFAULT_PYTHON_HEALTH_URL = "http://127.0.0.1:8420/health"
DEFAULT_STOPPED_HOLD_SECONDS = 5.0
DEFAULT_STOPPED_PROBE_COUNT = 2


def build_final_backup_report(
    *,
    config_path: Path = DEFAULT_CONFIG,
    local_env_path: Path | None = None,
    output_dir: Path | None = None,
    python_health_url: str | None = None,
    health_timeout_seconds: float = 1.0,
    stopped_hold_seconds: float = DEFAULT_STOPPED_HOLD_SECONDS,
    stopped_probe_count: int = DEFAULT_STOPPED_PROBE_COUNT,
    execute: bool = False,
    ledger_path: Path | None = None,
    record_ledger: bool = False,
    urlopen: Callable[..., Any] = urllib.request.urlopen,
    sleep: Callable[[float], None] = time.sleep,
) -> dict[str, Any]:
    if execute and output_dir is None:
        raise ValueError("--execute requires --output-dir")
    if health_timeout_seconds <= 0:
        raise ValueError("--health-timeout-seconds must be positive")
    if stopped_hold_seconds < 0:
        raise ValueError("--stopped-hold-seconds must be non-negative")
    if stopped_probe_count < 1:
        raise ValueError("--stopped-probe-count must be at least 1")
    if execute and stopped_probe_count < 2:
        raise ValueError("--execute requires --stopped-probe-count >= 2")
    generated_at = datetime.now(timezone.utc).isoformat()
    resolved_health_url = python_health_url or configured_python_health_url(config_path)
    if execute:
        health = probe_python_origin_stopped_durably(
            resolved_health_url,
            timeout_seconds=health_timeout_seconds,
            hold_seconds=stopped_hold_seconds,
            probe_count=stopped_probe_count,
            urlopen=urlopen,
            sleep=sleep,
        )
    else:
        health = probe_python_origin_stopped(
            resolved_health_url,
            timeout_seconds=health_timeout_seconds,
            urlopen=urlopen,
        )
    ledger = _ledger_info(ledger_path, record_ledger=record_ledger)
    blockers = [*health["blockers"], *ledger["blockers"]]

    backup: dict[str, Any] | None = None
    if not execute or not blockers:
        backup = build_backup_plan(
            config_path=config_path,
            local_env_path=local_env_path,
            output_dir=output_dir,
            execute=execute,
        )
        blockers.extend(backup["blockers"])

    status = "blocked" if blockers else "copied" if execute else "planned"
    report: dict[str, Any] = {
        "schema_version": 1,
        "generated_at": generated_at,
        "status": status,
        "mode": "execute" if execute else "dry_run",
        "python_origin": health,
        "backup": backup,
        "ledger": {
            "path": str(ledger_path.expanduser()) if ledger_path else None,
            "record_ledger": record_ledger,
            "written": False,
            "entry_kind": "final_backup",
        },
        "summary": {
            "python_origin_stopped": health["stopped"],
            "backup_status": backup.get("status") if backup else None,
            "copied": (backup.get("summary") or {}).get("copied") if backup else 0,
            "planned": (backup.get("summary") or {}).get("planned") if backup else 0,
            "blockers": len(blockers),
            "warnings": len((backup or {}).get("warnings", [])),
        },
        "blockers": blockers,
        "warnings": (backup or {}).get("warnings", []),
    }
    if execute and record_ledger and not blockers:
        _append_ledger_entry(report, Path(ledger_path).expanduser())
        report["ledger"]["written"] = True
    return report


def configured_python_health_url(config_path: Path = DEFAULT_CONFIG) -> str:
    config = _load_yaml_config(config_path.expanduser())
    host = str(_get(config, ("server", "host"), "127.0.0.1")).strip()
    if not host:
        host = "127.0.0.1"
    port = _coerce_port(_get(config, ("server", "port"), 8420))
    if host == "0.0.0.0":
        host = "127.0.0.1"
    elif host in {"::", "[::]"}:
        host = "[::1]"
    if ":" in host and not host.startswith("["):
        host = f"[{host}]"
    return f"http://{host}:{port}/health"


def probe_python_origin_stopped(
    url: str,
    *,
    timeout_seconds: float,
    urlopen: Callable[..., Any] = urllib.request.urlopen,
) -> dict[str, Any]:
    blockers: list[dict[str, str]] = []
    try:
        with urlopen(url, timeout=timeout_seconds) as response:
            status = getattr(response, "status", None) or response.getcode()
        blockers.append(
            _issue(
                "python_origin",
                "python_origin_reachable",
                f"Python origin answered {url} with HTTP {status}",
            )
        )
        return {
            "url": url,
            "stopped": False,
            "status": "reachable",
            "http_status": status,
            "detail": f"HTTP {status}",
            "blockers": blockers,
        }
    except urllib.error.HTTPError as exc:
        blockers.append(
            _issue(
                "python_origin",
                "python_origin_reachable",
                f"Python origin answered {url} with HTTP {exc.code}",
            )
        )
        return {
            "url": url,
            "stopped": False,
            "status": "reachable",
            "http_status": exc.code,
            "detail": f"HTTP {exc.code}",
            "blockers": blockers,
        }
    except urllib.error.URLError as exc:
        if _is_connection_refused(exc.reason):
            return {
                "url": url,
                "stopped": True,
                "status": "connection_refused",
                "http_status": None,
                "detail": str(exc.reason),
                "blockers": [],
            }
        blockers.append(
            _issue(
                "python_origin",
                "python_origin_probe_failed",
                f"Python origin probe did not prove stopped: {exc.reason}",
            )
        )
        return {
            "url": url,
            "stopped": False,
            "status": "probe_failed",
            "http_status": None,
            "detail": str(exc.reason),
            "blockers": blockers,
        }
    except (TimeoutError, socket.timeout) as exc:
        blockers.append(
            _issue(
                "python_origin",
                "python_origin_probe_timeout",
                f"Python origin probe timed out: {exc}",
            )
        )
        return {
            "url": url,
            "stopped": False,
            "status": "timeout",
            "http_status": None,
            "detail": str(exc),
            "blockers": blockers,
        }
    except ValueError as exc:
        blockers.append(
            _issue(
                "python_origin",
                "python_origin_probe_invalid_url",
                f"Python origin health URL is invalid: {exc}",
            )
        )
        return {
            "url": url,
            "stopped": False,
            "status": "invalid_url",
            "http_status": None,
            "detail": str(exc),
            "blockers": blockers,
        }


def probe_python_origin_stopped_durably(
    url: str,
    *,
    timeout_seconds: float,
    hold_seconds: float,
    probe_count: int,
    urlopen: Callable[..., Any] = urllib.request.urlopen,
    sleep: Callable[[float], None] = time.sleep,
) -> dict[str, Any]:
    attempts: list[dict[str, Any]] = []
    interval = hold_seconds / (probe_count - 1) if probe_count > 1 else 0.0
    for index in range(probe_count):
        if index and interval:
            sleep(interval)
        probe = probe_python_origin_stopped(
            url,
            timeout_seconds=timeout_seconds,
            urlopen=urlopen,
        )
        attempts.append(_probe_attempt(probe))
        if not probe["stopped"]:
            probe = dict(probe)
            probe["attempts"] = attempts
            probe["required_probe_count"] = probe_count
            probe["hold_seconds"] = hold_seconds
            return probe
    return {
        "url": url,
        "stopped": True,
        "status": "connection_refused",
        "http_status": None,
        "detail": f"{probe_count} refused probes over {hold_seconds:g}s",
        "attempts": attempts,
        "required_probe_count": probe_count,
        "hold_seconds": hold_seconds,
        "blockers": [],
    }


def _probe_attempt(probe: dict[str, Any]) -> dict[str, Any]:
    return {
        "stopped": probe["stopped"],
        "status": probe["status"],
        "http_status": probe.get("http_status"),
        "detail": probe.get("detail"),
    }


def render_final_backup_text(report: dict[str, Any]) -> str:
    lines = [
        "Rust final backup gate",
        f"status: {report['status']}",
        f"mode: {report['mode']}",
        f"python_origin_stopped: {str(report['python_origin']['stopped']).lower()}",
        f"python_origin_status: {report['python_origin']['status']}",
        f"backup_status: {report['summary']['backup_status']}",
        f"copied: {report['summary']['copied']}",
        f"planned: {report['summary']['planned']}",
        f"blockers: {report['summary']['blockers']}",
        f"warnings: {report['summary']['warnings']}",
    ]
    if report.get("backup"):
        lines.extend(["", render_text_report(report["backup"])])
    if report["blockers"]:
        lines.extend(["", "Blockers:"])
        for blocker in report["blockers"]:
            lines.append(f"  {blocker['store_id']}: {blocker['kind']} - {blocker['detail']}")
    return "\n".join(lines)


def _is_connection_refused(reason: Any) -> bool:
    if isinstance(reason, ConnectionRefusedError):
        return True
    if isinstance(reason, OSError):
        return reason.errno == errno.ECONNREFUSED
    return False


def _ledger_info(
    ledger_path: Path | None,
    *,
    record_ledger: bool,
) -> dict[str, Any]:
    blockers: list[dict[str, str]] = []
    if not record_ledger:
        return {"blockers": blockers}
    if ledger_path is None:
        blockers.append(_issue("ledger", "missing_ledger_path", "--record-ledger requires --ledger"))
        return {"blockers": blockers}
    path = ledger_path.expanduser()
    parent = path.parent
    if not parent.exists():
        blockers.append(
            _issue("ledger", "ledger_parent_missing", f"ledger parent does not exist: {parent}")
        )
    elif not parent.is_dir():
        blockers.append(
            _issue("ledger", "ledger_parent_not_dir", f"ledger parent is not a directory: {parent}")
        )
    if path.exists() and path.is_dir():
        blockers.append(_issue("ledger", "ledger_is_directory", f"ledger is a directory: {path}"))
    if path.is_symlink():
        blockers.append(_issue("ledger", "ledger_is_symlink", f"ledger is a symlink: {path}"))
    return {"blockers": blockers}


def _append_ledger_entry(report: dict[str, Any], ledger_path: Path) -> None:
    backup = report["backup"] or {}
    manifest_path = backup.get("manifest_path")
    entry = {
        "schema_version": 1,
        "kind": "final_backup",
        "generated_at": report["generated_at"],
        "status": report["status"],
        "python_origin_stopped": report["python_origin"]["stopped"],
        "python_origin_status": report["python_origin"]["status"],
        "backup_root": backup.get("backup_root"),
        "manifest_path": manifest_path,
        "manifest_sha256": _path_sha256(Path(manifest_path)) if manifest_path else None,
        "backup_summary": backup.get("summary"),
        "store_evidence": _store_backup_evidence(backup),
    }
    ledger_path.parent.mkdir(parents=True, exist_ok=True)
    with ledger_path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(entry, sort_keys=True) + "\n")


def _issue(store_id: str, kind: str, detail: str) -> dict[str, str]:
    return {
        "store_id": store_id,
        "kind": kind,
        "severity": "blocker",
        "detail": detail,
    }


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--local-env", type=Path, default=None)
    parser.add_argument("--output-dir", type=Path, default=None)
    parser.add_argument(
        "--python-health-url",
        default=None,
        help=(
            "Override the Python origin health URL. Defaults to server.host/server.port "
            "from --config."
        ),
    )
    parser.add_argument("--health-timeout-seconds", type=float, default=1.0)
    parser.add_argument(
        "--stopped-hold-seconds",
        type=float,
        default=DEFAULT_STOPPED_HOLD_SECONDS,
        help="Seconds over which stopped-service probes must remain connection-refused before --execute copies.",
    )
    parser.add_argument(
        "--stopped-probe-count",
        type=int,
        default=DEFAULT_STOPPED_PROBE_COUNT,
        help="Number of stopped-service probes required before --execute copies.",
    )
    parser.add_argument("--ledger", type=Path, default=None)
    parser.add_argument(
        "--record-ledger",
        action="store_true",
        help="Append final-backup JSONL evidence to --ledger",
    )
    parser.add_argument("--execute", action="store_true", help="Copy the final backup")
    parser.add_argument("--json", action="store_true", help="Emit JSON report")
    parser.add_argument(
        "--fail-on-blockers",
        action="store_true",
        help="Exit nonzero when the report has blockers",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    try:
        report = build_final_backup_report(
            config_path=args.config,
            local_env_path=args.local_env,
            output_dir=args.output_dir,
            python_health_url=args.python_health_url,
            health_timeout_seconds=args.health_timeout_seconds,
            stopped_hold_seconds=args.stopped_hold_seconds,
            stopped_probe_count=args.stopped_probe_count,
            execute=args.execute,
            ledger_path=args.ledger,
            record_ledger=args.record_ledger,
        )
    except ValueError as exc:
        raise SystemExit(str(exc)) from exc
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render_final_backup_text(report))
    if args.fail_on_blockers and report["blockers"]:
        return 1
    return 0


def _coerce_port(value: Any) -> int:
    try:
        port = int(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"server.port must be an integer, got {value!r}") from exc
    if port <= 0 or port > 65535:
        raise ValueError(f"server.port must be in 1..65535, got {port}")
    return port


def _path_sha256(path: Path) -> str | None:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError:
        return None
    return digest.hexdigest()


def _store_backup_evidence(backup: dict[str, Any]) -> list[dict[str, Any]]:
    evidence: list[dict[str, Any]] = []
    for entry in backup.get("entries") or []:
        if entry.get("action") != "copy":
            continue
        evidence.append(
            {
                "store_id": entry.get("store_id"),
                "kind": entry.get("kind"),
                "source": entry.get("source"),
                "destination": entry.get("destination"),
                "size_bytes": entry.get("size_bytes"),
                "sha256": entry.get("sha256"),
                "file_count": entry.get("file_count"),
                "backup_size_bytes": entry.get("backup_size_bytes"),
                "backup_sha256": entry.get("backup_sha256"),
                "backup_file_count": entry.get("backup_file_count"),
            }
        )
    return evidence


if __name__ == "__main__":
    raise SystemExit(main())
