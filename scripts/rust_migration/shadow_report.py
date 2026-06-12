"""Summarize Rust shadow comparison ledgers for cutover observation."""

from __future__ import annotations

import argparse
import fnmatch
import json
from collections import Counter, defaultdict
from collections.abc import Mapping, Sequence
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any


DEFAULT_LEDGER_PATH = Path("~/.local/share/claude-sessions/rust_shadow.jsonl")
BLOCKING_COMPARISONS = {
    "body_mismatch",
    "shadow_endpoint_non_json",
    "status_mismatch",
}


def summarize_ledger(
    ledger_path: Path,
    *,
    since: datetime | None = None,
    last_minutes: float | None = None,
    now: datetime | None = None,
    min_rows: int | None = None,
    required_routes: Sequence[str] | None = None,
    min_route_rows: Mapping[str, int] | None = None,
    required_route_patterns: Sequence[str] | None = None,
    min_route_pattern_rows: Mapping[str, int] | None = None,
) -> dict[str, Any]:
    ledger_path = ledger_path.expanduser()
    since_filter = _resolve_since_filter(
        since=since,
        last_minutes=last_minutes,
        now=now,
    )
    if min_rows is not None and min_rows < 1:
        raise ValueError("min_rows must be greater than zero")
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
                if since_filter and not _record_is_inside_window(
                    record=record,
                    since=since_filter,
                    line_number=line_number,
                    blockers=blockers,
                ):
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
    route_row_counts: dict[str, int] = {}
    for route, summary in sorted(route_stats.items()):
        route_row_counts[route] = summary["rows"]
        route_summaries.append(
            {
                "route": route,
                "rows": summary["rows"],
                "comparisons": dict(sorted(summary["comparisons"].items())),
                "support_statuses": dict(sorted(summary["support_statuses"].items())),
                "blockers": summary["blockers"],
            }
        )

    blockers.extend(
        _coverage_gate_blockers(
            row_count=row_count,
            route_row_counts=route_row_counts,
            min_rows=min_rows,
            required_routes=required_routes or (),
            min_route_rows=min_route_rows or {},
            required_route_patterns=required_route_patterns or (),
            min_route_pattern_rows=min_route_pattern_rows or {},
        )
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
        "filter": {
            "since": since_filter.isoformat() if since_filter else None,
            "last_minutes": last_minutes,
        },
        "gates": {
            "min_rows": min_rows,
            "required_routes": list(required_routes or ()),
            "min_route_rows": dict(sorted((min_route_rows or {}).items())),
            "required_route_patterns": list(required_route_patterns or ()),
            "min_route_pattern_rows": dict(sorted((min_route_pattern_rows or {}).items())),
        },
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
        f"window: {_window_label(report.get('filter'))}",
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

    gates = report.get("gates")
    if isinstance(gates, dict) and any(gates.values()):
        lines.extend(["", "Coverage Gates:"])
        if gates.get("min_rows") is not None:
            lines.append(f"  min rows: {gates['min_rows']}")
        for route in gates.get("required_routes") or ():
            lines.append(f"  required route: {route}")
        for route, count in (gates.get("min_route_rows") or {}).items():
            lines.append(f"  min route rows: {route} >= {count}")
        for pattern in gates.get("required_route_patterns") or ():
            lines.append(f"  required route pattern: {pattern}")
        for pattern, count in (gates.get("min_route_pattern_rows") or {}).items():
            lines.append(f"  min route pattern rows: {pattern} >= {count}")

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


def _resolve_since_filter(
    *,
    since: datetime | None,
    last_minutes: float | None,
    now: datetime | None,
) -> datetime | None:
    if since is not None and last_minutes is not None:
        raise ValueError("since and last_minutes are mutually exclusive")
    if last_minutes is not None:
        if last_minutes <= 0:
            raise ValueError("last_minutes must be greater than zero")
        resolved_now = _ensure_aware_utc(now or datetime.now(timezone.utc))
        return resolved_now - timedelta(minutes=last_minutes)
    if since is not None:
        return _ensure_aware_utc(since)
    return None


def _record_is_inside_window(
    *,
    record: dict[str, Any],
    since: datetime,
    line_number: int,
    blockers: list[dict[str, Any]],
) -> bool:
    observed_at = record.get("observed_at")
    try:
        observed_at_dt = _parse_timestamp(str(observed_at))
    except ValueError as exc:
        blockers.append(
            {
                "kind": "invalid_observed_at",
                "line": line_number,
                "route": _route_key(record),
                "detail": str(exc),
            }
        )
        return False
    return observed_at_dt >= since


def _parse_timestamp(value: str) -> datetime:
    if not value or value == "None":
        raise ValueError("missing observed_at timestamp")
    normalized = value
    if normalized.endswith("Z"):
        normalized = normalized[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError as exc:
        raise ValueError(f"invalid observed_at timestamp: {value}") from exc
    return _ensure_aware_utc(parsed)


def _ensure_aware_utc(value: datetime) -> datetime:
    if value.tzinfo is None:
        return value.replace(tzinfo=timezone.utc)
    return value.astimezone(timezone.utc)


def _coverage_gate_blockers(
    *,
    row_count: int,
    route_row_counts: Mapping[str, int],
    min_rows: int | None,
    required_routes: Sequence[str],
    min_route_rows: Mapping[str, int],
    required_route_patterns: Sequence[str],
    min_route_pattern_rows: Mapping[str, int],
) -> list[dict[str, Any]]:
    blockers: list[dict[str, Any]] = []
    if min_rows is not None and row_count < min_rows:
        blockers.append(
            {
                "kind": "insufficient_rows",
                "detail": f"observed {row_count}, required {min_rows}",
            }
        )
    for route in required_routes:
        if route_row_counts.get(route, 0) == 0:
            blockers.append(
                {
                    "kind": "missing_required_route",
                    "route": route,
                    "detail": "required route not observed",
                }
            )
    for route, minimum in min_route_rows.items():
        observed = route_row_counts.get(route, 0)
        if observed < minimum:
            blockers.append(
                {
                    "kind": "insufficient_route_rows",
                    "route": route,
                    "detail": f"observed {observed}, required {minimum}",
                }
            )
    for pattern in required_route_patterns:
        if _route_pattern_count(route_row_counts, pattern) == 0:
            blockers.append(
                {
                    "kind": "missing_required_route_pattern",
                    "route": pattern,
                    "detail": "required route pattern not observed",
                }
            )
    for pattern, minimum in min_route_pattern_rows.items():
        observed = _route_pattern_count(route_row_counts, pattern)
        if observed < minimum:
            blockers.append(
                {
                    "kind": "insufficient_route_pattern_rows",
                    "route": pattern,
                    "detail": f"observed {observed}, required {minimum}",
                }
            )
    return blockers


def _route_pattern_count(route_row_counts: Mapping[str, int], pattern: str) -> int:
    return sum(
        count
        for route, count in route_row_counts.items()
        if fnmatch.fnmatchcase(route, pattern)
    )


def _normalize_route_requirement(value: str) -> str:
    parts = value.strip().split(maxsplit=1)
    if len(parts) != 2 or not parts[0] or not parts[1]:
        raise ValueError("route requirements must use 'METHOD /path' format")
    return f"{parts[0].upper()} {parts[1]}"


def _parse_route_minimum(value: str) -> tuple[str, int]:
    if "=" not in value:
        raise ValueError("route row minimums must use 'METHOD /path=N' format")
    route, raw_count = value.rsplit("=", 1)
    try:
        minimum = int(raw_count)
    except ValueError as exc:
        raise ValueError(
            "route row minimum count must be an integer greater than zero"
        ) from exc
    if minimum < 1:
        raise ValueError("route row minimum count must be greater than zero")
    return _normalize_route_requirement(route), minimum


def _window_label(filter_info: Any) -> str:
    if not isinstance(filter_info, dict) or not filter_info.get("since"):
        return "all"
    if filter_info.get("last_minutes") is not None:
        return f"last {filter_info['last_minutes']} minutes since {filter_info['since']}"
    return f"since {filter_info['since']}"


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
    window_group = parser.add_mutually_exclusive_group()
    window_group.add_argument(
        "--since",
        help=(
            "Only summarize valid rows observed at or after this ISO-8601 "
            "timestamp. Invalid rows remain blockers because they cannot be "
            "timestamp-filtered safely."
        ),
    )
    window_group.add_argument(
        "--last-minutes",
        type=float,
        help="Only summarize valid rows from the last N minutes.",
    )
    parser.add_argument(
        "--fail-on-blockers",
        action="store_true",
        help="Exit non-zero when mismatches, shadow errors, or invalid rows exist",
    )
    parser.add_argument(
        "--min-rows",
        type=int,
        help="Require at least this many valid rows in the selected window.",
    )
    parser.add_argument(
        "--require-route",
        action="append",
        default=[],
        help=(
            "Require at least one observation for a route, formatted as "
            "'METHOD /path'. Repeatable."
        ),
    )
    parser.add_argument(
        "--min-route-rows",
        action="append",
        default=[],
        help=(
            "Require at least N observations for a route, formatted as "
            "'METHOD /path=N'. Repeatable."
        ),
    )
    parser.add_argument(
        "--require-route-pattern",
        action="append",
        default=[],
        help=(
            "Require at least one observation matching a shell-style route "
            "pattern, formatted as 'METHOD /path/*'. Repeatable."
        ),
    )
    parser.add_argument(
        "--min-route-pattern-rows",
        action="append",
        default=[],
        help=(
            "Require at least N observations matching a shell-style route "
            "pattern, formatted as 'METHOD /path/*=N'. Repeatable."
        ),
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    try:
        since = _parse_timestamp(args.since) if args.since else None
        required_routes = [
            _normalize_route_requirement(route) for route in args.require_route
        ]
        min_route_rows = dict(
            _parse_route_minimum(requirement)
            for requirement in args.min_route_rows
        )
        required_route_patterns = [
            _normalize_route_requirement(pattern)
            for pattern in args.require_route_pattern
        ]
        min_route_pattern_rows = dict(
            _parse_route_minimum(requirement)
            for requirement in args.min_route_pattern_rows
        )
        report = summarize_ledger(
            args.ledger,
            since=since,
            last_minutes=args.last_minutes,
            min_rows=args.min_rows,
            required_routes=required_routes,
            min_route_rows=min_route_rows,
            required_route_patterns=required_route_patterns,
            min_route_pattern_rows=min_route_pattern_rows,
        )
    except ValueError as exc:
        parser.error(str(exc))
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render_text_report(report))
    if args.fail_on_blockers and (report["blockers"] or report["status"] == "no_data"):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
