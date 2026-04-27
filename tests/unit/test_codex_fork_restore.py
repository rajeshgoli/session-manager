from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import patch

from src.models import AgentRegistration, Session, SessionStatus
from src.session_manager import SessionManager


def test_load_state_heals_live_stopped_codex_fork_and_preserves_registry(tmp_path):
    state_file = tmp_path / "sessions.json"
    session = Session(
        id="forkheal",
        name="codex-fork-forkheal",
        working_dir=str(tmp_path),
        tmux_session="codex-fork-forkheal",
        log_file=str(tmp_path / "forkheal.log"),
        provider="codex-fork",
        status=SessionStatus.STOPPED,
    )
    registration = AgentRegistration(role="maintainer", session_id=session.id)
    state_file.write_text(
        json.dumps(
            {
                "sessions": [session.to_dict()],
                "em_topic": None,
                "maintainer_session_id": session.id,
                "agent_registrations": [registration.to_dict()],
                "adoption_proposals": [],
            }
        )
    )

    with patch("src.session_manager.TmuxController") as mock_tmux_cls:
        mock_tmux_cls.return_value.session_exists.return_value = True
        with patch.object(SessionManager, "_codex_fork_runtime_reachable", return_value=True):
            manager = SessionManager(
                log_dir=str(tmp_path),
                state_file=str(state_file),
                config={},
            )

    restored = manager.get_session(session.id)
    assert restored is not None
    assert restored.status == SessionStatus.IDLE
    assert manager.maintainer_session_id == session.id
    maintainer = manager.lookup_agent_registration("maintainer")
    assert maintainer is not None
    assert maintainer.session_id == session.id
    assert [s.id for s in manager.list_sessions()] == [session.id]


def test_load_state_preserves_stopped_restore_target_without_tmux_runtime(tmp_path):
    state_file = tmp_path / "sessions.json"
    session = Session(
        id="dead1234",
        name="codex-dead1234",
        working_dir=str(tmp_path / "workspace"),
        tmux_session="codex-dead1234",
        log_file=str(tmp_path / "dead1234.log"),
        provider="codex",
        status=SessionStatus.STOPPED,
        provider_resume_id="019d5bac-3980-7291-8b17-b61f5e618748",
    )
    state_file.write_text(
        json.dumps(
            {
                "sessions": [session.to_dict()],
                "em_topic": None,
                "maintainer_session_id": None,
                "agent_registrations": [],
                "adoption_proposals": [],
            }
        )
    )

    with patch("src.session_manager.TmuxController") as mock_tmux_cls:
        mock_tmux_cls.return_value.session_exists.return_value = False
        manager = SessionManager(
            log_dir=str(tmp_path),
            state_file=str(state_file),
            config={},
        )

    restored = manager.get_session(session.id)
    assert restored is not None
    assert restored.status == SessionStatus.STOPPED
    assert restored.provider_resume_id == "019d5bac-3980-7291-8b17-b61f5e618748"
    assert [s.id for s in manager.list_sessions(include_stopped=True)] == [session.id]
    mock_tmux_cls.return_value.session_exists.assert_not_called()


def test_state_file_expands_user_and_creates_parent_dir(tmp_path, monkeypatch):
    home = tmp_path / "home"
    monkeypatch.setenv("HOME", str(home))

    with patch("src.session_manager.TmuxController") as mock_tmux_cls:
        mock_tmux_cls.return_value.session_exists.return_value = False
        manager = SessionManager(
            log_dir=str(tmp_path / "logs"),
            state_file="~/.local/share/claude-sessions/sessions.json",
            config={},
        )

    expected_state_path = home / ".local" / "share" / "claude-sessions" / "sessions.json"
    assert manager.state_file == expected_state_path
    assert expected_state_path.parent.exists()


def test_load_state_migrates_legacy_tmp_state_file_when_durable_target_missing(tmp_path):
    legacy_state_file = tmp_path / "legacy-sessions.json"
    home = tmp_path / "home"
    durable_state_file = home / ".local" / "share" / "claude-sessions" / "sessions.json"
    session = Session(
        id="legacy123",
        name="claude-legacy123",
        working_dir=str(tmp_path / "workspace"),
        tmux_session="claude-legacy123",
        log_file=str(tmp_path / "legacy.log"),
        provider="claude",
        status=SessionStatus.STOPPED,
        provider_resume_id="resume-legacy123",
    )
    legacy_state_file.write_text(
        json.dumps(
            {
                "sessions": [session.to_dict()],
                "em_topic": None,
                "maintainer_session_id": None,
                "agent_registrations": [],
                "adoption_proposals": [],
            }
        )
    )

    with patch("src.session_manager.LEGACY_TMP_SESSION_STATE_FILE", str(legacy_state_file)):
        with patch("src.session_manager.TmuxController") as mock_tmux_cls:
            mock_tmux_cls.return_value.session_exists.return_value = False
            with patch.dict("os.environ", {"HOME": str(home)}):
                manager = SessionManager(
                    log_dir=str(tmp_path / "logs"),
                    state_file=str(durable_state_file).replace(str(home), "~", 1),
                    config={},
                )

    restored = manager.get_session(session.id)
    assert restored is not None
    assert restored.provider_resume_id == "resume-legacy123"
    assert durable_state_file.exists()
    assert json.loads(durable_state_file.read_text())["sessions"][0]["id"] == session.id
    assert legacy_state_file.exists()


def test_load_state_falls_back_to_legacy_tmp_state_when_migration_copy_fails(tmp_path):
    legacy_state_file = tmp_path / "legacy-sessions.json"
    home = tmp_path / "home"
    durable_state_file = home / ".local" / "share" / "claude-sessions" / "sessions.json"
    session = Session(
        id="legacycopyfail",
        name="claude-legacycopyfail",
        working_dir=str(tmp_path / "workspace"),
        tmux_session="claude-legacycopyfail",
        log_file=str(tmp_path / "legacycopyfail.log"),
        provider="claude",
        status=SessionStatus.STOPPED,
        provider_resume_id="resume-copy-fail",
    )
    legacy_state_file.write_text(
        json.dumps(
            {
                "sessions": [session.to_dict()],
                "em_topic": None,
                "maintainer_session_id": None,
                "agent_registrations": [],
                "adoption_proposals": [],
            }
        )
    )

    with patch("src.session_manager.LEGACY_TMP_SESSION_STATE_FILE", str(legacy_state_file)):
        with patch("src.session_manager.shutil.copy2", side_effect=OSError("disk full")):
            with patch("src.session_manager.TmuxController") as mock_tmux_cls:
                mock_tmux_cls.return_value.session_exists.return_value = False
                with patch.dict("os.environ", {"HOME": str(home)}):
                    manager = SessionManager(
                        log_dir=str(tmp_path / "logs"),
                        state_file=str(durable_state_file).replace(str(home), "~", 1),
                        config={},
                    )

    restored = manager.get_session(session.id)
    assert restored is not None
    assert restored.provider_resume_id == "resume-copy-fail"
    assert not durable_state_file.exists()


def test_load_state_falls_back_to_legacy_when_durable_file_is_unreadable(tmp_path):
    legacy_state_file = tmp_path / "legacy-sessions.json"
    home = tmp_path / "home"
    durable_state_file = home / ".local" / "share" / "claude-sessions" / "sessions.json"
    durable_state_file.parent.mkdir(parents=True, exist_ok=True)
    durable_state_file.write_text("{broken json")
    session = Session(
        id="legacyparsefallback",
        name="claude-legacyparsefallback",
        working_dir=str(tmp_path / "workspace"),
        tmux_session="claude-legacyparsefallback",
        log_file=str(tmp_path / "legacyparsefallback.log"),
        provider="claude",
        status=SessionStatus.STOPPED,
        provider_resume_id="resume-parse-fallback",
    )
    legacy_state_file.write_text(
        json.dumps(
            {
                "sessions": [session.to_dict()],
                "em_topic": None,
                "maintainer_session_id": None,
                "agent_registrations": [],
                "adoption_proposals": [],
            }
        )
    )

    with patch("src.session_manager.LEGACY_TMP_SESSION_STATE_FILE", str(legacy_state_file)):
        with patch("src.session_manager.TmuxController") as mock_tmux_cls:
            mock_tmux_cls.return_value.session_exists.return_value = False
            with patch.dict("os.environ", {"HOME": str(home)}):
                manager = SessionManager(
                    log_dir=str(tmp_path / "logs"),
                    state_file=str(durable_state_file).replace(str(home), "~", 1),
                    config={},
                )

    restored = manager.get_session(session.id)
    assert restored is not None
    assert restored.provider_resume_id == "resume-parse-fallback"
