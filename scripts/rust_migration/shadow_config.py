"""Prepare local Python config for Rust shadow observation."""

from __future__ import annotations

import argparse
import difflib
import json
import re
import shutil
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import yaml


DEFAULT_CONFIG = Path("config.yaml")
DEFAULT_RUST_ENDPOINT = "http://127.0.0.1:8421/__shadow/http"
DEFAULT_LEDGER_PATH = "~/.local/share/claude-sessions/rust_shadow.jsonl"
DEFAULT_TIMEOUT_SECONDS = 0.5
DEFAULT_MAX_BODY_BYTES = 65536


def prepare_shadow_config(
    *,
    config_path: Path = DEFAULT_CONFIG,
    endpoint: str = DEFAULT_RUST_ENDPOINT,
    ledger_path: str = DEFAULT_LEDGER_PATH,
    secret: str = "",
    timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
    max_body_bytes: int = DEFAULT_MAX_BODY_BYTES,
    write: bool = False,
) -> dict[str, Any]:
    config_path = config_path.expanduser()
    original = config_path.read_text(encoding="utf-8") if config_path.exists() else ""
    rust_shadow_block = _render_rust_shadow_block(
        endpoint=endpoint,
        ledger_path=ledger_path,
        secret=secret,
        timeout_seconds=timeout_seconds,
        max_body_bytes=max_body_bytes,
    )
    updated, action = _replace_top_level_block(
        original,
        section_name="rust_shadow",
        replacement=rust_shadow_block,
    )
    _validate_yaml(updated)
    changed = original != updated
    diff = "".join(
        difflib.unified_diff(
            original.splitlines(keepends=True),
            updated.splitlines(keepends=True),
            fromfile=str(config_path),
            tofile=f"{config_path} (rust_shadow)",
        )
    )

    backup_path: Path | None = None
    if write and changed:
        config_path.parent.mkdir(parents=True, exist_ok=True)
        if config_path.exists():
            backup_path = config_path.with_suffix(
                config_path.suffix
                + f".shadow-backup-{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')}"
            )
            shutil.copy2(config_path, backup_path)
        config_path.write_text(updated, encoding="utf-8")

    return {
        "schema_version": 1,
        "config_path": str(config_path),
        "status": "written" if write and changed else "unchanged" if not changed else "dry_run",
        "action": action,
        "changed": changed,
        "write": write,
        "backup_path": str(backup_path) if backup_path else None,
        "diff": diff,
        "rust_shadow": {
            "enabled": True,
            "endpoint": endpoint,
            "ledger_path": ledger_path,
            "secret_configured": bool(secret),
            "timeout_seconds": timeout_seconds,
            "max_body_bytes": max_body_bytes,
        },
    }


def render_text_result(result: dict[str, Any]) -> str:
    lines = [
        "Rust shadow config activation",
        f"config: {result['config_path']}",
        f"status: {result['status']}",
        f"action: {result['action']}",
        f"changed: {result['changed']}",
    ]
    if result["backup_path"]:
        lines.append(f"backup: {result['backup_path']}")
    if result["diff"]:
        lines.extend(["", "Diff:", result["diff"].rstrip()])
    else:
        lines.extend(["", "Diff: none"])
    if not result["write"] and result["changed"]:
        lines.extend(
            [
                "",
                "Dry run only. Re-run with --write to update the config file.",
            ]
        )
    if result["write"]:
        lines.extend(
            [
                "",
                "No service was restarted. Restart Python Session Manager deliberately after reviewing the diff.",
            ]
        )
    return "\n".join(lines)


def _render_rust_shadow_block(
    *,
    endpoint: str,
    ledger_path: str,
    secret: str,
    timeout_seconds: float,
    max_body_bytes: int,
) -> str:
    lines = [
        "rust_shadow:",
        "  enabled: true",
        f"  endpoint: {_quote_yaml(endpoint)}",
    ]
    if secret:
        lines.append(f"  secret: {_quote_yaml(secret)}")
    lines.extend(
        [
            f"  ledger_path: {_quote_yaml(ledger_path)}",
            f"  timeout_seconds: {timeout_seconds:g}",
            f"  max_body_bytes: {int(max_body_bytes)}",
        ]
    )
    return "\n".join(lines) + "\n"


def _quote_yaml(value: str) -> str:
    return json.dumps(value)


def _replace_top_level_block(
    original: str, *, section_name: str, replacement: str
) -> tuple[str, str]:
    lines = original.splitlines(keepends=True)
    starts = [
        index
        for index, line in enumerate(lines)
        if _is_top_level_section_line(line, section_name=section_name)
    ]
    if len(starts) > 1:
        raise ValueError(
            f"config contains multiple top-level {section_name} sections; "
            "remove duplicates before using the shadow config helper"
        )
    if not starts:
        prefix = original
        if prefix and not prefix.endswith("\n"):
            prefix += "\n"
        if prefix and not prefix.endswith("\n\n"):
            prefix += "\n"
        return prefix + replacement, "append"

    start = starts[0]
    end = _find_top_level_block_end(lines, start)
    updated = "".join(lines[:start]) + replacement + "".join(lines[end:])
    return updated, "replace"


def _find_top_level_block_end(lines: list[str], start: int) -> int:
    end = start + 1
    while end < len(lines):
        line = lines[end]
        stripped = line.strip()
        if line.startswith((" ", "\t")):
            end += 1
            continue
        if not stripped or stripped.startswith("#"):
            next_content = _next_content_line_index(lines, end + 1)
            if next_content is not None and lines[next_content].startswith((" ", "\t")):
                end += 1
                continue
            break
        break
    return end


def _next_content_line_index(lines: list[str], start: int) -> int | None:
    for index in range(start, len(lines)):
        stripped = lines[index].strip()
        if stripped and not stripped.startswith("#"):
            return index
    return None


def _is_top_level_section_line(line: str, *, section_name: str) -> bool:
    stripped = line.strip()
    if not stripped or stripped.startswith("#") or line.startswith((" ", "\t")):
        return False
    key_pattern = rf"(?:{re.escape(section_name)}|['\"]{re.escape(section_name)}['\"])"
    return re.match(rf"^{key_pattern}\s*:", stripped) is not None


def _validate_yaml(content: str) -> None:
    parsed = yaml.safe_load(content) if content.strip() else {}
    if parsed is not None and not isinstance(parsed, dict):
        raise ValueError("config YAML must be a mapping")
    rust_shadow = (parsed or {}).get("rust_shadow")
    if not isinstance(rust_shadow, dict):
        raise ValueError("rendered rust_shadow section is not a mapping")


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Safely prepare config.yaml for Rust shadow observation."
    )
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--endpoint", default=DEFAULT_RUST_ENDPOINT)
    parser.add_argument("--ledger", default=DEFAULT_LEDGER_PATH)
    parser.add_argument("--secret", default="")
    parser.add_argument("--timeout-seconds", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--max-body-bytes", type=int, default=DEFAULT_MAX_BODY_BYTES)
    parser.add_argument("--write", action="store_true")
    parser.add_argument("--json", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    try:
        result = prepare_shadow_config(
            config_path=args.config,
            endpoint=args.endpoint,
            ledger_path=args.ledger,
            secret=args.secret,
            timeout_seconds=args.timeout_seconds,
            max_body_bytes=args.max_body_bytes,
            write=args.write,
        )
    except ValueError as exc:
        if args.json:
            print(
                json.dumps(
                    {
                        "schema_version": 1,
                        "status": "error",
                        "config_path": str(args.config.expanduser()),
                        "error": str(exc),
                    },
                    indent=2,
                    sort_keys=True,
                )
            )
        else:
            print(f"error: {exc}", file=sys.stderr)
        return 2
    if args.json:
        print(json.dumps(result, indent=2, sort_keys=True))
    else:
        print(render_text_result(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
