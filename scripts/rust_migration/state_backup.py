"""Plan and optionally copy Rust migration state backups.

By default this tool is a dry run. It consumes the non-mutating state preflight
report and produces a deterministic backup manifest for existing copyable
stores. Use ``--execute --output-dir <dir>`` to copy files/directories and write
the manifest into that directory.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from scripts.rust_migration.state_preflight import (
    DEFAULT_CONFIG,
    build_state_preflight_report,
    is_unsafe_directory_root,
)


DEFAULT_BACKUP_PARENT = Path(".local/rust-state-backups")


def build_backup_plan(
    *,
    config_path: Path = DEFAULT_CONFIG,
    local_env_path: Path | None = None,
    output_dir: Path | None = None,
    execute: bool = False,
) -> dict[str, Any]:
    if execute and output_dir is None:
        raise ValueError("--execute requires --output-dir")
    generated_at = datetime.now(timezone.utc).isoformat()
    backup_root = _backup_root(output_dir, generated_at=generated_at)
    preflight = build_state_preflight_report(
        config_path=config_path,
        local_env_path=local_env_path,
    )
    entries: list[dict[str, Any]] = []
    blockers = list(preflight["blockers"])
    warnings = list(preflight["warnings"])
    if execute and backup_root.exists():
        blockers.append(
            {
                "store_id": "backup_root",
                "kind": "backup_root_exists",
                "severity": "blocker",
                "detail": f"backup root already exists: {backup_root}",
            }
        )

    for row in preflight["stores"]:
        entry = _plan_entry(row, backup_root)
        entries.append(entry)
        blockers.extend(entry["blockers"])
        warnings.extend(entry["warnings"])
    blockers.extend(_backup_root_inside_source_blockers(entries, backup_root))

    report: dict[str, Any] = {
        "schema_version": 1,
        "generated_at": generated_at,
        "status": "blocked" if blockers else "copied" if execute else "planned",
        "mode": "execute" if execute else "dry_run",
        "backup_root": str(backup_root),
        "manifest_path": str(backup_root / "state-backup-manifest.json"),
        "preflight_status": preflight["status"],
        "preflight_summary": preflight["summary"],
        "summary": {
            "entries": len(entries),
            "planned": sum(1 for entry in entries if entry["action"] == "copy"),
            "skipped": sum(1 for entry in entries if entry["action"] == "skip"),
            "copied": 0,
            "blockers": len(blockers),
            "warnings": len(warnings),
            "planned_bytes": sum(
                int(entry.get("size_bytes") or 0)
                for entry in entries
                if entry["action"] == "copy"
            ),
        },
        "entries": entries,
        "blockers": blockers,
        "warnings": warnings,
    }
    if execute and not blockers:
        copied = _execute_plan(report)
        report["summary"]["copied"] = copied
        report["summary"]["blockers"] = len(report["blockers"])
        report["summary"]["warnings"] = len(report["warnings"])
        _write_manifest(report)
    return report


def render_text_report(report: dict[str, Any]) -> str:
    summary = report["summary"]
    lines = [
        "Rust state backup plan",
        f"status: {report['status']}",
        f"mode: {report['mode']}",
        f"backup_root: {report['backup_root']}",
        (
            "entries: "
            f"{summary['entries']} total, {summary['planned']} planned, "
            f"{summary['skipped']} skipped, {summary['copied']} copied"
        ),
        f"planned_bytes: {summary['planned_bytes']}",
        f"blockers: {summary['blockers']}",
        f"warnings: {summary['warnings']}",
        "",
        "Stores:",
    ]
    for entry in report["entries"]:
        evidence = _entry_evidence(entry)
        if entry["action"] == "copy":
            lines.append(
                f"  copy  {entry['store_id']}: {entry['source']} -> {entry['destination']} ({evidence})"
            )
        else:
            lines.append(
                f"  skip  {entry['store_id']}: {entry['skip_reason']} ({evidence})"
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


def _backup_root(output_dir: Path | None, *, generated_at: str) -> Path:
    if output_dir is not None:
        return output_dir.expanduser().resolve()
    stamp = (
        generated_at.replace("-", "")
        .replace(":", "")
        .replace("+00:00", "Z")
        .replace(".", "-")
    )
    return (DEFAULT_BACKUP_PARENT / stamp).resolve()


def _plan_entry(row: dict[str, Any], backup_root: Path) -> dict[str, Any]:
    source = Path(row["path"])
    entry: dict[str, Any] = {
        "store_id": row["id"],
        "label": row["label"],
        "category": row["category"],
        "kind": row["kind"],
        "source": str(source),
        "destination": str(backup_root / "stores" / row["id"]),
        "required": row["required"],
        "exists": row["exists"],
        "size_bytes": row["size_bytes"],
        "sha256": row["sha256"],
        "file_count": row["file_count"],
        "action": "skip",
        "skip_reason": None,
        "blockers": [],
        "warnings": [],
    }
    if not row["exists"]:
        entry["skip_reason"] = "missing"
        return entry
    if row["issues"]:
        entry["skip_reason"] = "preflight_issue"
        return entry
    if not row["copyable"]:
        entry["skip_reason"] = "not_copyable"
        entry["blockers"].append(
            _issue(row["id"], "not_copyable", "path exists but is not copyable")
        )
        return entry
    if source.is_symlink():
        entry["skip_reason"] = "symlink_source"
        entry["blockers"].append(
            _issue(row["id"], "symlink_source", "top-level store path is a symlink")
        )
        return entry
    if row["kind"] == "dir" and is_unsafe_directory_root(source):
        entry["skip_reason"] = "unsafe_source_root"
        entry["blockers"].append(
            _issue(
                row["id"],
                "unsafe_source_root",
                f"directory source is too broad to back up safely: {source}",
            )
        )
        return entry
    entry["action"] = "copy"
    entry["skip_reason"] = None
    return entry


def _entry_evidence(entry: dict[str, Any]) -> str:
    fields = [
        f"kind={entry['kind']}",
        f"size_bytes={entry['size_bytes']}",
        f"sha256={entry['sha256']}",
        f"file_count={entry['file_count']}",
    ]
    return ", ".join(fields)


def _backup_root_inside_source_blockers(
    entries: list[dict[str, Any]],
    backup_root: Path,
) -> list[dict[str, str]]:
    blockers: list[dict[str, str]] = []
    backup_root_resolved = backup_root.resolve()
    for entry in entries:
        if entry["action"] != "copy" or entry["kind"] != "dir":
            continue
        source = Path(entry["source"]).resolve()
        if _is_same_or_descendant(backup_root_resolved, source):
            blockers.append(
                _issue(
                    entry["store_id"],
                    "backup_root_inside_source",
                    f"backup root {backup_root_resolved} is inside copied directory source {source}",
                )
            )
    return blockers


def _is_same_or_descendant(path: Path, parent: Path) -> bool:
    return path == parent or parent in path.parents


def _execute_plan(report: dict[str, Any]) -> int:
    backup_root = Path(report["backup_root"])
    backup_root.mkdir(parents=True, exist_ok=False)
    copied = 0
    for entry in report["entries"]:
        if entry["action"] != "copy":
            continue
        source = Path(entry["source"])
        destination = Path(entry["destination"])
        destination.parent.mkdir(parents=True, exist_ok=True)
        if entry["kind"] == "file":
            shutil.copy2(source, destination)
            entry["backup_size_bytes"] = destination.stat().st_size
            entry["backup_sha256"] = _file_hash(destination)
        elif entry["kind"] == "dir":
            _copy_directory(source, destination, entry)
            backup_size, backup_file_count = _dir_stats(destination)
            entry["backup_size_bytes"] = backup_size
            entry["backup_file_count"] = backup_file_count
        else:
            entry["blockers"].append(
                _issue(entry["store_id"], "unsupported_kind", f"cannot copy kind {entry['kind']}")
            )
            continue
        entry["copied"] = True
        copied += 1
    report["blockers"] = [
        blocker
        for entry in report["entries"]
        for blocker in entry.get("blockers", [])
    ]
    report["warnings"].extend(
        warning
        for entry in report["entries"]
        for warning in entry.get("warnings", [])
    )
    if report["blockers"]:
        report["status"] = "blocked"
    return copied


def _copy_directory(source: Path, destination: Path, entry: dict[str, Any]) -> None:
    destination.mkdir(parents=True, exist_ok=False)
    for child in source.rglob("*"):
        relative = child.relative_to(source)
        target = destination / relative
        if child.is_symlink():
            entry["warnings"].append(
                _issue(
                    entry["store_id"],
                    "skipped_symlink",
                    f"skipped symlink inside directory: {child}",
                )
            )
            continue
        if child.is_dir():
            target.mkdir(parents=True, exist_ok=True)
            continue
        if child.is_file():
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(child, target)


def _file_hash(path: Path) -> str | None:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError:
        return None
    return digest.hexdigest()


def _dir_stats(path: Path) -> tuple[int, int]:
    total_size = 0
    file_count = 0
    for child in path.rglob("*"):
        if child.is_symlink():
            continue
        if child.is_file():
            file_count += 1
            total_size += child.stat().st_size
    return total_size, file_count


def _write_manifest(report: dict[str, Any]) -> None:
    manifest_path = Path(report["manifest_path"])
    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    manifest_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")


def _issue(store_id: str, kind: str, detail: str) -> dict[str, str]:
    return {
        "store_id": store_id,
        "kind": kind,
        "severity": "blocker" if kind not in {"skipped_symlink"} else "warning",
        "detail": detail,
    }


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--local-env", type=Path, default=None)
    parser.add_argument("--output-dir", type=Path, default=None)
    parser.add_argument("--execute", action="store_true", help="Copy planned stores")
    parser.add_argument("--json", action="store_true", help="Emit JSON report")
    parser.add_argument(
        "--fail-on-blockers",
        action="store_true",
        help="Exit nonzero when the plan/report has blockers",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    try:
        report = build_backup_plan(
            config_path=args.config,
            local_env_path=args.local_env,
            output_dir=args.output_dir,
            execute=args.execute,
        )
    except ValueError as exc:
        raise SystemExit(str(exc)) from exc
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render_text_report(report))
    if args.fail_on_blockers and report["blockers"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
