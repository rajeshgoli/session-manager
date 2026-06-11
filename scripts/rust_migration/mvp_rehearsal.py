"""Run the Rust MVP sidecar rehearsal gate.

The rehearsal is intentionally an observation gate, not a cutover. Python stays
authoritative on port 8420 while Rust runs as a sidecar on port 8421 by default.
The report separates implemented core checks from gap probes so the next
implementation slice is visible without hiding current blockers.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import signal
import subprocess
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

from .baseline import run_baseline
from .contracts import ContractManifest, run_checks, summarize


PYTHON_BASE_URL = "http://127.0.0.1:8420"
RUST_BASE_URL = "http://127.0.0.1:8421"

CORE_READ_CHECK_IDS = (
    "http.health",
    "http.health_detailed",
    "http.auth_session",
    "http.client_bootstrap",
    "http.client_analytics_summary",
    "http.events_state",
    "http.events_sse_hello",
    "http.sessions",
    "http.client_sessions",
    "http.nodes_list",
    "http.codex_review_requests_list",
    "http.queue_jobs_list",
    "http.api_sessions_absent",
    "http.public_watch_operational_data_denied",
    "http.scheduler_remind_retired",
    "http.job_watches_retired",
    "http.queue_policy_runs_retired",
)

MVP_GAP_PROBE_CHECK_IDS = ()

BASELINE_CHECK_IDS = (
    "http.health",
    "http.health_detailed",
    "http.auth_session",
    "http.client_bootstrap",
    "http.events_state",
    "http.sessions",
    "http.client_sessions",
    "http.api_sessions_absent",
)

SHADOW_COMPARE_PATHS = (
    "/health",
    "/health/detailed",
    "/auth/session",
    "/client/bootstrap",
    "/sessions",
    "/client/sessions",
    "/nodes",
    "/events/state",
)


def run_rehearsal(args: argparse.Namespace) -> dict[str, Any]:
    started_at = time.perf_counter()
    output_dir = _resolve_output_dir(args.output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    manifest = ContractManifest.load()
    report: dict[str, Any] = {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "inputs": {
            "python_base_url": args.python_base_url,
            "rust_base_url": args.rust_base_url,
            "config": str(args.config),
            "local_env": str(args.local_env) if args.local_env else None,
            "reuse_rust_sidecar": args.reuse_rust_sidecar,
            "include_gap_probes": not args.core_only,
            "skip_python_health": args.skip_python_health,
            "skip_smoke": args.skip_smoke,
            "skip_baseline": args.skip_baseline,
            "baseline_repetitions": args.baseline_repetitions,
            "skip_shadow": args.skip_shadow,
            "output_dir": str(output_dir),
        },
        "steps": [],
        "blockers": [],
        "artifacts": {},
    }

    rust_process: subprocess.Popen[str] | None = None
    rust_ready = False
    try:
        if not args.skip_python_health:
            python_health = _probe_health(args.python_base_url, args.timeout)
            report["steps"].append({"name": "python_health", **python_health})
            if python_health["status"] != "passed":
                _add_blocker(report, "python_health", python_health["detail"])

        if args.reuse_rust_sidecar:
            rust_health = _probe_health(args.rust_base_url, args.timeout)
            report["steps"].append({"name": "rust_sidecar_reuse_health", **rust_health})
            if rust_health["status"] != "passed":
                _add_blocker(report, "rust_sidecar_reuse_health", rust_health["detail"])
            else:
                rust_ready = True
        else:
            preflight = _probe_health(args.rust_base_url, args.timeout)
            if preflight["status"] == "passed":
                detail = (
                    f"{args.rust_base_url} was already healthy before fresh sidecar "
                    "start; rerun with --reuse-rust-sidecar to target an existing "
                    "process explicitly"
                )
                report["steps"].append(
                    {
                        "name": "rust_sidecar_fresh_start_preflight",
                        "status": "failed",
                        "detail": detail,
                    }
                )
                _add_blocker(report, "rust_sidecar_fresh_start_preflight", detail)
            else:
                report["steps"].append(
                    {
                        "name": "rust_sidecar_fresh_start_preflight",
                        "status": "passed",
                        "detail": preflight["detail"],
                    }
                )
                rust_log_path = output_dir / "rust-sidecar.log"
                start_step: dict[str, Any] = {"name": "rust_sidecar_start"}
                start_failed = False
                try:
                    rust_process = _start_rust_sidecar(args, output_dir)
                except OSError as exc:
                    start_failed = True
                    detail = f"failed to start sidecar: {exc}"
                    start_step.update(
                        {
                            "status": "failed",
                            "detail": detail,
                            "log_path": str(rust_log_path),
                        }
                    )
                    report["steps"].append(start_step)
                    _add_blocker(report, "rust_sidecar_start", detail)
                    rust_process = None
                if rust_process is None:
                    rust_health: dict[str, Any] = {
                        "status": "failed",
                        "detail": start_step["detail"],
                    }
                    sidecar_exited = False
                else:
                    start_step.update(
                        {
                            "status": "passed",
                            "pid": rust_process.pid,
                            "log_path": str(rust_log_path),
                        }
                    )
                    report["steps"].append(start_step)
                    rust_health = _wait_for_sidecar_health(
                        rust_process,
                        args.rust_base_url,
                        timeout_seconds=args.startup_timeout,
                        request_timeout=args.timeout,
                        log_path=rust_log_path,
                    )
                    sidecar_exited = rust_health.pop("process_exited", False)
                    if sidecar_exited:
                        start_step["status"] = "failed"
                        start_step["exit_code"] = rust_health.get("exit_code")
                        start_step["detail"] = rust_health["detail"]
                        _add_blocker(report, "rust_sidecar_start", rust_health["detail"])
                report["steps"].append({"name": "rust_sidecar_health", **rust_health})
                if rust_health["status"] != "passed" and not sidecar_exited and not start_failed:
                    _add_blocker(report, "rust_sidecar_health", rust_health["detail"])
                else:
                    rust_ready = rust_health["status"] == "passed"

        if not rust_ready:
            report["steps"].append(
                {
                    "name": "rust_sidecar_dependent_checks",
                    "status": "skipped",
                    "detail": "Rust sidecar was not verified as ready",
                }
            )
            return _finalize_report(report, output_dir, started_at)

        if not args.skip_smoke:
            smoke = _run_command(
                ["scripts/rust-mvp-smoke.sh"],
                cwd=Path.cwd(),
                timeout_seconds=args.smoke_timeout,
            )
            report["steps"].append({"name": "isolated_runtime_smoke", **smoke})
            if smoke["status"] != "passed":
                _add_blocker(report, "isolated_runtime_smoke", smoke["detail"])

        core_contracts = _run_contract_group(
            manifest,
            name="rust_core_sidecar_contracts",
            target="rust",
            base_url=args.rust_base_url,
            check_ids=set(CORE_READ_CHECK_IDS),
            timeout_seconds=args.timeout,
        )
        report["steps"].append(core_contracts)
        _add_contract_blockers(report, core_contracts, blocker_kind="core_contract")

        if not args.core_only:
            gap_contracts = _run_contract_group(
                manifest,
                name="rust_mvp_gap_probes",
                target="rust",
                base_url=args.rust_base_url,
                check_ids=set(MVP_GAP_PROBE_CHECK_IDS),
                timeout_seconds=args.timeout,
            )
            report["steps"].append(gap_contracts)
            _add_contract_blockers(report, gap_contracts, blocker_kind="mvp_gap")

        if not args.skip_baseline:
            baseline_dir = output_dir / "baseline"
            baseline_dir.mkdir(parents=True, exist_ok=True)
            python_baseline = run_baseline(
                target="python",
                base_url=args.python_base_url,
                sm_binary=args.sm_binary,
                repetitions=args.baseline_repetitions,
                output=baseline_dir / "python-baseline.json",
                server_pid=None,
                check_ids=set(BASELINE_CHECK_IDS),
            )
            report["artifacts"]["python_baseline"] = str(
                baseline_dir / "python-baseline.json"
            )
            report["steps"].append(_baseline_step("python_baseline", python_baseline))

            rust_pid = rust_process.pid if rust_process else None
            rust_baseline = run_baseline(
                target="rust",
                base_url=args.rust_base_url,
                sm_binary=args.sm_binary,
                repetitions=args.baseline_repetitions,
                output=baseline_dir / "rust-baseline.json",
                server_pid=rust_pid,
                check_ids=set(BASELINE_CHECK_IDS),
            )
            report["artifacts"]["rust_baseline"] = str(baseline_dir / "rust-baseline.json")
            report["steps"].append(_baseline_step("rust_baseline", rust_baseline))
            for step in report["steps"][-2:]:
                if step["status"] != "passed":
                    _add_blocker(report, step["name"], step["detail"])

        if not args.skip_shadow:
            shadow = _run_shadow_summary(
                python_base_url=args.python_base_url,
                rust_base_url=args.rust_base_url,
                paths=SHADOW_COMPARE_PATHS,
                timeout_seconds=args.timeout,
                shadow_secret=args.shadow_secret,
            )
            report["steps"].append({"name": "shadow_read_summary", **shadow})
            if shadow["summary"]["failed"]:
                _add_blocker(
                    report,
                    "shadow_read_summary",
                    f"{shadow['summary']['failed']} shadow comparison request(s) failed",
                )
    finally:
        if rust_process is not None:
            _terminate_process_group(rust_process)

    return _finalize_report(report, output_dir, started_at)


def _finalize_report(
    report: dict[str, Any], output_dir: Path, started_at: float
) -> dict[str, Any]:
    report["elapsed_seconds"] = round(time.perf_counter() - started_at, 3)
    report["summary"] = {
        "status": "blocked" if report["blockers"] else "passed",
        "blocker_count": len(report["blockers"]),
        "step_count": len(report["steps"]),
    }
    report_path = output_dir / "mvp-rehearsal-report.json"
    report["artifacts"]["report"] = str(report_path)
    report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    return report


def _resolve_output_dir(raw: Path | None) -> Path:
    if raw is not None:
        return raw
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return Path(".local/rust-mvp-rehearsals") / stamp


def _start_rust_sidecar(
    args: argparse.Namespace, output_dir: Path
) -> subprocess.Popen[str]:
    command = [
        args.cargo,
        "run",
        "-p",
        "sm-server",
        "--bin",
        "sm-server",
        "--",
        "--host",
        _host_from_base_url(args.rust_base_url),
        "--port",
        str(_port_from_base_url(args.rust_base_url)),
        "--config",
        str(args.config),
    ]
    if args.local_env:
        command.extend(["--local-env", str(args.local_env)])
    log_file = (output_dir / "rust-sidecar.log").open("w")
    try:
        return subprocess.Popen(
            command,
            cwd=Path.cwd(),
            text=True,
            stdout=log_file,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    finally:
        log_file.close()


def _probe_health(base_url: str, timeout_seconds: float) -> dict[str, Any]:
    started_at = time.perf_counter()
    try:
        status, body, _headers = _http_request(
            "GET", base_url.rstrip("/") + "/health", timeout_seconds=timeout_seconds
        )
    except Exception as exc:  # noqa: BLE001 - report should preserve the concrete transport error.
        return {
            "status": "failed",
            "elapsed_ms": _elapsed_ms(started_at),
            "detail": str(exc),
        }
    if status == 200 and b'"healthy"' in body:
        return {
            "status": "passed",
            "elapsed_ms": _elapsed_ms(started_at),
            "detail": "HTTP 200 healthy",
        }
    return {
        "status": "failed",
        "elapsed_ms": _elapsed_ms(started_at),
        "detail": f"expected healthy HTTP 200, got HTTP {status}",
    }


def _wait_for_health(
    base_url: str, *, timeout_seconds: float, request_timeout: float
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout_seconds
    last_detail = "not attempted"
    while time.monotonic() < deadline:
        result = _probe_health(base_url, request_timeout)
        if result["status"] == "passed":
            return result
        last_detail = result["detail"]
        time.sleep(0.5)
    return {
        "status": "failed",
        "elapsed_ms": round(timeout_seconds * 1000, 3),
        "detail": f"timed out waiting for health: {last_detail}",
    }


def _wait_for_sidecar_health(
    process: subprocess.Popen[str],
    base_url: str,
    *,
    timeout_seconds: float,
    request_timeout: float,
    log_path: Path,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout_seconds
    last_detail = "not attempted"
    started_at = time.perf_counter()
    while time.monotonic() < deadline:
        exit_code = process.poll()
        if exit_code is not None:
            detail = (
                f"sidecar exited with code {exit_code} before health; "
                f"last health error: {last_detail}"
            )
            log_tail = _tail_file(log_path)
            if log_tail:
                detail = f"{detail}; log tail: {log_tail}"
            return {
                "status": "failed",
                "process_exited": True,
                "exit_code": exit_code,
                "elapsed_ms": _elapsed_ms(started_at),
                "detail": detail,
            }
        result = _probe_health(base_url, request_timeout)
        if result["status"] == "passed":
            return result
        last_detail = result["detail"]
        time.sleep(0.5)
    return {
        "status": "failed",
        "elapsed_ms": round(timeout_seconds * 1000, 3),
        "detail": f"timed out waiting for health: {last_detail}",
    }


def _run_contract_group(
    manifest: ContractManifest,
    *,
    name: str,
    target: str,
    base_url: str,
    check_ids: set[str],
    timeout_seconds: float,
) -> dict[str, Any]:
    started_at = time.perf_counter()
    results = run_checks(
        manifest,
        target=target,
        base_url=base_url,
        sm_binary="sm",
        session_id=None,
        fixtures={},
        include_mutating=False,
        check_ids=check_ids,
        timeout_seconds=timeout_seconds,
    )
    summary = summarize(results)
    return {
        "name": name,
        "status": "passed" if summary.get("failed", 0) == 0 else "failed",
        "elapsed_ms": _elapsed_ms(started_at),
        "summary": summary,
        "results": [result.to_dict() for result in results],
    }


def _run_shadow_summary(
    *,
    python_base_url: str,
    rust_base_url: str,
    paths: tuple[str, ...],
    timeout_seconds: float,
    shadow_secret: str | None,
) -> dict[str, Any]:
    started_at = time.perf_counter()
    results = []
    for path in paths:
        python_url = python_base_url.rstrip("/") + path
        try:
            python_status, python_body, _headers = _http_request(
                "GET", python_url, timeout_seconds=timeout_seconds
            )
            envelope = {
                "request": {"method": "GET", "path": path, "query_string": ""},
                "python_response": {
                    "status": python_status,
                    "body_sha256": hashlib.sha256(python_body).hexdigest(),
                },
            }
            headers = {}
            if shadow_secret:
                headers["x-sm-rust-shadow-secret"] = shadow_secret
            rust_status, rust_body, _headers = _http_request(
                "POST",
                rust_base_url.rstrip("/") + "/__shadow/http",
                body=json.dumps(envelope).encode("utf-8"),
                headers=headers,
                timeout_seconds=timeout_seconds,
            )
            if rust_status != 200:
                results.append(
                    {
                        "path": path,
                        "status": "failed",
                        "detail": f"Rust shadow endpoint returned HTTP {rust_status}",
                    }
                )
                continue
            payload = json.loads(rust_body.decode("utf-8"))
            comparison = payload.get("comparison")
            results.append(
                {
                    "path": path,
                    "status": "passed" if comparison in {"match", "status_match"} else "failed",
                    "comparison": comparison,
                    "support_status": payload.get("support_status"),
                    "python_status": payload.get("python_status"),
                    "predicted_status": payload.get("predicted_status"),
                    "body_sha256_match": payload.get("body_sha256_match"),
                    "detail": payload.get("detail"),
                }
            )
        except Exception as exc:  # noqa: BLE001 - report should preserve the concrete error.
            results.append({"path": path, "status": "failed", "detail": str(exc)})
    failed = sum(1 for result in results if result["status"] == "failed")
    return {
        "status": "passed" if failed == 0 else "failed",
        "elapsed_ms": _elapsed_ms(started_at),
        "summary": {"passed": len(results) - failed, "failed": failed},
        "results": results,
    }


def _baseline_step(name: str, report: dict[str, Any]) -> dict[str, Any]:
    failed = report["latency"]["failed"]
    return {
        "name": name,
        "status": "passed" if not failed else "failed",
        "elapsed_ms": round(float(report.get("elapsed_seconds", 0.0)) * 1000, 3),
        "detail": "baseline completed" if not failed else f"failed checks: {sorted(failed)}",
        "memory": report.get("memory"),
        "latency": report.get("latency"),
    }


def _run_command(
    command: list[str], *, cwd: Path, timeout_seconds: float
) -> dict[str, Any]:
    started_at = time.perf_counter()
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout_seconds,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        return {
            "status": "failed",
            "elapsed_ms": _elapsed_ms(started_at),
            "detail": f"timed out after {timeout_seconds}s",
            "stdout_tail": _tail_text(exc.stdout or ""),
            "stderr_tail": _tail_text(exc.stderr or ""),
        }
    return {
        "status": "passed" if completed.returncode == 0 else "failed",
        "elapsed_ms": _elapsed_ms(started_at),
        "detail": f"exit {completed.returncode}",
        "stdout_tail": _tail_text(completed.stdout),
        "stderr_tail": _tail_text(completed.stderr),
    }


def _http_request(
    method: str,
    url: str,
    *,
    body: bytes | None = None,
    headers: dict[str, str] | None = None,
    timeout_seconds: float,
) -> tuple[int, bytes, dict[str, str]]:
    request = urllib.request.Request(url, data=body, headers=headers or {}, method=method)
    if body is not None and "Content-Type" not in request.headers:
        request.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(request, timeout=timeout_seconds) as response:
            return response.status, response.read(), dict(response.headers)
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read(), dict(exc.headers)


def _add_contract_blockers(
    report: dict[str, Any], step: dict[str, Any], *, blocker_kind: str
) -> None:
    for result in step.get("results", []):
        if result["status"] != "failed":
            continue
        _add_blocker(
            report,
            blocker_kind,
            f"{result['id']}: {result['detail']}",
            check_id=result["id"],
        )


def _add_blocker(
    report: dict[str, Any], kind: str, detail: str, *, check_id: str | None = None
) -> None:
    blocker: dict[str, Any] = {"kind": kind, "detail": detail}
    if check_id:
        blocker["check_id"] = check_id
    report["blockers"].append(blocker)


def _terminate_process_group(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            return
        process.wait(timeout=5)


def _host_from_base_url(base_url: str) -> str:
    parsed = urlparse(base_url)
    return parsed.hostname or "127.0.0.1"


def _port_from_base_url(base_url: str) -> int:
    parsed = urlparse(base_url)
    if parsed.port:
        return parsed.port
    return 443 if parsed.scheme == "https" else 80


def _tail_text(value: str | bytes, max_chars: int = 4000) -> str:
    if isinstance(value, bytes):
        value = value.decode("utf-8", errors="replace")
    return value[-max_chars:]


def _tail_file(path: Path, max_chars: int = 4000) -> str:
    try:
        return path.read_text(errors="replace")[-max_chars:]
    except FileNotFoundError:
        return ""


def _elapsed_ms(started_at: float) -> float:
    return round((time.perf_counter() - started_at) * 1000, 3)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python-base-url", default=PYTHON_BASE_URL)
    parser.add_argument("--rust-base-url", default=RUST_BASE_URL)
    parser.add_argument("--config", type=Path, default=Path("config.yaml"))
    parser.add_argument("--local-env", type=Path, default=None)
    parser.add_argument("--output-dir", type=Path, default=None)
    parser.add_argument("--reuse-rust-sidecar", action="store_true")
    parser.add_argument("--skip-python-health", action="store_true")
    parser.add_argument("--skip-smoke", action="store_true")
    parser.add_argument("--skip-baseline", action="store_true")
    parser.add_argument("--skip-shadow", action="store_true")
    parser.add_argument("--core-only", action="store_true")
    parser.add_argument("--allow-blockers", action="store_true")
    parser.add_argument("--shadow-secret", default=None)
    parser.add_argument("--baseline-repetitions", type=_positive_int, default=3)
    parser.add_argument("--timeout", type=float, default=5.0)
    parser.add_argument("--startup-timeout", type=float, default=60.0)
    parser.add_argument("--smoke-timeout", type=float, default=120.0)
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--sm-binary", default="sm")
    return parser


def _positive_int(raw: str) -> int:
    value = int(raw)
    if value <= 0:
        raise argparse.ArgumentTypeError("must be greater than 0")
    return value


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    report = run_rehearsal(args)
    print(json.dumps(report, indent=2, sort_keys=True))
    if report["blockers"] and not args.allow_blockers:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
