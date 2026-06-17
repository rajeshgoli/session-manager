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


def test_public_tunnel_preflight_accepts_protected_app_to_rust_service(tmp_path):
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

    assert report["status"] == "passed"
    assert report["summary"]["blockers"] == 0
    assert report["ingress"][0]["hostname"] == "sm-app.rajeshgo.li"
    assert report["ingress"][0]["service"] == "http://127.0.0.1:8420"


def test_public_tunnel_preflight_blocks_manual_8421_sidecar_target(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        """
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
        "index": 0,
    } in report["blockers"]


def test_public_tunnel_preflight_blocks_earlier_hostless_rule(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        """
  - service: http://127.0.0.1:8421
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - service: http_status:404
""".rstrip(),
    )

    report = build_public_tunnel_preflight_report(config_path=config)

    assert report["status"] == "blocked"
    assert any(
        issue["kind"] == "app_host_shadowed" and issue["index"] == 0
        for issue in report["blockers"]
    )


def test_public_tunnel_preflight_blocks_path_scoped_app_rule(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        """
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


def test_public_tunnel_preflight_blocks_legacy_public_host(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        """
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
        issue["kind"] == "forbidden_host_present" and issue["index"] == 1
        for issue in report["blockers"]
    )


def test_public_tunnel_preflight_blocks_wildcard_and_missing_404(tmp_path):
    config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(
        config,
        """
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
        """
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
