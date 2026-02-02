"""
Regression tests for issue #42: Refactor asymmetric create_session vs spawn_child_session

Tests verify that both methods produce consistent sessions with proper tmux_session naming.
"""

import pytest
from unittest.mock import Mock, AsyncMock, MagicMock, patch

from src.models import Session, SessionStatus
from src.session_manager import SessionManager


@pytest.fixture
def mock_tmux():
    """Create a mock TmuxController."""
    tmux = Mock()
    tmux.create_session_with_command = Mock(return_value=True)
    return tmux


@pytest.fixture
def session_manager_with_mock_tmux(tmp_path, mock_tmux):
    """Create a SessionManager with mocked tmux."""
    config = {
        "claude": {
            "command": "claude",
            "args": ["--verbose"],
            "default_model": "sonnet",
        }
    }

    sm = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config=config,
    )

    # Replace tmux controller with mock
    sm.tmux = mock_tmux

    return sm


class TestTmuxSessionNaming:
    """Test that tmux_session is always claude-{id}, never the name."""

    @pytest.mark.asyncio
    async def test_create_session_tmux_naming_without_name(self, session_manager_with_mock_tmux):
        """Test that create_session uses claude-{id} for tmux_session when no name provided."""
        sm = session_manager_with_mock_tmux

        # Mock git remote detection
        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            session = await sm.create_session(working_dir="/tmp/test")

        assert session is not None
        # tmux_session should be claude-{id}
        assert session.tmux_session == f"claude-{session.id}"
        # name should also be claude-{id} (default)
        assert session.name == f"claude-{session.id}"

    @pytest.mark.asyncio
    async def test_create_session_tmux_naming_with_name(self, session_manager_with_mock_tmux):
        """Test that create_session uses claude-{id} for tmux_session even with custom name."""
        sm = session_manager_with_mock_tmux

        # Mock git remote detection
        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            session = await sm.create_session(working_dir="/tmp/test", name="my-custom-name")

        assert session is not None
        # tmux_session should ALWAYS be claude-{id}, NOT the custom name
        assert session.tmux_session == f"claude-{session.id}"
        # name should be the custom name
        assert session.name == "my-custom-name"

    @pytest.mark.asyncio
    async def test_spawn_child_session_tmux_naming(self, session_manager_with_mock_tmux):
        """Test that spawn_child_session uses claude-{id} for tmux_session."""
        sm = session_manager_with_mock_tmux

        # Create parent session
        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            parent = await sm.create_session(working_dir="/tmp/test")

        # Spawn child
        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            child = await sm.spawn_child_session(
                parent_session_id=parent.id,
                prompt="Do something",
                name="child-agent",
            )

        assert child is not None
        # tmux_session should be claude-{id}
        assert child.tmux_session == f"claude-{child.id}"
        # friendly_name should be set to the name parameter
        assert child.friendly_name == "child-agent"

    @pytest.mark.asyncio
    async def test_spawn_child_without_name_tmux_naming(self, session_manager_with_mock_tmux):
        """Test that spawn_child_session without name still uses claude-{id} for tmux."""
        sm = session_manager_with_mock_tmux

        # Create parent session
        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            parent = await sm.create_session(working_dir="/tmp/test")

        # Spawn child without name
        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            child = await sm.spawn_child_session(
                parent_session_id=parent.id,
                prompt="Do something",
            )

        assert child is not None
        # tmux_session should be claude-{id}
        assert child.tmux_session == f"claude-{child.id}"
        # name should be auto-generated child-{parent_id}
        assert child.name.startswith("child-")


class TestConsistentBehavior:
    """Test that create_session and spawn_child_session behave consistently."""

    @pytest.mark.asyncio
    async def test_both_set_git_remote(self, session_manager_with_mock_tmux):
        """Test that both methods detect git remote URL."""
        sm = session_manager_with_mock_tmux

        # Mock git remote detection to return a URL
        git_url = "https://github.com/test/repo.git"

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=git_url):
            # Test create_session
            session1 = await sm.create_session(working_dir="/tmp/test")
            assert session1.git_remote_url == git_url

            # Test spawn_child_session
            session2 = await sm.spawn_child_session(
                parent_session_id=session1.id,
                prompt="Do something",
            )
            assert session2.git_remote_url == git_url

    @pytest.mark.asyncio
    async def test_both_set_status_running(self, session_manager_with_mock_tmux):
        """Test that both methods set status to RUNNING."""
        sm = session_manager_with_mock_tmux

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            # Test create_session
            session1 = await sm.create_session(working_dir="/tmp/test")
            assert session1.status == SessionStatus.RUNNING

            # Test spawn_child_session
            session2 = await sm.spawn_child_session(
                parent_session_id=session1.id,
                prompt="Do something",
            )
            assert session2.status == SessionStatus.RUNNING

    @pytest.mark.asyncio
    async def test_both_save_state(self, session_manager_with_mock_tmux):
        """Test that both methods save state."""
        sm = session_manager_with_mock_tmux

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            # Mock _save_state
            with patch.object(sm, '_save_state') as mock_save:
                # Test create_session
                session1 = await sm.create_session(working_dir="/tmp/test")
                assert mock_save.call_count >= 1

                # Test spawn_child_session
                mock_save.reset_mock()
                session2 = await sm.spawn_child_session(
                    parent_session_id=session1.id,
                    prompt="Do something",
                )
                assert mock_save.call_count >= 1

    @pytest.mark.asyncio
    async def test_both_add_to_sessions_dict(self, session_manager_with_mock_tmux):
        """Test that both methods add session to sessions dict."""
        sm = session_manager_with_mock_tmux

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            # Test create_session
            session1 = await sm.create_session(working_dir="/tmp/test")
            assert session1.id in sm.sessions
            assert sm.sessions[session1.id] == session1

            # Test spawn_child_session
            session2 = await sm.spawn_child_session(
                parent_session_id=session1.id,
                prompt="Do something",
            )
            assert session2.id in sm.sessions
            assert sm.sessions[session2.id] == session2

    @pytest.mark.asyncio
    async def test_both_use_claude_config(self, session_manager_with_mock_tmux):
        """Test that both methods use Claude config args."""
        sm = session_manager_with_mock_tmux
        mock_tmux = sm.tmux

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            # Test create_session
            session1 = await sm.create_session(working_dir="/tmp/test")

            # Verify tmux was called with config args
            call_kwargs = mock_tmux.create_session_with_command.call_args[1]
            assert call_kwargs['command'] == "claude"
            assert call_kwargs['args'] == ["--verbose"]

            # Reset mock
            mock_tmux.create_session_with_command.reset_mock()

            # Test spawn_child_session
            session2 = await sm.spawn_child_session(
                parent_session_id=session1.id,
                prompt="Do something",
            )

            # Verify tmux was called with config args
            call_kwargs = mock_tmux.create_session_with_command.call_args[1]
            assert call_kwargs['command'] == "claude"
            assert call_kwargs['args'] == ["--verbose"]


class TestParentChildRelationships:
    """Test that parent-child relationships are preserved."""

    @pytest.mark.asyncio
    async def test_child_has_parent_id(self, session_manager_with_mock_tmux):
        """Test that child session has parent_session_id set."""
        sm = session_manager_with_mock_tmux

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            parent = await sm.create_session(working_dir="/tmp/test")
            child = await sm.spawn_child_session(
                parent_session_id=parent.id,
                prompt="Do something",
            )

        assert child.parent_session_id == parent.id

    @pytest.mark.asyncio
    async def test_child_has_spawn_prompt(self, session_manager_with_mock_tmux):
        """Test that child session has spawn_prompt set."""
        sm = session_manager_with_mock_tmux

        prompt = "Build a web scraper"

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            parent = await sm.create_session(working_dir="/tmp/test")
            child = await sm.spawn_child_session(
                parent_session_id=parent.id,
                prompt=prompt,
            )

        assert child.spawn_prompt == prompt

    @pytest.mark.asyncio
    async def test_child_has_spawned_at(self, session_manager_with_mock_tmux):
        """Test that child session has spawned_at timestamp."""
        sm = session_manager_with_mock_tmux

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            parent = await sm.create_session(working_dir="/tmp/test")
            child = await sm.spawn_child_session(
                parent_session_id=parent.id,
                prompt="Do something",
            )

        assert child.spawned_at is not None

    @pytest.mark.asyncio
    async def test_parent_has_no_parent_fields(self, session_manager_with_mock_tmux):
        """Test that parent session has no parent-related fields."""
        sm = session_manager_with_mock_tmux

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            parent = await sm.create_session(working_dir="/tmp/test")

        assert parent.parent_session_id is None
        assert parent.spawn_prompt is None
        assert parent.spawned_at is None


class TestModelParameter:
    """Test model parameter handling."""

    @pytest.mark.asyncio
    async def test_spawn_child_with_model_override(self, session_manager_with_mock_tmux):
        """Test that spawn_child_session uses model override."""
        sm = session_manager_with_mock_tmux
        mock_tmux = sm.tmux

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            parent = await sm.create_session(working_dir="/tmp/test")
            child = await sm.spawn_child_session(
                parent_session_id=parent.id,
                prompt="Do something",
                model="opus",
            )

        # Verify tmux was called with model override
        call_kwargs = mock_tmux.create_session_with_command.call_args[1]
        assert call_kwargs['model'] == "opus"

    @pytest.mark.asyncio
    async def test_spawn_child_without_model_uses_default(self, session_manager_with_mock_tmux):
        """Test that spawn_child_session without model uses None (tmux decides)."""
        sm = session_manager_with_mock_tmux
        mock_tmux = sm.tmux

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            parent = await sm.create_session(working_dir="/tmp/test")
            child = await sm.spawn_child_session(
                parent_session_id=parent.id,
                prompt="Do something",
            )

        # Verify tmux was called with model=None (uses default)
        call_kwargs = mock_tmux.create_session_with_command.call_args[1]
        assert call_kwargs['model'] is None


class TestErrorHandling:
    """Test error handling in both methods."""

    @pytest.mark.asyncio
    async def test_create_session_fails_if_tmux_fails(self, session_manager_with_mock_tmux):
        """Test that create_session returns None if tmux creation fails."""
        sm = session_manager_with_mock_tmux
        sm.tmux.create_session_with_command = Mock(return_value=False)

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            session = await sm.create_session(working_dir="/tmp/test")

        assert session is None

    @pytest.mark.asyncio
    async def test_spawn_child_fails_if_tmux_fails(self, session_manager_with_mock_tmux):
        """Test that spawn_child_session returns None if tmux creation fails."""
        sm = session_manager_with_mock_tmux

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            parent = await sm.create_session(working_dir="/tmp/test")

            # Make tmux fail
            sm.tmux.create_session_with_command = Mock(return_value=False)

            child = await sm.spawn_child_session(
                parent_session_id=parent.id,
                prompt="Do something",
            )

        assert child is None

    @pytest.mark.asyncio
    async def test_spawn_child_fails_if_parent_not_found(self, session_manager_with_mock_tmux):
        """Test that spawn_child_session returns None if parent not found."""
        sm = session_manager_with_mock_tmux

        with patch.object(sm, '_get_git_remote_url_async', new_callable=AsyncMock, return_value=None):
            child = await sm.spawn_child_session(
                parent_session_id="nonexistent",
                prompt="Do something",
            )

        assert child is None
