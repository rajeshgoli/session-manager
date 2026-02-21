"""Unit tests for codex structured request ledger."""

from __future__ import annotations

import asyncio

import pytest

from src.codex_request_ledger import CodexRequestLedger


@pytest.mark.asyncio
async def test_register_resolve_and_idempotent_response(tmp_path):
    ledger = CodexRequestLedger(db_path=str(tmp_path / "codex_requests.db"), process_generation="gen-a")

    pending = await ledger.register_request(
        session_id="sess1",
        rpc_request_id=7,
        request_method="item/commandExecution/requestApproval",
        request_payload={"turnId": "turn-1"},
        thread_id="thread-1",
        turn_id="turn-1",
        item_id="item-1",
        request_type="request_approval",
        timeout_seconds=60,
        policy_payload={"decision": "decline"},
    )

    request_id = pending["request_id"]
    assert ledger.has_pending_requests("sess1") is True
    assert ledger.oldest_pending_summary("sess1")["request_id"] == request_id

    waiter = asyncio.create_task(ledger.wait_for_resolution(request_id))
    resolved = await ledger.resolve_request(
        request_id=request_id,
        response_payload={"decision": "accept"},
        resolution_source="api",
    )

    assert resolved["ok"] is True
    assert resolved["idempotent"] is False
    assert await waiter == {"decision": "accept"}
    assert ledger.has_pending_requests("sess1") is False

    replay = await ledger.resolve_request(
        request_id=request_id,
        response_payload={"decision": "decline"},
        resolution_source="api",
    )
    assert replay["ok"] is True
    assert replay["idempotent"] is True
    assert replay["request"]["resolved_payload"] == {"decision": "accept"}


@pytest.mark.asyncio
async def test_startup_orphans_unresolved_pending_rows(tmp_path):
    db_path = tmp_path / "codex_requests.db"
    first = CodexRequestLedger(db_path=str(db_path), process_generation="gen-1")

    pending = await first.register_request(
        session_id="sess-orphan",
        rpc_request_id=8,
        request_method="item/tool/requestUserInput",
        request_payload={"turnId": "turn-2"},
        thread_id="thread-2",
        turn_id="turn-2",
        item_id="item-2",
        request_type="request_user_input",
        timeout_seconds=600,
        policy_payload={"answers": {}},
    )

    second = CodexRequestLedger(db_path=str(db_path), process_generation="gen-2")
    rows = second.list_requests("sess-orphan", include_orphaned=True)
    assert len(rows) == 1
    assert rows[0]["request_id"] == pending["request_id"]
    assert rows[0]["status"] == "orphaned"
    assert rows[0]["error_code"] == "server_restarted"

    # Cleanup pending async task from first ledger to avoid lingering task warnings.
    first.orphan_pending_for_session("sess-orphan")


@pytest.mark.asyncio
async def test_startup_does_not_orphan_same_generation_rows(tmp_path):
    db_path = tmp_path / "codex_requests.db"
    first = CodexRequestLedger(db_path=str(db_path), process_generation="gen-same")

    pending = await first.register_request(
        session_id="sess-same",
        rpc_request_id=11,
        request_method="item/commandExecution/requestApproval",
        request_payload={"turnId": "turn-same"},
        thread_id="thread-same",
        turn_id="turn-same",
        item_id="item-same",
        request_type="request_approval",
        timeout_seconds=600,
        policy_payload={"decision": "decline"},
    )

    second = CodexRequestLedger(db_path=str(db_path), process_generation="gen-same")
    rows = second.list_requests("sess-same", include_orphaned=True)
    assert len(rows) == 1
    assert rows[0]["request_id"] == pending["request_id"]
    assert rows[0]["status"] == "pending"

    first.orphan_pending_for_session("sess-same")


@pytest.mark.asyncio
async def test_expired_then_policy_resolved_lifecycle(tmp_path):
    ledger = CodexRequestLedger(db_path=str(tmp_path / "codex_requests.db"), process_generation="gen-exp")

    pending = await ledger.register_request(
        session_id="sess-exp",
        rpc_request_id=9,
        request_method="item/tool/requestUserInput",
        request_payload={"turnId": "turn-exp"},
        thread_id="thread-exp",
        turn_id="turn-exp",
        item_id="item-exp",
        request_type="request_user_input",
        timeout_seconds=60,
        policy_payload={"answers": {}},
    )
    request_id = pending["request_id"]

    assert ledger._mark_expired(request_id) is True
    expired = ledger.get_request(request_id)
    assert expired is not None
    assert expired["status"] == "expired"
    assert expired["error_code"] == "request_expired"

    unavailable = await ledger.resolve_request(
        request_id=request_id,
        response_payload={"answers": {"k": "v"}},
        resolution_source="api",
    )
    assert unavailable["ok"] is False
    assert unavailable["error_code"] == "request_unavailable"

    resolved = await ledger.resolve_request(
        request_id=request_id,
        response_payload={"answers": {}},
        resolution_source="policy",
        error_code="request_expired",
        error_message="request expired before explicit response",
        allow_expired=True,
    )
    assert resolved["ok"] is True
    assert resolved["idempotent"] is False
    assert resolved["request"]["status"] == "resolved"
    assert resolved["request"]["resolution_source"] == "policy"
