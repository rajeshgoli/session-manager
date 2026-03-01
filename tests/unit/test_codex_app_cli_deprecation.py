from src.cli import commands


def test_cmd_removed_entrypoint_codex_server(capsys):
    rc = commands.cmd_removed_entrypoint("codex-server")
    assert rc == 1
    assert "sm codex-server has been removed" in capsys.readouterr().err


def test_get_codex_app_policy_for_cli_uses_fallback_defaults():
    class _Client:
        @staticmethod
        def get_rollout_flags():
            return None

    policy = commands._get_codex_app_policy_for_cli(_Client())
    assert policy["phase"] == "pre_cutover"
    assert policy["allow_create"] is True
    assert "provider=codex-app is deprecated" in policy["warning"]

