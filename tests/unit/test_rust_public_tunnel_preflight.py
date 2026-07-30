from pathlib import Path

from scripts.rust_migration.public_tunnel_preflight import (
    build_public_tunnel_preflight_report,
    main as public_tunnel_preflight_main,
    render_text_report,
)


def _write_tunnel_config(path: Path, ingress: str) -> None:
    path.write_text(
        f"""
tunnel: test-tunnel
credentials-file: /tmp/test-tunnel.json
ingress:
{ingress}
""".lstrip(),
        encoding="utf-8",
    )


EMAIL_ROUTE = """  - hostname: sm.rajeshgo.li
    path: ^/api/email-inbound$
    service: http://127.0.0.1:8420
"""


def test_public_tunnel_preflight_accepts_protected_app_to_rust_service(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        f"""
{EMAIL_ROUTE.rstrip()}
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - service: http_status:404
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(config_path=config)

    assert report["status"] == "passed"
    assert report["summary"]["blockers"] == 0
    assert report["ingress"][0]["hostname"] == "sm.rajeshgo.li"
    assert report["ingress"][0]["path"] == "^/api/email-inbound$"
    assert report["ingress"][0]["service"] == "http://127.0.0.1:8420"


def test_public_tunnel_preflight_blocks_manual_8421_sidecar_target(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        f"""
{EMAIL_ROUTE.rstrip()}
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8421
  - service: http_status:404
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(config_path=config)

    assert report["status"] == "blocked"
    assert {
        "kind": "app_host_wrong_origin",
        "severity": "blocker",
        "detail": (
            "sm-app.rajeshgo.li routes to 'http://127.0.0.1:8421'; "
            "expected 'http://127.0.0.1:8420'"
        ),
        "index": 1,
    } in report["blockers"]


def test_public_tunnel_preflight_blocks_earlier_hostless_rule(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        f"""
{EMAIL_ROUTE.rstrip()}
  - service: http://127.0.0.1:8421
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - service: http_status:404
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(config_path=config)

    assert report["status"] == "blocked"
    assert any(
        issue["kind"] == "app_host_shadowed" and issue["index"] == 1
        for issue in report["blockers"]
    )


def test_public_tunnel_preflight_blocks_path_scoped_app_rule(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        f"""
{EMAIL_ROUTE.rstrip()}
  - hostname: sm-app.rajeshgo.li
    path: /client/*
    service: http://127.0.0.1:8420
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - service: http_status:404
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(config_path=config)

    assert report["status"] == "blocked"
    kinds = {issue["kind"] for issue in report["blockers"]}
    assert "app_host_path_scoped" in kinds
    assert "app_host_shadowed" in kinds


def test_public_tunnel_preflight_blocks_unscoped_legacy_public_host(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        f"""
{EMAIL_ROUTE.rstrip()}
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - hostname: sm.rajeshgo.li
    service: http://127.0.0.1:8420
  - service: http_status:404
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(config_path=config)

    assert report["status"] == "blocked"
    assert any(
        issue["kind"] == "email_host_unexpected_route" and issue["index"] == 2
        for issue in report["blockers"]
    )


def test_public_tunnel_preflight_blocks_missing_email_route(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        """
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - service: http_status:404
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(config_path=config)

    assert report["status"] == "blocked"
    assert any(issue["kind"] == "email_route_missing" for issue in report["blockers"])


def test_public_tunnel_preflight_blocks_wrong_email_path(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        """
  - hostname: sm.rajeshgo.li
    path: /api/email-inbound
    service: http://127.0.0.1:8420
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - service: http_status:404
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(config_path=config)

    kinds = {issue["kind"] for issue in report["blockers"]}
    assert "email_route_missing" in kinds
    assert "email_host_unexpected_route" in kinds


def test_public_tunnel_preflight_blocks_email_route_to_wrong_origin(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        """
  - hostname: sm.rajeshgo.li
    path: ^/api/email-inbound$
    service: http://127.0.0.1:8421
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - service: http_status:404
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(config_path=config)

    assert any(issue["kind"] == "email_route_wrong_origin" for issue in report["blockers"])


def test_public_tunnel_preflight_blocks_shadowed_email_route(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        f"""
  - service: http://127.0.0.1:8421
{EMAIL_ROUTE.rstrip()}
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - service: http_status:404
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(config_path=config)

    assert any(
        issue["kind"] == "email_route_shadowed" and issue["index"] == 0
        for issue in report["blockers"]
    )


def test_public_tunnel_preflight_restricts_custom_email_host(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        """
  - hostname: mail.example.com
    path: ^/api/email-inbound$
    service: http://127.0.0.1:8420
  - hostname: mail.example.com
    service: http://127.0.0.1:8420
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - service: http_status:404
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(
        config_path=config,
        email_host="mail.example.com",
        forbidden_hosts=(),
    )

    assert any(
        issue["kind"] == "email_host_unexpected_route" and issue["index"] == 1
        for issue in report["blockers"]
    )


def test_public_tunnel_preflight_blocks_wildcard_and_missing_404(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        f"""
{EMAIL_ROUTE.rstrip()}
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - hostname: '*.rajeshgo.li'
    service: http://127.0.0.1:8420
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(config_path=config)

    assert report["status"] == "blocked"
    kinds = {issue["kind"] for issue in report["blockers"]}
    assert "wildcard_hostname_present" in kinds
    assert "catch_all_not_404" in kinds


def test_public_tunnel_preflight_text_and_cli_fail_on_blockers(tmp_path, capsys):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        f"""
{EMAIL_ROUTE.rstrip()}
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8421
  - service: http_status:404
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(config_path=config)
    text = render_text_report(report)
    assert "Rust public tunnel preflight" in text
    assert "app_host_wrong_origin" in text

    rc = public_tunnel_preflight_main(
        ["--config", str(config), "--fail-on-blockers"]
    )
    captured = capsys.readouterr()
    assert rc == 1
    assert "status: blocked" in captured.out
