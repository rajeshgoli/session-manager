import json
import sqlite3
import subprocess
import urllib.error
from pathlib import Path
from unittest.mock import patch

from scripts.rust_migration.baseline import (
    _parse_memory_to_mib,
    _port_from_base_url,
    _resolve_base_url,
    run_baseline,
)
from scripts.rust_migration.contracts import (
    ContractCheck,
    ContractManifest,
    JsonExpectation,
    _build_parser as _build_contracts_parser,
    _json_expectation_error,
    _parse_fixtures,
    _resolve_sm_binary,
    _run_cli_check,
    _run_http_check,
    _render_template,
    checks_for_target,
    run_checks,
    summarize,
)
from scripts.rust_migration.mutating_fixture import (
    DEFAULT_CHILD_SESSION_ID,
    DEFAULT_CLI_RESTORE_SESSION_ID,
    DEFAULT_EM_SESSION_ID,
    DEFAULT_NOTIFY_CHILD_SESSION_ID,
    DEFAULT_SESSION_ID,
    DEFAULT_STOPPED_SESSION_ID,
    create_mutating_fixture_workspace,
)
from scripts.rust_migration.mvp_rehearsal import (
    CORE_MUTATING_CHECK_IDS,
    DEFAULT_RUST_SM_BINARY,
    READ_ONLY_FIXTURE_CHECK_IDS,
    READ_ONLY_FIXTURE_VALUES,
    _build_parser,
    _default_read_only_fixture_rust_base_url,
    _default_mutating_rust_base_url,
    _ensure_rust_cli_available,
    _resolve_shadow_compare_paths,
    _run_contract_group,
    _run_state_gate,
    run_rehearsal,
)


REPO_ROOT = Path(__file__).resolve().parents[2]
STAGE5_DIR = REPO_ROOT / "specs" / "762_stage5_artifacts"


def test_manifest_preserves_mobile_kill_route_while_retiring_cli_alias():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}

    http_stop = checks["http.mobile_session_stop"]
    assert http_stop.classification == "retained"
    assert http_stop.target == "python_and_rust"
    assert http_stop.path == "/sessions/{session_id}/kill"
    assert "mutating_opt_in" in http_stop.preconditions

    cli_kill = checks["cli.kill_alias_retired"]
    assert cli_kill.classification == "retired"
    assert cli_kill.target == "rust_only"
    assert cli_kill.command == ("kill", "--help")


def test_manifest_retains_local_and_durable_codex_review_cli_surfaces():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}

    review = checks["cli.review_help"]
    assert review.classification == "retained"
    assert review.target == "python_and_rust"
    assert review.command == ("review", "--help")
    assert review.expected_output_contains_all == (
        "--base",
        "--uncommitted",
        "--commit",
        "--custom",
        "--new",
        "--pr",
    )

    durable = checks["cli.request_codex_review_help"]
    assert durable.classification == "retained"
    assert durable.target == "python_and_rust"
    assert durable.command == ("request-codex-review", "--help")
    assert "--notify" in durable.expected_output_contains_all
    assert "--steer" in durable.expected_output_contains_all
    assert "--poll-interval" in durable.expected_output_contains_all
    assert "--retry-interval" in durable.expected_output_contains_all

    for check_id, subcommand in {
        "cli.request_codex_review_list_help": "list",
        "cli.request_codex_review_status_help": "status",
        "cli.request_codex_review_cancel_help": "cancel",
    }.items():
        check = checks[check_id]
        assert check.classification == "retained"
        assert check.target == "python_and_rust"
        assert check.command == ("request-codex-review", subcommand, "--help")


def test_manifest_covers_core_retained_cli_help_surfaces():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}
    required_ids = {
        "cli.me_help",
        "cli.who_help",
        "cli.status_help",
        "cli.all_help",
        "cli.watch_help",
        "cli.send_help",
        "cli.email_help",
        "cli.wait_help",
        "cli.spawn_help",
        "cli.fork_help",
        "cli.new_help",
        "cli.children_help",
        "cli.retire_help",
        "cli.restore_help",
        "cli.attach_help",
        "cli.output_help",
        "cli.tail_raw_help",
        "cli.clear_help",
        "cli.handoff_help",
        "cli.context_monitor_help",
        "cli.maintainer_help",
        "cli.register_help",
        "cli.unregister_help",
        "cli.lookup_help",
        "cli.roster_help",
        "cli.queue_run_help",
        "cli.queue_list_help",
        "cli.queue_status_help",
        "cli.queue_cancel_help",
        "cli.review_help",
        "cli.request_codex_review_help",
        "cli.request_codex_review_list_help",
        "cli.request_codex_review_status_help",
        "cli.request_codex_review_cancel_help",
        "cli.claude_help",
        "cli.codex_help",
        "cli.codex_app_help",
        "cli.codex_fork_help",
        "cli.codex_2_help",
    }

    missing = required_ids - set(checks)
    assert not missing
    for check_id in required_ids:
        check = checks[check_id]
        assert check.classification == "retained"
        assert check.target == "python_and_rust"
        assert check.command[-1] == "--help"
        assert check.expected_exit == (0,)


def test_manifest_covers_rust_only_retired_cli_surfaces():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}
    retired_ids = {
        "cli.what_retired",
        "cli.kill_alias_retired",
        "cli.dispatch_retired",
        "cli.remind_retired",
        "cli.watch_job_retired",
        "cli.queue_policy_retired",
        "cli.queue_policy_status_retired",
        "cli.queue_policy_history_retired",
        "cli.telegram_retired",
        "cli.telegram_alias_retired",
        "cli.codex_legacy_retired",
        "cli.codex_server_retired",
    }

    missing = retired_ids - set(checks)
    assert not missing
    for check_id in retired_ids:
        check = checks[check_id]
        assert check.classification == "retired"
        assert check.target == "rust_only"
        assert check.command[-1] == "--help"
        assert "removed" in check.expected_output_contains_any


def test_manifest_covers_rust_only_mobile_device_cli_surfaces():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}
    required_ids = {
        "cli.list_devices_help",
        "cli.remove_device_help",
    }

    missing = required_ids - set(checks)
    assert not missing

    list_devices = checks["cli.list_devices_help"]
    assert list_devices.classification == "retained"
    assert list_devices.target == "rust_only"
    assert list_devices.command == ("list-devices", "--help")
    assert list_devices.expected_exit == (0,)
    assert "--json" in list_devices.expected_output_contains_all

    remove_device = checks["cli.remove_device_help"]
    assert remove_device.classification == "retained"
    assert remove_device.target == "rust_only"
    assert remove_device.command == ("remove-device", "--help")
    assert remove_device.expected_exit == (0,)
    assert "DEVICE_ID" in remove_device.expected_output_contains_all
    assert "--user-id" in remove_device.expected_output_contains_all


def test_manifest_links_cloudflare_access_cutover_evidence():
    manifest = ContractManifest.load()

    assert "specs/945_cloudflare_access_auth_model.md" in manifest.artifacts
    assert (
        "specs/762_stage5_artifacts/cloudflare_access_cutover_evidence.md"
        in manifest.artifacts
    )


def test_cloudflare_access_cutover_evidence_pins_policy_and_origin_gates():
    evidence = (STAGE5_DIR / "cloudflare_access_cutover_evidence.md").read_text(
        encoding="utf-8"
    )
    index = (STAGE5_DIR / "index.md").read_text(encoding="utf-8")

    assert "cloudflare_access_cutover_evidence.md" in index
    for required in [
        "PR #950",
        "sm-browser",
        "sm-mobile-app",
        "sm-node-fallback",
        "sm-email-worker",
        "No broad Valid Certificate policy",
        "same mobile user resolved from the SM session or bearer actor",
        "revoked-device denial",
        "Native app smoke",
    ]:
        assert required in evidence


def test_handoff_and_progress_are_current_through_review_route_pr984():
    progress = (STAGE5_DIR / "mvp_progress.md").read_text(encoding="utf-8")
    handoff = (STAGE5_DIR / "resume_handoff.md").read_text(encoding="utf-8")

    for text in [progress, handoff]:
        assert "#950" in text
        assert "#984" in text
        assert "Cloudflare Access" in text
        assert "cloudflare_access_cutover_evidence.md" in text

    assert "Latest merged commit before this docs refresh: `8c261ee`" in handoff
    assert "records the PR lineage through #984" in handoff


def test_manifest_covers_rust_only_retired_http_surfaces():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}
    retired_ids = {
        "http.public_watch_operational_data_denied",
        "http.summary_provider_route_retired",
        "http.scheduler_remind_retired",
        "http.job_watches_retired",
        "http.queue_policy_runs_retired",
    }

    missing = retired_ids - set(checks)
    assert not missing
    for check_id in retired_ids:
        check = checks[check_id]
        assert check.classification == "retired"
        assert check.target == "rust_only"
        assert 404 in check.expected_status
    assert "session_id" in checks["http.summary_provider_route_retired"].preconditions


def test_manifest_covers_rust_core_fixture_lifecycle_cli_checks():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}
    required_ids = {
        "cli.rust_core_spawn_fixture",
        "cli.rust_core_spawn_child_inherits_parent_fixture",
        "cli.rust_core_me_fixture",
        "cli.rust_core_all_fixture",
        "cli.rust_core_children_fixture",
        "cli.rust_core_children_target_fixture",
        "cli.rust_core_context_monitor_enable_child_fixture",
        "cli.rust_core_context_monitor_status_fixture",
        "cli.rust_core_status_fixture",
        "cli.rust_core_send_fixture",
        "cli.rust_core_send_urgent_fixture",
        "cli.rust_core_send_wait_fixture",
        "cli.rust_core_output_fixture",
        "cli.rust_core_clear_child_fixture",
        "cli.rust_core_tail_fixture",
        "cli.rust_core_retire_fixture",
        "cli.rust_core_restore_fixture",
        "cli.rust_core_restore_node_primary_fixture",
        "cli.rust_core_retire_restored_fixture",
        "cli.rust_core_wait_retired_fixture",
    }

    missing = required_ids - set(checks)
    assert not missing
    for check_id in required_ids:
        check = checks[check_id]
        assert check.classification == "retained"
        assert check.target == "rust_only"
        assert check.safety == "mutating"
        assert "mutating_opt_in" in check.preconditions
        assert "fixture:base_url" in check.preconditions


def test_manifest_uses_dedicated_notify_child_for_stop_notification_fixture():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}

    notify = checks["http.rust_core_notify_on_stop_fixture"]
    assert notify.path == "/sessions/{notify_child_session_id}/notify-on-stop"
    assert "fixture:notify_child_session_id" in notify.preconditions
    assert "fixture:child_session_id" not in notify.preconditions
    expectations = {expectation.path: expectation for expectation in notify.expected_json}
    assert expectations["/session_id"].equals == "{notify_child_session_id}"


def test_manifest_covers_rust_core_review_fixture_checks():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}

    existing = checks["http.rust_core_review_existing_fixture"]
    assert existing.classification == "retained"
    assert existing.target == "rust_only"
    assert existing.safety == "mutating"
    assert existing.method == "POST"
    assert existing.path == "/sessions/{session_id}/review"
    assert existing.body["mode"] == "custom"
    assert "session_id" in existing.preconditions
    assert "mutating_opt_in" in existing.preconditions
    expectations = {expectation.path: expectation for expectation in existing.expected_json}
    assert expectations["/error"].equals.startswith("Review requires a Codex session")

    spawned = checks["http.rust_core_spawn_review_fixture"]
    assert spawned.classification == "retained"
    assert spawned.target == "rust_only"
    assert spawned.safety == "mutating"
    assert spawned.method == "POST"
    assert spawned.path == "/sessions/review"
    assert spawned.body["parent_session_id"] == "{session_id}"
    assert spawned.body["mode"] == "custom"
    assert "session_id" in spawned.preconditions
    assert "mutating_opt_in" in spawned.preconditions
    expectations = {expectation.path: expectation for expectation in spawned.expected_json}
    assert expectations["/error"].equals == "Failed to send review sequence to tmux"


def test_manifest_covers_rust_queue_writer_fixture_checks():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}

    cli_check = checks["cli.rust_queue_run_fixture"]
    assert cli_check.classification == "retained"
    assert cli_check.target == "rust_only"
    assert cli_check.safety == "mutating"
    assert "mutating_opt_in" in cli_check.preconditions
    assert "fixture:working_dir" in cli_check.preconditions
    assert cli_check.command[:4] == ("--api-url", "{base_url}", "queue", "run")

    cancel_check = checks["cli.rust_queue_cancel_fixture"]
    assert cancel_check.classification == "retained"
    assert cancel_check.target == "rust_only"
    assert cancel_check.safety == "mutating"
    assert "mutating_opt_in" in cancel_check.preconditions
    assert "fixture:queue_job_id" in cancel_check.preconditions
    assert cancel_check.command[:4] == ("--api-url", "{base_url}", "queue", "cancel")

    http_check = checks["http.rust_queue_job_create_fixture"]
    assert http_check.classification == "retained"
    assert http_check.target == "rust_only"
    assert http_check.safety == "mutating"
    assert http_check.method == "POST"
    assert http_check.path == "/queue-jobs"
    assert http_check.body["argv"] == ["echo", "queue http fixture"]
    assert "mutating_opt_in" in http_check.preconditions


def test_manifest_covers_rust_node_restore_fixture_check():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}

    restore_check = checks["http.rust_node_primary_restore_fixture"]
    assert restore_check.classification == "retained"
    assert restore_check.target == "rust_only"
    assert restore_check.safety == "mutating"
    assert restore_check.method == "POST"
    assert (
        restore_check.path
        == "/nodes/primary/restore-candidates/{stopped_session_id}/restore"
    )
    assert "fixture:stopped_session_id" in restore_check.preconditions
    assert "mutating_opt_in" in restore_check.preconditions
    expectations = {
        expectation.path: expectation for expectation in restore_check.expected_json
    }
    assert expectations["/id"].equals == "{stopped_session_id}"
    assert expectations["/status"].equals == "running"
    assert expectations["/stopped_at"].value_type == "null"


def test_manifest_covers_rust_cli_restore_node_fixture_check():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}

    restore_check = checks["cli.rust_core_restore_node_primary_fixture"]
    assert restore_check.classification == "retained"
    assert restore_check.target == "rust_only"
    assert restore_check.safety == "mutating"
    assert restore_check.command == (
        "--api-url",
        "{base_url}",
        "restore",
        "{cli_restore_session_id}",
        "--node",
        "primary",
    )
    assert "fixture:cli_restore_session_id" in restore_check.preconditions
    assert "mutating_opt_in" in restore_check.preconditions


def test_mvp_rehearsal_mutating_check_ids_match_manifest():
    manifest = ContractManifest.load()
    expected = {
        check.id
        for check in manifest.checks
        if check.target == "rust_only" and check.safety == "mutating"
    }

    assert set(CORE_MUTATING_CHECK_IDS) == expected


def test_mvp_rehearsal_read_only_fixture_check_ids_are_retained_reads():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}
    expected = {
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
    }

    assert set(READ_ONLY_FIXTURE_CHECK_IDS) == expected
    for check_id in READ_ONLY_FIXTURE_CHECK_IDS:
        check = checks[check_id]
        assert check.classification == "retained"
        if check.surface == "cli":
            assert check.target == "rust_only"
        else:
            assert check.target == "python_and_rust"
        assert check.safety == "read_only"
        if check.surface == "http":
            assert check.method == "GET"


def test_mvp_rehearsal_read_only_fixture_values_cover_preconditions():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}

    for check_id in READ_ONLY_FIXTURE_CHECK_IDS:
        check = checks[check_id]
        for precondition in check.preconditions:
            if precondition == "session_id":
                assert READ_ONLY_FIXTURE_VALUES["session_id"] == "fixture001"
            if precondition.startswith("fixture:"):
                fixture_name = precondition.split(":", 1)[1]
                if fixture_name == "base_url":
                    continue
                assert READ_ONLY_FIXTURE_VALUES[fixture_name]


def test_manifest_covers_rust_queue_cli_read_fixtures():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}
    required_ids = {
        "cli.rust_queue_list_fixture",
        "cli.rust_queue_list_json_fixture",
        "cli.rust_queue_status_fixture",
        "cli.rust_queue_status_json_fixture",
    }

    missing = required_ids - set(checks)
    assert not missing
    for check_id in required_ids:
        check = checks[check_id]
        assert check.classification == "retained"
        assert check.target == "rust_only"
        assert check.safety == "read_only"
        assert check.surface == "cli"
        assert "fixture:queue_job_id" in check.preconditions
        assert "fixture:base_url" in check.preconditions


def test_mvp_rehearsal_defaults_mutating_sidecar_to_next_port():
    assert (
        _default_mutating_rust_base_url("http://127.0.0.1:8421")
        == "http://127.0.0.1:8422"
    )
    assert (
        _default_mutating_rust_base_url("https://sm.example.test")
        == "https://sm.example.test:444"
    )


def test_mvp_rehearsal_defaults_read_only_fixture_sidecar_to_second_next_port():
    assert (
        _default_read_only_fixture_rust_base_url("http://127.0.0.1:8421")
        == "http://127.0.0.1:8423"
    )
    assert (
        _default_read_only_fixture_rust_base_url("https://sm.example.test")
        == "https://sm.example.test:445"
    )


def test_mvp_rehearsal_defaults_to_rust_cli_binary():
    args = _build_parser().parse_args([])

    assert args.sm_binary == DEFAULT_RUST_SM_BINARY


def test_contract_harness_resolves_target_specific_cli_defaults():
    args = _build_contracts_parser().parse_args([])

    assert args.sm_binary is None
    assert _resolve_sm_binary("python", args.sm_binary) == "sm"
    assert _resolve_sm_binary("rust", args.sm_binary) == "target/debug/sm"
    assert _resolve_sm_binary("rust", "/tmp/custom-sm") == "/tmp/custom-sm"


def test_mvp_rehearsal_does_not_build_custom_cli_binary():
    args = _build_parser().parse_args(["--sm-binary", "/tmp/custom-sm"])

    assert _ensure_rust_cli_available(args) is None


def test_mvp_rehearsal_mutating_group_fails_on_skipped_checks():
    manifest = ContractManifest(
        schema_version=1,
        source_spec="test",
        artifacts=(),
        checks=(
            ContractCheck(
                id="cli.fixture_missing_cli",
                surface="cli",
                classification="retained",
                target="rust_only",
                safety="mutating",
                command=("noop",),
                expected_exit=(0,),
                preconditions=("sm_cli", "mutating_opt_in"),
                source="test",
            ),
        ),
    )

    step = _run_contract_group(
        manifest,
        name="mutating",
        target="rust",
        base_url="http://127.0.0.1:8422",
        sm_binary="/definitely/not/a/session-manager-cli",
        check_ids={"cli.fixture_missing_cli"},
        timeout_seconds=0.1,
        include_mutating=True,
        fail_on_skipped=True,
    )

    assert step["status"] == "failed"
    assert step["summary"]["skipped"] == 1


def test_mvp_rehearsal_records_read_only_fixture_skip_step(tmp_path, monkeypatch):
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(tmp_path / "missing-client.yaml"))
    args = _build_parser().parse_args(
        [
            "--skip-python-health",
            "--skip-state-gate",
            "--skip-smoke",
            "--skip-baseline",
            "--skip-shadow",
            "--skip-read-only-fixture-contracts",
            "--skip-mutating-contracts",
            "--reuse-rust-sidecar",
            "--output-dir",
            str(tmp_path / "rehearsal"),
        ]
    )

    with patch("scripts.rust_migration.mvp_rehearsal._probe_health") as probe:
        probe.return_value = {
            "status": "passed",
            "elapsed_ms": 1.0,
            "detail": "mock healthy",
        }
        with patch("scripts.rust_migration.mvp_rehearsal._run_contract_group") as run_group:
            run_group.return_value = {
                "name": "rust_core_sidecar_contracts",
                "status": "passed",
                "elapsed_ms": 1.0,
                "summary": {"passed": 1, "failed": 0, "skipped": 0},
                "results": [],
            }
            report = run_rehearsal(args)

    step = next(
        step
        for step in report["steps"]
        if step["name"] == "rust_read_only_fixture_contracts"
    )
    assert step["status"] == "skipped"
    assert report["summary"]["status"] == "passed"


def test_mvp_rehearsal_resolves_mobile_shadow_paths(monkeypatch):
    def fake_http_request(method, url, *, timeout_seconds, **_kwargs):
        assert method == "GET"
        assert url == "http://python.test/sessions"
        assert timeout_seconds == 0.5
        return (
            200,
            json.dumps({"sessions": [{"id": "mobile-session"}]}).encode(),
            {},
        )

    monkeypatch.setattr(
        "scripts.rust_migration.mvp_rehearsal._http_request",
        fake_http_request,
    )

    step = _resolve_shadow_compare_paths(
        python_base_url="http://python.test",
        base_paths=("/health", "/client/analytics/summary"),
        timeout_seconds=0.5,
    )

    assert step["status"] == "passed"
    assert step["session_id"] == "mobile-session"
    assert step["paths"] == [
        "/health",
        "/client/analytics/summary",
        "/client/sessions/mobile-session",
        "/sessions/mobile-session/attach-descriptor",
    ]


def test_mvp_rehearsal_mobile_shadow_paths_skip_without_session(monkeypatch):
    def fake_http_request(method, url, *, timeout_seconds, **_kwargs):
        return 200, b'{"sessions":[]}', {}

    monkeypatch.setattr(
        "scripts.rust_migration.mvp_rehearsal._http_request",
        fake_http_request,
    )

    step = _resolve_shadow_compare_paths(
        python_base_url="http://python.test",
        base_paths=("/health",),
        timeout_seconds=0.5,
    )

    assert step["status"] == "skipped"
    assert step["session_id"] is None
    assert step["paths"] == ["/health"]


def test_mvp_rehearsal_state_gate_copies_restores_and_records_plan(
    tmp_path, monkeypatch
):
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(tmp_path / "missing-client.yaml"))
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    (state_dir / "sessions.json").write_text("[]\n", encoding="utf-8")
    (state_dir / "message_queue.db").write_text("queue\n", encoding="utf-8")
    (state_dir / "logs").mkdir()
    (state_dir / "logs/server.log").write_text("log\n", encoding="utf-8")
    config = tmp_path / "config.yaml"
    config.write_text(
        f"""
paths:
  state_file: "{state_dir / 'sessions.json'}"
  log_dir: "{state_dir / 'logs'}"
  server_log_file: "{state_dir / 'server.log'}"
  app_artifacts_dir: "{state_dir / 'apps'}"
  bug_reports_db: "{state_dir / 'bug_reports.db'}"
sm_send:
  db_path: "{state_dir / 'message_queue.db'}"
response_relay:
  db_path: "{state_dir / 'response_relay.db'}"
tool_logging:
  db_path: "{state_dir / 'tool_usage.db'}"
telegram:
  topic_registry:
    path: "{state_dir / 'telegram_topics.json'}"
email:
  bridge_config: "{state_dir / 'email_send.yaml'}"
queue_runner:
  state_dir: "{state_dir / 'queue-runner'}"
""".strip()
        + "\n",
        encoding="utf-8",
    )
    args = _build_parser().parse_args(["--config", str(config)])
    output_dir = tmp_path / "rehearsal"

    step = _run_state_gate(args, output_dir)

    assert step["status"] == "passed"
    assert step["summary"]["preflight_status"] == "passed"
    assert step["summary"]["backup_status"] == "copied"
    assert step["summary"]["restore_verify_status"] == "verified"
    assert step["summary"]["restore_execute_status"] == "restored"
    assert step["summary"]["freeze_drain_status"] == "planned"
    assert step["summary"]["freeze_drain_ledger_written"] is True
    for path in step["artifacts"].values():
        if path.endswith("restore"):
            assert Path(path).is_dir()
        else:
            assert Path(path).exists()
    manifest = json.loads(
        Path(step["artifacts"]["backup_manifest"]).read_text(encoding="utf-8")
    )
    assert manifest["status"] == "copied"
    restore_report = json.loads(
        Path(step["artifacts"]["restore_report"]).read_text(encoding="utf-8")
    )
    assert restore_report["status"] == "restored"
    assert (
        Path(step["artifacts"]["restore_root"]) / "stores/sessions_state"
    ).read_text(encoding="utf-8") == "[]\n"
    ledger_rows = Path(step["artifacts"]["freeze_drain_ledger"]).read_text(
        encoding="utf-8"
    ).splitlines()
    assert len(ledger_rows) == 1
    assert json.loads(ledger_rows[0])["kind"] == "freeze_drain_plan"


def test_mvp_rehearsal_stops_after_state_gate_blocker(tmp_path, monkeypatch):
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(tmp_path / "missing-client.yaml"))
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    config = tmp_path / "config.yaml"
    config.write_text(
        f"""
paths:
  state_file: "{state_dir / 'sessions.json'}"
  log_dir: "{state_dir / 'logs'}"
sm_send:
  db_path: "{state_dir / 'message_queue.db'}"
""".strip()
        + "\n",
        encoding="utf-8",
    )
    args = _build_parser().parse_args(
        [
            "--skip-python-health",
            "--config",
            str(config),
            "--output-dir",
            str(tmp_path / "rehearsal"),
        ]
    )

    report = run_rehearsal(args)

    assert report["summary"]["status"] == "blocked"
    state_blockers = [
        blocker
        for blocker in report["blockers"]
        if blocker["kind"] == "state_ownership_gate"
    ]
    assert len(state_blockers) == 1
    assert "preflight/backup/freeze_drain" in state_blockers[0]["detail"]
    assert "sessions_state" in state_blockers[0]["detail"]
    step_names = [step["name"] for step in report["steps"]]
    assert step_names == ["state_ownership_backup_restore_gate"]
    state_step = report["steps"][0]
    assert len(state_step["blockers"]) == 1
    assert state_step["blockers"][0]["substeps"] == [
        "preflight",
        "backup",
        "freeze_drain",
    ]
    for name, path in report["artifacts"].items():
        if name.startswith("state_gate_"):
            assert Path(path).exists()
    assert "state_gate_backup_manifest" not in report["artifacts"]
    assert "state_gate_restore_verify_report" not in report["artifacts"]
    assert "state_gate_restore_report" not in report["artifacts"]
    assert "state_gate_restore_root" not in report["artifacts"]
    assert "state_gate_freeze_drain_ledger" not in report["artifacts"]


def test_manifest_has_json_shape_assertions_for_core_http_surfaces():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}

    assert checks["http.health"].expected_json
    assert checks["http.health"].expected_json[0].path == "/status"
    assert checks["http.health"].expected_json[0].equals == "healthy"
    assert checks["http.client_bootstrap"].expected_json
    assert any(
        expectation.path == "/auth/device_auth_endpoint"
        and expectation.equals == "/auth/device/google"
        for expectation in checks["http.client_bootstrap"].expected_json
    )
    assert checks["http.session_detail_fixture"].expected_json
    assert any(
        expectation.path == "/id" and expectation.equals == "{session_id}"
        for expectation in checks["http.session_detail_fixture"].expected_json
    )
    assert checks["http.client_session_detail_fixture"].expected_json
    client_session_expectations = {
        expectation.path: expectation
        for expectation in checks["http.client_session_detail_fixture"].expected_json
    }
    assert (
        client_session_expectations["/attach_descriptor/attach_supported"].value_type
        == "boolean"
    )
    assert client_session_expectations["/termux_attach"].value_type == "null"
    assert (
        client_session_expectations["/mobile_terminal/supported"].equals is False
    )
    assert checks["http.session_output_fixture"].expected_json
    assert any(
        expectation.path == "/tmux_client_event_version" and expectation.value_type == "number"
        for expectation in checks["http.events_state"].expected_json
    )


def test_manifest_covers_retained_codex_read_http_surfaces():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}
    required_ids = {
        "http.session_tool_calls",
        "http.codex_events",
        "http.codex_activity_actions",
        "http.codex_pending_requests",
    }

    missing = required_ids - set(checks)
    assert not missing
    for check_id in required_ids:
        check = checks[check_id]
        assert check.classification == "retained"
        assert check.target == "python_and_rust"
        assert check.safety == "read_only"
        assert check.method == "GET"

    assert "fixture:codex_app_session_id" in checks["http.codex_events"].preconditions
    assert "fixture:codex_app_session_id" in checks["http.codex_activity_actions"].preconditions
    assert "fixture:codex_app_session_id" in checks["http.codex_pending_requests"].preconditions
    assert any(
        expectation.path == "/events" and expectation.value_type == "array"
        for expectation in checks["http.codex_events"].expected_json
    )
    assert any(
        expectation.path == "/actions" and expectation.value_type == "array"
        for expectation in checks["http.codex_activity_actions"].expected_json
    )


def test_read_only_fixture_provides_codex_app_session_for_retained_reads():
    fixture_path = Path("scripts/rust_migration/fixtures/read_only/sessions.json")
    fixture = json.loads(fixture_path.read_text())
    sessions = {session["id"]: session for session in fixture["sessions"]}

    codex_fixture = sessions["fixture-codex"]
    assert codex_fixture["provider"] == "codex-app"
    assert codex_fixture["status"] == "running"
    assert codex_fixture["tmux_session"] == ""


def test_manifest_covers_implemented_detail_http_surfaces():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}
    required_ids = {
        "http.queue_job_detail",
        "http.codex_review_request_detail",
    }

    missing = required_ids - set(checks)
    assert not missing
    for check_id in required_ids:
        check = checks[check_id]
        assert check.classification == "retained"
        assert check.target == "python_and_rust"
        assert check.safety == "read_only"
        assert check.method == "GET"
        assert check.expected_status == (200,)

    queue_job = checks["http.queue_job_detail"]
    assert queue_job.path == "/queue-jobs/{queue_job_id}"
    assert "fixture:queue_job_id" in queue_job.preconditions
    assert any(
        expectation.path == "/id" and expectation.equals == "{queue_job_id}"
        for expectation in queue_job.expected_json
    )

    codex_review = checks["http.codex_review_request_detail"]
    assert codex_review.path == "/codex-review-requests/{codex_review_request_id}"
    assert "fixture:codex_review_request_id" in codex_review.preconditions
    assert any(
        expectation.path == "/id" and expectation.equals == "{codex_review_request_id}"
        for expectation in codex_review.expected_json
    )


def test_manifest_covers_native_mobile_support_http_surfaces():
    manifest = ContractManifest.load()
    checks = {check.id: check for check in manifest.checks}

    request_status = checks["http.client_request_status"]
    assert request_status.classification == "retained"
    assert request_status.target == "python_and_rust"
    assert request_status.safety == "mutating"
    assert request_status.path == "/client/request-status"
    assert "mutating_opt_in" in request_status.preconditions

    bug_report = checks["http.client_bug_report"]
    assert bug_report.classification == "retained"
    assert bug_report.target == "python_and_rust"
    assert bug_report.safety == "mutating"
    assert bug_report.path == "/client/bug-reports"
    assert "mutating_opt_in" in bug_report.preconditions

    app_metadata = checks["http.app_artifact_metadata"]
    assert app_metadata.classification == "retained"
    assert app_metadata.target == "python_and_rust"
    assert app_metadata.path == "/apps/{app_name}/meta.json"
    assert "fixture:app_name" in app_metadata.preconditions


def test_read_only_fixture_provides_app_artifact_metadata():
    config_path = Path("scripts/rust_migration/fixtures/read_only/config.yaml")
    metadata_path = Path(
        "scripts/rust_migration/fixtures/read_only/apps/session-manager-android/meta.json"
    )

    assert "app_artifacts_dir: \"scripts/rust_migration/fixtures/read_only/apps\"" in (
        config_path.read_text()
    )
    metadata = json.loads(metadata_path.read_text())
    assert metadata["artifact_hash"] == "deadbeef"
    assert metadata["size_bytes"] == 9
    assert metadata["uploaded_by"] == "fixture@example.com"


def test_read_only_fixture_provides_queue_job_detail_row():
    db_path = Path(
        "scripts/rust_migration/fixtures/read_only/queue-runner/queue_runner.db"
    )
    assert db_path.exists()

    with sqlite3.connect(db_path) as conn:
        row = conn.execute(
            "SELECT id, type, label, state FROM queue_jobs WHERE id = ?",
            ("job-fixture",),
        ).fetchone()

    assert row == ("job-fixture", "tests", "fixture queue job", "pending")


def test_mutating_fixture_workspace_creates_disposable_config_and_seed(tmp_path):
    workspace = create_mutating_fixture_workspace(tmp_path)

    assert workspace.root == tmp_path.resolve()
    assert workspace.config_path.exists()
    assert workspace.state_file.exists()
    assert workspace.log_dir.exists()
    assert Path(workspace.fixtures["working_dir"]).is_dir()
    assert workspace.fixtures["session_id"] == DEFAULT_SESSION_ID
    assert workspace.fixtures["child_session_id"] == DEFAULT_CHILD_SESSION_ID
    assert workspace.fixtures["em_session_id"] == DEFAULT_EM_SESSION_ID
    assert workspace.fixtures["notify_child_session_id"] == DEFAULT_NOTIFY_CHILD_SESSION_ID
    assert workspace.fixtures["stopped_session_id"] == DEFAULT_STOPPED_SESSION_ID
    assert workspace.fixtures["cli_restore_session_id"] == DEFAULT_CLI_RESTORE_SESSION_ID
    assert workspace.fixtures["queue_job_id"] == "job-fixture"

    config_text = workspace.config_path.read_text()
    assert "fixture_writes_enabled: true" in config_text
    assert str(workspace.state_file) in config_text
    assert str(workspace.fixture_dir / "apps") in config_text
    assert str(workspace.fixture_dir / "queue-runner") in config_text
    assert str(workspace.root / "message_queue.db") in config_text
    assert str(workspace.root / "tool_usage.db") in config_text
    assert "~/.local/share/claude-sessions" not in config_text

    state = json.loads(workspace.state_file.read_text())
    sessions = {session["id"]: session for session in state["sessions"]}
    assert sessions[DEFAULT_STOPPED_SESSION_ID]["status"] == "stopped"
    assert sessions[DEFAULT_CLI_RESTORE_SESSION_ID]["status"] == "stopped"
    for session in sessions.values():
        assert Path(session["log_file"]).is_relative_to(workspace.log_dir)
    assert sessions[DEFAULT_EM_SESSION_ID]["is_em"] is True
    assert sessions[DEFAULT_EM_SESSION_ID]["status"] == "running"
    assert (
        sessions[DEFAULT_NOTIFY_CHILD_SESSION_ID]["parent_session_id"]
        == DEFAULT_EM_SESSION_ID
    )
    assert sessions[DEFAULT_NOTIFY_CHILD_SESSION_ID]["is_em"] is False
    assert sessions[DEFAULT_NOTIFY_CHILD_SESSION_ID]["status"] == "running"

    read_only_state = json.loads(
        Path("scripts/rust_migration/fixtures/read_only/sessions.json").read_text()
    )
    read_only_ids = {session["id"] for session in read_only_state["sessions"]}
    assert DEFAULT_EM_SESSION_ID not in read_only_ids
    assert DEFAULT_NOTIFY_CHILD_SESSION_ID not in read_only_ids
    assert DEFAULT_CLI_RESTORE_SESSION_ID not in read_only_ids


def test_mutating_fixture_workspace_refuses_non_empty_output_dir(tmp_path):
    (tmp_path / "leftover.txt").write_text("do not overwrite\n")

    try:
        create_mutating_fixture_workspace(tmp_path)
    except FileExistsError as exc:
        assert str(tmp_path) in str(exc)
    else:
        raise AssertionError("expected non-empty output directory to be rejected")


def test_python_target_does_not_run_rust_only_retirement_checks():
    manifest = ContractManifest.load()
    selected = checks_for_target(manifest.checks, target="python", include_mutating=False)
    ids = {check.id for check in selected}

    assert "cli.kill_alias_retired" not in ids
    assert "cli.what_retired" not in ids
    assert "cli.telegram_alias_retired" not in ids
    assert "cli.codex_server_retired" not in ids
    assert "http.public_watch_operational_data_denied" not in ids
    assert "http.summary_provider_route_retired" not in ids
    assert "http.mobile_session_stop" in ids
    assert "http.api_sessions_absent" in ids


def test_mutating_checks_are_reported_as_skipped_without_opt_in():
    manifest = ContractManifest.load()
    results = run_checks(
        manifest,
        target="python",
        base_url=None,
        sm_binary="sm",
        session_id=None,
        include_mutating=False,
    )
    result_by_id = {result.id: result for result in results}

    assert result_by_id["http.mobile_session_stop"].status == "skipped"
    assert result_by_id["http.mobile_session_stop"].detail


def test_skip_fixture_checks_excludes_synthetic_fixture_checks_only():
    manifest = ContractManifest.load()
    selected = checks_for_target(
        manifest.checks,
        target="rust",
        include_mutating=False,
        skip_fixture_checks=True,
    )
    ids = {check.id for check in selected}

    assert "http.session_detail_fixture" not in ids
    assert "http.session_output_fixture" not in ids
    assert "http.queue_job_detail" not in ids
    assert "http.health" in ids
    assert "http.sessions" in ids
    assert "http.session_tool_calls" in ids
    assert "cli.what_retired" in ids


def test_run_checks_skip_fixture_checks_allows_live_session_id_without_fixture_assertions():
    manifest = ContractManifest(
        schema_version=1,
        source_spec="test",
        artifacts=(),
        checks=(
            ContractCheck(
                id="http.live",
                surface="http",
                classification="retained",
                target="python_and_rust",
                safety="read_only",
                method="GET",
                path="/health",
                expected_status=(200,),
                preconditions=("live_server",),
                source="test",
            ),
            ContractCheck(
                id="http.session_detail_fixture",
                surface="http",
                classification="retained",
                target="python_and_rust",
                safety="read_only",
                method="GET",
                path="/sessions/{session_id}",
                expected_status=(200,),
                preconditions=("live_server", "session_id"),
                source="test",
            ),
        ),
    )

    with patch("scripts.rust_migration.contracts.urllib.request.urlopen") as urlopen:
        response = urlopen.return_value.__enter__.return_value
        response.status = 200
        response.read.return_value = b"{}"
        results = run_checks(
            manifest,
            target="rust",
            base_url="http://127.0.0.1:8421",
            sm_binary="target/debug/sm",
            session_id="live-session",
            include_mutating=False,
            skip_fixture_checks=True,
        )

    assert [result.id for result in results] == ["http.live"]


def test_existing_session_precondition_fails_for_stale_session_fixture():
    manifest = ContractManifest.load()
    not_found = urllib.error.HTTPError(
        url="http://127.0.0.1:8421/sessions/stale",
        code=404,
        msg="not found",
        hdrs=None,
        fp=None,
    )

    with patch("scripts.rust_migration.contracts.urllib.request.urlopen", side_effect=not_found):
        results = run_checks(
            manifest,
            target="rust",
            base_url="http://127.0.0.1:8421",
            sm_binary="sm",
            session_id="stale",
            check_ids={"http.summary_provider_route_retired"},
            include_mutating=False,
        )

    assert len(results) == 1
    assert results[0].status == "failed"
    assert "session fixture precondition failed" in results[0].detail


def test_summary_counts_statuses():
    manifest = ContractManifest.load()
    results = run_checks(
        manifest,
        target="python",
        base_url=None,
        sm_binary="definitely-not-an-sm-binary",
        session_id=None,
        include_mutating=False,
    )

    summary = summarize(results)
    assert summary["failed"] == 0
    assert summary["skipped"] > 0


def test_supplied_live_server_connection_failure_is_failed_not_skipped():
    check = ContractCheck(
        id="http.test",
        surface="http",
        classification="retained",
        target="python_and_rust",
        safety="read_only",
        method="GET",
        path="/health",
        expected_status=(200,),
        preconditions=("live_server",),
        source="test",
    )

    with patch(
        "scripts.rust_migration.contracts.urllib.request.urlopen",
        side_effect=urllib.error.URLError("down"),
    ):
        result = _run_http_check(check, "http://127.0.0.1:1", {}, 0.1)

    assert result.status == "failed"
    assert "live server unavailable" in result.detail


def test_supplied_live_server_connection_reset_is_failed_not_crashed():
    check = ContractCheck(
        id="http.test",
        surface="http",
        classification="retained",
        target="python_and_rust",
        safety="read_only",
        method="GET",
        path="/health",
        expected_status=(200,),
        preconditions=("live_server",),
        source="test",
    )

    with patch(
        "scripts.rust_migration.contracts.urllib.request.urlopen",
        side_effect=ConnectionResetError("reset by peer"),
    ):
        result = _run_http_check(check, "http://127.0.0.1:8420", {}, 0.1)

    assert result.status == "failed"
    assert "live server unavailable" in result.detail


def test_post_checks_send_empty_json_body():
    check = ContractCheck(
        id="http.post",
        surface="http",
        classification="retained",
        target="python_and_rust",
        safety="mutating",
        method="POST",
        path="/sessions/{session_id}/kill",
        expected_status=(200,),
        preconditions=("live_server", "session_id", "mutating_opt_in"),
        source="test",
    )
    seen = {}

    class FakeResponse:
        status = 200

        def __enter__(self):
            return self

        def __exit__(self, exc_type, exc, tb):
            return False

        def read(self, _size):
            return b"{}"

    def fake_urlopen(request, timeout):
        seen["data"] = request.data
        seen["content_type"] = request.headers.get("Content-type")
        seen["url"] = request.full_url
        return FakeResponse()

    with patch("scripts.rust_migration.contracts.urllib.request.urlopen", fake_urlopen):
        result = _run_http_check(check, "http://127.0.0.1:8420", {"session_id": "abc123"}, 1.0)

    assert result.status == "passed"
    assert seen["data"] == b"{}"
    assert seen["content_type"] == "application/json"
    assert seen["url"].endswith("/sessions/abc123/kill")


def test_http_check_renders_request_headers_and_checks_json():
    check = ContractCheck(
        id="http.header_json",
        surface="http",
        classification="retained",
        target="python_and_rust",
        safety="read_only",
        method="GET",
        path="/sessions",
        expected_status=(200,),
        request_headers=(("Host", "{public_host}"),),
        expected_json=(
            JsonExpectation(path="/sessions", value_type="array"),
        ),
        preconditions=("live_server",),
        source="test",
    )
    seen = {}

    class FakeResponse:
        status = 200

        def __enter__(self):
            return self

        def __exit__(self, exc_type, exc, tb):
            return False

        def read(self, _size):
            return b'{"sessions":[]}'

    def fake_urlopen(request, timeout):
        seen["host"] = request.headers.get("Host")
        return FakeResponse()

    with patch("scripts.rust_migration.contracts.urllib.request.urlopen", fake_urlopen):
        result = _run_http_check(
            check,
            "http://127.0.0.1:8420",
            {"public_host": "sm.example.com"},
            1.0,
        )

    assert result.status == "passed"
    assert seen["host"] == "sm.example.com"


def test_http_check_renders_json_expectation_fixture_values():
    check = ContractCheck(
        id="http.fixture_json",
        surface="http",
        classification="retained",
        target="python_and_rust",
        safety="read_only",
        method="GET",
        path="/sessions/{session_id}",
        expected_status=(200,),
        expected_json=(
            JsonExpectation(path="/id", equals="{session_id}", has_equals=True),
            JsonExpectation(path="/output", contains="{expected_line}"),
        ),
        preconditions=("live_server", "session_id"),
        source="test",
    )

    class FakeResponse:
        status = 200

        def __enter__(self):
            return self

        def __exit__(self, exc_type, exc, tb):
            return False

        def read(self, _size):
            return b'{"id":"fixture001","output":"fixture line 3"}'

    with patch(
        "scripts.rust_migration.contracts.urllib.request.urlopen",
        return_value=FakeResponse(),
    ):
        result = _run_http_check(
            check,
            "http://127.0.0.1:8420",
            {"session_id": "fixture001", "expected_line": "line 3"},
            1.0,
        )

    assert result.status == "passed"


def test_http_check_reports_json_shape_mismatch():
    check = ContractCheck(
        id="http.json",
        surface="http",
        classification="retained",
        target="python_and_rust",
        safety="read_only",
        method="GET",
        path="/sessions",
        expected_status=(200,),
        expected_json=(
            JsonExpectation(path="/sessions", value_type="array"),
        ),
        preconditions=("live_server",),
        source="test",
    )

    class FakeResponse:
        status = 200

        def __enter__(self):
            return self

        def __exit__(self, exc_type, exc, tb):
            return False

        def read(self, _size):
            return b'{"sessions":{}}'

    with patch(
        "scripts.rust_migration.contracts.urllib.request.urlopen",
        return_value=FakeResponse(),
    ):
        result = _run_http_check(check, "http://127.0.0.1:8420", {}, 1.0)

    assert result.status == "failed"
    assert "expected type array" in result.detail


def test_json_expectations_support_absent_equals_and_contains():
    expectations = (
        JsonExpectation(path="/auth/session_endpoint", equals="/auth/session", has_equals=True),
        JsonExpectation(path="/status", one_of=("healthy", "degraded", "unhealthy")),
        JsonExpectation(path="/external_access/ssh_proxy_command", absent=True),
        JsonExpectation(path="/values", contains="mobile"),
    )
    body = (
        '{"auth":{"session_endpoint":"/auth/session"},"status":"degraded",'
        '"external_access":{},"values":["mobile"]}'
    )

    assert _json_expectation_error(body, expectations) is None


def test_cli_check_requires_all_expected_output_tokens():
    check = ContractCheck(
        id="cli.review",
        surface="cli",
        classification="retained",
        target="python_and_rust",
        safety="read_only",
        command=("review", "--help"),
        expected_exit=(0,),
        expected_output_contains_all=("--base", "--pr"),
        preconditions=("sm_cli",),
        source="test",
    )
    completed = subprocess.CompletedProcess(
        args=["sm", "review", "--help"],
        returncode=0,
        stdout="usage: sm review --base\n",
        stderr="",
    )

    with patch(
        "scripts.rust_migration.contracts.subprocess.run",
        return_value=completed,
    ):
        result = _run_cli_check(check, "sm", {}, 1.0)

    assert result.status == "failed"
    assert "--pr" in result.detail


def test_cli_check_renders_fixture_values_in_command():
    check = ContractCheck(
        id="cli.fixture",
        surface="cli",
        classification="retained",
        target="rust_only",
        safety="mutating",
        command=("--api-url", "{base_url}", "output", "{session_id}"),
        expected_exit=(0,),
        expected_output_contains_any=("hello",),
        preconditions=("sm_cli", "fixture:base_url", "session_id"),
        source="test",
    )
    completed = subprocess.CompletedProcess(
        args=["target/debug/sm", "--api-url", "http://127.0.0.1:8421", "output", "rustcore"],
        returncode=0,
        stdout="hello\n",
        stderr="",
    )

    with patch(
        "scripts.rust_migration.contracts.subprocess.run",
        return_value=completed,
    ) as run:
        result = _run_cli_check(
            check,
            "target/debug/sm",
            {"base_url": "http://127.0.0.1:8421", "session_id": "rustcore"},
            1.0,
        )

    assert result.status == "passed"
    assert run.call_args.args[0] == [
        "target/debug/sm",
        "--api-url",
        "http://127.0.0.1:8421",
        "output",
        "rustcore",
    ]


def test_cli_check_renders_fixture_values_in_environment():
    check = ContractCheck(
        id="cli.env_fixture",
        surface="cli",
        classification="retained",
        target="rust_only",
        safety="mutating",
        command=("status", "{status_text}"),
        expected_exit=(0,),
        expected_output_contains_any=("Status set",),
        env=(("SESSION_MANAGER_ID", "{session_id}"),),
        preconditions=("sm_cli", "session_id", "fixture:status_text"),
        source="test",
    )
    completed = subprocess.CompletedProcess(
        args=["target/debug/sm", "status", "working"],
        returncode=0,
        stdout="Status set: working\n",
        stderr="",
    )

    with patch(
        "scripts.rust_migration.contracts.subprocess.run",
        return_value=completed,
    ) as run:
        result = _run_cli_check(
            check,
            "target/debug/sm",
            {"session_id": "rustcore", "status_text": "working"},
            1.0,
        )

    assert result.status == "passed"
    assert run.call_args.kwargs["env"]["SESSION_MANAGER_ID"] == "rustcore"


def test_line_mode_http_check_reads_one_streaming_line():
    check = ContractCheck(
        id="http.events",
        surface="http",
        classification="retained",
        target="python_and_rust",
        safety="read_only",
        method="GET",
        path="/events",
        expected_status=(200,),
        preconditions=("live_server",),
        source="test",
        read_mode="line",
        read_bytes=128,
        expected_body_contains_any=("event: hello",),
    )

    class FakeResponse:
        status = 200

        def __enter__(self):
            return self

        def __exit__(self, exc_type, exc, tb):
            return False

        def readline(self, _size):
            return b"event: hello\n"

        def read(self, _size):
            raise AssertionError("line-mode checks should not use read()")

    with patch(
        "scripts.rust_migration.contracts.urllib.request.urlopen",
        return_value=FakeResponse(),
    ):
        result = _run_http_check(check, "http://127.0.0.1:8420", {}, 1.0)

    assert result.status == "passed"


def test_baseline_memory_discovery_uses_selected_base_url_port():
    assert _port_from_base_url("http://127.0.0.1:8421") == 8421
    assert _port_from_base_url("http://127.0.0.1") == 80
    assert _port_from_base_url("https://sm.example.test") == 443
    assert _port_from_base_url(None) == 8420


def test_rust_baseline_requires_explicit_base_url():
    assert _resolve_base_url("python", None) == "http://127.0.0.1:8420"
    assert _resolve_base_url("rust", "http://127.0.0.1:8421") == "http://127.0.0.1:8421"
    try:
        _resolve_base_url("rust", None)
    except ValueError as exc:
        assert "--base-url" in str(exc)
    else:
        raise AssertionError("expected ValueError")


def test_memory_unit_parser_handles_vmmap_units():
    assert _parse_memory_to_mib("512K") == 0.5
    assert _parse_memory_to_mib("87.3M") == 87.3
    assert _parse_memory_to_mib("1.5G") == 1536.0
    assert _parse_memory_to_mib("unknown") is None


def test_baseline_runner_records_owner_waived_hardening_and_target():
    report = run_baseline(
        target="rust",
        base_url=None,
        sm_binary="sm",
        repetitions=1,
        output=None,
        server_pid=None,
        check_ids={"http.health"},
    )

    assert report["target"] == "rust"
    assert report["inputs"]["target"] == "rust"
    assert report["inputs"]["check_ids"] == ["http.health"]
    assert report["python_hardening_comparison"]["status"] == "owner_waived"
    assert report["latency"]["skipped"]["http.health"] == "live server URL not supplied"


def test_template_rendering_replaces_nested_fixture_values():
    rendered = _render_template(
        {
            "path": "/sessions/{session_id}/kill",
            "items": ["{session_id}", "{missing}"],
        },
        {"session_id": "abc123"},
    )

    assert rendered == {
        "path": "/sessions/abc123/kill",
        "items": ["abc123", "{missing}"],
    }


def test_parse_fixtures_rejects_missing_equals():
    try:
        _parse_fixtures(["session_id"])
    except SystemExit as exc:
        assert "KEY=VALUE" in str(exc)
    else:
        raise AssertionError("expected SystemExit")


def test_fixture_precondition_reports_missing_value():
    manifest = ContractManifest.load()
    results = run_checks(
        manifest,
        target="python",
        base_url="http://127.0.0.1:8420",
        sm_binary="sm",
        session_id=None,
        fixtures={},
        include_mutating=False,
    )
    result_by_id = {result.id: result for result in results}

    assert result_by_id["http.app_artifact_metadata"].status == "skipped"
    assert "app_name" in result_by_id["http.app_artifact_metadata"].detail


def test_check_id_filter_limits_selected_checks():
    manifest = ContractManifest.load()
    results = run_checks(
        manifest,
        target="python",
        base_url="http://127.0.0.1:8420",
        sm_binary="sm",
        session_id=None,
        fixtures={},
        check_ids={"http.health"},
        include_mutating=False,
    )

    assert [result.id for result in results] == ["http.health"]


def test_check_id_filter_rejects_unknown_ids():
    manifest = ContractManifest.load()

    try:
        run_checks(
            manifest,
            target="python",
            base_url="http://127.0.0.1:8420",
            sm_binary="sm",
            session_id=None,
            fixtures={},
            check_ids={"does.not.exist"},
            include_mutating=False,
        )
    except ValueError as exc:
        assert "does.not.exist" in str(exc)
    else:
        raise AssertionError("expected ValueError")
