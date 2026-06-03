import os
import subprocess

from src.node_runner import NodeRegistry, NodeRunner


def test_primary_command_is_local_argv():
    runner = NodeRunner(NodeRegistry.from_config({}))

    assert runner.command("primary", ["tmux", "list-sessions"]) == ["tmux", "list-sessions"]


def test_remote_command_uses_controlmaster_and_remote_shell():
    registry = NodeRegistry.from_config(
        {
            "nodes": {
                "registry": {
                    "worker": {
                        "ssh": "dev@example",
                        "control_path": "~/.ssh/sm-worker",
                    }
                }
            }
        }
    )
    runner = NodeRunner(registry)

    command = runner.command("worker", ["tmux", "has-session", "-t", "claude-1234"])

    assert command[:2] == ["ssh", "-o"]
    assert "ControlMaster=auto" in command
    assert "ControlPersist=600" in command
    assert "-S" in command
    assert os.path.expanduser("~/.ssh/sm-worker") in command
    assert command[-4:] == [
        "dev@example",
        "/bin/sh",
        "-lc",
        "'tmux has-session -t claude-1234'",
    ]


def test_remote_attach_allocates_tty():
    registry = NodeRegistry.from_config(
        {"nodes": {"registry": {"worker": {"ssh": "dev@example"}}}}
    )
    runner = NodeRunner(registry)

    command = runner.attach_command("worker", ["tmux", "attach", "-t", "claude-1234"])

    assert command[0:2] == ["ssh", "-tt"]
    assert command[-4:] == [
        "dev@example",
        "/bin/sh",
        "-lc",
        "'tmux attach -t claude-1234'",
    ]


def test_remote_resolve_directory_expands_home_relative_path(monkeypatch):
    registry = NodeRegistry.from_config(
        {"nodes": {"registry": {"worker": {"ssh": "dev@example"}}}}
    )
    runner = NodeRunner(registry)
    captured = {}

    def _fake_run(cmd, **kwargs):
        captured["cmd"] = cmd
        return subprocess.CompletedProcess(cmd, 0, stdout="/home/dev/repo\n", stderr="")

    monkeypatch.setattr(subprocess, "run", _fake_run)

    resolved = runner.resolve_directory("worker", "~/repo")

    assert resolved == "/home/dev/repo"
    assert "${HOME%/}/${path#\\~/}" in captured["cmd"][-1]
    assert "sh " in captured["cmd"][-1]
    assert "~/repo" in captured["cmd"][-1]


def test_remote_resolve_directory_returns_none_for_empty_stdout(monkeypatch):
    registry = NodeRegistry.from_config(
        {"nodes": {"registry": {"worker": {"ssh": "dev@example"}}}}
    )
    runner = NodeRunner(registry)

    monkeypatch.setattr(
        subprocess,
        "run",
        lambda cmd, **kwargs: subprocess.CompletedProcess(cmd, 0, stdout="", stderr=""),
    )

    assert runner.resolve_directory("worker", "~/missing") is None


def test_primary_ensure_file_preserves_existing_content(tmp_path):
    runner = NodeRunner(NodeRegistry.from_config({}))
    log_file = tmp_path / "logs" / "session.log"
    log_file.parent.mkdir()
    log_file.write_text("existing output\n")

    assert runner.ensure_file("primary", str(log_file)) is True
    assert log_file.read_text() == "existing output\n"


def test_remote_ensure_file_is_non_truncating_and_expands_home(monkeypatch):
    registry = NodeRegistry.from_config(
        {"nodes": {"registry": {"worker": {"ssh": "dev@example"}}}}
    )
    runner = NodeRunner(registry)
    captured = {}

    def _fake_run(cmd, **kwargs):
        captured["cmd"] = cmd
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", _fake_run)

    assert runner.ensure_file("worker", "~/.sm/logs/session.log") is True
    remote_payload = captured["cmd"][-1]
    assert "${HOME%/}/${path#\\~/}" in remote_payload
    assert ": >>" in remote_payload
    assert ": > " not in remote_payload
    assert "~/.sm/logs/session.log" in remote_payload


def test_remote_command_available_expands_home_relative_path(monkeypatch):
    registry = NodeRegistry.from_config(
        {"nodes": {"registry": {"worker": {"ssh": "dev@example"}}}}
    )
    runner = NodeRunner(registry)
    captured = {}

    def _fake_run(cmd, **kwargs):
        captured["cmd"] = cmd
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", _fake_run)

    assert runner.command_available("worker", "~/bin/claude") is True
    remote_payload = captured["cmd"][-1]
    assert "${HOME%/}/${path#\\~/}" in remote_payload
    assert 'test -f "$path" && test -x "$path"' in remote_payload
    assert "~/bin/claude" in remote_payload
