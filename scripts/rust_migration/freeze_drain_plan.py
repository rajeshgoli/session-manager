"""Plan Rust migration write-freeze and drain coverage.

This tool is non-mutating by default. It turns the Stage 5 state-ownership
families into an operator-readable freeze/drain plan and can optionally append a
JSONL ledger entry that records the plan only. It does not enable a live freeze,
claim Rust ownership, or block Session Manager writes.
"""

from __future__ import annotations

import argparse
import json
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from scripts.rust_migration.state_preflight import (
    DEFAULT_CONFIG,
    build_state_preflight_report,
)


@dataclass(frozen=True)
class WriterFamily:
    id: str
    label: str
    store_ids: tuple[str, ...]
    evidence: str
    freeze_action: str
    drain_action: str
    gate: str


WRITER_FAMILIES: tuple[WriterFamily, ...] = (
    WriterFamily(
        "session_runtime",
        "Session state and tmux/runtime state",
        ("sessions_state", "log_dir"),
        "sessions.json, tmux/log paths, retained session runtime fields",
        "block new retained session lifecycle mutations or journal accepted requests",
        "drain active tmux/session mutations and verify attachable rollback state",
        "future_runtime_gated",
    ),
    WriterFamily(
        "message_queue",
        "Message queue, parent wakes, notify-on-stop, Codex review registrations",
        ("message_queue_db",),
        "message_queue.db retained delivery and review registration tables",
        "block or journal enqueue/claim/update paths for retained queue families",
        "drain queued delivery, parent wakes, notify-on-stop, and review notifications",
        "future_runtime_gated",
    ),
    WriterFamily(
        "tool_audit_and_telemetry",
        "Tool audit rows and Telegram telemetry/archive rows",
        ("tool_usage_db", "telegram_topics_json"),
        "tool_usage.db audit rows and Telegram archive/telemetry files",
        "freeze or journal tool audit inserts and Telegram archive writes",
        "drain asynchronous audit loggers and telemetry flushes",
        "future_runtime_gated",
    ),
    WriterFamily(
        "response_relay",
        "Response relay claims and delivery state",
        ("response_relay_db",),
        "response_relay.db inbound turns, assistant outputs, claims, and relayed markers",
        "block or journal relay claim/release/mark-relayed writes",
        "drain accepted notifier sends and release failed claims before final backup",
        "future_runtime_gated",
    ),
    WriterFamily(
        "email_human_delivery",
        "Inbound email admission and outbound email/human delivery attempts",
        ("email_bridge_config", "message_queue_db", "response_relay_db"),
        "email bridge config, human recipient config, queue rows, and relay source rows",
        "block or journal inbound email admission and outbound email/human attempts",
        "drain accepted inbound messages and notifier delivery-result updates",
        "future_runtime_gated",
    ),
    WriterFamily(
        "codex_state",
        "Codex events, cursors, requests, and observability rows",
        ("codex_events_db", "codex_requests_db", "codex_observability_db"),
        "Codex event store, request ledger, observability rows, and provider cursors",
        "freeze or journal provider event ingestion, cursor advancement, and request resolution",
        "drain pending Codex requests, event reducers, and observability/prune tasks",
        "future_runtime_gated",
    ),
    WriterFamily(
        "queue_runner",
        "Queue runner jobs, state, logs, and scripts",
        ("queue_runner_state_dir", "message_queue_db"),
        "queue_runner state dir, queue jobs DBs, per-job logs/scripts, and notifications",
        "block or journal queue job admission, cancellation, state updates, and notifications",
        "drain active jobs or record explicit operator risk acceptance",
        "future_runtime_gated",
    ),
    WriterFamily(
        "native_bug_reports",
        "Native bug-report create/update/prune and delivery-result writes",
        ("bug_reports_db",),
        "bug report DB, attachment metadata, prune state, and maintainer notification updates",
        "block or journal bug-report admission, attachment writes, prune, and delivery-result updates",
        "drain maintainer notification/update paths and record replay/discard requirements",
        "future_runtime_gated",
    ),
    WriterFamily(
        "app_artifacts",
        "App artifact uploads and metadata",
        ("app_artifacts_dir",),
        "app artifact root, latest APK, immutable hash APKs, and meta.json",
        "block or journal app upload and metadata replacement paths",
        "drain in-flight uploads before the final backup restore point",
        "future_runtime_gated",
    ),
    WriterFamily(
        "nodes",
        "Node-agent/control state and restore inventory caches",
        ("config_yaml", "sessions_state", "log_dir"),
        "node config, session node mapping, control streams, and restore inventory cache behavior",
        "block or journal node placement/control mutations and cache writes",
        "drain active node-agent control streams and verify reconnect/rollback behavior",
        "manual",
    ),
    WriterFamily(
        "locks_worktrees",
        "Locks, worktree mutations, and direct CLI writer paths",
        ("sessions_state", "tool_usage_db"),
        "lock/worktree state, Stop-hook cleanup state, and direct CLI writer commands",
        "block direct CLI/local writer paths or route them through the active owner",
        "drain active lock/worktree mutations and verify conflict text/state",
        "manual",
    ),
    WriterFamily(
        "service_diagnostics",
        "Service logs and runtime diagnostics relevant to rollback",
        ("config_yaml", "client_yaml", "local_env_overlay", "server_log_file", "log_dir"),
        "config, client config, local env overlay, service logs, and runtime diagnostics",
        "freeze config rewrites and journal operator-approved service changes",
        "drain service/package updates and record rollback entrypoint/log locations",
        "manual",
    ),
)


def build_freeze_drain_plan(
    *,
    config_path: Path = DEFAULT_CONFIG,
    local_env_path: Path | None = None,
    ledger_path: Path | None = None,
    record_plan: bool = False,
) -> dict[str, Any]:
    generated_at = datetime.now(timezone.utc).isoformat()
    preflight = build_state_preflight_report(
        config_path=config_path,
        local_env_path=local_env_path,
    )
    rows = [_family_row(family, preflight) for family in WRITER_FAMILIES]
    blockers = list(preflight["blockers"])
    warnings = list(preflight["warnings"])
    ledger = _ledger_info(ledger_path, record_plan=record_plan)
    blockers.extend(ledger["blockers"])

    report: dict[str, Any] = {
        "schema_version": 1,
        "generated_at": generated_at,
        "status": "blocked" if blockers else "planned",
        "mode": "record_plan" if record_plan else "dry_run",
        "freeze_active": False,
        "rust_ownership_active": False,
        "preflight_status": preflight["status"],
        "preflight_summary": preflight["summary"],
        "ledger": {
            "path": str(ledger_path.expanduser()) if ledger_path else None,
            "record_plan": record_plan,
            "written": False,
            "entry_kind": "freeze_drain_plan",
        },
        "summary": {
            "writer_families": len(rows),
            "automatic": sum(1 for row in rows if row["gate"] == "automatic"),
            "manual": sum(1 for row in rows if row["gate"] == "manual"),
            "future_runtime_gated": sum(
                1 for row in rows if row["gate"] == "future_runtime_gated"
            ),
            "blockers": len(blockers),
            "warnings": len(warnings),
        },
        "writer_families": rows,
        "blockers": blockers,
        "warnings": warnings,
    }
    if record_plan and not blockers:
        _append_ledger_entry(report, Path(ledger_path).expanduser())
        report["ledger"]["written"] = True
    return report


def render_text_report(report: dict[str, Any]) -> str:
    summary = report["summary"]
    lines = [
        "Rust freeze/drain plan",
        f"status: {report['status']}",
        f"mode: {report['mode']}",
        f"freeze_active: {str(report['freeze_active']).lower()}",
        f"rust_ownership_active: {str(report['rust_ownership_active']).lower()}",
        (
            "writer_families: "
            f"{summary['writer_families']} total, {summary['manual']} manual, "
            f"{summary['future_runtime_gated']} future_runtime_gated"
        ),
        f"blockers: {summary['blockers']}",
        f"warnings: {summary['warnings']}",
        "",
        "Writer families:",
    ]
    for row in report["writer_families"]:
        lines.extend(
            [
                f"  {row['id']} ({row['gate']}): {row['label']}",
                f"    stores: {', '.join(row['store_ids'])}",
                f"    evidence: {row['evidence']}",
                f"    freeze: {row['freeze_action']}",
                f"    drain: {row['drain_action']}",
            ]
        )
    if report["blockers"]:
        lines.extend(["", "Blockers:"])
        for blocker in report["blockers"]:
            lines.append(f"  {blocker['store_id']}: {blocker['kind']} - {blocker['detail']}")
    if report["warnings"]:
        lines.extend(["", "Warnings:"])
        for warning in report["warnings"]:
            lines.append(f"  {warning['store_id']}: {warning['kind']} - {warning['detail']}")
    return "\n".join(lines)


def _family_row(family: WriterFamily, preflight: dict[str, Any]) -> dict[str, Any]:
    stores = {row["id"]: row for row in preflight["stores"]}
    store_rows = [stores[store_id] for store_id in family.store_ids if store_id in stores]
    missing_store_ids = [
        store_id for store_id in family.store_ids if store_id not in stores
    ]
    return {
        "id": family.id,
        "label": family.label,
        "store_ids": list(family.store_ids),
        "store_paths": {
            row["id"]: row["path"]
            for row in store_rows
        },
        "missing_store_ids": missing_store_ids,
        "evidence": family.evidence,
        "freeze_action": family.freeze_action,
        "drain_action": family.drain_action,
        "gate": family.gate,
    }


def _ledger_info(
    ledger_path: Path | None,
    *,
    record_plan: bool,
) -> dict[str, Any]:
    blockers: list[dict[str, str]] = []
    if not record_plan:
        return {"blockers": blockers}
    if ledger_path is None:
        blockers.append(_issue("ledger", "missing_ledger_path", "--record-plan requires --ledger"))
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
    entry = {
        "schema_version": 1,
        "kind": "freeze_drain_plan",
        "generated_at": report["generated_at"],
        "freeze_active": False,
        "rust_ownership_active": False,
        "status": report["status"],
        "writer_families": [
            {
                "id": row["id"],
                "store_ids": row["store_ids"],
                "gate": row["gate"],
                "freeze_action": row["freeze_action"],
                "drain_action": row["drain_action"],
            }
            for row in report["writer_families"]
        ],
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
    parser.add_argument("--ledger", type=Path, default=None)
    parser.add_argument(
        "--record-plan",
        action="store_true",
        help="Append a plan-only JSONL entry to --ledger",
    )
    parser.add_argument("--json", action="store_true", help="Emit JSON report")
    parser.add_argument(
        "--fail-on-blockers",
        action="store_true",
        help="Exit nonzero when the plan/report has blockers",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    report = build_freeze_drain_plan(
        config_path=args.config,
        local_env_path=args.local_env,
        ledger_path=args.ledger,
        record_plan=args.record_plan,
    )
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render_text_report(report))
    if args.fail_on_blockers and report["blockers"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
