from src.cli import commands


class _AttachClient:
    def __init__(
        self,
        session: dict,
        descriptor: dict,
        default_node: str | None = None,
        local_node: str | None = None,
    ):
        self._session = session
        self._descriptor = descriptor
        self.default_node = default_node
        self.local_node = local_node

    def get_session(self, session_id: str):
        if session_id == self._session["id"]:
            return self._session
        return None

    def list_sessions(self):
        return [self._session]

    def get_attach_descriptor(self, session_id: str):
        if session_id == self._session["id"]:
            return self._descriptor
        return None


def test_cmd_attach_uses_detached_runtime_descriptor(monkeypatch, capsys):
    calls = []

    def _fake_run(args, check):
        calls.append((args, check))

    monkeypatch.setattr("subprocess.run", _fake_run)

    session = {
        "id": "fork1001",
        "provider": "codex-fork",
        "tmux_session": "codex-fork-fork1001",
        "status": "running",
    }
    descriptor = {
        "session_id": "fork1001",
        "provider": "codex-fork",
        "attach_supported": True,
        "tmux_session": "codex-fork-fork1001",
        "runtime_id": "codex-fork:fork1001",
        "lifecycle_state": "running",
    }
    rc = commands.cmd_attach(_AttachClient(session, descriptor), "fork1001")
    assert rc == 0
    assert calls == [(["tmux", "attach", "-t", "codex-fork-fork1001"], True)]
    assert "Reattaching to detached codex-fork runtime codex-fork:fork1001" in capsys.readouterr().out


def test_cmd_attach_uses_descriptor_tmux_socket(monkeypatch):
    calls = []

    def _fake_run(args, check):
        calls.append((args, check))

    monkeypatch.setattr("subprocess.run", _fake_run)

    session = {
        "id": "claude1001",
        "provider": "claude",
        "tmux_session": "claude-claude1001",
        "status": "running",
    }
    descriptor = {
        "session_id": "claude1001",
        "provider": "claude",
        "attach_supported": True,
        "tmux_session": "claude-claude1001",
        "tmux_socket_name": "session-manager-test",
    }

    rc = commands.cmd_attach(_AttachClient(session, descriptor), "claude1001")

    assert rc == 0
    assert calls == [
        (["tmux", "-L", "session-manager-test", "attach", "-t", "claude-claude1001"], True)
    ]


def test_cmd_attach_uses_descriptor_attach_command(monkeypatch):
    calls = []

    def _fake_run(args, check):
        calls.append((args, check))

    monkeypatch.setattr("subprocess.run", _fake_run)

    session = {
        "id": "remote1001",
        "provider": "claude",
        "tmux_session": "claude-remote1001",
        "status": "running",
        "node": "worker",
    }
    descriptor = {
        "session_id": "remote1001",
        "provider": "claude",
        "attach_supported": True,
        "tmux_session": "claude-remote1001",
        "node": "worker",
        "attach_command": [
            "ssh",
            "-tt",
            "worker",
            "/bin/sh",
            "-lc",
            "'tmux attach -t claude-remote1001'",
        ],
    }

    rc = commands.cmd_attach(_AttachClient(session, descriptor), "remote1001")

    assert rc == 0
    assert calls == [
        (
            ["ssh", "-tt", "worker", "/bin/sh", "-lc", "'tmux attach -t claude-remote1001'"],
            True,
        )
    ]


def test_cmd_attach_keeps_descriptor_attach_command_for_remote_default_node(monkeypatch):
    calls = []

    def _fake_run(args, check):
        calls.append((args, check))

    monkeypatch.setattr("subprocess.run", _fake_run)

    session = {
        "id": "localnode1",
        "provider": "codex-fork",
        "tmux_session": "codex-fork-localnode1",
        "status": "running",
        "node": "macbook",
    }
    descriptor = {
        "session_id": "localnode1",
        "provider": "codex-fork",
        "attach_supported": True,
        "tmux_session": "codex-fork-localnode1",
        "tmux_socket_name": "session-manager-test",
        "node": "macbook",
        "attach_command": [
            "ssh",
            "-tt",
            "rajesh@macbook.local",
            "/bin/sh",
            "-lc",
            "'tmux -L session-manager-test attach -t codex-fork-localnode1'",
        ],
    }

    rc = commands.cmd_attach(_AttachClient(session, descriptor, default_node="macbook"), "localnode1")

    assert rc == 0
    assert calls == [
        (
            [
                "ssh",
                "-tt",
                "rajesh@macbook.local",
                "/bin/sh",
                "-lc",
                "'tmux -L session-manager-test attach -t codex-fork-localnode1'",
            ],
            True,
        )
    ]


def test_cmd_attach_prefers_local_tmux_when_descriptor_node_matches_client_local_node(monkeypatch):
    calls = []

    def _fake_run(args, check):
        calls.append((args, check))

    monkeypatch.setattr("subprocess.run", _fake_run)

    session = {
        "id": "localnode1",
        "provider": "codex-fork",
        "tmux_session": "codex-fork-localnode1",
        "status": "running",
        "node": "macbook",
    }
    descriptor = {
        "session_id": "localnode1",
        "provider": "codex-fork",
        "attach_supported": True,
        "tmux_session": "codex-fork-localnode1",
        "tmux_socket_name": "session-manager-test",
        "node": "macbook",
        "attach_command": [
            "ssh",
            "-tt",
            "rajesh@macbook.local",
            "/bin/sh",
            "-lc",
            "'tmux -L session-manager-test attach -t codex-fork-localnode1'",
        ],
    }

    rc = commands.cmd_attach(
        _AttachClient(session, descriptor, default_node="macbook", local_node="macbook"),
        "localnode1",
    )

    assert rc == 0
    assert calls == [
        (
            ["tmux", "-L", "session-manager-test", "attach", "-t", "codex-fork-localnode1"],
            True,
        )
    ]
