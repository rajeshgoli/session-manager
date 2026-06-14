"""Create a disposable fixture workspace for Rust mutating contract checks.

The checked-in migration fixtures are intentionally read-only. Rust-core
contract checks that create sessions, enqueue messages, or arm stop
notifications need a writable copy plus a small EM/child seed. This helper
builds that workspace without touching repository fixture files or live
Session Manager state.
"""

from __future__ import annotations

import argparse
import json
import shutil
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[2]
READ_ONLY_FIXTURE_DIR = REPO_ROOT / "scripts/rust_migration/fixtures/read_only"

DEFAULT_SESSION_ID = "fixture-parent"
DEFAULT_SESSION_NAME = "FixtureParent"
DEFAULT_CHILD_SESSION_ID = "fixture-child"
DEFAULT_CHILD_SESSION_NAME = "FixtureChild"
DEFAULT_EM_SESSION_ID = "fixture-em"
DEFAULT_NOTIFY_CHILD_SESSION_ID = "fixture-notify-child"
DEFAULT_STOPPED_SESSION_ID = "fixture-stop"
DEFAULT_CLI_RESTORE_SESSION_ID = "fixture-cli-restore"


@dataclass(frozen=True)
class MutatingFixtureWorkspace:
    root: Path
    fixture_dir: Path
    config_path: Path
    state_file: Path
    log_dir: Path
    fixtures: dict[str, str]

    def to_dict(self) -> dict[str, Any]:
        return {
            "root": str(self.root),
            "fixture_dir": str(self.fixture_dir),
            "config_path": str(self.config_path),
            "state_file": str(self.state_file),
            "log_dir": str(self.log_dir),
            "fixtures": self.fixtures,
        }


def create_mutating_fixture_workspace(output_dir: Path | None = None) -> MutatingFixtureWorkspace:
    """Create a writable copy of the migration fixtures and return its config.

    If ``output_dir`` is supplied it must be empty or absent. Refusing to reuse a
    populated directory keeps the helper from overwriting ad hoc rehearsal state.
    """

    root = output_dir or Path(tempfile.mkdtemp(prefix="sm-rust-mutating-fixture-"))
    root = root.expanduser().resolve()
    if root.exists() and any(root.iterdir()):
        raise FileExistsError(f"output directory is not empty: {root}")
    root.mkdir(parents=True, exist_ok=True)

    fixture_dir = root / "read_only"
    shutil.copytree(READ_ONLY_FIXTURE_DIR, fixture_dir)

    log_dir = root / "logs"
    log_dir.mkdir()
    working_dir = root / "workspace"
    working_dir.mkdir()
    state_file = fixture_dir / "sessions.json"
    _seed_em_notify_sessions(state_file, log_dir, working_dir)

    config_path = root / "config.yaml"
    _write_config(config_path, root, fixture_dir, state_file, log_dir)

    fixtures = {
        "session_id": DEFAULT_SESSION_ID,
        "session_name": DEFAULT_SESSION_NAME,
        "child_session_id": DEFAULT_CHILD_SESSION_ID,
        "child_session_name": DEFAULT_CHILD_SESSION_NAME,
        "working_dir": str(working_dir),
        "message_text": "hello from rust fixture",
        "status_text": "writing Rust status",
        "urgent_message_text": "urgent fixture note",
        "wait_message_text": "wait fixture note",
        "clear_prompt_text": "new task after clear",
        "em_session_id": DEFAULT_EM_SESSION_ID,
        "notify_child_session_id": DEFAULT_NOTIFY_CHILD_SESSION_ID,
        "stopped_session_id": DEFAULT_STOPPED_SESSION_ID,
        "cli_restore_session_id": DEFAULT_CLI_RESTORE_SESSION_ID,
        "queue_job_id": "job-fixture",
    }
    return MutatingFixtureWorkspace(
        root=root,
        fixture_dir=fixture_dir,
        config_path=config_path,
        state_file=state_file,
        log_dir=log_dir,
        fixtures=fixtures,
    )


def _seed_em_notify_sessions(state_file: Path, log_dir: Path, working_dir: Path) -> None:
    state = json.loads(state_file.read_text())
    sessions = list(state.get("sessions", []))
    _relocate_existing_session_logs(sessions, log_dir)
    seeded_ids = {
        DEFAULT_EM_SESSION_ID,
        DEFAULT_NOTIFY_CHILD_SESSION_ID,
        DEFAULT_CLI_RESTORE_SESSION_ID,
    }
    sessions = [session for session in sessions if session.get("id") not in seeded_ids]
    created_at = "2026-06-01T00:10:00"
    sessions.extend(
        [
            {
                "id": DEFAULT_CLI_RESTORE_SESSION_ID,
                "name": "fixture-cli-restore",
                "working_dir": str(working_dir),
                "tmux_session": "fixture-cli-restore",
                "node": "primary",
                "provider": "claude",
                "log_file": str(log_dir / "fixture-cli-restore.log"),
                "status": "stopped",
                "created_at": created_at,
                "last_activity": created_at,
                "stopped_at": "2026-06-01T00:11:00",
                "friendly_name": "Fixture CLI Restore",
            },
            {
                "id": DEFAULT_EM_SESSION_ID,
                "name": "fixture-em",
                "working_dir": str(working_dir),
                "tmux_session": "",
                "node": "primary",
                "provider": "claude",
                "log_file": str(log_dir / "fixture-em.log"),
                "status": "running",
                "created_at": created_at,
                "last_activity": created_at,
                "friendly_name": "Fixture EM",
                "is_em": True,
            },
            {
                "id": DEFAULT_NOTIFY_CHILD_SESSION_ID,
                "name": "fixture-notify-child",
                "working_dir": str(working_dir),
                "tmux_session": "",
                "node": "primary",
                "provider": "claude",
                "log_file": str(log_dir / "fixture-notify-child.log"),
                "status": "running",
                "created_at": created_at,
                "last_activity": created_at,
                "friendly_name": "Fixture Notify Child",
                "parent_session_id": DEFAULT_EM_SESSION_ID,
                "is_em": False,
            },
        ]
    )
    state["sessions"] = sessions
    state_file.write_text(json.dumps(state, indent=2, sort_keys=True) + "\n")


def _relocate_existing_session_logs(sessions: list[dict[str, Any]], log_dir: Path) -> None:
    for session in sessions:
        session_id = str(session.get("id") or session.get("name") or "session")
        destination = log_dir / f"{session_id}.log"
        source = session.get("log_file")
        if isinstance(source, str) and source:
            source_path = Path(source).expanduser()
            if not source_path.is_absolute():
                source_path = REPO_ROOT / source_path
            if source_path.exists() and source_path.is_file():
                shutil.copyfile(source_path, destination)
            else:
                destination.touch()
        else:
            destination.touch()
        session["log_file"] = str(destination)


def _write_config(
    config_path: Path, root: Path, fixture_dir: Path, state_file: Path, log_dir: Path
) -> None:
    config_path.write_text(
        "\n".join(
            [
                "paths:",
                f"  state_file: {_yaml_string(state_file)}",
                f"  app_artifacts_dir: {_yaml_string(fixture_dir / 'apps')}",
                f"  message_queue_db: {_yaml_string(root / 'message_queue.db')}",
                f"  server_log_file: {_yaml_string(root / 'session-manager.log')}",
                f"  bug_reports_db: {_yaml_string(root / 'bug_reports.db')}",
                "",
                "queue_runner:",
                f"  state_dir: {_yaml_string(fixture_dir / 'queue-runner')}",
                "",
                "sm_send:",
                f"  db_path: {_yaml_string(root / 'message_queue.db')}",
                "",
                "tool_logging:",
                f"  db_path: {_yaml_string(root / 'tool_usage.db')}",
                "",
                "codex_events:",
                f"  db_path: {_yaml_string(root / 'codex_events.db')}",
                "",
                "codex_requests:",
                f"  db_path: {_yaml_string(root / 'codex_requests.db')}",
                "",
                "codex_observability:",
                f"  db_path: {_yaml_string(root / 'codex_observability.db')}",
                "",
                "rust_core:",
                "  fixture_writes_enabled: true",
                f"  log_dir: {_yaml_string(log_dir)}",
                "",
            ]
        )
    )


def _yaml_string(value: Path | str) -> str:
    return json.dumps(str(value))


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=None,
        help="Optional empty directory for the fixture workspace; defaults to mkdtemp",
    )
    parser.add_argument("--json", action="store_true", help="Emit machine-readable metadata")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    workspace = create_mutating_fixture_workspace(args.output_dir)
    payload = workspace.to_dict()
    if args.json:
        print(json.dumps(payload, indent=2, sort_keys=True))
    else:
        print(f"config_path={payload['config_path']}")
        for key, value in payload["fixtures"].items():
            print(f"fixture:{key}={value}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
