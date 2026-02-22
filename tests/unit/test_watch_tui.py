"""Unit tests for sm watch row building and filtering (#289)."""

from src.cli.watch_tui import build_watch_rows, can_attach_session, filter_sessions


def _session(
    session_id: str,
    name: str,
    working_dir: str,
    *,
    parent_session_id: str | None = None,
    role: str | None = None,
    provider: str = "claude",
    activity_state: str = "idle",
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
        "last_activity": "2026-02-21T23:00:00",
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
    assert "|-" in session_rows[parent_idx].text or "`-" in session_rows[parent_idx].text
    assert "|  " in session_rows[child_idx].text or "   " in session_rows[child_idx].text


def test_unparented_sessions_are_roots():
    sessions = [
        _session("r1", "root-1", "/tmp/repo"),
        _session("r2", "root-2", "/tmp/repo", parent_session_id="missing-parent"),
    ]
    rows, selectable, _ = build_watch_rows(sessions)

    session_rows = [row for row in rows if row.kind == "session"]
    assert len(session_rows) == 2
    assert selectable == ["r1", "r2"]


def test_build_rows_does_not_merge_same_basename_paths():
    sessions = [
        _session("a1", "alpha", "/tmp/a/repo"),
        _session("b1", "beta", "/tmp/b/repo"),
    ]
    rows, selectable, repo_count = build_watch_rows(sessions)

    repo_rows = [row.text for row in rows if row.kind == "repo"]
    assert repo_count == 2
    assert len(repo_rows) == 2
    assert repo_rows[0] != repo_rows[1]
    assert all(row.startswith("repo/") for row in repo_rows)
    assert all("(" in row and ")" in row for row in repo_rows)
    assert selectable == ["a1", "b1"]


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
