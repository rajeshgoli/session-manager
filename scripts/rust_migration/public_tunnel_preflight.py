"""Validate the public tunnel shape for Rust service cutover.

This is intentionally local and non-mutating. It reads a cloudflared ingress
config and verifies that the protected SM app hostname reaches the
launchd-managed Rust service origin while legacy public operational hostnames
do not route to origin.
"""

from __future__ import annotations

import argparse
import json
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import yaml


DEFAULT_CONFIG = Path(".local/android-parity/cloudflared/config-http-only.yml")
DEFAULT_APP_HOST = "sm-app.rajeshgo.li"
DEFAULT_EXPECTED_ORIGIN = "http://127.0.0.1:8420"
DEFAULT_FORBIDDEN_HOSTS = ("sm.rajeshgo.li",)


@dataclass(frozen=True)
class IngressRow:
    index: int
    hostname: str | None
    path: str | None
    service: str | None


def build_public_tunnel_preflight_report(
    *,
    config_path: Path = DEFAULT_CONFIG,
    app_host: str = DEFAULT_APP_HOST,
    expected_origin: str = DEFAULT_EXPECTED_ORIGIN,
    forbidden_hosts: tuple[str, ...] = DEFAULT_FORBIDDEN_HOSTS,
) -> dict[str, Any]:
    config_path = config_path.expanduser().resolve()
    blockers: list[dict[str, Any]] = []
    warnings: list[dict[str, Any]] = []
    rows: list[IngressRow] = []

    if not config_path.exists():
        blockers.append(_issue("config_missing", f"path does not exist: {config_path}"))
    elif not config_path.is_file():
        blockers.append(_issue("config_not_file", f"path is not a file: {config_path}"))
    else:
        try:
            config = yaml.safe_load(config_path.read_text(encoding="utf-8")) or {}
        except Exception as exc:  # pragma: no cover - exact parser errors vary by PyYAML
            blockers.append(_issue("config_parse_error", str(exc)))
            config = {}
        ingress = config.get("ingress")
        if not isinstance(ingress, list):
            blockers.append(_issue("ingress_missing", "cloudflared config must contain an ingress list"))
        else:
            rows = [_row_from_raw(index, raw) for index, raw in enumerate(ingress)]
            blockers.extend(_validate_rows(rows, app_host, expected_origin, forbidden_hosts))
            warnings.extend(_warning_rows(rows))

    return {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "status": "blocked" if blockers else "passed",
        "inputs": {
            "config": str(config_path),
            "app_host": _normalize_host(app_host),
            "expected_origin": expected_origin,
            "forbidden_hosts": [_normalize_host(host) for host in forbidden_hosts],
        },
        "summary": {
            "ingress_rows": len(rows),
            "blockers": len(blockers),
            "warnings": len(warnings),
        },
        "ingress": [
            {
                "index": row.index,
                "hostname": row.hostname,
                "path": row.path,
                "service": row.service,
            }
            for row in rows
        ],
        "blockers": blockers,
        "warnings": warnings,
    }


def render_text_report(report: dict[str, Any]) -> str:
    summary = report["summary"]
    inputs = report["inputs"]
    lines = [
        "Rust public tunnel preflight",
        f"status: {report['status']}",
        f"config: {inputs['config']}",
        f"app_host: {inputs['app_host']}",
        f"expected_origin: {inputs['expected_origin']}",
        f"forbidden_hosts: {', '.join(inputs['forbidden_hosts']) or '<none>'}",
        f"ingress_rows: {summary['ingress_rows']}",
        f"blockers: {summary['blockers']}",
        f"warnings: {summary['warnings']}",
        "",
        "Ingress:",
    ]
    for row in report["ingress"]:
        hostname = row["hostname"] if row["hostname"] is not None else "<catch-all>"
        path = f" {row['path']}" if row["path"] is not None else ""
        service = row["service"] if row["service"] is not None else "<missing>"
        lines.append(f"  {row['index']}: {hostname}{path} -> {service}")
    if report["blockers"]:
        lines.extend(["", "Blockers:"])
        for issue in report["blockers"]:
            lines.append(f"  {issue['kind']}: {issue['detail']}")
    if report["warnings"]:
        lines.extend(["", "Warnings:"])
        for issue in report["warnings"]:
            lines.append(f"  {issue['kind']}: {issue['detail']}")
    return "\n".join(lines)


def _validate_rows(
    rows: list[IngressRow],
    app_host: str,
    expected_origin: str,
    forbidden_hosts: tuple[str, ...],
) -> list[dict[str, Any]]:
    blockers: list[dict[str, Any]] = []
    normalized_app_host = _normalize_host(app_host)
    normalized_forbidden = {_normalize_host(host) for host in forbidden_hosts if host.strip()}

    unscoped_app_rows = [
        row
        for row in rows
        if _normalize_host(row.hostname) == normalized_app_host and row.path is None
    ]
    all_app_rows = [row for row in rows if _normalize_host(row.hostname) == normalized_app_host]
    if not unscoped_app_rows:
        blockers.append(
            _issue(
                "app_host_missing",
                f"{normalized_app_host} is not present as an unscoped ingress host",
            )
        )
    elif len(unscoped_app_rows) > 1:
        blockers.append(
            _issue(
                "app_host_duplicate",
                f"{normalized_app_host} appears {len(unscoped_app_rows)} times",
            )
        )
    elif unscoped_app_rows[0].service != expected_origin:
        blockers.append(
            _issue(
                "app_host_wrong_origin",
                (
                    f"{normalized_app_host} routes to {unscoped_app_rows[0].service!r}; "
                    f"expected {expected_origin!r}"
                ),
                index=unscoped_app_rows[0].index,
            )
        )
    for row in all_app_rows:
        if row.path is not None:
            blockers.append(
                _issue(
                    "app_host_path_scoped",
                    f"{normalized_app_host} row is path-scoped to {row.path!r}; expected all paths",
                    index=row.index,
                )
            )

    first_match = _first_host_match(rows, normalized_app_host)
    if first_match is None:
        blockers.append(
            _issue("app_host_no_matching_rule", f"no ingress rule matches {normalized_app_host}")
        )
    elif _normalize_host(first_match.hostname) != normalized_app_host or first_match.path is not None:
        blockers.append(
            _issue(
                "app_host_shadowed",
                (
                    f"row {first_match.index} matches {normalized_app_host} before the "
                    "unscoped app-host rule"
                ),
                index=first_match.index,
            )
        )
    elif first_match.service != expected_origin:
        blockers.append(
            _issue(
                "app_host_first_match_wrong_origin",
                (
                    f"first {normalized_app_host} match routes to {first_match.service!r}; "
                    f"expected {expected_origin!r}"
                ),
                index=first_match.index,
            )
        )

    for row in rows:
        host = _normalize_host(row.hostname)
        if host in normalized_forbidden:
            blockers.append(
                _issue(
                    "forbidden_host_present",
                    f"{host} must not be present in the public ingress config",
                    index=row.index,
                )
            )
        if host.startswith("*.") or host == "*":
            blockers.append(
                _issue(
                    "wildcard_hostname_present",
                    f"wildcard hostname {host!r} is not allowed for SM public ingress",
                    index=row.index,
                )
            )

    if not rows:
        blockers.append(_issue("ingress_empty", "ingress list is empty"))
    elif rows[-1].hostname is not None or rows[-1].service != "http_status:404":
        blockers.append(
            _issue(
                "catch_all_not_404",
                "final ingress row must be catch-all service http_status:404",
                index=rows[-1].index,
            )
        )
    return blockers


def _warning_rows(rows: list[IngressRow]) -> list[dict[str, Any]]:
    warnings: list[dict[str, Any]] = []
    for row in rows:
        if row.service is None:
            warnings.append(
                _issue(
                    "service_missing",
                    f"ingress row {row.index} has no service",
                    severity="warning",
                    index=row.index,
                )
            )
    return warnings


def _row_from_raw(index: int, raw: Any) -> IngressRow:
    if not isinstance(raw, dict):
        return IngressRow(index=index, hostname=None, path=None, service=None)
    hostname = raw.get("hostname")
    path = raw.get("path")
    service = raw.get("service")
    return IngressRow(
        index=index,
        hostname=str(hostname).strip() if hostname is not None else None,
        path=str(path).strip() if path is not None else None,
        service=str(service).strip() if service is not None else None,
    )


def _first_host_match(rows: list[IngressRow], hostname: str) -> IngressRow | None:
    for row in rows:
        row_host = _normalize_host(row.hostname)
        if row.hostname is None or row_host == hostname or _wildcard_matches(row_host, hostname):
            return row
    return None


def _wildcard_matches(pattern: str, hostname: str) -> bool:
    if pattern == "*":
        return True
    if pattern.startswith("*."):
        suffix = pattern[1:]
        return hostname.endswith(suffix) and hostname != pattern[2:]
    return False


def _normalize_host(hostname: str | None) -> str:
    if hostname is None:
        return ""
    return hostname.strip().rstrip(".").lower()


def _issue(
    kind: str,
    detail: str,
    *,
    severity: str = "blocker",
    index: int | None = None,
) -> dict[str, Any]:
    issue: dict[str, Any] = {"kind": kind, "severity": severity, "detail": detail}
    if index is not None:
        issue["index"] = index
    return issue


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--app-host", default=DEFAULT_APP_HOST)
    parser.add_argument("--expected-origin", default=DEFAULT_EXPECTED_ORIGIN)
    parser.add_argument(
        "--forbid-host",
        action="append",
        default=list(DEFAULT_FORBIDDEN_HOSTS),
        help="Hostnames that must not appear in ingress. Repeatable.",
    )
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--fail-on-blockers", action="store_true")
    args = parser.parse_args(argv)

    report = build_public_tunnel_preflight_report(
        config_path=args.config,
        app_host=args.app_host,
        expected_origin=args.expected_origin,
        forbidden_hosts=tuple(args.forbid_host),
    )
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render_text_report(report))
    if args.fail_on_blockers and report["blockers"]:
        return 1
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
