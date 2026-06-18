import base64
import hashlib
import hmac
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
