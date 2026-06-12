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
    _json_expectation_error,
    _parse_fixtures,
    _run_cli_check,
    _run_http_check,
    _render_template,
    checks_for_target,
    run_checks,
    summarize,
)
from scripts.rust_migration.mutating_fixture import (
    DEFAULT_CHILD_SESSION_ID,
    DEFAULT_EM_SESSION_ID,
    DEFAULT_NOTIFY_CHILD_SESSION_ID,
    DEFAULT_SESSION_ID,
    create_mutating_fixture_workspace,
)


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
    assert workspace.fixtures["session_id"] == DEFAULT_SESSION_ID
    assert workspace.fixtures["child_session_id"] == DEFAULT_CHILD_SESSION_ID
    assert workspace.fixtures["em_session_id"] == DEFAULT_EM_SESSION_ID
    assert workspace.fixtures["notify_child_session_id"] == DEFAULT_NOTIFY_CHILD_SESSION_ID

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
