"""Unit tests for codex event persistence and cursor replay semantics."""

from __future__ import annotations

import sqlite3
from unittest.mock import patch

from src.codex_event_store import CodexEventStore


def test_append_and_get_events_sequence(tmp_path):
    store = CodexEventStore(db_path=str(tmp_path / "codex_events.db"), startup_maintenance=False)

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
        startup_maintenance=False,
    )

    for idx in range(5):
        store.append_event(
            session_id="sess-retention",
            event_type="turn_delta",
            turn_id="turn-1",
            payload={"idx": idx},
        )
    store._run_startup_maintenance()

    page = store.get_events(session_id="sess-retention", since_seq=0, limit=10)
    assert page["history_gap"] is True
    assert page["gap_reason"] == "retention"
    assert page["earliest_seq"] == 3
    assert page["events"][0]["seq"] == 3


def test_persistence_recovery_emits_marker_event(tmp_path):
    store = CodexEventStore(db_path=str(tmp_path / "codex_events.db"), startup_maintenance=False)

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


def test_init_does_not_prune_on_startup_path(tmp_path):
    with patch.object(CodexEventStore, "_prune_locked") as prune:
        store = CodexEventStore(
            db_path=str(tmp_path / "codex_events.db"),
            startup_maintenance=False,
        )

    assert store._prune_index_ready is False
    prune.assert_not_called()


def test_startup_maintenance_adds_timestamp_index_and_prunes(tmp_path):
    store = CodexEventStore(
        db_path=str(tmp_path / "codex_events.db"),
        startup_maintenance=False,
    )

    with patch.object(store, "_prune_locked") as prune:
        store._run_startup_maintenance()

    with sqlite3.connect(str(tmp_path / "codex_events.db")) as conn:
        indexes = {
            row[1]
            for row in conn.execute("PRAGMA index_list(codex_session_events)").fetchall()
        }

    assert "idx_codex_session_events_timestamp" in indexes
    assert store._prune_index_ready is True
    prune.assert_called_once()


def test_append_schedules_maintenance_when_prune_index_not_ready(tmp_path):
    store = CodexEventStore(
        db_path=str(tmp_path / "codex_events.db"),
        prune_every_writes=1,
        startup_maintenance=False,
    )

    with patch.object(store, "_start_startup_maintenance") as start_maintenance:
        store.append_event(
            session_id="sess-maint",
            event_type="turn_delta",
            turn_id="turn-1",
            payload={"idx": 1},
        )

    start_maintenance.assert_called_once()


def test_startup_maintenance_failure_rolls_back_and_allows_next_write(tmp_path):
    store = CodexEventStore(
        db_path=str(tmp_path / "codex_events.db"),
        startup_maintenance=False,
    )

    with patch.object(store, "_prune_locked", side_effect=sqlite3.OperationalError("forced")):
        store._run_startup_maintenance()

    event = store.append_event(
        session_id="sess-after-failure",
        event_type="turn_delta",
        turn_id="turn-1",
        payload={"idx": 1},
    )

    assert event["persisted"] is True
