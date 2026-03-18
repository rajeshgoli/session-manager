"""Unit tests for sm watch rows/details (#309)."""

from __future__ import annotations

import time

from src.cli.watch_tui import (
    DetailFetchWorker,
    DetailSnapshot,
    _create_watch_session,
    _compute_column_widths,
    _default_create_working_dir,
    _normalize_create_working_dir,
    _render_columns,
    _resolve_create_provider,
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
    agent_status_text: str | None = None,
    agent_status_at: str | None = None,
    agent_task_completed_at: str | None = None,
    pending_adoption_proposals: list[dict] | None = None,
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
        "agent_status_text": agent_status_text,
        "agent_status_at": agent_status_at,
        "agent_task_completed_at": agent_task_completed_at,
        "pending_adoption_proposals": pending_adoption_proposals or [],
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
    assert session_rows[child_idx].columns["ID"] == "c1"
    assert "[c1]" not in session_rows[child_idx].columns["Session"]


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

    assert session_row.columns["ID"] == "s1"
    assert session_row.columns["Provider"] == "claude"
    assert session_row.columns["Status"] == "running"
    assert "Read" in session_row.columns["Last"]
    assert session_row.columns["Parent"] == "-"


def test_parent_column_shows_parent_name_and_id():
    rows, _, _ = build_watch_rows(
        [
            _session("p1", "em-parent", "/tmp/repo"),
            _session("c1", "child", "/tmp/repo", parent_session_id="p1"),
        ]
    )
    child_row = next(row for row in rows if row.kind == "session" and row.session_id == "c1")
    assert child_row.columns["Parent"] == "em-parent [p1]"


def test_parent_column_survives_cross_repo_grouping():
    rows, _, _ = build_watch_rows(
        [
            _session("p1", "em-parent", "/tmp/repo-a"),
            _session("c1", "child", "/tmp/repo-b", parent_session_id="p1"),
        ]
    )
    child_row = next(row for row in rows if row.kind == "session" and row.session_id == "c1")
    assert child_row.columns["Parent"] == "em-parent [p1]"


def test_cross_repo_child_renders_as_nested_repo_subtree():
    rows, selectable, repo_count = build_watch_rows(
        [
            _session("p1", "em-parent", "/tmp/repo-a"),
            _session("c1", "child", "/tmp/repo-b", parent_session_id="p1"),
        ]
    )

    repo_rows = [row for row in rows if row.kind == "repo"]
    nested_repo_rows = [row for row in rows if row.kind == "repo_ref"]
    session_rows = [row for row in rows if row.kind == "session"]

    assert repo_count == 2
    assert len(repo_rows) == 1
    assert repo_rows[0].text.startswith("repo-a/")
    assert len(nested_repo_rows) == 1
    assert "repo-b/" in nested_repo_rows[0].text
    assert nested_repo_rows[0].text.startswith("   `-")

    parent_idx = next(i for i, row in enumerate(rows) if row.kind == "session" and row.session_id == "p1")
    nested_repo_idx = next(i for i, row in enumerate(rows) if row.kind == "repo_ref")
    child_idx = next(i for i, row in enumerate(rows) if row.kind == "session" and row.session_id == "c1")
    assert parent_idx < nested_repo_idx < child_idx
    assert selectable == ["p1", "c1"]
    assert session_rows[1].columns["Session"].startswith("      `-child")
    assert session_rows[1].columns["ID"] == "c1"


def test_status_row_shows_text_and_age():
    rows, _, _ = build_watch_rows(
        [
            _session(
                "s1",
                "agent",
                "/tmp/repo",
                agent_status_text="investigating queue race",
                agent_status_at="2026-02-21T22:59:00",
            )
        ]
    )
    status_rows = [row for row in rows if row.kind == "status"]
    assert any('status: "investigating queue race"' in row.text for row in status_rows)
    assert any("(" in row.text and ")" in row.text for row in status_rows)


def test_task_completed_row_shows_age():
    rows, _, _ = build_watch_rows(
        [
            _session(
                "s1",
                "agent",
                "/tmp/repo",
                agent_task_completed_at="2026-02-21T22:58:00",
            )
        ]
    )
    status_rows = [row for row in rows if row.kind == "status"]
    assert any("task: completed (" in row.text for row in status_rows)


def test_pending_adoption_row_shows_proposer_and_actions():
    rows, _, _ = build_watch_rows(
        [
            _session(
                "s1",
                "agent",
                "/tmp/repo",
                pending_adoption_proposals=[
                    {
                        "id": "proposal123",
                        "proposer_session_id": "em123456",
                        "proposer_name": "em-ops",
                        "target_session_id": "s1",
                        "created_at": "2026-02-21T22:58:00",
                        "status": "pending",
                        "decided_at": None,
                    }
                ],
            )
        ]
    )

    status_rows = [row for row in rows if row.kind == "status"]
    assert any("adopt: pending from em-ops [em123456]" in row.text for row in status_rows)
    assert any("[A accept / X reject]" in row.text for row in status_rows)


def test_status_rows_follow_tree_indentation():
    rows, _, _ = build_watch_rows(
        [
            _session(
                "p1",
                "parent",
                "/tmp/repo",
                agent_status_text="parent status",
                agent_status_at="2026-02-21T22:59:00",
            ),
            _session(
                "c1",
                "child",
                "/tmp/repo",
                parent_session_id="p1",
                agent_status_text="child status",
                agent_status_at="2026-02-21T22:58:00",
            ),
        ]
    )

    parent_status = next(row for row in rows if row.kind == "status" and "parent status" in row.text)
    child_status = next(row for row in rows if row.kind == "status" and "child status" in row.text)
    assert parent_status.text.startswith("  status:")
    assert child_status.text.startswith("     status:")


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


def test_render_columns_uses_full_visible_width_except_reserved_footer_cell():
    assert _render_columns(80, 0) == 80
    assert _render_columns(80, 2) == 78
    assert _render_columns(80, 4) == 76
    assert _render_columns(80, 0, reserve_last_cell=True) == 79


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


def test_detail_worker_fetches_codex_fork_actions():
    client = type(
        "_Client",
        (),
        {
            "get_tool_calls": staticmethod(
                lambda session_id, limit, timeout: {
                    "tool_calls": [
                        {"tool_name": "exec_command", "timestamp": "2026-02-21T22:59:55"},
                        {"tool_name": "sm_send", "timestamp": "2026-02-21T22:59:58"},
                    ]
                }
            )
        },
    )()
    worker = DetailFetchWorker(client=client, codex_projection_enabled=True)

    lines = worker._fetch_actions("fork1234", "codex-fork")

    assert len(lines) == 2
    assert lines[0].startswith("exec_command")
    assert lines[1].startswith("sm_send")


def test_detail_worker_handles_codex_fork_unavailable():
    client = type("_Client", (), {"get_tool_calls": staticmethod(lambda session_id, limit, timeout: None)})()
    worker = DetailFetchWorker(client=client, codex_projection_enabled=True)

    assert worker._fetch_actions("fork1234", "codex-fork") == ["n/a (unavailable)"]


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


def test_filter_by_repo_includes_cross_repo_ancestors_for_context():
    sessions = [
        _session("p1", "em-parent", "/tmp/repo-a"),
        _session("c1", "child", "/tmp/repo-b", parent_session_id="p1"),
    ]

    filtered = filter_sessions(sessions, repo_filter="/tmp/repo-b")

    assert [s["id"] for s in filtered] == ["p1", "c1"]

    rows, _, repo_count = build_watch_rows(filtered)
    assert repo_count == 2
    assert any(row.kind == "repo_ref" and "repo-b/" in row.text for row in rows)
    child_row = next(row for row in rows if row.kind == "session" and row.session_id == "c1")
    assert child_row.columns["Parent"] == "em-parent [p1]"


def test_filter_by_repo_includes_cross_repo_descendants_for_context():
    sessions = [
        _session("p1", "em-parent", "/tmp/repo-a"),
        _session("c1", "child", "/tmp/repo-b", parent_session_id="p1"),
    ]

    filtered = filter_sessions(sessions, repo_filter="/tmp/repo-a")

    assert [s["id"] for s in filtered] == ["p1", "c1"]

    rows, selectable, repo_count = build_watch_rows(filtered)
    assert repo_count == 2
    parent_idx = next(i for i, row in enumerate(rows) if row.kind == "session" and row.session_id == "p1")
    nested_repo_idx = next(i for i, row in enumerate(rows) if row.kind == "repo_ref")
    child_idx = next(i for i, row in enumerate(rows) if row.kind == "session" and row.session_id == "c1")
    assert parent_idx < nested_repo_idx < child_idx
    assert selectable == ["p1", "c1"]


def test_filter_by_role_does_not_pull_hierarchy_context():
    sessions = [
        _session("p1", "architect-parent", "/tmp/repo-a", role="architect"),
        _session("c1", "engineer-child", "/tmp/repo-b", parent_session_id="p1", role="engineer"),
    ]

    filtered_engineers = filter_sessions(sessions, role_filter="engineer")
    filtered_architects = filter_sessions(sessions, role_filter="architect")

    assert [s["id"] for s in filtered_engineers] == ["c1"]
    assert [s["id"] for s in filtered_architects] == ["p1"]


def test_filter_by_text_does_not_pull_hierarchy_context():
    sessions = [
        _session("p1", "architect-parent", "/tmp/repo-a"),
        _session("c1", "engineer-child", "/tmp/repo-b", parent_session_id="p1"),
    ]

    filtered = filter_sessions(sessions, text_filter="engineer-child")

    assert [s["id"] for s in filtered] == ["c1"]


def test_filter_by_repo_and_role_does_not_pull_hierarchy_context():
    sessions = [
        _session("p1", "architect-parent", "/tmp/repo-a", role="architect"),
        _session("c1", "engineer-child", "/tmp/repo-b", parent_session_id="p1", role="engineer"),
    ]

    filtered = filter_sessions(sessions, repo_filter="/tmp/repo-b", role_filter="engineer")

    assert [s["id"] for s in filtered] == ["c1"]


def test_filter_by_repo_and_text_does_not_pull_hierarchy_context():
    sessions = [
        _session("p1", "architect-parent", "/tmp/repo-a"),
        _session("c1", "engineer-child", "/tmp/repo-b", parent_session_id="p1"),
    ]

    filtered = filter_sessions(sessions, repo_filter="/tmp/repo-b", text_filter="engineer-child")

    assert [s["id"] for s in filtered] == ["c1"]


def test_codex_app_rows_are_not_attachable():
    session = _session("app1", "codex-app", "/tmp/repo", provider="codex-app")
    assert can_attach_session(session) is False


def test_default_create_working_dir_prefers_selected_session_then_repo_filter(monkeypatch, tmp_path):
    monkeypatch.chdir(tmp_path)

    assert _default_create_working_dir(_session("s1", "agent", "/tmp/selected"), None) == "/tmp/selected"
    assert _default_create_working_dir(None, "/tmp/filter") == "/tmp/filter"
    assert _default_create_working_dir(None, None) == str(tmp_path)


def test_normalize_create_working_dir_resolves_relative_paths(monkeypatch, tmp_path):
    child = tmp_path / "child"
    child.mkdir()
    monkeypatch.chdir(tmp_path)

    normalized, error = _normalize_create_working_dir("./child")

    assert error is None
    assert normalized == str(child.resolve())


def test_normalize_create_working_dir_rejects_missing_path(tmp_path):
    normalized, error = _normalize_create_working_dir(str(tmp_path / "missing"))

    assert normalized is None
    assert error == f"Working dir does not exist: {tmp_path / 'missing'}"


def test_resolve_create_provider_maps_supported_aliases():
    assert _resolve_create_provider("") == "codex-fork"
    assert _resolve_create_provider("codex") == "codex-fork"
    assert _resolve_create_provider("co") == "codex-fork"
    assert _resolve_create_provider("claude") == "claude"
    assert _resolve_create_provider("cl") == "claude"
    assert _resolve_create_provider("weird") is None


def test_create_watch_session_passes_parent_session_id_and_returns_attach_target():
    client = type(
        "_Client",
        (),
        {
            "session_id": "parent123",
            "create_session_result": staticmethod(
                lambda working_dir, provider, parent_session_id: {
                    "ok": True,
                    "unavailable": False,
                    "status_code": 200,
                    "detail": None,
                    "data": {
                        "id": "child456",
                        "tmux_session": "codex-fork-child456",
                    },
                }
                if (working_dir, provider, parent_session_id) == ("/tmp/repo", "codex-fork", "parent123")
                else {"ok": False, "unavailable": False, "status_code": 400, "detail": "bad request", "data": None}
            ),
            "get_attach_descriptor": staticmethod(
                lambda session_id: {"tmux_session": "descriptor-child456"} if session_id == "child456" else None
            ),
        },
    )()

    session, tmux_session, error = _create_watch_session(client, "codex-fork", "/tmp/repo")

    assert error is None
    assert session["id"] == "child456"
    assert tmux_session == "descriptor-child456"


def test_create_watch_session_returns_attach_error_when_not_supported():
    client = type(
        "_Client",
        (),
        {
            "session_id": "parent123",
            "create_session_result": staticmethod(
                lambda working_dir, provider, parent_session_id: {
                    "ok": True,
                    "unavailable": False,
                    "status_code": 200,
                    "detail": None,
                    "data": {
                        "id": "app789",
                        "tmux_session": None,
                    },
                }
            ),
            "get_attach_descriptor": staticmethod(
                lambda session_id: {"attach_supported": False, "message": "No terminal for this provider"}
            ),
        },
    )()

    session, tmux_session, error = _create_watch_session(client, "claude", "/tmp/repo")

    assert session["id"] == "app789"
    assert tmux_session is None
    assert error == "No terminal for this provider"


def test_create_watch_session_preserves_api_error_detail():
    client = type(
        "_Client",
        (),
        {
            "session_id": "parent123",
            "create_session_result": staticmethod(
                lambda working_dir, provider, parent_session_id: {
                    "ok": False,
                    "unavailable": False,
                    "status_code": 422,
                    "detail": "Provider not enabled",
                    "data": {"detail": "Provider not enabled"},
                }
            ),
            "get_attach_descriptor": staticmethod(lambda session_id: None),
        },
    )()

    session, tmux_session, error = _create_watch_session(client, "codex-fork", "/tmp/repo")

    assert session is None
    assert tmux_session is None
    assert error == "Provider not enabled"


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
