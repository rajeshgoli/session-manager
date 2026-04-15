from src.infra_supervisor import InfrastructureSupervisor


def test_on_ac_power_tolerates_missing_pmset(monkeypatch):
    supervisor = InfrastructureSupervisor({})

    def fake_run(*args, **kwargs):
        raise FileNotFoundError("pmset not installed")

    monkeypatch.setattr("src.infra_supervisor.subprocess.run", fake_run)

    assert supervisor._on_ac_power() is True


def test_android_sshd_reports_warning_when_config_missing(tmp_path):
    supervisor = InfrastructureSupervisor(
        {
            "external_access": {"public_ssh_host": "sm-ssh.example.com"},
            "infra_supervisor": {
                "android_sshd": {
                    "config_path": str(tmp_path / "missing_sshd_config"),
                }
            },
        }
    )

    result = supervisor.ensure_now()["android_sshd"]

    assert result["status"] == "warning"
    assert "config is missing" in result["message"]


def test_android_sshd_repairs_via_launch_agent_when_listener_is_down(monkeypatch, tmp_path):
    config_path = tmp_path / "sshd_config"
    config_path.write_text("Port 22220\nListenAddress 127.0.0.1\n")
    plist_path = tmp_path / "com.rajesh.sm-android-sshd.plist"
    plist_path.write_text("<plist/>")

    supervisor = InfrastructureSupervisor(
        {
            "external_access": {"public_ssh_host": "sm-ssh.example.com"},
            "infra_supervisor": {
                "android_sshd": {
                    "config_path": str(config_path),
                    "launch_agent_plist": str(plist_path),
                    "launch_agent_label": "com.rajesh.sm-android-sshd",
                }
            },
        }
    )

    checks = iter([False, True])
    monkeypatch.setattr(supervisor, "_tcp_listening", lambda host, port: next(checks))
    monkeypatch.setattr(supervisor, "_repair_launch_agent", lambda label, path: ["bootstrap", "kickstart"])

    result = supervisor.ensure_now()["android_sshd"]

    assert result["status"] == "warning"
    assert "was down and was restarted" in result["message"]
    assert result["details"]["actions"] == ["bootstrap", "kickstart"]


def test_android_tunnel_reports_warning_when_launch_agent_missing(tmp_path):
    supervisor = InfrastructureSupervisor(
        {
            "external_access": {
                "public_ssh_host": "sm-ssh.example.com",
                "ssh_proxy_command": "cloudflared access ssh --hostname %h",
            },
            "infra_supervisor": {
                "android_tunnel": {
                    "launch_agent_plist": str(tmp_path / "missing_tunnel.plist"),
                }
            },
        }
    )

    supervisor._ensure_android_sshd = lambda: {"status": "ok"}
    supervisor._ensure_tmux_base = lambda: {"status": "ok"}
    supervisor._ensure_ac_caffeinate = lambda: {"status": "ok"}

    result = supervisor.ensure_now()["android_tunnel"]

    assert result["status"] == "warning"
    assert "launch agent is missing" in result["message"]
    assert result["details"]["attach_ready"] is False


def test_android_tunnel_repairs_via_launch_agent_when_down(monkeypatch, tmp_path):
    plist_path = tmp_path / "com.rajesh.sm-android-tunnel.plist"
    plist_path.write_text("<plist/>")

    supervisor = InfrastructureSupervisor(
        {
            "external_access": {
                "public_ssh_host": "sm-ssh.example.com",
                "ssh_proxy_command": "cloudflared access ssh --hostname %h",
            },
            "infra_supervisor": {
                "android_tunnel": {
                    "launch_agent_plist": str(plist_path),
                    "launch_agent_label": "com.rajesh.sm-android-tunnel",
                }
            },
        }
    )

    states = iter([False, True])
    monkeypatch.setattr(supervisor, "_ensure_android_sshd", lambda: {"status": "ok"})
    monkeypatch.setattr(supervisor, "_ensure_tmux_base", lambda: {"status": "ok"})
    monkeypatch.setattr(supervisor, "_ensure_ac_caffeinate", lambda: {"status": "ok"})
    monkeypatch.setattr(supervisor, "_launch_agent_running", lambda label: next(states))
    monkeypatch.setattr(supervisor, "_repair_launch_agent", lambda label, path: ["bootstrap", "kickstart"])

    result = supervisor.ensure_now()["android_tunnel"]

    assert result["status"] == "warning"
    assert "was down and was restarted" in result["message"]
    assert result["details"]["attach_ready"] is True
    assert result["details"]["actions"] == ["bootstrap", "kickstart"]


def test_android_tunnel_reports_error_when_public_probe_fails(monkeypatch, tmp_path):
    plist_path = tmp_path / "com.rajesh.sm-android-tunnel.plist"
    plist_path.write_text("<plist/>")

    supervisor = InfrastructureSupervisor(
        {
            "external_access": {
                "public_ssh_host": "sm-ssh.example.com",
                "ssh_username": "rajesh",
                "ssh_proxy_command": "cloudflared access ssh --hostname %h",
            },
            "infra_supervisor": {
                "android_tunnel": {
                    "launch_agent_plist": str(plist_path),
                    "launch_agent_label": "com.rajesh.sm-android-tunnel",
                }
            },
        }
    )

    monkeypatch.setattr(supervisor, "_launch_agent_running", lambda label: True)
    monkeypatch.setattr(supervisor, "_repair_launch_agent", lambda label, path: ["kickstart"])
    monkeypatch.setattr(
        supervisor,
        "_probe_android_public_ssh",
        lambda **kwargs: (False, "Connection timed out during banner exchange"),
    )

    result = supervisor.ensure_now()["android_tunnel"]

    assert result["status"] == "error"
    assert "public SSH path is unhealthy" in result["message"]
    assert result["details"]["attach_ready"] is False
    assert result["details"]["actions"] == ["kickstart"]
    assert result["details"]["public_probe_error"] == "Connection timed out during banner exchange"


def test_android_tunnel_restart_recovers_public_probe(monkeypatch, tmp_path):
    plist_path = tmp_path / "com.rajesh.sm-android-tunnel.plist"
    plist_path.write_text("<plist/>")

    supervisor = InfrastructureSupervisor(
        {
            "external_access": {
                "public_ssh_host": "sm-ssh.example.com",
                "ssh_username": "rajesh",
                "ssh_proxy_command": "cloudflared access ssh --hostname %h",
            },
            "infra_supervisor": {
                "android_tunnel": {
                    "launch_agent_plist": str(plist_path),
                    "launch_agent_label": "com.rajesh.sm-android-tunnel",
                }
            },
        }
    )

    probe_results = iter(
        [
            (False, "Connection timed out during banner exchange"),
            (True, None),
        ]
    )
    monkeypatch.setattr(supervisor, "_launch_agent_running", lambda label: True)
    monkeypatch.setattr(supervisor, "_repair_launch_agent", lambda label, path: ["kickstart"])
    monkeypatch.setattr(supervisor, "_probe_android_public_ssh", lambda **kwargs: next(probe_results))

    result = supervisor.ensure_now()["android_tunnel"]

    assert result["status"] == "warning"
    assert "was unhealthy and was restarted" in result["message"]
    assert result["details"]["attach_ready"] is True
    assert result["details"]["actions"] == ["kickstart"]


def test_tmux_base_is_recreated_when_missing(monkeypatch):
    supervisor = InfrastructureSupervisor({})
    monkeypatch.setattr(supervisor, "_ensure_android_sshd", lambda: {"status": "ok"})
    monkeypatch.setattr(supervisor, "_ensure_ac_caffeinate", lambda: {"status": "ok"})

    calls = []

    class Result:
        def __init__(self, returncode: int, stderr: str = "", stdout: str = ""):
            self.returncode = returncode
            self.stderr = stderr
            self.stdout = stdout

    def fake_run(cmd, capture_output=False, text=False):
        calls.append(cmd)
        if cmd[1] == "has-session":
            return Result(1)
        return Result(0)

    monkeypatch.setattr("src.infra_supervisor.shutil.which", lambda name: "/opt/homebrew/bin/tmux")
    monkeypatch.setattr("src.infra_supervisor.subprocess.run", fake_run)

    result = supervisor.ensure_now()["tmux_base"]

    assert result["status"] == "warning"
    assert result["details"]["session"] == "base"
    assert calls[0][:3] == ["/opt/homebrew/bin/tmux", "has-session", "-t"]
    assert calls[1][:3] == ["/opt/homebrew/bin/tmux", "new-session", "-d"]


def test_parse_sshd_listener_targets_honors_host_port_entries(tmp_path):
    config_path = tmp_path / "sshd_config"
    config_path.write_text(
        "Port 22\n"
        "ListenAddress 127.0.0.1:22220\n"
        "ListenAddress [::1]:22221\n"
        "ListenAddress 192.168.5.47\n"
    )

    targets = InfrastructureSupervisor._parse_sshd_listener_targets(config_path)

    assert targets == [
        ("127.0.0.1", 22220),
        ("::1", 22221),
        ("192.168.5.47", 22),
    ]
