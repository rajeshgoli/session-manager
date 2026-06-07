"""Executable contract harness for the Rust migration.

The harness intentionally starts with a safe, manifest-driven subset. Checks
that need a live server, session id, credentials, or mutating opt-in are
reported as skipped until the caller supplies the preconditions.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable


MANIFEST_PATH = Path(__file__).with_name("contracts_manifest.json")
DEFAULT_TIMEOUT_SECONDS = 5.0


@dataclass(frozen=True)
class ContractCheck:
    id: str
    surface: str
    classification: str
    target: str
    safety: str
    source: str
    preconditions: tuple[str, ...]
    method: str | None = None
    path: str | None = None
    expected_status: tuple[int, ...] = ()
    command: tuple[str, ...] = ()
    expected_exit: tuple[int, ...] = ()
    expected_output_contains_any: tuple[str, ...] = ()
    body: Any = None
    read_mode: str = "bytes"
    read_bytes: int = 1024
    expected_body_contains_any: tuple[str, ...] = ()

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> "ContractCheck":
        return cls(
            id=str(raw["id"]),
            surface=str(raw["surface"]),
            classification=str(raw["classification"]),
            target=str(raw["target"]),
            safety=str(raw["safety"]),
            source=str(raw["source"]),
            preconditions=tuple(raw.get("preconditions", [])),
            method=raw.get("method"),
            path=raw.get("path"),
            expected_status=tuple(int(v) for v in raw.get("expected_status", [])),
            command=tuple(str(v) for v in raw.get("command", [])),
            expected_exit=tuple(int(v) for v in raw.get("expected_exit", [])),
            expected_output_contains_any=tuple(
                str(v) for v in raw.get("expected_output_contains_any", [])
            ),
            body=raw.get("body"),
            read_mode=str(raw.get("read_mode", "bytes")),
            read_bytes=int(raw.get("read_bytes", 1024)),
            expected_body_contains_any=tuple(
                str(v) for v in raw.get("expected_body_contains_any", [])
            ),
        )


@dataclass(frozen=True)
class ContractManifest:
    schema_version: int
    source_spec: str
    artifacts: tuple[str, ...]
    checks: tuple[ContractCheck, ...]

    @classmethod
    def load(cls, path: Path = MANIFEST_PATH) -> "ContractManifest":
        raw = json.loads(path.read_text())
        return cls(
            schema_version=int(raw["schema_version"]),
            source_spec=str(raw["source_spec"]),
            artifacts=tuple(str(v) for v in raw.get("artifacts", [])),
            checks=tuple(ContractCheck.from_dict(item) for item in raw["checks"]),
        )


@dataclass(frozen=True)
class CheckResult:
    id: str
    status: str
    classification: str
    target: str
    surface: str
    elapsed_ms: float | None
    detail: str
    source: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "status": self.status,
            "classification": self.classification,
            "target": self.target,
            "surface": self.surface,
            "elapsed_ms": self.elapsed_ms,
            "detail": self.detail,
            "source": self.source,
        }


def checks_for_target(
    checks: Iterable[ContractCheck], target: str, include_mutating: bool
) -> list[ContractCheck]:
    selected: list[ContractCheck] = []
    for check in checks:
        if check.target == "rust_only" and target != "rust":
            continue
        if check.target == "python_only" and target != "python":
            continue
        if check.safety == "mutating" and not include_mutating:
            selected.append(check)
            continue
        selected.append(check)
    return selected


def run_checks(
    manifest: ContractManifest,
    *,
    target: str,
    base_url: str | None,
    sm_binary: str,
    session_id: str | None,
    fixtures: dict[str, str] | None = None,
    include_mutating: bool,
    timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
) -> list[CheckResult]:
    results: list[CheckResult] = []
    fixture_values = dict(fixtures or {})
    if session_id:
        fixture_values.setdefault("session_id", session_id)
    for check in checks_for_target(manifest.checks, target, include_mutating):
        skip_reason = _skip_reason(
            check,
            base_url=base_url,
            sm_binary=sm_binary,
            fixtures=fixture_values,
            include_mutating=include_mutating,
        )
        if skip_reason:
            results.append(_result(check, "skipped", None, skip_reason))
            continue

        if check.surface == "http":
            results.append(_run_http_check(check, base_url or "", fixture_values, timeout_seconds))
        elif check.surface == "cli":
            results.append(_run_cli_check(check, sm_binary, timeout_seconds))
        else:
            results.append(_result(check, "skipped", None, f"unsupported surface {check.surface}"))
    return results


def summarize(results: Iterable[CheckResult]) -> dict[str, int]:
    summary = {"passed": 0, "failed": 0, "skipped": 0}
    for result in results:
        summary[result.status] = summary.get(result.status, 0) + 1
    return summary


def _skip_reason(
    check: ContractCheck,
    *,
    base_url: str | None,
    sm_binary: str,
    fixtures: dict[str, str],
    include_mutating: bool,
) -> str | None:
    if "live_server" in check.preconditions and not base_url:
        return "live server URL not supplied"
    if "sm_cli" in check.preconditions and not shutil.which(sm_binary):
        return f"sm CLI not found: {sm_binary}"
    for precondition in check.preconditions:
        if precondition == "session_id" and not fixtures.get("session_id"):
            return "session id not supplied"
        if precondition.startswith("fixture:"):
            fixture_name = precondition.split(":", 1)[1]
            if not fixtures.get(fixture_name):
                return f"fixture not supplied: {fixture_name}"
    if "mutating_opt_in" in check.preconditions and not include_mutating:
        return "mutating check requires --include-mutating"
    return None


def _run_http_check(
    check: ContractCheck, base_url: str, fixtures: dict[str, str], timeout_seconds: float
) -> CheckResult:
    assert check.method and check.path
    path = _render_template(check.path, fixtures)
    url = base_url.rstrip("/") + path
    start = time.perf_counter()
    data = None
    headers = {}
    if check.method not in {"GET", "HEAD"}:
        body = {} if check.body is None else _render_template(check.body, fixtures)
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, headers=headers, method=check.method)
    body_text = ""
    try:
        with urllib.request.urlopen(request, timeout=timeout_seconds) as response:
            status = response.status
            body_text = _read_response_text(response, check)
    except urllib.error.HTTPError as exc:
        status = exc.code
        body_text = _read_response_text(exc, check)
    except (urllib.error.URLError, TimeoutError) as exc:
        return _result(check, "failed", _elapsed_ms(start), f"live server unavailable: {exc}")

    body_ok = not check.expected_body_contains_any or any(
        expected in body_text for expected in check.expected_body_contains_any
    )
    if status in check.expected_status and body_ok:
        return _result(check, "passed", _elapsed_ms(start), f"HTTP {status}")
    if not body_ok:
        return _result(
            check,
            "failed",
            _elapsed_ms(start),
            f"HTTP {status}; body missing one of {list(check.expected_body_contains_any)}",
        )
    return _result(
        check,
        "failed",
        _elapsed_ms(start),
        f"expected HTTP {list(check.expected_status)}, got {status}",
    )


def _run_cli_check(check: ContractCheck, sm_binary: str, timeout_seconds: float) -> CheckResult:
    start = time.perf_counter()
    try:
        completed = subprocess.run(
            [sm_binary, *check.command],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout_seconds,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return _result(check, "failed", _elapsed_ms(start), f"timed out after {timeout_seconds}s")
    output = (completed.stdout + completed.stderr).lower()
    exit_ok = completed.returncode in check.expected_exit
    contains_any = not check.expected_output_contains_any or any(
        needle.lower() in output for needle in check.expected_output_contains_any
    )
    if exit_ok and contains_any:
        return _result(check, "passed", _elapsed_ms(start), f"exit {completed.returncode}")

    detail = f"exit {completed.returncode}; expected {list(check.expected_exit)}"
    if check.expected_output_contains_any and not contains_any:
        detail += f"; missing one of {list(check.expected_output_contains_any)}"
    return _result(check, "failed", _elapsed_ms(start), detail)


def _read_response_text(response: Any, check: ContractCheck) -> str:
    if check.read_mode == "line":
        raw = response.readline(check.read_bytes)
    else:
        raw = response.read(check.read_bytes)
    return raw.decode("utf-8", errors="replace")


_TEMPLATE_PATTERN = re.compile(r"\{([A-Za-z_][A-Za-z0-9_]*)\}")


def _render_template(value: Any, fixtures: dict[str, str]) -> Any:
    if isinstance(value, str):
        def replace(match: re.Match[str]) -> str:
            key = match.group(1)
            return fixtures.get(key, match.group(0))

        return _TEMPLATE_PATTERN.sub(replace, value)
    if isinstance(value, list):
        return [_render_template(item, fixtures) for item in value]
    if isinstance(value, dict):
        return {key: _render_template(item, fixtures) for key, item in value.items()}
    return value


def _result(
    check: ContractCheck, status: str, elapsed_ms: float | None, detail: str
) -> CheckResult:
    return CheckResult(
        id=check.id,
        status=status,
        classification=check.classification,
        target=check.target,
        surface=check.surface,
        elapsed_ms=elapsed_ms,
        detail=detail,
        source=check.source,
    )


def _elapsed_ms(start: float) -> float:
    return round((time.perf_counter() - start) * 1000, 3)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=MANIFEST_PATH)
    parser.add_argument("--target", choices=("python", "rust"), default="python")
    parser.add_argument("--base-url", default=None)
    parser.add_argument("--sm-binary", default="sm")
    parser.add_argument("--session-id", default=None)
    parser.add_argument(
        "--fixture",
        action="append",
        default=[],
        metavar="KEY=VALUE",
        help="Fixture value for manifest substitutions; repeatable",
    )
    parser.add_argument("--include-mutating", action="store_true")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--json", action="store_true", help="Emit JSON report")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    manifest = ContractManifest.load(args.manifest)
    fixtures = _parse_fixtures(args.fixture)
    results = run_checks(
        manifest,
        target=args.target,
        base_url=args.base_url,
        sm_binary=args.sm_binary,
        session_id=args.session_id,
        fixtures=fixtures,
        include_mutating=args.include_mutating,
        timeout_seconds=args.timeout,
    )
    summary = summarize(results)
    if args.json:
        print(
            json.dumps(
                {
                    "schema_version": manifest.schema_version,
                    "target": args.target,
                    "summary": summary,
                    "results": [result.to_dict() for result in results],
                },
                indent=2,
                sort_keys=True,
            )
        )
    else:
        for result in results:
            elapsed = "" if result.elapsed_ms is None else f" ({result.elapsed_ms} ms)"
            print(f"{result.status.upper():7} {result.id}{elapsed}: {result.detail}")
        print(f"Summary: {summary}")
    return 1 if summary.get("failed", 0) else 0


def _parse_fixtures(raw_items: list[str]) -> dict[str, str]:
    fixtures: dict[str, str] = {}
    for item in raw_items:
        if "=" not in item:
            raise SystemExit(f"--fixture must be KEY=VALUE, got: {item}")
        key, value = item.split("=", 1)
        key = key.strip()
        if not key:
            raise SystemExit(f"--fixture key cannot be empty: {item}")
        fixtures[key] = value
    return fixtures


if __name__ == "__main__":
    raise SystemExit(main())
