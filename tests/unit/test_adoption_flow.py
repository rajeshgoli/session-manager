from __future__ import annotations

from unittest.mock import Mock, patch

from fastapi.testclient import TestClient

from src.models import AdoptionProposalStatus, Session, SessionStatus
from src.server import create_app
from src.session_manager import SessionManager
from src.cli.commands import cmd_adopt


def _session(
    session_id: str,
    tmp_path,
    *,
    is_em: bool = False,
    parent_session_id: str | None = None,
) -> Session:
    return Session(
        id=session_id,
        name=f"claude-{session_id}",
        working_dir=str(tmp_path),
        tmux_session=f"claude-{session_id}",
        provider="claude",
        log_file=str(tmp_path / f"{session_id}.log"),
        status=SessionStatus.RUNNING,
        is_em=is_em,
        role="em" if is_em else None,
        parent_session_id=parent_session_id,
    )


def _manager(tmp_path) -> SessionManager:
    return SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={},
    )


def test_adoption_proposal_persists_and_accept_rebinds_parent(tmp_path):
    manager = _manager(tmp_path)
    proposer = _session("em123456", tmp_path, is_em=True)
    target = _session("child001", tmp_path)
    manager.sessions[proposer.id] = proposer
    manager.sessions[target.id] = target

    proposal = manager.create_adoption_proposal(proposer.id, target.id)
    assert proposal.status == AdoptionProposalStatus.PENDING

    with patch("src.session_manager.TmuxController.session_exists", return_value=True):
        restored = SessionManager(
            log_dir=str(tmp_path / "logs"),
            state_file=str(tmp_path / "sessions.json"),
            config={},
        )

    restored_proposals = restored.list_adoption_proposals(
        target_session_id=target.id,
        status=AdoptionProposalStatus.PENDING,
    )
    assert len(restored_proposals) == 1
    assert restored_proposals[0].id == proposal.id

    accepted = restored.decide_adoption_proposal(proposal.id, accepted=True)
    assert accepted.status == AdoptionProposalStatus.ACCEPTED
    assert restored.get_session(target.id).parent_session_id == proposer.id


def test_adoption_proposal_requires_em(tmp_path):
    manager = _manager(tmp_path)
    proposer = _session("worker001", tmp_path, is_em=False)
    target = _session("child001", tmp_path)
    manager.sessions[proposer.id] = proposer
    manager.sessions[target.id] = target

    try:
        manager.create_adoption_proposal(proposer.id, target.id)
    except ValueError as exc:
        assert "Only EM sessions" in str(exc)
    else:
        raise AssertionError("Expected ValueError for non-EM proposer")


def test_sessions_api_exposes_pending_adoption_proposals(tmp_path):
    manager = _manager(tmp_path)
    proposer = _session("em123456", tmp_path, is_em=True)
    proposer.friendly_name = "em-ops"
    target = _session("child001", tmp_path)
    manager.sessions[proposer.id] = proposer
    manager.sessions[target.id] = target
    proposal = manager.create_adoption_proposal(proposer.id, target.id)

    client = TestClient(create_app(session_manager=manager))
    response = client.get("/sessions")

    assert response.status_code == 200
    sessions = response.json()["sessions"]
    target_payload = next(item for item in sessions if item["id"] == target.id)
    assert target_payload["pending_adoption_proposals"] == [
        {
            "id": proposal.id,
            "proposer_session_id": proposer.id,
            "proposer_name": "em-ops",
            "target_session_id": target.id,
            "created_at": proposal.created_at.isoformat(),
            "status": "pending",
            "decided_at": None,
        }
    ]


def test_accept_adoption_route_rebinds_parent(tmp_path):
    manager = _manager(tmp_path)
    proposer = _session("em123456", tmp_path, is_em=True)
    target = _session("child001", tmp_path)
    manager.sessions[proposer.id] = proposer
    manager.sessions[target.id] = target
    proposal = manager.create_adoption_proposal(proposer.id, target.id)

    client = TestClient(create_app(session_manager=manager))
    response = client.post(f"/adoption-proposals/{proposal.id}/accept", json={})

    assert response.status_code == 200
    assert response.json()["status"] == "accepted"
    assert manager.get_session(target.id).parent_session_id == proposer.id


def test_cmd_adopt_submits_proposal(capsys):
    client = Mock()
    client.get_session.return_value = {"id": "child001", "parent_session_id": None}
    client.propose_adoption.return_value = {
        "ok": True,
        "unavailable": False,
        "data": {"proposal": {"id": "proposal123"}},
    }

    rc = cmd_adopt(client, "em123456", "child001")

    assert rc == 0
    client.propose_adoption.assert_called_once_with("child001", "em123456")
    assert "proposal123" in capsys.readouterr().out
