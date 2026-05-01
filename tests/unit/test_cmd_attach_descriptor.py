from src.cli import commands


class _AttachClient:
    def __init__(self, session: dict, descriptor: dict):
        self._session = session
        self._descriptor = descriptor

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
