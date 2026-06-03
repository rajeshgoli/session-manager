import os

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
