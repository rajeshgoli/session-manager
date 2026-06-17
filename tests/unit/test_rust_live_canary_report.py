import json
import subprocess
import urllib.error
from pathlib import Path

from scripts.rust_migration.live_canary_report import (
    build_live_canary_report,
    main as live_canary_main,
    render_text_report,
)


class FakeResponse:
    def __init__(self, status, body, headers=None):
        self.status = status
        self._body = body
        self.headers = headers or {"content-type": "application/json"}

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def getcode(self):
        return self.status

    def read(self):
        return self._body


class _BytesHandle:
    def __init__(self, body):
        self._body = body

    def read(self):
        return self._body

    def close(self):
        pass


def _http_error(url, status, body, content_type="application/json"):
    if isinstance(body, dict):
        payload = json.dumps(body).encode("utf-8")
    elif isinstance(body, str):
        payload = body.encode("utf-8")
    else:
        payload = body
    return urllib.error.HTTPError(
        url,
        status,
        "error",
        {"content-type": content_type},
        _BytesHandle(payload),
    )


def _write_tunnel_config(path: Path, service="http://127.0.0.1:8420") -> None:
    path.write_text(
        f"""
tunnel: test-tunnel
credentials-file: /tmp/test-tunnel.json
ingress:
  - hostname: sm-app.rajeshgo.li
    service: {service}
  - service: http_status:404
""".lstrip(),
        encoding="utf-8",
    )


def _command_runner(command, text, capture_output, check, timeout):
    assert text is True
    assert capture_output is True
    assert check is False
    assert timeout == 2.5
    if command[0] == "launchctl":
        return subprocess.CompletedProcess(command, 0, "state = running\n", "")
    if command[-1] == "status":
        return subprocess.CompletedProcess(command, 0, "007c6275 idle sm-maintainer\n", "")
    raise AssertionError(f"unexpected command {command}")


def _urlopen(request, timeout):
    assert timeout == 2.5
    url = request.full_url
    if url.endswith("/health") and url.startswith("http://127.0.0.1:8420"):
        return FakeResponse(200, b'{"status":"healthy"}')
    if url.endswith("/health/detailed"):
        return FakeResponse(200, b'{"status":"healthy","checks":{}}')
    if url.endswith("/client/bootstrap"):
        return FakeResponse(
            200,
            json.dumps(
                {
                    "auth": {},
                    "external_access": {
                        "public_http_host": "sm-app.rajeshgo.li",
                        "mobile_terminal_ws_url": "wss://sm-app.rajeshgo.li/client/terminal",
                    },
                }
            ).encode("utf-8"),
        )
    if url.endswith("/client/sessions"):
        return FakeResponse(200, b'{"sessions":[]}')
    if url.endswith("/client/analytics/summary"):
        return FakeResponse(200, b'{"generated_at":"now","kpis":{}}')
    if url == "https://sm-app.rajeshgo.li/health":
        raise _http_error(
            url,
            403,
            "<title>Error - Cloudflare Access</title>",
            content_type="text/html",
        )
    if url == "https://sm.rajeshgo.li/health":
        raise _http_error(url, 404, b"", content_type="text/plain")
    raise AssertionError(f"unexpected URL {url}")


def test_live_canary_report_passes_with_expected_cutover_shape(tmp_path):
    tunnel_config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(tunnel_config)

    report = build_live_canary_report(
        tunnel_config=tunnel_config,
        timeout_seconds=2.5,
        command_runner=_command_runner,
        urlopen=_urlopen,
    )

    assert report["status"] == "passed"
    assert report["blockers"] == []
    assert report["summary"]["blocked"] == 0
    by_id = {check["id"]: check for check in report["checks"]}
    assert by_id["launchd.rust_service_running"]["status"] == "passed"
    assert by_id["tunnel.public_preflight"]["status"] == "passed"
    assert by_id["public.sm_app_requires_access"]["status"] == "passed"
    assert by_id["public.legacy_host_absent"]["status"] == "passed"
    assert by_id["cloudflare.smoke_report"]["status"] == "skipped"


def test_live_canary_report_blocks_when_app_public_probe_reaches_origin(tmp_path):
    tunnel_config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(tunnel_config)

    def urlopen_origin_leak(request, timeout):
        if request.full_url == "https://sm-app.rajeshgo.li/health":
            return FakeResponse(200, b'{"status":"healthy"}')
        return _urlopen(request, timeout)

    report = build_live_canary_report(
        tunnel_config=tunnel_config,
        timeout_seconds=2.5,
        command_runner=_command_runner,
        urlopen=urlopen_origin_leak,
    )

    assert report["status"] == "blocked"
    assert {
        "check_id": "public.sm_app_requires_access",
        "kind": "status_mismatch",
        "detail": "expected HTTP 403, got HTTP 200",
    } in report["blockers"]


def test_live_canary_report_accepts_prefixed_mobile_terminal_url(tmp_path):
    tunnel_config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(tunnel_config)

    def urlopen_prefixed_terminal(request, timeout):
        if request.full_url.endswith("/client/bootstrap"):
            return FakeResponse(
                200,
                json.dumps(
                    {
                        "auth": {},
                        "external_access": {
                            "public_http_host": "sm-app.rajeshgo.li",
                            "mobile_terminal_ws_url": (
                                "wss://sm-app.rajeshgo.li/app/client/terminal"
                            ),
                        },
                    }
                ).encode("utf-8"),
            )
        return _urlopen(request, timeout)

    report = build_live_canary_report(
        tunnel_config=tunnel_config,
        timeout_seconds=2.5,
        command_runner=_command_runner,
        urlopen=urlopen_prefixed_terminal,
    )

    assert report["status"] == "passed"


def test_live_canary_report_blocks_invalid_mobile_terminal_url(tmp_path):
    tunnel_config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(tunnel_config)

    def urlopen_bad_terminal(request, timeout):
        if request.full_url.endswith("/client/bootstrap"):
            return FakeResponse(
                200,
                json.dumps(
                    {
                        "auth": {},
                        "external_access": {
                            "public_http_host": "sm-app.rajeshgo.li",
                            "mobile_terminal_ws_url": (
                                "wss://sm-app.rajeshgo.li/app/not-terminal"
                            ),
                        },
                    }
                ).encode("utf-8"),
            )
        return _urlopen(request, timeout)

    report = build_live_canary_report(
        tunnel_config=tunnel_config,
        timeout_seconds=2.5,
        command_runner=_command_runner,
        urlopen=urlopen_bad_terminal,
    )

    assert report["status"] == "blocked"
    assert any(
        blocker["check_id"] == "local.client_bootstrap"
        and blocker["kind"] == "json_value_mismatch"
        for blocker in report["blockers"]
    )


def test_live_canary_report_accepts_cloudflare_1010_denials(tmp_path):
    tunnel_config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(tunnel_config)

    def urlopen_cloudflare_1010(request, timeout):
        if request.full_url in {
            "https://sm-app.rajeshgo.li/health",
            "https://sm.rajeshgo.li/health",
        }:
            raise _http_error(
                request.full_url,
                403,
                "error code: 1010",
                content_type="text/plain",
            )
        return _urlopen(request, timeout)

    report = build_live_canary_report(
        tunnel_config=tunnel_config,
        timeout_seconds=2.5,
        command_runner=_command_runner,
        urlopen=urlopen_cloudflare_1010,
    )

    assert report["status"] == "passed"
    by_id = {check["id"]: check for check in report["checks"]}
    assert by_id["public.sm_app_requires_access"]["status"] == "passed"
    assert by_id["public.legacy_host_absent"]["status"] == "passed"


def test_live_canary_report_blocks_when_tunnel_targets_sidecar(tmp_path):
    tunnel_config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(tunnel_config, service="http://127.0.0.1:8421")

    report = build_live_canary_report(
        tunnel_config=tunnel_config,
        timeout_seconds=2.5,
        command_runner=_command_runner,
        urlopen=_urlopen,
    )

    assert report["status"] == "blocked"
    assert any(
        blocker["check_id"] == "tunnel.public_preflight"
        and blocker["kind"] == "tunnel_preflight_blocked"
        for blocker in report["blockers"]
    )


def test_live_canary_report_uses_configured_legacy_host_in_tunnel_preflight(tmp_path):
    tunnel_config = tmp_path / "cloudflared.yml"
    tunnel_config.write_text(
        """
tunnel: test-tunnel
credentials-file: /tmp/test-tunnel.json
ingress:
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - hostname: old.example.com
    service: http://127.0.0.1:8420
  - service: http_status:404
""".lstrip(),
        encoding="utf-8",
    )

    def urlopen_custom_legacy(request, timeout):
        if request.full_url == "https://old.example.com/health":
            raise _http_error(
                request.full_url,
                403,
                "error code: 1010",
                content_type="text/plain",
            )
        return _urlopen(request, timeout)

    report = build_live_canary_report(
        tunnel_config=tunnel_config,
        legacy_host="old.example.com",
        timeout_seconds=2.5,
        command_runner=_command_runner,
        urlopen=urlopen_custom_legacy,
    )

    assert report["status"] == "blocked"
    assert any(
        blocker["check_id"] == "tunnel.public_preflight"
        and blocker["kind"] == "tunnel_preflight_blocked"
        for blocker in report["blockers"]
    )


def test_live_canary_report_blocks_command_timeouts(tmp_path):
    tunnel_config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(tunnel_config)

    def command_timeout(command, text, capture_output, check, timeout):
        if command[-1] == "status":
            raise subprocess.TimeoutExpired(command, timeout)
        return _command_runner(command, text, capture_output, check, timeout)

    report = build_live_canary_report(
        tunnel_config=tunnel_config,
        timeout_seconds=2.5,
        command_runner=command_timeout,
        urlopen=_urlopen,
    )

    assert report["status"] == "blocked"
    assert {
        "check_id": "cli.status",
        "kind": "command_timeout",
        "detail": "command timed out after 2.5 seconds",
    } in report["blockers"]


def test_live_canary_report_blocks_supplied_failed_smoke_report(tmp_path):
    tunnel_config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(tunnel_config)
    smoke_report = tmp_path / "smoke.json"
    smoke_report.write_text(
        json.dumps({"status": "blocked", "summary": {"blocked": 1}, "blockers": [{"x": 1}]}),
        encoding="utf-8",
    )

    report = build_live_canary_report(
        tunnel_config=tunnel_config,
        cloudflare_smoke_report=smoke_report,
        timeout_seconds=2.5,
        command_runner=_command_runner,
        urlopen=_urlopen,
    )

    assert report["status"] == "blocked"
    assert any(
        blocker["check_id"] == "cloudflare.smoke_report"
        and blocker["kind"] == "smoke_report_blocked"
        for blocker in report["blockers"]
    )


def test_live_canary_report_text_cli_and_output_file(tmp_path, capsys):
    tunnel_config = tmp_path / "cloudflared.yml"
    _write_tunnel_config(tunnel_config, service="http://127.0.0.1:8421")
    output = tmp_path / "report.json"

    report = build_live_canary_report(
        tunnel_config=tunnel_config,
        timeout_seconds=2.5,
        command_runner=_command_runner,
        urlopen=_urlopen,
    )
    text = render_text_report(report)
    assert "Rust live canary report" in text
    assert "tunnel.public_preflight" in text

    rc = live_canary_main(
        [
            "--tunnel-config",
            str(tunnel_config),
            "--output",
            str(output),
            "--fail-on-blockers",
        ],
        command_runner=_command_runner,
        urlopen=_urlopen,
    )
    captured = capsys.readouterr()
    assert rc == 1
    assert "status: blocked" in captured.out
    assert json.loads(output.read_text(encoding="utf-8"))["status"] == "blocked"
