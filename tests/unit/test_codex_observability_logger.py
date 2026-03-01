"""Unit tests for codex observability logger storage and retention."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

from src.codex_observability_logger import CodexObservabilityLogger


def test_log_tool_and_turn_events_with_bounded_payload(tmp_path):
    logger = CodexObservabilityLogger(
        db_path=str(tmp_path / "codex_observability.db"),
        payload_max_chars=240,
    )

    logger.log_tool_event(
        session_id="sess1",
        thread_id="thread-1",
        turn_id="turn-1",
        item_id="item-1",
        event_type="started",
        item_type="commandExecution",
        phase="running",
        command="echo hello",
        raw_payload={"blob": "x" * 2000},
    )
    logger.log_turn_event(
        session_id="sess1",
        thread_id="thread-1",
        turn_id="turn-1",
        event_type="turn_completed",
        status="completed",
        output_preview="done",
    )

    tool_events = logger.list_recent_tool_events("sess1", limit=10)
    turn_events = logger.list_recent_turn_events("sess1", limit=10)

    assert len(tool_events) == 1
    assert len(turn_events) == 1
    assert tool_events[0]["event_type"] == "started"
    assert tool_events[0]["provider"] == "codex-app"
    assert tool_events[0]["schema_version"] is None
    assert tool_events[0]["raw_payload_json"] is not None
    assert "truncated" in tool_events[0]["raw_payload_json"]
    assert turn_events[0]["event_type"] == "turn_completed"
    assert turn_events[0]["status"] == "completed"


def test_prune_applies_age_and_per_session_caps(tmp_path):
    logger = CodexObservabilityLogger(
        db_path=str(tmp_path / "codex_observability.db"),
        retention_max_age_days=1,
        retention_tool_events_per_session=2,
        retention_turn_events_per_session=1,
    )
    now = datetime.now(timezone.utc)
    old = now - timedelta(days=2)

    logger.log_tool_event(session_id="sess2", event_type="started", created_at=old)
    logger.log_tool_event(session_id="sess2", event_type="output_delta", created_at=now - timedelta(minutes=3))
    logger.log_tool_event(session_id="sess2", event_type="completed", created_at=now - timedelta(minutes=2))
    logger.log_tool_event(session_id="sess2", event_type="failed", created_at=now - timedelta(minutes=1))

    logger.log_turn_event(session_id="sess2", turn_id="t1", event_type="turn_started", created_at=old)
    logger.log_turn_event(session_id="sess2", turn_id="t2", event_type="turn_started", created_at=now - timedelta(minutes=1))

    result = logger.prune()
    assert result["tool_age"] >= 1
    assert result["turn_age"] >= 1

    tool_events = logger.list_recent_tool_events("sess2", limit=10)
    turn_events = logger.list_recent_turn_events("sess2", limit=10)
    assert len(tool_events) == 2
    assert tool_events[-1]["event_type"] == "failed"
    assert len(turn_events) == 1
    assert turn_events[0]["turn_id"] == "t2"


def test_prune_uses_provider_specific_age_boundary(tmp_path):
    logger = CodexObservabilityLogger(
        db_path=str(tmp_path / "codex_observability.db"),
        retention_max_age_days=1,
        retention_codex_fork_max_age_days=30,
        retention_tool_events_per_session=20,
    )
    now = datetime.now(timezone.utc)
    older_than_default = now - timedelta(days=10)

    logger.log_tool_event(
        session_id="sess3",
        event_type="started",
        provider="codex-app",
        created_at=older_than_default,
    )
    logger.log_tool_event(
        session_id="sess3",
        event_type="started",
        provider="codex-fork",
        created_at=older_than_default,
    )

    logger.prune()
    tool_events = logger.list_recent_tool_events("sess3", limit=10)
    providers = [row["provider"] for row in tool_events]

    assert "codex-fork" in providers
    assert "codex-app" not in providers
