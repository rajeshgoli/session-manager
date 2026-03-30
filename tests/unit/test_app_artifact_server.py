import hashlib
import json
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import MagicMock

from fastapi.testclient import TestClient

from src.server import create_app, _issue_device_access_token


def _artifact_config(root_dir: str) -> dict:
    return {
        "paths": {
            "app_artifacts_dir": root_dir,
        },
        "auth": {
            "google": {
                "enabled": True,
                "public_host": "sm.rajeshgo.li",
                "client_id": "web-client-id",
                "android_client_id": "android-client-id",
                "client_secret": "web-client-secret",
                "redirect_uri": "https://sm.rajeshgo.li/auth/google/callback",
                "allowlist_emails": ["rajeshgoli@gmail.com"],
                "session_cookie_secret": "test-session-secret",
            }
        },
    }


def _session_manager() -> MagicMock:
    manager = MagicMock()
    manager.list_sessions.return_value = []
    manager.sessions = {}
    manager.get_effective_session_name.side_effect = lambda session: getattr(session, "friendly_name", None) or getattr(session, "name", "")
    manager.get_session_aliases.return_value = []
    manager.list_adoption_proposals.return_value = []
    return manager


def test_deploy_upload_writes_artifact_and_metadata_for_local_bypass():
    with TemporaryDirectory() as temp_dir:
        client = TestClient(create_app(session_manager=_session_manager(), config=_artifact_config(temp_dir)))

        response = client.post(
            "/deploy/session-manager-android",
            files={"file": ("app-debug.apk", b"apk-bytes", "application/vnd.android.package-archive")},
            data={"version_code": "7", "version_name": "0.1.7"},
        )

        assert response.status_code == 200
        payload = response.json()
        artifact_hash = hashlib.sha256(b"apk-bytes").hexdigest()[:8]
        assert payload == {
            "ok": True,
            "app": "session-manager-android",
            "size_bytes": len(b"apk-bytes"),
            "download_url": "/apps/session-manager-android/latest.apk",
            "artifact_hash": artifact_hash,
        }

        app_dir = Path(temp_dir) / "session-manager-android"
        assert (app_dir / "latest.apk").read_bytes() == b"apk-bytes"
        assert (app_dir / f"{artifact_hash}.apk").read_bytes() == b"apk-bytes"

        metadata = json.loads((app_dir / "meta.json").read_text())
        assert metadata["artifact_hash"] == artifact_hash
        assert metadata["size_bytes"] == len(b"apk-bytes")
        assert metadata["version_code"] == 7
        assert metadata["version_name"] == "0.1.7"
        assert metadata["uploaded_by"] == "local_bypass"


def test_deploy_requires_auth_for_external_requests():
    with TemporaryDirectory() as temp_dir:
        client = TestClient(
            create_app(session_manager=_session_manager(), config=_artifact_config(temp_dir)),
            base_url="https://sm.rajeshgo.li",
        )

        response = client.post(
            "/deploy/session-manager-android",
            files={"file": ("app-debug.apk", b"apk-bytes", "application/vnd.android.package-archive")},
        )

        assert response.status_code == 401
        assert response.json()["detail"] == "Authentication required"


def test_deploy_accepts_external_device_bearer_and_public_downloads_work():
    with TemporaryDirectory() as temp_dir:
        config = _artifact_config(temp_dir)
        issued = _issue_device_access_token(config, email="rajeshgoli@gmail.com", name="Rajesh Goli")
        assert issued is not None

        client = TestClient(
            create_app(session_manager=_session_manager(), config=config),
            base_url="https://sm.rajeshgo.li",
        )

        upload_response = client.post(
            "/deploy/session-manager-android",
            headers={"Authorization": f"Bearer {issued['access_token']}"},
            files={"file": ("app-debug.apk", b"public-apk", "application/vnd.android.package-archive")},
            data={"version_name": "0.2.0"},
        )
        assert upload_response.status_code == 200
        artifact_hash = hashlib.sha256(b"public-apk").hexdigest()[:8]

        latest_response = client.get("/apps/session-manager-android/latest.apk", follow_redirects=False)
        assert latest_response.status_code == 302
        assert latest_response.headers["location"] == f"/apps/session-manager-android/{artifact_hash}.apk"
        assert latest_response.headers["cache-control"] == "no-cache"

        hashed_response = client.get(f"/apps/session-manager-android/{artifact_hash}.apk")
        assert hashed_response.status_code == 200
        assert hashed_response.content == b"public-apk"
        assert hashed_response.headers["cache-control"] == "public, max-age=31536000, immutable"

        metadata_response = client.get("/apps/session-manager-android/meta.json")
        assert metadata_response.status_code == 200
        assert metadata_response.json()["artifact_hash"] == artifact_hash
        assert metadata_response.json()["uploaded_by"] == "rajeshgoli@gmail.com"
        assert metadata_response.json()["version_name"] == "0.2.0"

        legacy_response = client.get("/apk", follow_redirects=False)
        assert legacy_response.status_code == 302
        assert legacy_response.headers["location"] == "/apps/session-manager-android/latest.apk"
