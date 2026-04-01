from __future__ import annotations

import asyncio
from datetime import datetime, timedelta
from types import SimpleNamespace
from unittest.mock import Mock
from unittest.mock import AsyncMock

from src.models import DeliveryResult, Session, SessionStatus
from src.session_manager import SessionManager


def _manager(tmp_path, config=None) -> SessionManager:
    manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config=config or {},
    )
    manager.tmux = Mock()
    manager.tmux.list_sessions.return_value = []
    manager.tmux.session_exists.return_value = False
    manager.tmux.set_status_bar.return_value = True
    manager.tmux.kill_session.return_value = True
    return manager


def _session(tmp_path, session_id: str) -> Session:
    return Session(
        id=session_id,
        name=f"claude-{session_id}",
        working_dir=str(tmp_path),
        tmux_session=f"claude-{session_id}",
        provider="claude",
        log_file=str(tmp_path / f"{session_id}.log"),
        status=SessionStatus.RUNNING,
    )


def test_reap_kills_completed_auto_bootstrapped_maintainer_after_ttl(tmp_path):
    manager = _manager(
        tmp_path,
        config={
            "service_roles": {
                "maintainer": {
                    "auto_bootstrap": True,
                    "working_dir": str(tmp_path),
                    "friendly_name": "maintainer",
                    "preferred_providers": ["claude"],
                    "bootstrap_prompt": "Act as {role} in {working_dir}.",
                    "task_complete_ttl_seconds": 600,
                }
            }
        },
    )
    session = _session(tmp_path, "maint001")
    session.role = "maintainer"
    session.auto_bootstrapped_role = "maintainer"
    session.agent_task_completed_at = datetime.now() - timedelta(minutes=11)
    manager.sessions[session.id] = session
    manager.register_agent_role(session.id, "maintainer")

    killed = manager.reap_completed_auto_bootstrapped_service_sessions(now=datetime.now())

    assert killed == [session.id]
    assert manager.sessions[session.id].status == SessionStatus.STOPPED
    assert manager.lookup_agent_registration("maintainer") is None


def test_reap_skips_manual_maintainer_even_after_ttl(tmp_path):
    manager = _manager(
        tmp_path,
        config={
            "service_roles": {
                "maintainer": {
                    "auto_bootstrap": True,
                    "working_dir": str(tmp_path),
                    "friendly_name": "maintainer",
                    "preferred_providers": ["claude"],
                    "bootstrap_prompt": "Act as {role} in {working_dir}.",
                    "task_complete_ttl_seconds": 600,
                }
            }
        },
    )
    session = _session(tmp_path, "maint002")
    session.role = "maintainer"
    session.agent_task_completed_at = datetime.now() - timedelta(minutes=11)
    manager.sessions[session.id] = session
    manager.register_agent_role(session.id, "maintainer")

    killed = manager.reap_completed_auto_bootstrapped_service_sessions(now=datetime.now())

    assert killed == []
    assert manager.sessions[session.id].status == SessionStatus.RUNNING
    assert manager.lookup_agent_registration("maintainer") is not None


def test_reap_skips_auto_bootstrapped_maintainer_before_ttl(tmp_path):
    manager = _manager(
        tmp_path,
        config={
            "service_roles": {
                "maintainer": {
                    "auto_bootstrap": True,
                    "working_dir": str(tmp_path),
                    "friendly_name": "maintainer",
                    "preferred_providers": ["claude"],
                    "bootstrap_prompt": "Act as {role} in {working_dir}.",
                    "task_complete_ttl_seconds": 600,
                }
            }
        },
    )
    session = _session(tmp_path, "maint003")
    session.role = "maintainer"
    session.auto_bootstrapped_role = "maintainer"
    session.agent_task_completed_at = datetime.now() - timedelta(minutes=5)
    manager.sessions[session.id] = session
    manager.register_agent_role(session.id, "maintainer")

    killed = manager.reap_completed_auto_bootstrapped_service_sessions(now=datetime.now())

    assert killed == []
    assert manager.sessions[session.id].status == SessionStatus.RUNNING
    assert manager.lookup_agent_registration("maintainer") is not None


def test_new_incoming_work_clears_task_complete_before_reap(tmp_path):
    manager = _manager(
        tmp_path,
        config={
            "service_roles": {
                "maintainer": {
                    "auto_bootstrap": True,
                    "working_dir": str(tmp_path),
                    "friendly_name": "maintainer",
                    "preferred_providers": ["claude"],
                    "bootstrap_prompt": "Act as {role} in {working_dir}.",
                    "task_complete_ttl_seconds": 600,
                }
            }
        },
    )
    session = _session(tmp_path, "maint004")
    session.role = "maintainer"
    session.auto_bootstrapped_role = "maintainer"
    session.agent_task_completed_at = datetime.now() - timedelta(minutes=11)
    manager.sessions[session.id] = session
    manager.register_agent_role(session.id, "maintainer")

    sender = _session(tmp_path, "sender004")
    manager.sessions[sender.id] = sender

    queue = Mock()
    queue.queue_message.return_value = SimpleNamespace(id="msg-004")
    queue.deliver_queued_message_now = AsyncMock(return_value=False)
    manager.message_queue_manager = queue

    result = asyncio.run(
        manager.send_input(
            session.id,
            "new maintainer request",
            sender_session_id=sender.id,
            delivery_mode="sequential",
            from_sm_send=True,
        )
    )

    killed = manager.reap_completed_auto_bootstrapped_service_sessions(now=datetime.now())

    assert result == DeliveryResult.QUEUED
    assert session.agent_task_completed_at is None
    assert killed == []
    assert manager.sessions[session.id].status == SessionStatus.RUNNING
