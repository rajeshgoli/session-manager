from unittest.mock import MagicMock

from fastapi.testclient import TestClient

from src.models import Session, SessionStatus
from src.server import create_app


def _android_config() -> dict:
    return {
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
        "external_access": {
            "public_http_host": "sm.rajeshgo.li",
            "public_ssh_host": "ssh.sm.rajeshgo.li",
            "ssh_username": "rajesh",
            "ssh_proxy_command": "cloudflared access ssh --hostname %h",
        },
    }


def _session(
    session_id: str = "fork1001",
    provider: str = "codex-fork",
    status: SessionStatus = SessionStatus.RUNNING,
) -> Session:
    return Session(
        id=session_id,
        name=f"{provider}-{session_id}",
        working_dir="/tmp/project",
        tmux_session=f"{provider}-{session_id}",
        status=status,
        provider=provider,
        log_file=f"/tmp/{session_id}.log",
    )


def _manager(session: Session) -> MagicMock:
    manager = MagicMock()
    manager.sessions = {session.id: session}
    manager.list_sessions.return_value = [session]
    manager.get_session.side_effect = lambda session_id: manager.sessions.get(session_id)
    manager.get_effective_session_name.side_effect = lambda current_session: current_session.friendly_name or current_session.name
    manager.get_session_aliases.return_value = []
    manager.list_adoption_proposals.return_value = []
    manager.get_codex_latest_activity_action.return_value = None
    manager.is_codex_rollout_enabled.return_value = True
    manager.get_attach_descriptor.side_effect = lambda session_id: {
        "session_id": session_id,
        "provider": session.provider,
        "attach_supported": True,
        "attach_transport": "tmux",
        "tmux_session": session.tmux_session,
        "runtime_mode": "detached_runtime" if session.provider == "codex-fork" else "tmux",
    }
    return manager


def test_client_bootstrap_is_public_for_cold_mobile_clients():
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_android_config(),
    )
    client = TestClient(app, base_url="https://sm.rajeshgo.li")

    response = client.get("/client/bootstrap")

    assert response.status_code == 200
    assert response.json()["auth"]["device_auth_endpoint"] == "/auth/device/google"


def test_client_bootstrap_reports_termux_attach_defaults():
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_android_config(),
    )
    client = TestClient(app)

    response = client.get("/client/bootstrap")

    assert response.status_code == 200
    assert response.json() == {
        "auth": {
            "mode": "browser_session_cookie",
            "session_endpoint": "/auth/session",
            "login_endpoint": "/auth/google/login",
            "logout_endpoint": "/auth/logout",
            "device_auth_endpoint": "/auth/device/google",
            "device_auth_token_type": "Bearer",
            "google_server_client_id": "web-client-id",
        },
        "external_access": {
            "public_http_host": "sm.rajeshgo.li",
            "public_ssh_host": "ssh.sm.rajeshgo.li",
            "ssh_username": "rajesh",
            "termux_attach_supported": True,
        },
        "session_open_defaults": {
            "preferred_action": "termux_attach",
            "termux_package": "com.termux",
        },
    }


def test_client_bootstrap_does_not_leak_raw_proxy_command():
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_android_config(),
    )
    client = TestClient(app)

    response = client.get("/client/bootstrap")

    assert response.status_code == 200
    payload = response.json()
    assert "ssh_proxy_command" not in payload["external_access"]


def test_client_sessions_include_termux_attach_metadata():
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_android_config(),
    )
    client = TestClient(app)

    response = client.get("/client/sessions")

    assert response.status_code == 200
    payload = response.json()["sessions"][0]
    assert payload["id"] == "fork1001"
    assert payload["attach_descriptor"]["tmux_session"] == "codex-fork-fork1001"
    assert payload["termux_attach"] == {
        "supported": True,
        "transport": "termux-ssh-tmux",
        "ssh_host": "ssh.sm.rajeshgo.li",
        "ssh_username": "rajesh",
        "ssh_proxy_command": "cloudflared access ssh --hostname %h",
        "ssh_command": payload["termux_attach"]["ssh_command"],
        "tmux_session": "codex-fork-fork1001",
        "runtime_mode": "detached_runtime",
        "termux_package": "com.termux",
    }
    ssh_command = payload["termux_attach"]["ssh_command"]
    assert ssh_command.startswith("sh -lc ")
    assert "Connecting to codex-fork-fork1001..." in ssh_command
    assert "Attach transport failed (255); retrying once..." in ssh_command
    assert "run_attach() {" in ssh_command
    assert "stty sane" in ssh_command
    assert "sm-attach-$$-${RANDOM:-0}.log" in ssh_command
    assert "websocket: bad handshake" in ssh_command
    assert "Cloudflare Access SSH auth failed; refreshing login for ssh.sm.rajeshgo.li..." in ssh_command
    assert "cloudflared access login https://ssh.sm.rajeshgo.li" in ssh_command
    assert "attach_pid=$!" not in ssh_command
    assert "fg %1 >/dev/null 2>&1 || wait \"$attach_pid\"" not in ssh_command
    assert "kill \"$attach_pid\"" not in ssh_command
    assert "pkill -P \"$attach_pid\"" not in ssh_command
    assert "ProxyCommand=cloudflared access ssh --hostname %h" in ssh_command
    assert "StrictHostKeyChecking=accept-new" in ssh_command
    assert "rajesh@ssh.sm.rajeshgo.li" in ssh_command
    assert "exec tmux attach-session -d -t \"$SM_TMUX_SESSION\"" in ssh_command
    assert payload["primary_action"] == {
        "type": "termux_attach",
        "label": "Attach in Termux",
    }


def test_client_session_reports_details_when_external_attach_not_configured():
    session = _session()
    app = create_app(session_manager=_manager(session), config={})
    client = TestClient(app)

    response = client.get(f"/client/sessions/{session.id}")

    assert response.status_code == 200
    payload = response.json()
    assert payload["termux_attach"] == {
        "supported": False,
        "reason": "external ssh attach is not configured",
        "transport": "termux-ssh-tmux",
    }
    assert payload["primary_action"] == {
        "type": "details",
        "label": "View details",
    }


def test_client_session_reports_details_when_attach_infra_is_down():
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_android_config(),
    )
    app.state.infra_supervisor = MagicMock()
    app.state.infra_supervisor.get_check.side_effect = lambda name: {
        "status": "error",
        "message": "android attach sshd is unavailable",
    } if name == "android_sshd" else None
    client = TestClient(app)

    response = client.get(f"/client/sessions/{session.id}")

    assert response.status_code == 200
    payload = response.json()
    assert payload["termux_attach"] == {
        "supported": False,
        "reason": "android attach sshd is unavailable",
        "transport": "termux-ssh-tmux",
    }
    assert payload["primary_action"] == {
        "type": "details",
        "label": "View details",
    }


def test_client_bootstrap_disables_termux_attach_when_attach_infra_is_down():
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_android_config(),
    )
    app.state.infra_supervisor = MagicMock()
    app.state.infra_supervisor.get_check.side_effect = lambda name: {
        "status": "error",
        "message": "android attach sshd is unavailable",
    } if name == "android_sshd" else None
    client = TestClient(app)

    response = client.get("/client/bootstrap")

    assert response.status_code == 200
    payload = response.json()
    assert payload["external_access"]["termux_attach_supported"] is False
    assert payload["session_open_defaults"]["preferred_action"] == "details"


def test_client_sessions_disable_termux_attach_when_tunnel_is_down():
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_android_config(),
    )
    app.state.infra_supervisor = MagicMock()
    app.state.infra_supervisor.get_check.side_effect = lambda name: {
        "status": "error",
        "message": "android attach cloudflared tunnel is unavailable",
        "details": {"attach_ready": False},
    } if name == "android_tunnel" else {
        "status": "ok",
        "message": "android attach sshd is listening",
        "details": {"attach_ready": True},
    }
    client = TestClient(app)

    response = client.get(f"/client/sessions/{session.id}")

    assert response.status_code == 200
    payload = response.json()
    assert payload["termux_attach"] == {
        "supported": False,
        "reason": "android attach cloudflared tunnel is unavailable",
        "transport": "termux-ssh-tmux",
    }
    assert payload["primary_action"] == {
        "type": "details",
        "label": "View details",
    }


def test_client_bootstrap_disables_termux_attach_when_tunnel_is_down():
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_android_config(),
    )
    app.state.infra_supervisor = MagicMock()
    app.state.infra_supervisor.get_check.side_effect = lambda name: {
        "status": "error",
        "message": "android attach cloudflared tunnel is unavailable",
        "details": {"attach_ready": False},
    } if name == "android_tunnel" else {
        "status": "ok",
        "message": "android attach sshd is listening",
        "details": {"attach_ready": True},
    }
    client = TestClient(app)

    response = client.get("/client/bootstrap")

    assert response.status_code == 200
    payload = response.json()
    assert payload["external_access"]["termux_attach_supported"] is False
    assert payload["session_open_defaults"]["preferred_action"] == "details"


def test_client_session_reports_headless_provider_as_details():
    session = _session(session_id="app1001", provider="codex-app", status=SessionStatus.IDLE)
    manager = _manager(session)
    manager.get_attach_descriptor.side_effect = lambda session_id: {
        "session_id": session_id,
        "provider": "codex-app",
        "attach_supported": False,
        "runtime_mode": "headless",
        "message": "provider=codex-app is headless; use watch/status APIs instead of attach.",
    }
    app = create_app(
        session_manager=manager,
        config=_android_config(),
    )
    client = TestClient(app)

    response = client.get(f"/client/sessions/{session.id}")

    assert response.status_code == 200
    payload = response.json()
    assert payload["termux_attach"] == {
        "supported": False,
        "reason": "provider=codex-app is headless; use watch/status APIs instead of attach.",
        "transport": "termux-ssh-tmux",
    }
    assert payload["primary_action"] == {
        "type": "details",
        "label": "View details",
        "reason": "provider=codex-app is headless; use watch/status APIs instead of attach.",
    }


def test_device_google_auth_exchange_and_bearer_access(monkeypatch):
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_android_config(),
    )
    client = TestClient(app, base_url="https://sm.rajeshgo.li")

    async def fake_verify_google_id_token(id_token: str) -> dict:
        assert id_token == "google-id-token"
        return {
            "aud": "android-client-id",
            "email": "rajeshgoli@gmail.com",
            "email_verified": "true",
            "name": "Rajesh Goli",
        }

    monkeypatch.setattr("src.server._verify_google_id_token", fake_verify_google_id_token)

    auth_response = client.post("/auth/device/google", json={"id_token": "google-id-token"})

    assert auth_response.status_code == 200
    token_payload = auth_response.json()
    assert token_payload["token_type"] == "Bearer"
    assert token_payload["email"] == "rajeshgoli@gmail.com"

    auth_session_response = client.get(
        "/auth/session",
        headers={"Authorization": f"Bearer {token_payload['access_token']}"},
    )
    assert auth_session_response.status_code == 200
    assert auth_session_response.json() == {
        "enabled": True,
        "authenticated": True,
        "bypass": False,
        "email": "rajeshgoli@gmail.com",
        "name": "Rajesh Goli",
        "auth_type": "device_bearer",
    }

    sessions_response = client.get(
        "/client/sessions",
        headers={"Authorization": f"Bearer {token_payload['access_token']}"},
    )
    assert sessions_response.status_code == 200
    assert sessions_response.json()["sessions"][0]["id"] == "fork1001"


def test_device_google_auth_rejects_wrong_audience(monkeypatch):
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_android_config(),
    )
    client = TestClient(app, base_url="https://sm.rajeshgo.li")

    async def fake_verify_google_id_token(id_token: str) -> dict:
        return {
            "aud": "wrong-client-id",
            "email": "rajeshgoli@gmail.com",
            "email_verified": "true",
            "name": "Rajesh Goli",
        }

    monkeypatch.setattr("src.server._verify_google_id_token", fake_verify_google_id_token)

    response = client.post("/auth/device/google", json={"id_token": "google-id-token"})

    assert response.status_code == 401
    assert response.json()["detail"] == "Google ID token audience is not allowed"
