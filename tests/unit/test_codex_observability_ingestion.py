"""SessionManager ingestion tests for codex observability events."""

from __future__ import annotations

import asyncio
import json
import sqlite3
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.models import Session, SessionStatus
from src.session_manager import SessionManager


def _codex_fork_relay_manager(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="forkrelay1",
        name="codex-fork-forkrelay1",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
        telegram_chat_id=1234,
        telegram_thread_id=5678,
    )
    manager.sessions[session.id] = session
    manager.set_hook_output_store({})
    manager.notifier = SimpleNamespace(notify=AsyncMock(return_value=True))
    return manager, session


async def _ingest_and_relay(manager: SessionManager, session_id: str, event: dict):
    manager.ingest_codex_fork_event(session_id, event)
    await manager._handle_codex_fork_assistant_relay_event(session_id, event)


@pytest.mark.asyncio
async def test_structured_request_and_response_logged(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="obsreq1",
        name="codex-app-obsreq1",
        working_dir=str(tmp_path),
        provider="codex-app",
        status=SessionStatus.RUNNING,
        codex_thread_id="thread-req",
    )
    manager.sessions[session.id] = session
    manager.codex_sessions[session.id] = SimpleNamespace(thread_id="thread-req")

    request_task = asyncio.create_task(
        manager._handle_codex_server_request(
            session.id,
            42,
            "item/commandExecution/requestApproval",
            {"turnId": "turn-req", "item": {"id": "item-req"}},
        )
    )
    await asyncio.sleep(0)
    pending = manager.list_codex_pending_requests(session.id)
    assert len(pending) == 1

    request_id = pending[0]["request_id"]
    resolved = await manager.respond_codex_request(session.id, request_id, {"decision": "accept"})
    assert resolved["ok"] is True
    assert await request_task == {"decision": "accept"}

    tool_events = manager.codex_observability_logger.list_recent_tool_events(session.id, limit=20)
    event_types = [row["event_type"] for row in tool_events]
    assert "request_approval" in event_types
    assert "approval_decision" in event_types
    approval_events = [row for row in tool_events if row["event_type"] == "approval_decision"]
    assert approval_events[-1]["item_type"] == "commandExecution"


@pytest.mark.asyncio
async def test_item_lifecycle_notifications_logged(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="obsitem1",
        name="codex-app-obsitem1",
        working_dir=str(tmp_path),
        provider="codex-app",
        status=SessionStatus.RUNNING,
        codex_thread_id="thread-item",
    )
    manager.sessions[session.id] = session
    manager.codex_sessions[session.id] = SimpleNamespace(thread_id="thread-item")

    await manager._handle_codex_item_notification(
        session.id,
        "item/started",
        {
            "turnId": "turn-item",
            "item": {"id": "item-1", "type": "commandExecution", "command": "ls", "cwd": str(tmp_path)},
        },
    )
    await manager._handle_codex_item_notification(
        session.id,
        "item/commandExecution/outputDelta",
        {
            "turnId": "turn-item",
            "item": {"id": "item-1", "type": "commandExecution"},
            "delta": "stdout line",
        },
    )
    await manager._handle_codex_item_notification(
        session.id,
        "item/completed",
        {
            "turnId": "turn-item",
            "item": {
                "id": "item-1",
                "type": "commandExecution",
                "status": "failed",
                "exitCode": 2,
                "errorCode": "command_failed",
                "errorMessage": "non-zero exit",
            },
        },
    )

    tool_events = manager.codex_observability_logger.list_recent_tool_events(session.id, limit=20)
    assert [row["event_type"] for row in tool_events][-3:] == ["started", "output_delta", "failed"]
    assert tool_events[-1]["final_status"] == "failed"
    assert tool_events[-1]["error_code"] == "command_failed"


def test_codex_fork_after_tool_use_ingestion_redacts_and_tags(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    assert manager.codex_observability_logger.retention_codex_fork_max_age_days == 30

    session = Session(
        id="forkobs1",
        name="codex-fork-forkobs1",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    manager.ingest_codex_fork_event(
        session.id,
        {
            "schema_version": 2,
            "event_type": "after_tool_use",
            "session_id": "thread-forkobs1",
            "seq": 12,
            "session_epoch": 1,
            "ts": "2026-03-01T01:02:03Z",
            "payload": {
                "turn_id": "turn-123",
                "call_id": "call-456",
                "tool_name": "exec_command",
                "tool_kind": "command",
                "executed": True,
                "success": False,
                "duration_ms": 321,
                "mutating": True,
                "tool_input": {
                    "command": "echo hello",
                    "Authorization": "Bearer topsecret-token-value",
                    "env": {
                        "API_KEY": "raw-api-key-value",
                        "NORMAL": "ok",
                    },
                    "notes": "z" * 5000,
                },
                "output_preview": "token=raw-token-value " + ("x" * 2500),
            },
        },
    )

    tool_events = manager.codex_observability_logger.list_recent_tool_events(session.id, limit=20)
    assert len(tool_events) == 1
    event = tool_events[0]
    assert event["turn_id"] == "turn-123"
    assert event["item_id"] == "call-456"
    assert event["provider"] == "codex-fork"
    assert event["schema_version"] == 2
    assert event["final_status"] == "failed"
    assert event["created_at"].startswith("2026-03-01T01:02:03")

    payload = json.loads(event["raw_payload_json"])
    assert payload["tool_input"]["Authorization"] == "[REDACTED]"
    assert payload["tool_input"]["env"]["API_KEY"] == "[REDACTED]"
    assert payload["tool_input"]["env"]["NORMAL"] == "ok"
    assert payload["tool_input"]["notes"]["truncated"] is True
    assert payload["output_preview"]["truncated"] is True

    stored_events = manager.codex_event_store.get_events(session.id, limit=10)["events"]
    assert stored_events
    stored_preview = stored_events[0]["payload_preview"]
    if "payload" in stored_preview:
        assert stored_preview["payload"]["tool_input"]["Authorization"] == "[REDACTED]"
    else:
        assert "[REDACTED]" in stored_preview["preview"]


def test_codex_fork_raw_function_call_ingestion_logs_tool_event(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="forkraw1",
        name="codex-fork-forkraw1",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
    )
    manager.sessions[session.id] = session

    manager.ingest_codex_fork_event(
        session.id,
        {
            "schema_version": 2,
            "event_type": "raw_response_item",
            "session_id": "thread-forkraw1",
            "seq": 4,
            "session_epoch": 1,
            "ts": "2026-03-09T08:55:00Z",
            "payload": {
                "turn_id": "turn-raw-1",
                "item": {
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": json.dumps(
                        {
                            "cmd": "git status",
                            "Authorization": "Bearer super-secret-token",
                            "env": {"API_KEY": "raw-api-key-value", "SAFE": "ok"},
                        }
                    ),
                    "call_id": "call-raw-1",
                },
            },
        },
    )

    tool_events = manager.codex_observability_logger.list_recent_tool_events(session.id, limit=10)
    assert len(tool_events) == 1
    event = tool_events[0]
    assert event["event_type"] == "submitted"
    assert event["provider"] == "codex-fork"
    assert event["created_at"].startswith("2026-03-09T08:55:00")
    payload = json.loads(event["raw_payload_json"])
    assert payload["tool_name"] == "exec_command"
    assert payload["tool_input"]["Authorization"] == "[REDACTED]"
    assert payload["tool_input"]["env"]["API_KEY"] == "[REDACTED]"
    assert payload["tool_input"]["env"]["SAFE"] == "ok"
    assert session.last_tool_name == "exec_command"
    assert session.last_tool_call is not None


@pytest.mark.asyncio
async def test_codex_fork_turn_complete_updates_last_message_and_notifies(tmp_path):
    manager = SessionManager(log_dir=str(tmp_path), state_file=str(tmp_path / "state.json"))
    session = Session(
        id="forkturn1",
        name="codex-fork-forkturn1",
        working_dir=str(tmp_path),
        provider="codex-fork",
        status=SessionStatus.RUNNING,
        telegram_chat_id=1234,
    )
    manager.sessions[session.id] = session
    manager.set_hook_output_store({})
    manager.message_queue_manager = SimpleNamespace(mark_session_idle=MagicMock())
    manager.notifier = SimpleNamespace(notify=AsyncMock())

    await manager._handle_codex_fork_turn_complete(
        session_id=session.id,
        last_message="Final answer from codex-fork",
        event={"schema_version": 2, "payload": {"turn_id": "turn-1"}},
    )

    assert manager.hook_output_store[session.id] == "Final answer from codex-fork"
    assert manager.hook_output_store["latest"] == "Final answer from codex-fork"
    assert session.status == SessionStatus.IDLE
    manager.notifier.notify.assert_awaited()
    manager.message_queue_manager.mark_session_idle.assert_called_once_with(
        session.id,
        completion_transition=True,
    )


@pytest.mark.asyncio
async def test_codex_fork_completed_agent_message_relays_without_turn_last_message(tmp_path):
    manager, session = _codex_fork_relay_manager(tmp_path)

    await _ingest_and_relay(
        manager,
        session.id,
        {
            "schema_version": 2,
            "event_type": "item/agentMessage/delta",
            "session_id": "thread-relay",
            "seq": 1,
            "session_epoch": 1,
            "payload": {
                "threadId": "thread-relay",
                "turnId": "turn-relay",
                "itemId": "msg-relay",
                "delta": "delta fallback text",
            },
        },
    )
    await _ingest_and_relay(
        manager,
        session.id,
        {
            "schema_version": 2,
            "event_type": "item_completed",
            "session_id": "thread-relay",
            "seq": 2,
            "session_epoch": 1,
            "payload": {
                "threadId": "thread-relay",
                "turnId": "turn-relay",
                "item": {
                    "type": "agentMessage",
                    "id": "msg-relay",
                    "text": "completed text wins",
                },
            },
        },
    )
    await _ingest_and_relay(
        manager,
        session.id,
        {
            "schema_version": 2,
            "event_type": "turn_complete",
            "session_id": "thread-relay",
            "seq": 3,
            "session_epoch": 1,
            "payload": {
                "turn_id": "turn-relay",
                "last_agent_message": None,
            },
        },
    )

    manager.notifier.notify.assert_awaited_once()
    event = manager.notifier.notify.await_args.args[0]
    assert event.event_type == "response"
    assert event.context == "completed text wins"
    assert manager.hook_output_store[session.id] == "completed text wins"
    assert manager.codex_event_store.has_assistant_message_relayed(
        session_id=session.id,
        thread_id="thread-relay",
        turn_id="turn-relay",
        message_item_id="msg-relay",
    )


@pytest.mark.asyncio
async def test_codex_fork_assistant_relay_dedupes_after_restart(tmp_path):
    manager, session = _codex_fork_relay_manager(tmp_path)
    event = {
        "schema_version": 2,
        "event_type": "item_completed",
        "session_id": "thread-dedupe",
        "seq": 1,
        "session_epoch": 1,
        "payload": {
            "threadId": "thread-dedupe",
            "turnId": "turn-dedupe",
            "item": {
                "type": "agentMessage",
                "id": "msg-dedupe",
                "text": "send me once",
            },
        },
    }

    await _ingest_and_relay(manager, session.id, event)
    manager.notifier.notify.assert_awaited_once()

    restarted, restarted_session = _codex_fork_relay_manager(tmp_path)
    restarted_session.id = session.id
    restarted.sessions = {session.id: restarted_session}
    await _ingest_and_relay(restarted, session.id, event)

    restarted.notifier.notify.assert_not_awaited()


@pytest.mark.asyncio
async def test_codex_fork_assistant_relay_suppresses_empty_output(tmp_path):
    manager, session = _codex_fork_relay_manager(tmp_path)

    await _ingest_and_relay(
        manager,
        session.id,
        {
            "schema_version": 2,
            "event_type": "item_completed",
            "session_id": "thread-empty",
            "seq": 1,
            "session_epoch": 1,
            "payload": {
                "threadId": "thread-empty",
                "turnId": "turn-empty",
                "item": {
                    "type": "agentMessage",
                    "id": "msg-empty",
                    "text": "   ",
                },
            },
        },
    )

    manager.notifier.notify.assert_not_awaited()
    assert not manager.codex_event_store.has_assistant_message_relayed(
        session_id=session.id,
        thread_id="thread-empty",
        turn_id="turn-empty",
        message_item_id="msg-empty",
    )


@pytest.mark.asyncio
async def test_codex_fork_turn_complete_full_output_beats_delta_fallback(tmp_path):
    manager, session = _codex_fork_relay_manager(tmp_path)

    await _ingest_and_relay(
        manager,
        session.id,
        {
            "schema_version": 2,
            "event_type": "item/agentMessage/delta",
            "session_id": "thread-full",
            "seq": 1,
            "session_epoch": 1,
            "payload": {
                "threadId": "thread-full",
                "turnId": "turn-full",
                "itemId": "msg-full",
                "delta": "partial suffix only",
            },
        },
    )
    turn_complete = {
        "schema_version": 2,
        "event_type": "turn_complete",
        "session_id": "thread-full",
        "seq": 2,
        "session_epoch": 1,
        "payload": {
            "turnId": "turn-full",
            "last_agent_message": "canonical full answer",
        },
    }

    await _ingest_and_relay(manager, session.id, turn_complete)
    await manager._handle_codex_fork_turn_complete(
        session_id=session.id,
        last_message="canonical full answer",
        event=turn_complete,
    )

    manager.notifier.notify.assert_awaited_once()
    event = manager.notifier.notify.await_args.args[0]
    assert event.context == "canonical full answer"
    assert manager._codex_assistant_message_deltas == {}
    assert manager.codex_event_store.has_assistant_turn_relayed(
        session_id=session.id,
        thread_id="thread-full",
        turn_id="turn-full",
    )


@pytest.mark.asyncio
async def test_codex_fork_turn_complete_camel_case_turn_id_flushes_delta_fallback(tmp_path):
    manager, session = _codex_fork_relay_manager(tmp_path)

    await _ingest_and_relay(
        manager,
        session.id,
        {
            "schema_version": 2,
            "event_type": "item/agentMessage/delta",
            "session_id": "thread-camel",
            "seq": 1,
            "session_epoch": 1,
            "payload": {
                "threadId": "thread-camel",
                "turnId": "turn-camel",
                "itemId": "msg-camel",
                "delta": "fallback from camel turn id",
            },
        },
    )
    await _ingest_and_relay(
        manager,
        session.id,
        {
            "schema_version": 2,
            "event_type": "turn_complete",
            "session_id": "thread-camel",
            "seq": 2,
            "session_epoch": 1,
            "payload": {
                "turnId": "turn-camel",
                "last_agent_message": None,
            },
        },
    )

    manager.notifier.notify.assert_awaited_once()
    event = manager.notifier.notify.await_args.args[0]
    assert event.context == "fallback from camel turn id"
    assert manager.codex_event_store.has_assistant_message_relayed(
        session_id=session.id,
        thread_id="thread-camel",
        turn_id="turn-camel",
        message_item_id="msg-camel",
    )


@pytest.mark.asyncio
async def test_codex_fork_assistant_relay_tolerates_malformed_events(tmp_path):
    manager, session = _codex_fork_relay_manager(tmp_path)

    manager.ingest_codex_fork_event(session.id, ["not", "a", "dict"])
    await manager._handle_codex_fork_assistant_relay_event(session.id, ["not", "a", "dict"])
    await _ingest_and_relay(
        manager,
        session.id,
        {
            "schema_version": 2,
            "event_type": "item_completed",
            "payload": {"item": "not-a-dict"},
        },
    )
    await _ingest_and_relay(
        manager,
        session.id,
        {
            "schema_version": 2,
            "event_type": "item/agentMessage/delta",
            "payload": {"turnId": "turn-bad", "delta": {"not": "text"}},
        },
    )

    manager.notifier.notify.assert_not_awaited()


@pytest.mark.asyncio
async def test_codex_fork_monitor_surfaces_ingestion_failures(tmp_path):
    manager, session = _codex_fork_relay_manager(tmp_path)
    stream_path = manager._codex_fork_event_stream_path(session)
    stream_path.write_text(
        json.dumps(
            {
                "schema_version": 2,
                "event_type": "turn_started",
                "session_id": "thread-failure",
                "seq": 1,
                "session_epoch": 1,
                "payload": {"turn_id": "turn-failure"},
            }
        )
        + "\n"
    )

    with patch.object(
        manager,
        "ingest_codex_fork_event",
        side_effect=sqlite3.OperationalError("forced persistence failure"),
    ):
        await asyncio.wait_for(manager._monitor_codex_fork_event_stream(session.id), timeout=1.0)

    assert manager.codex_fork_lifecycle[session.id]["state"] == "error"
    assert (
        manager.codex_fork_lifecycle[session.id]["cause_event_type"]
        == "event_stream_monitor_error"
    )


@pytest.mark.asyncio
async def test_codex_fork_monitor_surfaces_assistant_relay_failures(tmp_path):
    manager, session = _codex_fork_relay_manager(tmp_path)
    stream_path = manager._codex_fork_event_stream_path(session)
    stream_path.write_text(
        json.dumps(
            {
                "schema_version": 2,
                "event_type": "item_completed",
                "session_id": "thread-relay-failure",
                "seq": 1,
                "session_epoch": 1,
                "payload": {
                    "threadId": "thread-relay-failure",
                    "turnId": "turn-relay-failure",
                    "item": {
                        "type": "agentMessage",
                        "id": "msg-relay-failure",
                        "text": "should not be swallowed",
                    },
                },
            }
        )
        + "\n"
    )

    with patch.object(
        manager.codex_event_store,
        "has_assistant_message_relayed",
        side_effect=sqlite3.OperationalError("forced relay ledger failure"),
    ):
        await asyncio.wait_for(manager._monitor_codex_fork_event_stream(session.id), timeout=1.0)

    assert manager.codex_fork_lifecycle[session.id]["state"] == "error"
    assert (
        manager.codex_fork_lifecycle[session.id]["cause_event_type"]
        == "event_stream_monitor_error"
    )
