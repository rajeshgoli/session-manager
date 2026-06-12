"""Plan a non-destructive Rust shadow observation window."""

from __future__ import annotations

import argparse
import json
import shlex
import shutil
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable
from urllib.parse import urlparse


PYTHON_BASE_URL = "http://127.0.0.1:8420"
RUST_BASE_URL = "http://127.0.0.1:8421"
DEFAULT_LEDGER_PATH = Path("~/.local/share/claude-sessions/rust_shadow.jsonl")
DEFAULT_CONFIG = Path("config.yaml")
DEFAULT_TIMEOUT_SECONDS = 1.0


@dataclass(frozen=True)
class HealthProbe:
    status: str
    detail: str
    elapsed_ms: float


def build_observation_plan(
    *,
    python_base_url: str = PYTHON_BASE_URL,
    rust_base_url: str = RUST_BASE_URL,
    config: Path = DEFAULT_CONFIG,
    local_env: Path | None = None,
    ledger: Path = DEFAULT_LEDGER_PATH,
    cargo: str = "cargo",
    shadow_secret: str = "",
    timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
    reuse_rust_sidecar: bool = False,
    probe_health: Callable[[str, float], HealthProbe] = None,
    cargo_resolver: Callable[[str], str | None] = shutil.which,
) -> dict[str, Any]:
    probe_health = probe_health or _probe_health
    config = config.expanduser()
    local_env = local_env.expanduser() if local_env else None
    ledger = ledger.expanduser()

    python_health = probe_health(python_base_url, timeout_seconds)
    rust_health = probe_health(rust_base_url, timeout_seconds)
    cargo_path = cargo_resolver(cargo)

    blockers: list[dict[str, str]] = []
    warnings: list[dict[str, str]] = []

    if not config.exists():
        blockers.append(
            {
                "kind": "missing_config",
                "detail": f"Rust sidecar config does not exist: {config}",
            }
        )
    if local_env and not local_env.exists():
        blockers.append(
            {
                "kind": "missing_local_env",
                "detail": f"local env file does not exist: {local_env}",
            }
        )
    if cargo_path is None:
        blockers.append(
            {
                "kind": "missing_cargo",
                "detail": f"cargo executable not found: {cargo}",
            }
        )
    if python_health.status != "healthy":
        warnings.append(
            {
                "kind": "python_health",
                "detail": python_health.detail,
            }
        )
    if rust_health.status == "healthy" and not reuse_rust_sidecar:
        blockers.append(
            {
                "kind": "rust_port_in_use",
                "detail": (
                    f"{rust_base_url} is already healthy; pass --reuse-rust-sidecar "
                    "or stop that process before starting a fresh sidecar"
                ),
            }
        )
    elif rust_health.status == "unhealthy":
        blockers.append(
            {
                "kind": "rust_port_unhealthy",
                "detail": rust_health.detail,
            }
        )

    sidecar_command = _sidecar_command(
        cargo=cargo,
        rust_base_url=rust_base_url,
        config=config,
        local_env=local_env,
    )
    report_command = [
        "./venv/bin/python",
        "-m",
        "scripts.rust_migration.shadow_report",
        "--ledger",
        str(ledger),
        "--fail-on-blockers",
    ]

    return {
        "schema_version": 1,
        "status": "blocked" if blockers else "ready",
        "inputs": {
            "python_base_url": python_base_url,
            "rust_base_url": rust_base_url,
            "config": str(config),
            "local_env": str(local_env) if local_env else None,
            "ledger": str(ledger),
            "cargo": cargo,
            "shadow_secret_configured": bool(shadow_secret),
            "reuse_rust_sidecar": reuse_rust_sidecar,
        },
        "checks": {
            "python_health": python_health.__dict__,
            "rust_health": rust_health.__dict__,
            "config_exists": config.exists(),
            "local_env_exists": None if local_env is None else local_env.exists(),
            "cargo_path": cargo_path,
            "ledger_parent_exists": ledger.parent.exists(),
        },
        "blockers": blockers,
        "warnings": warnings,
        "commands": {
            "start_rust_sidecar": None if reuse_rust_sidecar else sidecar_command,
            "start_rust_sidecar_shell": None
            if reuse_rust_sidecar
            else shlex.join(sidecar_command),
            "summarize_shadow_ledger": report_command,
            "summarize_shadow_ledger_shell": shlex.join(report_command),
        },
        "python_config_snippet": _python_config_snippet(
            rust_base_url=rust_base_url,
            ledger=ledger,
            shadow_secret=shadow_secret,
        ),
        "operator_steps": [
            "Start the Rust sidecar command in a separate terminal, unless reusing an existing sidecar.",
            "Add the rust_shadow snippet to local config and restart Python Session Manager deliberately.",
            "Let normal traffic run for the agreed observation window.",
            "Run the shadow ledger report command and treat blockers as cutover blockers.",
        ],
    }


def render_text_plan(plan: dict[str, Any]) -> str:
    lines = [
        "Rust shadow observation plan",
        f"status: {plan['status']}",
        "",
        "Checks:",
    ]
    checks = plan["checks"]
    lines.append(f"  python health: {checks['python_health']['status']} - {checks['python_health']['detail']}")
    lines.append(f"  rust health: {checks['rust_health']['status']} - {checks['rust_health']['detail']}")
    lines.append(f"  config exists: {checks['config_exists']}")
    lines.append(f"  cargo: {checks['cargo_path'] or 'missing'}")
    lines.append(f"  ledger parent exists: {checks['ledger_parent_exists']}")

    if plan["blockers"]:
        lines.extend(["", "Blockers:"])
        for blocker in plan["blockers"]:
            lines.append(f"  {blocker['kind']}: {blocker['detail']}")
    if plan["warnings"]:
        lines.extend(["", "Warnings:"])
        for warning in plan["warnings"]:
            lines.append(f"  {warning['kind']}: {warning['detail']}")

    lines.extend(["", "Start Rust sidecar:"])
    if plan["commands"]["start_rust_sidecar_shell"]:
        lines.append(f"  {plan['commands']['start_rust_sidecar_shell']}")
    else:
        lines.append("  reusing existing Rust sidecar")

    lines.extend(["", "Python local config snippet:"])
    lines.append(plan["python_config_snippet"])

    lines.extend(["", "Summarize ledger:"])
    lines.append(f"  {plan['commands']['summarize_shadow_ledger_shell']}")

    lines.extend(["", "Operator steps:"])
    for index, step in enumerate(plan["operator_steps"], start=1):
        lines.append(f"  {index}. {step}")
    return "\n".join(lines)


def _sidecar_command(
    *,
    cargo: str,
    rust_base_url: str,
    config: Path,
    local_env: Path | None,
) -> list[str]:
    command = [
        cargo,
        "run",
        "-p",
        "sm-server",
        "--bin",
        "sm-server",
        "--",
        "--host",
        _host_from_base_url(rust_base_url),
        "--port",
        str(_port_from_base_url(rust_base_url)),
        "--config",
        str(config),
    ]
    if local_env:
        command.extend(["--local-env", str(local_env)])
    return command


def _python_config_snippet(
    *,
    rust_base_url: str,
    ledger: Path,
    shadow_secret: str,
) -> str:
    lines = [
        "rust_shadow:",
        "  enabled: true",
        f'  endpoint: "{rust_base_url.rstrip("/")}/__shadow/http"',
    ]
    if shadow_secret:
        lines.append(f'  secret: "{shadow_secret}"')
    else:
        lines.append("  # secret: \"local-dev-shared-secret\"")
    lines.extend(
        [
            f'  ledger_path: "{ledger}"',
            "  timeout_seconds: 0.5",
            "  max_body_bytes: 65536",
        ]
    )
    return "\n".join(lines)


def _probe_health(base_url: str, timeout_seconds: float) -> HealthProbe:
    started = time.perf_counter()
    try:
        with urllib.request.urlopen(
            base_url.rstrip("/") + "/health", timeout=timeout_seconds
        ) as response:
            body = response.read(2048)
            status = int(response.status)
    except urllib.error.HTTPError as exc:
        try:
            body = exc.read(2048)
        except Exception:  # noqa: BLE001 - best-effort diagnostic only.
            body = b""
        body_preview = body.decode("utf-8", errors="replace") or exc.reason
        return HealthProbe(
            status="unhealthy",
            detail=f"HTTP {exc.code}: {body_preview}",
            elapsed_ms=_elapsed_ms(started),
        )
    except urllib.error.URLError as exc:
        return HealthProbe(
            status="unreachable",
            detail=str(exc.reason if hasattr(exc, "reason") else exc),
            elapsed_ms=_elapsed_ms(started),
        )
    except Exception as exc:  # noqa: BLE001 - operator report should preserve transport errors.
        return HealthProbe(
            status="unhealthy",
            detail=str(exc),
            elapsed_ms=_elapsed_ms(started),
        )
    if status == 200 and b'"healthy"' in body:
        return HealthProbe(
            status="healthy",
            detail="HTTP 200 healthy",
            elapsed_ms=_elapsed_ms(started),
        )
    return HealthProbe(
        status="unhealthy",
        detail=f"HTTP {status}: {body[:200].decode('utf-8', errors='replace')}",
        elapsed_ms=_elapsed_ms(started),
    )


def _host_from_base_url(base_url: str) -> str:
    parsed = urlparse(base_url)
    return parsed.hostname or "127.0.0.1"


def _port_from_base_url(base_url: str) -> int:
    parsed = urlparse(base_url)
    if parsed.port:
        return parsed.port
    return 443 if parsed.scheme == "https" else 80


def _elapsed_ms(started: float) -> float:
    return round((time.perf_counter() - started) * 1000, 3)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Plan a non-destructive Rust shadow observation window."
    )
    parser.add_argument("--python-base-url", default=PYTHON_BASE_URL)
    parser.add_argument("--rust-base-url", default=RUST_BASE_URL)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--local-env", type=Path, default=None)
    parser.add_argument("--ledger", type=Path, default=DEFAULT_LEDGER_PATH)
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--shadow-secret", default="")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--reuse-rust-sidecar", action="store_true")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--fail-on-blockers", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    plan = build_observation_plan(
        python_base_url=args.python_base_url,
        rust_base_url=args.rust_base_url,
        config=args.config,
        local_env=args.local_env,
        ledger=args.ledger,
        cargo=args.cargo,
        shadow_secret=args.shadow_secret,
        timeout_seconds=args.timeout,
        reuse_rust_sidecar=args.reuse_rust_sidecar,
    )
    if args.json:
        print(json.dumps(plan, indent=2, sort_keys=True))
    else:
        print(render_text_plan(plan))
    if args.fail_on_blockers and plan["blockers"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
