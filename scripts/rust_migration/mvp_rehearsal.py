"""Run the Rust MVP sidecar rehearsal gate.

The rehearsal is intentionally an observation gate, not a cutover. Python stays
authoritative on port 8420 while Rust runs as a sidecar on port 8421 by default.
The report separates implemented core checks from gap probes so the next
implementation slice is visible without hiding current blockers.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import signal
import subprocess
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

from .baseline import run_baseline
from .contracts import ContractManifest, run_checks, summarize
from .final_backup import build_final_backup_report, configured_python_health_url
from .freeze_drain_plan import build_freeze_drain_plan
from .mutating_fixture import create_mutating_fixture_workspace
from .state_backup import build_backup_plan
from .state_preflight import build_state_preflight_report
from .state_restore import build_restore_report


PYTHON_BASE_URL = "http://127.0.0.1:8420"
RUST_BASE_URL = "http://127.0.0.1:8421"
DEFAULT_RUST_SM_BINARY = "target/debug/sm"
DEFAULT_READ_ONLY_FIXTURE_CONFIG = Path(
    "scripts/rust_migration/fixtures/read_only/config.yaml"
)

CORE_READ_CHECK_IDS = (
    "http.health",
    "http.health_detailed",
    "http.auth_session",
    "http.client_bootstrap",
    "http.client_analytics_summary",
    "http.events_state",
    "http.events_sse_hello",
    "http.sessions",
    "http.client_sessions",
    "http.nodes_list",
    "http.codex_review_requests_list",
    "http.queue_jobs_list",
    "http.api_sessions_absent",
    "http.public_watch_operational_data_denied",
    "http.scheduler_remind_retired",
    "http.job_watches_retired",
    "http.queue_policy_runs_retired",
)

MVP_GAP_PROBE_CHECK_IDS = ()

CORE_MUTATING_CHECK_IDS = (
    "cli.rust_core_spawn_fixture",
    "http.rust_core_review_existing_fixture",
    "http.rust_core_spawn_review_fixture",
    "cli.rust_core_send_fixture",
    "cli.rust_core_spawn_child_inherits_parent_fixture",
    "cli.rust_core_me_fixture",
    "cli.rust_core_all_fixture",
    "cli.rust_core_children_fixture",
    "cli.rust_core_children_target_fixture",
    "cli.rust_core_context_monitor_enable_child_fixture",
    "cli.rust_core_context_monitor_status_fixture",
    "cli.rust_core_task_complete_fixture",
    "cli.rust_core_turn_complete_fixture",
    "cli.rust_core_status_fixture",
    "cli.rust_core_send_urgent_fixture",
    "cli.rust_core_send_wait_fixture",
    "cli.rust_core_output_fixture",
    "cli.rust_core_clear_child_fixture",
    "cli.rust_core_tail_fixture",
    "cli.rust_core_maintainer_fixture",
    "cli.rust_core_register_fixture",
    "cli.rust_core_lookup_fixture",
    "cli.rust_core_roster_fixture",
    "cli.rust_core_unregister_fixture",
    "cli.rust_core_maintainer_clear_fixture",
    "cli.rust_core_retire_fixture",
    "cli.rust_queue_run_fixture",
    "cli.rust_queue_cancel_fixture",
    "http.rust_core_task_complete_fixture",
    "http.rust_core_turn_complete_fixture",
    "http.rust_core_notify_on_stop_fixture",
    "http.rust_queue_job_create_fixture",
    "http.rust_node_primary_restore_fixture",
    "cli.rust_core_restore_fixture",
    "cli.rust_core_restore_node_primary_fixture",
    "cli.rust_core_retire_restored_fixture",
    "cli.rust_core_wait_retired_fixture",
)

READ_ONLY_FIXTURE_CHECK_IDS = (
    "http.session_detail_fixture",
    "http.client_session_detail_fixture",
    "http.session_output_fixture",
    "http.attach_descriptor_fixture",
    "http.codex_events",
    "http.codex_activity_actions",
    "http.codex_pending_requests",
    "http.app_artifact_metadata",
    "http.queue_jobs_list",
    "http.queue_job_detail",
    "cli.rust_queue_list_fixture",
    "cli.rust_queue_list_json_fixture",
    "cli.rust_queue_status_fixture",
    "cli.rust_queue_status_json_fixture",
)

READ_ONLY_FIXTURE_VALUES = {
    "session_id": "fixture001",
    "codex_app_session_id": "fixture-codex",
    "app_name": "session-manager-android",
    "queue_job_id": "job-fixture",
}

BASELINE_CHECK_IDS = (
    "http.health",
    "http.health_detailed",
    "http.auth_session",
    "http.client_bootstrap",
    "http.events_state",
    "http.sessions",
    "http.client_sessions",
    "http.api_sessions_absent",
)

SHADOW_COMPARE_PATHS = (
    "/health",
    "/health/detailed",
    "/auth/session",
    "/client/bootstrap",
    "/client/analytics/summary",
    "/sessions",
    "/client/sessions",
    "/nodes",
    "/events/state",
)


def run_rehearsal(args: argparse.Namespace) -> dict[str, Any]:
    started_at = time.perf_counter()
    output_dir = _resolve_output_dir(args.output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    manifest = ContractManifest.load()
    report: dict[str, Any] = {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "inputs": {
            "python_base_url": args.python_base_url,
            "rust_base_url": args.rust_base_url,
            "config": str(args.config),
            "local_env": str(args.local_env) if args.local_env else None,
            "reuse_rust_sidecar": args.reuse_rust_sidecar,
            "include_gap_probes": not args.core_only,
            "skip_python_health": args.skip_python_health,
            "skip_smoke": args.skip_smoke,
            "skip_state_gate": args.skip_state_gate,
            "run_final_backup_gate": args.run_final_backup_gate,
            "final_backup_python_health_url": args.final_backup_python_health_url,
            "final_backup_stopped_hold_seconds": args.final_backup_stopped_hold_seconds,
            "final_backup_stopped_probe_count": args.final_backup_stopped_probe_count,
            "skip_baseline": args.skip_baseline,
            "baseline_repetitions": args.baseline_repetitions,
            "skip_shadow": args.skip_shadow,
            "skip_read_only_fixture_contracts": args.skip_read_only_fixture_contracts,
            "read_only_fixture_config": str(args.read_only_fixture_config),
            "read_only_fixture_rust_base_url": _read_only_fixture_rust_base_url(args),
            "skip_mutating_contracts": args.skip_mutating_contracts,
            "mutating_rust_base_url": _mutating_rust_base_url(args),
            "sm_binary": args.sm_binary,
            "cargo": args.cargo,
            "output_dir": str(output_dir),
        },
        "steps": [],
        "blockers": [],
        "artifacts": {},
    }

    rust_process: subprocess.Popen[str] | None = None
    rust_ready = False
    try:
        if not args.skip_python_health and not args.run_final_backup_gate:
            python_health = _probe_health(args.python_base_url, args.timeout)
            report["steps"].append({"name": "python_health", **python_health})
            if python_health["status"] != "passed":
                _add_blocker(report, "python_health", python_health["detail"])
        elif args.run_final_backup_gate and not args.skip_python_health:
            report["steps"].append(
                {
                    "name": "python_health",
                    "status": "skipped",
                    "detail": "Python liveness probe is superseded by stopped-origin final backup gate",
                }
            )

        if args.skip_state_gate:
            report["steps"].append(
                {
                    "name": "state_ownership_backup_restore_gate",
                    "status": "skipped",
                    "detail": "state gate skipped by flag",
                }
            )
        else:
            state_gate = _run_state_gate(args, output_dir)
            report["steps"].append(state_gate)
            for name, path in state_gate.get("artifacts", {}).items():
                report["artifacts"][f"state_gate_{name}"] = path
            if state_gate["status"] != "passed":
                _add_state_gate_blockers(report, state_gate)
                if not any(
                    blocker["kind"] == "state_ownership_gate"
                    for blocker in report["blockers"]
                ):
                    _add_blocker(
                        report,
                        "state_ownership_gate",
                        state_gate.get("detail", "state ownership gate failed"),
                    )
                return _finalize_report(report, output_dir, started_at)

        if args.run_final_backup_gate:
            final_backup = _run_final_backup_gate(args, output_dir)
            report["steps"].append(final_backup)
            for name, path in final_backup.get("artifacts", {}).items():
                report["artifacts"][f"final_backup_{name}"] = path
            if final_backup["status"] != "passed":
                for blocker in final_backup.get("blockers", []):
                    _add_blocker(
                        report,
                        "final_backup_gate",
                        blocker.get("detail", str(blocker)),
                    )
                if not final_backup.get("blockers"):
                    _add_blocker(
                        report,
                        "final_backup_gate",
                        final_backup.get("detail", "final backup gate failed"),
                    )
                return _finalize_report(report, output_dir, started_at)

        if args.reuse_rust_sidecar:
            rust_health = _probe_health(args.rust_base_url, args.timeout)
            report["steps"].append({"name": "rust_sidecar_reuse_health", **rust_health})
            if rust_health["status"] != "passed":
                _add_blocker(report, "rust_sidecar_reuse_health", rust_health["detail"])
            else:
                rust_ready = True
        else:
            preflight = _probe_health(args.rust_base_url, args.timeout)
            if preflight["status"] == "passed":
                detail = (
                    f"{args.rust_base_url} was already healthy before fresh sidecar "
                    "start; rerun with --reuse-rust-sidecar to target an existing "
                    "process explicitly"
                )
                report["steps"].append(
                    {
                        "name": "rust_sidecar_fresh_start_preflight",
                        "status": "failed",
                        "detail": detail,
                    }
                )
                _add_blocker(report, "rust_sidecar_fresh_start_preflight", detail)
            else:
                report["steps"].append(
                    {
                        "name": "rust_sidecar_fresh_start_preflight",
                        "status": "passed",
                        "detail": preflight["detail"],
                    }
                )
                rust_log_path = output_dir / "rust-sidecar.log"
                start_step: dict[str, Any] = {"name": "rust_sidecar_start"}
                start_failed = False
                try:
                    rust_process = _start_rust_sidecar(args, output_dir)
                except OSError as exc:
                    start_failed = True
                    detail = f"failed to start sidecar: {exc}"
                    start_step.update(
                        {
                            "status": "failed",
                            "detail": detail,
                            "log_path": str(rust_log_path),
                        }
                    )
                    report["steps"].append(start_step)
                    _add_blocker(report, "rust_sidecar_start", detail)
                    rust_process = None
                if rust_process is None:
                    rust_health: dict[str, Any] = {
                        "status": "failed",
                        "detail": start_step["detail"],
                    }
                    sidecar_exited = False
                else:
                    start_step.update(
                        {
                            "status": "passed",
                            "pid": rust_process.pid,
                            "log_path": str(rust_log_path),
                        }
                    )
                    report["steps"].append(start_step)
                    rust_health = _wait_for_sidecar_health(
                        rust_process,
                        args.rust_base_url,
                        timeout_seconds=args.startup_timeout,
                        request_timeout=args.timeout,
                        log_path=rust_log_path,
                    )
                    sidecar_exited = rust_health.pop("process_exited", False)
                    if sidecar_exited:
                        start_step["status"] = "failed"
                        start_step["exit_code"] = rust_health.get("exit_code")
                        start_step["detail"] = rust_health["detail"]
                        _add_blocker(report, "rust_sidecar_start", rust_health["detail"])
                report["steps"].append({"name": "rust_sidecar_health", **rust_health})
                if rust_health["status"] != "passed" and not sidecar_exited and not start_failed:
                    _add_blocker(report, "rust_sidecar_health", rust_health["detail"])
                else:
                    rust_ready = rust_health["status"] == "passed"

        if not rust_ready:
            report["steps"].append(
                {
                    "name": "rust_sidecar_dependent_checks",
                    "status": "skipped",
                    "detail": "Rust sidecar was not verified as ready",
                }
            )
            return _finalize_report(report, output_dir, started_at)

        if not args.skip_smoke:
            smoke = _run_command(
                ["scripts/rust-mvp-smoke.sh"],
                cwd=Path.cwd(),
                timeout_seconds=args.smoke_timeout,
            )
            report["steps"].append({"name": "isolated_runtime_smoke", **smoke})
            if smoke["status"] != "passed":
                _add_blocker(report, "isolated_runtime_smoke", smoke["detail"])

        core_contracts = _run_contract_group(
            manifest,
            name="rust_core_sidecar_contracts",
            target="rust",
            base_url=args.rust_base_url,
            sm_binary=args.sm_binary,
            check_ids=set(CORE_READ_CHECK_IDS),
            timeout_seconds=args.timeout,
        )
        report["steps"].append(core_contracts)
        _add_contract_blockers(report, core_contracts, blocker_kind="core_contract")

        if args.skip_read_only_fixture_contracts:
            report["steps"].append(
                {
                    "name": "rust_read_only_fixture_contracts",
                    "status": "skipped",
                    "detail": "read-only fixture contracts skipped by flag",
                }
            )
        else:
            read_only_fixture_contracts = _run_read_only_fixture_contract_group(
                manifest, args, output_dir
            )
            report["steps"].append(read_only_fixture_contracts)
            for name, path in read_only_fixture_contracts.get("artifacts", {}).items():
                report["artifacts"][f"read_only_fixture_{name}"] = path
            _add_contract_blockers(
                report,
                read_only_fixture_contracts,
                blocker_kind="read_only_fixture_contract",
                include_skipped=True,
            )
            if read_only_fixture_contracts["status"] == "failed" and not any(
                blocker["kind"] == "read_only_fixture_contract"
                for blocker in report["blockers"]
            ):
                _add_blocker(
                    report,
                    "read_only_fixture_contract",
                    read_only_fixture_contracts.get(
                        "detail", "read-only fixture contract step failed"
                    ),
                )

        if args.skip_mutating_contracts:
            report["steps"].append(
                {
                    "name": "rust_core_mutating_fixture_contracts",
                    "status": "skipped",
                    "detail": "mutating fixture contracts skipped by flag",
                }
            )
        else:
            cli_step = _ensure_rust_cli_available(args)
            if cli_step is not None:
                report["steps"].append(cli_step)
                if cli_step["status"] != "passed":
                    _add_blocker(report, "rust_cli_build", cli_step["detail"])
                    report["steps"].append(
                        {
                            "name": "rust_core_mutating_fixture_contracts",
                            "status": "skipped",
                            "detail": "Rust CLI binary was not available",
                        }
                    )
                    return _finalize_report(report, output_dir, started_at)

            mutating_contracts = _run_mutating_contract_group(manifest, args, output_dir)
            report["steps"].append(mutating_contracts)
            for name, path in mutating_contracts.get("artifacts", {}).items():
                report["artifacts"][f"mutating_{name}"] = path
            _add_contract_blockers(
                report,
                mutating_contracts,
                blocker_kind="mutating_core_contract",
                include_skipped=True,
            )
            if mutating_contracts["status"] == "failed" and not any(
                blocker["kind"] == "mutating_core_contract"
                for blocker in report["blockers"]
            ):
                _add_blocker(
                    report,
                    "mutating_core_contract",
                    mutating_contracts.get("detail", "mutating contract step failed"),
                )

        if not args.core_only:
            gap_contracts = _run_contract_group(
                manifest,
                name="rust_mvp_gap_probes",
                target="rust",
                base_url=args.rust_base_url,
                sm_binary=args.sm_binary,
                check_ids=set(MVP_GAP_PROBE_CHECK_IDS),
                timeout_seconds=args.timeout,
            )
            report["steps"].append(gap_contracts)
            _add_contract_blockers(report, gap_contracts, blocker_kind="mvp_gap")

        if not args.skip_baseline:
            baseline_dir = output_dir / "baseline"
            baseline_dir.mkdir(parents=True, exist_ok=True)
            python_baseline = run_baseline(
                target="python",
                base_url=args.python_base_url,
                sm_binary=args.sm_binary,
                repetitions=args.baseline_repetitions,
                output=baseline_dir / "python-baseline.json",
                server_pid=None,
                check_ids=set(BASELINE_CHECK_IDS),
            )
            report["artifacts"]["python_baseline"] = str(
                baseline_dir / "python-baseline.json"
            )
            report["steps"].append(_baseline_step("python_baseline", python_baseline))

            rust_pid = rust_process.pid if rust_process else None
            rust_baseline = run_baseline(
                target="rust",
                base_url=args.rust_base_url,
                sm_binary=args.sm_binary,
                repetitions=args.baseline_repetitions,
                output=baseline_dir / "rust-baseline.json",
                server_pid=rust_pid,
                check_ids=set(BASELINE_CHECK_IDS),
            )
            report["artifacts"]["rust_baseline"] = str(baseline_dir / "rust-baseline.json")
            report["steps"].append(_baseline_step("rust_baseline", rust_baseline))
            for step in report["steps"][-2:]:
                if step["status"] != "passed":
                    _add_blocker(report, step["name"], step["detail"])

        if not args.skip_shadow:
            shadow_paths = _resolve_shadow_compare_paths(
                python_base_url=args.python_base_url,
                base_paths=SHADOW_COMPARE_PATHS,
                timeout_seconds=args.timeout,
            )
            report["steps"].append(shadow_paths)
            shadow = _run_shadow_summary(
                python_base_url=args.python_base_url,
                rust_base_url=args.rust_base_url,
                paths=tuple(shadow_paths["paths"]),
                timeout_seconds=args.timeout,
                shadow_secret=args.shadow_secret,
            )
            report["steps"].append({"name": "shadow_read_summary", **shadow})
            if shadow["summary"]["failed"]:
                _add_blocker(
                    report,
                    "shadow_read_summary",
                    f"{shadow['summary']['failed']} shadow comparison request(s) failed",
                )
    finally:
        if rust_process is not None:
            _terminate_process_group(rust_process)

    return _finalize_report(report, output_dir, started_at)


def _finalize_report(
    report: dict[str, Any], output_dir: Path, started_at: float
) -> dict[str, Any]:
    report["elapsed_seconds"] = round(time.perf_counter() - started_at, 3)
    report["summary"] = {
        "status": "blocked" if report["blockers"] else "passed",
        "blocker_count": len(report["blockers"]),
        "step_count": len(report["steps"]),
    }
    report_path = output_dir / "mvp-rehearsal-report.json"
    report["artifacts"]["report"] = str(report_path)
    report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    return report


def _resolve_output_dir(raw: Path | None) -> Path:
    if raw is not None:
        return raw
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return Path(".local/rust-mvp-rehearsals") / stamp


def _start_rust_sidecar(
    args: argparse.Namespace, output_dir: Path
) -> subprocess.Popen[str]:
    return _start_rust_sidecar_process(
        cargo=args.cargo,
        base_url=args.rust_base_url,
        config=args.config,
        local_env=args.local_env,
        log_path=output_dir / "rust-sidecar.log",
    )


def _start_rust_sidecar_process(
    *,
    cargo: str,
    base_url: str,
    config: Path,
    local_env: Path | None,
    log_path: Path,
) -> subprocess.Popen[str]:
    command = [
        cargo,
        "run",
        "-p",
        "sm-server",
        "--bin",
        "sm-server",
        "--",
        "--host",
        _host_from_base_url(base_url),
        "--port",
        str(_port_from_base_url(base_url)),
        "--config",
        str(config),
    ]
    if local_env:
        command.extend(["--local-env", str(local_env)])
    log_file = log_path.open("w")
    try:
        return subprocess.Popen(
            command,
            cwd=Path.cwd(),
            text=True,
            stdout=log_file,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    finally:
        log_file.close()


def _probe_health(base_url: str, timeout_seconds: float) -> dict[str, Any]:
    started_at = time.perf_counter()
    try:
        status, body, _headers = _http_request(
            "GET", base_url.rstrip("/") + "/health", timeout_seconds=timeout_seconds
        )
    except Exception as exc:  # noqa: BLE001 - report should preserve the concrete transport error.
        return {
            "status": "failed",
            "elapsed_ms": _elapsed_ms(started_at),
            "detail": str(exc),
        }
    if status == 200 and b'"healthy"' in body:
        return {
            "status": "passed",
            "elapsed_ms": _elapsed_ms(started_at),
            "detail": "HTTP 200 healthy",
        }
    return {
        "status": "failed",
        "elapsed_ms": _elapsed_ms(started_at),
        "detail": f"expected healthy HTTP 200, got HTTP {status}",
    }


def _wait_for_health(
    base_url: str, *, timeout_seconds: float, request_timeout: float
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout_seconds
    last_detail = "not attempted"
    while time.monotonic() < deadline:
        result = _probe_health(base_url, request_timeout)
        if result["status"] == "passed":
            return result
        last_detail = result["detail"]
        time.sleep(0.5)
    return {
        "status": "failed",
        "elapsed_ms": round(timeout_seconds * 1000, 3),
        "detail": f"timed out waiting for health: {last_detail}",
    }


def _wait_for_sidecar_health(
    process: subprocess.Popen[str],
    base_url: str,
    *,
    timeout_seconds: float,
    request_timeout: float,
    log_path: Path,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout_seconds
    last_detail = "not attempted"
    started_at = time.perf_counter()
    while time.monotonic() < deadline:
        exit_code = process.poll()
        if exit_code is not None:
            detail = (
                f"sidecar exited with code {exit_code} before health; "
                f"last health error: {last_detail}"
            )
            log_tail = _tail_file(log_path)
            if log_tail:
                detail = f"{detail}; log tail: {log_tail}"
            return {
                "status": "failed",
                "process_exited": True,
                "exit_code": exit_code,
                "elapsed_ms": _elapsed_ms(started_at),
                "detail": detail,
            }
        result = _probe_health(base_url, request_timeout)
        if result["status"] == "passed":
            return result
        last_detail = result["detail"]
        time.sleep(0.5)
    return {
        "status": "failed",
        "elapsed_ms": round(timeout_seconds * 1000, 3),
        "detail": f"timed out waiting for health: {last_detail}",
    }


def _run_contract_group(
    manifest: ContractManifest,
    *,
    name: str,
    target: str,
    base_url: str,
    sm_binary: str,
    check_ids: set[str],
    timeout_seconds: float,
    fixtures: dict[str, str] | None = None,
    include_mutating: bool = False,
    fail_on_skipped: bool = False,
) -> dict[str, Any]:
    started_at = time.perf_counter()
    results = run_checks(
        manifest,
        target=target,
        base_url=base_url,
        sm_binary=sm_binary,
        session_id=None,
        fixtures=fixtures or {},
        include_mutating=include_mutating,
        check_ids=check_ids,
        timeout_seconds=timeout_seconds,
    )
    summary = summarize(results)
    failed = summary.get("failed", 0)
    skipped = summary.get("skipped", 0)
    return {
        "name": name,
        "status": (
            "passed"
            if failed == 0 and (not fail_on_skipped or skipped == 0)
            else "failed"
        ),
        "elapsed_ms": _elapsed_ms(started_at),
        "summary": summary,
        "results": [result.to_dict() for result in results],
    }


def _ensure_rust_cli_available(args: argparse.Namespace) -> dict[str, Any] | None:
    if args.sm_binary != DEFAULT_RUST_SM_BINARY:
        return None
    if Path(args.sm_binary).exists():
        return {
            "name": "rust_cli_build",
            "status": "passed",
            "detail": f"{args.sm_binary} already exists",
        }
    step = _run_command(
        [args.cargo, "build", "-p", "sm-server", "--bin", "sm"],
        cwd=Path.cwd(),
        timeout_seconds=args.smoke_timeout,
    )
    step["name"] = "rust_cli_build"
    return step


def _run_mutating_contract_group(
    manifest: ContractManifest, args: argparse.Namespace, output_dir: Path
) -> dict[str, Any]:
    mutating_base_url = _mutating_rust_base_url(args)
    step_name = "rust_core_mutating_fixture_contracts"
    started_at = time.perf_counter()
    try:
        workspace = create_mutating_fixture_workspace(output_dir / "mutating-fixture")
    except OSError as exc:
        return {
            "name": step_name,
            "status": "failed",
            "elapsed_ms": _elapsed_ms(started_at),
            "detail": f"failed to create mutating fixture workspace: {exc}",
            "summary": {"passed": 0, "failed": 1, "skipped": 0},
            "results": [],
            "artifacts": {},
        }
    log_path = output_dir / "rust-mutating-sidecar.log"
    artifacts = {
        "fixture_root": str(workspace.root),
        "fixture_config": str(workspace.config_path),
        "fixture_state": str(workspace.state_file),
        "sidecar_log": str(log_path),
    }
    preflight = _probe_health(mutating_base_url, args.timeout)
    if preflight["status"] == "passed":
        return {
            "name": step_name,
            "status": "failed",
            "elapsed_ms": _elapsed_ms(started_at),
            "detail": (
                f"{mutating_base_url} was already healthy before mutating sidecar "
                "start; choose --mutating-rust-base-url with a free port"
            ),
            "summary": {"passed": 0, "failed": 1, "skipped": 0},
            "results": [],
            "artifacts": artifacts,
        }

    mutating_process: subprocess.Popen[str] | None = None
    try:
        try:
            mutating_process = _start_rust_sidecar_process(
                cargo=args.cargo,
                base_url=mutating_base_url,
                config=workspace.config_path,
                local_env=None,
                log_path=log_path,
            )
        except OSError as exc:
            return {
                "name": step_name,
                "status": "failed",
                "elapsed_ms": _elapsed_ms(started_at),
                "detail": f"failed to start mutating sidecar: {exc}",
                "summary": {"passed": 0, "failed": 1, "skipped": 0},
                "results": [],
                "artifacts": artifacts,
            }

        health = _wait_for_sidecar_health(
            mutating_process,
            mutating_base_url,
            timeout_seconds=args.startup_timeout,
            request_timeout=args.timeout,
            log_path=log_path,
        )
        sidecar_exited = health.pop("process_exited", False)
        if health["status"] != "passed":
            detail = health["detail"]
            if sidecar_exited and health.get("exit_code") is not None:
                detail = f"{detail}; exit_code={health['exit_code']}"
            return {
                "name": step_name,
                "status": "failed",
                "elapsed_ms": _elapsed_ms(started_at),
                "detail": detail,
                "summary": {"passed": 0, "failed": 1, "skipped": 0},
                "results": [],
                "artifacts": artifacts,
            }

        fixtures = dict(workspace.fixtures)
        fixtures["base_url"] = mutating_base_url
        step = _run_contract_group(
            manifest,
            name=step_name,
            target="rust",
            base_url=mutating_base_url,
            sm_binary=args.sm_binary,
            check_ids=set(CORE_MUTATING_CHECK_IDS),
            timeout_seconds=args.timeout,
            fixtures=fixtures,
            include_mutating=True,
            fail_on_skipped=True,
        )
        step["artifacts"] = artifacts
        return step
    finally:
        if mutating_process is not None:
            _terminate_process_group(mutating_process)


def _run_read_only_fixture_contract_group(
    manifest: ContractManifest, args: argparse.Namespace, output_dir: Path
) -> dict[str, Any]:
    fixture_base_url = _read_only_fixture_rust_base_url(args)
    step_name = "rust_read_only_fixture_contracts"
    started_at = time.perf_counter()
    log_path = output_dir / "rust-read-only-fixture-sidecar.log"
    artifacts = {
        "fixture_root": str(args.read_only_fixture_config.parent),
        "fixture_config": str(args.read_only_fixture_config),
        "sidecar_log": str(log_path),
    }
    if not args.read_only_fixture_config.exists():
        return {
            "name": step_name,
            "status": "failed",
            "elapsed_ms": _elapsed_ms(started_at),
            "detail": f"read-only fixture config not found: {args.read_only_fixture_config}",
            "summary": {"passed": 0, "failed": 1, "skipped": 0},
            "results": [],
            "artifacts": artifacts,
        }

    preflight = _probe_health(fixture_base_url, args.timeout)
    if preflight["status"] == "passed":
        return {
            "name": step_name,
            "status": "failed",
            "elapsed_ms": _elapsed_ms(started_at),
            "detail": (
                f"{fixture_base_url} was already healthy before read-only fixture "
                "sidecar start; choose --read-only-fixture-rust-base-url with a free port"
            ),
            "summary": {"passed": 0, "failed": 1, "skipped": 0},
            "results": [],
            "artifacts": artifacts,
        }

    fixture_process: subprocess.Popen[str] | None = None
    try:
        try:
            fixture_process = _start_rust_sidecar_process(
                cargo=args.cargo,
                base_url=fixture_base_url,
                config=args.read_only_fixture_config,
                local_env=None,
                log_path=log_path,
            )
        except OSError as exc:
            return {
                "name": step_name,
                "status": "failed",
                "elapsed_ms": _elapsed_ms(started_at),
                "detail": f"failed to start read-only fixture sidecar: {exc}",
                "summary": {"passed": 0, "failed": 1, "skipped": 0},
                "results": [],
                "artifacts": artifacts,
            }

        health = _wait_for_sidecar_health(
            fixture_process,
            fixture_base_url,
            timeout_seconds=args.startup_timeout,
            request_timeout=args.timeout,
            log_path=log_path,
        )
        sidecar_exited = health.pop("process_exited", False)
        if health["status"] != "passed":
            detail = health["detail"]
            if sidecar_exited and health.get("exit_code") is not None:
                detail = f"{detail}; exit_code={health['exit_code']}"
            return {
                "name": step_name,
                "status": "failed",
                "elapsed_ms": _elapsed_ms(started_at),
                "detail": detail,
                "summary": {"passed": 0, "failed": 1, "skipped": 0},
                "results": [],
                "artifacts": artifacts,
            }

        fixtures = dict(READ_ONLY_FIXTURE_VALUES)
        fixtures["base_url"] = fixture_base_url
        step = _run_contract_group(
            manifest,
            name=step_name,
            target="rust",
            base_url=fixture_base_url,
            sm_binary=args.sm_binary,
            check_ids=set(READ_ONLY_FIXTURE_CHECK_IDS),
            timeout_seconds=args.timeout,
            fixtures=fixtures,
            include_mutating=False,
            fail_on_skipped=True,
        )
        step["artifacts"] = artifacts
        return step
    finally:
        if fixture_process is not None:
            _terminate_process_group(fixture_process)


def _run_state_gate(args: argparse.Namespace, output_dir: Path) -> dict[str, Any]:
    step_name = "state_ownership_backup_restore_gate"
    started_at = time.perf_counter()
    root = output_dir / "state-gate"
    root.mkdir(parents=True, exist_ok=True)
    artifacts = {
        "preflight_report": str(root / "state-preflight-report.json"),
        "backup_report": str(root / "state-backup-report.json"),
        "backup_manifest": str(root / "backup" / "state-backup-manifest.json"),
        "restore_verify_report": str(root / "state-restore-verify-report.json"),
        "restore_report": str(root / "restore" / "state-restore-report.json"),
        "restore_root": str(root / "restore"),
        "freeze_drain_report": str(root / "freeze-drain-report.json"),
        "freeze_drain_ledger": str(root / "freeze-drain-ledger.jsonl"),
    }
    reports: dict[str, dict[str, Any] | None] = {
        "preflight": None,
        "backup": None,
        "restore_verify": None,
        "restore_execute": None,
        "freeze_drain": None,
    }

    try:
        preflight = build_state_preflight_report(
            config_path=args.config,
            local_env_path=args.local_env,
        )
        reports["preflight"] = preflight
        _write_json(Path(artifacts["preflight_report"]), preflight)

        backup = build_backup_plan(
            config_path=args.config,
            local_env_path=args.local_env,
            output_dir=root / "backup",
            execute=True,
        )
        reports["backup"] = backup
        _write_json(Path(artifacts["backup_report"]), backup)

        if backup["status"] == "copied":
            restore_verify = build_restore_report(
                manifest_path=Path(backup["manifest_path"]),
            )
            reports["restore_verify"] = restore_verify
            _write_json(Path(artifacts["restore_verify_report"]), restore_verify)

            restore_execute = build_restore_report(
                manifest_path=Path(backup["manifest_path"]),
                restore_dir=root / "restore",
                execute_restore=True,
            )
            reports["restore_execute"] = restore_execute
            # build_restore_report writes this file on success. Write it here too so
            # blocked/error reports still have an artifact at the advertised path.
            _write_json(Path(artifacts["restore_report"]), restore_execute)

        freeze_drain = build_freeze_drain_plan(
            config_path=args.config,
            local_env_path=args.local_env,
            ledger_path=root / "freeze-drain-ledger.jsonl",
            record_plan=True,
        )
        reports["freeze_drain"] = freeze_drain
        _write_json(Path(artifacts["freeze_drain_report"]), freeze_drain)
    except Exception as exc:  # noqa: BLE001 - preserve concrete tool failure in report.
        return {
            "name": step_name,
            "status": "failed",
            "elapsed_ms": _elapsed_ms(started_at),
            "detail": f"state gate raised {type(exc).__name__}: {exc}",
            "summary": _state_gate_summary(reports),
            "blockers": [
                {
                    "substep": "state_gate",
                    "kind": "exception",
                    "detail": f"{type(exc).__name__}: {exc}",
                }
            ],
            "artifacts": _existing_artifacts(artifacts),
        }

    blockers = _state_gate_report_blockers(reports)
    expected_statuses = {
        "preflight": "passed",
        "backup": "copied",
        "restore_verify": "verified",
        "restore_execute": "restored",
        "freeze_drain": "planned",
    }
    missing_or_bad = [
        f"{name}={report.get('status') if report else 'skipped'}"
        for name, expected in expected_statuses.items()
        if not (report := reports.get(name)) or report.get("status") != expected
    ]
    if missing_or_bad and not blockers:
        blockers.append(
            {
                "substep": "state_gate",
                "kind": "unexpected_status",
                "detail": "expected successful state gate statuses; got "
                + ", ".join(missing_or_bad),
            }
        )

    return {
        "name": step_name,
        "status": "passed" if not blockers else "failed",
        "elapsed_ms": _elapsed_ms(started_at),
        "detail": "state backup/restore gate completed" if not blockers else "state backup/restore gate blocked",
        "summary": _state_gate_summary(reports),
        "blockers": blockers,
        "artifacts": _existing_artifacts(artifacts),
    }


def _run_final_backup_gate(args: argparse.Namespace, output_dir: Path) -> dict[str, Any]:
    step_name = "stopped_origin_final_backup_gate"
    started_at = time.perf_counter()
    root = output_dir / "final-backup"
    root.mkdir(parents=True, exist_ok=True)
    artifacts = {
        "report": str(root / "final-backup-report.json"),
        "backup_manifest": str(root / "backup" / "state-backup-manifest.json"),
        "ledger": str(root / "final-backup-ledger.jsonl"),
    }

    try:
        final_backup = build_final_backup_report(
            config_path=args.config,
            local_env_path=args.local_env,
            output_dir=root / "backup",
            python_health_url=_final_backup_python_health_url(args),
            health_timeout_seconds=args.final_backup_health_timeout,
            stopped_hold_seconds=args.final_backup_stopped_hold_seconds,
            stopped_probe_count=args.final_backup_stopped_probe_count,
            execute=True,
            ledger_path=root / "final-backup-ledger.jsonl",
            record_ledger=True,
        )
        _write_json(Path(artifacts["report"]), final_backup)
    except Exception as exc:  # noqa: BLE001 - preserve concrete tool failure in report.
        return {
            "name": step_name,
            "status": "failed",
            "elapsed_ms": _elapsed_ms(started_at),
            "detail": f"final backup gate raised {type(exc).__name__}: {exc}",
            "summary": {
                "final_backup_status": None,
                "python_origin_stopped": None,
                "backup_status": None,
                "backup_copied": None,
                "ledger_written": None,
            },
            "blockers": [
                {
                    "substep": "final_backup",
                    "kind": "exception",
                    "detail": f"{type(exc).__name__}: {exc}",
                }
            ],
            "artifacts": _existing_artifacts(artifacts),
        }

    blockers = [
        {
            "substep": blocker.get("store_id", "final_backup"),
            "kind": blocker.get("kind", "blocker"),
            "detail": blocker.get("detail", str(blocker)),
        }
        for blocker in final_backup.get("blockers", [])
    ]
    status = final_backup.get("status")
    if status != "copied" and not blockers:
        blockers.append(
            {
                "substep": "final_backup",
                "kind": "unexpected_status",
                "detail": f"expected copied final backup; got {status}",
            }
        )

    return {
        "name": step_name,
        "status": "passed" if not blockers else "failed",
        "elapsed_ms": _elapsed_ms(started_at),
        "detail": "stopped-origin final backup completed" if not blockers else "stopped-origin final backup blocked",
        "summary": {
            "final_backup_status": status,
            "python_origin_stopped": (final_backup.get("python_origin") or {}).get("stopped"),
            "backup_status": (final_backup.get("summary") or {}).get("backup_status"),
            "backup_copied": (final_backup.get("summary") or {}).get("copied"),
            "ledger_written": (final_backup.get("ledger") or {}).get("written"),
        },
        "blockers": blockers,
        "artifacts": _existing_artifacts(artifacts),
    }


def _final_backup_python_health_url(args: argparse.Namespace) -> str:
    if args.final_backup_python_health_url:
        return args.final_backup_python_health_url
    config_url = configured_python_health_url(args.config)
    rehearsal_url = args.python_base_url.rstrip("/") + "/health"
    if rehearsal_url != config_url:
        raise ValueError(
            "final backup health URL is ambiguous: "
            f"--python-base-url resolves to {rehearsal_url}, "
            f"but --config resolves to {config_url}; pass "
            "--final-backup-python-health-url explicitly"
        )
    return config_url


def _existing_artifacts(artifacts: dict[str, str]) -> dict[str, str]:
    return {
        name: path
        for name, path in artifacts.items()
        if Path(path).exists()
    }


def _write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def _state_gate_summary(
    reports: dict[str, dict[str, Any] | None],
) -> dict[str, Any]:
    preflight = reports.get("preflight") or {}
    backup = reports.get("backup") or {}
    restore_verify = reports.get("restore_verify") or {}
    restore_execute = reports.get("restore_execute") or {}
    freeze_drain = reports.get("freeze_drain") or {}
    return {
        "preflight_status": preflight.get("status"),
        "preflight_stores": (preflight.get("summary") or {}).get("stores"),
        "preflight_existing": (preflight.get("summary") or {}).get("existing"),
        "backup_status": backup.get("status"),
        "backup_copied": (backup.get("summary") or {}).get("copied"),
        "backup_skipped": (backup.get("summary") or {}).get("skipped"),
        "restore_verify_status": restore_verify.get("status"),
        "restore_verified": (restore_verify.get("summary") or {}).get("verified"),
        "restore_execute_status": restore_execute.get("status"),
        "restore_restored": (restore_execute.get("summary") or {}).get("restored"),
        "freeze_drain_status": freeze_drain.get("status"),
        "freeze_drain_ledger_written": (freeze_drain.get("ledger") or {}).get("written"),
        "writer_families": (freeze_drain.get("summary") or {}).get("writer_families"),
    }


def _state_gate_report_blockers(
    reports: dict[str, dict[str, Any] | None],
) -> list[dict[str, Any]]:
    blockers: dict[tuple[str | None, str, str], dict[str, Any]] = {}
    for substep, report in reports.items():
        if not report:
            continue
        for blocker in report.get("blockers", []):
            store_id = blocker.get("store_id")
            kind = blocker.get("kind", "blocker")
            detail = blocker.get("detail", str(blocker))
            key = (store_id, kind, detail)
            entry = blockers.setdefault(
                key,
                {
                    "substeps": [],
                    "store_id": store_id,
                    "kind": kind,
                    "detail": detail,
                },
            )
            entry["substeps"].append(substep)
    return list(blockers.values())


def _add_state_gate_blockers(report: dict[str, Any], step: dict[str, Any]) -> None:
    for blocker in step.get("blockers", []):
        detail_parts = []
        if blocker.get("substeps"):
            detail_parts.append("/".join(str(substep) for substep in blocker["substeps"]))
        elif blocker.get("substep"):
            detail_parts.append(str(blocker["substep"]))
        if blocker.get("store_id"):
            detail_parts.append(str(blocker["store_id"]))
        if blocker.get("kind"):
            detail_parts.append(str(blocker["kind"]))
        prefix = ": ".join(detail_parts)
        detail = blocker.get("detail", "state gate blocker")
        _add_blocker(
            report,
            "state_ownership_gate",
            f"{prefix} - {detail}" if prefix else str(detail),
        )


def _run_shadow_summary(
    *,
    python_base_url: str,
    rust_base_url: str,
    paths: tuple[str, ...],
    timeout_seconds: float,
    shadow_secret: str | None,
) -> dict[str, Any]:
    started_at = time.perf_counter()
    results = []
    for path in paths:
        python_url = python_base_url.rstrip("/") + path
        try:
            python_status, python_body, _headers = _http_request(
                "GET", python_url, timeout_seconds=timeout_seconds
            )
            envelope = {
                "request": {"method": "GET", "path": path, "query_string": ""},
                "python_response": {
                    "status": python_status,
                    "body_sha256": hashlib.sha256(python_body).hexdigest(),
                },
            }
            headers = {}
            if shadow_secret:
                headers["x-sm-rust-shadow-secret"] = shadow_secret
            rust_status, rust_body, _headers = _http_request(
                "POST",
                rust_base_url.rstrip("/") + "/__shadow/http",
                body=json.dumps(envelope).encode("utf-8"),
                headers=headers,
                timeout_seconds=timeout_seconds,
            )
            if rust_status != 200:
                results.append(
                    {
                        "path": path,
                        "status": "failed",
                        "detail": f"Rust shadow endpoint returned HTTP {rust_status}",
                    }
                )
                continue
            payload = json.loads(rust_body.decode("utf-8"))
            comparison = payload.get("comparison")
            results.append(
                {
                    "path": path,
                    "status": "passed" if comparison in {"match", "status_match"} else "failed",
                    "comparison": comparison,
                    "support_status": payload.get("support_status"),
                    "python_status": payload.get("python_status"),
                    "predicted_status": payload.get("predicted_status"),
                    "body_sha256_match": payload.get("body_sha256_match"),
                    "detail": payload.get("detail"),
                }
            )
        except Exception as exc:  # noqa: BLE001 - report should preserve the concrete error.
            results.append({"path": path, "status": "failed", "detail": str(exc)})
    failed = sum(1 for result in results if result["status"] == "failed")
    return {
        "status": "passed" if failed == 0 else "failed",
        "elapsed_ms": _elapsed_ms(started_at),
        "summary": {"passed": len(results) - failed, "failed": failed},
        "results": results,
    }


def _resolve_shadow_compare_paths(
    *,
    python_base_url: str,
    base_paths: tuple[str, ...],
    timeout_seconds: float,
) -> dict[str, Any]:
    started_at = time.perf_counter()
    paths = list(base_paths)
    detail = "mobile session detail probes skipped: no live session id resolved"
    status = "passed"
    session_id = None
    try:
        response_status, response_body, _headers = _http_request(
            "GET",
            python_base_url.rstrip("/") + "/sessions",
            timeout_seconds=timeout_seconds,
        )
        if response_status != 200:
            status = "skipped"
            detail = f"mobile session detail probes skipped: /sessions returned HTTP {response_status}"
        else:
            session_id = _first_shadow_session_id(response_body)
            if session_id:
                paths.extend(
                    [
                        f"/client/sessions/{session_id}",
                        f"/sessions/{session_id}/attach-descriptor",
                    ]
                )
                detail = f"mobile session detail probes use session_id={session_id}"
            else:
                status = "skipped"
    except Exception as exc:  # noqa: BLE001 - report should preserve the concrete transport/parsing error.
        status = "skipped"
        detail = f"mobile session detail probes skipped: {type(exc).__name__}: {exc}"
    return {
        "name": "shadow_mobile_path_resolution",
        "status": status,
        "elapsed_ms": _elapsed_ms(started_at),
        "detail": detail,
        "session_id": session_id,
        "paths": paths,
    }


def _first_shadow_session_id(response_body: bytes) -> str | None:
    payload = json.loads(response_body.decode("utf-8"))
    sessions = payload.get("sessions") if isinstance(payload, dict) else payload
    if not isinstance(sessions, list):
        return None
    for session in sessions:
        if not isinstance(session, dict):
            continue
        session_id = session.get("id")
        if isinstance(session_id, str) and session_id:
            return session_id
    return None


def _baseline_step(name: str, report: dict[str, Any]) -> dict[str, Any]:
    failed = report["latency"]["failed"]
    return {
        "name": name,
        "status": "passed" if not failed else "failed",
        "elapsed_ms": round(float(report.get("elapsed_seconds", 0.0)) * 1000, 3),
        "detail": "baseline completed" if not failed else f"failed checks: {sorted(failed)}",
        "memory": report.get("memory"),
        "latency": report.get("latency"),
    }


def _run_command(
    command: list[str], *, cwd: Path, timeout_seconds: float
) -> dict[str, Any]:
    started_at = time.perf_counter()
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout_seconds,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        return {
            "status": "failed",
            "elapsed_ms": _elapsed_ms(started_at),
            "detail": f"timed out after {timeout_seconds}s",
            "stdout_tail": _tail_text(exc.stdout or ""),
            "stderr_tail": _tail_text(exc.stderr or ""),
        }
    return {
        "status": "passed" if completed.returncode == 0 else "failed",
        "elapsed_ms": _elapsed_ms(started_at),
        "detail": f"exit {completed.returncode}",
        "stdout_tail": _tail_text(completed.stdout),
        "stderr_tail": _tail_text(completed.stderr),
    }


def _http_request(
    method: str,
    url: str,
    *,
    body: bytes | None = None,
    headers: dict[str, str] | None = None,
    timeout_seconds: float,
) -> tuple[int, bytes, dict[str, str]]:
    request = urllib.request.Request(url, data=body, headers=headers or {}, method=method)
    if body is not None and "Content-Type" not in request.headers:
        request.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(request, timeout=timeout_seconds) as response:
            return response.status, response.read(), dict(response.headers)
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read(), dict(exc.headers)


def _add_contract_blockers(
    report: dict[str, Any],
    step: dict[str, Any],
    *,
    blocker_kind: str,
    include_skipped: bool = False,
) -> None:
    for result in step.get("results", []):
        if result["status"] != "failed" and not (
            include_skipped and result["status"] == "skipped"
        ):
            continue
        _add_blocker(
            report,
            blocker_kind,
            f"{result['id']}: {result['detail']}",
            check_id=result["id"],
        )


def _add_blocker(
    report: dict[str, Any], kind: str, detail: str, *, check_id: str | None = None
) -> None:
    blocker: dict[str, Any] = {"kind": kind, "detail": detail}
    if check_id:
        blocker["check_id"] = check_id
    report["blockers"].append(blocker)


def _terminate_process_group(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            return
        process.wait(timeout=5)


def _host_from_base_url(base_url: str) -> str:
    parsed = urlparse(base_url)
    return parsed.hostname or "127.0.0.1"


def _port_from_base_url(base_url: str) -> int:
    parsed = urlparse(base_url)
    if parsed.port:
        return parsed.port
    return 443 if parsed.scheme == "https" else 80


def _mutating_rust_base_url(args: argparse.Namespace) -> str:
    if args.mutating_rust_base_url:
        return args.mutating_rust_base_url.rstrip("/")
    return _default_mutating_rust_base_url(args.rust_base_url)


def _default_mutating_rust_base_url(base_url: str) -> str:
    return _default_offset_rust_base_url(base_url, offset=1)


def _read_only_fixture_rust_base_url(args: argparse.Namespace) -> str:
    if args.read_only_fixture_rust_base_url:
        return args.read_only_fixture_rust_base_url.rstrip("/")
    return _default_read_only_fixture_rust_base_url(args.rust_base_url)


def _default_read_only_fixture_rust_base_url(base_url: str) -> str:
    return _default_offset_rust_base_url(base_url, offset=2)


def _default_offset_rust_base_url(base_url: str, *, offset: int) -> str:
    parsed = urlparse(base_url)
    scheme = parsed.scheme or "http"
    host = parsed.hostname or "127.0.0.1"
    if ":" in host and not host.startswith("["):
        host = f"[{host}]"
    return f"{scheme}://{host}:{_port_from_base_url(base_url) + offset}"


def _tail_text(value: str | bytes, max_chars: int = 4000) -> str:
    if isinstance(value, bytes):
        value = value.decode("utf-8", errors="replace")
    return value[-max_chars:]


def _tail_file(path: Path, max_chars: int = 4000) -> str:
    try:
        return path.read_text(errors="replace")[-max_chars:]
    except FileNotFoundError:
        return ""


def _elapsed_ms(started_at: float) -> float:
    return round((time.perf_counter() - started_at) * 1000, 3)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python-base-url", default=PYTHON_BASE_URL)
    parser.add_argument("--rust-base-url", default=RUST_BASE_URL)
    parser.add_argument(
        "--mutating-rust-base-url",
        default=None,
        help="Fresh Rust sidecar URL for disposable mutating fixture checks; defaults to rust port + 1",
    )
    parser.add_argument(
        "--read-only-fixture-rust-base-url",
        default=None,
        help="Fresh Rust sidecar URL for synthetic read-only fixture checks; defaults to rust port + 2",
    )
    parser.add_argument(
        "--read-only-fixture-config",
        type=Path,
        default=DEFAULT_READ_ONLY_FIXTURE_CONFIG,
        help="Rust config for the synthetic read-only fixture sidecar",
    )
    parser.add_argument("--config", type=Path, default=Path("config.yaml"))
    parser.add_argument("--local-env", type=Path, default=None)
    parser.add_argument("--output-dir", type=Path, default=None)
    parser.add_argument("--reuse-rust-sidecar", action="store_true")
    parser.add_argument("--skip-python-health", action="store_true")
    parser.add_argument("--skip-state-gate", action="store_true")
    parser.add_argument(
        "--run-final-backup-gate",
        action="store_true",
        help="After state preflight/backup/restore/freeze-drain, require stopped Python evidence and create the final backup",
    )
    parser.add_argument(
        "--final-backup-python-health-url",
        default=None,
        help="Override the Python health URL used by the stopped-origin final backup gate",
    )
    parser.add_argument(
        "--final-backup-health-timeout",
        type=float,
        default=1.0,
        help="Per-probe timeout for stopped-origin final backup health checks",
    )
    parser.add_argument(
        "--final-backup-stopped-hold-seconds",
        type=float,
        default=5.0,
        help="Hold window for durable stopped-origin final backup evidence",
    )
    parser.add_argument(
        "--final-backup-stopped-probe-count",
        type=_positive_int,
        default=2,
        help="Number of refused probes required for stopped-origin final backup evidence",
    )
    parser.add_argument("--skip-smoke", action="store_true")
    parser.add_argument("--skip-baseline", action="store_true")
    parser.add_argument("--skip-shadow", action="store_true")
    parser.add_argument("--skip-read-only-fixture-contracts", action="store_true")
    parser.add_argument("--skip-mutating-contracts", action="store_true")
    parser.add_argument("--core-only", action="store_true")
    parser.add_argument("--allow-blockers", action="store_true")
    parser.add_argument("--shadow-secret", default=None)
    parser.add_argument("--baseline-repetitions", type=_positive_int, default=3)
    parser.add_argument("--timeout", type=float, default=5.0)
    parser.add_argument("--startup-timeout", type=float, default=60.0)
    parser.add_argument("--smoke-timeout", type=float, default=120.0)
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--sm-binary", default=DEFAULT_RUST_SM_BINARY)
    return parser


def _positive_int(raw: str) -> int:
    value = int(raw)
    if value <= 0:
        raise argparse.ArgumentTypeError("must be greater than 0")
    return value


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    report = run_rehearsal(args)
    print(json.dumps(report, indent=2, sort_keys=True))
    if report["blockers"] and not args.allow_blockers:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
