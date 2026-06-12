import json
from dataclasses import replace

from scripts.rust_migration.cli_cutover_audit import (
    audit_cli_cutover_scope,
    main as cli_cutover_audit_main,
    render_text_report,
)
from scripts.rust_migration.contracts import MANIFEST_PATH, ContractManifest


def test_cli_cutover_audit_passes_current_manifest():
    report = audit_cli_cutover_scope(ContractManifest.load())

    assert report["status"] == "passed"
    assert report["summary"]["checked"] == 50
    assert report["summary"]["retained_python_and_rust"] == 36
    assert report["summary"]["retained_rust_only"] == 2
    assert report["summary"]["retired_rust_only"] == 12
    assert report["failures"] == []


def test_cli_cutover_audit_reports_missing_command_check():
    manifest = ContractManifest.load()
    altered = replace(
        manifest,
        checks=tuple(
            check for check in manifest.checks if check.id != "cli.send_help"
        ),
    )

    report = audit_cli_cutover_scope(altered)

    assert report["status"] == "failed"
    assert {
        "kind": "missing_check",
        "group": "retained_python_and_rust",
        "check_id": "cli.send_help",
        "detail": "check id is absent",
    } in report["failures"]


def test_cli_cutover_audit_reports_wrong_target_and_command():
    manifest = ContractManifest.load()
    altered_checks = []
    for check in manifest.checks:
        if check.id == "cli.list_devices_help":
            altered_checks.append(
                replace(
                    check,
                    target="python_and_rust",
                    command=("devices", "list", "--help"),
                )
            )
        else:
            altered_checks.append(check)
    altered = replace(manifest, checks=tuple(altered_checks))

    report = audit_cli_cutover_scope(altered)
    failure_kinds = {
        (failure["check_id"], failure["kind"]) for failure in report["failures"]
    }

    assert report["status"] == "failed"
    assert ("cli.list_devices_help", "wrong_target") in failure_kinds
    assert ("cli.list_devices_help", "wrong_command") in failure_kinds


def test_cli_cutover_audit_reports_retired_command_without_retirement_text():
    manifest = ContractManifest.load()
    altered_checks = []
    for check in manifest.checks:
        if check.id == "cli.what_retired":
            altered_checks.append(replace(check, expected_output_contains_any=()))
        else:
            altered_checks.append(check)
    altered = replace(manifest, checks=tuple(altered_checks))

    report = audit_cli_cutover_scope(altered)

    assert report["status"] == "failed"
    assert {
        "kind": "missing_output_contains_any",
        "group": "retired_rust_only",
        "check_id": "cli.what_retired",
        "detail": "expected one-of output fragment 'removed'",
    } in report["failures"]


def test_cli_cutover_audit_text_report_summarizes_groups():
    report = audit_cli_cutover_scope(ContractManifest.load())

    rendered = render_text_report(report)

    assert "CLI cutover audit: passed" in rendered
    assert "36 retained Python/Rust" in rendered
    assert "2 retained Rust-only" in rendered
    assert "12 retired Rust-only" in rendered


def test_cli_cutover_audit_cli_json_and_fail_on_gaps(tmp_path, capsys):
    raw = json.loads(MANIFEST_PATH.read_text())
    raw["checks"] = [
        check for check in raw["checks"] if check["id"] != "cli.send_help"
    ]
    manifest_path = tmp_path / "contracts_manifest.json"
    manifest_path.write_text(json.dumps(raw))

    exit_code = cli_cutover_audit_main(
        ["--manifest", str(manifest_path), "--json", "--fail-on-gaps"]
    )
    output = json.loads(capsys.readouterr().out)

    assert exit_code == 1
    assert output["status"] == "failed"
    assert output["summary"]["failed"] == 1
