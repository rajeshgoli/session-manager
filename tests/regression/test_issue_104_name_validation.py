"""Regression test for issue #104: sm name should validate names for shell compatibility.

Bug: sm name accepted any string including spaces, causing shell parsing issues
when those names were used in other commands.

Fix: Added validate_friendly_name() to enforce shell-safe naming rules.
"""

import pytest
from unittest.mock import Mock

from src.cli.commands import cmd_name, validate_friendly_name


class TestValidateFriendlyName:
    """Test the validate_friendly_name() function."""

    def test_valid_name_alphanumeric(self):
        """Valid name with only alphanumeric characters."""
        valid, error = validate_friendly_name("architect102")
        assert valid is True
        assert error == ""

    def test_valid_name_with_dash(self):
        """Valid name with dashes."""
        valid, error = validate_friendly_name("architect-102")
        assert valid is True
        assert error == ""

    def test_valid_name_with_underscore(self):
        """Valid name with underscores."""
        valid, error = validate_friendly_name("architect_102")
        assert valid is True
        assert error == ""

    def test_valid_name_mixed(self):
        """Valid name with alphanumeric, dashes, and underscores."""
        valid, error = validate_friendly_name("test-agent_v2")
        assert valid is True
        assert error == ""

    def test_invalid_name_with_spaces(self):
        """Invalid name with spaces (the original bug)."""
        valid, error = validate_friendly_name("architect 102")
        assert valid is False
        assert "no spaces" in error.lower()

    def test_invalid_name_empty(self):
        """Invalid name: empty string."""
        valid, error = validate_friendly_name("")
        assert valid is False
        assert "empty" in error.lower()

    def test_invalid_name_too_long(self):
        """Invalid name: exceeds 32 character limit."""
        long_name = "a" * 33
        valid, error = validate_friendly_name(long_name)
        assert valid is False
        assert "too long" in error.lower()
        assert "32" in error

    def test_valid_name_exactly_32_chars(self):
        """Valid name: exactly 32 characters."""
        name = "a" * 32
        valid, error = validate_friendly_name(name)
        assert valid is True
        assert error == ""

    def test_invalid_name_with_special_chars(self):
        """Invalid name with shell metacharacters."""
        special_chars = ["$var", "test|grep", "foo;bar", "test&", "a*b", "test?"]
        for name in special_chars:
            valid, error = validate_friendly_name(name)
            assert valid is False, f"Should reject: {name}"
            assert "alphanumeric" in error.lower()

    def test_invalid_name_with_dot(self):
        """Invalid name with dots."""
        valid, error = validate_friendly_name("test.agent")
        assert valid is False
        assert "alphanumeric" in error.lower()

    def test_invalid_name_with_slash(self):
        """Invalid name with slashes."""
        valid, error = validate_friendly_name("test/agent")
        assert valid is False
        assert "alphanumeric" in error.lower()


class TestCmdNameValidation:
    """Test that cmd_name() enforces validation."""

    def test_cmd_name_rejects_spaces_when_renaming_self(self):
        """cmd_name should reject names with spaces when renaming self."""
        mock_client = Mock()

        # Call cmd_name with a name containing spaces
        result = cmd_name(mock_client, "session123", "architect 102")

        # Should return error code 1
        assert result == 1

        # Should NOT call update_friendly_name
        mock_client.update_friendly_name.assert_not_called()

    def test_cmd_name_accepts_valid_name_when_renaming_self(self):
        """cmd_name should accept valid names when renaming self."""
        mock_client = Mock()
        mock_client.update_friendly_name.return_value = (True, False)

        # Call cmd_name with a valid name
        result = cmd_name(mock_client, "session123", "architect-102")

        # Should succeed
        assert result == 0

        # Should call update_friendly_name
        mock_client.update_friendly_name.assert_called_once_with("session123", "architect-102")

    def test_cmd_name_rejects_empty_name_when_renaming_self(self):
        """cmd_name should reject empty names when renaming self."""
        mock_client = Mock()

        # Call cmd_name with empty name
        result = cmd_name(mock_client, "session123", "")

        # Should return error code 1
        assert result == 1

        # Should NOT call update_friendly_name
        mock_client.update_friendly_name.assert_not_called()

    def test_cmd_name_rejects_too_long_name_when_renaming_self(self):
        """cmd_name should reject names longer than 32 chars when renaming self."""
        mock_client = Mock()
        long_name = "a" * 33

        # Call cmd_name with too long name
        result = cmd_name(mock_client, "session123", long_name)

        # Should return error code 1
        assert result == 1

        # Should NOT call update_friendly_name
        mock_client.update_friendly_name.assert_not_called()

    def test_cmd_name_rejects_spaces_when_renaming_child(self):
        """cmd_name should reject names with spaces when renaming child."""
        mock_client = Mock()

        # Mock session lookup
        mock_client.get_session.return_value = None
        mock_client.list_sessions.return_value = [
            {
                "id": "child123",
                "friendly_name": "child",
                "parent_session_id": "session123",
            }
        ]

        # Call cmd_name to rename child with name containing spaces
        result = cmd_name(mock_client, "session123", "child123", "architect 102")

        # Should return error code 1 (validation failure)
        assert result == 1

        # Should NOT call update_friendly_name
        mock_client.update_friendly_name.assert_not_called()

    def test_cmd_name_accepts_valid_name_when_renaming_child(self):
        """cmd_name should accept valid names when renaming child."""
        mock_client = Mock()

        # Mock session lookup - resolve_session_id tries get_session first, then list_sessions
        child_session = {
            "id": "child123",
            "friendly_name": "child",
            "parent_session_id": "session123",
        }

        # First call: resolve_session_id calls get_session("child123") -> returns the session
        mock_client.get_session.return_value = child_session
        mock_client.update_friendly_name.return_value = (True, False)

        # Call cmd_name to rename child with valid name
        result = cmd_name(mock_client, "session123", "child123", "architect-102")

        # Should succeed
        assert result == 0

        # Should call update_friendly_name
        mock_client.update_friendly_name.assert_called_once_with("child123", "architect-102")

    def test_cmd_name_rejects_special_chars(self):
        """cmd_name should reject names with shell metacharacters."""
        mock_client = Mock()

        # Test various shell metacharacters
        invalid_names = ["test$var", "foo|bar", "test;cmd", "test&bg"]

        for invalid_name in invalid_names:
            mock_client.reset_mock()

            result = cmd_name(mock_client, "session123", invalid_name)

            assert result == 1, f"Should reject: {invalid_name}"
            mock_client.update_friendly_name.assert_not_called()
