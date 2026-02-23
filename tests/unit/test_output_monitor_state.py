"""Unit tests for OutputMonitor state projection helpers (#288)."""

from __future__ import annotations

import asyncio
from datetime import datetime, timedelta
from types import SimpleNamespace

import pytest

from src.models import MonitorState, Session
from src.output_monitor import OutputMonitor


def _make_session(session_id: str = "mon12345") -> Session:
    return Session(
        id=session_id,
        name=f"claude-{session_id}",
        working_dir="/tmp",
        tmux_session=f"claude-{session_id}",
        provider="claude",
        log_file="/tmp/test.log",
    )


@pytest.mark.asyncio
async def test_analyze_content_sets_last_pattern_permission_then_none():
    monitor = OutputMonitor()
    session = _make_session()

    await monitor._analyze_content(session, "Allow once? [Y/n]")
    assert monitor.get_session_state(session.id).last_pattern == "permission"

    await monitor._analyze_content(session, "plain output with no known pattern")
    assert monitor.get_session_state(session.id).last_pattern is None


@pytest.mark.asyncio
async def test_permission_pattern_takes_precedence_over_completion_when_batched():
    monitor = OutputMonitor()
    session = _make_session("monperm2")

    await monitor._analyze_content(
        session,
        "Task complete\nAllow once? [Y/n]\n",
    )

    assert monitor.get_session_state(session.id).last_pattern == "permission"


def test_output_bytes_window_tracks_last_10_seconds():
    monitor = OutputMonitor()
    session_id = "bytes123"
    monitor._monitor_states[session_id] = MonitorState()
    now = datetime.now()
    monitor._output_history[session_id] = [
        (now - timedelta(seconds=12), 10),
        (now - timedelta(seconds=4), 20),
        (now - timedelta(seconds=1), 30),
    ]

    monitor._refresh_output_bytes_window(session_id, now)
    state = monitor.get_session_state(session_id)

    assert state is not None
    assert state.output_bytes_last_10s == 50


@pytest.mark.asyncio
async def test_cleanup_session_continues_when_telegram_notify_fails():
    class _Telegram:
        def __init__(self):
            async def _close_forum_topic(chat_id: int, message_thread_id: int):
                return None

            self.bot = SimpleNamespace(close_forum_topic=_close_forum_topic)
            self._topic_sessions = {(123, 456): "monclean1"}
            self._session_threads = {"monclean1": 456}

        async def send_with_fallback(self, chat_id: int, message: str, thread_id: int):
            raise RuntimeError("telegram down")

    class _SessionManager:
        def __init__(self, session: Session):
            self.sessions = {session.id: session}
            self.notifier = SimpleNamespace(telegram=_Telegram())
            self.saved = 0

        def _save_state(self):
            self.saved += 1

    session = _make_session("monclean1")
    session.telegram_chat_id = 123
    session.telegram_thread_id = 456
    monitor = OutputMonitor()
    sm = _SessionManager(session)
    monitor.set_session_manager(sm)

    await monitor.cleanup_session(session)

    assert session.id not in sm.sessions
    assert sm.saved >= 1


@pytest.mark.asyncio
async def test_cleanup_session_notification_timeout_is_non_fatal():
    class _Telegram:
        def __init__(self):
            self.bot = SimpleNamespace(close_forum_topic=self._close_forum_topic)
            self._topic_sessions = {(321, 654): "monclean2"}
            self._session_threads = {"monclean2": 654}

        async def send_with_fallback(self, chat_id: int, message: str, thread_id: int):
            await asyncio.sleep(0.2)
            return None

        async def _close_forum_topic(self, chat_id: int, message_thread_id: int):
            return None

    class _SessionManager:
        def __init__(self, session: Session):
            self.sessions = {session.id: session}
            self.notifier = SimpleNamespace(telegram=_Telegram())
            self.saved = 0

        def _save_state(self):
            self.saved += 1

    session = _make_session("monclean2")
    session.telegram_chat_id = 321
    session.telegram_thread_id = 654
    monitor = OutputMonitor(config={"timeouts": {"output_monitor": {"cleanup_notify_timeout_seconds": 0.01}}})
    sm = _SessionManager(session)
    monitor.set_session_manager(sm)

    await monitor.cleanup_session(session)

    assert session.id not in sm.sessions
    assert sm.saved >= 1
