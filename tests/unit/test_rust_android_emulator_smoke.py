import base64
import hashlib
import hmac
import io
import json
from argparse import Namespace
from pathlib import Path

import pytest

from scripts.rust_migration import android_emulator_smoke as smoke


def test_device_access_token_matches_server_signature_shape():
    issued = smoke.build_device_access_token(
        session_cookie_secret="secret",
        email="RAJESH@example.com",
        name="Rajesh",
        now=1000,
        expires_in_seconds=60,
    )

    assert issued["access_token"].startswith("smat_")
    payload_b64, signature = issued["access_token"].removeprefix("smat_").split(".", 1)
    expected_signature = base64.urlsafe_b64encode(
        hmac.new(b"secret", payload_b64.encode("ascii"), hashlib.sha256).digest()
    ).decode("ascii").rstrip("=")
    assert signature == expected_signature

    padded_payload = payload_b64 + "=" * ((4 - len(payload_b64) % 4) % 4)
    payload = json.loads(base64.urlsafe_b64decode(padded_payload.encode("ascii")))
    assert payload == {
        "email": "rajesh@example.com",
        "exp": 1060,
        "iat": 1000,
        "name": "Rajesh",
        "type": "device_access",
        "v": 1,
    }
    assert issued["expires_at"].startswith("1970-01-01T00:17:40")


def test_mobile_smoke_identity_resolves_single_interactive_user(monkeypatch):
    monkeypatch.setattr(
        smoke,
        "_load_runtime_config",
        lambda _path: {
            "auth": {"google": {"session_cookie_secret": "secret"}},
            "mobile_terminal": {
                "allowed_users": {
                    "rajesh": {
                        "email": "rajesh@example.com",
                        "interactive_shell_access": True,
                    }
                }
            },
        },
    )

    identity = smoke.load_mobile_smoke_identity(Path("config.yaml"))

    assert identity == {
        "user_id": "rajesh",
        "email": "rajesh@example.com",
        "name": "rajesh@example.com",
        "session_cookie_secret": "secret",
    }


def test_mobile_smoke_identity_requires_explicit_user_when_ambiguous(monkeypatch):
    monkeypatch.setattr(
        smoke,
        "_load_runtime_config",
        lambda _path: {
            "auth": {"google": {"session_cookie_secret": "secret"}},
            "mobile_terminal": {
                "allowed_users": {
                    "one": {"interactive_shell_access": True},
                    "two": {"interactive_shell_access": True},
                }
            },
        },
    )

    with pytest.raises(ValueError, match="pass --user-id"):
        smoke.load_mobile_smoke_identity(Path("config.yaml"))


def test_runtime_config_loads_local_session_secret(tmp_path):
    config_path = tmp_path / "config.yaml"
    config_path.write_text("auth:\n  google:\n    enabled: true\n", encoding="utf-8")
    env_dir = tmp_path / ".local" / "android-parity"
    env_dir.mkdir(parents=True)
    (env_dir / "values.env").write_text(
        "SESSION_COOKIE_SECRET=local-secret\n", encoding="utf-8"
    )

    config = smoke._load_runtime_config(config_path)

    assert config["auth"]["google"]["session_cookie_secret"] == "local-secret"


def test_smoke_summary_includes_android_report_counts():
    report = {
        "host_steps": [
            {"id": "host", "status": "passed"},
            {"id": "optional", "status": "skipped"},
        ],
        "android_report": {
            "summary": {
                "passed": 3,
                "skipped": 1,
                "blocked": 0,
            }
        },
    }

    assert smoke._summarize(report) == {
        "status": "passed",
        "passed": 4,
        "skipped": 2,
        "blocked": 0,
    }


def test_start_enrollment_listener_uses_adb_reverse_local_url(monkeypatch):
    captured = {}

    class FakePopen:
        stdout = None

        def __init__(self, command, **kwargs):
            captured["command"] = command
            captured["kwargs"] = kwargs

    monkeypatch.setattr(smoke, "_resolve_sm_binary", lambda path: Path("target/debug/sm"))
    monkeypatch.setattr(smoke.subprocess, "Popen", FakePopen)

    args = Namespace(
        sm_binary="target/debug/sm",
        config=Path("config.yaml"),
        enrollment_expires_minutes=15,
    )

    smoke._start_enrollment_listener(args, "rajesh", 19192)

    assert captured["command"] == [
        "target/debug/sm",
        "enroll-device",
        "--config",
        "config.yaml",
        "--user-id",
        "rajesh",
        "--expires-in-minutes",
        "15",
        "--listen",
        "127.0.0.1:19192",
        "--url-base",
        "http://127.0.0.1:19192",
        "--no-qr",
    ]



class _FakeResponse:
    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        return False

    def read(self):
        return b"{}"


def _report_with_enrolled_device(device_id):
    return {
        "inputs": {"user_id": "rajesh"},
        "host_steps": [],
        "android_report": {
            "steps": [
                {
                    "id": "enroll_device_certificate",
                    "status": "passed",
                    "detail": {"device_id": device_id},
                }
            ]
        },
    }


_ARGS = Namespace(local_server_url="http://127.0.0.1:8420/")
_TOKEN = {"access_token": "smat_payload.sig"}


def test_revoke_smoke_device_deletes_with_the_run_bearer(monkeypatch):
    requests = []

    def fake_urlopen(request, timeout):
        requests.append(request)
        return _FakeResponse()

    monkeypatch.setattr(smoke.urllib.request, "urlopen", fake_urlopen)
    report = _report_with_enrolled_device("android-abc")

    smoke._revoke_smoke_device(_ARGS, report, _TOKEN)

    [request] = requests
    assert request.get_method() == "DELETE"
    assert request.full_url == (
        "http://127.0.0.1:8420/client/mobile-terminal/devices/android-abc?user_id=rajesh"
    )
    assert request.get_header("Authorization") == "Bearer smat_payload.sig"
    assert report["host_steps"][-1]["id"] == "revoke_smoke_device"
    assert report["host_steps"][-1]["status"] == "passed"


def test_revoke_smoke_device_blocks_when_device_id_is_unknown(monkeypatch):
    monkeypatch.setattr(
        smoke.urllib.request,
        "urlopen",
        lambda *_a, **_k: pytest.fail("must not call the server without a device id"),
    )
    report = {"inputs": {}, "host_steps": [], "android_report": None}

    smoke._revoke_smoke_device(_ARGS, report, _TOKEN)

    assert report["host_steps"][-1]["status"] == "blocked"
    assert "sm list-devices" in report["host_steps"][-1]["detail"]


def test_revoke_smoke_device_blocks_when_server_refuses(monkeypatch):
    def refuse(request, timeout):
        raise smoke.urllib.error.HTTPError(
            request.full_url, 403, "Forbidden", {}, io.BytesIO(b'{"detail":"nope"}')
        )

    monkeypatch.setattr(smoke.urllib.request, "urlopen", refuse)
    report = _report_with_enrolled_device("android-abc")

    smoke._revoke_smoke_device(_ARGS, report, _TOKEN)

    assert report["host_steps"][-1]["status"] == "blocked"
    assert "HTTP 403" in report["host_steps"][-1]["detail"]


def test_revoke_smoke_device_prefers_the_listener_device_id(monkeypatch):
    requests = []
    monkeypatch.setattr(
        smoke.urllib.request,
        "urlopen",
        lambda request, timeout: requests.append(request) or _FakeResponse(),
    )
    report = {"inputs": {"user_id": "rajesh"}, "host_steps": [], "android_report": None}

    smoke._revoke_smoke_device(_ARGS, report, _TOKEN, "android-from-listener")

    assert requests[0].full_url.endswith("/devices/android-from-listener?user_id=rajesh")
    assert report["host_steps"][-1]["status"] == "passed"


def test_enrolled_device_id_parses_listener_output():
    process = smoke.subprocess.Popen(
        [
            "printf",
            "Enrollment URL: http://x\nEnrolled device: android-abc (user_id=rajesh)\n",
        ],
        stdout=smoke.subprocess.PIPE,
        text=True,
    )
    process.wait()

    assert smoke._enrolled_device_id(process) == "android-abc"
