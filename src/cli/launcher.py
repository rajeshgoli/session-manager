"""Compatibility launcher for the retired Python ``sm`` CLI."""

from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import Mapping, Optional, Sequence


_RUST_CLI_ENV = "SM_RUST_CLI"


def _resolved(path: Path) -> Path:
    try:
        return path.expanduser().resolve()
    except OSError:
        return path.expanduser().absolute()


def _is_python_console_script(path: Path) -> bool:
    try:
        with path.open("rb") as candidate:
            first_line = candidate.readline(256).lower()
    except OSError:
        return False
    return first_line.startswith(b"#!") and b"python" in first_line


def find_rust_sm(
    *,
    argv0: Optional[str] = None,
    environ: Optional[Mapping[str, str]] = None,
    repo_root: Optional[Path] = None,
) -> Optional[Path]:
    """Find an executable Rust ``sm`` without resolving back to this launcher."""
    env = os.environ if environ is None else environ
    current = _resolved(Path(argv0 or sys.argv[0]))
    root = _resolved(repo_root or Path(__file__).parents[2])

    candidates: list[Path] = []
    if explicit := env.get(_RUST_CLI_ENV):
        candidates.append(Path(explicit))
    candidates.extend(
        [
            root / ".local/bin/sm",
            root / "target/release/sm",
            root / "target/debug/sm",
        ]
    )
    for directory in env.get("PATH", "").split(os.pathsep):
        if directory:
            candidates.append(Path(directory) / "sm")

    seen: set[Path] = set()
    for candidate in candidates:
        resolved = _resolved(candidate)
        if resolved == current or resolved in seen:
            continue
        seen.add(resolved)
        if (
            resolved.is_file()
            and os.access(resolved, os.X_OK)
            and not _is_python_console_script(resolved)
        ):
            return resolved
    return None


def main(argv: Optional[Sequence[str]] = None) -> int:
    """Replace the Python console process with the Rust CLI."""
    rust_sm = find_rust_sm()
    if rust_sm is None:
        print(
            "Error: Rust sm CLI not found. Build it with "
            "`cargo build --release -p sm-server --bin sm` or set SM_RUST_CLI.",
            file=sys.stderr,
        )
        return 127

    arguments = list(sys.argv[1:] if argv is None else argv)
    os.execv(str(rust_sm), [str(rust_sm), *arguments])
    return 126
