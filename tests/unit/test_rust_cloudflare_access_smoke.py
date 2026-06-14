import json
import urllib.error

from scripts.rust_migration.cloudflare_access_smoke import (
    build_smoke_report,
    main,
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


def _http_error(url, status, payload):
    body = json.dumps(payload).encode("utf-8")
    return urllib.error.HTTPError(
        url,
        status,
        "error",
        {"content-type": "application/json"},
        None,
    ).__class__(
        url,
        status,
        "error",
        {"content-type": "application/json"},
        _BytesHandle(body),
    )


class _BytesHandle:
    def __init__(self, body):
        self._body = body

    def read(self):
        return self._body

    def close(self):
        pass


def test_cloudflare_access_smoke_blocks_when_required_inputs_are_missing():
    report = build_smoke_report()

    assert report["status"] == "blocked"
    assert report["summary"]["skipped"] == len(report["checks"])
    assert [blocker["check_id"] for blocker in report["blockers"]] == [
        "mobile.bootstrap_requires_access",
        "mobile.bootstrap_with_access",
        "mobile.bootstrap_requires_public_edge",
        "mobile.sessions_require_sm_auth",
        "mobile.sessions_with_sm_auth",
        "mobile.app_artifact_metadata",
    ]
    assert {blocker["kind"] for blocker in report["blockers"]} == {
        "required_check_skipped"
    }
    assert all(check["status"] == "skipped" for check in report["checks"])


def test_cloudflare_access_smoke_blocks_when_access_assertion_is_missing():
    def fake_urlopen(request, timeout):
        path = request.full_url.removeprefix("http://127.0.0.1:8421")
        if path == "/client/bootstrap":
            raise _http_error(
                request.full_url,
                403,
                {"detail": "Cloudflare Access mobile app assertion is required"},
            )
        raise AssertionError(f"unexpected request path {path}")

    report = build_smoke_report(
        mobile_host="sm-app.example.com",
        public_edge_secret="edge-secret",
        timeout_seconds=2.5,
        urlopen=fake_urlopen,
    )

    assert report["status"] == "blocked"
    assert [blocker["check_id"] for blocker in report["blockers"]] == [
        "mobile.bootstrap_with_access",
        "mobile.bootstrap_requires_public_edge",
        "mobile.sessions_require_sm_auth",
        "mobile.sessions_with_sm_auth",
        "mobile.app_artifact_metadata",
    ]
    assert {blocker["kind"] for blocker in report["blockers"]} == {
        "required_check_skipped"
    }


def test_cloudflare_access_smoke_blocks_when_public_edge_secret_is_missing():
    def fake_urlopen(request, timeout):
        raise AssertionError(f"unexpected request to {request.full_url}")

    report = build_smoke_report(
        mobile_host="sm-app.example.com",
        mobile_access_jwt="mobile.jwt",
        bearer_token="smat_token",
        timeout_seconds=2.5,
        urlopen=fake_urlopen,
    )

    assert report["status"] == "blocked"
    assert [blocker["check_id"] for blocker in report["blockers"]] == [
        "mobile.bootstrap_requires_access",
        "mobile.bootstrap_with_access",
        "mobile.bootstrap_requires_public_edge",
        "mobile.sessions_require_sm_auth",
        "mobile.sessions_with_sm_auth",
        "mobile.app_artifact_metadata",
    ]
    assert {blocker["kind"] for blocker in report["blockers"]} == {
        "required_check_skipped"
    }
    assert {
        check["detail"]
        for check in report["checks"]
        if check["id"].startswith("mobile.")
    } == {"public edge secret was not supplied"}


def test_cloudflare_access_smoke_records_mobile_boundary_and_headers():
    seen = []

    def fake_urlopen(request, timeout):
        seen.append((request, timeout))
        path = request.full_url.removeprefix("http://127.0.0.1:8421")
        if path == "/client/bootstrap" and not request.get_header(
            "Cf-access-jwt-assertion"
        ):
            raise _http_error(
                request.full_url,
                403,
                {"detail": "Cloudflare Access mobile app assertion is required"},
            )
        if path == "/client/bootstrap" and not request.get_header(
            "X-sm-edge-signature"
        ):
            raise _http_error(
                request.full_url,
                403,
                {"detail": "Public edge assertion is required"},
            )
        if path == "/client/bootstrap":
            return FakeResponse(200, b'{"auth":{}}')
        if path == "/client/sessions" and not request.get_header("Authorization"):
            raise _http_error(
                request.full_url,
                401,
                {"detail": "Authentication required"},
            )
        if path == "/client/sessions":
            return FakeResponse(200, b'{"sessions":[]}')
        if path == "/apps/session-manager-android/meta.json" and request.get_header(
            "Authorization"
        ):
            return FakeResponse(200, b'{"artifact_hash":"deadbeef"}')
        raise AssertionError(f"unexpected request path {path}")

    report = build_smoke_report(
        mobile_host="sm-app.example.com",
        mobile_access_jwt="mobile.jwt",
        public_edge_secret="edge-secret",
        bearer_token="smat_token",
        timeout_seconds=2.5,
        urlopen=fake_urlopen,
    )

    assert report["status"] == "passed"
    assert report["blockers"] == []
    assert report["summary"]["blocked"] == 0
    by_id = {check["id"]: check for check in report["checks"]}
    assert by_id["mobile.bootstrap_requires_access"]["status"] == "passed"
    assert by_id["mobile.bootstrap_with_access"]["status"] == "passed"
    assert by_id["mobile.bootstrap_requires_public_edge"]["status"] == "passed"
    assert by_id["mobile.sessions_require_sm_auth"]["status"] == "passed"
    assert by_id["mobile.sessions_with_sm_auth"]["status"] == "passed"
    assert by_id["mobile.app_artifact_metadata"]["status"] == "passed"
    sessions_response = by_id["mobile.sessions_with_sm_auth"]["response"]
    assert sessions_response["json_redacted"] is True
    assert sessions_response["json_keys"] == ["sessions"]
    assert "json" not in sessions_response
    metadata_response = by_id["mobile.app_artifact_metadata"]["response"]
    assert metadata_response["json_redacted"] is True
    assert metadata_response["json_keys"] == ["artifact_hash"]
    assert "json" not in metadata_response

    bootstrap_with_access = seen[1][0]
    assert bootstrap_with_access.get_header("Host") == "sm-app.example.com"
    assert bootstrap_with_access.get_header("Cf-access-jwt-assertion") == "mobile.jwt"
    assert bootstrap_with_access.get_header("X-sm-edge-timestamp")
    assert bootstrap_with_access.get_header("X-sm-edge-nonce")
    assert bootstrap_with_access.get_header("X-sm-edge-signature")
    sessions_with_auth = [
        request
        for request, _timeout in seen
        if request.full_url.endswith("/client/sessions")
        and request.get_header("Authorization")
    ][0]
    assert sessions_with_auth.get_header("Authorization") == "Bearer smat_token"
    metadata_with_auth = [
        request
        for request, _timeout in seen
        if request.full_url.endswith("/apps/session-manager-android/meta.json")
    ][0]
    assert metadata_with_auth.get_header("Authorization") == "Bearer smat_token"
    assert {timeout for _request, timeout in seen} == {2.5}


def test_cloudflare_access_smoke_blocks_unexpected_status():
    def fake_urlopen(request, timeout=None):
        path = request.full_url.removeprefix("http://127.0.0.1:8421")
        if path == "/client/bootstrap" and not request.get_header(
            "Cf-access-jwt-assertion"
        ):
            return FakeResponse(200, b'{"auth":{}}')
        if path == "/client/bootstrap" and not request.get_header(
            "X-sm-edge-signature"
        ):
            raise _http_error(
                request.full_url,
                403,
                {"detail": "Public edge assertion is required"},
            )
        if path == "/client/bootstrap":
            return FakeResponse(200, b'{"auth":{}}')
        if path == "/client/sessions" and not request.get_header("Authorization"):
            raise _http_error(
                request.full_url,
                401,
                {"detail": "Authentication required"},
            )
        if path == "/client/sessions":
            return FakeResponse(200, b'{"sessions":[]}')
        if path == "/apps/session-manager-android/meta.json":
            return FakeResponse(200, b'{"artifact_hash":"deadbeef"}')
        raise AssertionError(f"unexpected request path {path}")

    report = build_smoke_report(
        mobile_host="sm-app.example.com",
        mobile_access_jwt="mobile.jwt",
        public_edge_secret="edge-secret",
        bearer_token="smat_token",
        timeout_seconds=1.0,
        urlopen=fake_urlopen,
    )

    assert report["status"] == "blocked"
    assert report["blockers"] == [
        {
            "check_id": "mobile.bootstrap_requires_access",
            "kind": "unexpected_status",
            "detail": "expected HTTP 403, got HTTP 200",
        }
    ]


def test_cloudflare_access_smoke_blocks_redirects():
    def fake_urlopen(request, timeout=None):
        path = request.full_url.removeprefix("http://127.0.0.1:8421")
        if path == "/client/bootstrap" and not request.get_header(
            "Cf-access-jwt-assertion"
        ):
            raise urllib.error.HTTPError(
                request.full_url,
                302,
                "redirect",
                {"content-type": "text/html", "location": "https://login.example.com"},
                _BytesHandle(b"<html>login</html>"),
            )
        if path == "/client/bootstrap" and not request.get_header(
            "X-sm-edge-signature"
        ):
            raise _http_error(
                request.full_url,
                403,
                {"detail": "Public edge assertion is required"},
            )
        if path == "/client/bootstrap":
            return FakeResponse(200, b'{"auth":{}}')
        if path == "/client/sessions" and not request.get_header("Authorization"):
            raise _http_error(
                request.full_url,
                401,
                {"detail": "Authentication required"},
            )
        if path == "/client/sessions":
            return FakeResponse(200, b'{"sessions":[]}')
        if path == "/apps/session-manager-android/meta.json":
            return FakeResponse(200, b'{"artifact_hash":"deadbeef"}')
        raise AssertionError(f"unexpected request path {path}")

    report = build_smoke_report(
        mobile_host="sm-app.example.com",
        mobile_access_jwt="mobile.jwt",
        public_edge_secret="edge-secret",
        bearer_token="smat_token",
        timeout_seconds=1.0,
        urlopen=fake_urlopen,
    )

    assert report["status"] == "blocked"
    assert report["blockers"] == [
        {
            "check_id": "mobile.bootstrap_requires_access",
            "kind": "unexpected_status",
            "detail": "expected HTTP 403, got HTTP 302",
        }
    ]


def test_cloudflare_access_smoke_blocks_bootstrap_without_contract_keys():
    def fake_urlopen(request, timeout=None):
        path = request.full_url.removeprefix("http://127.0.0.1:8421")
        if path == "/client/bootstrap" and not request.get_header(
            "Cf-access-jwt-assertion"
        ):
            raise _http_error(
                request.full_url,
                403,
                {"detail": "Cloudflare Access mobile app assertion is required"},
            )
        if path == "/client/bootstrap" and not request.get_header(
            "X-sm-edge-signature"
        ):
            raise _http_error(
                request.full_url,
                403,
                {"detail": "Public edge assertion is required"},
            )
        if path == "/client/bootstrap":
            return FakeResponse(200, b'{"detail":"wrong service"}')
        if path == "/client/sessions" and not request.get_header("Authorization"):
            raise _http_error(
                request.full_url,
                401,
                {"detail": "Authentication required"},
            )
        if path == "/client/sessions":
            return FakeResponse(200, b'{"sessions":[]}')
        if path == "/apps/session-manager-android/meta.json":
            return FakeResponse(200, b'{"artifact_hash":"deadbeef"}')
        raise AssertionError(f"unexpected request path {path}")

    report = build_smoke_report(
        mobile_host="sm-app.example.com",
        mobile_access_jwt="mobile.jwt",
        public_edge_secret="edge-secret",
        bearer_token="smat_token",
        timeout_seconds=1.0,
        urlopen=fake_urlopen,
    )

    assert report["status"] == "blocked"
    assert report["blockers"] == [
        {
            "check_id": "mobile.bootstrap_with_access",
            "kind": "unexpected_json",
            "detail": "missing expected JSON keys: auth",
        }
    ]


def test_cloudflare_access_smoke_blocks_login_html_success_responses():
    def fake_urlopen(request, timeout=None):
        if request.full_url.endswith("/client/bootstrap") and not request.get_header(
            "Cf-access-jwt-assertion"
        ):
            raise _http_error(
                request.full_url,
                403,
                {"detail": "Cloudflare Access mobile app assertion is required"},
            )
        if request.full_url.endswith("/client/bootstrap") and not request.get_header(
            "X-sm-edge-signature"
        ):
            raise _http_error(
                request.full_url,
                403,
                {"detail": "Public edge assertion is required"},
            )
        if request.full_url.endswith("/client/bootstrap"):
            return FakeResponse(
                200,
                b"<html>login</html>",
                headers={"content-type": "text/html"},
            )
        if request.full_url.endswith("/client/sessions") and not request.get_header(
            "Authorization"
        ):
            raise _http_error(
                request.full_url,
                401,
                {"detail": "Authentication required"},
            )
        if request.full_url.endswith("/client/sessions"):
            return FakeResponse(200, b'{"sessions":[]}')
        if request.full_url.endswith("/apps/session-manager-android/meta.json"):
            return FakeResponse(200, b'{"artifact_hash":"deadbeef"}')
        raise AssertionError("unexpected request")

    report = build_smoke_report(
        mobile_host="sm-app.example.com",
        mobile_access_jwt="mobile.jwt",
        public_edge_secret="edge-secret",
        bearer_token="smat_token",
        timeout_seconds=1.0,
        urlopen=fake_urlopen,
    )

    by_id = {check["id"]: check for check in report["checks"]}
    assert by_id["mobile.bootstrap_requires_access"]["status"] == "passed"
    assert by_id["mobile.bootstrap_with_access"]["status"] == "blocked"
    assert by_id["mobile.bootstrap_with_access"]["blocker_kind"] == "unexpected_json"
    assert report["blockers"] == [
        {
            "check_id": "mobile.bootstrap_with_access",
            "kind": "unexpected_json",
            "detail": "expected JSON dict, got non-matching response",
        }
    ]


def test_cloudflare_access_smoke_skips_browser_origin_access_checks():
    def fake_urlopen(request, timeout=None):
        raise AssertionError(f"unexpected request to {request.full_url}")

    report = build_smoke_report(
        browser_host="sm.example.com",
        browser_access_jwt="browser.jwt",
        urlopen=fake_urlopen,
    )

    by_id = {check["id"]: check for check in report["checks"]}
    assert by_id["browser.auth_session_requires_access"] == {
        "id": "browser.auth_session_requires_access",
        "description": "browser host denies auth-session without browser Access assertion",
        "status": "skipped",
        "detail": "browser Cloudflare Access is enforced at the edge, not by the Rust origin",
        "required": False,
    }
    assert by_id["browser.auth_session_with_access"] == {
        "id": "browser.auth_session_with_access",
        "description": "browser host reaches SM auth-session after browser Access proof",
        "status": "skipped",
        "detail": "browser Cloudflare Access is enforced at the edge, not by the Rust origin",
        "required": False,
    }
    assert report["status"] == "blocked"
    assert all(
        blocker["check_id"].startswith("mobile.") for blocker in report["blockers"]
    )


def test_cloudflare_access_smoke_cli_writes_json_and_fails_on_blockers(
    tmp_path, monkeypatch, capsys
):
    def fake_report(**_kwargs):
        return {
            "status": "blocked",
            "summary": {"passed": 0, "blocked": 1, "skipped": 0},
            "checks": [],
            "blockers": [
                {
                    "check_id": "mobile.bootstrap_requires_access",
                    "kind": "unexpected_status",
                    "detail": "expected HTTP 403, got HTTP 200",
                }
            ],
        }

    monkeypatch.setattr(
        "scripts.rust_migration.cloudflare_access_smoke.build_smoke_report",
        fake_report,
    )
    output = tmp_path / "cloudflare-smoke.json"

    exit_code = main(["--output", str(output), "--json", "--fail-on-blockers"])

    assert exit_code == 1
    assert json.loads(output.read_text(encoding="utf-8"))["status"] == "blocked"
    assert json.loads(capsys.readouterr().out)["status"] == "blocked"


def test_cloudflare_access_smoke_cli_resolves_secret_env_vars(monkeypatch):
    captured = {}

    def fake_report(**kwargs):
        captured.update(kwargs)
        return {
            "status": "passed",
            "summary": {"passed": 0, "blocked": 0, "skipped": 0},
            "checks": [],
            "blockers": [],
        }

    monkeypatch.setenv("CF_MOBILE_ACCESS_JWT", "mobile.jwt")
    monkeypatch.setenv("CF_BROWSER_ACCESS_JWT", "browser.jwt")
    monkeypatch.setenv("SM_PUBLIC_EDGE_SECRET", "edge-secret")
    monkeypatch.setenv("SM_DEVICE_BEARER_TOKEN", "smat_token")
    monkeypatch.setattr(
        "scripts.rust_migration.cloudflare_access_smoke.build_smoke_report",
        fake_report,
    )

    exit_code = main(
        [
            "--mobile-access-jwt-env",
            "CF_MOBILE_ACCESS_JWT",
            "--browser-access-jwt-env",
            "CF_BROWSER_ACCESS_JWT",
            "--public-edge-secret-env",
            "SM_PUBLIC_EDGE_SECRET",
            "--bearer-token-env",
            "SM_DEVICE_BEARER_TOKEN",
        ]
    )

    assert exit_code == 0
    assert captured["mobile_access_jwt"] == "mobile.jwt"
    assert captured["browser_access_jwt"] == "browser.jwt"
    assert captured["public_edge_secret"] == "edge-secret"
    assert captured["bearer_token"] == "smat_token"


def test_cloudflare_access_smoke_cli_resolves_secret_files(tmp_path, monkeypatch):
    captured = {}

    def fake_report(**kwargs):
        captured.update(kwargs)
        return {
            "status": "passed",
            "summary": {"passed": 0, "blocked": 0, "skipped": 0},
            "checks": [],
            "blockers": [],
        }

    cookie_file = tmp_path / "cookie.txt"
    cookie_file.write_text("session=abc\n", encoding="utf-8")
    monkeypatch.setattr(
        "scripts.rust_migration.cloudflare_access_smoke.build_smoke_report",
        fake_report,
    )

    exit_code = main(["--cookie-file", str(cookie_file)])

    assert exit_code == 0
    assert captured["cookie"] == "session=abc"
