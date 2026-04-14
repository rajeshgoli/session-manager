from src.cli import commands


class _RestoreClient:
    def __init__(self, session: dict, restored: dict):
        self._session = session
        self._restored = restored

    def get_session(self, session_id: str):
        if session_id == self._session["id"]:
            return self._session
        return None

    def list_sessions(self, include_stopped: bool = False):
        assert include_stopped is True
        return [self._session]

    def restore_session_result(self, session_id: str):
        if session_id != self._session["id"]:
            return {"ok": False, "unavailable": False, "detail": "not found"}
        return {"ok": True, "unavailable": False, "data": self._restored}

    def get_attach_descriptor(self, session_id: str):
        if session_id != self._session["id"]:
            return None
        return {
            "session_id": session_id,
            "provider": self._restored["provider"],
            "attach_supported": True,
            "tmux_session": self._restored["tmux_session"],
            "runtime_mode": "tmux",
        }


def test_cmd_restore_attaches_to_tmux_session(monkeypatch, capsys):
    calls = []

    def _fake_run(args, check):
        calls.append((args, check))

    monkeypatch.setattr("subprocess.run", _fake_run)
    monkeypatch.setattr(commands.sys.stdin, "isatty", lambda: True)
    monkeypatch.setattr(commands.sys.stdout, "isatty", lambda: True)

    session = {"id": "dead123", "friendly_name": "engineer-ticket2508", "status": "stopped"}
    restored = {
        "id": "dead123",
        "provider": "claude",
        "tmux_session": "claude-dead123",
    }

    rc = commands.cmd_restore(_RestoreClient(session, restored), "engineer-ticket2508")

    assert rc == 0
    assert calls == [(["tmux", "attach", "-t", "claude-dead123"], True)]
    stdout = capsys.readouterr().out
    assert "Session restored: dead123" in stdout


def test_cmd_restore_skips_attach_without_interactive_terminal(monkeypatch, capsys):
    monkeypatch.setattr(commands.sys.stdin, "isatty", lambda: False)
    monkeypatch.setattr(commands.sys.stdout, "isatty", lambda: False)

    session = {"id": "deadtty", "friendly_name": "review-tty", "status": "stopped"}
    restored = {
        "id": "deadtty",
        "provider": "codex-fork",
        "tmux_session": "codex-fork-deadtty",
    }

    rc = commands.cmd_restore(_RestoreClient(session, restored), "review-tty")

    assert rc == 0
    stdout = capsys.readouterr().out
    assert "Session restored: deadtty" in stdout
    assert "Automatic attach skipped: current shell is not interactive." in stdout
    assert "sm attach deadtty" in stdout


def test_cmd_restore_codex_app_is_headless(capsys):
    session = {"id": "deadapp", "friendly_name": "review-app", "status": "stopped"}
    restored = {"id": "deadapp", "provider": "codex-app", "tmux_session": ""}

    rc = commands.cmd_restore(_RestoreClient(session, restored), "review-app")

    assert rc == 0
    stdout = capsys.readouterr().out
    assert "Session restored: deadapp" in stdout
    assert "No tmux attach for Codex app sessions." in stdout
