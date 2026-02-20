"""Regression tests for sm#271: Telegram thread cleanup for completed sessions.

Three fixes:

Fix A — close_session_topic() in OutputMonitor + called by ChildMonitor on completion
Fix B — EM session thread continuity: inherit previous EM topic on sm em
Fix C — POST /admin/cleanup-idle-topics endpoint

Spec: docs/specs/271_telegram_thread_cleanup.md
"""

import json
import tempfile
from pathlib import Path
from unittest.mock import AsyncMock, Mock

import pytest
from fastapi.testclient import TestClient

from src.child_monitor import ChildMonitor
from src.models import CompletionStatus, Session, SessionStatus
from src.output_monitor import OutputMonitor
from src.server import create_app


# ============================================================================
# Shared helpers
# ============================================================================


def _make_forum_session(
    session_id="child-001",
    chat_id=10000,
    thread_id=50000,
    status=SessionStatus.IDLE,
    completion_status=None,
    is_em=False,
):
    return Session(
        id=session_id,
        name=session_id,
        working_dir="/tmp",
        tmux_session=f"claude-{session_id}",
        log_file=f"/tmp/{session_id}.log",
        status=status,
        telegram_chat_id=chat_id,
        telegram_thread_id=thread_id,
        completion_status=completion_status,
        is_em=is_em,
    )


def _make_telegram_bot(send_forum_returns=999):
    """Build a minimal mock TelegramBot."""
    tg = Mock()
    tg.bot = AsyncMock()
    tg._topic_sessions = {}
    tg._session_threads = {}
    tg.send_with_fallback = AsyncMock(return_value=send_forum_returns)
    tg.delete_forum_topic = AsyncMock(return_value=True)
    tg.reopen_forum_topic = AsyncMock(return_value=True)
    tg.create_forum_topic = AsyncMock(return_value=99999)
    tg.register_topic_session = Mock()
    return tg


def _make_session_manager(sessions: dict, telegram_bot=None):
    mgr = Mock()
    mgr.sessions = sessions
    mgr._save_state = Mock()
    mgr.em_topic = None
    if telegram_bot:
        notifier = Mock()
        notifier.telegram = telegram_bot
        mgr.notifier = notifier
    else:
        mgr.notifier = None
    return mgr


def _make_output_monitor(session_manager):
    monitor = OutputMonitor(poll_interval=0.1)
    monitor.set_session_manager(session_manager)
    monitor.set_save_state_callback(session_manager._save_state)
    return monitor


# ============================================================================
# Fix A — close_session_topic()
# ============================================================================


@pytest.mark.asyncio
async def test_close_session_topic_sends_message_and_closes_forum_topic():
    """close_session_topic() sends a completion message and closes the forum topic."""
    session = _make_forum_session()
    chat_id = session.telegram_chat_id
    thread_id = session.telegram_thread_id

    tg = _make_telegram_bot(send_forum_returns=1001)
    tg._topic_sessions[(chat_id, thread_id)] = session.id
    tg._session_threads[session.id] = (chat_id, thread_id)

    mgr = _make_session_manager({session.id: session}, tg)
    monitor = _make_output_monitor(mgr)

    await monitor.close_session_topic(session, message="Work done")

    # Completion message sent via send_with_fallback
    tg.send_with_fallback.assert_called_once_with(
        chat_id=chat_id,
        message=f"Session completed [{session.id}]: Work done",
        thread_id=thread_id,
    )

    # Forum topic closed via bot
    tg.bot.close_forum_topic.assert_called_once_with(
        chat_id=chat_id, message_thread_id=thread_id
    )

    # In-memory mappings cleaned up
    assert (chat_id, thread_id) not in tg._topic_sessions
    assert session.id not in tg._session_threads


@pytest.mark.asyncio
async def test_close_session_topic_session_remains_in_sessions_dict():
    """close_session_topic() does NOT remove the session from session_manager.sessions."""
    session = _make_forum_session()
    tg = _make_telegram_bot()
    mgr = _make_session_manager({session.id: session}, tg)
    monitor = _make_output_monitor(mgr)

    await monitor.close_session_topic(session, message="Completed")

    # Session still in dict (unlike cleanup_session which removes it)
    assert session.id in mgr.sessions


@pytest.mark.asyncio
async def test_close_session_topic_nulls_thread_id_to_prevent_double_close():
    """After close_session_topic(), session.telegram_thread_id is None."""
    session = _make_forum_session()
    tg = _make_telegram_bot()
    mgr = _make_session_manager({session.id: session}, tg)
    monitor = _make_output_monitor(mgr)

    await monitor.close_session_topic(session, message="Completed")

    assert session.telegram_thread_id is None
    mgr._save_state.assert_called()


@pytest.mark.asyncio
async def test_close_session_topic_no_telegram_configured():
    """close_session_topic() on session without Telegram: no calls, no error."""
    session = Session(
        id="no-tg",
        name="no-tg",
        working_dir="/tmp",
        tmux_session="claude-no-tg",
        log_file="/tmp/no-tg.log",
        status=SessionStatus.IDLE,
    )
    tg = _make_telegram_bot()
    mgr = _make_session_manager({session.id: session}, tg)
    monitor = _make_output_monitor(mgr)

    # Must not raise
    await monitor.close_session_topic(session, message="Completed")

    tg.send_with_fallback.assert_not_called()
    tg.bot.close_forum_topic.assert_not_called()


@pytest.mark.asyncio
async def test_close_session_topic_forum_send_fails_skips_close():
    """If send_with_fallback returns None (forum failed), close_forum_topic is not called."""
    session = _make_forum_session()
    tg = _make_telegram_bot(send_forum_returns=None)
    mgr = _make_session_manager({session.id: session}, tg)
    monitor = _make_output_monitor(mgr)

    await monitor.close_session_topic(session, message="Completed")

    # send attempted
    tg.send_with_fallback.assert_called_once()
    # but forum close NOT called (forum send failed → not a confirmed forum topic)
    tg.bot.close_forum_topic.assert_not_called()
    # mappings still cleaned up
    assert session.id not in tg._session_threads


@pytest.mark.asyncio
async def test_close_session_topic_forum_close_error_does_not_raise():
    """If close_forum_topic raises, close_session_topic handles it gracefully."""
    session = _make_forum_session()
    tg = _make_telegram_bot(send_forum_returns=555)
    tg.bot.close_forum_topic = AsyncMock(side_effect=Exception("API error"))

    mgr = _make_session_manager({session.id: session}, tg)
    monitor = _make_output_monitor(mgr)

    # Must not raise
    await monitor.close_session_topic(session, message="Completed")

    # Mappings still cleaned up despite close_forum_topic failure
    assert session.id not in tg._session_threads
    assert session.telegram_thread_id is None


@pytest.mark.asyncio
async def test_close_session_topic_codex_app_session_isolation():
    """close_session_topic() on a codex-app session only affects Telegram mappings.

    codex_sessions dict must remain unchanged — no resource leak.
    Spec requirement: Fix A unit test for codex-app session isolation.
    """
    from src.models import Session

    # Create a codex-app session with a Telegram forum topic
    codex_session = Session(
        id="codex-app-001",
        name="codex-app-001",
        working_dir="/tmp",
        tmux_session="",
        log_file="/tmp/codex-app-001.log",
        provider="codex-app",
        status=SessionStatus.IDLE,
        telegram_chat_id=10000,
        telegram_thread_id=50000,
    )

    tg = _make_telegram_bot(send_forum_returns=1234)
    tg._topic_sessions[(10000, 50000)] = codex_session.id
    tg._session_threads[codex_session.id] = (10000, 50000)

    # A sentinel object representing the CodexAppServerSession in codex_sessions
    sentinel_codex_app = object()

    mgr = _make_session_manager({codex_session.id: codex_session}, tg)
    # Simulate codex_sessions dict on the session manager
    mgr.codex_sessions = {codex_session.id: sentinel_codex_app}

    monitor = _make_output_monitor(mgr)

    await monitor.close_session_topic(codex_session, message="Codex task done")

    # Telegram mappings cleaned up (expected behaviour)
    assert (10000, 50000) not in tg._topic_sessions
    assert codex_session.id not in tg._session_threads
    assert codex_session.telegram_thread_id is None

    # codex_sessions dict untouched — no resource leak
    assert codex_session.id in mgr.codex_sessions
    assert mgr.codex_sessions[codex_session.id] is sentinel_codex_app


# ============================================================================
# Fix A — ChildMonitor calls close_session_topic on completion
# ============================================================================


@pytest.mark.asyncio
async def test_child_monitor_calls_close_session_topic_on_completion():
    """ChildMonitor calls close_session_topic after marking child COMPLETED."""
    child = _make_forum_session(session_id="child-abc", status=SessionStatus.IDLE)
    parent = Session(
        id="parent-xyz",
        name="parent",
        working_dir="/tmp",
        tmux_session="claude-parent-xyz",
        log_file="/tmp/parent.log",
        status=SessionStatus.IDLE,
    )

    session_manager = Mock()
    session_manager.sessions = {child.id: child, parent.id: parent}
    session_manager.get_session = Mock(side_effect=lambda sid: session_manager.sessions.get(sid))
    session_manager._save_state = Mock()
    session_manager.send_input = AsyncMock(return_value=__import__("src.models", fromlist=["DeliveryResult"]).DeliveryResult.DELIVERED)

    child_monitor = ChildMonitor(session_manager)

    output_monitor = Mock()
    output_monitor.close_session_topic = AsyncMock()
    child_monitor.set_output_monitor(output_monitor)

    # Note: _notify_parent_completion signature is (parent_id, child_id, message)
    await child_monitor._notify_parent_completion(parent.id, child.id, "Task finished")

    # close_session_topic called with the child session and the completion message
    output_monitor.close_session_topic.assert_called_once_with(
        child,
        message="Task finished",
    )


@pytest.mark.asyncio
async def test_child_monitor_no_output_monitor_does_not_raise():
    """ChildMonitor without output_monitor set: notification works, no crash."""
    from src.models import DeliveryResult

    child = _make_forum_session(session_id="child-noom", status=SessionStatus.IDLE)
    parent = Session(
        id="parent-noom",
        name="parent",
        working_dir="/tmp",
        tmux_session="claude-parent-noom",
        log_file="/tmp/parent-noom.log",
        status=SessionStatus.IDLE,
    )

    session_manager = Mock()
    session_manager.sessions = {child.id: child, parent.id: parent}
    session_manager.get_session = Mock(side_effect=lambda sid: session_manager.sessions.get(sid))
    session_manager._save_state = Mock()
    session_manager.send_input = AsyncMock(return_value=DeliveryResult.DELIVERED)

    child_monitor = ChildMonitor(session_manager)
    # _output_monitor is None by default

    # Must not raise — note: (parent_id, child_id, message)
    await child_monitor._notify_parent_completion(parent.id, child.id, "Done")


@pytest.mark.asyncio
async def test_child_monitor_completion_marks_session_completed():
    """ChildMonitor marks child completion_status=COMPLETED after notification."""
    from src.models import DeliveryResult

    child = _make_forum_session(session_id="child-cc", status=SessionStatus.IDLE)
    parent = Session(
        id="parent-cc",
        name="parent",
        working_dir="/tmp",
        tmux_session="claude-parent-cc",
        log_file="/tmp/parent-cc.log",
        status=SessionStatus.IDLE,
    )

    session_manager = Mock()
    session_manager.sessions = {child.id: child, parent.id: parent}
    session_manager.get_session = Mock(side_effect=lambda sid: session_manager.sessions.get(sid))
    session_manager._save_state = Mock()
    session_manager.send_input = AsyncMock(return_value=DeliveryResult.DELIVERED)

    child_monitor = ChildMonitor(session_manager)
    output_monitor = Mock()
    output_monitor.close_session_topic = AsyncMock()
    child_monitor.set_output_monitor(output_monitor)

    # Note: (parent_id, child_id, message)
    await child_monitor._notify_parent_completion(parent.id, child.id, "Finished work")

    assert child.completion_status == CompletionStatus.COMPLETED
    assert child.completion_message == "Finished work"
    # Session remains in sessions dict
    assert child.id in session_manager.sessions


# ============================================================================
# Fix B — EM thread continuity
# ============================================================================


def _make_app_for_em_tests(session, session_manager, telegram_bot):
    """Build a TestClient app wired to a mock session manager and telegram bot."""
    notifier = Mock()
    notifier.telegram = telegram_bot
    notifier.rename_session_topic = AsyncMock(return_value=True)

    session_manager.sessions = {session.id: session}
    session_manager.get_session = Mock(side_effect=lambda sid: session_manager.sessions.get(sid))

    tmux = Mock()
    tmux.set_status_bar = Mock()
    session_manager.tmux = tmux

    mqm = Mock()
    mqm.cancel_remind = Mock()
    mqm.cancel_parent_wake = Mock()
    mqm.cancel_context_monitor_messages_from = Mock()
    mqm.delivery_states = {}
    session_manager.message_queue_manager = mqm

    app = create_app(
        session_manager=session_manager,
        notifier=notifier,
        output_monitor=None,
        config={},
    )
    return TestClient(app)


def test_em_topic_persisted_when_no_previous_em_topic():
    """First time is_em=True: no previous em_topic → keep new topic, persist em_topic."""
    session = _make_forum_session(session_id="em-first", chat_id=10000, thread_id=42000)
    tg = _make_telegram_bot()

    mgr = Mock()
    mgr.em_topic = None  # No previous EM topic

    client = _make_app_for_em_tests(session, mgr, tg)

    resp = client.patch(f"/sessions/{session.id}", json={"is_em": True})
    assert resp.status_code == 200

    # delete_forum_topic NOT called — new topic kept
    tg.delete_forum_topic.assert_not_called()
    # em_topic set to the new topic
    assert mgr.em_topic == {"chat_id": 10000, "thread_id": 42000}
    mgr._save_state.assert_called()


def test_em_topic_inherits_previous_em_topic():
    """Second EM session: previous em_topic found → delete new, reopen old, inherit."""
    session = _make_forum_session(session_id="em-second", chat_id=10000, thread_id=99999)
    tg = _make_telegram_bot()

    mgr = Mock()
    mgr.em_topic = {"chat_id": 10000, "thread_id": 42000}  # Previous EM topic

    client = _make_app_for_em_tests(session, mgr, tg)

    resp = client.patch(f"/sessions/{session.id}", json={"is_em": True})
    assert resp.status_code == 200

    # New topic (99999) deleted
    tg.delete_forum_topic.assert_called_once_with(10000, 99999)
    # Old topic (42000) reopened
    tg.reopen_forum_topic.assert_called_once_with(10000, 42000)
    # Session's thread_id now points to the inherited topic
    assert session.telegram_thread_id == 42000
    # em_topic updated to the inherited topic
    assert mgr.em_topic == {"chat_id": 10000, "thread_id": 42000}
    mgr._save_state.assert_called()


def test_em_topic_different_chat_id_keeps_new_topic():
    """Previous em_topic exists but different chat_id → keep new topic (no cross-chat inheritance)."""
    session = _make_forum_session(session_id="em-diff-chat", chat_id=20000, thread_id=55555)
    tg = _make_telegram_bot()

    mgr = Mock()
    mgr.em_topic = {"chat_id": 10000, "thread_id": 42000}  # Different chat

    client = _make_app_for_em_tests(session, mgr, tg)

    resp = client.patch(f"/sessions/{session.id}", json={"is_em": True})
    assert resp.status_code == 200

    # No deletion — different chat, no inheritance
    tg.delete_forum_topic.assert_not_called()
    # em_topic updated to new session's topic
    assert mgr.em_topic == {"chat_id": 20000, "thread_id": 55555}


def test_em_topic_inheritance_delete_fails_keeps_new_topic():
    """delete_forum_topic fails → abort inheritance, keep new topic."""
    session = _make_forum_session(session_id="em-del-fail", chat_id=10000, thread_id=77777)
    tg = _make_telegram_bot()
    tg.delete_forum_topic = AsyncMock(return_value=False)  # Delete fails

    mgr = Mock()
    mgr.em_topic = {"chat_id": 10000, "thread_id": 42000}

    client = _make_app_for_em_tests(session, mgr, tg)

    resp = client.patch(f"/sessions/{session.id}", json={"is_em": True})
    assert resp.status_code == 200

    # Delete attempted
    tg.delete_forum_topic.assert_called_once()
    # Reopen NOT called (abort after delete failure)
    tg.reopen_forum_topic.assert_not_called()
    # Session keeps its new topic
    assert session.telegram_thread_id == 77777
    # em_topic updated to the kept new topic
    assert mgr.em_topic == {"chat_id": 10000, "thread_id": 77777}


def test_em_topic_inheritance_reopen_fails_creates_new_topic():
    """delete succeeds but reopen fails → create brand-new topic."""
    session = _make_forum_session(session_id="em-reopen-fail", chat_id=10000, thread_id=88888)
    tg = _make_telegram_bot()
    tg.delete_forum_topic = AsyncMock(return_value=True)
    tg.reopen_forum_topic = AsyncMock(return_value=False)  # Reopen fails
    tg.create_forum_topic = AsyncMock(return_value=11111)  # Brand-new topic

    mgr = Mock()
    mgr.em_topic = {"chat_id": 10000, "thread_id": 42000}

    client = _make_app_for_em_tests(session, mgr, tg)

    resp = client.patch(f"/sessions/{session.id}", json={"is_em": True})
    assert resp.status_code == 200

    # delete + reopen both attempted
    tg.delete_forum_topic.assert_called_once()
    tg.reopen_forum_topic.assert_called_once()
    # create_forum_topic called as fallback
    tg.create_forum_topic.assert_called_once()
    # em_topic updated to brand-new topic
    assert mgr.em_topic is not None
    assert mgr.em_topic["thread_id"] == 11111


def test_em_topic_inheritance_success_clears_stale_topic_sessions():
    """After delete succeeds in success path, stale _topic_sessions entry is removed."""
    new_thread_id = 99999
    old_thread_id = 42000
    session = _make_forum_session(session_id="em-stale-success", chat_id=10000, thread_id=new_thread_id)
    tg = _make_telegram_bot()
    # Pre-populate the stale entry for the new (about-to-be-deleted) topic
    tg._topic_sessions[(10000, new_thread_id)] = session.id

    mgr = Mock()
    mgr.em_topic = {"chat_id": 10000, "thread_id": old_thread_id}

    client = _make_app_for_em_tests(session, mgr, tg)

    resp = client.patch(f"/sessions/{session.id}", json={"is_em": True})
    assert resp.status_code == 200

    # Stale _topic_sessions entry for the deleted topic must be removed
    assert (10000, new_thread_id) not in tg._topic_sessions
    # Session now points to inherited topic
    assert session.telegram_thread_id == old_thread_id


def test_em_topic_inheritance_reopen_fail_clears_stale_topic_sessions():
    """After delete succeeds in reopen-fail path, stale _topic_sessions entry is removed."""
    new_thread_id = 88888
    session = _make_forum_session(session_id="em-stale-reopen-fail", chat_id=10000, thread_id=new_thread_id)
    tg = _make_telegram_bot()
    tg.delete_forum_topic = AsyncMock(return_value=True)
    tg.reopen_forum_topic = AsyncMock(return_value=False)
    tg.create_forum_topic = AsyncMock(return_value=22222)
    # Pre-populate stale entry
    tg._topic_sessions[(10000, new_thread_id)] = session.id

    mgr = Mock()
    mgr.em_topic = {"chat_id": 10000, "thread_id": 42000}

    client = _make_app_for_em_tests(session, mgr, tg)

    resp = client.patch(f"/sessions/{session.id}", json={"is_em": True})
    assert resp.status_code == 200

    # Stale _topic_sessions entry for the deleted topic must be removed
    assert (10000, new_thread_id) not in tg._topic_sessions
    # em_topic points to brand-new topic (not the deleted one)
    assert mgr.em_topic["thread_id"] == 22222


def test_em_topic_inheritance_create_forum_topic_returns_none_nulls_thread():
    """reopen fails + create_forum_topic returns None → thread_id=None, deleted ID not persisted."""
    new_thread_id = 77777
    session = _make_forum_session(session_id="em-create-none", chat_id=10000, thread_id=new_thread_id)
    tg = _make_telegram_bot()
    tg.delete_forum_topic = AsyncMock(return_value=True)
    tg.reopen_forum_topic = AsyncMock(return_value=False)
    tg.create_forum_topic = AsyncMock(return_value=None)  # Total failure

    mgr = Mock()
    mgr.em_topic = {"chat_id": 10000, "thread_id": 42000}

    client = _make_app_for_em_tests(session, mgr, tg)

    resp = client.patch(f"/sessions/{session.id}", json={"is_em": True})
    assert resp.status_code == 200

    # Invariant: session must not point to the deleted topic
    assert session.telegram_thread_id is None
    # em_topic must not be persisted with the deleted topic ID
    # (em_topic may be unchanged from previous value, but never the deleted new_thread_id)
    if mgr.em_topic is not None:
        assert mgr.em_topic.get("thread_id") != new_thread_id


def test_em_topic_inheritance_clears_old_em_sessions():
    """Old EM sessions' telegram_thread_id cleared to prevent double-close."""
    old_em = _make_forum_session(
        session_id="old-em", chat_id=10000, thread_id=42000, is_em=True
    )
    new_em = _make_forum_session(
        session_id="new-em", chat_id=10000, thread_id=99999
    )
    tg = _make_telegram_bot()
    tg._session_threads["old-em"] = (10000, 42000)

    mgr = Mock()
    mgr.em_topic = {"chat_id": 10000, "thread_id": 42000}
    mgr.sessions = {old_em.id: old_em, new_em.id: new_em}
    mgr.get_session = Mock(side_effect=lambda sid: mgr.sessions.get(sid))

    tmux = Mock()
    tmux.set_status_bar = Mock()
    mgr.tmux = tmux
    mgr._save_state = Mock()

    mqm = Mock()
    mqm.cancel_remind = Mock()
    mqm.cancel_parent_wake = Mock()
    mqm.cancel_context_monitor_messages_from = Mock()
    mqm.delivery_states = {}
    mgr.message_queue_manager = mqm

    notifier = Mock()
    notifier.telegram = tg
    notifier.rename_session_topic = AsyncMock(return_value=True)

    app = create_app(
        session_manager=mgr,
        notifier=notifier,
        output_monitor=None,
        config={},
    )
    client = TestClient(app)

    resp = client.patch(f"/sessions/{new_em.id}", json={"is_em": True})
    assert resp.status_code == 200

    # Old EM session's thread_id cleared (prevents double-close when cleanup_session fires)
    assert old_em.telegram_thread_id is None


def test_em_topic_no_telegram_configured_no_inheritance():
    """Session with no Telegram thread: skip inheritance entirely."""
    session = Session(
        id="em-no-tg",
        name="em-no-tg",
        working_dir="/tmp",
        tmux_session="claude-em-no-tg",
        log_file="/tmp/em-no-tg.log",
        status=SessionStatus.IDLE,
        # No telegram_chat_id / telegram_thread_id
    )
    tg = _make_telegram_bot()

    mgr = Mock()
    mgr.em_topic = {"chat_id": 10000, "thread_id": 42000}

    client = _make_app_for_em_tests(session, mgr, tg)

    resp = client.patch(f"/sessions/{session.id}", json={"is_em": True})
    assert resp.status_code == 200

    # No Telegram calls — no topic to inherit
    tg.delete_forum_topic.assert_not_called()
    tg.reopen_forum_topic.assert_not_called()


def test_em_topic_load_state_backward_compat(tmp_path):
    """_load_state() reads em_topic field; missing field → em_topic=None (backward compat)."""
    import json
    from src.session_manager import SessionManager

    # State file without em_topic (legacy format)
    state_file = tmp_path / "sessions.json"
    state_file.write_text(json.dumps({"sessions": []}))

    mgr = SessionManager(log_dir=str(tmp_path), state_file=str(state_file))
    assert mgr.em_topic is None


def test_em_topic_load_state_reads_em_topic_field(tmp_path):
    """_load_state() reads em_topic field from sessions.json."""
    import json
    from src.session_manager import SessionManager

    em_topic_data = {"chat_id": 12345, "thread_id": 67890}
    state_file = tmp_path / "sessions.json"
    state_file.write_text(json.dumps({"sessions": [], "em_topic": em_topic_data}))

    mgr = SessionManager(log_dir=str(tmp_path), state_file=str(state_file))
    assert mgr.em_topic == em_topic_data


def test_em_topic_save_state_writes_em_topic_field(tmp_path):
    """_save_state() writes em_topic field to sessions.json."""
    import json
    from src.session_manager import SessionManager

    state_file = tmp_path / "sessions.json"
    state_file.write_text(json.dumps({"sessions": []}))

    mgr = SessionManager(log_dir=str(tmp_path), state_file=str(state_file))
    mgr.em_topic = {"chat_id": 99999, "thread_id": 11111}
    mgr._save_state()

    data = json.loads(state_file.read_text())
    assert data.get("em_topic") == {"chat_id": 99999, "thread_id": 11111}


def test_is_em_clears_other_sessions_em_flag():
    """Setting is_em=True clears is_em from all other sessions."""
    session_a = _make_forum_session(session_id="em-a", is_em=True)
    session_b = _make_forum_session(session_id="em-b", thread_id=60000)

    tg = _make_telegram_bot()

    mgr = Mock()
    mgr.em_topic = None
    mgr.sessions = {session_a.id: session_a, session_b.id: session_b}
    mgr.get_session = Mock(side_effect=lambda sid: mgr.sessions.get(sid))
    mgr._save_state = Mock()

    tmux = Mock()
    tmux.set_status_bar = Mock()
    mgr.tmux = tmux

    mqm = Mock()
    mqm.cancel_remind = Mock()
    mqm.cancel_parent_wake = Mock()
    mqm.cancel_context_monitor_messages_from = Mock()
    mqm.delivery_states = {}
    mgr.message_queue_manager = mqm

    notifier = Mock()
    notifier.telegram = tg
    notifier.rename_session_topic = AsyncMock(return_value=True)

    app = create_app(
        session_manager=mgr,
        notifier=notifier,
        output_monitor=None,
        config={},
    )
    client = TestClient(app)

    # Set session_b as EM
    resp = client.patch(f"/sessions/{session_b.id}", json={"is_em": True})
    assert resp.status_code == 200

    # session_a's is_em cleared
    assert session_a.is_em is False
    assert session_b.is_em is True


# ============================================================================
# Fix C — POST /admin/cleanup-idle-topics
# ============================================================================


def _make_app_with_cleanup(sessions: dict, output_monitor):
    """Build a TestClient app for cleanup endpoint tests."""
    mgr = Mock()
    mgr.sessions = sessions
    mgr.get_session = Mock(side_effect=lambda sid: sessions.get(sid))
    mgr._save_state = Mock()

    app = create_app(
        session_manager=mgr,
        notifier=None,
        output_monitor=output_monitor,
        config={},
    )
    return TestClient(app)


def _make_mock_output_monitor():
    om = Mock()
    om.close_session_topic = AsyncMock()
    return om


def test_cleanup_idle_mode1_only_completed_sessions():
    """Mode 1: only sessions with completion_status=COMPLETED get close_session_topic."""
    completed = _make_forum_session(
        session_id="done-1",
        completion_status=CompletionStatus.COMPLETED,
    )
    idle_no_status = _make_forum_session(
        session_id="idle-1",
        thread_id=60000,
        completion_status=None,
    )
    running = _make_forum_session(
        session_id="run-1",
        thread_id=70000,
        status=SessionStatus.RUNNING,
        completion_status=None,
    )

    om = _make_mock_output_monitor()
    client = _make_app_with_cleanup(
        {
            completed.id: completed,
            idle_no_status.id: idle_no_status,
            running.id: running,
        },
        om,
    )

    resp = client.post("/admin/cleanup-idle-topics")
    assert resp.status_code == 200

    data = resp.json()
    assert data["closed"] == 1
    # Only the completed session gets close_session_topic
    om.close_session_topic.assert_called_once_with(completed, message="Completed")


def test_cleanup_idle_mode1_completed_without_telegram_skipped():
    """Mode 1: completed session with no Telegram thread is counted as skipped (closed=0)."""
    completed_no_tg = Session(
        id="done-notg",
        name="done-notg",
        working_dir="/tmp",
        tmux_session="claude-done-notg",
        log_file="/tmp/done-notg.log",
        status=SessionStatus.IDLE,
        completion_status=CompletionStatus.COMPLETED,
        # No telegram_chat_id / telegram_thread_id
    )

    om = _make_mock_output_monitor()
    client = _make_app_with_cleanup({completed_no_tg.id: completed_no_tg}, om)

    resp = client.post("/admin/cleanup-idle-topics")
    assert resp.status_code == 200

    data = resp.json()
    assert data["closed"] == 0
    om.close_session_topic.assert_not_called()


def test_cleanup_idle_mode2_explicit_session_ids():
    """Mode 2: explicit session_ids closes topics for those sessions."""
    s1 = _make_forum_session(session_id="s1", thread_id=11111)
    s2 = _make_forum_session(session_id="s2", thread_id=22222)

    om = _make_mock_output_monitor()
    client = _make_app_with_cleanup({s1.id: s1, s2.id: s2}, om)

    resp = client.post(
        "/admin/cleanup-idle-topics", json={"session_ids": [s1.id, s2.id]}
    )
    assert resp.status_code == 200

    data = resp.json()
    assert data["closed"] == 2
    assert data["rejected"] == []
    assert om.close_session_topic.call_count == 2


def test_cleanup_idle_mode2_rejects_running_session():
    """Mode 2: running session is rejected, not closed."""
    running = _make_forum_session(
        session_id="run-rej", status=SessionStatus.RUNNING
    )

    om = _make_mock_output_monitor()
    client = _make_app_with_cleanup({running.id: running}, om)

    resp = client.post(
        "/admin/cleanup-idle-topics", json={"session_ids": [running.id]}
    )
    assert resp.status_code == 200

    data = resp.json()
    assert data["closed"] == 0
    assert len(data["rejected"]) == 1
    assert data["rejected"][0]["id"] == running.id
    assert "running" in data["rejected"][0]["reason"]
    om.close_session_topic.assert_not_called()


def test_cleanup_idle_mode2_rejects_em_session():
    """Mode 2: is_em=True session is rejected (safety guard)."""
    em_session = _make_forum_session(session_id="em-guard", is_em=True)

    om = _make_mock_output_monitor()
    client = _make_app_with_cleanup({em_session.id: em_session}, om)

    resp = client.post(
        "/admin/cleanup-idle-topics", json={"session_ids": [em_session.id]}
    )
    assert resp.status_code == 200

    data = resp.json()
    assert data["closed"] == 0
    assert len(data["rejected"]) == 1
    assert "is_em" in data["rejected"][0]["reason"]
    om.close_session_topic.assert_not_called()


def test_cleanup_idle_mode2_unknown_session_rejected():
    """Mode 2: unknown session ID is rejected with 'not found' reason."""
    om = _make_mock_output_monitor()
    client = _make_app_with_cleanup({}, om)

    resp = client.post(
        "/admin/cleanup-idle-topics", json={"session_ids": ["nonexistent"]}
    )
    assert resp.status_code == 200

    data = resp.json()
    assert data["closed"] == 0
    assert len(data["rejected"]) == 1
    assert data["rejected"][0]["id"] == "nonexistent"
    assert "not found" in data["rejected"][0]["reason"]


def test_cleanup_idle_mode2_partial_closed_partial_rejected():
    """Mode 2: mix of valid and invalid sessions → partial close, partial reject."""
    good = _make_forum_session(session_id="good-s", thread_id=33333)
    bad_running = _make_forum_session(
        session_id="bad-running", thread_id=44444, status=SessionStatus.RUNNING
    )

    om = _make_mock_output_monitor()
    client = _make_app_with_cleanup(
        {good.id: good, bad_running.id: bad_running}, om
    )

    resp = client.post(
        "/admin/cleanup-idle-topics",
        json={"session_ids": [good.id, bad_running.id]},
    )
    assert resp.status_code == 200

    data = resp.json()
    assert data["closed"] == 1
    assert len(data["rejected"]) == 1
    assert data["rejected"][0]["id"] == bad_running.id
    om.close_session_topic.assert_called_once_with(good, message="Manually closed")
