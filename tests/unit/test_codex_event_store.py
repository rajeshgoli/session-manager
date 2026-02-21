"""Unit tests for codex event persistence and cursor replay semantics."""

from __future__ import annotations

import sqlite3

from src.codex_event_store import CodexEventStore


def test_append_and_get_events_sequence(tmp_path):
    store = CodexEventStore(db_path=str(tmp_path / "codex_events.db"))

    first = store.append_event(
        session_id="sess1",
        event_type="turn_started",
        turn_id="turn-1",
        payload={"model": "sonnet"},
    )
    second = store.append_event(
        session_id="sess1",
        event_type="turn_delta",
        turn_id="turn-1",
        payload={"delta_preview": "hello"},
    )

    assert first["persisted"] is True
    assert first["seq"] == 1
    assert second["seq"] == 2

    page = store.get_events(session_id="sess1", since_seq=0, limit=50)
    assert page["earliest_seq"] == 1
    assert page["latest_seq"] == 2
    assert page["next_seq"] == 3
    assert page["history_gap"] is False
    assert [e["event_type"] for e in page["events"]] == ["turn_started", "turn_delta"]


def test_history_gap_when_since_seq_is_older_than_retained(tmp_path):
    store = CodexEventStore(
        db_path=str(tmp_path / "codex_events.db"),
        retention_max_events_per_session=3,
        prune_every_writes=1,
    )

    for idx in range(5):
        store.append_event(
            session_id="sess-retention",
            event_type="turn_delta",
            turn_id="turn-1",
            payload={"idx": idx},
        )

    page = store.get_events(session_id="sess-retention", since_seq=0, limit=10)
    assert page["history_gap"] is True
    assert page["gap_reason"] == "retention"
    assert page["earliest_seq"] == 3
    assert page["events"][0]["seq"] == 3


def test_persistence_recovery_emits_marker_event(tmp_path):
    store = CodexEventStore(db_path=str(tmp_path / "codex_events.db"))

    original_get_conn = store._get_conn

    def fail_get_conn():
        raise sqlite3.OperationalError("forced failure")

    store._get_conn = fail_get_conn
    failed = store.append_event(
        session_id="sess-recover",
        event_type="turn_started",
        turn_id="turn-x",
        payload={"forced": True},
    )
    assert failed["persisted"] is False
    assert failed["seq"] is None

    store._get_conn = original_get_conn
    succeeded = store.append_event(
        session_id="sess-recover",
        event_type="turn_started",
        turn_id="turn-x",
        payload={"forced": False},
    )
    assert succeeded["persisted"] is True
    assert succeeded["seq"] == 2

    page = store.get_events(session_id="sess-recover", since_seq=0, limit=10)
    assert [event["event_type"] for event in page["events"]] == [
        "event_persist_recovered",
        "turn_started",
    ]
    assert page["history_gap"] is False
