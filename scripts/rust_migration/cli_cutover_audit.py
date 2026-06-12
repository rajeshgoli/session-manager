"""Audit CLI contract coverage against the Rust cutover scope.

This is a manifest-quality gate, not a runtime command runner. It verifies that
the executable contract manifest still covers the owner-approved retained and
retired CLI surfaces with the expected classification, target, safety, and
command tuples.
"""

from __future__ import annotations

import argparse
import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from scripts.rust_migration.contracts import (
    MANIFEST_PATH,
    ContractCheck,
    ContractManifest,
)


@dataclass(frozen=True)
class ExpectedCliCheck:
    id: str
    command: tuple[str, ...]
    classification: str
    target: str
    safety: str
    group: str
    expected_exit: tuple[int, ...] | None = None
    output_contains_any: tuple[str, ...] = ()
    output_contains_all: tuple[str, ...] = ()


def _retained_python_and_rust(
    check_id: str, command: tuple[str, ...]
) -> ExpectedCliCheck:
    return ExpectedCliCheck(
        check_id,
        command,
        "retained",
        "python_and_rust",
        "read_only",
        "retained_python_and_rust",
        (0,),
    )


def _retained_rust_only(
    check_id: str,
    command: tuple[str, ...],
    *,
    output_contains_all: tuple[str, ...] = (),
) -> ExpectedCliCheck:
    return ExpectedCliCheck(
        check_id,
        command,
        "retained",
        "rust_only",
        "read_only",
        "retained_rust_only",
        (0,),
        output_contains_all=output_contains_all,
    )


def _retired_rust_only(check_id: str, command: tuple[str, ...]) -> ExpectedCliCheck:
    return ExpectedCliCheck(
        check_id,
        command,
        "retired",
        "rust_only",
        "read_only",
        "retired_rust_only",
        output_contains_any=("removed",),
    )


RETAINED_PYTHON_AND_RUST_CHECKS: tuple[ExpectedCliCheck, ...] = tuple(
    _retained_python_and_rust(check_id, command)
    for check_id, command in (
        ("cli.status_help", ("status", "--help")),
        ("cli.me_help", ("me", "--help")),
        ("cli.who_help", ("who", "--help")),
        ("cli.all_help", ("all", "--help")),
        ("cli.watch_help", ("watch", "--help")),
        ("cli.send_help", ("send", "--help")),
        ("cli.email_help", ("email", "--help")),
        ("cli.wait_help", ("wait", "--help")),
        ("cli.spawn_help", ("spawn", "--help")),
        ("cli.fork_help", ("fork", "--help")),
        ("cli.new_help", ("new", "--help")),
        ("cli.children_help", ("children", "--help")),
        ("cli.retire_help", ("retire", "--help")),
        ("cli.restore_help", ("restore", "--help")),
        ("cli.attach_help", ("attach", "--help")),
        ("cli.output_help", ("output", "--help")),
        ("cli.tail_raw_help", ("tail", "--help")),
        ("cli.clear_help", ("clear", "--help")),
        ("cli.handoff_help", ("handoff", "--help")),
        ("cli.context_monitor_help", ("context-monitor", "--help")),
        ("cli.maintainer_help", ("maintainer", "--help")),
        ("cli.register_help", ("register", "--help")),
        ("cli.unregister_help", ("unregister", "--help")),
        ("cli.lookup_help", ("lookup", "--help")),
        ("cli.roster_help", ("roster", "--help")),
        ("cli.queue_run_help", ("queue", "run", "--help")),
        ("cli.queue_list_help", ("queue", "list", "--help")),
        ("cli.queue_status_help", ("queue", "status", "--help")),
        ("cli.queue_cancel_help", ("queue", "cancel", "--help")),
        ("cli.review_help", ("review", "--help")),
        ("cli.request_codex_review_help", ("request-codex-review", "--help")),
        ("cli.claude_help", ("claude", "--help")),
        ("cli.codex_help", ("codex", "--help")),
        ("cli.codex_app_help", ("codex-app", "--help")),
        ("cli.codex_fork_help", ("codex-fork", "--help")),
        ("cli.codex_2_help", ("codex-2", "--help")),
    )
)

RETAINED_RUST_ONLY_CHECKS: tuple[ExpectedCliCheck, ...] = (
    _retained_rust_only(
        "cli.list_devices_help",
        ("list-devices", "--help"),
        output_contains_all=("--json",),
    ),
    _retained_rust_only(
        "cli.remove_device_help",
        ("remove-device", "--help"),
        output_contains_all=("DEVICE_ID", "--user-id"),
    ),
)

RETIRED_RUST_ONLY_CHECKS: tuple[ExpectedCliCheck, ...] = tuple(
    _retired_rust_only(check_id, command)
    for check_id, command in (
        ("cli.what_retired", ("what", "--help")),
        ("cli.kill_alias_retired", ("kill", "--help")),
        ("cli.dispatch_retired", ("dispatch", "--help")),
        ("cli.remind_retired", ("remind", "--help")),
        ("cli.watch_job_retired", ("watch-job", "--help")),
        ("cli.queue_policy_retired", ("queue", "ci-run", "--help")),
        ("cli.queue_policy_status_retired", ("queue", "ci-status", "--help")),
        ("cli.queue_policy_history_retired", ("queue", "ci-history", "--help")),
        ("cli.telegram_retired", ("telegram", "--help")),
        ("cli.telegram_alias_retired", ("tg", "--help")),
        ("cli.codex_legacy_retired", ("codex-legacy", "--help")),
        ("cli.codex_server_retired", ("codex-server", "--help")),
    )
)

EXPECTED_CHECKS: tuple[ExpectedCliCheck, ...] = (
    *RETAINED_PYTHON_AND_RUST_CHECKS,
    *RETAINED_RUST_ONLY_CHECKS,
    *RETIRED_RUST_ONLY_CHECKS,
)


def audit_cli_cutover_scope(manifest: ContractManifest) -> dict[str, Any]:
    checks_by_id = {check.id: check for check in manifest.checks}
    failures: list[dict[str, Any]] = []
    group_counts = {
        "retained_python_and_rust": 0,
        "retained_rust_only": 0,
        "retired_rust_only": 0,
    }

    if "specs/762_stage5_artifacts/cutover_scope.md" not in manifest.artifacts:
        failures.append(
            {
                "kind": "missing_cutover_scope_artifact",
                "group": "manifest",
                "check_id": None,
                "detail": "manifest artifacts do not include cutover_scope.md",
            }
        )

    for expected in EXPECTED_CHECKS:
        group_counts[expected.group] += 1
        check = checks_by_id.get(expected.id)
        if check is None:
            failures.append(_failure(expected, "missing_check", "check id is absent"))
            continue
        failures.extend(_validate_check(expected, check))

    status = "failed" if failures else "passed"
    return {
        "schema_version": 1,
        "status": status,
        "source_spec": manifest.source_spec,
        "summary": {
            "checked": len(EXPECTED_CHECKS),
            "failed": len(failures),
            "retained_python_and_rust": group_counts["retained_python_and_rust"],
            "retained_rust_only": group_counts["retained_rust_only"],
            "retired_rust_only": group_counts["retired_rust_only"],
        },
        "failures": failures,
    }


def render_text_report(report: dict[str, Any]) -> str:
    summary = report["summary"]
    lines = [
        f"CLI cutover audit: {report['status']}",
        (
            "Checked "
            f"{summary['checked']} manifest rows "
            f"({summary['retained_python_and_rust']} retained Python/Rust, "
            f"{summary['retained_rust_only']} retained Rust-only, "
            f"{summary['retired_rust_only']} retired Rust-only)"
        ),
    ]
    if not report["failures"]:
        lines.append("No CLI cutover gaps found.")
        return "\n".join(lines)
    lines.append(f"Failures: {summary['failed']}")
    for failure in report["failures"]:
        check_id = failure.get("check_id") or "-"
        lines.append(
            f"- {failure['kind']} [{failure['group']}] {check_id}: {failure['detail']}"
        )
    return "\n".join(lines)


def _validate_check(
    expected: ExpectedCliCheck, check: ContractCheck
) -> list[dict[str, Any]]:
    failures: list[dict[str, Any]] = []
    simple_fields = (
        ("surface", "cli", check.surface),
        ("classification", expected.classification, check.classification),
        ("target", expected.target, check.target),
        ("safety", expected.safety, check.safety),
        ("command", expected.command, check.command),
    )
    for field, expected_value, actual_value in simple_fields:
        if actual_value != expected_value:
            failures.append(
                _failure(
                    expected,
                    f"wrong_{field}",
                    f"expected {expected_value!r}, got {actual_value!r}",
                )
            )

    if expected.expected_exit is not None:
        if check.expected_exit != expected.expected_exit:
            failures.append(
                _failure(
                    expected,
                    "wrong_expected_exit",
                    f"expected {expected.expected_exit!r}, got {check.expected_exit!r}",
                )
            )
    elif not check.expected_exit or any(code == 0 for code in check.expected_exit):
        failures.append(
            _failure(
                expected,
                "wrong_expected_exit",
                f"retired command must expect only nonzero exits, got {check.expected_exit!r}",
            )
        )

    for needle in expected.output_contains_any:
        if needle not in check.expected_output_contains_any:
            failures.append(
                _failure(
                    expected,
                    "missing_output_contains_any",
                    f"expected one-of output fragment {needle!r}",
                )
            )
    for needle in expected.output_contains_all:
        if needle not in check.expected_output_contains_all:
            failures.append(
                _failure(
                    expected,
                    "missing_output_contains_all",
                    f"expected required output fragment {needle!r}",
                )
            )
    return failures


def _failure(expected: ExpectedCliCheck, kind: str, detail: str) -> dict[str, Any]:
    return {
        "kind": kind,
        "group": expected.group,
        "check_id": expected.id,
        "detail": detail,
    }


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--manifest",
        type=Path,
        default=MANIFEST_PATH,
        help=f"Contract manifest path (default: {MANIFEST_PATH})",
    )
    parser.add_argument("--json", action="store_true", help="Emit JSON report")
    parser.add_argument(
        "--fail-on-gaps",
        action="store_true",
        help="Exit nonzero when expected CLI cutover coverage is missing or wrong",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    manifest = ContractManifest.load(args.manifest)
    report = audit_cli_cutover_scope(manifest)
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render_text_report(report))
    if args.fail_on_gaps and report["failures"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
