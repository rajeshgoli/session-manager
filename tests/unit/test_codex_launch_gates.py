from src.models import Session, SessionStatus
from src.session_manager import SessionManager


def test_codex_launch_gates_report_expected_gate_keys(tmp_path):
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={
            "codex_rollout": {
                "enable_durable_events": True,
                "enable_structured_requests": True,
                "enable_observability_projection": True,
                "provider_mapping_phase": "migration_window",
            },
            "codex_fork": {
                "artifact_ref": "abc123",
                "artifact_release": "v1",
                "event_schema_version": 2,
            },
        },
    )
    manager.sessions["app1"] = Session(
        id="app1",
        name="codex-app-app1",
        provider="codex-app",
        working_dir=str(tmp_path),
        tmux_session="",
        log_file="",
        status=SessionStatus.RUNNING,
    )

    payload = manager.get_codex_launch_gates()
    gates = payload["gates"]
    assert "a0_event_schema_contract" in gates
    assert "launch_artifact_pin" in gates
    assert "launch_codex_app_drain" in gates
    assert "launch_provider_mapping_phase" in gates
    assert gates["launch_artifact_pin"]["ok"] is True
    assert gates["launch_codex_app_drain"]["ok"] is False


def test_codex_launch_gates_mark_unpinned_as_fail(tmp_path):
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={},
    )
    payload = manager.get_codex_launch_gates()
    assert payload["gates"]["launch_artifact_pin"]["ok"] is False


def test_codex_launch_gates_ignores_stopped_codex_app_sessions(tmp_path):
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={
            "codex_rollout": {"provider_mapping_phase": "migration_window"},
            "codex_fork": {"artifact_ref": "abc123"},
        },
    )
    manager.sessions["stopped-app"] = Session(
        id="stopped-app",
        name="codex-app-stopped",
        provider="codex-app",
        working_dir=str(tmp_path),
        tmux_session="",
        log_file="",
        status=SessionStatus.STOPPED,
    )

    payload = manager.get_codex_launch_gates()
    assert payload["provider_counts"]["codex-app"] == 0
    assert payload["gates"]["launch_codex_app_drain"]["ok"] is True
