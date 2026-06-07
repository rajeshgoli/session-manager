"""Minimal baseline runner for the Rust migration value gate."""

from __future__ import annotations

import argparse
import json
import platform
import re
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
    target: str,
    base_url: str | None,
    sm_binary: str,
    repetitions: int,
    output: Path | None,
    server_pid: int | None,
    check_ids: set[str] | None = None,
) -> dict[str, Any]:
    start = time.perf_counter()
    manifest = ContractManifest.load()
    runs = []
    for _ in range(repetitions):
        results = run_checks(
            manifest,
            target=target,
            base_url=base_url,
            sm_binary=sm_binary,
            session_id=None,
            fixtures={},
            check_ids=check_ids,
            include_mutating=False,
        )
        runs.append([result.to_dict() for result in results])

    report = {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "target": target,
        "host": {
            "system": platform.system(),
            "machine": platform.machine(),
            "python": platform.python_version(),
        },
        "inputs": {
            "target": target,
            "base_url": base_url,
            "sm_binary": sm_binary,
            "repetitions": repetitions,
            "server_pid": server_pid,
            "check_ids": sorted(check_ids) if check_ids else None,
        },
        "memory": _memory_snapshot(server_pid, base_url),
        "latency": _latency_summary(runs),
        "runs": runs,
        "python_hardening_comparison": {
            "status": "owner_waived",
            "reason": (
                "Owner requested a minimal current-Python versus Rust value baseline "
                "and no throwaway Python hardening/config variant work."
            ),
        },
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
    snapshot = {
        "status": "measured",
        "pid": server_pid,
        "rss_kib": rss_kib,
        "rss_mib": round(rss_kib / 1024, 3),
        "uss": {
            "status": "unknown",
            "reason": "USS requires platform-specific tooling not used by the safe stdlib runner",
        },
    }
    footprint = _darwin_physical_footprint(server_pid)
    if footprint:
        snapshot["physical_footprint"] = footprint
    return snapshot


def _darwin_physical_footprint(pid: int) -> dict[str, Any] | None:
    if platform.system() != "Darwin":
        return None
    try:
        completed = subprocess.run(
            ["vmmap", "-summary", str(pid)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return {"status": "skipped", "reason": str(exc)}
    if completed.returncode != 0:
        return {
            "status": "skipped",
            "reason": completed.stderr.strip() or "vmmap did not return a summary",
        }
    for line in completed.stdout.splitlines():
        stripped = line.strip()
        if stripped.startswith("Physical footprint:"):
            raw_value = stripped.split(":", 1)[1].strip()
            return {
                "status": "measured",
                "raw": raw_value,
                "mib": _parse_memory_to_mib(raw_value),
            }
    return {"status": "skipped", "reason": "vmmap summary omitted physical footprint"}


def _parse_memory_to_mib(raw_value: str) -> float | None:
    match = re.match(r"^([0-9]+(?:\.[0-9]+)?)([KMG])$", raw_value.strip())
    if not match:
        return None
    value = float(match.group(1))
    unit = match.group(2)
    if unit == "K":
        return round(value / 1024, 3)
    if unit == "M":
        return round(value, 3)
    if unit == "G":
        return round(value * 1024, 3)
    return None


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
    parser.add_argument("--target", choices=["python", "rust"], default="python")
    parser.add_argument(
        "--base-url",
        default=None,
        help="Server base URL; defaults to http://127.0.0.1:8420 for Python and is required for Rust",
    )
    parser.add_argument("--sm-binary", default="sm")
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--server-pid", type=int, default=None)
    parser.add_argument(
        "--check-id",
        action="append",
        default=[],
        help="Run only the named check; repeat for multiple checks",
    )
    parser.add_argument("--output", type=Path, default=None)
    return parser


def _resolve_base_url(target: str, base_url: str | None) -> str:
    if base_url:
        return base_url
    if target == "rust":
        raise ValueError("--base-url is required when --target rust")
    return "http://127.0.0.1:8420"


def main(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    try:
        base_url = _resolve_base_url(args.target, args.base_url)
    except ValueError as exc:
        parser.error(str(exc))
    report = run_baseline(
        target=args.target,
        base_url=base_url,
        sm_binary=args.sm_binary,
        repetitions=args.repetitions,
        output=args.output,
        server_pid=args.server_pid,
        check_ids=set(args.check_id) if args.check_id else None,
    )
    print(json.dumps(report, indent=2, sort_keys=True))
    return 1 if report["latency"]["failed"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
