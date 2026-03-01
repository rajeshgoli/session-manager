from src.cli import commands


class _FakeClient:
    def __init__(self, runtime=None, rollout=None):
        self._runtime = runtime
        self._rollout = rollout

    def get_codex_fork_runtime(self):
        return self._runtime

    def get_rollout_flags(self):
        return self._rollout


def test_cmd_codex_fork_info_text_output(capsys):
    client = _FakeClient(
        runtime={
            "command": "codex",
            "args": ["--danger"],
            "artifact_release": "v0.2.0-sm",
            "artifact_ref": "abc123",
            "is_pinned": True,
            "event_schema_version": 2,
            "artifact_platforms": ["darwin-arm64", "linux-x86_64"],
            "rollback_provider": "codex",
            "rollback_command": "sm codex",
        }
    )
    rc = commands.cmd_codex_fork_info(client)
    assert rc == 0
    output = capsys.readouterr().out
    assert "Codex-fork runtime metadata" in output
    assert "artifact_ref: abc123" in output
    assert "event_schema_version: 2" in output


def test_cmd_codex_fork_info_json_output(capsys):
    client = _FakeClient(
        runtime={
            "artifact_ref": "abc123",
            "event_schema_version": 2,
            "is_pinned": True,
        }
    )
    rc = commands.cmd_codex_fork_info(client, json_output=True)
    assert rc == 0
    output = capsys.readouterr().out
    assert "\"artifact_ref\": \"abc123\"" in output


def test_cmd_codex_fork_info_unavailable(capsys):
    client = _FakeClient(runtime=None, rollout=None)
    rc = commands.cmd_codex_fork_info(client)
    assert rc == 1
    assert "endpoint unavailable or incompatible" in capsys.readouterr().err
