from src.models import Session, SessionStatus
from src.session_manager import SessionManager


class _FakeQueueManager:
    def __init__(self):
        self.calls = []

    def retire_session_queue(self, session_id: str, reason: str) -> int:
        self.calls.append((session_id, reason))
        return 3


def test_retire_codex_app_sessions_marks_sessions_stopped(tmp_path):
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={},
    )
    queue = _FakeQueueManager()
    manager.message_queue_manager = queue

    session = Session(
        id="app12345",
        name="codex-app-app12345",
        provider="codex-app",
        working_dir=str(tmp_path),
        tmux_session="",
        log_file="",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    retired = manager.retire_codex_app_sessions(reason="provider_retired_codex_app")
    assert retired == 1
    assert session.status == SessionStatus.STOPPED
    assert session.completion_message == "provider_retired_codex_app"
    assert session.error_message == "provider_retired_codex_app"
    assert queue.calls == [(session.id, "provider_retired_codex_app")]


def test_post_cutover_load_retires_restored_codex_app_session(tmp_path):
    state_file = tmp_path / "sessions.json"
    state_file.write_text(
        '{"sessions":[{"id":"app00001","name":"codex-app-app00001","working_dir":"%s","tmux_session":"",'
        '"provider":"codex-app","log_file":"","status":"running","created_at":"2026-01-01T00:00:00",'
        '"last_activity":"2026-01-01T00:00:00","telegram_chat_id":null,"telegram_thread_id":null,'
        '"error_message":null,"transcript_path":null,"friendly_name":null,"current_task":null,'
        '"git_remote_url":null,"subagents":[],"codex_thread_id":"thr_1","review_config":null,'
        '"parent_session_id":null,"spawn_prompt":null,"completion_status":null,"completion_message":null,'
        '"spawned_at":null,"completed_at":null,"tokens_used":0,"tools_used":{},"last_tool_call":null,'
        '"last_tool_name":null,"touched_repos":[],"worktrees":[],"cleanup_prompted":{},'
        '"recovery_count":0,"last_handoff_path":null,"context_monitor_enabled":false,'
        '"context_monitor_notify":null,"is_em":false,"role":null,"agent_status_text":null,'
        '"agent_status_at":null,"agent_task_completed_at":null}]}'
        % str(tmp_path).replace("\\", "\\\\")
    )

    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(state_file),
        config={"codex_rollout": {"provider_mapping_phase": "post_cutover"}},
    )
    restored = manager.get_session("app00001")
    assert restored is not None
    assert restored.status == SessionStatus.STOPPED
    assert restored.error_message == "provider_retired_codex_app"
    state_after = state_file.read_text()
    assert '"status": "stopped"' in state_after
    assert '"error_message": "provider_retired_codex_app"' in state_after


def test_retire_codex_app_sessions_cleans_queue_for_already_retired(tmp_path):
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={},
    )
    queue = _FakeQueueManager()
    manager.message_queue_manager = queue

    session = Session(
        id="retired001",
        name="codex-app-retired001",
        provider="codex-app",
        working_dir=str(tmp_path),
        tmux_session="",
        log_file="",
        status=SessionStatus.STOPPED,
    )
    session.error_message = "provider_retired_codex_app"
    manager.sessions[session.id] = session

    retired = manager.retire_codex_app_sessions(reason="provider_retired_codex_app")
    assert retired == 0
    assert queue.calls == [(session.id, "provider_retired_codex_app")]
