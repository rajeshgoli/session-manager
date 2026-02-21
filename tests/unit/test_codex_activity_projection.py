"""Unit tests for codex activity projection adapter."""

from __future__ import annotations

from src.codex_activity_projection import CodexActivityProjection


class _FakeObservabilityLogger:
    def __init__(self, rows):
        self.rows = rows

    def list_recent_tool_events(self, session_id: str, limit: int = 20):
        return self.rows[:limit]


def test_projection_maps_command_lifecycle_to_provider_neutral_shape():
    rows = [
        {
            "session_id": "sess1",
            "turn_id": "turn-1",
            "item_id": "item-1",
            "event_type": "started",
            "item_type": "commandExecution",
            "command": "pytest -q",
            "created_at": "2026-02-21T10:00:00+00:00",
            "latency_ms": None,
            "final_status": None,
            "file_path": None,
            "approval_decision": None,
            "error_message": None,
        },
        {
            "session_id": "sess1",
            "turn_id": "turn-1",
            "item_id": "item-1",
            "event_type": "failed",
            "item_type": "commandExecution",
            "command": "pytest -q",
            "created_at": "2026-02-21T10:00:05+00:00",
            "latency_ms": 5000,
            "final_status": "failed",
            "file_path": None,
            "approval_decision": None,
            "error_message": "non-zero exit",
        },
    ]
    projection = CodexActivityProjection(_FakeObservabilityLogger(rows))

    actions = projection.recent_actions("sess1", limit=10)
    assert len(actions) == 2
    assert actions[0]["source_provider"] == "codex-app"
    assert actions[0]["action_kind"] == "command"
    assert actions[0]["status"] == "running"
    assert "Started:" in actions[0]["summary_text"]
    assert actions[1]["status"] == "failed"
    assert actions[1]["ended_at"] == "2026-02-21T10:00:05+00:00"


def test_projection_maps_approval_events():
    rows = [
        {
            "session_id": "sess2",
            "turn_id": "turn-2",
            "item_id": "item-2",
            "event_type": "request_approval",
            "item_type": "fileChange",
            "created_at": "2026-02-21T10:00:00+00:00",
            "latency_ms": None,
            "final_status": None,
            "file_path": "src/main.py",
            "command": None,
            "approval_decision": None,
            "error_message": None,
        },
        {
            "session_id": "sess2",
            "turn_id": "turn-2",
            "item_id": "item-2",
            "event_type": "approval_decision",
            "item_type": "fileChange",
            "created_at": "2026-02-21T10:00:03+00:00",
            "latency_ms": None,
            "final_status": None,
            "file_path": "src/main.py",
            "command": None,
            "approval_decision": "accept",
            "error_message": None,
        },
    ]
    projection = CodexActivityProjection(_FakeObservabilityLogger(rows))

    actions = projection.recent_actions("sess2", limit=10)
    assert actions[0]["action_kind"] == "approval"
    assert actions[0]["status"] == "pending"
    assert actions[1]["status"] == "completed"
    assert "accept" in actions[1]["summary_text"]
