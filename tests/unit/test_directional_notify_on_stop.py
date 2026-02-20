"""Unit tests for sm#256: directional notify-on-stop guard in send_input()."""

import asyncio
from datetime import datetime
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.models import (
    DeliveryResult,
    Session,
    SessionStatus,
)
from src.session_manager import SessionManager


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_session(session_id: str, is_em: bool = False) -> Session:
    return Session(
        id=session_id,
        name=f"claude-{session_id}",
        working_dir="/tmp",
        tmux_session=f"claude-{session_id}",
        log_file=f"/tmp/{session_id}.log",
        status=SessionStatus.IDLE,
        is_em=is_em,
    )


def _make_session_manager(sessions: dict[str, Session]) -> SessionManager:
    """Build a SessionManager with given sessions dict and mocked internals."""
    sm = SessionManager.__new__(SessionManager)
    sm.sessions = sessions
    sm.message_queue_manager = MagicMock()
    sm.message_queue_manager.queue_message = MagicMock()
    sm.message_queue_manager.delivery_states = {}
    sm.message_queue_manager._get_or_create_state = MagicMock(return_value=MagicMock())
    sm.config = {}
    return sm


def noop_create_task(coro):
    """Silently close coroutine without running it."""
    coro.close()
    return MagicMock()


# ---------------------------------------------------------------------------
# Tests: guard in send_input
# ---------------------------------------------------------------------------


class TestDirectionalNotifyOnStop:
    """Tests for the is_em guard in session_manager.send_input()."""

    @pytest.mark.asyncio
    async def test_em_sender_preserves_notify_on_stop(self):
        """EM sender (is_em=True) → notify_on_stop=True preserved (queue called with True)."""
        em = _make_session("em01", is_em=True)
        target = _make_session("tgt1")
        sm = _make_session_manager({"em01": em, "tgt1": target})

        with patch("asyncio.create_task", noop_create_task):
            await sm.send_input(
                session_id="tgt1",
                text="do task",
                sender_session_id="em01",
                delivery_mode="sequential",
                notify_on_stop=True,
            )

        call_kwargs = sm.message_queue_manager.queue_message.call_args[1]
        assert call_kwargs["notify_on_stop"] is True

    @pytest.mark.asyncio
    async def test_non_em_sender_suppresses_notify_on_stop(self):
        """Non-EM sender (is_em=False) → notify_on_stop overridden to False."""
        engineer = _make_session("eng1", is_em=False)
        em_target = _make_session("em01", is_em=True)
        sm = _make_session_manager({"eng1": engineer, "em01": em_target})

        with patch("asyncio.create_task", noop_create_task):
            await sm.send_input(
                session_id="em01",
                text="task done",
                sender_session_id="eng1",
                delivery_mode="sequential",
                notify_on_stop=True,
            )

        call_kwargs = sm.message_queue_manager.queue_message.call_args[1]
        assert call_kwargs["notify_on_stop"] is False

    @pytest.mark.asyncio
    async def test_is_em_defaults_to_false_treated_as_non_em(self):
        """Session with no explicit is_em (defaults False) is treated as non-EM → suppressed."""
        sender = _make_session("sndr")  # is_em defaults to False
        target = _make_session("tgt1")
        sm = _make_session_manager({"sndr": sender, "tgt1": target})

        with patch("asyncio.create_task", noop_create_task):
            await sm.send_input(
                session_id="tgt1",
                text="msg",
                sender_session_id="sndr",
                delivery_mode="sequential",
                notify_on_stop=True,
            )

        call_kwargs = sm.message_queue_manager.queue_message.call_args[1]
        assert call_kwargs["notify_on_stop"] is False

    @pytest.mark.asyncio
    async def test_notify_on_stop_false_not_flipped_by_em_sender(self):
        """EM sender with explicit notify_on_stop=False → guard does NOT flip to True."""
        em = _make_session("em01", is_em=True)
        target = _make_session("tgt1")
        sm = _make_session_manager({"em01": em, "tgt1": target})

        with patch("asyncio.create_task", noop_create_task):
            await sm.send_input(
                session_id="tgt1",
                text="task",
                sender_session_id="em01",
                delivery_mode="sequential",
                notify_on_stop=False,
            )

        call_kwargs = sm.message_queue_manager.queue_message.call_args[1]
        assert call_kwargs["notify_on_stop"] is False

    @pytest.mark.asyncio
    async def test_no_sender_session_id_guard_skipped(self):
        """sender_session_id=None → guard skipped, notify_on_stop passed as-is (True)."""
        target = _make_session("tgt1")
        sm = _make_session_manager({"tgt1": target})

        with patch("asyncio.create_task", noop_create_task):
            await sm.send_input(
                session_id="tgt1",
                text="msg",
                sender_session_id=None,
                delivery_mode="sequential",
                notify_on_stop=True,
            )

        call_kwargs = sm.message_queue_manager.queue_message.call_args[1]
        assert call_kwargs["notify_on_stop"] is True

    @pytest.mark.asyncio
    async def test_sender_not_in_sessions_fail_closed(self):
        """Sender ID set but not found in sessions dict → fail-closed → notify_on_stop=False."""
        target = _make_session("tgt1")
        sm = _make_session_manager({"tgt1": target})
        # "ghost01" is not in sessions

        with patch("asyncio.create_task", noop_create_task):
            await sm.send_input(
                session_id="tgt1",
                text="msg",
                sender_session_id="ghost01",
                delivery_mode="sequential",
                notify_on_stop=True,
            )

        call_kwargs = sm.message_queue_manager.queue_message.call_args[1]
        assert call_kwargs["notify_on_stop"] is False

    @pytest.mark.asyncio
    async def test_em_sender_urgent_mode_preserves_notify_on_stop(self):
        """EM sender with urgent delivery mode → notify_on_stop=True preserved."""
        em = _make_session("em01", is_em=True)
        target = _make_session("tgt1")
        sm = _make_session_manager({"em01": em, "tgt1": target})

        with patch("asyncio.create_task", noop_create_task):
            await sm.send_input(
                session_id="tgt1",
                text="urgent task",
                sender_session_id="em01",
                delivery_mode="urgent",
                notify_on_stop=True,
            )

        call_kwargs = sm.message_queue_manager.queue_message.call_args[1]
        assert call_kwargs["notify_on_stop"] is True

    @pytest.mark.asyncio
    async def test_non_em_sender_important_mode_suppressed(self):
        """Non-EM sender with important delivery mode → notify_on_stop suppressed to False."""
        engineer = _make_session("eng1", is_em=False)
        target = _make_session("tgt1")
        sm = _make_session_manager({"eng1": engineer, "tgt1": target})

        with patch("asyncio.create_task", noop_create_task):
            await sm.send_input(
                session_id="tgt1",
                text="important msg",
                sender_session_id="eng1",
                delivery_mode="important",
                notify_on_stop=True,
            )

        call_kwargs = sm.message_queue_manager.queue_message.call_args[1]
        assert call_kwargs["notify_on_stop"] is False
