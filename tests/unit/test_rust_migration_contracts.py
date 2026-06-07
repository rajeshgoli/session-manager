import urllib.error
from unittest.mock import patch

from scripts.rust_migration.baseline import _port_from_base_url
from scripts.rust_migration.contracts import (
    ContractCheck,
    ContractManifest,
    _run_http_check,
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


def test_python_target_does_not_run_rust_only_retirement_checks():
    manifest = ContractManifest.load()
    selected = checks_for_target(manifest.checks, target="python", include_mutating=False)
    ids = {check.id for check in selected}

    assert "cli.kill_alias_retired" not in ids
    assert "cli.what_retired" not in ids
    assert "http.mobile_session_stop" in ids


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
        result = _run_http_check(check, "http://127.0.0.1:1", None, 0.1)

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
        result = _run_http_check(check, "http://127.0.0.1:8420", "abc123", 1.0)

    assert result.status == "passed"
    assert seen["data"] == b"{}"
    assert seen["content_type"] == "application/json"
    assert seen["url"].endswith("/sessions/abc123/kill")


def test_baseline_memory_discovery_uses_selected_base_url_port():
    assert _port_from_base_url("http://127.0.0.1:8421") == 8421
    assert _port_from_base_url("http://127.0.0.1") == 80
    assert _port_from_base_url("https://sm.example.test") == 443
    assert _port_from_base_url(None) == 8420
