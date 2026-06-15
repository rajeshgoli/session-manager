import json
from datetime import datetime, timezone

from scripts.rust_migration.shadow_report import (
    main,
    render_text_report,
    summarize_ledger,
)


def _write_jsonl(path, rows):
    path.write_text(
        "\n".join(json.dumps(row, sort_keys=True) for row in rows) + "\n",
        encoding="utf-8",
    )


def test_shadow_report_summarizes_clean_status_and_body_matches(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "schema_version": 1,
                "observed_at": "2026-06-12T01:00:00Z",
                "method": "GET",
                "path": "/health",
                "query_string": "",
                "python_status": 200,
                "python_body_sha256": "python-health",
                "rust_http_status": 200,
                "rust_result": {
                    "comparison": "match",
                    "support_status": "implemented_read",
                    "predicted_status": 200,
                    "body_sha256_match": True,
                },
            },
            {
                "schema_version": 1,
                "observed_at": "2026-06-12T01:00:01Z",
                "method": "GET",
                "path": "/sessions",
                "query_string": "",
                "python_status": 200,
                "python_body_sha256": "python-sessions",
                "rust_http_status": 200,
                "rust_result": {
                    "comparison": "status_match",
                    "support_status": "implemented_read_status_only",
                    "predicted_status": 200,
                    "body_sha256_match": None,
                },
            },
        ],
    )

    report = summarize_ledger(ledger)

    assert report["status"] == "passed"
    assert report["row_count"] == 2
    assert report["comparison_counts"] == {"match": 1, "status_match": 1}
    assert report["support_status_counts"] == {
        "implemented_read": 1,
        "implemented_read_status_only": 1,
    }
    assert report["blockers"] == []
    assert [row["route"] for row in report["route_summaries"]] == [
        "GET /health",
        "GET /sessions",
    ]


def test_shadow_report_marks_mismatches_and_shadow_errors_as_blockers(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "schema_version": 1,
                "observed_at": "2026-06-12T01:00:00Z",
                "method": "GET",
                "path": "/events/state",
                "query_string": "",
                "python_status": 200,
                "python_body_sha256": "python-events",
                "rust_http_status": 200,
                "rust_result": {
                    "comparison": "body_mismatch",
                    "support_status": "implemented_read",
                    "predicted_status": 200,
                    "body_sha256_match": False,
                },
            },
            {
                "schema_version": 1,
                "observed_at": "2026-06-12T01:00:01Z",
                "method": "GET",
                "path": "/nodes",
                "query_string": "",
                "python_status": 200,
                "python_body_sha256": "python-nodes",
                "shadow_error": "ConnectError",
                "shadow_error_message": "connection refused",
            },
        ],
    )

    report = summarize_ledger(ledger)

    assert report["status"] == "blocked"
    assert report["comparison_counts"] == {"body_mismatch": 1, "shadow_error": 1}
    assert [blocker["kind"] for blocker in report["blockers"]] == [
        "body_mismatch",
        "shadow_error",
    ]
    assert report["blockers"][0]["route"] == "GET /events/state"
    assert report["blockers"][0]["support_status"] == "implemented_read"
    assert report["blockers"][0]["python_status"] == 200
    assert report["blockers"][0]["predicted_status"] == 200
    assert report["blockers"][1]["detail"] == "connection refused"


def test_shadow_report_blocks_unimplemented_device_auth_success(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "schema_version": 1,
                "observed_at": "2026-06-12T01:00:00Z",
                "method": "POST",
                "path": "/auth/device/google",
                "query_string": "",
                "python_status": 200,
                "python_body_sha256": "python-device-auth",
                "rust_http_status": 200,
                "rust_result": {
                    "comparison": "status_mismatch",
                    "support_status": "unimplemented_device_auth_success",
                    "predicted_status": 401,
                    "body_sha256_match": None,
                },
            }
        ],
    )

    report = summarize_ledger(ledger)

    assert report["status"] == "blocked"
    assert report["comparison_counts"] == {"status_mismatch": 1}
    assert report["blockers"] == [
        {
            "kind": "status_mismatch",
            "line": 1,
            "route": "POST /auth/device/google",
            "support_status": "unimplemented_device_auth_success",
            "predicted_status": 401,
            "python_status": 200,
        }
    ]


def test_shadow_report_tracks_invalid_json_as_blocker(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    ledger.write_text(
        '{"method":"GET","path":"/health","python_status":200,'
        '"rust_http_status":200,"rust_result":{"comparison":"match"}}\n'
        "{not-json}\n",
        encoding="utf-8",
    )

    report = summarize_ledger(ledger)

    assert report["status"] == "blocked"
    assert report["row_count"] == 1
    assert report["invalid_row_count"] == 1
    assert report["blockers"][-1]["kind"] == "invalid_json"
    assert report["blockers"][-1]["line"] == 2


def test_shadow_report_tracks_non_object_json_rows_as_blockers(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    ledger.write_text(
        '{"method":"GET","path":"/health","python_status":200,'
        '"rust_http_status":200,"rust_result":{"comparison":"match"}}\n'
        "[]\n",
        encoding="utf-8",
    )

    report = summarize_ledger(ledger)

    assert report["status"] == "blocked"
    assert report["row_count"] == 1
    assert report["invalid_row_count"] == 1
    assert report["blockers"][-1] == {
        "kind": "invalid_row_shape",
        "line": 2,
        "detail": "expected JSON object, got list",
    }


def test_shadow_report_renders_text_and_fail_on_blockers_exit(tmp_path, capsys):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "method": "GET",
                "path": "/health",
                "python_status": 200,
                "rust_http_status": 503,
                "rust_result": {"comparison": "match"},
            }
        ],
    )

    report = summarize_ledger(ledger)
    text = render_text_report(report)

    assert "status: blocked" in text
    assert "GET /health" in text
    assert "shadow_http_status" in text
    assert main(["--ledger", str(ledger), "--fail-on-blockers"]) == 1
    rendered = capsys.readouterr().out
    assert "Rust shadow observation report" in rendered


def test_shadow_report_fail_on_blockers_exits_nonzero_for_no_data(tmp_path, capsys):
    missing_ledger = tmp_path / "missing-rust-shadow.jsonl"

    assert main(["--ledger", str(missing_ledger), "--fail-on-blockers"]) == 1
    rendered = capsys.readouterr().out
    assert "status: no_data" in rendered
    assert "rows: 0" in rendered


def test_shadow_report_marks_non_numeric_http_status_as_blocker(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "method": "GET",
                "path": "/health",
                "python_status": 200,
                "rust_http_status": "not-a-status",
                "rust_result": {"comparison": "match"},
            }
        ],
    )

    report = summarize_ledger(ledger)

    assert report["status"] == "blocked"
    assert report["blockers"] == [
        {
            "kind": "invalid_rust_http_status",
            "line": 1,
            "route": "GET /health",
            "detail": "not-a-status",
        }
    ]


def test_shadow_report_filters_valid_rows_since_timestamp(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "observed_at": "2026-06-12T01:59:59Z",
                "method": "GET",
                "path": "/before",
                "rust_http_status": 200,
                "rust_result": {"comparison": "match"},
            },
            {
                "observed_at": "2026-06-12T02:00:00+00:00",
                "method": "GET",
                "path": "/at",
                "rust_http_status": 200,
                "rust_result": {"comparison": "match"},
            },
            {
                "observed_at": "2026-06-12T02:00:01+00:00",
                "method": "GET",
                "path": "/after",
                "rust_http_status": 200,
                "rust_result": {"comparison": "status_match"},
            },
        ],
    )

    report = summarize_ledger(
        ledger,
        since=datetime(2026, 6, 12, 2, 0, 0, tzinfo=timezone.utc),
    )

    assert report["status"] == "passed"
    assert report["row_count"] == 2
    assert report["filter"]["since"] == "2026-06-12T02:00:00+00:00"
    assert [row["route"] for row in report["route_summaries"]] == [
        "GET /after",
        "GET /at",
    ]


def test_shadow_report_last_minutes_uses_supplied_now(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "observed_at": "2026-06-12T02:44:59+00:00",
                "method": "GET",
                "path": "/old",
                "rust_http_status": 200,
                "rust_result": {"comparison": "match"},
            },
            {
                "observed_at": "2026-06-12T02:45:00+00:00",
                "method": "GET",
                "path": "/inside",
                "rust_http_status": 200,
                "rust_result": {"comparison": "match"},
            },
        ],
    )

    report = summarize_ledger(
        ledger,
        last_minutes=15,
        now=datetime(2026, 6, 12, 3, 0, 0, tzinfo=timezone.utc),
    )

    assert report["status"] == "passed"
    assert report["row_count"] == 1
    assert report["filter"] == {
        "since": "2026-06-12T02:45:00+00:00",
        "last_minutes": 15,
    }
    assert report["route_summaries"][0]["route"] == "GET /inside"


def test_shadow_report_keeps_invalid_json_visible_under_filter(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    ledger.write_text(
        json.dumps(
            {
                "observed_at": "2026-06-12T01:00:00Z",
                "method": "GET",
                "path": "/old",
                "rust_http_status": 200,
                "rust_result": {"comparison": "match"},
            }
        )
        + "\n{not-json}\n",
        encoding="utf-8",
    )

    report = summarize_ledger(
        ledger,
        since=datetime(2026, 6, 12, 2, 0, 0, tzinfo=timezone.utc),
    )

    assert report["status"] == "blocked"
    assert report["row_count"] == 0
    assert report["invalid_row_count"] == 1
    assert report["blockers"][0]["kind"] == "invalid_json"


def test_shadow_report_marks_unfilterable_timestamp_as_blocker(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "method": "GET",
                "path": "/missing-timestamp",
                "rust_http_status": 200,
                "rust_result": {"comparison": "match"},
            }
        ],
    )

    report = summarize_ledger(
        ledger,
        since=datetime(2026, 6, 12, 2, 0, 0, tzinfo=timezone.utc),
    )

    assert report["status"] == "blocked"
    assert report["row_count"] == 0
    assert report["blockers"] == [
        {
            "kind": "invalid_observed_at",
            "line": 1,
            "route": "GET /missing-timestamp",
            "detail": "missing observed_at timestamp",
        }
    ]


def test_shadow_report_cli_accepts_since_filter(tmp_path, capsys):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "observed_at": "2026-06-12T01:00:00Z",
                "method": "GET",
                "path": "/old",
                "rust_http_status": 200,
                "rust_result": {"comparison": "match"},
            },
            {
                "observed_at": "2026-06-12T02:00:00Z",
                "method": "GET",
                "path": "/new",
                "rust_http_status": 200,
                "rust_result": {"comparison": "match"},
            },
        ],
    )

    assert main(["--ledger", str(ledger), "--since", "2026-06-12T02:00:00Z", "--json"]) == 0
    report = json.loads(capsys.readouterr().out)

    assert report["row_count"] == 1
    assert report["route_summaries"][0]["route"] == "GET /new"


def test_shadow_report_min_rows_gate_blocks_small_window(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "observed_at": "2026-06-12T02:00:00Z",
                "method": "GET",
                "path": "/sessions",
                "rust_http_status": 200,
                "rust_result": {"comparison": "status_match"},
            },
        ],
    )

    report = summarize_ledger(ledger, min_rows=2)

    assert report["status"] == "blocked"
    assert report["row_count"] == 1
    assert report["gates"]["min_rows"] == 2
    assert report["blockers"] == [
        {
            "kind": "insufficient_rows",
            "detail": "observed 1, required 2",
        }
    ]
    assert "Coverage Gates:" in render_text_report(report)
    assert "insufficient_rows" in render_text_report(report)


def test_shadow_report_route_coverage_gates_block_missing_or_sparse_routes(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "observed_at": "2026-06-12T02:00:00Z",
                "method": "GET",
                "path": "/sessions",
                "rust_http_status": 200,
                "rust_result": {"comparison": "status_match"},
            },
            {
                "observed_at": "2026-06-12T02:00:01Z",
                "method": "GET",
                "path": "/events/state",
                "rust_http_status": 200,
                "rust_result": {"comparison": "status_match"},
            },
        ],
    )

    report = summarize_ledger(
        ledger,
        required_routes=("GET /sessions", "GET /client/sessions"),
        min_route_rows={"GET /sessions": 2, "GET /events/state": 1},
    )

    assert report["status"] == "blocked"
    assert report["gates"] == {
        "min_rows": None,
        "required_routes": ["GET /sessions", "GET /client/sessions"],
        "min_route_rows": {
            "GET /events/state": 1,
            "GET /sessions": 2,
        },
        "required_route_patterns": [],
        "min_route_pattern_rows": {},
    }
    assert report["blockers"] == [
        {
            "kind": "missing_required_route",
            "route": "GET /client/sessions",
            "detail": "required route not observed",
        },
        {
            "kind": "insufficient_route_rows",
            "route": "GET /sessions",
            "detail": "observed 1, required 2",
        },
    ]


def test_shadow_report_route_pattern_gates_cover_mobile_detail_routes(tmp_path):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "observed_at": "2026-06-12T02:00:00Z",
                "method": "GET",
                "path": "/client/sessions/fixture001",
                "rust_http_status": 200,
                "rust_result": {"comparison": "status_match"},
            },
            {
                "observed_at": "2026-06-12T02:00:01Z",
                "method": "GET",
                "path": "/sessions/fixture001/attach-descriptor",
                "rust_http_status": 200,
                "rust_result": {"comparison": "match"},
            },
        ],
    )

    report = summarize_ledger(
        ledger,
        required_route_patterns=(
            "GET /client/sessions/*",
            "GET /sessions/*/attach-descriptor",
            "GET /client/bug-reports/*",
        ),
        min_route_pattern_rows={
            "GET /client/sessions/*": 2,
            "GET /sessions/*/attach-descriptor": 1,
        },
    )

    assert report["status"] == "blocked"
    assert report["gates"]["required_route_patterns"] == [
        "GET /client/sessions/*",
        "GET /sessions/*/attach-descriptor",
        "GET /client/bug-reports/*",
    ]
    assert report["gates"]["min_route_pattern_rows"] == {
        "GET /client/sessions/*": 2,
        "GET /sessions/*/attach-descriptor": 1,
    }
    assert report["blockers"] == [
        {
            "kind": "missing_required_route_pattern",
            "route": "GET /client/bug-reports/*",
            "detail": "required route pattern not observed",
        },
        {
            "kind": "insufficient_route_pattern_rows",
            "route": "GET /client/sessions/*",
            "detail": "observed 1, required 2",
        },
    ]


def test_shadow_report_cli_accepts_coverage_gates(tmp_path, capsys):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "observed_at": "2026-06-12T02:00:00Z",
                "method": "GET",
                "path": "/sessions",
                "rust_http_status": 200,
                "rust_result": {"comparison": "status_match"},
            },
            {
                "observed_at": "2026-06-12T02:00:01Z",
                "method": "GET",
                "path": "/client/sessions/fixture001",
                "rust_http_status": 200,
                "rust_result": {"comparison": "status_match"},
            },
        ],
    )

    assert (
        main(
            [
                "--ledger",
                str(ledger),
                "--min-rows",
                "1",
                "--require-route",
                "get /sessions",
                "--min-route-rows",
                "get /sessions=1",
                "--require-route-pattern",
                "get /client/sessions/*",
                "--min-route-pattern-rows",
                "get /client/sessions/*=1",
                "--json",
                "--fail-on-blockers",
            ]
        )
        == 0
    )
    report = json.loads(capsys.readouterr().out)

    assert report["status"] == "passed"
    assert report["gates"] == {
        "min_rows": 1,
        "required_routes": ["GET /sessions"],
        "min_route_rows": {"GET /sessions": 1},
        "required_route_patterns": ["GET /client/sessions/*"],
        "min_route_pattern_rows": {"GET /client/sessions/*": 1},
    }


def test_shadow_report_cli_coverage_gates_fail_with_blockers(tmp_path, capsys):
    ledger = tmp_path / "rust_shadow.jsonl"
    _write_jsonl(
        ledger,
        [
            {
                "observed_at": "2026-06-12T02:00:00Z",
                "method": "GET",
                "path": "/sessions",
                "rust_http_status": 200,
                "rust_result": {"comparison": "status_match"},
            },
        ],
    )

    assert (
        main(
            [
                "--ledger",
                str(ledger),
                "--min-rows",
                "2",
                "--require-route",
                "GET /client/sessions",
                "--fail-on-blockers",
            ]
        )
        == 1
    )
    rendered = capsys.readouterr().out

    assert "status: blocked" in rendered
    assert "insufficient_rows" in rendered
    assert "missing_required_route" in rendered
