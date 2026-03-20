from unittest.mock import MagicMock
import re

from fastapi.testclient import TestClient

from src.models import Session, SessionStatus
from src.server import create_app


def _auth_config() -> dict:
    return {
        "auth": {
            "google": {
                "enabled": True,
                "public_host": "sm.rajeshgo.li",
                "client_id": "web-client-id",
                "client_secret": "web-client-secret",
                "redirect_uri": "https://sm.rajeshgo.li/auth/google/callback",
                "allowlist_emails": ["rajeshgoli@gmail.com"],
                "session_cookie_secret": "test-session-secret",
            }
        }
    }


def _misconfigured_auth_config() -> dict:
    return {
        "auth": {
            "google": {
                "enabled": True,
                "public_host": "sm.rajeshgo.li",
                "client_id": "web-client-id",
                # client_secret intentionally missing
                "redirect_uri": "https://sm.rajeshgo.li/auth/google/callback",
                "allowlist_emails": ["rajeshgoli@gmail.com"],
                "session_cookie_secret": "test-session-secret",
            }
        }
    }


def _session() -> Session:
    return Session(
        id="abc12345",
        name="abc12345",
        working_dir="/tmp/project",
        tmux_session="claude-abc12345",
        status=SessionStatus.IDLE,
        provider="claude",
        log_file="/tmp/abc12345.log",
    )


def _session_manager() -> MagicMock:
    session = _session()
    manager = MagicMock()
    manager.sessions = {session.id: session}
    manager.list_sessions.return_value = [session]
    manager.get_session.side_effect = lambda session_id: manager.sessions.get(session_id)
    manager.get_effective_session_name.side_effect = lambda current_session: current_session.friendly_name or current_session.name
    manager.get_session_aliases.return_value = []
    manager.list_adoption_proposals.return_value = []
    return manager


def test_external_sessions_requires_google_auth():
    client = TestClient(
        create_app(session_manager=_session_manager(), config=_auth_config()),
        base_url="https://sm.rajeshgo.li",
    )

    response = client.get("/sessions")

    assert response.status_code == 401
    assert response.json()["detail"] == "Authentication required"
    assert response.json()["login_url"] == "/auth/google/login?next=%2Fsessions"


def test_external_watch_redirects_to_google_login():
    client = TestClient(
        create_app(session_manager=_session_manager(), config=_auth_config()),
        base_url="https://sm.rajeshgo.li",
    )

    response = client.get("/watch", follow_redirects=False)

    assert response.status_code == 302
    assert response.headers["location"] == "/auth/google/login?next=%2Fwatch"


def test_external_root_redirects_into_watch_flow():
    client = TestClient(
        create_app(session_manager=_session_manager(), config=_auth_config()),
        base_url="https://sm.rajeshgo.li",
    )

    response = client.get("/", follow_redirects=False)

    assert response.status_code == 302
    assert response.headers["location"] == "/watch/"


def test_external_sessions_ignore_forwarded_host_spoof():
    client = TestClient(
        create_app(session_manager=_session_manager(), config=_auth_config()),
        base_url="https://sm.rajeshgo.li",
    )

    response = client.get("/sessions", headers={"x-forwarded-host": "localhost"})

    assert response.status_code == 401


def test_local_loopback_bypasses_google_auth():
    client = TestClient(create_app(session_manager=_session_manager(), config=_auth_config()))

    response = client.get("/sessions")
    root_response = client.get("/")

    assert response.status_code == 200
    assert response.json()["sessions"][0]["id"] == "abc12345"
    assert root_response.status_code == 200
    assert root_response.json() == {"status": "ok", "service": "session-manager"}


def test_watch_html_is_not_cached_but_hashed_assets_remain_static():
    client = TestClient(create_app(session_manager=_session_manager(), config=_auth_config()))

    html_response = client.get("/watch/")

    assert html_response.status_code == 200
    assert html_response.headers["cache-control"] == "no-store, max-age=0, must-revalidate"
    assert html_response.headers["pragma"] == "no-cache"

    match = re.search(r"/watch/assets/[^\"']+\.js", html_response.text)
    assert match is not None

    asset_response = client.get(match.group(0))

    assert asset_response.status_code == 200
    assert "no-store" not in asset_response.headers.get("cache-control", "")

    etag = html_response.headers.get("etag")
    assert etag

    revalidated_response = client.get("/watch/", headers={"if-none-match": etag})

    assert revalidated_response.status_code == 304
    assert revalidated_response.headers["cache-control"] == "no-store, max-age=0, must-revalidate"
    assert revalidated_response.headers["pragma"] == "no-cache"


def test_external_requests_fail_closed_when_google_auth_is_misconfigured():
    client = TestClient(
        create_app(session_manager=_session_manager(), config=_misconfigured_auth_config()),
        base_url="https://sm.rajeshgo.li",
    )

    sessions_response = client.get("/sessions")
    watch_response = client.get("/watch")
    health_response = client.get("/health")
    auth_session_response = client.get("/auth/session")

    assert sessions_response.status_code == 503
    assert sessions_response.json()["detail"] == "Google auth is enabled but incomplete"
    assert watch_response.status_code == 503
    assert watch_response.json()["detail"] == "Google auth is enabled but incomplete"
    assert health_response.status_code == 200
    assert health_response.json() == {"status": "healthy"}
    assert auth_session_response.status_code == 200
    assert auth_session_response.json() == {
        "enabled": True,
        "authenticated": False,
        "bypass": False,
        "email": None,
        "name": None,
        "error": "misconfigured",
    }


def test_local_auth_session_reports_bypass_when_google_auth_is_misconfigured():
    client = TestClient(create_app(session_manager=_session_manager(), config=_misconfigured_auth_config()))

    response = client.get("/auth/session")

    assert response.status_code == 200
    assert response.json() == {
        "enabled": True,
        "authenticated": True,
        "bypass": True,
        "email": None,
        "name": None,
    }


def test_logout_redirects_cleanly_when_google_auth_is_misconfigured():
    client = TestClient(
        create_app(session_manager=_session_manager(), config=_misconfigured_auth_config()),
        base_url="https://sm.rajeshgo.li",
    )

    response = client.get("/auth/logout", follow_redirects=False)

    assert response.status_code == 302
    assert response.headers["location"] == "/logged-out"
    assert "sm_auth=\"\";" in response.headers["set-cookie"]

    landing_response = client.get("/logged-out")
    assert landing_response.status_code == 200
    assert "Signed out" in landing_response.text


def test_logged_out_page_hides_google_login_when_auth_is_incomplete():
    client = TestClient(
        create_app(session_manager=_session_manager(), config=_misconfigured_auth_config()),
        base_url="https://sm.rajeshgo.li",
    )

    response = client.get("/logged-out")

    assert response.status_code == 200
    assert "Google sign-in is not available on this deployment right now." in response.text
    assert "/auth/google/login" not in response.text


def test_google_callback_authenticates_allowlisted_email(monkeypatch):
    monkeypatch.setattr("src.server.secrets.token_urlsafe", lambda _: "oauth-state-123")

    async def fake_exchange_google_code(client_id: str, client_secret: str, redirect_uri: str, code: str) -> dict:
        assert client_id == "web-client-id"
        assert client_secret == "web-client-secret"
        assert redirect_uri == "https://sm.rajeshgo.li/auth/google/callback"
        assert code == "oauth-code"
        return {"access_token": "token-123"}

    async def fake_fetch_google_userinfo(access_token: str) -> dict:
        assert access_token == "token-123"
        return {
            "email": "rajeshgoli@gmail.com",
            "email_verified": True,
            "name": "Rajesh Goli",
        }

    monkeypatch.setattr("src.server._exchange_google_code", fake_exchange_google_code)
    monkeypatch.setattr("src.server._fetch_google_userinfo", fake_fetch_google_userinfo)

    client = TestClient(
        create_app(session_manager=_session_manager(), config=_auth_config()),
        base_url="https://sm.rajeshgo.li",
    )

    login_response = client.get("/auth/google/login?next=/watch/", follow_redirects=False)
    assert login_response.status_code == 302

    callback_response = client.get(
        "/auth/google/callback?state=oauth-state-123&code=oauth-code",
        follow_redirects=False,
    )

    assert callback_response.status_code == 302
    assert callback_response.headers["location"] == "/watch/"

    session_response = client.get("/auth/session")
    assert session_response.status_code == 200
    assert session_response.json() == {
        "enabled": True,
        "authenticated": True,
        "bypass": False,
        "email": "rajeshgoli@gmail.com",
        "name": "Rajesh Goli",
        "auth_type": "browser_session",
    }

    protected_response = client.get("/sessions")
    assert protected_response.status_code == 200


def test_logout_redirects_to_public_signed_out_page(monkeypatch):
    monkeypatch.setattr("src.server.secrets.token_urlsafe", lambda _: "oauth-state-123")

    async def fake_exchange_google_code(client_id: str, client_secret: str, redirect_uri: str, code: str) -> dict:
        return {"access_token": "token-123"}

    async def fake_fetch_google_userinfo(access_token: str) -> dict:
        return {
            "email": "rajeshgoli@gmail.com",
            "email_verified": True,
            "name": "Rajesh Goli",
        }

    monkeypatch.setattr("src.server._exchange_google_code", fake_exchange_google_code)
    monkeypatch.setattr("src.server._fetch_google_userinfo", fake_fetch_google_userinfo)

    client = TestClient(
        create_app(session_manager=_session_manager(), config=_auth_config()),
        base_url="https://sm.rajeshgo.li",
    )

    client.get("/auth/google/login?next=/watch/", follow_redirects=False)
    client.get("/auth/google/callback?state=oauth-state-123&code=oauth-code", follow_redirects=False)

    logout_response = client.get("/auth/logout", follow_redirects=False)

    assert logout_response.status_code == 302
    assert logout_response.headers["location"] == "/logged-out"

    post_logout_watch = client.get("/watch", follow_redirects=False)
    assert post_logout_watch.status_code == 302
    assert post_logout_watch.headers["location"] == "/auth/google/login?next=%2Fwatch"

    landing_response = client.get("/logged-out")
    assert landing_response.status_code == 200
    assert "Signed out" in landing_response.text


def test_google_callback_rejects_non_allowlisted_email(monkeypatch):
    monkeypatch.setattr("src.server.secrets.token_urlsafe", lambda _: "oauth-state-123")

    async def fake_exchange_google_code(client_id: str, client_secret: str, redirect_uri: str, code: str) -> dict:
        return {"access_token": "token-123"}

    async def fake_fetch_google_userinfo(access_token: str) -> dict:
        return {
            "email": "not-rajesh@example.com",
            "email_verified": True,
            "name": "Intruder",
        }

    monkeypatch.setattr("src.server._exchange_google_code", fake_exchange_google_code)
    monkeypatch.setattr("src.server._fetch_google_userinfo", fake_fetch_google_userinfo)

    client = TestClient(
        create_app(session_manager=_session_manager(), config=_auth_config()),
        base_url="https://sm.rajeshgo.li",
    )

    client.get("/auth/google/login?next=/watch/", follow_redirects=False)

    callback_response = client.get(
        "/auth/google/callback?state=oauth-state-123&code=oauth-code",
        follow_redirects=False,
    )

    assert callback_response.status_code == 302
    assert callback_response.headers["location"] == "/watch/?auth_error=unauthorized_email"

    protected_response = client.get("/sessions")
    assert protected_response.status_code == 401
