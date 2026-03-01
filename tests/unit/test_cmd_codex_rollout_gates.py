from src.cli import commands


class _FakeClient:
    def __init__(self, payload=None):
        self._payload = payload

    def get_codex_launch_gates(self):
        return self._payload


def test_cmd_codex_rollout_gates_text_output(capsys):
    client = _FakeClient(
        payload={
            "gates": {
                "a0_event_schema_contract": {"ok": True, "details": "event_schema_version=2"},
                "launch_artifact_pin": {"ok": False, "details": "artifact_ref=local-unpinned"},
            },
            "provider_counts": {"codex-app": 1},
            "codex_provider_policy": {"phase": "pre_cutover"},
        }
    )
    rc = commands.cmd_codex_rollout_gates(client)
    assert rc == 0
    out = capsys.readouterr().out
    assert "Codex launch gate snapshot" in out
    assert "[PASS] a0_event_schema_contract" in out
    assert "[FAIL] launch_artifact_pin" in out
    assert "provider_mapping_phase: pre_cutover" in out


def test_cmd_codex_rollout_gates_json_output(capsys):
    client = _FakeClient(payload={"gates": {"launch_artifact_pin": {"ok": True}}})
    rc = commands.cmd_codex_rollout_gates(client, json_output=True)
    assert rc == 0
    assert "\"launch_artifact_pin\"" in capsys.readouterr().out


def test_cmd_codex_rollout_gates_unavailable(capsys):
    client = _FakeClient(payload=None)
    rc = commands.cmd_codex_rollout_gates(client)
    assert rc == 1
    assert "Failed to fetch codex launch gate snapshot" in capsys.readouterr().err

