from unittest.mock import MagicMock
import base64
import subprocess
import time

from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from fastapi.testclient import TestClient

from src.models import Session, SessionStatus
from src.server import _issue_device_access_token, create_app


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
            "mobile_terminal_supported": False,
            "mobile_terminal_ws_url": None,
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
    assert "Cloudflare Tunnel SSH transport failed and no LAN SSH fallback is configured." in ssh_command
    assert "cloudflared access login" not in ssh_command
    assert "if [ \"$attach_status\" -ne 0 ]; then show_attach_error; fi; attach_cleanup" in ssh_command
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


def _mobile_terminal_config(private_key) -> dict:
    config = _android_config()
    public_key = private_key.public_key().public_bytes(
        serialization.Encoding.PEM,
        serialization.PublicFormat.SubjectPublicKeyInfo,
    ).decode("utf-8")
    config["mobile_terminal"] = {
        "enabled": True,
        "allowed_users": {
            "local_bypass": {
                "interactive_shell_access": True,
                "registered_device_keys": [
                    {
                        "id": "test-device",
                        "public_key": public_key,
                        "enabled": True,
                    }
                ],
            }
        },
        "ticket_ttl_seconds": 30,
        "auth_frame_timeout_seconds": 3,
        "max_concurrent_attaches_per_user": 1,
        "max_concurrent_attaches_per_session": 1,
        "max_concurrent_attaches_global": 4,
    }
    return config


def _sign_mobile_ticket_headers(
    private_key,
    session_id: str,
    *,
    actor_email: str = "local_bypass",
    path: str | None = None,
    timestamp: str | None = None,
) -> dict[str, str]:
    timestamp = timestamp or str(time.time())
    nonce = "nonce-1"
    message = "\n".join(
        [
            "SM-MOBILE-TERMINAL-TICKET-V1",
            "POST",
            path or f"/client/sessions/{session_id}/attach-ticket",
            session_id,
            actor_email,
            "test-device",
            timestamp,
            nonce,
        ]
    )
    signature = private_key.sign(message.encode("utf-8"), ec.ECDSA(hashes.SHA256()))
    return {
        "X-SM-Device-Key-Id": "test-device",
        "X-SM-Device-Timestamp": timestamp,
        "X-SM-Device-Nonce": nonce,
        "X-SM-Device-Signature": base64.b64encode(signature).decode("ascii"),
    }


def _sign_mobile_ws_auth(private_key, *, ticket_id: str, session_id: str, nonce: str = "ws-nonce-1") -> str:
    message = "\n".join(
        [
            "SM-MOBILE-TERMINAL-WS-V1",
            ticket_id,
            session_id,
            "local_bypass",
            "test-device",
            nonce,
        ]
    )
    signature = private_key.sign(message.encode("utf-8"), ec.ECDSA(hashes.SHA256()))
    return base64.b64encode(signature).decode("ascii")


def test_client_sessions_prefer_mobile_terminal_when_enabled():
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_mobile_terminal_config(private_key),
    )
    client = TestClient(app)

    response = client.get("/client/sessions")

    assert response.status_code == 200
    payload = response.json()["sessions"][0]
    assert payload["mobile_terminal"] == {
        "supported": True,
        "transport": "sm-https-tmux",
        "ticket_endpoint": "/client/sessions/fork1001/attach-ticket",
        "ws_url": "wss://sm.rajeshgo.li/client/terminal",
        "tmux_session": "codex-fork-fork1001",
        "tmux_socket_name": None,
        "runtime_mode": "detached_runtime",
        "requires_device_key": True,
    }
    assert payload["primary_action"] == {
        "type": "mobile_terminal",
        "label": "Attach",
    }


def test_mobile_terminal_require_tls_rejects_configured_plaintext_ws_url():
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    config = _mobile_terminal_config(private_key)
    config["mobile_terminal"]["require_tls"] = True
    config["mobile_terminal"]["ws_url"] = "ws://localhost:8420/client/terminal"
    app = create_app(
        session_manager=_manager(session),
        config=config,
    )
    client = TestClient(app)

    response = client.get("/client/sessions")

    assert response.status_code == 200
    payload = response.json()["sessions"][0]
    assert payload["mobile_terminal"]["supported"] is False
    assert payload["mobile_terminal"]["reason"] == "mobile terminal public HTTPS host is not configured"
    assert payload["primary_action"] == {
        "type": "termux_attach",
        "label": "Attach in Termux",
    }


def test_mobile_terminal_can_allow_plaintext_ws_when_tls_not_required():
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    config = _mobile_terminal_config(private_key)
    config["mobile_terminal"]["require_tls"] = False
    config["mobile_terminal"]["ws_url"] = "ws://localhost:8420/client/terminal"
    app = create_app(
        session_manager=_manager(session),
        config=config,
    )
    client = TestClient(app)

    response = client.get("/client/sessions")

    assert response.status_code == 200
    payload = response.json()["sessions"][0]
    assert payload["mobile_terminal"]["supported"] is True
    assert payload["mobile_terminal"]["ws_url"] == "ws://localhost:8420/client/terminal"


def test_mobile_terminal_metadata_preserves_configured_public_path_prefix():
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    config = _mobile_terminal_config(private_key)
    config["external_access"]["public_http_path_prefix"] = "/sm"
    app = create_app(
        session_manager=_manager(session),
        config=config,
    )
    client = TestClient(app)

    response = client.get("/client/sessions")

    assert response.status_code == 200
    payload = response.json()["sessions"][0]["mobile_terminal"]
    assert payload["ticket_endpoint"] == "/sm/client/sessions/fork1001/attach-ticket"
    assert payload["ws_url"] == "wss://sm.rajeshgo.li/sm/client/terminal"


def test_mobile_terminal_metadata_preserves_request_root_path_prefix():
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_mobile_terminal_config(private_key),
    )
    client = TestClient(app, root_path="/sm")

    response = client.get("/client/sessions")

    assert response.status_code == 200
    payload = response.json()["sessions"][0]["mobile_terminal"]
    assert payload["ticket_endpoint"] == "/sm/client/sessions/fork1001/attach-ticket"
    assert payload["ws_url"] == "wss://sm.rajeshgo.li/sm/client/terminal"


def test_client_sessions_hide_mobile_terminal_action_for_unregistered_actor():
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    config = _mobile_terminal_config(private_key)
    token = _issue_device_access_token(config, email="other@example.com", name="Other")["access_token"]
    app = create_app(
        session_manager=_manager(session),
        config=config,
    )
    client = TestClient(app, base_url="https://sm.rajeshgo.li")

    response = client.get(
        "/client/sessions",
        headers={"Authorization": f"Bearer {token}"},
    )

    assert response.status_code == 200
    payload = response.json()["sessions"][0]
    assert payload["mobile_terminal"]["supported"] is False
    assert payload["mobile_terminal"]["reason"] == "mobile terminal user is not configured"
    assert payload["primary_action"] == {
        "type": "termux_attach",
        "label": "Attach in Termux",
    }


def test_mobile_terminal_user_matching_does_not_accept_email_local_part():
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    config = _mobile_terminal_config(private_key)
    config["mobile_terminal"]["allowed_users"] = {
        "rajesh": config["mobile_terminal"]["allowed_users"]["local_bypass"],
    }
    actor_email = "rajesh@example.invalid"
    token = _issue_device_access_token(config, email=actor_email, name="Rajesh")["access_token"]
    app = create_app(
        session_manager=_manager(session),
        config=config,
    )
    client = TestClient(app, base_url="https://sm.rajeshgo.li")

    response = client.post(
        f"/client/sessions/{session.id}/attach-ticket",
        json={},
        headers={
            "Authorization": f"Bearer {token}",
            **_sign_mobile_ticket_headers(private_key, session.id, actor_email=actor_email),
        },
    )

    assert response.status_code == 403
    assert response.json()["detail"] == "mobile terminal user is not configured"


def test_mobile_attach_ticket_requires_registered_device_signature():
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_mobile_terminal_config(private_key),
    )
    client = TestClient(app)

    missing = client.post(f"/client/sessions/{session.id}/attach-ticket", json={})
    assert missing.status_code == 401

    response = client.post(
        f"/client/sessions/{session.id}/attach-ticket",
        json={},
        headers=_sign_mobile_ticket_headers(private_key, session.id),
    )

    assert response.status_code == 200
    payload = response.json()
    assert payload["ticket_id"].startswith("att_")
    assert len(payload["ticket_secret"]) >= 40
    assert payload["device_key_id"] == "test-device"
    assert payload["ws_url"] == "wss://sm.rajeshgo.li/client/terminal"
    assert payload["ticket_secret"] not in payload["ws_url"]


def test_mobile_attach_ticket_rejects_non_finite_device_timestamp():
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_mobile_terminal_config(private_key),
    )
    client = TestClient(app)

    response = client.post(
        f"/client/sessions/{session.id}/attach-ticket",
        json={},
        headers=_sign_mobile_ticket_headers(private_key, session.id, timestamp="NaN"),
    )

    assert response.status_code == 401
    assert response.json()["detail"] == "Invalid device timestamp"


def test_mobile_terminal_websocket_consumes_ticket_and_bridges_tmux(monkeypatch):
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_mobile_terminal_config(private_key),
    )
    client = TestClient(app)

    ticket_response = client.post(
        f"/client/sessions/{session.id}/attach-ticket",
        json={},
        headers=_sign_mobile_ticket_headers(private_key, session.id),
    )
    assert ticket_response.status_code == 200
    ticket = ticket_response.json()

    commands = []

    def fake_run(args, capture_output, text, check, timeout):
        commands.append(args)
        if "capture-pane" in args:
            return subprocess.CompletedProcess(args, 0, stdout="live pane output", stderr="")
        if "send-keys" in args:
            return subprocess.CompletedProcess(args, 0, stdout="", stderr="")
        if "resize-window" in args:
            return subprocess.CompletedProcess(args, 0, stdout="", stderr="")
        return subprocess.CompletedProcess(args, 1, stdout="", stderr="unexpected tmux command")

    monkeypatch.setattr("src.server.subprocess.run", fake_run)

    with client.websocket_connect("/client/terminal") as websocket:
        websocket.send_json(
            {
                "type": "auth",
                "ticket_id": ticket["ticket_id"],
                "ticket_secret": ticket["ticket_secret"],
                "device_key_id": "test-device",
                "nonce": "ws-nonce-1",
                "signature": _sign_mobile_ws_auth(
                    private_key,
                    ticket_id=ticket["ticket_id"],
                    session_id=session.id,
                ),
            }
        )
        assert websocket.receive_json() == {"type": "status", "state": "attached", "session_id": session.id}
        output = websocket.receive_json()
        assert output == {"type": "output", "mode": "snapshot", "data": "live pane output"}
        websocket.send_json({"type": "input", "data": "hello"})
        websocket.send_json({"type": "key", "key": "enter"})
        websocket.send_json({"type": "resize", "rows": 32, "cols": 120})
        assert websocket.receive_json() == {"type": "status", "state": "resized", "rows": 32, "cols": 120}
        websocket.send_json({"type": "detach"})
    assert any("resize-window" in command for command in commands)


def test_mobile_terminal_websocket_enforces_active_attach_limit_at_consume_time():
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_mobile_terminal_config(private_key),
    )
    client = TestClient(app)

    ticket_response = client.post(
        f"/client/sessions/{session.id}/attach-ticket",
        json={},
        headers=_sign_mobile_ticket_headers(private_key, session.id),
    )
    assert ticket_response.status_code == 200
    ticket = ticket_response.json()
    app.state.mobile_terminal_active_attaches["busy"] = {
        "user_id": "local_bypass",
        "session_id": "other-session",
        "provider": "claude",
        "device_key_id": "test-device",
        "started_at": time.time(),
    }

    with client.websocket_connect("/client/terminal") as websocket:
        websocket.send_json(
            {
                "type": "auth",
                "ticket_id": ticket["ticket_id"],
                "ticket_secret": ticket["ticket_secret"],
                "device_key_id": "test-device",
                "nonce": "ws-nonce-1",
                "signature": _sign_mobile_ws_auth(
                    private_key,
                    ticket_id=ticket["ticket_id"],
                    session_id=session.id,
                ),
            }
        )
        assert websocket.receive_json() == {
            "type": "error",
            "message": "Too many active mobile attaches for user",
        }


def test_mobile_terminal_disable_requires_owner_authorization():
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_mobile_terminal_config(private_key),
    )
    client = TestClient(app)

    response = client.post("/client/mobile-terminal/disable")

    assert response.status_code == 403
    assert response.json()["detail"] == "User is not allowed to disable mobile terminal attach"
    assert app.state.mobile_terminal_runtime_disabled is False


def test_mobile_terminal_disable_owner_terminates_active_attaches():
    private_key = ec.generate_private_key(ec.SECP256R1())
    session = _session()
    config = _mobile_terminal_config(private_key)
    config["mobile_terminal"]["allowed_users"]["local_bypass"]["mobile_terminal_owner"] = True
    app = create_app(
        session_manager=_manager(session),
        config=config,
    )
    client = TestClient(app)
    stop_event = MagicMock()
    app.state.mobile_terminal_active_attaches["active-1"] = {
        "user_id": "local_bypass",
        "session_id": session.id,
        "provider": session.provider,
        "device_key_id": "test-device",
        "started_at": time.time(),
        "stop_event": stop_event,
    }

    response = client.post("/client/mobile-terminal/disable")

    assert response.status_code == 200
    assert response.json() == {
        "ok": True,
        "disabled": True,
        "active_attaches_terminated": 1,
    }
    assert app.state.mobile_terminal_runtime_disabled is True
    assert app.state.mobile_terminal_active_attaches == {}
    stop_event.set.assert_called_once_with()


def test_client_sessions_fall_back_to_lan_ssh_on_cloudflare_bad_handshake():
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_android_config(),
    )
    app.state.infra_supervisor = MagicMock()
    app.state.infra_supervisor.get_check.side_effect = lambda name: {
        "status": "ok",
        "message": "android attach sshd is listening",
        "details": {
            "attach_ready": True,
            "listeners": ["127.0.0.1:22220", "192.168.4.21:22220"],
        },
    } if name == "android_sshd" else {
        "status": "ok",
        "message": "android attach cloudflared tunnel is running",
        "details": {"attach_ready": True},
    }
    client = TestClient(app)

    response = client.get("/client/sessions")

    assert response.status_code == 200
    ssh_command = response.json()["sessions"][0]["termux_attach"]["ssh_command"]
    assert "Cloudflare Tunnel SSH transport failed; trying LAN SSH fallback 192.168.4.21:22220..." in ssh_command
    assert "ssh -o StrictHostKeyChecking=accept-new -p 22220 -tt rajesh@192.168.4.21" in ssh_command
    assert "cloudflared access login" not in ssh_command


def test_client_sessions_fall_back_to_ipv6_lan_ssh_on_cloudflare_bad_handshake():
    session = _session()
    app = create_app(
        session_manager=_manager(session),
        config=_android_config(),
    )
    app.state.infra_supervisor = MagicMock()
    app.state.infra_supervisor.get_check.side_effect = lambda name: {
        "status": "ok",
        "message": "android attach sshd is listening",
        "details": {
            "attach_ready": True,
            "listeners": ["127.0.0.1:22220", "[fe80::1420:ef0f:9f96:cd24]:22220"],
        },
    } if name == "android_sshd" else {
        "status": "ok",
        "message": "android attach cloudflared tunnel is running",
        "details": {"attach_ready": True},
    }
    client = TestClient(app)

    response = client.get("/client/sessions")

    assert response.status_code == 200
    ssh_command = response.json()["sessions"][0]["termux_attach"]["ssh_command"]
    assert "Cloudflare Tunnel SSH transport failed; trying LAN SSH fallback fe80::1420:ef0f:9f96:cd24:22220..." in ssh_command
    assert "ssh -o StrictHostKeyChecking=accept-new -p 22220 -tt" in ssh_command
    assert "rajesh@[fe80::1420:ef0f:9f96:cd24]" in ssh_command
    assert "cloudflared access login" not in ssh_command


def test_client_sessions_generate_valid_non_cloudflare_attach_command():
    session = _session()
    config = _android_config()
    config["external_access"]["ssh_proxy_command"] = "nc %h 22"
    app = create_app(
        session_manager=_manager(session),
        config=config,
    )
    client = TestClient(app)

    response = client.get("/client/sessions")

    assert response.status_code == 200
    ssh_command = response.json()["sessions"][0]["termux_attach"]["ssh_command"]
    assert "ProxyCommand=nc %h 22" in ssh_command
    assert "maybe_recover_cloudflared() { :; return 1; }" in ssh_command
    assert "cloudflared access login" not in ssh_command


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
