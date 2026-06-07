"""Safe Python baseline runner for the Rust migration value gate."""

from __future__ import annotations

import argparse
import json
import platform
import statistics
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

from .contracts import ContractManifest, run_checks


def run_baseline(
    *,
    base_url: str | None,
    sm_binary: str,
    repetitions: int,
    output: Path | None,
    server_pid: int | None,
) -> dict[str, Any]:
    start = time.perf_counter()
    manifest = ContractManifest.load()
    runs = []
    for _ in range(repetitions):
        results = run_checks(
            manifest,
            target="python",
            base_url=base_url,
            sm_binary=sm_binary,
            session_id=None,
            fixtures={},
            include_mutating=False,
        )
        runs.append([result.to_dict() for result in results])

    report = {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "target": "python",
        "host": {
            "system": platform.system(),
            "machine": platform.machine(),
            "python": platform.python_version(),
        },
        "inputs": {
            "base_url": base_url,
            "sm_binary": sm_binary,
            "repetitions": repetitions,
            "server_pid": server_pid,
        },
        "memory": _memory_snapshot(server_pid, base_url),
        "latency": _latency_summary(runs),
        "runs": runs,
        "python_hardening_variants": [
            {
                "variant": "disable already-unused integrations by config",
                "status": "not_measured",
                "reason": "requires controlled config copy and restart rehearsal",
            },
            {
                "variant": "reduce retained event/log scan windows where compatible",
                "status": "not_measured",
                "reason": "requires retained-state workload and compatibility fixture comparison",
            },
            {
                "variant": "defer startup background work not needed for first response",
                "status": "not_measured",
                "reason": "requires startup harness and controlled service restart",
            },
            {
                "variant": "remove retired surfaces while isolating optional retained integrations",
                "status": "not_measured",
                "reason": "requires feature-gated Python patch or Rust prototype comparison",
            },
            {
                "variant": "reduce logging verbosity or request timing thresholds where compatible",
                "status": "not_measured",
                "reason": "requires log/telemetry compatibility comparison",
            },
        ],
    }
    report["elapsed_seconds"] = round(time.perf_counter() - start, 3)
    if output:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    return report


def _latency_summary(runs: list[list[dict[str, Any]]]) -> dict[str, Any]:
    by_id: dict[str, list[float]] = {}
    skipped: dict[str, str] = {}
    failed: dict[str, str] = {}
    for run in runs:
        for result in run:
            check_id = result["id"]
            if result["status"] == "passed" and result["elapsed_ms"] is not None:
                by_id.setdefault(check_id, []).append(float(result["elapsed_ms"]))
            elif result["status"] == "skipped":
                skipped[check_id] = result["detail"]
            elif result["status"] == "failed":
                failed[check_id] = result["detail"]

    summaries = {}
    for check_id, values in by_id.items():
        ordered = sorted(values)
        summaries[check_id] = {
            "count": len(values),
            "min_ms": ordered[0],
            "median_ms": statistics.median(ordered),
            "max_ms": ordered[-1],
            "p95_ms": _percentile(ordered, 0.95),
        }
    return {"summaries": summaries, "skipped": skipped, "failed": failed}


def _percentile(sorted_values: list[float], quantile: float) -> float:
    if not sorted_values:
        return 0.0
    if len(sorted_values) == 1:
        return sorted_values[0]
    index = (len(sorted_values) - 1) * quantile
    lower = int(index)
    upper = min(lower + 1, len(sorted_values) - 1)
    weight = index - lower
    return round(sorted_values[lower] * (1 - weight) + sorted_values[upper] * weight, 3)


def _memory_snapshot(server_pid: int | None, base_url: str | None) -> dict[str, Any]:
    if server_pid is None:
        server_pid = _discover_server_pid(_port_from_base_url(base_url))
    if server_pid is None:
        return {
            "status": "skipped",
            "reason": "server pid not supplied and no process found on selected base-url port",
        }

    try:
        completed = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(server_pid)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=3,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return {"status": "skipped", "pid": server_pid, "reason": str(exc)}
    if completed.returncode != 0 or not completed.stdout.strip():
        return {
            "status": "skipped",
            "pid": server_pid,
            "reason": completed.stderr.strip() or "ps did not return RSS",
        }
    rss_kib = int(completed.stdout.strip().splitlines()[0])
    return {
        "status": "measured",
        "pid": server_pid,
        "rss_kib": rss_kib,
        "rss_mib": round(rss_kib / 1024, 3),
        "uss": {
            "status": "unknown",
            "reason": "USS requires platform-specific tooling not used by the safe stdlib runner",
        },
    }


def _port_from_base_url(base_url: str | None) -> int:
    if not base_url:
        return 8420
    parsed = urlparse(base_url)
    if parsed.port:
        return parsed.port
    if parsed.scheme == "https":
        return 443
    if parsed.scheme == "http":
        return 80
    return 8420


def _discover_server_pid(port: int) -> int | None:
    try:
        completed = subprocess.run(
            ["lsof", "-ti", f"tcp:{port}"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=3,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    for line in completed.stdout.splitlines():
        line = line.strip()
        if line.isdigit():
            return int(line)
    return None


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default="http://127.0.0.1:8420")
    parser.add_argument("--sm-binary", default="sm")
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--server-pid", type=int, default=None)
    parser.add_argument("--output", type=Path, default=None)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    report = run_baseline(
        base_url=args.base_url,
        sm_binary=args.sm_binary,
        repetitions=args.repetitions,
        output=args.output,
        server_pid=args.server_pid,
    )
    print(json.dumps(report, indent=2, sort_keys=True))
    return 1 if report["latency"]["failed"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
