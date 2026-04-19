from src.cli import commands


class _RestoreClient:
    def __init__(self, sessions: list[dict], restored_by_id: dict[str, dict]):
        self._sessions = sessions
        self._restored_by_id = restored_by_id

    def get_session(self, session_id: str):
        for session in self._sessions:
            if session_id == session["id"]:
                return session
        return None

    def list_sessions(self, include_stopped: bool = False):
        assert include_stopped is True
        return self._sessions

    def restore_session_result(self, session_id: str):
        restored = self._restored_by_id.get(session_id)
        if restored is None:
            return {"ok": False, "unavailable": False, "detail": "not found"}
        return {"ok": True, "unavailable": False, "data": restored}

    def get_attach_descriptor(self, session_id: str):
        restored = self._restored_by_id.get(session_id)
        if restored is None:
            return None
        return {
            "session_id": session_id,
            "provider": restored["provider"],
            "attach_supported": True,
            "tmux_session": restored["tmux_session"],
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

    rc = commands.cmd_restore(_RestoreClient([session], {"dead123": restored}), "engineer-ticket2508")

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

    rc = commands.cmd_restore(_RestoreClient([session], {"deadtty": restored}), "review-tty")

    assert rc == 0
    stdout = capsys.readouterr().out
    assert "Session restored: deadtty" in stdout
    assert "Automatic attach skipped: current shell is not interactive." in stdout
    assert "sm attach deadtty" in stdout


def test_cmd_restore_codex_app_is_headless(capsys):
    session = {"id": "deadapp", "friendly_name": "review-app", "status": "stopped"}
    restored = {"id": "deadapp", "provider": "codex-app", "tmux_session": ""}

    rc = commands.cmd_restore(_RestoreClient([session], {"deadapp": restored}), "review-app")

    assert rc == 0
    stdout = capsys.readouterr().out
    assert "Session restored: deadapp" in stdout
    assert "No tmux attach for Codex app sessions." in stdout


def test_cmd_restore_prefers_stopped_friendly_name_match(monkeypatch, capsys):
    monkeypatch.setattr(commands.sys.stdin, "isatty", lambda: False)
    monkeypatch.setattr(commands.sys.stdout, "isatty", lambda: False)

    sessions = [
        {"id": "live123", "friendly_name": "onboarder", "status": "running"},
        {"id": "dead123", "friendly_name": "onboarder", "status": "stopped"},
    ]
    restored = {
        "dead123": {"id": "dead123", "provider": "claude", "tmux_session": "claude-dead123"},
    }

    rc = commands.cmd_restore(_RestoreClient(sessions, restored), "onboarder")

    assert rc == 0
    stdout = capsys.readouterr().out
    assert "Session restored: dead123" in stdout
    assert "sm attach dead123" in stdout


def test_cmd_restore_prefers_stopped_alias_match(monkeypatch, capsys):
    monkeypatch.setattr(commands.sys.stdin, "isatty", lambda: False)
    monkeypatch.setattr(commands.sys.stdout, "isatty", lambda: False)

    sessions = [
        {"id": "live123", "friendly_name": "review-a", "aliases": ["onboarder"], "status": "running"},
        {"id": "dead123", "friendly_name": "review-b", "aliases": ["onboarder"], "status": "stopped"},
    ]
    restored = {
        "dead123": {"id": "dead123", "provider": "codex-fork", "tmux_session": "codex-fork-dead123"},
    }

    rc = commands.cmd_restore(_RestoreClient(sessions, restored), "onboarder")

    assert rc == 0
    stdout = capsys.readouterr().out
    assert "Session restored: dead123" in stdout


def test_cmd_restore_rejects_ambiguous_stopped_name_matches(capsys):
    sessions = [
        {"id": "dead123", "friendly_name": "onboarder", "status": "stopped"},
        {"id": "dead456", "friendly_name": "onboarder", "status": "stopped"},
    ]

    rc = commands.cmd_restore(_RestoreClient(sessions, {}), "onboarder")

    assert rc == 1
    stderr = capsys.readouterr().err
    assert "Multiple stopped sessions match 'onboarder': dead123, dead456. Use a session ID." in stderr
