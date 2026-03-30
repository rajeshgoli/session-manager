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
