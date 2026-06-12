"""Verify and rehearse restoring a Rust migration state backup.

Default mode is read-only verification of ``state-backup-manifest.json``. Use
``--execute-restore --restore-dir <dir>`` to copy backup contents into a fresh
rehearsal directory. This tool never restores into live Session Manager paths.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from scripts.rust_migration.state_preflight import is_unsafe_directory_root


MANIFEST_NAME = "state-backup-manifest.json"


def build_restore_report(
    *,
    manifest_path: Path,
    restore_dir: Path | None = None,
    execute_restore: bool = False,
) -> dict[str, Any]:
    if execute_restore and restore_dir is None:
        raise ValueError("--execute-restore requires --restore-dir")
    generated_at = datetime.now(timezone.utc).isoformat()
    manifest_path = manifest_path.expanduser().resolve()
    manifest, load_blockers = _load_manifest(manifest_path)
    entries: list[dict[str, Any]] = []
    blockers = list(load_blockers)
    warnings: list[dict[str, str]] = []
    backup_root = _manifest_backup_root(manifest, manifest_path)
    restore_root_input = restore_dir.expanduser() if restore_dir else None
    restore_root = restore_root_input.resolve() if restore_root_input else None

    if manifest:
        blockers.extend(_manifest_shape_blockers(manifest, manifest_path))
        warnings.extend(
            warning
            for warning in manifest.get("warnings", [])
            if isinstance(warning, dict)
        )
        for manifest_entry in manifest.get("entries", []):
            entry = _verify_entry(manifest_entry)
            entries.append(entry)
            blockers.extend(entry["blockers"])
            warnings.extend(entry["warnings"])
        if execute_restore:
            blockers.extend(
                _restore_root_blockers(
                    restore_root_input=restore_root_input,
                    restore_root=restore_root,
                    backup_root=backup_root,
                    entries=entries,
                )
            )

    report: dict[str, Any] = {
        "schema_version": 1,
        "generated_at": generated_at,
        "status": "blocked" if blockers else "restored" if execute_restore else "verified",
        "mode": "execute_restore" if execute_restore else "verify",
        "manifest_path": str(manifest_path),
        "backup_root": str(backup_root) if backup_root else None,
        "restore_root": str(restore_root) if restore_root else None,
        "backup_status": manifest.get("status") if manifest else None,
        "backup_generated_at": manifest.get("generated_at") if manifest else None,
        "summary": {
            "entries": len(entries),
            "copied_entries": sum(1 for entry in entries if entry["action"] == "copy"),
            "skipped_entries": sum(1 for entry in entries if entry["action"] == "skip"),
            "verified": sum(1 for entry in entries if entry["verification_status"] == "verified"),
            "restored": 0,
            "blockers": len(blockers),
            "warnings": len(warnings),
        },
        "entries": entries,
        "blockers": blockers,
        "warnings": warnings,
    }
    if execute_restore and not blockers:
        restored = _execute_restore(report)
        report["summary"]["restored"] = restored
        report["summary"]["blockers"] = len(report["blockers"])
        report["summary"]["warnings"] = len(report["warnings"])
        if report["blockers"]:
            report["status"] = "blocked"
        else:
            _write_restore_report(report)
    return report


def render_text_report(report: dict[str, Any]) -> str:
    summary = report["summary"]
    lines = [
        "Rust state backup restore report",
        f"status: {report['status']}",
        f"mode: {report['mode']}",
        f"manifest_path: {report['manifest_path']}",
        f"backup_root: {report['backup_root']}",
        f"restore_root: {report['restore_root']}",
        (
            "entries: "
            f"{summary['entries']} total, {summary['verified']} verified, "
            f"{summary['skipped_entries']} skipped, {summary['restored']} restored"
        ),
        f"blockers: {summary['blockers']}",
        f"warnings: {summary['warnings']}",
        "",
        "Stores:",
    ]
    for entry in report["entries"]:
        if entry["action"] == "copy":
            lines.append(
                f"  {entry['verification_status']:8} {entry['store_id']}: {entry['backup_path']} -> {entry['restore_path']}"
            )
        else:
            lines.append(
                f"  skip     {entry['store_id']}: {entry['skip_reason']}"
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


def _load_manifest(manifest_path: Path) -> tuple[dict[str, Any], list[dict[str, str]]]:
    if not manifest_path.exists():
        return {}, [_issue("manifest", "manifest_missing", f"manifest does not exist: {manifest_path}")]
    if not manifest_path.is_file():
        return {}, [_issue("manifest", "manifest_not_file", f"manifest is not a file: {manifest_path}")]
    try:
        loaded = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        return {}, [_issue("manifest", "manifest_invalid_json", str(exc))]
    if not isinstance(loaded, dict):
        return {}, [_issue("manifest", "manifest_not_object", "manifest JSON must be an object")]
    return loaded, []


def _manifest_backup_root(manifest: dict[str, Any], manifest_path: Path) -> Path | None:
    raw = manifest.get("backup_root") if manifest else None
    if raw:
        return Path(str(raw)).expanduser().resolve()
    if manifest_path.name == MANIFEST_NAME:
        return manifest_path.parent
    return None


def _manifest_shape_blockers(
    manifest: dict[str, Any],
    manifest_path: Path,
) -> list[dict[str, str]]:
    blockers: list[dict[str, str]] = []
    if manifest.get("status") != "copied":
        blockers.append(
            _issue(
                "manifest",
                "backup_not_copied",
                f"manifest status must be copied, got {manifest.get('status')!r}",
            )
        )
    if not isinstance(manifest.get("entries"), list):
        blockers.append(_issue("manifest", "entries_not_list", "manifest entries must be a list"))
    manifest_path_value = manifest.get("manifest_path")
    if manifest_path_value and Path(str(manifest_path_value)).expanduser().resolve() != manifest_path:
        blockers.append(
            _issue(
                "manifest",
                "manifest_path_mismatch",
                f"manifest_path points to {manifest_path_value}, not {manifest_path}",
            )
        )
    return blockers


def _verify_entry(manifest_entry: Any) -> dict[str, Any]:
    if not isinstance(manifest_entry, dict):
        return _invalid_entry("unknown", "entry_not_object", "manifest entry must be an object")
    store_id = str(manifest_entry.get("store_id") or "unknown")
    action = str(manifest_entry.get("action") or "")
    kind = str(manifest_entry.get("kind") or "")
    destination = manifest_entry.get("destination")
    backup_path = Path(str(destination)) if destination else None
    entry: dict[str, Any] = {
        "store_id": store_id,
        "label": manifest_entry.get("label"),
        "action": action,
        "kind": kind,
        "skip_reason": manifest_entry.get("skip_reason"),
        "source": manifest_entry.get("source"),
        "backup_path": str(backup_path) if backup_path else None,
        "restore_path": None,
        "backup_exists": False,
        "size_bytes": None,
        "sha256": None,
        "file_count": None,
        "verification_status": "skipped" if action == "skip" else "blocked",
        "blockers": [],
        "warnings": [],
    }
    if action == "skip":
        if manifest_entry.get("required"):
            entry["verification_status"] = "blocked"
            entry["blockers"].append(
                _issue(store_id, "required_store_skipped", "required store was skipped in backup manifest")
            )
        elif not _is_safe_store_id(store_id):
            entry["verification_status"] = "blocked"
            entry["blockers"].append(
                _issue(store_id, "unsafe_store_id", f"store_id is not a safe path segment: {store_id}")
            )
        return entry
    if action != "copy":
        entry["blockers"].append(_issue(store_id, "unexpected_action", f"unexpected action: {action}"))
        return entry
    if not _is_safe_store_id(store_id):
        entry["blockers"].append(
            _issue(store_id, "unsafe_store_id", f"store_id is not a safe path segment: {store_id}")
        )
        return entry
    if backup_path is None:
        entry["blockers"].append(_issue(store_id, "missing_destination", "copy entry is missing destination"))
        return entry
    if not backup_path.exists():
        entry["blockers"].append(_issue(store_id, "backup_missing", f"backup destination missing: {backup_path}"))
        return entry
    entry["backup_exists"] = True
    if kind == "file":
        _verify_file_entry(entry, manifest_entry, backup_path)
    elif kind == "dir":
        _verify_dir_entry(entry, manifest_entry, backup_path)
    else:
        entry["blockers"].append(_issue(store_id, "unsupported_kind", f"unsupported kind: {kind}"))
    if not entry["blockers"]:
        entry["verification_status"] = "verified"
    return entry


def _invalid_entry(store_id: str, kind: str, detail: str) -> dict[str, Any]:
    return {
        "store_id": store_id,
        "action": None,
        "kind": None,
        "skip_reason": None,
        "source": None,
        "backup_path": None,
        "restore_path": None,
        "backup_exists": False,
        "size_bytes": None,
        "sha256": None,
        "file_count": None,
        "verification_status": "blocked",
        "blockers": [_issue(store_id, kind, detail)],
        "warnings": [],
    }


def _verify_file_entry(
    entry: dict[str, Any],
    manifest_entry: dict[str, Any],
    backup_path: Path,
) -> None:
    if not backup_path.is_file():
        entry["blockers"].append(
            _issue(entry["store_id"], "backup_wrong_kind", f"expected file: {backup_path}")
        )
        return
    readable, digest = _file_hash(backup_path)
    size = backup_path.stat().st_size
    entry["size_bytes"] = size
    entry["sha256"] = digest
    expected_size = manifest_entry.get("backup_size_bytes", manifest_entry.get("size_bytes"))
    expected_hash = manifest_entry.get("backup_sha256", manifest_entry.get("sha256"))
    if not readable:
        entry["blockers"].append(_issue(entry["store_id"], "backup_not_readable", f"cannot read: {backup_path}"))
    if expected_size is not None and size != expected_size:
        entry["blockers"].append(
            _issue(entry["store_id"], "size_mismatch", f"expected {expected_size}, got {size}")
        )
    if expected_hash and digest != expected_hash:
        entry["blockers"].append(
            _issue(entry["store_id"], "sha256_mismatch", f"expected {expected_hash}, got {digest}")
        )


def _verify_dir_entry(
    entry: dict[str, Any],
    manifest_entry: dict[str, Any],
    backup_path: Path,
) -> None:
    if not backup_path.is_dir():
        entry["blockers"].append(
            _issue(entry["store_id"], "backup_wrong_kind", f"expected directory: {backup_path}")
        )
        return
    readable, size, file_count = _dir_stats(backup_path)
    entry["size_bytes"] = size
    entry["file_count"] = file_count
    expected_size = manifest_entry.get("backup_size_bytes", manifest_entry.get("size_bytes"))
    expected_file_count = manifest_entry.get("backup_file_count", manifest_entry.get("file_count"))
    if not readable:
        entry["blockers"].append(_issue(entry["store_id"], "backup_not_readable", f"cannot read: {backup_path}"))
    if expected_size is not None and size != expected_size:
        entry["blockers"].append(
            _issue(entry["store_id"], "size_mismatch", f"expected {expected_size}, got {size}")
        )
    if expected_file_count is not None and file_count != expected_file_count:
        entry["blockers"].append(
            _issue(entry["store_id"], "file_count_mismatch", f"expected {expected_file_count}, got {file_count}")
        )


def _restore_root_blockers(
    *,
    restore_root_input: Path | None,
    restore_root: Path | None,
    backup_root: Path | None,
    entries: list[dict[str, Any]],
) -> list[dict[str, str]]:
    blockers: list[dict[str, str]] = []
    if restore_root_input is None or restore_root is None:
        return [_issue("restore_root", "missing_restore_dir", "--execute-restore requires --restore-dir")]
    if restore_root_input.exists():
        blockers.append(_issue("restore_root", "restore_root_exists", f"restore root already exists: {restore_root_input}"))
    if restore_root_input.is_symlink():
        blockers.append(_issue("restore_root", "restore_root_symlink", f"restore root is a symlink: {restore_root_input}"))
    if is_unsafe_directory_root(restore_root):
        blockers.append(_issue("restore_root", "unsafe_restore_root", f"restore root is too broad: {restore_root}"))
    parent = restore_root_input.parent
    if not parent.exists():
        blockers.append(_issue("restore_root", "restore_parent_missing", f"restore parent missing: {parent}"))
    elif not parent.is_dir():
        blockers.append(_issue("restore_root", "restore_parent_not_dir", f"restore parent is not a directory: {parent}"))
    if backup_root and _is_same_or_descendant(restore_root, backup_root):
        blockers.append(
            _issue(
                "restore_root",
                "restore_root_inside_backup",
                f"restore root {restore_root} is inside backup root {backup_root}",
            )
        )
    stores_root = restore_root / "stores"
    for entry in entries:
        if entry["action"] != "copy":
            continue
        destination_blocker = _restore_destination_blocker(
            entry=entry,
            stores_root=stores_root,
        )
        if destination_blocker:
            blockers.append(destination_blocker)
        for key, kind in (
            ("backup_path", "restore_root_inside_backup_entry"),
            ("source", "restore_root_inside_live_source"),
        ):
            raw = entry.get(key)
            if not raw:
                continue
            path = Path(str(raw)).expanduser().resolve()
            if _is_same_or_descendant(restore_root, path):
                blockers.append(
                    _issue(
                        entry["store_id"],
                        kind,
                        f"restore root {restore_root} is inside {key} {path}",
                    )
                )
    return blockers


def _restore_destination_blocker(
    *,
    entry: dict[str, Any],
    stores_root: Path,
) -> dict[str, str] | None:
    store_id = str(entry.get("store_id") or "")
    if not _is_safe_store_id(store_id):
        return _issue(store_id, "unsafe_store_id", f"store_id is not a safe path segment: {store_id}")
    destination = (stores_root / store_id).resolve()
    if not _is_same_or_descendant(destination, stores_root.resolve()):
        return _issue(
            store_id,
            "restore_destination_escape",
            f"restore destination {destination} escapes {stores_root}",
        )
    return None


def _execute_restore(report: dict[str, Any]) -> int:
    restore_root = Path(str(report["restore_root"]))
    restore_root.mkdir(parents=True, exist_ok=False)
    stores_root = restore_root / "stores"
    restored = 0
    for entry in report["entries"]:
        if entry["verification_status"] != "verified":
            continue
        source = Path(str(entry["backup_path"]))
        destination = stores_root / entry["store_id"]
        destination_blocker = _restore_destination_blocker(
            entry=entry,
            stores_root=stores_root,
        )
        if destination_blocker:
            entry["blockers"].append(destination_blocker)
            continue
        entry["restore_path"] = str(destination)
        destination.parent.mkdir(parents=True, exist_ok=True)
        if entry["kind"] == "file":
            shutil.copy2(source, destination)
        elif entry["kind"] == "dir":
            _restore_directory(source, destination, entry)
        else:
            entry["blockers"].append(_issue(entry["store_id"], "unsupported_kind", f"cannot restore kind {entry['kind']}"))
            continue
        restored += 1
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
    return restored


def _restore_directory(source: Path, destination: Path, entry: dict[str, Any]) -> None:
    destination.mkdir(parents=True, exist_ok=False)
    for child in source.rglob("*"):
        relative = child.relative_to(source)
        target = destination / relative
        if child.is_symlink():
            entry["warnings"].append(
                {
                    "store_id": entry["store_id"],
                    "kind": "skipped_backup_symlink",
                    "severity": "warning",
                    "detail": f"skipped symlink inside backup directory: {child}",
                }
            )
            continue
        if child.is_dir():
            target.mkdir(parents=True, exist_ok=True)
            continue
        if child.is_file():
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(child, target)


def _write_restore_report(report: dict[str, Any]) -> None:
    restore_root = Path(str(report["restore_root"]))
    report_path = restore_root / "state-restore-report.json"
    report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def _file_hash(path: Path) -> tuple[bool, str | None]:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError:
        return False, None
    return True, digest.hexdigest()


def _dir_stats(path: Path) -> tuple[bool, int, int]:
    total_size = 0
    file_count = 0
    try:
        for child in path.rglob("*"):
            if child.is_symlink():
                continue
            if child.is_file():
                file_count += 1
                total_size += child.stat().st_size
    except OSError:
        return False, total_size, file_count
    return True, total_size, file_count


def _is_same_or_descendant(path: Path, parent: Path) -> bool:
    return path == parent or parent in path.parents


def _is_safe_store_id(store_id: str) -> bool:
    if not store_id or store_id in {".", ".."}:
        return False
    path = Path(store_id)
    return (
        not path.is_absolute()
        and len(path.parts) == 1
        and "/" not in store_id
        and "\\" not in store_id
    )


def _issue(store_id: str, kind: str, detail: str) -> dict[str, str]:
    return {
        "store_id": store_id,
        "kind": kind,
        "severity": "blocker",
        "detail": detail,
    }


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--restore-dir", type=Path, default=None)
    parser.add_argument("--execute-restore", action="store_true")
    parser.add_argument("--json", action="store_true", help="Emit JSON report")
    parser.add_argument(
        "--fail-on-blockers",
        action="store_true",
        help="Exit nonzero when verification/restore has blockers",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    try:
        report = build_restore_report(
            manifest_path=args.manifest,
            restore_dir=args.restore_dir,
            execute_restore=args.execute_restore,
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
