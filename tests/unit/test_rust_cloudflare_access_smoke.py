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
        "mobile.bootstrap_with_access",
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
        if check["id"].startswith("mobile.") and check["status"] == "skipped"
    } == {"public edge secret was not supplied"}
    by_id = {check["id"]: check for check in report["checks"]}
    assert by_id["mobile.bootstrap_requires_access"]["status"] == "passed"
    assert by_id["mobile.bootstrap_requires_public_edge"]["status"] == "passed"


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


def test_cloudflare_access_smoke_public_mtls_records_public_boundary(tmp_path):
    cert_file = tmp_path / "client.cert.pem"
    key_file = tmp_path / "client.key.pem"
    cert_file.write_text("cert", encoding="utf-8")
    key_file.write_text("key", encoding="utf-8")
    seen = []

    def fake_urlopen(request, timeout):
        seen.append((request.full_url, request.get_header("Authorization"), timeout))
        path = request.full_url.removeprefix("https://sm-app.example.com")
        if len(seen) == 1 and path == "/client/bootstrap":
            raise urllib.error.HTTPError(
                request.full_url,
                403,
                "forbidden",
                {"content-type": "text/html"},
                _BytesHandle(b"<html>Cloudflare Access</html>"),
            )
        if path == "/client/bootstrap":
            return FakeResponse(200, b'{"auth":{},"external_access":{}}')
        if path == "/client/sessions":
            raise _http_error(
                request.full_url,
                401,
                {"detail": "Authentication required"},
            )
        if path == "/apps/session-manager-android/meta.json":
            return FakeResponse(
                200,
                b'{"artifact_hash":"deadbeef","version_code":1033}',
            )
        raise AssertionError(f"unexpected request path {path}")

    report = build_smoke_report(
        mode="public-mtls",
        public_base_url="https://sm-app.example.com",
        client_cert_file=cert_file,
        client_key_file=key_file,
        timeout_seconds=2.5,
        urlopen=fake_urlopen,
    )

    assert report["status"] == "passed"
    assert report["summary"] == {"passed": 4, "blocked": 0, "skipped": 1}
    assert report["blockers"] == []
    assert report["inputs"]["mode"] == "public-mtls"
    assert report["inputs"]["client_cert_file_supplied"] is True
    assert report["inputs"]["client_key_file_supplied"] is True
    assert report["inputs"]["ephemeral_client_cert_generated"] is False
    by_id = {check["id"]: check for check in report["checks"]}
    assert by_id["public_mtls.bootstrap_requires_client_cert"]["status"] == "passed"
    assert by_id["public_mtls.bootstrap_with_client_cert"]["status"] == "passed"
    assert by_id["public_mtls.sessions_require_sm_auth"]["status"] == "passed"
    assert by_id["public_mtls.app_artifact_metadata"]["status"] == "passed"
    assert by_id["public_mtls.sessions_with_sm_auth"] == {
        "id": "public_mtls.sessions_with_sm_auth",
        "description": "public mobile API returns sessions after mTLS and SM auth",
        "status": "skipped",
        "detail": "SM bearer token or cookie was not supplied",
        "required": False,
    }
    assert by_id["public_mtls.bootstrap_with_client_cert"]["response"][
        "json_redacted"
    ] is True
    assert by_id["public_mtls.bootstrap_with_client_cert"]["response"]["json_keys"] == [
        "auth",
        "external_access",
    ]
    assert "json" not in by_id["public_mtls.bootstrap_with_client_cert"]["response"]
    assert seen == [
        ("https://sm-app.example.com/client/bootstrap", None, 2.5),
        ("https://sm-app.example.com/client/bootstrap", None, 2.5),
        ("https://sm-app.example.com/client/sessions", None, 2.5),
        (
            "https://sm-app.example.com/apps/session-manager-android/meta.json",
            None,
            2.5,
        ),
    ]


def test_cloudflare_access_smoke_public_mtls_blocks_missing_cert_source():
    report = build_smoke_report(
        mode="public-mtls",
        public_base_url="https://sm-app.example.com",
    )

    assert report["status"] == "blocked"
    assert report["blockers"] == [
        {
            "check_id": "public_mtls.client_certificate_available",
            "kind": "client_cert_missing",
            "detail": (
                "supply client cert/key files or client cert common name plus device CA "
                "cert/key files"
            ),
        }
    ]


def test_cloudflare_access_smoke_public_mtls_uses_generated_ephemeral_cert(
    tmp_path, monkeypatch
):
    ca_cert = tmp_path / "ca.cert.pem"
    ca_key = tmp_path / "ca.key.pem"
    client_cert = tmp_path / "generated.cert.pem"
    client_key = tmp_path / "generated.key.pem"
    ca_cert.write_text("ca cert", encoding="utf-8")
    ca_key.write_text("ca key", encoding="utf-8")
    client_cert.write_text("client cert", encoding="utf-8")
    client_key.write_text("client key", encoding="utf-8")
    captured = {}

    def fake_generate(**kwargs):
        captured.update(kwargs)
        return {
            "cert_file": client_cert,
            "key_file": client_key,
            "_tempdir": object(),
        }

    def fake_urlopen(request, timeout):
        path = request.full_url.removeprefix("https://sm-app.example.com")
        if path == "/client/bootstrap" and not hasattr(fake_urlopen, "denied"):
            fake_urlopen.denied = True
            raise urllib.error.HTTPError(
                request.full_url,
                403,
                "forbidden",
                {"content-type": "text/html"},
                _BytesHandle(b"<html>Cloudflare Access</html>"),
            )
        if path == "/client/bootstrap":
            return FakeResponse(200, b'{"auth":{}}')
        if path == "/client/sessions":
            raise _http_error(
                request.full_url,
                401,
                {"detail": "Authentication required"},
            )
        if path == "/apps/session-manager-android/meta.json":
            return FakeResponse(200, b'{"artifact_hash":"deadbeef"}')
        raise AssertionError(f"unexpected request path {path}")

    monkeypatch.setattr(
        "scripts.rust_migration.cloudflare_access_smoke._generate_ephemeral_client_cert",
        fake_generate,
    )

    report = build_smoke_report(
        mode="public-mtls",
        public_base_url="https://sm-app.example.com",
        client_cert_common_name="android-c6c90c26d0d90faf",
        device_ca_cert_file=ca_cert,
        device_ca_key_file=ca_key,
        urlopen=fake_urlopen,
    )

    assert report["status"] == "passed"
    assert report["inputs"]["ephemeral_client_cert_generated"] is True
    assert report["inputs"]["client_cert_common_name"] == "android-c6c90c26d0d90faf"
    assert captured == {
        "common_name": "android-c6c90c26d0d90faf",
        "device_ca_cert_file": ca_cert,
        "device_ca_key_file": ca_key,
    }


def test_cloudflare_access_smoke_cli_passes_public_mtls_args(tmp_path, monkeypatch):
    captured = {}

    def fake_report(**kwargs):
        captured.update(kwargs)
        return {
            "status": "passed",
            "summary": {"passed": 4, "blocked": 0, "skipped": 1},
            "checks": [],
            "blockers": [],
        }

    monkeypatch.setattr(
        "scripts.rust_migration.cloudflare_access_smoke.build_smoke_report",
        fake_report,
    )

    exit_code = main(
        [
            "--mode",
            "public-mtls",
            "--public-base-url",
            "https://sm-app.example.com",
            "--client-cert-common-name",
            "android-c6c90c26d0d90faf",
            "--device-ca-cert-file",
            str(tmp_path / "ca.cert.pem"),
            "--device-ca-key-file",
            str(tmp_path / "ca.key.pem"),
        ]
    )

    assert exit_code == 0
    assert captured["mode"] == "public-mtls"
    assert captured["public_base_url"] == "https://sm-app.example.com"
    assert captured["client_cert_common_name"] == "android-c6c90c26d0d90faf"
    assert captured["device_ca_cert_file"] == tmp_path / "ca.cert.pem"
    assert captured["device_ca_key_file"] == tmp_path / "ca.key.pem"
