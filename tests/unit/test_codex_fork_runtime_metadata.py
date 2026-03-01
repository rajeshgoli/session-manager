from src.session_manager import SessionManager


def test_codex_fork_runtime_metadata_uses_config_pin(tmp_path):
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={
            "codex_fork": {
                "command": "codex-fork-bin",
                "args": ["--fast"],
                "event_schema_version": 7,
                "artifact_release": "v1.2.3-sm",
                "artifact_ref": "f00dbabe1234",
                "artifact_platforms": ["darwin-arm64"],
                "rollback_provider": "codex",
                "rollback_command": "sm codex",
            }
        },
    )

    metadata = manager.get_codex_fork_runtime_info()
    assert metadata["command"] == "codex-fork-bin"
    assert metadata["args"] == ["--fast"]
    assert metadata["event_schema_version"] == 7
    assert metadata["artifact_release"] == "v1.2.3-sm"
    assert metadata["artifact_ref"] == "f00dbabe1234"
    assert metadata["artifact_platforms"] == ["darwin-arm64"]
    assert metadata["rollback_provider"] == "codex"
    assert metadata["rollback_command"] == "sm codex"
    assert metadata["is_pinned"] is True


def test_codex_fork_runtime_metadata_defaults_to_unpinned(tmp_path):
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={},
    )

    metadata = manager.get_codex_fork_runtime_info()
    assert metadata["artifact_ref"] == "local-unpinned"
    assert metadata["is_pinned"] is False

