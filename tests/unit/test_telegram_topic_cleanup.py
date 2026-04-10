import json
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock, patch

import pytest

from src.main import SessionManagerApp
from src.models import Session, SessionStatus, TelegramTopicRecord
from src.session_manager import SessionManager


def _make_session(
    session_id: str,
    *,
    chat_id: int = -1003506774897,
    thread_id: int | None = 12345,
    is_em: bool = False,
    provider: str = "claude",
    tmux_session: str | None = None,
) -> Session:
    return Session(
        id=session_id,
        name=f"{provider}-{session_id}",
        working_dir="/tmp",
        tmux_session=tmux_session or ("" if provider == "codex-app" else f"{provider}-{session_id}"),
        log_file=f"/tmp/{session_id}.log",
        provider=provider,
        status=SessionStatus.IDLE,
        telegram_chat_id=chat_id,
        telegram_thread_id=thread_id,
        is_em=is_em,
    )


def _make_record(
    session_id: str,
    *,
    chat_id: int = -1003506774897,
    thread_id: int = 12345,
    tmux_session: str | None = None,
    provider: str = "claude",
    is_em_topic: bool = False,
) -> TelegramTopicRecord:
    return TelegramTopicRecord(
        session_id=session_id,
        chat_id=chat_id,
        thread_id=thread_id,
        tmux_session=tmux_session or ("" if provider == "codex-app" else f"{provider}-{session_id}"),
        provider=provider,
        is_em_topic=is_em_topic,
    )


def _make_app(
    sessions: dict[str, Session],
    *,
    live_tmux_sessions: set[str] | None = None,
    delete_ok: bool = True,
    records: list[TelegramTopicRecord] | None = None,
    em_topic: dict | None = None,
) -> SessionManagerApp:
    app = SessionManagerApp.__new__(SessionManagerApp)
    app.telegram_bot = SimpleNamespace(
        delete_forum_topic=AsyncMock(return_value=delete_ok),
        register_topic_session=Mock(),
        _session_threads={},
    )
    app.telegram_topic_cleanup_enabled = True
    app.telegram_topic_cleanup_interval_seconds = 900
    app._telegram_topic_cleanup_task = None
    live_tmux_sessions = live_tmux_sessions or set()
    registry_records = records or [
        _make_record(
            session.id,
            chat_id=session.telegram_chat_id,
            thread_id=session.telegram_thread_id,
            tmux_session=session.tmux_session,
            provider=session.provider,
            is_em_topic=session.is_em,
        )
        for session in sessions.values()
        if session.telegram_chat_id and session.telegram_thread_id
    ]
    registry = {(record.chat_id, record.thread_id): record for record in registry_records}

    def _mark_deleted(chat_id: int, thread_id: int, *, session=None, persist=True):
        record = registry[(chat_id, thread_id)]
        if record.deleted_at is None:
            record.deleted_at = session.created_at if session else record.created_at

    app.session_manager = SimpleNamespace(
        default_forum_chat_id=-1003506774897,
        em_topic=em_topic,
        orphaned_topics=[],
        sessions=sessions,
        telegram_topic_registry=registry,
        _save_state=Mock(),
        get_session=lambda session_id: sessions.get(session_id),
        list_sessions=lambda include_stopped=False: list(sessions.values()),
        get_active_telegram_topic_record=lambda session_id, chat_id=None: next(
            (
                record
                for record in registry.values()
                if record.session_id == session_id
                and record.deleted_at is None
                and (chat_id is None or record.chat_id == chat_id)
            ),
            None,
        ),
        mark_telegram_topic_deleted=Mock(side_effect=_mark_deleted),
        _ensure_telegram_topic=AsyncMock(),
        tmux=SimpleNamespace(session_exists=lambda tmux_name: tmux_name in live_tmux_sessions),
    )
    return app


@pytest.mark.asyncio
async def test_cleanup_deletes_topics_when_tmux_runtime_is_gone():
    stale_idle = _make_session("idle1", thread_id=12345, tmux_session="claude-idle1")
    stale_stopped = _make_session("stop1", thread_id=12346, tmux_session="claude-stop1")
    app = _make_app(
        {"idle1": stale_idle, "stop1": stale_stopped},
        records=[
            _make_record("idle1", thread_id=12345, tmux_session="claude-idle1"),
            _make_record("stop1", thread_id=12346, tmux_session="claude-stop1"),
        ],
    )

    result = await app._cleanup_stale_telegram_topics_once()

    assert result == {"deleted": 2, "skipped": 0}
    assert stale_idle.telegram_thread_id is None
    assert stale_stopped.telegram_thread_id is None
    assert app.telegram_bot.delete_forum_topic.await_count == 2
    app.session_manager._save_state.assert_called_once()


@pytest.mark.asyncio
async def test_cleanup_deletes_topics_when_session_record_is_missing():
    record = _make_record("gone1", thread_id=54321, tmux_session="claude-gone1")
    app = _make_app({}, records=[record])

    result = await app._cleanup_stale_telegram_topics_once()

    assert result == {"deleted": 1, "skipped": 0}
    assert record.deleted_at is not None
    app.session_manager.mark_telegram_topic_deleted.assert_called_once()


@pytest.mark.asyncio
async def test_cleanup_deletes_orphaned_codex_app_topics():
    record = _make_record("goneapp", thread_id=54321, provider="codex-app", tmux_session="")
    app = _make_app({}, records=[record])

    result = await app._cleanup_stale_telegram_topics_once()

    assert result == {"deleted": 1, "skipped": 0}
    assert record.deleted_at is not None
    app.session_manager.mark_telegram_topic_deleted.assert_called_once()


@pytest.mark.asyncio
async def test_cleanup_skips_em_live_tmux_non_forum_deleted_and_live_codex_app_topics():
    live = _make_session("live1", thread_id=10002, tmux_session="claude-live1")
    sessions = {
        "live1": live,
        "app1": _make_session("app1", provider="codex-app", thread_id=10005, tmux_session=""),
    }
    records = [
        _make_record("em1", thread_id=10001, is_em_topic=True),
        _make_record("live1", thread_id=10002, tmux_session="claude-live1"),
        _make_record("otherchat1", chat_id=1234, thread_id=10003),
        TelegramTopicRecord(
            session_id="done1",
            chat_id=-1003506774897,
            thread_id=10004,
            tmux_session="claude-done1",
            provider="claude",
            deleted_at=live.created_at,
        ),
        _make_record("app1", thread_id=10005, provider="codex-app", tmux_session=""),
    ]
    app = _make_app(sessions, live_tmux_sessions={"claude-live1"}, records=records)
    app.session_manager.em_topic = {"chat_id": -1003506774897, "thread_id": 10001}

    result = await app._cleanup_stale_telegram_topics_once()

    assert result == {"deleted": 0, "skipped": 5}
    app.telegram_bot.delete_forum_topic.assert_not_awaited()
    app.session_manager._save_state.assert_not_called()


@pytest.mark.asyncio
async def test_cleanup_deletes_obsolete_em_topic_when_not_continuity_topic():
    obsolete_em = _make_record("oldem1", thread_id=10001, is_em_topic=True)
    current_em = _make_record("currentem", thread_id=10002, is_em_topic=True)
    app = _make_app(
        {},
        records=[obsolete_em, current_em],
        em_topic={"chat_id": -1003506774897, "thread_id": 10002},
    )

    result = await app._cleanup_stale_telegram_topics_once()

    assert result == {"deleted": 1, "skipped": 1}
    assert obsolete_em.deleted_at is not None
    assert current_em.deleted_at is None
    app.telegram_bot.delete_forum_topic.assert_awaited_once_with(-1003506774897, 10001)


@pytest.mark.asyncio
async def test_cleanup_preserves_em_topics_when_continuity_topic_is_unknown():
    em_record = _make_record("oldem1", thread_id=10001, is_em_topic=True)
    app = _make_app({}, records=[em_record], em_topic=None)

    result = await app._cleanup_stale_telegram_topics_once()

    assert result == {"deleted": 0, "skipped": 1}
    assert em_record.deleted_at is None
    app.telegram_bot.delete_forum_topic.assert_not_awaited()


@pytest.mark.asyncio
async def test_cleanup_deletes_duplicate_topic_for_live_session():
    session = _make_session("live1", thread_id=20002, tmux_session="claude-live1")
    old_record = _make_record("live1", thread_id=20001, tmux_session="claude-live1")
    current_record = _make_record("live1", thread_id=20002, tmux_session="claude-live1")
    app = _make_app(
        {"live1": session},
        live_tmux_sessions={"claude-live1"},
        records=[old_record, current_record],
    )

    result = await app._cleanup_stale_telegram_topics_once()

    assert result == {"deleted": 1, "skipped": 1}
    assert old_record.deleted_at is not None
    assert current_record.deleted_at is None
    assert session.telegram_thread_id == 20002
    app.telegram_bot.delete_forum_topic.assert_awaited_once_with(-1003506774897, 20001)


@pytest.mark.asyncio
async def test_cleanup_deletes_duplicate_topic_for_live_codex_app_session():
    session = _make_session("app1", provider="codex-app", thread_id=20002, tmux_session="")
    old_record = _make_record("app1", provider="codex-app", thread_id=20001, tmux_session="")
    current_record = _make_record("app1", provider="codex-app", thread_id=20002, tmux_session="")
    app = _make_app({"app1": session}, records=[old_record, current_record])

    result = await app._cleanup_stale_telegram_topics_once()

    assert result == {"deleted": 1, "skipped": 1}
    assert old_record.deleted_at is not None
    assert current_record.deleted_at is None
    assert session.telegram_thread_id == 20002
    app.telegram_bot.delete_forum_topic.assert_awaited_once_with(-1003506774897, 20001)


@pytest.mark.asyncio
async def test_cleanup_leaves_registry_active_when_delete_fails():
    stale_idle = _make_session("idle1", tmux_session="claude-idle1")
    app = _make_app({"idle1": stale_idle}, delete_ok=False)
    record = next(iter(app.session_manager.telegram_topic_registry.values()))

    result = await app._cleanup_stale_telegram_topics_once()

    assert result == {"deleted": 0, "skipped": 1}
    assert stale_idle.telegram_thread_id == 12345
    assert record.deleted_at is None
    app.session_manager._save_state.assert_not_called()


@pytest.mark.asyncio
async def test_reconcile_reuses_durable_topic_before_creating_new_one():
    session = _make_session("sess1", thread_id=None)
    record = _make_record("sess1", thread_id=77777, tmux_session="claude-sess1")
    app = _make_app({"sess1": session}, records=[record])

    await app._reconcile_telegram_topics()

    assert session.telegram_thread_id == 77777
    app.session_manager._ensure_telegram_topic.assert_not_awaited()
    app.telegram_bot.register_topic_session.assert_called_once_with(
        record.chat_id,
        record.thread_id,
        session.id,
    )
    assert app.telegram_bot._session_threads[session.id] == (record.chat_id, record.thread_id)
    app.session_manager._save_state.assert_called_once()


def test_load_state_backfills_durable_topic_registry(tmp_path):
    state_file = tmp_path / "sessions.json"
    registry_file = tmp_path / "telegram_topics.json"
    state_file.write_text(
        json.dumps(
            {
                "sessions": [
                    {
                        "id": "dead1234",
                        "name": "claude-dead1234",
                        "working_dir": "/tmp",
                        "tmux_session": "claude-dead1234",
                        "log_file": "/tmp/dead1234.log",
                        "status": "idle",
                        "created_at": "2026-03-06T10:00:00",
                        "last_activity": "2026-03-06T10:00:00",
                        "telegram_chat_id": -1003506774897,
                        "telegram_thread_id": 24680,
                    }
                ]
            }
        )
    )
    config = {
        "telegram": {
            "default_forum_chat_id": -1003506774897,
            "topic_registry": {"path": str(registry_file)},
        }
    }

    with patch("src.session_manager.TmuxController") as mock_tmux_cls:
        mock_tmux_cls.return_value.session_exists.return_value = False
        manager = SessionManager(
            log_dir=str(tmp_path),
            state_file=str(state_file),
            config=config,
        )

    record = manager.telegram_topic_registry[(-1003506774897, 24680)]
    assert record.session_id == "dead1234"
    assert record.tmux_session == "claude-dead1234"
    saved = json.loads(registry_file.read_text())
    assert saved["topics"][0]["thread_id"] == 24680
    assert manager.get_session("dead1234") is None


def test_load_state_preserves_deleted_topic_tombstone(tmp_path):
    state_file = tmp_path / "sessions.json"
    registry_file = tmp_path / "telegram_topics.json"
    state_file.write_text(
        json.dumps(
            {
                "sessions": [
                    {
                        "id": "dead1234",
                        "name": "claude-dead1234",
                        "working_dir": "/tmp",
                        "tmux_session": "claude-dead1234",
                        "log_file": "/tmp/dead1234.log",
                        "status": "idle",
                        "created_at": "2026-03-06T10:00:00",
                        "last_activity": "2026-03-06T10:00:00",
                        "telegram_chat_id": -1003506774897,
                        "telegram_thread_id": 24680,
                    }
                ]
            }
        )
    )
    registry_file.write_text(
        json.dumps(
            {
                "topics": [
                    {
                        "session_id": "dead1234",
                        "chat_id": -1003506774897,
                        "thread_id": 24680,
                        "tmux_session": "claude-dead1234",
                        "provider": "claude",
                        "created_at": "2026-03-06T10:00:00",
                        "last_seen_at": "2026-03-06T10:00:00",
                        "deleted_at": "2026-03-06T11:00:00",
                        "is_em_topic": False,
                    }
                ]
            }
        )
    )
    config = {
        "telegram": {
            "default_forum_chat_id": -1003506774897,
            "topic_registry": {"path": str(registry_file)},
        }
    }

    with patch("src.session_manager.TmuxController") as mock_tmux_cls:
        mock_tmux_cls.return_value.session_exists.return_value = False
        manager = SessionManager(
            log_dir=str(tmp_path),
            state_file=str(state_file),
            config=config,
        )

    record = manager.telegram_topic_registry[(-1003506774897, 24680)]
    assert record.deleted_at is not None


def test_update_telegram_thread_revives_deleted_topic_tombstone(tmp_path):
    state_file = tmp_path / "sessions.json"
    registry_file = tmp_path / "telegram_topics.json"
    state_file.write_text(
        json.dumps(
            {
                "sessions": [
                    {
                        "id": "em123456",
                        "name": "claude-em123456",
                        "working_dir": "/tmp",
                        "tmux_session": "claude-em123456",
                        "log_file": "/tmp/em123456.log",
                        "status": "idle",
                        "created_at": "2026-03-06T10:00:00",
                        "last_activity": "2026-03-06T10:00:00",
                        "telegram_chat_id": -1003506774897,
                        "telegram_thread_id": None,
                        "is_em": True,
                    }
                ]
            }
        )
    )
    registry_file.write_text(
        json.dumps(
            {
                "topics": [
                    {
                        "session_id": "oldem123",
                        "chat_id": -1003506774897,
                        "thread_id": 24680,
                        "tmux_session": "claude-oldem123",
                        "provider": "claude",
                        "created_at": "2026-03-06T09:00:00",
                        "last_seen_at": "2026-03-06T09:30:00",
                        "deleted_at": "2026-03-06T10:30:00",
                        "is_em_topic": True,
                    }
                ]
            }
        )
    )
    config = {
        "telegram": {
            "default_forum_chat_id": -1003506774897,
            "topic_registry": {"path": str(registry_file)},
        }
    }

    with patch("src.session_manager.TmuxController") as mock_tmux_cls:
        mock_tmux_cls.return_value.session_exists.return_value = True
        manager = SessionManager(
            log_dir=str(tmp_path),
            state_file=str(state_file),
            config=config,
        )

    manager.update_telegram_thread("em123456", -1003506774897, 24680)

    record = manager.telegram_topic_registry[(-1003506774897, 24680)]
    assert record.session_id == "em123456"
    assert record.tmux_session == "claude-em123456"
    assert record.deleted_at is None
    assert record.is_em_topic is True


def test_load_state_revives_deleted_topic_tombstone_for_live_session(tmp_path):
    state_file = tmp_path / "sessions.json"
    registry_file = tmp_path / "telegram_topics.json"
    state_file.write_text(
        json.dumps(
            {
                "sessions": [
                    {
                        "id": "em123456",
                        "name": "claude-em123456",
                        "working_dir": "/tmp",
                        "tmux_session": "claude-em123456",
                        "log_file": "/tmp/em123456.log",
                        "status": "idle",
                        "created_at": "2026-03-06T10:00:00",
                        "last_activity": "2026-03-06T10:00:00",
                        "telegram_chat_id": -1003506774897,
                        "telegram_thread_id": 24680,
                        "is_em": True,
                    }
                ]
            }
        )
    )
    registry_file.write_text(
        json.dumps(
            {
                "topics": [
                    {
                        "session_id": "oldem123",
                        "chat_id": -1003506774897,
                        "thread_id": 24680,
                        "tmux_session": "claude-oldem123",
                        "provider": "claude",
                        "created_at": "2026-03-06T09:00:00",
                        "last_seen_at": "2026-03-06T09:30:00",
                        "deleted_at": "2026-03-06T10:30:00",
                        "is_em_topic": True,
                    }
                ]
            }
        )
    )
    config = {
        "telegram": {
            "default_forum_chat_id": -1003506774897,
            "topic_registry": {"path": str(registry_file)},
        }
    }

    with patch("src.session_manager.TmuxController") as mock_tmux_cls:
        mock_tmux_cls.return_value.session_exists.return_value = True
        manager = SessionManager(
            log_dir=str(tmp_path),
            state_file=str(state_file),
            config=config,
        )

    record = manager.telegram_topic_registry[(-1003506774897, 24680)]
    assert record.session_id == "em123456"
    assert record.tmux_session == "claude-em123456"
    assert record.deleted_at is None
    assert manager.get_session("em123456") is not None
