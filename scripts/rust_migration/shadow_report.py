"""Summarize Rust shadow comparison ledgers for cutover observation."""

from __future__ import annotations

import argparse
import json
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


DEFAULT_LEDGER_PATH = Path("~/.local/share/claude-sessions/rust_shadow.jsonl")
BLOCKING_COMPARISONS = {
    "body_mismatch",
    "shadow_endpoint_non_json",
    "status_mismatch",
}


def summarize_ledger(ledger_path: Path) -> dict[str, Any]:
    ledger_path = ledger_path.expanduser()
    route_stats: dict[str, dict[str, Any]] = defaultdict(_route_summary)
    comparison_counts: Counter[str] = Counter()
    support_status_counts: Counter[str] = Counter()
    blockers: list[dict[str, Any]] = []
    invalid_rows: list[dict[str, Any]] = []
    row_count = 0

    if ledger_path.exists():
        with ledger_path.open("r", encoding="utf-8") as handle:
            for line_number, line in enumerate(handle, start=1):
                stripped = line.strip()
                if not stripped:
                    continue
                try:
                    record = json.loads(stripped)
                except json.JSONDecodeError as exc:
                    blocker = {
                        "kind": "invalid_json",
                        "line": line_number,
                        "detail": str(exc),
                    }
                    invalid_rows.append(blocker)
                    blockers.append(blocker)
                    continue
                if not isinstance(record, dict):
                    blocker = {
                        "kind": "invalid_row_shape",
                        "line": line_number,
                        "detail": (
                            f"expected JSON object, got {type(record).__name__}"
                        ),
                    }
                    invalid_rows.append(blocker)
                    blockers.append(blocker)
                    continue
                row_count += 1
                _record_row(
                    record=record,
                    line_number=line_number,
                    route_stats=route_stats,
                    comparison_counts=comparison_counts,
                    support_status_counts=support_status_counts,
                    blockers=blockers,
                )

    route_summaries = []
    for route, summary in sorted(route_stats.items()):
        route_summaries.append(
            {
                "route": route,
                "rows": summary["rows"],
                "comparisons": dict(sorted(summary["comparisons"].items())),
                "support_statuses": dict(sorted(summary["support_statuses"].items())),
                "blockers": summary["blockers"],
            }
        )

    status = "passed"
    if blockers:
        status = "blocked"
    elif row_count == 0:
        status = "no_data"

    return {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "ledger_path": str(ledger_path),
        "status": status,
        "row_count": row_count,
        "invalid_row_count": len(invalid_rows),
        "comparison_counts": dict(sorted(comparison_counts.items())),
        "support_status_counts": dict(sorted(support_status_counts.items())),
        "route_summaries": route_summaries,
        "blockers": blockers,
    }


def render_text_report(report: dict[str, Any]) -> str:
    lines = [
        "Rust shadow observation report",
        f"ledger: {report['ledger_path']}",
        f"status: {report['status']}",
        f"rows: {report['row_count']}",
        f"blockers: {len(report['blockers'])}",
        "",
        "Comparisons:",
    ]
    if report["comparison_counts"]:
        for comparison, count in report["comparison_counts"].items():
            lines.append(f"  {comparison}: {count}")
    else:
        lines.append("  none")

    lines.extend(["", "Routes:"])
    if report["route_summaries"]:
        for route in report["route_summaries"]:
            comparisons = ", ".join(
                f"{key}={value}" for key, value in route["comparisons"].items()
            )
            support = ", ".join(
                f"{key}={value}" for key, value in route["support_statuses"].items()
            )
            lines.append(
                f"  {route['route']}: rows={route['rows']}; "
                f"comparisons={comparisons or 'none'}; "
                f"support={support or 'none'}; blockers={len(route['blockers'])}"
            )
    else:
        lines.append("  none")

    if report["blockers"]:
        lines.extend(["", "Blockers:"])
        for blocker in report["blockers"]:
            route = blocker.get("route", "ledger")
            line = blocker.get("line", "?")
            detail = blocker.get("detail")
            suffix = f" - {detail}" if detail else ""
            lines.append(f"  line {line} {route}: {blocker['kind']}{suffix}")

    return "\n".join(lines)


def _route_summary() -> dict[str, Any]:
    return {
        "rows": 0,
        "comparisons": Counter(),
        "support_statuses": Counter(),
        "blockers": [],
    }


def _record_row(
    *,
    record: dict[str, Any],
    line_number: int,
    route_stats: dict[str, dict[str, Any]],
    comparison_counts: Counter[str],
    support_status_counts: Counter[str],
    blockers: list[dict[str, Any]],
) -> None:
    route = _route_key(record)
    summary = route_stats[route]
    summary["rows"] += 1

    rust_result = record.get("rust_result")
    comparison = _comparison_for(record)
    comparison_counts[comparison] += 1
    summary["comparisons"][comparison] += 1

    support_status = None
    if isinstance(rust_result, dict):
        support_status = rust_result.get("support_status")
    if support_status:
        support_status = str(support_status)
        support_status_counts[support_status] += 1
        summary["support_statuses"][support_status] += 1

    blocker = _blocker_for(
        record=record,
        line_number=line_number,
        route=route,
        comparison=comparison,
        support_status=support_status,
    )
    if blocker:
        blockers.append(blocker)
        summary["blockers"].append(blocker)


def _route_key(record: dict[str, Any]) -> str:
    method = str(record.get("method") or "?").upper()
    path = str(record.get("path") or "?")
    return f"{method} {path}"


def _comparison_for(record: dict[str, Any]) -> str:
    if record.get("shadow_error"):
        return "shadow_error"
    rust_result = record.get("rust_result")
    if not isinstance(rust_result, dict):
        return "missing_rust_result"
    comparison = rust_result.get("comparison")
    return str(comparison or "missing_comparison")


def _blocker_for(
    *,
    record: dict[str, Any],
    line_number: int,
    route: str,
    comparison: str,
    support_status: str | None,
) -> dict[str, Any] | None:
    if record.get("shadow_error"):
        return {
            "kind": "shadow_error",
            "line": line_number,
            "route": route,
            "detail": record.get("shadow_error_message") or record.get("shadow_error"),
        }
    rust_http_status = _optional_int(record.get("rust_http_status"))
    if rust_http_status == "invalid":
        return {
            "kind": "invalid_rust_http_status",
            "line": line_number,
            "route": route,
            "detail": str(record.get("rust_http_status")),
        }
    if rust_http_status is not None and rust_http_status != 200:
        return {
            "kind": "shadow_http_status",
            "line": line_number,
            "route": route,
            "detail": f"shadow endpoint returned HTTP {rust_http_status}",
        }
    if comparison in BLOCKING_COMPARISONS:
        blocker = {
            "kind": comparison,
            "line": line_number,
            "route": route,
        }
        if support_status:
            blocker["support_status"] = support_status
        predicted_status = _rust_result_value(record, "predicted_status")
        if predicted_status is not None:
            blocker["predicted_status"] = predicted_status
        blocker["python_status"] = record.get("python_status")
        return blocker
    if comparison in {"missing_comparison", "missing_rust_result"}:
        return {
            "kind": comparison,
            "line": line_number,
            "route": route,
        }
    return None


def _rust_result_value(record: dict[str, Any], key: str) -> Any:
    rust_result = record.get("rust_result")
    if isinstance(rust_result, dict):
        return rust_result.get(key)
    return None


def _optional_int(value: Any) -> int | str | None:
    if value is None:
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return "invalid"


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Summarize a Rust shadow JSONL ledger."
    )
    parser.add_argument(
        "--ledger",
        type=Path,
        default=DEFAULT_LEDGER_PATH,
        help=f"Shadow ledger path (default: {DEFAULT_LEDGER_PATH})",
    )
    parser.add_argument("--json", action="store_true", help="Emit JSON report")
    parser.add_argument(
        "--fail-on-blockers",
        action="store_true",
        help="Exit non-zero when mismatches, shadow errors, or invalid rows exist",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    report = summarize_ledger(args.ledger)
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render_text_report(report))
    if args.fail_on_blockers and (report["blockers"] or report["status"] == "no_data"):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
