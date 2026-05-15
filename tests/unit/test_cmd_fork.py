from __future__ import annotations

from src.cli import commands


class _ForkClient:
    def __init__(self, sessions: list[dict], result: dict):
        self._sessions = sessions
        self._result = result
        self.calls: list[dict] = []

    def get_session(self, session_id: str, timeout=None):
        for session in self._sessions:
            if session_id == session["id"]:
                return session
        return None

    def list_sessions(self, include_stopped: bool = False, timeout=None):
        assert include_stopped is True
        return self._sessions

    def fork_session_result(self, session_id: str, *, name=None, requester_session_id=None):
        self.calls.append(
            {
                "session_id": session_id,
                "name": name,
                "requester_session_id": requester_session_id,
            }
        )
        return self._result


def _success_payload():
    return {
        "ok": True,
        "unavailable": False,
        "data": {
            "source_provider_resume_id": "thread-source",
            "fork_provider_resume_id": "thread-fork",
            "source_session": {
                "id": "source01",
                "name": "codex-fork-source01",
                "friendly_name": "maintainer",
                "provider": "codex-fork",
            },
            "fork_session": {
                "id": "fork01",
                "name": "codex-fork-fork01",
                "friendly_name": "maintainer-fork",
                "provider": "codex-fork",
            },
        },
    }


def test_cmd_fork_target_prints_source_and_fork_identities(capsys):
    client = _ForkClient(
        [{"id": "source01", "friendly_name": "maintainer"}],
        _success_payload(),
    )

    rc = commands.cmd_fork(
        client,
        "caller01",
        "maintainer",
        name="maintainer-fork",
    )

    assert rc == 0
    assert client.calls == [
        {
            "session_id": "source01",
            "name": "maintainer-fork",
            "requester_session_id": "caller01",
        }
    ]
    stdout = capsys.readouterr().out
    assert "Forked maintainer (source01)" in stdout
    assert "Original provider thread: thread-source" in stdout
    assert "Fork session: maintainer-fork (fork01)" in stdout
    assert "Fork provider thread: thread-fork" in stdout


def test_cmd_fork_json_output(capsys):
    client = _ForkClient(
        [{"id": "source01", "friendly_name": "maintainer"}],
        _success_payload(),
    )

    rc = commands.cmd_fork(
        client,
        "caller01",
        "source01",
        json_output=True,
    )

    assert rc == 0
    stdout = capsys.readouterr().out
    assert '"source_session_id": "source01"' in stdout
    assert '"fork_session_id": "fork01"' in stdout
    assert '"fork_provider_resume_id": "thread-fork"' in stdout


def test_cmd_fork_self_requires_current_session(capsys):
    client = _ForkClient([], _success_payload())

    rc = commands.cmd_fork(client, None, None, self_target=True)

    assert rc == 2
    stderr = capsys.readouterr().err
    assert "CLAUDE_SESSION_MANAGER_ID not set" in stderr


def test_cmd_fork_rejects_target_and_self(capsys):
    client = _ForkClient([], _success_payload())

    rc = commands.cmd_fork(client, "caller01", "source01", self_target=True)

    assert rc == 1
    stderr = capsys.readouterr().err
    assert "use either a session target or --self" in stderr


def test_cmd_fork_surfaces_api_error(capsys):
    client = _ForkClient(
        [{"id": "source01", "friendly_name": "maintainer"}],
        {"ok": False, "unavailable": False, "detail": "Session forking is not supported"},
    )

    rc = commands.cmd_fork(client, "caller01", "source01")

    assert rc == 1
    stderr = capsys.readouterr().err
    assert "Session forking is not supported" in stderr
