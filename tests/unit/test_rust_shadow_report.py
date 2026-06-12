import json

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
