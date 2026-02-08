"""Unit tests for models - ticket #62."""

import pytest
from datetime import datetime, timedelta
from src.models import (
    Session,
    SessionStatus,
    Subagent,
    SubagentStatus,
    QueuedMessage,
    NotificationEvent,
    NotificationChannel,
    DeliveryMode,
    DeliveryResult,
    CompletionStatus,
    SessionDeliveryState,
)


class TestSession:
    """Tests for Session dataclass."""

    def test_to_dict_roundtrip(self):
        """Session.to_dict() -> Session.from_dict() preserves all fields."""
        original = Session(
            id="test123",
            name="test-session",
            working_dir="/tmp/workspace",
            tmux_session="claude-test123",
            log_file="/tmp/logs/test.log",
            status=SessionStatus.RUNNING,
            created_at=datetime(2024, 1, 15, 10, 30, 0),
            last_activity=datetime(2024, 1, 15, 11, 45, 0),
            telegram_chat_id=123456,
            telegram_thread_id=789,
            error_message="test error",
            transcript_path="/tmp/transcripts/test.jsonl",
            friendly_name="My Test Session",
            current_task="Running unit tests",
            git_remote_url="https://github.com/test/repo.git",
            parent_session_id="parent123",
            spawn_prompt="Test spawn prompt",
            completion_status=CompletionStatus.COMPLETED,
            completion_message="Task completed",
            spawned_at=datetime(2024, 1, 15, 10, 0, 0),
            completed_at=datetime(2024, 1, 15, 12, 0, 0),
            tokens_used=5000,
            tools_used={"Read": 10, "Write": 5},
            last_tool_call=datetime(2024, 1, 15, 11, 30, 0),
            touched_repos={"/repo1", "/repo2"},
            worktrees=["/worktree1", "/worktree2"],
        )

        # Convert to dict and back
        as_dict = original.to_dict()
        restored = Session.from_dict(as_dict)

        # Verify all fields
        assert restored.id == original.id
        assert restored.name == original.name
        assert restored.working_dir == original.working_dir
        assert restored.tmux_session == original.tmux_session
        assert restored.log_file == original.log_file
        assert restored.status == original.status
        assert restored.created_at == original.created_at
        assert restored.last_activity == original.last_activity
        assert restored.telegram_chat_id == original.telegram_chat_id
        assert restored.telegram_thread_id == original.telegram_thread_id
        assert restored.error_message == original.error_message
        assert restored.transcript_path == original.transcript_path
        assert restored.friendly_name == original.friendly_name
        assert restored.current_task == original.current_task
        assert restored.git_remote_url == original.git_remote_url
        assert restored.parent_session_id == original.parent_session_id
        assert restored.spawn_prompt == original.spawn_prompt
        assert restored.completion_status == original.completion_status
        assert restored.completion_message == original.completion_message
        assert restored.spawned_at == original.spawned_at
        assert restored.completed_at == original.completed_at
        assert restored.tokens_used == original.tokens_used
        assert restored.tools_used == original.tools_used
        assert restored.last_tool_call == original.last_tool_call
        assert restored.touched_repos == original.touched_repos
        assert restored.worktrees == original.worktrees

    def test_default_values(self):
        """New Session has correct defaults."""
        session = Session()

        assert session.id is not None
        assert len(session.id) == 8  # UUID hex[:8]
        assert session.name == f"claude-{session.id}"
        assert session.working_dir == ""
        assert session.tmux_session == f"claude-{session.id}"
        assert session.log_file == ""
        assert session.status == SessionStatus.RUNNING
        assert isinstance(session.created_at, datetime)
        assert isinstance(session.last_activity, datetime)
        assert session.telegram_chat_id is None
        assert session.telegram_thread_id is None
        assert session.error_message is None
        assert session.friendly_name is None
        assert session.current_task is None
        assert session.subagents == []
        assert session.parent_session_id is None
        assert session.tokens_used == 0
        assert session.tools_used == {}
        assert session.touched_repos == set()
        assert session.worktrees == []

    def test_tmux_session_auto_generated(self):
        """tmux_session defaults to claude-{id}."""
        session = Session(id="abc12345")
        assert session.tmux_session == "claude-abc12345"

        # Custom name doesn't affect tmux_session
        session2 = Session(id="xyz99999", name="custom-name")
        assert session2.tmux_session == "claude-xyz99999"

    def test_name_auto_generated(self):
        """name defaults to claude-{id} when not provided."""
        session = Session(id="test1234")
        assert session.name == "claude-test1234"

    def test_name_preserved_when_provided(self):
        """Provided name is preserved."""
        session = Session(id="test1234", name="my-custom-session")
        assert session.name == "my-custom-session"

    def test_session_status_enum_values(self):
        """All status values serialize correctly."""
        for status in SessionStatus:
            session = Session(status=status)
            as_dict = session.to_dict()
            assert as_dict["status"] == status.value

            restored = Session.from_dict(as_dict)
            assert restored.status == status

    def test_backward_compatibility_telegram_topic_id(self):
        """from_dict handles legacy telegram_topic_id field."""
        data = {
            "id": "test123",
            "name": "test",
            "working_dir": "/tmp",
            "tmux_session": "claude-test123",
            "log_file": "/tmp/test.log",
            "status": "running",
            "created_at": "2024-01-15T10:00:00",
            "last_activity": "2024-01-15T11:00:00",
            "telegram_chat_id": 123456,
            "telegram_topic_id": 789,  # Legacy field name
        }
        session = Session.from_dict(data)
        assert session.telegram_thread_id == 789


class TestSubagent:
    """Tests for Subagent dataclass."""

    def test_to_dict_roundtrip(self):
        """Subagent serialization roundtrip."""
        original = Subagent(
            agent_id="agent123",
            agent_type="engineer",
            parent_session_id="parent456",
            transcript_path="/tmp/transcripts/agent.jsonl",
            started_at=datetime(2024, 1, 15, 10, 0, 0),
            stopped_at=datetime(2024, 1, 15, 11, 0, 0),
            status=SubagentStatus.COMPLETED,
            summary="Task completed successfully",
        )

        as_dict = original.to_dict()
        restored = Subagent.from_dict(as_dict)

        assert restored.agent_id == original.agent_id
        assert restored.agent_type == original.agent_type
        assert restored.parent_session_id == original.parent_session_id
        assert restored.transcript_path == original.transcript_path
        assert restored.started_at == original.started_at
        assert restored.stopped_at == original.stopped_at
        assert restored.status == original.status
        assert restored.summary == original.summary

    def test_subagent_status_enum(self):
        """SubagentStatus values are correct."""
        assert SubagentStatus.RUNNING.value == "running"
        assert SubagentStatus.COMPLETED.value == "completed"
        assert SubagentStatus.ERROR.value == "error"

    def test_subagent_defaults(self):
        """Subagent has correct defaults."""
        subagent = Subagent(
            agent_id="agent123",
            agent_type="explorer",
            parent_session_id="parent456",
        )
        assert subagent.transcript_path is None
        assert isinstance(subagent.started_at, datetime)
        assert subagent.stopped_at is None
        assert subagent.status == SubagentStatus.RUNNING
        assert subagent.summary is None

    def test_subagent_none_stopped_at_serializes(self):
        """Subagent with None stopped_at serializes correctly."""
        subagent = Subagent(
            agent_id="agent123",
            agent_type="explorer",
            parent_session_id="parent456",
        )
        as_dict = subagent.to_dict()
        assert as_dict["stopped_at"] is None

        restored = Subagent.from_dict(as_dict)
        assert restored.stopped_at is None


class TestQueuedMessage:
    """Tests for QueuedMessage dataclass."""

    def test_to_dict_roundtrip(self):
        """QueuedMessage serialization roundtrip."""
        now = datetime.now()
        timeout = now + timedelta(minutes=30)

        original = QueuedMessage(
            id="msg123",
            target_session_id="target456",
            sender_session_id="sender789",
            sender_name="Test Sender",
            text="Hello, world!",
            delivery_mode="important",
            queued_at=now,
            timeout_at=timeout,
            notify_on_delivery=True,
            notify_after_seconds=60,
            delivered_at=None,
        )

        as_dict = original.to_dict()
        assert as_dict["id"] == "msg123"
        assert as_dict["target_session_id"] == "target456"
        assert as_dict["sender_session_id"] == "sender789"
        assert as_dict["sender_name"] == "Test Sender"
        assert as_dict["text"] == "Hello, world!"
        assert as_dict["delivery_mode"] == "important"
        assert as_dict["queued_at"] == now.isoformat()
        assert as_dict["timeout_at"] == timeout.isoformat()
        assert as_dict["notify_on_delivery"] is True
        assert as_dict["notify_after_seconds"] == 60
        assert as_dict["delivered_at"] is None

    def test_queued_message_defaults(self):
        """QueuedMessage has correct defaults."""
        msg = QueuedMessage(
            target_session_id="target123",
            text="Test message",
        )
        assert msg.id is not None
        assert len(msg.id) == 32  # Full UUID hex
        assert msg.sender_session_id is None
        assert msg.sender_name is None
        assert msg.delivery_mode == "sequential"
        assert isinstance(msg.queued_at, datetime)
        assert msg.timeout_at is None
        assert msg.notify_on_delivery is False
        assert msg.notify_after_seconds is None
        assert msg.delivered_at is None

    def test_is_expired_not_implemented(self):
        """Expiration is handled by MessageQueueManager, not model."""
        # QueuedMessage doesn't have is_expired method -
        # expiration is checked in MessageQueueManager.get_pending_messages
        msg = QueuedMessage(
            target_session_id="target123",
            text="Test",
            timeout_at=datetime.now() - timedelta(hours=1),  # Past timeout
        )
        # Just verify the field is set
        assert msg.timeout_at < datetime.now()


class TestNotificationEvent:
    """Tests for NotificationEvent dataclass."""

    def test_event_types(self):
        """NotificationEvent can hold various event types."""
        event_types = [
            "permission_prompt",
            "idle",
            "error",
            "complete",
            "response",
            "sm_send",
        ]

        for event_type in event_types:
            event = NotificationEvent(
                session_id="session123",
                event_type=event_type,
                message="Test message",
            )
            assert event.event_type == event_type

    def test_notification_event_defaults(self):
        """NotificationEvent has correct defaults."""
        event = NotificationEvent(
            session_id="session123",
            event_type="test",
            message="Test message",
        )
        assert event.context == ""
        assert event.urgent is False
        assert event.channel is None

    def test_notification_event_with_channel(self):
        """NotificationEvent accepts channel parameter."""
        event = NotificationEvent(
            session_id="session123",
            event_type="test",
            message="Test message",
            channel=NotificationChannel.TELEGRAM,
        )
        assert event.channel == NotificationChannel.TELEGRAM


class TestEnums:
    """Tests for enum values."""

    def test_session_status_values(self):
        """SessionStatus has all expected values."""
        expected = ["running", "idle", "stopped"]
        actual = [s.value for s in SessionStatus]
        assert set(actual) == set(expected)

    def test_delivery_mode_values(self):
        """DeliveryMode has all expected values."""
        assert DeliveryMode.SEQUENTIAL.value == "sequential"
        assert DeliveryMode.IMPORTANT.value == "important"
        assert DeliveryMode.URGENT.value == "urgent"

    def test_delivery_result_values(self):
        """DeliveryResult has all expected values."""
        assert DeliveryResult.DELIVERED.value == "delivered"
        assert DeliveryResult.QUEUED.value == "queued"
        assert DeliveryResult.FAILED.value == "failed"

    def test_completion_status_values(self):
        """CompletionStatus has all expected values."""
        assert CompletionStatus.COMPLETED.value == "completed"
        assert CompletionStatus.ERROR.value == "error"
        assert CompletionStatus.ABANDONED.value == "abandoned"
        assert CompletionStatus.KILLED.value == "killed"

    def test_notification_channel_values(self):
        """NotificationChannel has all expected values."""
        assert NotificationChannel.TELEGRAM.value == "telegram"
        assert NotificationChannel.EMAIL.value == "email"


class TestSessionDeliveryState:
    """Tests for SessionDeliveryState dataclass."""

    def test_default_values(self):
        """SessionDeliveryState has correct defaults."""
        state = SessionDeliveryState(session_id="session123")
        assert state.session_id == "session123"
        assert state.is_idle is False
        assert state.last_idle_at is None
        assert state.saved_user_input is None
        assert state.pending_user_input is None
        assert state.pending_input_first_seen is None
