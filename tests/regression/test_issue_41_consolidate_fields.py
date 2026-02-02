"""
Regression tests for issue #41: Consolidate overlapping data model fields

Tests verify:
1. CompletionStatus enum works correctly
2. telegram_thread_id consolidation works
3. Backward compatibility with old field names in from_dict()
"""

import pytest
from datetime import datetime

from src.models import Session, CompletionStatus, SessionStatus


class TestCompletionStatusEnum:
    """Test CompletionStatus enum functionality."""

    def test_completion_status_enum_values(self):
        """Test that CompletionStatus enum has expected values."""
        assert CompletionStatus.COMPLETED.value == "completed"
        assert CompletionStatus.ERROR.value == "error"
        assert CompletionStatus.ABANDONED.value == "abandoned"
        assert CompletionStatus.KILLED.value == "killed"

    def test_session_with_completion_status_enum(self):
        """Test that Session can use CompletionStatus enum."""
        session = Session(
            working_dir="/tmp/test",
            parent_session_id="parent123",
            completion_status=CompletionStatus.COMPLETED,
        )

        assert session.completion_status == CompletionStatus.COMPLETED
        assert session.completion_status.value == "completed"

    def test_session_completion_status_serialization(self):
        """Test that completion_status enum is serialized to string."""
        session = Session(
            working_dir="/tmp/test",
            completion_status=CompletionStatus.ERROR,
        )

        data = session.to_dict()
        assert data["completion_status"] == "error"
        assert isinstance(data["completion_status"], str)

    def test_session_completion_status_none_serialization(self):
        """Test that None completion_status is serialized correctly."""
        session = Session(working_dir="/tmp/test")

        data = session.to_dict()
        assert data["completion_status"] is None


class TestTelegramThreadIdConsolidation:
    """Test telegram_thread_id consolidation."""

    def test_session_with_telegram_thread_id(self):
        """Test that Session can use telegram_thread_id."""
        session = Session(
            working_dir="/tmp/test",
            telegram_chat_id=123456,
            telegram_thread_id=789,
        )

        assert session.telegram_thread_id == 789
        assert session.telegram_chat_id == 123456

    def test_telegram_thread_id_serialization(self):
        """Test that telegram_thread_id is serialized correctly."""
        session = Session(
            working_dir="/tmp/test",
            telegram_thread_id=999,
        )

        data = session.to_dict()
        assert data["telegram_thread_id"] == 999
        # Old fields should not be in dict
        assert "telegram_root_msg_id" not in data
        assert "telegram_topic_id" not in data

    def test_telegram_thread_id_none_serialization(self):
        """Test that None telegram_thread_id is serialized correctly."""
        session = Session(working_dir="/tmp/test")

        data = session.to_dict()
        assert data["telegram_thread_id"] is None


class TestBackwardCompatibility:
    """Test backward compatibility with old field names."""

    def test_from_dict_with_telegram_topic_id(self):
        """Test that from_dict can read old telegram_topic_id field."""
        data = {
            "id": "test123",
            "name": "test-session",
            "working_dir": "/tmp/test",
            "tmux_session": "claude-test123",
            "log_file": "/tmp/test.log",
            "status": "running",
            "created_at": datetime.now().isoformat(),
            "last_activity": datetime.now().isoformat(),
            "telegram_topic_id": 12345,  # Old field name
        }

        session = Session.from_dict(data)
        # Should be converted to telegram_thread_id
        assert session.telegram_thread_id == 12345

    def test_from_dict_with_telegram_root_msg_id(self):
        """Test that from_dict can read old telegram_root_msg_id field."""
        data = {
            "id": "test456",
            "name": "test-session",
            "working_dir": "/tmp/test",
            "tmux_session": "claude-test456",
            "log_file": "/tmp/test.log",
            "status": "running",
            "created_at": datetime.now().isoformat(),
            "last_activity": datetime.now().isoformat(),
            "telegram_root_msg_id": 67890,  # Old field name
        }

        session = Session.from_dict(data)
        # Should be converted to telegram_thread_id
        assert session.telegram_thread_id == 67890

    def test_from_dict_prefers_telegram_topic_id_over_root_msg_id(self):
        """Test that telegram_topic_id takes precedence over telegram_root_msg_id."""
        data = {
            "id": "test789",
            "name": "test-session",
            "working_dir": "/tmp/test",
            "tmux_session": "claude-test789",
            "log_file": "/tmp/test.log",
            "status": "running",
            "created_at": datetime.now().isoformat(),
            "last_activity": datetime.now().isoformat(),
            "telegram_topic_id": 11111,  # Should take precedence
            "telegram_root_msg_id": 22222,
        }

        session = Session.from_dict(data)
        # Should prefer telegram_topic_id
        assert session.telegram_thread_id == 11111

    def test_from_dict_with_new_telegram_thread_id(self):
        """Test that from_dict works with new telegram_thread_id field."""
        data = {
            "id": "test999",
            "name": "test-session",
            "working_dir": "/tmp/test",
            "tmux_session": "claude-test999",
            "log_file": "/tmp/test.log",
            "status": "running",
            "created_at": datetime.now().isoformat(),
            "last_activity": datetime.now().isoformat(),
            "telegram_thread_id": 33333,  # New field name
        }

        session = Session.from_dict(data)
        assert session.telegram_thread_id == 33333

    def test_from_dict_with_completion_status_string(self):
        """Test that from_dict converts completion_status string to enum."""
        data = {
            "id": "child123",
            "name": "child-session",
            "working_dir": "/tmp/test",
            "tmux_session": "claude-child123",
            "log_file": "/tmp/test.log",
            "status": "running",
            "created_at": datetime.now().isoformat(),
            "last_activity": datetime.now().isoformat(),
            "completion_status": "completed",  # String value
        }

        session = Session.from_dict(data)
        # Should be converted to enum
        assert session.completion_status == CompletionStatus.COMPLETED
        assert isinstance(session.completion_status, CompletionStatus)

    def test_from_dict_with_all_completion_status_values(self):
        """Test that all completion_status string values convert correctly."""
        statuses = ["completed", "error", "abandoned", "killed"]
        expected_enums = [
            CompletionStatus.COMPLETED,
            CompletionStatus.ERROR,
            CompletionStatus.ABANDONED,
            CompletionStatus.KILLED,
        ]

        for status_str, expected_enum in zip(statuses, expected_enums):
            data = {
                "id": f"test-{status_str}",
                "name": "test-session",
                "working_dir": "/tmp/test",
                "tmux_session": f"claude-test-{status_str}",
                "log_file": "/tmp/test.log",
                "status": "running",
                "created_at": datetime.now().isoformat(),
                "last_activity": datetime.now().isoformat(),
                "completion_status": status_str,
            }

            session = Session.from_dict(data)
            assert session.completion_status == expected_enum


class TestFieldClarityComments:
    """Test that field purposes are clear (name vs friendly_name)."""

    def test_name_is_always_set(self):
        """Test that name is always set (auto-generated if not provided)."""
        session = Session(working_dir="/tmp/test")
        assert session.name != ""
        assert session.name.startswith("claude-")

    def test_friendly_name_is_optional(self):
        """Test that friendly_name is optional."""
        session = Session(working_dir="/tmp/test")
        assert session.friendly_name is None

    def test_friendly_name_for_display(self):
        """Test that friendly_name is used for display purposes."""
        session = Session(
            working_dir="/tmp/test",
            friendly_name="My Custom Name",
        )

        assert session.friendly_name == "My Custom Name"
        assert session.name != "My Custom Name"  # name is still auto-generated

    def test_name_and_friendly_name_are_independent(self):
        """Test that name and friendly_name are independent fields."""
        session = Session(
            working_dir="/tmp/test",
            name="explicit-name",
            friendly_name="Friendly Name",
        )

        assert session.name == "explicit-name"
        assert session.friendly_name == "Friendly Name"


class TestFullRoundTrip:
    """Test full serialization/deserialization round trip."""

    def test_round_trip_with_all_new_fields(self):
        """Test round trip with all new consolidated fields."""
        original = Session(
            working_dir="/tmp/test",
            telegram_thread_id=12345,
            completion_status=CompletionStatus.COMPLETED,
            friendly_name="Test Session",
        )

        # Serialize
        data = original.to_dict()

        # Deserialize
        restored = Session.from_dict(data)

        assert restored.telegram_thread_id == original.telegram_thread_id
        assert restored.completion_status == original.completion_status
        assert restored.friendly_name == original.friendly_name
        assert restored.name == original.name

    def test_round_trip_from_old_format(self):
        """Test that old format can be loaded and resaved in new format."""
        # Old format dict
        old_data = {
            "id": "old123",
            "name": "old-session",
            "working_dir": "/tmp/test",
            "tmux_session": "claude-old123",
            "log_file": "/tmp/test.log",
            "status": "running",
            "created_at": datetime.now().isoformat(),
            "last_activity": datetime.now().isoformat(),
            "telegram_topic_id": 99999,  # Old field
            "completion_status": "error",  # Old string format
        }

        # Load from old format
        session = Session.from_dict(old_data)

        # Resave in new format
        new_data = session.to_dict()

        # Verify new format
        assert new_data["telegram_thread_id"] == 99999
        assert new_data["completion_status"] == "error"
        assert "telegram_topic_id" not in new_data
        assert "telegram_root_msg_id" not in new_data
