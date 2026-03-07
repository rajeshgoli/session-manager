from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock

import pytest

from src.main import SessionManagerApp
from src.models import Session, SessionStatus


def _make_session(
    session_id: str,
    *,
    chat_id: int = -1003506774897,
    thread_id: int | None = 12345,
    is_em: bool = False,
    tmux_session: str | None = None,
) -> Session:
    session = Session(
        id=session_id,
        name=f"claude-{session_id}",
        working_dir="/tmp",
        tmux_session=tmux_session or f"claude-{session_id}",
        log_file=f"/tmp/{session_id}.log",
        status=SessionStatus.IDLE,
        telegram_chat_id=chat_id,
        telegram_thread_id=thread_id,
        is_em=is_em,
    )
    return session


def _make_app(
    sessions: dict[str, Session],
    *,
    live_tmux_sessions: set[str] | None = None,
    delete_ok: bool = True,
) -> SessionManagerApp:
    app = SessionManagerApp.__new__(SessionManagerApp)
    app.telegram_bot = SimpleNamespace(delete_forum_topic=AsyncMock(return_value=delete_ok))
    app.telegram_topic_cleanup_enabled = True
    app.telegram_topic_cleanup_interval_seconds = 900
    app._telegram_topic_cleanup_task = None
    live_tmux_sessions = live_tmux_sessions or set()
    app.session_manager = SimpleNamespace(
        default_forum_chat_id=-1003506774897,
        sessions=sessions,
        _save_state=Mock(),
        tmux=SimpleNamespace(session_exists=lambda tmux_name: tmux_name in live_tmux_sessions),
    )
    return app


@pytest.mark.asyncio
async def test_cleanup_deletes_topics_when_tmux_runtime_is_gone():
    stale_idle = _make_session("idle1", tmux_session="claude-idle1")
    stale_stopped = _make_session("stop1", tmux_session="claude-stop1")
    app = _make_app({"idle1": stale_idle, "stop1": stale_stopped})

    result = await app._cleanup_stale_telegram_topics_once()

    assert result == {"deleted": 2, "skipped": 0}
    assert stale_idle.telegram_thread_id is None
    assert stale_stopped.telegram_thread_id is None
    assert app.telegram_bot.delete_forum_topic.await_count == 2
    app.session_manager._save_state.assert_called_once()


@pytest.mark.asyncio
async def test_cleanup_skips_em_live_tmux_non_forum_and_missing_thread():
    sessions = {
        "em1": _make_session("em1", is_em=True, tmux_session="claude-em1"),
        "live1": _make_session("live1", tmux_session="claude-live1"),
        "otherchat1": _make_session("otherchat1", chat_id=1234, tmux_session="claude-otherchat1"),
        "nothread1": _make_session("nothread1", thread_id=None, tmux_session="claude-nothread1"),
        "notmux1": _make_session("notmux1", tmux_session=None),
    }
    sessions["notmux1"].tmux_session = None
    app = _make_app(sessions, live_tmux_sessions={"claude-live1"})

    result = await app._cleanup_stale_telegram_topics_once()

    assert result == {"deleted": 0, "skipped": 5}
    app.telegram_bot.delete_forum_topic.assert_not_awaited()
    app.session_manager._save_state.assert_not_called()


@pytest.mark.asyncio
async def test_cleanup_leaves_session_mapped_when_delete_fails():
    stale_idle = _make_session("idle1", tmux_session="claude-idle1")
    app = _make_app({"idle1": stale_idle}, delete_ok=False)

    result = await app._cleanup_stale_telegram_topics_once()

    assert result == {"deleted": 0, "skipped": 1}
    assert stale_idle.telegram_thread_id == 12345
    app.session_manager._save_state.assert_not_called()
