"""Regression tests for sm#706 turn-bound Telegram response relay."""

from __future__ import annotations

import json
from datetime import datetime, timezone
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from src.message_queue import MessageQueueManager
from src.models import Session, SessionStatus
from src.response_relay import ResponseRelayLedger
from src.server import create_app


def _assistant_line(text: str, *, timestamp: str | None = None, uuid: str | None = None) -> str:
    entry = {
        "type": "assistant",
        "message": {
            "id": uuid or f"msg-{abs(hash(text))}",
            "content": [{"type": "text", "text": text}],
        },
    }
    if timestamp:
        entry["timestamp"] = timestamp
    if uuid:
        entry["uuid"] = uuid
    return json.dumps(entry) + "\n"


def _record_inbound(
    ledger: ResponseRelayLedger,
    session: Session,
    inbound_id: str,
    transcript_path,
    *,
    source: str = "sm-send",
) -> None:
    ledger.record_inbound_turn(
        session_id=session.id,
        inbound_id=inbound_id,
        source=source,
        provider=session.provider,
        delivered_at=datetime(2026, 5, 2, 22, 24, 19, tzinfo=timezone.utc),
        transcript_path=str(transcript_path),
        transcript_offset=transcript_path.stat().st_size,
        text="This is way too complex of an explanation",
    )


def _make_app(tmp_path, session: Session, ledger: ResponseRelayLedger):
    manager = MagicMock()
    manager.get_session.side_effect = lambda sid: session if sid == session.id else None
    manager.list_sessions.return_value = [session]
    manager._sync_session_resume_id = MagicMock()
    manager._save_state = MagicMock()
    manager.update_session_status = MagicMock()
    queue = MagicMock()
    queue.delivery_states = {session.id: SimpleNamespace(is_idle=True)}
    queue.mark_session_idle = MagicMock()
    queue._restore_user_input_after_response = AsyncMock()
    manager.message_queue_manager = queue

    notifier = MagicMock()
    notifier.notify = AsyncMock(return_value=True)
    output_monitor = MagicMock()
    app = create_app(
        session_manager=manager,
        notifier=notifier,
        output_monitor=output_monitor,
        response_relay_ledger=ledger,
        config={},
    )
    return app, TestClient(app), notifier, output_monitor


def _session(transcript_path) -> Session:
    return Session(
        id="3401-consultant",
        name="claude-3401-consultant",
        working_dir="/tmp",
        tmux_session="claude-3401-consultant",
        log_file="/tmp/3401.log",
        status=SessionStatus.RUNNING,
        telegram_chat_id=123,
        telegram_thread_id=456,
        transcript_path=str(transcript_path),
        provider="claude",
    )


def test_3401_timeline_suppresses_d13_then_relays_current_turn(tmp_path):
    transcript = tmp_path / "3401-consultant.jsonl"
    transcript.write_text(
        _assistant_line(
            "D13 previous assistant response",
            timestamp="2026-05-02T22:23:00Z",
            uuid="old-d13",
        )
    )
    session = _session(transcript)
    ledger = ResponseRelayLedger(str(tmp_path / "relay.db"))
    _record_inbound(ledger, session, "inbound-3401", transcript)
    app, client, notifier, _ = _make_app(tmp_path, session, ledger)

    with patch("asyncio.sleep", new_callable=AsyncMock):
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": session.id,
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    notifier.notify.assert_not_awaited()
    assert session.id in app.state.pending_stop_notifications

    transcript.write_text(
        transcript.read_text()
        + _assistant_line(
            "You're right - the correct current-turn answer",
            timestamp="2026-05-02T22:26:27Z",
            uuid="current-answer",
        )
    )

    response = client.post(
        "/hooks/claude",
        json={
            "hook_event_name": "Notification",
            "notification_type": "idle_prompt",
            "session_manager_id": session.id,
            "transcript_path": str(transcript),
        },
    )

    assert response.status_code == 200
    notifier.notify.assert_awaited_once()
    event = notifier.notify.await_args.args[0]
    assert event.context.startswith("You're right")
    assert "D13" not in event.context
    assert session.id not in app.state.pending_stop_notifications


def test_restart_dedupe_suppresses_replayed_hook(tmp_path):
    transcript = tmp_path / "restart.jsonl"
    transcript.write_text(_assistant_line("old answer", uuid="old"))
    session = _session(transcript)
    ledger_path = tmp_path / "relay.db"
    ledger = ResponseRelayLedger(str(ledger_path))
    _record_inbound(ledger, session, "inbound-restart", transcript)
    transcript.write_text(transcript.read_text() + _assistant_line("fresh answer", uuid="fresh"))

    app, client, notifier, _ = _make_app(tmp_path, session, ledger)
    with patch("asyncio.sleep", new_callable=AsyncMock):
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": session.id,
                "transcript_path": str(transcript),
            },
        )
    assert response.status_code == 200
    notifier.notify.assert_awaited_once()

    restarted_ledger = ResponseRelayLedger(str(ledger_path))
    _, restarted_client, restarted_notifier, _ = _make_app(tmp_path, session, restarted_ledger)
    with patch("asyncio.sleep", new_callable=AsyncMock):
        response = restarted_client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": session.id,
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    restarted_notifier.notify.assert_not_awaited()


def test_deferred_transcript_lag_waits_for_post_boundary_output(tmp_path):
    transcript = tmp_path / "lag.jsonl"
    transcript.write_text("")
    session = _session(transcript)
    ledger = ResponseRelayLedger(str(tmp_path / "relay.db"))
    _record_inbound(ledger, session, "inbound-lag", transcript)
    app, client, notifier, _ = _make_app(tmp_path, session, ledger)

    with patch("asyncio.sleep", new_callable=AsyncMock):
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": session.id,
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    notifier.notify.assert_not_awaited()
    assert session.id in app.state.pending_stop_notifications

    transcript.write_text(_assistant_line("response after transcript lag", uuid="lag-answer"))
    response = client.post(
        "/hooks/claude",
        json={
            "hook_event_name": "Notification",
            "notification_type": "idle_prompt",
            "session_manager_id": session.id,
            "transcript_path": str(transcript),
        },
    )

    assert response.status_code == 200
    notifier.notify.assert_awaited_once()
    assert notifier.notify.await_args.args[0].context == "response after transcript lag"


def test_long_chunk_group_is_deduped_as_one_output(tmp_path):
    transcript = tmp_path / "long.jsonl"
    transcript.write_text(_assistant_line("old", uuid="old"))
    session = _session(transcript)
    ledger = ResponseRelayLedger(str(tmp_path / "relay.db"))
    _record_inbound(ledger, session, "inbound-long", transcript)
    long_answer = "chunk-group " + ("x" * 9000)
    transcript.write_text(transcript.read_text() + _assistant_line(long_answer, uuid="long-answer"))
    _, client, notifier, _ = _make_app(tmp_path, session, ledger)

    with patch("asyncio.sleep", new_callable=AsyncMock):
        for _ in range(3):
            response = client.post(
                "/hooks/claude",
                json={
                    "hook_event_name": "Stop",
                    "session_manager_id": session.id,
                    "transcript_path": str(transcript),
                },
            )
            assert response.status_code == 200

    notifier.notify.assert_awaited_once()
    assert notifier.notify.await_args.args[0].context == long_answer


def test_newer_inbound_supersedes_late_output_for_previous_turn(tmp_path):
    transcript = tmp_path / "multi-turn.jsonl"
    transcript.write_text(_assistant_line("old", uuid="old"))
    session = _session(transcript)
    ledger = ResponseRelayLedger(str(tmp_path / "relay.db"))
    _record_inbound(ledger, session, "turn-1", transcript)
    transcript.write_text(transcript.read_text() + _assistant_line("late turn one answer", uuid="turn-1-answer"))
    _record_inbound(ledger, session, "turn-2", transcript)
    _, client, notifier, _ = _make_app(tmp_path, session, ledger)

    with patch("asyncio.sleep", new_callable=AsyncMock):
        response = client.post(
            "/hooks/claude",
            json={
                "hook_event_name": "Stop",
                "session_manager_id": session.id,
                "transcript_path": str(transcript),
            },
        )

    assert response.status_code == 200
    notifier.notify.assert_not_awaited()

    transcript.write_text(transcript.read_text() + _assistant_line("turn two answer", uuid="turn-2-answer"))
    response = client.post(
        "/hooks/claude",
        json={
            "hook_event_name": "Notification",
            "notification_type": "idle_prompt",
            "session_manager_id": session.id,
            "transcript_path": str(transcript),
        },
    )

    assert response.status_code == 200
    notifier.notify.assert_awaited_once()
    assert notifier.notify.await_args.args[0].context == "turn two answer"


@pytest.mark.asyncio
async def test_message_queue_records_inbound_boundary_only_after_delivery(tmp_path):
    transcript = tmp_path / "delivery.jsonl"
    transcript.write_text(_assistant_line("existing old output", uuid="old"))
    session = _session(transcript)
    manager = MagicMock()
    manager.get_session.return_value = session
    manager._deliver_direct = AsyncMock(return_value=True)
    manager._save_state = MagicMock()
    ledger = ResponseRelayLedger(str(tmp_path / "relay.db"))
    mq = MessageQueueManager(
        manager,
        db_path=str(tmp_path / "message_queue.db"),
        response_relay_ledger=ledger,
    )

    msg = mq.queue_message(
        session.id,
        "new user turn",
        from_sm_send=True,
        trigger_delivery=False,
    )
    assert ledger.get_latest_active_turn(session.id) is None

    await mq._try_deliver_messages(session.id)

    active_turn = ledger.get_latest_active_turn(session.id)
    assert active_turn is not None
    assert active_turn.inbound_id == msg.id
    assert active_turn.transcript_offset == transcript.stat().st_size
