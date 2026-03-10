from __future__ import annotations

import json
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
