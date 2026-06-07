import subprocess
import urllib.error
from unittest.mock import patch

from scripts.rust_migration.baseline import _port_from_base_url
from scripts.rust_migration.contracts import (
    ContractCheck,
    ContractManifest,
    _parse_fixtures,
    _run_cli_check,
    _run_http_check,
    _render_template,
    checks_for_target,
    run_checks,
    summarize,
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


def test_python_target_does_not_run_rust_only_retirement_checks():
    manifest = ContractManifest.load()
    selected = checks_for_target(manifest.checks, target="python", include_mutating=False)
    ids = {check.id for check in selected}

    assert "cli.kill_alias_retired" not in ids
    assert "cli.what_retired" not in ids
    assert "cli.telegram_alias_retired" not in ids
    assert "cli.codex_server_retired" not in ids
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
        result = _run_cli_check(check, "sm", 1.0)

    assert result.status == "failed"
    assert "--pr" in result.detail


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
