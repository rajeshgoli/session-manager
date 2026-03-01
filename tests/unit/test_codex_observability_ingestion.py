"""SessionManager ingestion tests for codex observability events."""

from __future__ import annotations

import asyncio
import json
from types import SimpleNamespace

import pytest

from src.models import Session, SessionStatus
from src.session_manager import SessionManager


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
    stored_payload = stored_events[-1]["payload_preview"]["payload"]
    assert stored_payload["tool_input"]["Authorization"] == "[REDACTED]"
