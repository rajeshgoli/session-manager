"""
Tests for Issue #51: Auto-acquire locks on file write + worktree cleanup prompts.

Verifies:
1. Session model has touched_repos and worktrees fields
2. LockManager supports multi-repo locks
3. Helper functions work correctly
4. PreToolUse hook acquires locks on file writes
5. Worktree creation is tracked
6. Stop hook releases locks and prompts for cleanup
"""

import pytest
import tempfile
from pathlib import Path
from datetime import datetime
from unittest.mock import Mock, patch, MagicMock, AsyncMock
from src.models import Session, SessionStatus
from src.lock_manager import (
    LockManager, LockResult, LockInfo,
    get_git_root, is_worktree, has_uncommitted_changes
)


class TestSessionModel:
    """Test Session model has new fields and serialization works."""

    def test_session_has_touched_repos_field(self):
        """Session should have touched_repos field as a set."""
        session = Session(id="test", working_dir="/tmp")
        assert hasattr(session, "touched_repos")
        assert isinstance(session.touched_repos, set)
        assert len(session.touched_repos) == 0

    def test_session_has_worktrees_field(self):
        """Session should have worktrees field as a list."""
        session = Session(id="test", working_dir="/tmp")
        assert hasattr(session, "worktrees")
        assert isinstance(session.worktrees, list)
        assert len(session.worktrees) == 0

    def test_session_serialization_includes_new_fields(self):
        """to_dict should serialize touched_repos and worktrees."""
        session = Session(id="test", working_dir="/tmp")
        session.touched_repos.add("/repo1")
        session.touched_repos.add("/repo2")
        session.worktrees.append("/worktree1")

        data = session.to_dict()
        assert "touched_repos" in data
        assert "worktrees" in data
        assert set(data["touched_repos"]) == {"/repo1", "/repo2"}
        assert data["worktrees"] == ["/worktree1"]

    def test_session_deserialization_restores_new_fields(self):
        """from_dict should restore touched_repos and worktrees."""
        data = {
            "id": "test",
            "name": "test-session",
            "working_dir": "/tmp",
            "tmux_session": "claude-test",
            "log_file": "/tmp/test.log",
            "status": "running",
            "created_at": datetime.now().isoformat(),
            "last_activity": datetime.now().isoformat(),
            "touched_repos": ["/repo1", "/repo2"],
            "worktrees": ["/worktree1"],
        }

        session = Session.from_dict(data)
        assert session.touched_repos == {"/repo1", "/repo2"}
        assert session.worktrees == ["/worktree1"]


class TestLockManager:
    """Test LockManager multi-repo lock support."""

    def test_try_acquire_creates_lock_result(self, tmp_path):
        """try_acquire should return LockResult."""
        repo_root = str(tmp_path)
        (tmp_path / ".git").mkdir()

        lock_mgr = LockManager(working_dir=repo_root)
        result = lock_mgr.try_acquire(repo_root, "session1")

        assert isinstance(result, LockResult)
        assert result.acquired is True
        assert result.locked_by_other is False

    def test_try_acquire_succeeds_when_unlocked(self, tmp_path):
        """try_acquire should succeed when repo is unlocked."""
        repo_root = str(tmp_path)
        (tmp_path / ".git").mkdir()

        lock_mgr = LockManager(working_dir=repo_root)
        result = lock_mgr.try_acquire(repo_root, "session1")

        assert result.acquired is True
        assert (tmp_path / ".claude" / "workspace.lock").exists()

    def test_try_acquire_fails_when_locked_by_other(self, tmp_path):
        """try_acquire should fail when repo is locked by another session."""
        repo_root = str(tmp_path)
        (tmp_path / ".git").mkdir()

        lock_mgr = LockManager(working_dir=repo_root)

        # Session1 acquires lock
        result1 = lock_mgr.try_acquire(repo_root, "session1")
        assert result1.acquired is True

        # Session2 tries to acquire - should fail
        result2 = lock_mgr.try_acquire(repo_root, "session2")
        assert result2.acquired is False
        assert result2.locked_by_other is True
        assert result2.owner_session_id == "session1"

    def test_try_acquire_succeeds_when_same_session(self, tmp_path):
        """try_acquire should succeed when same session already holds lock."""
        repo_root = str(tmp_path)
        (tmp_path / ".git").mkdir()

        lock_mgr = LockManager(working_dir=repo_root)

        # Session1 acquires lock
        result1 = lock_mgr.try_acquire(repo_root, "session1")
        assert result1.acquired is True

        # Session1 tries again - should succeed
        result2 = lock_mgr.try_acquire(repo_root, "session1")
        assert result2.acquired is True
        assert result2.locked_by_other is False

    def test_release_lock_with_repo_root(self, tmp_path):
        """release_lock should accept repo_root parameter."""
        repo_root = str(tmp_path)
        (tmp_path / ".git").mkdir()

        lock_mgr = LockManager(working_dir=repo_root)
        lock_mgr.try_acquire(repo_root, "session1")

        # Release with repo_root
        success = lock_mgr.release_lock(repo_root=repo_root, session_id="session1")
        assert success is True
        assert not (tmp_path / ".claude" / "workspace.lock").exists()


class TestHelperFunctions:
    """Test git helper functions."""

    def test_get_git_root_returns_repo_root(self, tmp_path):
        """get_git_root should find repo root for a file path."""
        # Create a mock git repo
        git_dir = tmp_path / ".git"
        git_dir.mkdir()
        test_file = tmp_path / "src" / "test.py"
        test_file.parent.mkdir()

        with patch("subprocess.run") as mock_run:
            mock_run.return_value = Mock(
                returncode=0,
                stdout=str(tmp_path) + "\n"
            )

            result = get_git_root(str(test_file))
            assert result == str(tmp_path)

    def test_get_git_root_returns_none_when_not_in_repo(self, tmp_path):
        """get_git_root should return None when not in a git repo."""
        test_file = tmp_path / "test.py"

        with patch("subprocess.run") as mock_run:
            mock_run.return_value = Mock(returncode=1)

            result = get_git_root(str(test_file))
            assert result is None

    def test_is_worktree_returns_true_for_worktree(self, tmp_path):
        """is_worktree should return True when .git is a file."""
        git_file = tmp_path / ".git"
        git_file.write_text("gitdir: /main/repo/.git/worktrees/branch")

        result = is_worktree(str(tmp_path))
        assert result is True

    def test_is_worktree_returns_false_for_main_repo(self, tmp_path):
        """is_worktree should return False when .git is a directory."""
        git_dir = tmp_path / ".git"
        git_dir.mkdir()

        result = is_worktree(str(tmp_path))
        assert result is False

    def test_has_uncommitted_changes_returns_true_when_dirty(self, tmp_path):
        """has_uncommitted_changes should return True when there are changes."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = Mock(
                returncode=0,
                stdout="M src/test.py\n"
            )

            result = has_uncommitted_changes(str(tmp_path))
            assert result is True

    def test_has_uncommitted_changes_returns_false_when_clean(self, tmp_path):
        """has_uncommitted_changes should return False when clean."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = Mock(
                returncode=0,
                stdout=""
            )

            result = has_uncommitted_changes(str(tmp_path))
            assert result is False


class TestPreToolUseHook:
    """Test PreToolUse hook auto-lock acquisition."""

    def test_edit_tool_acquires_lock(self, test_client, session_manager, tmp_path):
        """Edit tool should trigger lock acquisition."""
        # Create session
        session = Session(id="test", working_dir=str(tmp_path))
        session_manager.sessions["test"] = session

        # Mock git root
        git_dir = tmp_path / ".git"
        git_dir.mkdir()

        # Send PreToolUse hook for Edit
        with patch("src.lock_manager.get_git_root", return_value=str(tmp_path)):
            response = test_client.post(
                "/hooks/tool-use",
                json={
                    "session_manager_id": "test",
                    "hook_event_name": "PreToolUse",
                    "tool_name": "Edit",
                    "tool_input": {"file_path": str(tmp_path / "test.py")},
                    "cwd": str(tmp_path),
                }
            )

        assert response.status_code == 200
        # Session should have tracked the repo
        assert str(tmp_path) in session.touched_repos

    def test_write_tool_acquires_lock(self, test_client, session_manager, tmp_path):
        """Write tool should trigger lock acquisition."""
        session = Session(id="test", working_dir=str(tmp_path))
        session_manager.sessions["test"] = session

        git_dir = tmp_path / ".git"
        git_dir.mkdir()

        with patch("src.lock_manager.get_git_root", return_value=str(tmp_path)):
            response = test_client.post(
                "/hooks/tool-use",
                json={
                    "session_manager_id": "test",
                    "hook_event_name": "PreToolUse",
                    "tool_name": "Write",
                    "tool_input": {"file_path": str(tmp_path / "new.py")},
                    "cwd": str(tmp_path),
                }
            )

        assert response.status_code == 200
        assert str(tmp_path) in session.touched_repos

    def test_lock_error_returned_when_locked_by_other(self, test_client, session_manager, tmp_path):
        """Hook should return error when repo is locked by another session."""
        # Create two sessions
        session1 = Session(id="session1", working_dir=str(tmp_path), friendly_name="Engineer")
        session2 = Session(id="session2", working_dir=str(tmp_path))
        session_manager.sessions["session1"] = session1
        session_manager.sessions["session2"] = session2

        git_dir = tmp_path / ".git"
        git_dir.mkdir()

        # Session1 acquires lock
        with patch("src.lock_manager.get_git_root", return_value=str(tmp_path)):
            test_client.post(
                "/hooks/tool-use",
                json={
                    "session_manager_id": "session1",
                    "hook_event_name": "PreToolUse",
                    "tool_name": "Edit",
                    "tool_input": {"file_path": str(tmp_path / "test.py")},
                    "cwd": str(tmp_path),
                }
            )

        # Session2 tries to acquire - should get error
        with patch("src.lock_manager.get_git_root", return_value=str(tmp_path)):
            response = test_client.post(
                "/hooks/tool-use",
                json={
                    "session_manager_id": "session2",
                    "hook_event_name": "PreToolUse",
                    "tool_name": "Edit",
                    "tool_input": {"file_path": str(tmp_path / "test.py")},
                    "cwd": str(tmp_path),
                }
            )

        data = response.json()
        assert data["status"] == "error"
        assert "locked by session [Engineer]" in data["error"]
        assert "git worktree add" in data["error"]


class TestWorktreeTracking:
    """Test worktree creation tracking."""

    def test_bash_hook_detects_worktree_add(self, test_client, session_manager, tmp_path):
        """Bash hook should detect git worktree add commands."""
        session = Session(id="test", working_dir=str(tmp_path))
        session_manager.sessions["test"] = session

        # Send Bash PreToolUse with worktree add
        response = test_client.post(
            "/hooks/tool-use",
            json={
                "session_manager_id": "test",
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": "git worktree add ../my-feature feature-branch"},
                "cwd": str(tmp_path),
            }
        )

        assert response.status_code == 200
        # Session should have tracked the worktree
        expected_path = str((tmp_path / "../my-feature").resolve())
        assert expected_path in session.worktrees

    def test_bash_hook_ignores_non_worktree_commands(self, test_client, session_manager, tmp_path):
        """Bash hook should ignore non-worktree commands."""
        session = Session(id="test", working_dir=str(tmp_path))
        session_manager.sessions["test"] = session

        response = test_client.post(
            "/hooks/tool-use",
            json={
                "session_manager_id": "test",
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": "git status"},
                "cwd": str(tmp_path),
            }
        )

        assert response.status_code == 200
        assert len(session.worktrees) == 0


class TestStopHookCleanup:
    """Test Stop hook lock release and cleanup prompts."""

    def test_stop_hook_releases_locks(self, test_client, session_manager, tmp_path):
        """Stop hook should release all locks held by session."""
        session = Session(id="test", working_dir=str(tmp_path))
        session_manager.sessions["test"] = session

        # Add touched repos
        repo1 = tmp_path / "repo1"
        repo1.mkdir()
        (repo1 / ".git").mkdir()
        (repo1 / ".claude").mkdir()

        session.touched_repos.add(str(repo1))

        # Create lock file
        lock_file = repo1 / ".claude" / "workspace.lock"
        lock_file.write_text(f"session=test\ntask=test\nbranch=main\nstarted={datetime.now().isoformat()}\n")

        # Send Stop hook
        with patch("src.session_manager.SessionManager.send_input", new_callable=AsyncMock) as mock_send:
            test_client.post(
                "/hooks/claude",
                json={
                    "hook_event_name": "Stop",
                    "session_manager_id": "test",
                    "transcript_path": "/tmp/transcript.jsonl",
                }
            )

        # Lock should be released
        assert not lock_file.exists()

    def test_stop_hook_sends_cleanup_prompt_for_dirty_worktree(self, test_client, session_manager, tmp_path):
        """Stop hook should send cleanup prompt for worktrees with uncommitted changes."""
        session = Session(id="test", working_dir=str(tmp_path))
        session_manager.sessions["test"] = session

        # Add worktree
        worktree = tmp_path / "worktree"
        worktree.mkdir()
        (worktree / ".git").write_text("gitdir: /main/.git/worktrees/branch")

        session.touched_repos.add(str(worktree))

        # Mock status hash to return a dirty hash
        with patch("src.lock_manager.is_worktree", return_value=True):
            with patch("src.lock_manager.get_worktree_status_hash", return_value="hash1"):
                with patch("src.session_manager.SessionManager.send_input", new_callable=AsyncMock) as mock_send:
                    test_client.post(
                        "/hooks/claude",
                        json={
                            "hook_event_name": "Stop",
                            "session_manager_id": "test",
                            "transcript_path": "/tmp/transcript.jsonl",
                        }
                    )

                    # Should have sent cleanup prompt
                    mock_send.assert_called_once()
                    call_args = mock_send.call_args
                    assert "uncommitted changes" in call_args[0][1]
                    assert "git push" in call_args[0][1]
                    assert call_args[1]["delivery_mode"] == "important"

    def test_stop_hook_does_not_repeat_cleanup_prompt_for_same_changes(self, test_client, session_manager, tmp_path):
        """Stop hook should not resend cleanup prompt for unchanged dirty worktree."""
        session = Session(id="test", working_dir=str(tmp_path))
        session_manager.sessions["test"] = session

        worktree = tmp_path / "worktree"
        worktree.mkdir()
        (worktree / ".git").write_text("gitdir: /main/.git/worktrees/branch")

        session.touched_repos.add(str(worktree))
        session.cleanup_prompted[str(worktree)] = "hash1"

        with patch("src.lock_manager.is_worktree", return_value=True):
            with patch("src.lock_manager.get_worktree_status_hash", return_value="hash1"):
                with patch("src.session_manager.SessionManager.send_input", new_callable=AsyncMock) as mock_send:
                    test_client.post(
                        "/hooks/claude",
                        json={
                            "hook_event_name": "Stop",
                            "session_manager_id": "test",
                            "transcript_path": "/tmp/transcript.jsonl",
                        }
                    )

                    mock_send.assert_not_called()

    def test_stop_hook_no_prompt_for_clean_worktree(self, test_client, session_manager, tmp_path):
        """Stop hook should not prompt for clean worktrees."""
        session = Session(id="test", working_dir=str(tmp_path))
        session_manager.sessions["test"] = session

        worktree = tmp_path / "worktree"
        worktree.mkdir()
        (worktree / ".git").write_text("gitdir: /main/.git/worktrees/branch")

        session.touched_repos.add(str(worktree))

        # Mock status hash to return None (clean)
        with patch("src.lock_manager.is_worktree", return_value=True):
            with patch("src.lock_manager.get_worktree_status_hash", return_value=None):
                with patch("src.session_manager.SessionManager.send_input", new_callable=AsyncMock) as mock_send:
                    test_client.post(
                        "/hooks/claude",
                        json={
                            "hook_event_name": "Stop",
                            "session_manager_id": "test",
                            "transcript_path": "/tmp/transcript.jsonl",
                        }
                    )

                    # Should not have sent cleanup prompt
                    mock_send.assert_not_called()
