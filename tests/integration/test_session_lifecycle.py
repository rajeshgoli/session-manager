"""Integration tests for session lifecycle - ticket #66."""

import pytest
import json
import tempfile
import asyncio
from datetime import datetime
from pathlib import Path
from unittest.mock import MagicMock, AsyncMock, patch

from src.session_manager import SessionManager
from src.models import Session, SessionStatus, CompletionStatus
from src.tmux_controller import TmuxController


@pytest.fixture
def mock_tmux():
    """Mock TmuxController that tracks create/kill calls without real tmux."""
    mock = MagicMock(spec=TmuxController)
    mock.session_exists.return_value = True
    mock.create_session.return_value = True
    mock.create_session_with_command.return_value = True
    mock.send_input.return_value = True
    mock.send_input_async = AsyncMock(return_value=True)
    mock.send_key.return_value = True
    mock.kill_session.return_value = True
    mock.list_sessions.return_value = []
    mock.capture_pane.return_value = "Mock output"
    mock.set_status_bar.return_value = True
    mock.open_in_terminal.return_value = True
    return mock


@pytest.fixture
def temp_state_file(tmp_path):
    """Create a temporary state file."""
    state_file = tmp_path / "sessions.json"
    state_file.write_text(json.dumps({"sessions": []}))
    return state_file


@pytest.fixture
def temp_log_dir(tmp_path):
    """Create a temporary log directory."""
    log_dir = tmp_path / "logs"
    log_dir.mkdir()
    return log_dir


@pytest.fixture
def session_manager(mock_tmux, temp_state_file, temp_log_dir):
    """Create a SessionManager with mocked tmux."""
    manager = SessionManager(
        log_dir=str(temp_log_dir),
        state_file=str(temp_state_file),
        config={
            "claude": {
                "command": "claude",
                "args": [],
                "default_model": "sonnet",
            }
        }
    )
    manager.tmux = mock_tmux
    return manager


class TestSessionLifecycle:
    """Tests for full session lifecycle."""

    @pytest.mark.asyncio
    async def test_create_session_flow(self, session_manager, mock_tmux, temp_state_file):
        """Full session creation: ID generated, tmux created, state saved."""
        # Create session
        session = await session_manager.create_session(
            working_dir="/tmp/test-workspace",
        )

        # Verify session was created
        assert session is not None
        assert session.id is not None
        assert len(session.id) == 8  # UUID hex[:8]
        assert session.name == f"claude-{session.id}"
        assert session.tmux_session == f"claude-{session.id}"
        assert session.working_dir == "/tmp/test-workspace"
        assert session.status == SessionStatus.RUNNING

        # Verify tmux was called
        mock_tmux.create_session_with_command.assert_called_once()
        call_args = mock_tmux.create_session_with_command.call_args
        assert call_args[0][0] == session.tmux_session
        assert call_args[0][1] == "/tmp/test-workspace"

        # Verify state was saved
        saved_state = json.loads(temp_state_file.read_text())
        assert len(saved_state["sessions"]) == 1
        assert saved_state["sessions"][0]["id"] == session.id

        # Verify session is in memory
        assert session.id in session_manager.sessions
        assert session_manager.get_session(session.id) == session

    @pytest.mark.asyncio
    async def test_create_session_flow_preserves_parent_ownership(self, session_manager):
        """Direct create path records parent ownership when launched from a managed session."""
        session = await session_manager.create_session(
            working_dir="/tmp/test-workspace",
            parent_session_id="parent123",
        )

        assert session is not None
        assert session.parent_session_id == "parent123"
        assert session.spawned_at is not None

    @pytest.mark.asyncio
    async def test_kill_session_flow(self, session_manager, mock_tmux, temp_state_file):
        """Full session kill: tmux killed, state updated."""
        # First create a session
        session = await session_manager.create_session(
            working_dir="/tmp/test",
        )
        session_id = session.id

        # Verify it exists
        assert session_manager.get_session(session_id) is not None

        # Kill the session
        success = session_manager.kill_session(session_id)

        # Verify success
        assert success is True

        # Verify tmux was killed
        mock_tmux.kill_session.assert_called_with(session.tmux_session)

        # Verify status updated
        assert session.status == SessionStatus.STOPPED

        # Verify state was saved (session still in dict but marked stopped)
        saved_state = json.loads(temp_state_file.read_text())
        assert len(saved_state["sessions"]) == 1
        assert saved_state["sessions"][0]["status"] == "stopped"

    @pytest.mark.asyncio
    async def test_session_recovery_on_restart(self, mock_tmux, temp_state_file, temp_log_dir):
        """Sessions restored from state file on restart."""
        # Pre-populate state file with a session
        existing_session = {
            "id": "existing1",
            "name": "claude-existing1",
            "working_dir": "/tmp/existing",
            "tmux_session": "claude-existing1",
            "log_file": "/tmp/existing.log",
            "status": "running",
            "created_at": "2024-01-15T10:00:00",
            "last_activity": "2024-01-15T11:00:00",
        }
        temp_state_file.write_text(json.dumps({"sessions": [existing_session]}))

        # Mock tmux to say session exists
        mock_tmux.session_exists.return_value = True

        # Patch TmuxController to return our mock before SessionManager is created
        with patch('src.session_manager.TmuxController', return_value=mock_tmux):
            # Create new SessionManager (simulates restart)
            manager = SessionManager(
                log_dir=str(temp_log_dir),
                state_file=str(temp_state_file),
            )

        # Verify session was restored
        assert "existing1" in manager.sessions
        restored = manager.get_session("existing1")
        assert restored is not None
        assert restored.name == "claude-existing1"
        assert restored.working_dir == "/tmp/existing"

    @pytest.mark.asyncio
    async def test_dead_session_not_recovered(self, mock_tmux, temp_state_file, temp_log_dir):
        """Sessions without tmux are not restored."""
        # Pre-populate state file
        dead_session = {
            "id": "dead123",
            "name": "claude-dead123",
            "working_dir": "/tmp/dead",
            "tmux_session": "claude-dead123",
            "log_file": "/tmp/dead.log",
            "status": "running",
            "created_at": "2024-01-15T10:00:00",
            "last_activity": "2024-01-15T11:00:00",
        }
        temp_state_file.write_text(json.dumps({"sessions": [dead_session]}))

        # Mock tmux to say session does NOT exist
        mock_tmux.session_exists.return_value = False

        # Create new SessionManager
        manager = SessionManager(
            log_dir=str(temp_log_dir),
            state_file=str(temp_state_file),
        )
        manager.tmux = mock_tmux

        # Verify dead session was NOT restored
        assert "dead123" not in manager.sessions
        assert manager.get_session("dead123") is None


class TestSpawnChildSession:
    """Tests for child session spawning."""

    @pytest.mark.asyncio
    async def test_spawn_sets_parent_relationship(self, session_manager, mock_tmux):
        """Child has parent_session_id set."""
        # Create parent session
        parent = await session_manager.create_session(
            working_dir="/tmp/parent",
        )

        # Spawn child
        child = await session_manager.spawn_child_session(
            parent_session_id=parent.id,
            prompt="Do something",
        )

        # Verify relationship
        assert child is not None
        assert child.parent_session_id == parent.id
        assert child.spawn_prompt == "Do something"
        assert child.spawned_at is not None

    @pytest.mark.asyncio
    async def test_spawn_inherits_working_dir(self, session_manager, mock_tmux):
        """Child uses parent's working_dir by default."""
        # Create parent
        parent = await session_manager.create_session(
            working_dir="/tmp/parent-dir",
        )

        # Spawn child without explicit working_dir
        child = await session_manager.spawn_child_session(
            parent_session_id=parent.id,
            prompt="Test",
        )

        # Verify child inherited working_dir
        assert child.working_dir == "/tmp/parent-dir"

    @pytest.mark.asyncio
    async def test_spawn_with_custom_working_dir(self, session_manager, mock_tmux):
        """Child can override working_dir."""
        # Create parent
        parent = await session_manager.create_session(
            working_dir="/tmp/parent",
        )

        # Spawn child with custom dir
        child = await session_manager.spawn_child_session(
            parent_session_id=parent.id,
            prompt="Test",
            working_dir="/tmp/custom",
        )

        # Verify custom dir used
        assert child.working_dir == "/tmp/custom"

    @pytest.mark.asyncio
    async def test_spawn_with_model_override(self, session_manager, mock_tmux):
        """Child can specify different model."""
        # Create parent
        parent = await session_manager.create_session(
            working_dir="/tmp/parent",
        )

        # Spawn child with model override
        child = await session_manager.spawn_child_session(
            parent_session_id=parent.id,
            prompt="Test",
            model="haiku",
        )

        # Verify tmux was called with model
        call_kwargs = mock_tmux.create_session_with_command.call_args[1]
        assert call_kwargs.get("model") == "haiku"

    @pytest.mark.asyncio
    async def test_spawn_with_friendly_name(self, session_manager, mock_tmux):
        """Child can have friendly name set."""
        # Create parent
        parent = await session_manager.create_session(
            working_dir="/tmp/parent",
        )

        # Spawn child with name
        child = await session_manager.spawn_child_session(
            parent_session_id=parent.id,
            prompt="Test",
            name="my-test-child",
        )

        # Verify friendly name set
        assert child.friendly_name == "my-test-child"

    @pytest.mark.asyncio
    async def test_spawn_nonexistent_parent_fails(self, session_manager, mock_tmux):
        """Spawning from nonexistent parent returns None."""
        child = await session_manager.spawn_child_session(
            parent_session_id="nonexistent",
            prompt="Test",
        )

        assert child is None


class TestSessionQueries:
    """Tests for session query methods."""

    @pytest.mark.asyncio
    async def test_list_sessions(self, session_manager, mock_tmux):
        """list_sessions returns active sessions."""
        # Create some sessions
        s1 = await session_manager.create_session(working_dir="/tmp/1")
        s2 = await session_manager.create_session(working_dir="/tmp/2")

        # List sessions
        sessions = session_manager.list_sessions()

        assert len(sessions) == 2
        assert s1 in sessions
        assert s2 in sessions

    @pytest.mark.asyncio
    async def test_list_sessions_excludes_stopped(self, session_manager, mock_tmux):
        """list_sessions excludes stopped sessions by default."""
        # Create and kill a session
        s1 = await session_manager.create_session(working_dir="/tmp/1")
        session_manager.kill_session(s1.id)

        # Create active session
        s2 = await session_manager.create_session(working_dir="/tmp/2")

        # List sessions
        sessions = session_manager.list_sessions()

        assert len(sessions) == 1
        assert s2 in sessions
        assert s1 not in sessions

    @pytest.mark.asyncio
    async def test_list_sessions_include_stopped(self, session_manager, mock_tmux):
        """list_sessions can include stopped sessions."""
        # Create and kill a session
        s1 = await session_manager.create_session(working_dir="/tmp/1")
        session_manager.kill_session(s1.id)

        # List with include_stopped
        sessions = session_manager.list_sessions(include_stopped=True)

        assert len(sessions) == 1
        assert s1 in sessions

    @pytest.mark.asyncio
    async def test_get_session_by_name(self, session_manager, mock_tmux):
        """get_session_by_name finds session."""
        session = await session_manager.create_session(working_dir="/tmp/test")

        found = session_manager.get_session_by_name(session.name)

        assert found is not None
        assert found.id == session.id


class TestStatePersistence:
    """Tests for state file persistence."""

    @pytest.mark.asyncio
    async def test_state_saved_on_create(self, session_manager, temp_state_file):
        """State is saved when session created."""
        session = await session_manager.create_session(working_dir="/tmp/test")

        saved = json.loads(temp_state_file.read_text())
        assert len(saved["sessions"]) == 1
        assert saved["sessions"][0]["id"] == session.id

    @pytest.mark.asyncio
    async def test_state_saved_on_kill(self, session_manager, temp_state_file):
        """State is saved when session killed."""
        session = await session_manager.create_session(working_dir="/tmp/test")
        session_manager.kill_session(session.id)

        saved = json.loads(temp_state_file.read_text())
        assert saved["sessions"][0]["status"] == "stopped"

    @pytest.mark.asyncio
    async def test_state_saved_on_status_update(self, session_manager, temp_state_file):
        """State is saved when status updated."""
        session = await session_manager.create_session(working_dir="/tmp/test")
        session_manager.update_session_status(session.id, SessionStatus.IDLE)

        saved = json.loads(temp_state_file.read_text())
        assert saved["sessions"][0]["status"] == "idle"


class TestSendInput:
    """Tests for sending input to sessions."""

    @pytest.mark.asyncio
    async def test_send_input_success(self, session_manager, mock_tmux):
        """send_input delivers message."""
        session = await session_manager.create_session(working_dir="/tmp/test")

        from src.models import DeliveryResult
        result = await session_manager.send_input(session.id, "Hello!")

        assert result == DeliveryResult.DELIVERED
        mock_tmux.send_input_async.assert_called()

    @pytest.mark.asyncio
    async def test_send_input_nonexistent_session(self, session_manager, mock_tmux):
        """send_input fails for nonexistent session."""
        from src.models import DeliveryResult
        result = await session_manager.send_input("nonexistent", "Hello!")

        assert result == DeliveryResult.FAILED

    @pytest.mark.asyncio
    async def test_send_input_bypass_queue(self, session_manager, mock_tmux):
        """send_input with bypass_queue sends directly."""
        session = await session_manager.create_session(working_dir="/tmp/test")

        from src.models import DeliveryResult
        result = await session_manager.send_input(
            session.id,
            "Direct message",
            bypass_queue=True
        )

        assert result == DeliveryResult.DELIVERED
        mock_tmux.send_input_async.assert_called_with(
            session.tmux_session,
            "Direct message"
        )


class TestOpenTerminal:
    """Tests for opening session in terminal."""

    @pytest.mark.asyncio
    async def test_open_terminal(self, session_manager, mock_tmux):
        """open_terminal calls tmux.open_in_terminal."""
        session = await session_manager.create_session(working_dir="/tmp/test")

        result = session_manager.open_terminal(session.id)

        assert result is True
        mock_tmux.open_in_terminal.assert_called_with(session.tmux_session)

    @pytest.mark.asyncio
    async def test_open_terminal_nonexistent(self, session_manager, mock_tmux):
        """open_terminal fails for nonexistent session."""
        result = session_manager.open_terminal("nonexistent")

        assert result is False
