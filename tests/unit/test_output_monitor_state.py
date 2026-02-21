"""Unit tests for OutputMonitor state projection helpers (#288)."""

from __future__ import annotations

from datetime import datetime, timedelta

import pytest

from src.models import MonitorState, Session
from src.output_monitor import OutputMonitor


def _make_session(session_id: str = "mon12345") -> Session:
    return Session(
        id=session_id,
        name=f"claude-{session_id}",
        working_dir="/tmp",
        tmux_session=f"claude-{session_id}",
        provider="claude",
        log_file="/tmp/test.log",
    )


@pytest.mark.asyncio
async def test_analyze_content_sets_last_pattern_permission_then_none():
    monitor = OutputMonitor()
    session = _make_session()

    await monitor._analyze_content(session, "Allow once? [Y/n]")
    assert monitor.get_session_state(session.id).last_pattern == "permission"

    await monitor._analyze_content(session, "plain output with no known pattern")
    assert monitor.get_session_state(session.id).last_pattern is None


def test_output_bytes_window_tracks_last_10_seconds():
    monitor = OutputMonitor()
    session_id = "bytes123"
    monitor._monitor_states[session_id] = MonitorState()
    now = datetime.now()
    monitor._output_history[session_id] = [
        (now - timedelta(seconds=12), 10),
        (now - timedelta(seconds=4), 20),
        (now - timedelta(seconds=1), 30),
    ]

    monitor._refresh_output_bytes_window(session_id, now)
    state = monitor.get_session_state(session_id)

    assert state is not None
    assert state.output_bytes_last_10s == 50
