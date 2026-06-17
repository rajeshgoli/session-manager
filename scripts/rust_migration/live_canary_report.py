"""Collect post-cutover Rust canary evidence.

This command observes the live Rust service after ownership has moved to Rust.
It is intentionally non-mutating: it records local Rust health/read checks,
launchd ownership, public tunnel preflight, unauthenticated public edge probes,
and optional Cloudflare/mobile smoke evidence.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable
from urllib.parse import urlparse

from .public_tunnel_preflight import (
    DEFAULT_APP_HOST,
    DEFAULT_CONFIG as DEFAULT_TUNNEL_CONFIG,
    DEFAULT_EXPECTED_ORIGIN,
    DEFAULT_FORBIDDEN_HOSTS,
    build_public_tunnel_preflight_report,
)


DEFAULT_BASE_URL = "http://127.0.0.1:8420"
DEFAULT_SM_BINARY = "target/release/sm"
DEFAULT_RUST_LABEL = "com.rajeshgoli.session-manager-rust"
DEFAULT_LEGACY_HOST = "sm.rajeshgo.li"

CommandRunner = Callable[..., subprocess.CompletedProcess[str]]
JsonValidator = Callable[[dict[str, Any]], tuple[str, str] | None]


def build_live_canary_report(
    *,
    base_url: str = DEFAULT_BASE_URL,
    sm_binary: str = DEFAULT_SM_BINARY,
    rust_label: str = DEFAULT_RUST_LABEL,
    app_host: str = DEFAULT_APP_HOST,
    legacy_host: str = DEFAULT_LEGACY_HOST,
    tunnel_config: Path = DEFAULT_TUNNEL_CONFIG,
    expected_tunnel_origin: str = DEFAULT_EXPECTED_ORIGIN,
    cloudflare_smoke_report: Path | None = None,
    timeout_seconds: float = 5.0,
    command_runner: CommandRunner = subprocess.run,
    urlopen=urllib.request.urlopen,
) -> dict[str, Any]:
    if timeout_seconds <= 0:
        raise ValueError("--timeout must be positive")
    base_url = base_url.rstrip("/")
    checks: list[dict[str, Any]] = []

    checks.append(
        _command_check(
            "launchd.rust_service_running",
            f"Rust launchd label {rust_label} is running",
            ["launchctl", "print", f"gui/{_uid()}/{rust_label}"],
            command_runner=command_runner,
            timeout_seconds=timeout_seconds,
            expected_stdout_fragment="state = running",
        )
    )
    checks.extend(
        [
            _http_check(
                "local.health",
                "local Rust health is healthy",
                f"{base_url}/health",
                expected_status=200,
                expected_json_keys=("status",),
                expected_json_values={"status": "healthy"},
                timeout_seconds=timeout_seconds,
                urlopen=urlopen,
            ),
            _http_check(
                "local.health_detailed",
                "local Rust detailed health responds",
                f"{base_url}/health/detailed",
                expected_status=200,
                expected_json_keys=("status", "checks"),
                timeout_seconds=timeout_seconds,
                urlopen=urlopen,
            ),
            _http_check(
                "local.client_bootstrap",
                "local Rust bootstrap advertises app host",
                f"{base_url}/client/bootstrap",
                expected_status=200,
                expected_json_keys=("auth", "external_access"),
                expected_json_values={
                    "external_access.public_http_host": app_host,
                },
                expected_json_validators=(
                    _mobile_terminal_ws_url_validator(app_host),
                ),
                timeout_seconds=timeout_seconds,
                urlopen=urlopen,
            ),
            _http_check(
                "local.client_sessions",
                "local Rust native session list responds",
                f"{base_url}/client/sessions",
                expected_status=200,
                expected_json_keys=("sessions",),
                timeout_seconds=timeout_seconds,
                urlopen=urlopen,
            ),
            _http_check(
                "local.client_analytics_summary",
                "local Rust native analytics responds",
                f"{base_url}/client/analytics/summary",
                expected_status=200,
                expected_json_keys=("generated_at", "kpis"),
                timeout_seconds=timeout_seconds,
                urlopen=urlopen,
            ),
        ]
    )
    checks.append(
        _command_check(
            "cli.status",
            "Rust CLI status works against live service",
            [sm_binary, "--api-url", base_url, "status"],
            command_runner=command_runner,
            timeout_seconds=timeout_seconds,
        )
    )
    checks.append(
        _public_tunnel_check(
            config_path=tunnel_config,
            app_host=app_host,
            expected_origin=expected_tunnel_origin,
            forbidden_hosts=_forbidden_hosts(legacy_host),
        )
    )
    checks.extend(
        [
            _http_check(
                "public.sm_app_requires_access",
                "public app host denies unauthenticated health before origin",
                f"https://{app_host}/health",
                expected_status=403,
                expected_body_contains_any=(
                    "Cloudflare Access",
                    "error code: 1010",
                ),
                timeout_seconds=timeout_seconds,
                urlopen=urlopen,
            ),
            _http_check(
                "public.legacy_host_absent",
                "legacy public host does not route to origin",
                f"https://{legacy_host}/health",
                expected_status=404,
                expected_statuses=(403, 404),
                timeout_seconds=timeout_seconds,
                urlopen=urlopen,
            ),
        ]
    )
    checks.append(_cloudflare_smoke_check(cloudflare_smoke_report))

    blockers = [
        {
            "check_id": check["id"],
            "kind": check.get("blocker_kind", "check_failed"),
            "detail": check["detail"],
        }
        for check in checks
        if check["status"] == "blocked"
    ]
    return {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "status": "blocked" if blockers else "passed",
        "inputs": {
            "base_url": base_url,
            "sm_binary": sm_binary,
            "rust_label": rust_label,
            "app_host": app_host,
            "legacy_host": legacy_host,
            "tunnel_config": str(tunnel_config),
            "expected_tunnel_origin": expected_tunnel_origin,
            "cloudflare_smoke_report": (
                str(cloudflare_smoke_report) if cloudflare_smoke_report else None
            ),
        },
        "summary": {
            "checks": len(checks),
            "passed": sum(1 for check in checks if check["status"] == "passed"),
            "blocked": len(blockers),
            "skipped": sum(1 for check in checks if check["status"] == "skipped"),
        },
        "checks": checks,
        "blockers": blockers,
    }


def render_text_report(report: dict[str, Any]) -> str:
    lines = [
        "Rust live canary report",
        f"status: {report['status']}",
        f"base_url: {report['inputs']['base_url']}",
        f"app_host: {report['inputs']['app_host']}",
        f"legacy_host: {report['inputs']['legacy_host']}",
        f"checks: {report['summary']['checks']}",
        f"passed: {report['summary']['passed']}",
        f"blocked: {report['summary']['blocked']}",
        f"skipped: {report['summary']['skipped']}",
        "",
        "Checks:",
    ]
    for check in report["checks"]:
        lines.append(f"  {check['status']:7} {check['id']}: {check['detail']}")
    if report["blockers"]:
        lines.extend(["", "Blockers:"])
        for blocker in report["blockers"]:
            lines.append(
                f"  {blocker['check_id']}: {blocker['kind']} - {blocker['detail']}"
            )
    return "\n".join(lines)


def _command_check(
    check_id: str,
    description: str,
    command: list[str],
    *,
    command_runner: CommandRunner,
    timeout_seconds: float,
    expected_stdout_fragment: str | None = None,
) -> dict[str, Any]:
    try:
        result = command_runner(
            command,
            text=True,
            capture_output=True,
            check=False,
            timeout=timeout_seconds,
        )
    except subprocess.TimeoutExpired:
        return _blocked(
            check_id,
            description,
            "command_timeout",
            f"command timed out after {timeout_seconds} seconds",
        )
    except FileNotFoundError as exc:
        return _blocked(check_id, description, "command_not_found", str(exc))
    except Exception as exc:  # pragma: no cover - defensive for platform errors
        return _blocked(check_id, description, "command_error", str(exc))
    stdout = result.stdout or ""
    stderr = result.stderr or ""
    response = {
        "returncode": result.returncode,
        "stdout_preview": stdout[:500],
        "stderr_preview": stderr[:500],
    }
    if result.returncode != 0:
        return _blocked(
            check_id,
            description,
            "command_failed",
            f"command exited {result.returncode}",
            response=response,
        )
    if expected_stdout_fragment and expected_stdout_fragment not in stdout:
        return _blocked(
            check_id,
            description,
            "stdout_mismatch",
            f"stdout did not contain {expected_stdout_fragment!r}",
            response=response,
        )
    return _passed(check_id, description, response=response)


def _http_check(
    check_id: str,
    description: str,
    url: str,
    *,
    expected_status: int,
    timeout_seconds: float,
    urlopen,
    expected_statuses: tuple[int, ...] = (),
    expected_body_contains: str | None = None,
    expected_body_contains_any: tuple[str, ...] = (),
    expected_json_keys: tuple[str, ...] = (),
    expected_json_values: dict[str, Any] | None = None,
    expected_json_validators: tuple[JsonValidator, ...] = (),
) -> dict[str, Any]:
    try:
        request = urllib.request.Request(url, method="GET")
        with urlopen(request, timeout=timeout_seconds) as response:
            status = response.getcode()
            body = response.read()
            headers = dict(response.headers)
    except urllib.error.HTTPError as exc:
        status = exc.code
        body = exc.read()
        headers = dict(exc.headers)
    except Exception as exc:
        return _blocked(check_id, description, "request_failed", str(exc))

    response_summary = _response_summary(status, body, headers)
    allowed_statuses = expected_statuses or (expected_status,)
    if status not in allowed_statuses:
        expected_detail = (
            f"HTTP {allowed_statuses[0]}"
            if len(allowed_statuses) == 1
            else "one of " + ", ".join(f"HTTP {value}" for value in allowed_statuses)
        )
        return _blocked(
            check_id,
            description,
            "status_mismatch",
            f"expected {expected_detail}, got HTTP {status}",
            response=response_summary,
        )
    text = _decode_body(body)
    if expected_body_contains and expected_body_contains not in text:
        return _blocked(
            check_id,
            description,
            "body_mismatch",
            f"response body did not contain {expected_body_contains!r}",
            response=response_summary,
        )
    if expected_body_contains_any and not any(
        value in text for value in expected_body_contains_any
    ):
        return _blocked(
            check_id,
            description,
            "body_mismatch",
            "response body did not contain any of "
            + ", ".join(repr(value) for value in expected_body_contains_any),
            response=response_summary,
        )
    if expected_json_keys or expected_json_values:
        try:
            payload = json.loads(text or "{}")
        except json.JSONDecodeError:
            return _blocked(
                check_id,
                description,
                "invalid_json",
                "response was not valid JSON",
                response=response_summary,
            )
        if not isinstance(payload, dict):
            return _blocked(
                check_id,
                description,
                "json_shape_mismatch",
                "response JSON was not an object",
                response=response_summary,
            )
        for key in expected_json_keys:
            if key not in payload:
                return _blocked(
                    check_id,
                    description,
                    "json_key_missing",
                    f"response JSON omitted {key!r}",
                    response=response_summary,
                )
        for dotted_key, expected_value in (expected_json_values or {}).items():
            actual_value = _json_path(payload, dotted_key)
            if actual_value != expected_value:
                return _blocked(
                    check_id,
                    description,
                    "json_value_mismatch",
                    f"{dotted_key} expected {expected_value!r}, got {actual_value!r}",
                    response=response_summary,
                )
        for validator in expected_json_validators:
            validation_error = validator(payload)
            if validation_error is not None:
                kind, detail = validation_error
                return _blocked(
                    check_id,
                    description,
                    kind,
                    detail,
                    response=response_summary,
                )
    return _passed(check_id, description, response=response_summary)


def _public_tunnel_check(
    *,
    config_path: Path,
    app_host: str,
    expected_origin: str,
    forbidden_hosts: tuple[str, ...],
) -> dict[str, Any]:
    report = build_public_tunnel_preflight_report(
        config_path=config_path,
        app_host=app_host,
        expected_origin=expected_origin,
        forbidden_hosts=forbidden_hosts,
    )
    response = {
        "status": report["status"],
        "summary": report["summary"],
        "blockers": report["blockers"],
        "ingress": report["ingress"],
    }
    if report["status"] != "passed":
        return _blocked(
            "tunnel.public_preflight",
            "public tunnel routes app host to Rust and blocks legacy host",
            "tunnel_preflight_blocked",
            f"{report['summary']['blockers']} tunnel blocker(s)",
            response=response,
        )
    return _passed(
        "tunnel.public_preflight",
        "public tunnel routes app host to Rust and blocks legacy host",
        response=response,
    )


def _cloudflare_smoke_check(path: Path | None) -> dict[str, Any]:
    description = "optional Cloudflare/mobile smoke report is passing when supplied"
    if path is None:
        return {
            "id": "cloudflare.smoke_report",
            "description": description,
            "status": "skipped",
            "detail": "cloudflare smoke report was not supplied",
            "required": False,
        }
    try:
        report = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:
        return _blocked(
            "cloudflare.smoke_report",
            description,
            "smoke_report_unreadable",
            str(exc),
        )
    response = {
        "status": report.get("status"),
        "summary": report.get("summary"),
        "blockers": report.get("blockers", []),
    }
    if report.get("status") != "passed":
        return _blocked(
            "cloudflare.smoke_report",
            description,
            "smoke_report_blocked",
            f"smoke report status is {report.get('status')!r}",
            response=response,
        )
    return _passed("cloudflare.smoke_report", description, response=response)


def _response_summary(status: int, body: bytes, headers: dict[str, Any]) -> dict[str, Any]:
    summary = {
        "status": status,
        "content_type": headers.get("content-type") or headers.get("Content-Type"),
        "body_bytes": len(body),
        "body_preview": _decode_body(body)[:300],
    }
    try:
        payload = json.loads(_decode_body(body) or "{}")
    except json.JSONDecodeError:
        return summary
    if isinstance(payload, dict):
        summary["json_keys"] = sorted(payload.keys())
    return summary


def _json_path(payload: dict[str, Any], dotted_key: str) -> Any:
    current: Any = payload
    for part in dotted_key.split("."):
        if not isinstance(current, dict):
            return None
        current = current.get(part)
    return current


def _mobile_terminal_ws_url_validator(app_host: str) -> JsonValidator:
    def validate(payload: dict[str, Any]) -> tuple[str, str] | None:
        value = _json_path(payload, "external_access.mobile_terminal_ws_url")
        if not isinstance(value, str) or not value:
            return (
                "json_value_mismatch",
                "external_access.mobile_terminal_ws_url was not a non-empty string",
            )
        parsed = urlparse(value)
        if parsed.scheme != "wss":
            return (
                "json_value_mismatch",
                f"mobile terminal URL scheme expected 'wss', got {parsed.scheme!r}",
            )
        if parsed.hostname != app_host:
            return (
                "json_value_mismatch",
                f"mobile terminal URL host expected {app_host!r}, got {parsed.hostname!r}",
            )
        if not parsed.path.endswith("/client/terminal"):
            return (
                "json_value_mismatch",
                "mobile terminal URL path did not end with '/client/terminal'",
            )
        return None

    return validate


def _decode_body(body: bytes) -> str:
    return body.decode("utf-8", errors="replace")


def _passed(check_id: str, description: str, *, response: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": check_id,
        "description": description,
        "status": "passed",
        "detail": "ok",
        "response": response,
    }


def _blocked(
    check_id: str,
    description: str,
    kind: str,
    detail: str,
    *,
    response: dict[str, Any] | None = None,
) -> dict[str, Any]:
    check = {
        "id": check_id,
        "description": description,
        "status": "blocked",
        "blocker_kind": kind,
        "detail": detail,
    }
    if response is not None:
        check["response"] = response
    return check


def _uid() -> str:
    try:
        import os

        return str(os.getuid())
    except Exception:  # pragma: no cover
        return "$(id -u)"


def _forbidden_hosts(legacy_host: str) -> tuple[str, ...]:
    return tuple(dict.fromkeys((*DEFAULT_FORBIDDEN_HOSTS, legacy_host)))


def main(
    argv: list[str] | None = None,
    *,
    command_runner: CommandRunner = subprocess.run,
    urlopen=urllib.request.urlopen,
) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default=DEFAULT_BASE_URL)
    parser.add_argument("--sm-binary", default=DEFAULT_SM_BINARY)
    parser.add_argument("--rust-label", default=DEFAULT_RUST_LABEL)
    parser.add_argument("--app-host", default=DEFAULT_APP_HOST)
    parser.add_argument("--legacy-host", default=DEFAULT_LEGACY_HOST)
    parser.add_argument("--tunnel-config", type=Path, default=DEFAULT_TUNNEL_CONFIG)
    parser.add_argument("--expected-tunnel-origin", default=DEFAULT_EXPECTED_ORIGIN)
    parser.add_argument("--cloudflare-smoke-report", type=Path)
    parser.add_argument("--timeout", type=float, default=5.0)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--fail-on-blockers", action="store_true")
    args = parser.parse_args(argv)

    report = build_live_canary_report(
        base_url=args.base_url,
        sm_binary=args.sm_binary,
        rust_label=args.rust_label,
        app_host=args.app_host,
        legacy_host=args.legacy_host,
        tunnel_config=args.tunnel_config,
        expected_tunnel_origin=args.expected_tunnel_origin,
        cloudflare_smoke_report=args.cloudflare_smoke_report,
        timeout_seconds=args.timeout,
        command_runner=command_runner,
        urlopen=urlopen,
    )
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
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
