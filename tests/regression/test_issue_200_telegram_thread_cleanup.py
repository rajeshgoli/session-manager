"""Regression tests for issue #200: Telegram thread cleanup — try-and-fallback notification.

Bug:
- Kill path (forum mode): topic deleted silently with no goodbye message.
- Kill path (reply-thread mode): delete_forum_topic fails silently; thread goes dark.
- Clear path: no Telegram notification at all.

Fix:
- Kill path: send "Session stopped [id]" via try-and-fallback, then close_forum_topic
  (forum) or leave thread open (reply-thread). delete_forum_topic no longer used.
- Clear path: send "Context cleared [id] — ready for new task" via try-and-fallback.
"""

import pytest
from unittest.mock import Mock, AsyncMock, MagicMock

from fastapi.testclient import TestClient

from src.output_monitor import OutputMonitor
from src.server import create_app
from src.models import Session, SessionStatus


# ============================================================================
# Shared fixtures
# ============================================================================


@pytest.fixture
def forum_session():
    """Session with Telegram forum topic configured."""
    return Session(
        id="sess-forum",
        name="forum-session",
        working_dir="/tmp",
        tmux_session="claude-sess-forum",
        log_file="/tmp/forum.log",
        status=SessionStatus.RUNNING,
        telegram_chat_id=10000,
        telegram_thread_id=50000,
    )


@pytest.fixture
def reply_session():
    """Session with Telegram reply-thread configured (non-forum)."""
    return Session(
        id="sess-reply",
        name="reply-session",
        working_dir="/tmp",
        tmux_session="claude-sess-reply",
        log_file="/tmp/reply.log",
        status=SessionStatus.RUNNING,
        telegram_chat_id=20000,
        telegram_thread_id=60000,
    )


@pytest.fixture
def no_tg_session():
    """Session with no Telegram configured."""
    return Session(
        id="sess-notg",
        name="no-tg-session",
        working_dir="/tmp",
        tmux_session="claude-sess-notg",
        log_file="/tmp/notg.log",
        status=SessionStatus.RUNNING,
    )


def _make_session_manager(session, telegram_bot):
    """Build a minimal mock session manager with a given session and telegram_bot."""
    manager = Mock()
    manager.sessions = {session.id: session}
    manager._save_state = Mock()
    manager.app = Mock()
    manager.app.state = Mock()
    manager.app.state.last_claude_output = {}

    notifier = Mock()
    notifier.telegram = telegram_bot
    manager.notifier = notifier
    return manager


def _make_telegram_bot(send_forum_returns=1):
    """
    Build a mock TelegramBot.

    send_forum_returns: value returned by send_with_fallback
      (None = forum send failed / not a forum topic; non-None = forum succeeded)
    """
    tg = Mock()
    tg.bot = AsyncMock()
    tg._topic_sessions = {}
    tg._session_threads = {}

    # send_with_fallback returns the forum result: non-None = forum succeeded,
    # None = forum failed (fallback attempted internally).
    tg.send_with_fallback = AsyncMock(return_value=send_forum_returns)
    return tg


@pytest.fixture
def output_monitor_factory():
    """Return a factory that builds an OutputMonitor wired to a given session manager."""
    def _build(session_manager):
        monitor = OutputMonitor(poll_interval=0.1)
        monitor.set_session_manager(session_manager)
        monitor.set_save_state_callback(session_manager._save_state)
        return monitor
    return _build


# ============================================================================
# Kill path tests (cleanup_session)
# ============================================================================


@pytest.mark.asyncio
async def test_cleanup_kill_forum_mode(output_monitor_factory, forum_session):
    """Kill with forum topic: 'Session stopped' sent via message_thread_id, topic closed."""
    chat_id = forum_session.telegram_chat_id
    thread_id = forum_session.telegram_thread_id

    tg = _make_telegram_bot(send_forum_returns=9001)
    mgr = _make_session_manager(forum_session, tg)
    monitor = output_monitor_factory(mgr)

    await monitor.cleanup_session(forum_session)

    # Forum send called exactly once via send_with_fallback
    tg.send_with_fallback.assert_called_once_with(
        chat_id=chat_id,
        message=f"Session stopped [{forum_session.id}]",
        thread_id=thread_id,
    )

    # close_forum_topic called because forum send succeeded
    tg.bot.close_forum_topic.assert_called_once_with(
        chat_id=chat_id, message_thread_id=thread_id
    )

    # delete_forum_topic NOT called (replaced by close + stop message)
    tg.bot.delete_forum_topic.assert_not_called()

    # In-memory mappings cleaned up
    assert (chat_id, thread_id) not in tg._topic_sessions
    assert forum_session.id not in tg._session_threads


@pytest.mark.asyncio
async def test_cleanup_kill_reply_thread_mode(output_monitor_factory, reply_session):
    """Kill with reply-thread: 'Session stopped' sent via reply_to_message_id; no close/delete."""
    chat_id = reply_session.telegram_chat_id
    thread_id = reply_session.telegram_thread_id

    # Forum send fails (None), fallback send succeeds
    tg = _make_telegram_bot(send_forum_returns=None)
    mgr = _make_session_manager(reply_session, tg)
    monitor = output_monitor_factory(mgr)

    await monitor.cleanup_session(reply_session)

    # send_with_fallback called once with correct args
    tg.send_with_fallback.assert_called_once_with(
        chat_id=chat_id,
        message=f"Session stopped [{reply_session.id}]",
        thread_id=thread_id,
    )

    # close_forum_topic NOT called (forum send failed, send_with_fallback returned None)
    tg.bot.close_forum_topic.assert_not_called()

    # delete_forum_topic NOT called (old code path removed)
    tg.bot.delete_forum_topic.assert_not_called()

    # In-memory mappings cleaned up
    assert reply_session.id not in tg._session_threads


@pytest.mark.asyncio
async def test_cleanup_kill_post_restart_reply_thread(output_monitor_factory, reply_session):
    """Post-restart scenario: reply-thread session in _topic_sessions. Kill sends reply-thread
    notification (not forum), close_forum_topic NOT called."""
    chat_id = reply_session.telegram_chat_id
    thread_id = reply_session.telegram_thread_id

    # Forum send fails → fallback path taken
    tg = _make_telegram_bot(send_forum_returns=None)
    # Simulate server restart: reply-thread session in _topic_sessions
    tg._topic_sessions[(chat_id, thread_id)] = reply_session.id
    tg._session_threads[reply_session.id] = (chat_id, thread_id)

    mgr = _make_session_manager(reply_session, tg)
    monitor = output_monitor_factory(mgr)

    await monitor.cleanup_session(reply_session)

    # Fallback (reply-thread) path was taken
    tg.bot.close_forum_topic.assert_not_called()
    tg.send_with_fallback.assert_called_once()

    # Mappings removed
    assert (chat_id, thread_id) not in tg._topic_sessions
    assert reply_session.id not in tg._session_threads


@pytest.mark.asyncio
async def test_cleanup_no_telegram_configured(output_monitor_factory, no_tg_session):
    """Kill with no Telegram configured: cleanup proceeds without error, no Telegram calls."""
    tg = _make_telegram_bot()
    mgr = _make_session_manager(no_tg_session, tg)
    monitor = output_monitor_factory(mgr)

    # Should not raise
    await monitor.cleanup_session(no_tg_session)

    # No Telegram calls made
    tg.send_with_fallback.assert_not_called()
    tg.bot.close_forum_topic.assert_not_called()


@pytest.mark.asyncio
async def test_cleanup_notification_failure_still_cleans_mappings(
    output_monitor_factory, forum_session
):
    """Both forum and fallback sends fail: cleanup still removes in-memory mappings."""
    chat_id = forum_session.telegram_chat_id
    thread_id = forum_session.telegram_thread_id

    # Both sends fail
    tg = _make_telegram_bot(send_forum_returns=None)
    tg._topic_sessions[(chat_id, thread_id)] = forum_session.id
    tg._session_threads[forum_session.id] = (chat_id, thread_id)

    mgr = _make_session_manager(forum_session, tg)
    monitor = output_monitor_factory(mgr)

    # Should not raise
    await monitor.cleanup_session(forum_session)

    # Mappings cleaned up despite notification failure
    assert (chat_id, thread_id) not in tg._topic_sessions
    assert forum_session.id not in tg._session_threads


# ============================================================================
# Clear path tests (server.py /sessions/{id}/clear)
# ============================================================================


def _make_app_with_session(session, telegram_bot):
    """Build a TestClient app wired to a mock session manager and telegram bot."""
    mgr = Mock()
    mgr.sessions = {session.id: session}
    mgr.get_session = Mock(return_value=session)
    mgr.clear_session = AsyncMock(return_value=True)
    mgr._save_state = Mock()
    mgr.message_queue_manager = Mock()
    mgr.message_queue_manager.cancel_remind = Mock()
    mgr.message_queue_manager.cancel_parent_wake = Mock()
    mgr.message_queue_manager.delivery_states = {}

    notifier = Mock()
    notifier.telegram = telegram_bot

    app = create_app(
        session_manager=mgr,
        notifier=notifier,
        output_monitor=None,
        config={},
    )
    return TestClient(app)


def test_clear_sends_notification_forum_mode(forum_session):
    """POST /clear on a forum session sends 'Context cleared' via message_thread_id."""
    chat_id = forum_session.telegram_chat_id
    thread_id = forum_session.telegram_thread_id

    # Forum send succeeds (non-None)
    tg = _make_telegram_bot(send_forum_returns=5001)
    client = _make_app_with_session(forum_session, tg)

    resp = client.post(f"/sessions/{forum_session.id}/clear", json={})
    assert resp.status_code == 200

    # send_with_fallback called exactly once with correct args
    tg.send_with_fallback.assert_called_once_with(
        chat_id=chat_id,
        message=f"Context cleared [{forum_session.id}] — ready for new task",
        thread_id=thread_id,
    )


def test_clear_sends_notification_reply_thread_mode(reply_session):
    """POST /clear on a reply-thread session falls back to reply_to_message_id."""
    chat_id = reply_session.telegram_chat_id
    thread_id = reply_session.telegram_thread_id

    # Forum send fails, fallback succeeds
    tg = _make_telegram_bot(send_forum_returns=None)
    client = _make_app_with_session(reply_session, tg)

    resp = client.post(f"/sessions/{reply_session.id}/clear", json={})
    assert resp.status_code == 200

    # send_with_fallback called once with correct args (fallback handled internally)
    tg.send_with_fallback.assert_called_once_with(
        chat_id=chat_id,
        message=f"Context cleared [{reply_session.id}] — ready for new task",
        thread_id=thread_id,
    )


def test_clear_no_telegram_configured(no_tg_session):
    """POST /clear with no Telegram configured: no Telegram calls, 200 returned."""
    tg = _make_telegram_bot()
    client = _make_app_with_session(no_tg_session, tg)

    resp = client.post(f"/sessions/{no_tg_session.id}/clear", json={})
    assert resp.status_code == 200

    tg.send_with_fallback.assert_not_called()


def test_clear_thread_remains_open_after_clear(forum_session):
    """POST /clear does NOT close the forum topic (session continues on new task)."""
    tg = _make_telegram_bot(send_forum_returns=3001)
    client = _make_app_with_session(forum_session, tg)

    resp = client.post(f"/sessions/{forum_session.id}/clear", json={})
    assert resp.status_code == 200

    # close_forum_topic must NOT be called — the thread stays open
    tg.bot.close_forum_topic.assert_not_called()
