from pathlib import Path
from urllib.error import HTTPError

from scripts.rust_migration.shadow_observation import (
    HealthProbe,
    build_observation_plan,
    main,
    render_text_plan,
)


def _healthy_probe(base_url: str, timeout_seconds: float) -> HealthProbe:
    return HealthProbe(
        status="healthy",
        detail=f"{base_url} healthy in {timeout_seconds}s",
        elapsed_ms=1.0,
    )


def _python_healthy_rust_unreachable_probe(
    base_url: str, timeout_seconds: float
) -> HealthProbe:
    if base_url.endswith(":8420"):
        return HealthProbe(
            status="healthy",
            detail=f"{base_url} healthy in {timeout_seconds}s",
            elapsed_ms=1.0,
        )
    return HealthProbe(
        status="unreachable",
        detail="connection refused",
        elapsed_ms=1.0,
    )


def _python_healthy_rust_http_error_probe(
    base_url: str, timeout_seconds: float
) -> HealthProbe:
    if base_url.endswith(":8420"):
        return HealthProbe(
            status="healthy",
            detail=f"{base_url} healthy in {timeout_seconds}s",
            elapsed_ms=1.0,
        )
    return HealthProbe(
        status="unhealthy",
        detail="HTTP 404: {\"detail\":\"Not Found\"}",
        elapsed_ms=1.0,
    )


def test_shadow_observation_plan_generates_non_destructive_commands(tmp_path):
    config = tmp_path / "config.yaml"
    config.write_text("server: {}\n", encoding="utf-8")
    ledger = tmp_path / "rust_shadow.jsonl"

    plan = build_observation_plan(
        config=config,
        ledger=ledger,
        shadow_secret="shadow-secret",
        probe_health=_python_healthy_rust_unreachable_probe,
        cargo_resolver=lambda _cargo: "/usr/bin/cargo",
    )

    assert plan["status"] == "ready"
    assert plan["blockers"] == []
    assert plan["commands"]["start_rust_sidecar"] == [
        "cargo",
        "run",
        "-p",
        "sm-server",
        "--bin",
        "sm-server",
        "--",
        "--host",
        "127.0.0.1",
        "--port",
        "8421",
        "--config",
        str(config),
    ]
    assert "rust_shadow:" in plan["python_config_snippet"]
    assert 'secret: "shadow-secret"' in plan["python_config_snippet"]
    assert str(ledger) in plan["python_config_snippet"]
    assert plan["commands"]["summarize_shadow_ledger"] == [
        "./venv/bin/python",
        "-m",
        "scripts.rust_migration.shadow_report",
        "--ledger",
        str(ledger),
        "--fail-on-blockers",
    ]
    rendered = render_text_plan(plan)
    assert "Start Rust sidecar:" in rendered
    assert "Python local config snippet:" in rendered
    assert "Summarize ledger:" in rendered


def test_shadow_observation_plan_includes_report_coverage_gates(tmp_path):
    config = tmp_path / "config.yaml"
    config.write_text("server: {}\n", encoding="utf-8")
    ledger = tmp_path / "rust_shadow.jsonl"

    plan = build_observation_plan(
        config=config,
        ledger=ledger,
        probe_health=_python_healthy_rust_unreachable_probe,
        cargo_resolver=lambda _cargo: "/usr/bin/cargo",
        report_last_minutes=60,
        report_min_rows=1000,
        report_required_routes=["GET /sessions", "GET /events/state"],
        report_min_route_rows=["GET /sessions=100", "GET /events/state=100"],
    )

    assert plan["inputs"]["report_last_minutes"] == 60
    assert plan["inputs"]["report_min_rows"] == 1000
    assert plan["inputs"]["report_required_routes"] == [
        "GET /sessions",
        "GET /events/state",
    ]
    assert plan["inputs"]["report_min_route_rows"] == [
        "GET /sessions=100",
        "GET /events/state=100",
    ]
    assert plan["commands"]["summarize_shadow_ledger"] == [
        "./venv/bin/python",
        "-m",
        "scripts.rust_migration.shadow_report",
        "--ledger",
        str(ledger),
        "--fail-on-blockers",
        "--last-minutes",
        "60",
        "--min-rows",
        "1000",
        "--require-route",
        "GET /sessions",
        "--require-route",
        "GET /events/state",
        "--min-route-rows",
        "GET /sessions=100",
        "--min-route-rows",
        "GET /events/state=100",
    ]
    rendered = render_text_plan(plan)
    assert "--min-rows 1000" in rendered
    assert "'GET /sessions'" in rendered


def test_shadow_observation_mobile_profile_adds_native_app_gates(tmp_path):
    config = tmp_path / "config.yaml"
    config.write_text("server: {}\n", encoding="utf-8")
    ledger = tmp_path / "rust_shadow.jsonl"

    plan = build_observation_plan(
        config=config,
        ledger=ledger,
        probe_health=_python_healthy_rust_unreachable_probe,
        cargo_resolver=lambda _cargo: "/usr/bin/cargo",
        report_required_routes=["GET /sessions"],
        mobile_sm_app_profile=True,
    )

    assert plan["inputs"]["mobile_sm_app_profile"] is True
    assert plan["inputs"]["report_required_routes"] == [
        "GET /sessions",
        "GET /auth/session",
        "GET /client/bootstrap",
        "GET /client/sessions",
        "GET /client/analytics/summary",
    ]
    assert plan["inputs"]["report_required_route_patterns"] == [
        "GET /client/sessions/*",
        "GET /sessions/*/attach-descriptor",
    ]
    assert "--require-route-pattern" in plan["commands"]["summarize_shadow_ledger"]
    assert "GET /client/sessions/*" in plan["commands"]["summarize_shadow_ledger"]
    rendered = render_text_plan(plan)
    assert "--require-route 'GET /client/analytics/summary'" in rendered
    assert "--require-route-pattern 'GET /sessions/*/attach-descriptor'" in rendered
    assert "Exercise the native sm app" in rendered


def test_shadow_observation_cli_json_includes_mobile_profile_gates(tmp_path, capsys):
    config = tmp_path / "config.yaml"
    config.write_text("server: {}\n", encoding="utf-8")

    exit_code = main(
        [
            "--config",
            str(config),
            "--python-base-url",
            "http://127.0.0.1:1",
            "--rust-base-url",
            "http://127.0.0.1:2",
            "--reuse-rust-sidecar",
            "--mobile-sm-app-profile",
            "--json",
            "--fail-on-blockers",
        ]
    )

    assert exit_code == 0
    output = capsys.readouterr().out
    assert '"mobile_sm_app_profile": true' in output
    assert '"GET /client/analytics/summary"' in output
    assert '"GET /sessions/*/attach-descriptor"' in output
    assert '"--require-route-pattern"' in output


def test_shadow_observation_plan_blocks_when_rust_port_already_healthy(tmp_path):
    config = tmp_path / "config.yaml"
    config.write_text("server: {}\n", encoding="utf-8")

    plan = build_observation_plan(
        config=config,
        probe_health=_healthy_probe,
        cargo_resolver=lambda _cargo: "/usr/bin/cargo",
    )

    assert plan["status"] == "blocked"
    assert plan["blockers"] == [
        {
            "kind": "rust_port_in_use",
            "detail": (
                "http://127.0.0.1:8421 is already healthy; pass "
                "--reuse-rust-sidecar or stop that process before starting "
                "a fresh sidecar"
            ),
        }
    ]

    reuse_plan = build_observation_plan(
        config=config,
        probe_health=_healthy_probe,
        cargo_resolver=lambda _cargo: "/usr/bin/cargo",
        reuse_rust_sidecar=True,
    )
    assert reuse_plan["status"] == "ready"
    assert reuse_plan["commands"]["start_rust_sidecar"] is None


def test_shadow_observation_plan_blocks_when_rust_port_returns_http_error(tmp_path):
    config = tmp_path / "config.yaml"
    config.write_text("server: {}\n", encoding="utf-8")

    plan = build_observation_plan(
        config=config,
        probe_health=_python_healthy_rust_http_error_probe,
        cargo_resolver=lambda _cargo: "/usr/bin/cargo",
    )

    assert plan["status"] == "blocked"
    assert plan["blockers"] == [
        {
            "kind": "rust_port_unhealthy",
            "detail": 'HTTP 404: {"detail":"Not Found"}',
        }
    ]


def test_shadow_observation_plan_blocks_missing_config_local_env_and_cargo(tmp_path):
    missing_config = tmp_path / "missing.yaml"
    missing_env = tmp_path / "missing.env"

    plan = build_observation_plan(
        config=missing_config,
        local_env=missing_env,
        probe_health=_python_healthy_rust_unreachable_probe,
        cargo_resolver=lambda _cargo: None,
    )

    assert plan["status"] == "blocked"
    blocker_kinds = {blocker["kind"] for blocker in plan["blockers"]}
    assert blocker_kinds == {"missing_config", "missing_local_env", "missing_cargo"}


def test_shadow_observation_cli_json_and_fail_on_blockers(tmp_path, capsys):
    missing_config = tmp_path / "missing.yaml"

    exit_code = main(
        [
            "--config",
            str(missing_config),
            "--cargo",
            "definitely-not-cargo",
            "--json",
            "--fail-on-blockers",
        ]
    )

    assert exit_code == 1
    output = capsys.readouterr().out
    assert '"status": "blocked"' in output
    assert "missing_config" in output
    assert "missing_cargo" in output


def test_shadow_observation_cli_json_includes_report_coverage_gates(tmp_path, capsys):
    config = tmp_path / "config.yaml"
    config.write_text("server: {}\n", encoding="utf-8")

    exit_code = main(
        [
            "--config",
            str(config),
            "--python-base-url",
            "http://127.0.0.1:1",
            "--rust-base-url",
            "http://127.0.0.1:2",
            "--reuse-rust-sidecar",
            "--report-last-minutes",
            "60",
            "--report-min-rows",
            "1000",
            "--report-require-route",
            "GET /sessions",
            "--report-min-route-rows",
            "GET /sessions=100",
            "--json",
            "--fail-on-blockers",
        ]
    )

    assert exit_code == 0
    output = capsys.readouterr().out
    assert '"report_last_minutes": 60.0' in output
    assert '"report_min_rows": 1000' in output
    assert '"--last-minutes"' in output
    assert '"GET /sessions=100"' in output


def test_shadow_observation_probe_reports_http_error_as_unhealthy(monkeypatch):
    from scripts.rust_migration import shadow_observation

    def raise_http_error(*_args, **_kwargs):
        raise HTTPError(
            url="http://127.0.0.1:8421/health",
            code=404,
            msg="Not Found",
            hdrs={},
            fp=None,
        )

    monkeypatch.setattr(shadow_observation.urllib.request, "urlopen", raise_http_error)

    result = shadow_observation._probe_health("http://127.0.0.1:8421", 1.0)

    assert result.status == "unhealthy"
    assert result.detail == "HTTP 404: Not Found"
