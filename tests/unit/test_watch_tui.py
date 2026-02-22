"""Unit tests for sm watch rows/details (#309)."""

from __future__ import annotations

import time

from src.cli.watch_tui import (
    DetailFetchWorker,
    DetailSnapshot,
    _compute_column_widths,
    _session_line,
    build_watch_rows,
    can_attach_session,
    filter_sessions,
)


def _session(
    session_id: str,
    name: str,
    working_dir: str,
    *,
    parent_session_id: str | None = None,
    role: str | None = None,
    provider: str = "claude",
    activity_state: str = "idle",
    status: str = "running",
    last_tool_name: str | None = None,
    last_tool_call: str | None = None,
    last_action_summary: str | None = None,
    last_action_at: str | None = None,
    context_monitor_enabled: bool = False,
    tokens_used: int = 0,
):
    return {
        "id": session_id,
        "name": name,
        "friendly_name": None,
        "working_dir": working_dir,
        "parent_session_id": parent_session_id,
        "role": role,
        "provider": provider,
        "activity_state": activity_state,
        "status": status,
        "last_activity": "2026-02-21T23:00:00",
        "last_tool_name": last_tool_name,
        "last_tool_call": last_tool_call,
        "last_action_summary": last_action_summary,
        "last_action_at": last_action_at,
        "context_monitor_enabled": context_monitor_enabled,
        "tokens_used": tokens_used,
        "agent_status_text": None,
    }


def test_build_rows_groups_by_repo():
    sessions = [
        _session("a1", "agent-a", "/tmp/repo-a"),
        _session("b1", "agent-b", "/tmp/repo-b"),
    ]
    rows, selectable, repo_count = build_watch_rows(sessions)

    repo_rows = [row.text for row in rows if row.kind == "repo"]
    assert repo_count == 2
    assert any(row.startswith("repo-a/") for row in repo_rows)
    assert any(row.startswith("repo-b/") for row in repo_rows)
    assert selectable == ["a1", "b1"]


def test_build_rows_parent_before_child_with_tree_prefix():
    sessions = [
        _session("p1", "parent", "/tmp/repo"),
        _session("c1", "child", "/tmp/repo", parent_session_id="p1"),
    ]
    rows, _, _ = build_watch_rows(sessions)

    session_rows = [row for row in rows if row.kind == "session"]
    parent_idx = next(i for i, row in enumerate(session_rows) if row.session_id == "p1")
    child_idx = next(i for i, row in enumerate(session_rows) if row.session_id == "c1")
    assert parent_idx < child_idx
    assert session_rows[parent_idx].columns["Session"].startswith(("|-", "`-"))
    assert "[c1]" in session_rows[child_idx].columns["Session"]


def test_main_columns_include_provider_status_and_last():
    rows, _, _ = build_watch_rows(
        [
            _session(
                "s1",
                "agent",
                "/tmp/repo",
                provider="claude",
                status="running",
                last_tool_name="Read",
                last_tool_call="2026-02-21T22:59:00",
            )
        ]
    )
    session_row = next(row for row in rows if row.kind == "session")

    assert session_row.columns["Provider"] == "claude"
    assert session_row.columns["Status"] == "running"
    assert "Read" in session_row.columns["Last"]


def test_session_line_truncates_deterministically():
    rows, _, _ = build_watch_rows(
        [
            _session(
                "s1",
                "very-long-session-name-that-should-truncate",
                "/tmp/repo",
                last_tool_name="AReallyLongToolNameThatShouldTruncate",
                last_tool_call="2026-02-21T22:59:00",
            )
        ]
    )
    session_row = next(row for row in rows if row.kind == "session")

    widths = _compute_column_widths(50)
    rendered = _session_line(session_row, widths)
    assert len(rendered) >= 20
    assert "..." in rendered


def test_tab_expansion_renders_details_for_selected_session():
    session = _session(
        "s1",
        "agent",
        "/tmp/repo",
        context_monitor_enabled=True,
        tokens_used=1234,
    )
    detail = DetailSnapshot(
        action_lines=["Read (5s)", "Write (3s)"],
        tail_lines=["line one", "line two"],
        fetched_at=time.monotonic(),
        loading=False,
    )

    rows, _, _ = build_watch_rows(
        [session],
        expanded_session_ids={"s1"},
        detail_cache={"s1": detail},
    )

    detail_rows = [row.text for row in rows if row.kind == "detail"]
    assert any("context size: 1,234 tokens" in line for line in detail_rows)
    assert any("Read (5s)" in line for line in detail_rows)
    assert any("line one" in line for line in detail_rows)


def test_multi_expanded_sessions_render_independent_details():
    sessions = [
        _session("s1", "agent-1", "/tmp/repo", context_monitor_enabled=True, tokens_used=10),
        _session("s2", "agent-2", "/tmp/repo", context_monitor_enabled=True, tokens_used=20),
    ]
    rows, _, _ = build_watch_rows(
        sessions,
        expanded_session_ids={"s1", "s2"},
        detail_cache={
            "s1": DetailSnapshot(["A1"], ["T1"], time.monotonic()),
            "s2": DetailSnapshot(["A2"], ["T2"], time.monotonic()),
        },
    )

    details_s1 = [row.text for row in rows if row.kind == "detail" and row.session_id == "s1"]
    details_s2 = [row.text for row in rows if row.kind == "detail" and row.session_id == "s2"]
    assert any("A1" in line for line in details_s1)
    assert any("A2" in line for line in details_s2)


def test_codex_app_last_column_respects_projection_gate():
    session = _session(
        "app1",
        "codex-app",
        "/tmp/repo",
        provider="codex-app",
        last_action_summary="Completed command",
        last_action_at="2026-02-21T22:59:00",
    )

    rows_enabled, _, _ = build_watch_rows([session], codex_projection_enabled=True)
    row_enabled = next(row for row in rows_enabled if row.kind == "session")
    assert "Completed command" in row_enabled.columns["Last"]

    rows_disabled, _, _ = build_watch_rows([session], codex_projection_enabled=False)
    row_disabled = next(row for row in rows_disabled if row.kind == "session")
    assert row_disabled.columns["Last"] == "n/a (projection disabled)"


def test_filter_by_role():
    sessions = [
        _session("e1", "eng", "/tmp/repo", role="engineer"),
        _session("a1", "arch", "/tmp/repo", role="architect"),
    ]
    filtered = filter_sessions(sessions, role_filter="engineer")
    assert [s["id"] for s in filtered] == ["e1"]


def test_filter_by_repo_prefix():
    sessions = [
        _session("x1", "x", "/tmp/repo"),
        _session("x2", "x-child", "/tmp/repo/subdir"),
        _session("y1", "y", "/tmp/other"),
    ]
    filtered = filter_sessions(sessions, repo_filter="/tmp/repo")
    assert [s["id"] for s in filtered] == ["x1", "x2"]


def test_codex_app_rows_are_not_attachable():
    session = _session("app1", "codex-app", "/tmp/repo", provider="codex-app")
    assert can_attach_session(session) is False


class _SlowClient:
    def get_tool_calls(self, session_id: str, limit: int = 10, timeout: int | None = None):
        time.sleep(0.2)
        return {"tool_calls": [{"tool_name": "Read", "timestamp": "2026-02-21T22:59:00"}]}

    def get_output(self, session_id: str, lines: int = 10, timeout: int | None = None):
        time.sleep(0.2)
        return {"output": "one\ntwo\n"}

    def get_activity_actions(self, session_id: str, limit: int = 10):
        return {"actions": []}


def test_detail_worker_does_not_block_request_path():
    worker = DetailFetchWorker(client=_SlowClient(), codex_projection_enabled=True)
    session = _session("s1", "agent", "/tmp/repo")

    started = time.monotonic()
    worker.request(session)
    elapsed = time.monotonic() - started
    assert elapsed < 0.05

    deadline = time.monotonic() + 2.0
    snapshot = None
    while time.monotonic() < deadline:
        snapshot = worker.get("s1")
        if snapshot and not snapshot.loading:
            break
        time.sleep(0.05)

    worker.stop()

    assert snapshot is not None
    assert snapshot.loading is False
    assert any("Read" in line for line in snapshot.action_lines)
    assert any("one" in line for line in snapshot.tail_lines)
