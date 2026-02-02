"""Regression tests for #72 (sm send --wait) and #73 (sm wait)."""

import asyncio
import time
from unittest.mock import MagicMock, patch
import pytest

from src.cli.client import SessionManagerClient
from src.cli.commands import cmd_send, cmd_wait


class TestSendWithWait:
    """Test sm send --wait N functionality (issue #72)."""

    def test_send_with_wait_passes_parameter(self):
        """Test that --wait N parameter is correctly passed through the stack."""
        # Mock client
        mock_client = MagicMock(spec=SessionManagerClient)
        mock_client.session_id = "sender123"
        mock_client.send_input.return_value = (True, False)  # success, not unavailable

        # Mock resolve_session_id to return a valid session
        with patch('src.cli.commands.resolve_session_id') as mock_resolve:
            mock_resolve.return_value = ("target456", {
                "id": "target456",
                "friendly_name": "target-session",
                "name": "target-session"
            })

            # Call cmd_send with wait_seconds
            exit_code = cmd_send(
                mock_client,
                "target456",
                "test message",
                delivery_mode="sequential",
                wait_seconds=30
            )

            # Verify success
            assert exit_code == 0

            # Verify send_input was called with notify_after_seconds=30
            mock_client.send_input.assert_called_once()
            call_args = mock_client.send_input.call_args[0]
            call_kwargs = mock_client.send_input.call_args[1]
            assert call_args[0] == "target456"  # session_id as positional arg
            assert call_args[1] == "test message"  # text as positional arg
            assert call_kwargs['notify_after_seconds'] == 30
            assert call_kwargs['sender_session_id'] == "sender123"

    def test_send_with_wait_overrides_notify_after(self):
        """Test that wait_seconds takes precedence over notify_after_seconds."""
        mock_client = MagicMock(spec=SessionManagerClient)
        mock_client.session_id = "sender123"
        mock_client.send_input.return_value = (True, False)

        with patch('src.cli.commands.resolve_session_id') as mock_resolve:
            mock_resolve.return_value = ("target456", {
                "id": "target456",
                "friendly_name": "target-session"
            })

            # Call with both wait_seconds and notify_after_seconds
            exit_code = cmd_send(
                mock_client,
                "target456",
                "test message",
                delivery_mode="sequential",
                wait_seconds=30,
                notify_after_seconds=60  # Should be ignored
            )

            assert exit_code == 0

            # Verify wait_seconds took precedence
            call_kwargs = mock_client.send_input.call_args[1]
            assert call_kwargs['notify_after_seconds'] == 30

    def test_send_without_wait_uses_default(self):
        """Test that send without --wait works as before."""
        mock_client = MagicMock(spec=SessionManagerClient)
        mock_client.session_id = "sender123"
        mock_client.send_input.return_value = (True, False)

        with patch('src.cli.commands.resolve_session_id') as mock_resolve:
            mock_resolve.return_value = ("target456", {
                "id": "target456",
                "friendly_name": "target-session"
            })

            # Call without wait_seconds
            exit_code = cmd_send(
                mock_client,
                "target456",
                "test message",
                delivery_mode="sequential"
            )

            assert exit_code == 0

            # Verify notify_after_seconds is None
            call_kwargs = mock_client.send_input.call_args[1]
            assert call_kwargs['notify_after_seconds'] is None


class TestWaitCommand:
    """Test sm wait <session> <seconds> functionality (issue #73)."""

    def test_wait_returns_immediately_if_idle(self):
        """Test that wait returns immediately if session is already idle."""
        mock_client = MagicMock(spec=SessionManagerClient)
        mock_client.get_queue_status.return_value = {
            "is_idle": True,
            "pending_count": 0,
            "pending_messages": []
        }

        with patch('src.cli.commands.resolve_session_id') as mock_resolve:
            mock_resolve.return_value = ("target456", {
                "id": "target456",
                "friendly_name": "target-session"
            })

            start_time = time.time()
            exit_code = cmd_wait(mock_client, "target456", timeout_seconds=60)
            elapsed = time.time() - start_time

            # Should return immediately (exit 0)
            assert exit_code == 0
            assert elapsed < 1.0  # Should be nearly instant

    def test_wait_polls_until_idle(self):
        """Test that wait polls and detects when session becomes idle."""
        mock_client = MagicMock(spec=SessionManagerClient)

        # First 2 calls: not idle, then idle on 3rd call
        mock_client.get_queue_status.side_effect = [
            {"is_idle": False, "pending_count": 1},
            {"is_idle": False, "pending_count": 1},
            {"is_idle": True, "pending_count": 0}
        ]

        with patch('src.cli.commands.resolve_session_id') as mock_resolve:
            mock_resolve.return_value = ("target456", {
                "id": "target456",
                "friendly_name": "target-session"
            })

            with patch('time.sleep') as mock_sleep:
                exit_code = cmd_wait(mock_client, "target456", timeout_seconds=10)

                # Should return 0 (idle detected)
                assert exit_code == 0

                # Should have polled 3 times
                assert mock_client.get_queue_status.call_count == 3

                # Should have slept twice (between polls)
                assert mock_sleep.call_count == 2

    def test_wait_times_out_if_never_idle(self):
        """Test that wait returns 1 on timeout if session never goes idle."""
        mock_client = MagicMock(spec=SessionManagerClient)

        # Always return not idle
        mock_client.get_queue_status.return_value = {
            "is_idle": False,
            "pending_count": 1
        }

        with patch('src.cli.commands.resolve_session_id') as mock_resolve:
            mock_resolve.return_value = ("target456", {
                "id": "target456",
                "friendly_name": "target-session"
            })

            with patch('time.sleep') as mock_sleep:
                with patch('time.time') as mock_time:
                    # Simulate time passing
                    mock_time.side_effect = [0, 2, 4, 6, 8, 10, 12]  # Exceeds 10s timeout

                    exit_code = cmd_wait(mock_client, "target456", timeout_seconds=10)

                    # Should return 1 (timeout)
                    assert exit_code == 1

    def test_wait_handles_session_not_found(self):
        """Test that wait returns 2 if session not found."""
        mock_client = MagicMock(spec=SessionManagerClient)
        mock_client.list_sessions.return_value = []  # Not unavailable, just empty

        with patch('src.cli.commands.resolve_session_id') as mock_resolve:
            mock_resolve.return_value = (None, None)  # Session not found

            exit_code = cmd_wait(mock_client, "nonexistent", timeout_seconds=10)

            # Should return 2 (session not found)
            assert exit_code == 2

    def test_wait_handles_unavailable_server(self):
        """Test that wait returns 2 if session manager unavailable."""
        mock_client = MagicMock(spec=SessionManagerClient)
        mock_client.list_sessions.return_value = None  # Server unavailable

        with patch('src.cli.commands.resolve_session_id') as mock_resolve:
            mock_resolve.return_value = (None, None)

            exit_code = cmd_wait(mock_client, "target456", timeout_seconds=10)

            # Should return 2 (unavailable)
            assert exit_code == 2


class TestFollowupNotificationIdleCheck:
    """Test idle check in follow-up notifications (issue #72 fix)."""

    @pytest.mark.asyncio
    async def test_followup_notification_sent_regardless_of_idle(self):
        """Test that follow-up notification is sent after N seconds regardless of recipient state."""
        from src.message_queue import MessageQueueManager
        from src.models import QueuedMessage
        from datetime import datetime
        from unittest.mock import AsyncMock

        # Create mock session manager
        mock_sm = MagicMock()
        mock_sm.get_session.return_value = MagicMock(
            id="target456",
            tmux_session="test-session"
        )
        mock_sm.tmux = MagicMock()
        mock_sm.tmux.send_input_async = AsyncMock(return_value=True)
        mock_sm._save_state = MagicMock()

        # Create message queue manager
        queue_mgr = MessageQueueManager(
            session_manager=mock_sm,
            db_path="/tmp/test_queue_idle_check.db"
        )
        queue_mgr._init_db()

        # Mock is_session_idle to return False (recipient is active)
        queue_mgr.is_session_idle = MagicMock(return_value=False)

        # Create a message with notify_after_seconds
        msg = QueuedMessage(
            id="msg123",
            target_session_id="target456",
            sender_session_id="sender123",
            text="test message",
            queued_at=datetime.now(),
            notify_after_seconds=1
        )

        # Mock queue_message to track if notification was queued
        queue_mgr.queue_message = MagicMock()

        # Trigger the follow-up notification
        await queue_mgr._schedule_followup_notification(msg)

        # Wait for the notification to fire
        await asyncio.sleep(1.5)

        # Verify notification WAS queued even though recipient is not idle
        queue_mgr.queue_message.assert_called_once()

    @pytest.mark.asyncio
    async def test_followup_notification_sent_after_timeout(self):
        """Test that follow-up notification is sent after timeout with correct format."""
        from src.message_queue import MessageQueueManager
        from src.models import QueuedMessage
        from datetime import datetime
        from unittest.mock import AsyncMock

        # Create mock session manager
        mock_sm = MagicMock()
        mock_sm.get_session.return_value = MagicMock(
            id="target456",
            tmux_session="test-session"
        )
        mock_sm.tmux = MagicMock()
        mock_sm.tmux.send_input_async = AsyncMock(return_value=True)
        mock_sm._save_state = MagicMock()

        # Create message queue manager
        queue_mgr = MessageQueueManager(
            session_manager=mock_sm,
            db_path="/tmp/test_queue_idle_check_2.db"
        )
        queue_mgr._init_db()

        # Mock is_session_idle to return True (recipient is still idle)
        queue_mgr.is_session_idle = MagicMock(return_value=True)

        # Create a message with notify_after_seconds
        msg = QueuedMessage(
            id="msg123",
            target_session_id="target456",
            sender_session_id="sender123",
            text="test message",
            queued_at=datetime.now(),
            notify_after_seconds=1
        )

        # Mock queue_message to track if notification was queued
        queue_mgr.queue_message = MagicMock()

        # Trigger the follow-up notification
        await queue_mgr._schedule_followup_notification(msg)

        # Wait for the notification to fire
        await asyncio.sleep(1.5)

        # Verify notification was queued (recipient is idle)
        queue_mgr.queue_message.assert_called_once()
        call_args = queue_mgr.queue_message.call_args[1]
        assert call_args['target_session_id'] == "sender123"
        assert "Reminder" in call_args['text']


class TestIntegration:
    """Integration tests for wait features."""

    def test_send_wait_and_poll_workflow(self):
        """Test realistic workflow: send with --wait, then poll."""
        mock_client = MagicMock(spec=SessionManagerClient)
        mock_client.session_id = "sender123"
        mock_client.send_input.return_value = (True, False)
        mock_client.get_queue_status.return_value = {"is_idle": True, "pending_count": 0}

        with patch('src.cli.commands.resolve_session_id') as mock_resolve:
            mock_resolve.return_value = ("target456", {
                "id": "target456",
                "friendly_name": "target-session"
            })

            # Step 1: Send with --wait 30
            exit_code = cmd_send(
                mock_client,
                "target456",
                "do something",
                delivery_mode="sequential",
                wait_seconds=30
            )
            assert exit_code == 0

            # Verify notify_after_seconds was set
            call_kwargs = mock_client.send_input.call_args[1]
            assert call_kwargs['notify_after_seconds'] == 30

            # Step 2: Wait for completion
            exit_code = cmd_wait(mock_client, "target456", timeout_seconds=60)
            assert exit_code == 0
