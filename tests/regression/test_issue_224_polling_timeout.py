"""Regression tests for sm#224: Telegram polling timeout and health monitor."""

import asyncio
import time
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.telegram_bot import (
    _PollingTracker,
    TrackingHTTPXRequest,
    _POLLING_CHECK_INTERVAL,
    _POLLING_STALL_THRESHOLD,
    TelegramBot,
)


class TestPollingTracker:
    def test_initial_elapsed_is_small(self):
        tracker = _PollingTracker()
        assert tracker.elapsed() < 1.0

    def test_record_resets_elapsed(self):
        tracker = _PollingTracker()
        # Manually backdate the timestamp
        tracker._last_get_updates_ts = time.monotonic() - 100
        assert tracker.elapsed() >= 100
        tracker.record()
        assert tracker.elapsed() < 1.0

    def test_elapsed_increases_over_time(self):
        tracker = _PollingTracker()
        tracker._last_get_updates_ts = time.monotonic() - 10
        assert tracker.elapsed() >= 10


class TestTrackingHTTPXRequest:
    """TrackingHTTPXRequest records timestamp on getUpdates calls."""

    @pytest.mark.asyncio
    async def test_records_timestamp_on_get_updates(self):
        tracker = _PollingTracker()
        # Age the tracker
        tracker._last_get_updates_ts = time.monotonic() - 60

        req = TrackingHTTPXRequest(tracker=tracker, read_timeout=30.0)

        with patch.object(
            req.__class__.__bases__[0],
            "do_request",
            new_callable=AsyncMock,
            return_value=(200, b"{}"),
        ):
            # Call with a getUpdates URL
            await req.do_request(
                url="https://api.telegram.org/botTOKEN/getUpdates",
                method="POST",
            )

        # Timestamp should be fresh
        assert tracker.elapsed() < 2.0

    @pytest.mark.asyncio
    async def test_does_not_record_on_other_calls(self):
        tracker = _PollingTracker()
        tracker._last_get_updates_ts = time.monotonic() - 60

        req = TrackingHTTPXRequest(tracker=tracker, read_timeout=30.0)

        with patch.object(
            req.__class__.__bases__[0],
            "do_request",
            new_callable=AsyncMock,
            return_value=(200, b"{}"),
        ):
            await req.do_request(
                url="https://api.telegram.org/botTOKEN/sendMessage",
                method="POST",
            )

        # Timestamp should NOT have been refreshed
        assert tracker.elapsed() >= 59


class TestPollingHealthMonitor:
    """Health monitor restarts updater on stall."""

    async def _run_one_iteration(self, bot: TelegramBot) -> None:
        """Run the health monitor for exactly one check cycle."""
        # Set interval to 0 so the real asyncio.sleep returns immediately
        bot._polling_check_interval = 0
        task = asyncio.create_task(bot._polling_health_monitor())
        # Yield twice: once for the sleep, once for the body after sleep
        await asyncio.sleep(0)
        await asyncio.sleep(0)
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass

    @pytest.mark.asyncio
    async def test_restarts_updater_when_stalled(self):
        bot = TelegramBot(token="test-token")

        mock_updater = AsyncMock()
        mock_application = MagicMock()
        mock_application.updater = mock_updater
        bot.application = mock_application

        # Age the tracker well past the stall threshold
        bot._polling_tracker._last_get_updates_ts = (
            time.monotonic() - _POLLING_STALL_THRESHOLD - 10
        )

        await self._run_one_iteration(bot)

        mock_updater.stop.assert_called_once()
        mock_updater.start_polling.assert_called_once()

    @pytest.mark.asyncio
    async def test_does_not_restart_when_healthy(self):
        bot = TelegramBot(token="test-token")

        mock_updater = AsyncMock()
        mock_application = MagicMock()
        mock_application.updater = mock_updater
        bot.application = mock_application

        # Tracker is fresh — not stalled
        bot._polling_tracker.record()

        await self._run_one_iteration(bot)

        mock_updater.stop.assert_not_called()
        mock_updater.start_polling.assert_not_called()

    @pytest.mark.asyncio
    async def test_tracker_reset_after_restart(self):
        """After restart, tracker is refreshed so monitor doesn't re-fire immediately."""
        bot = TelegramBot(token="test-token")

        mock_updater = AsyncMock()
        mock_application = MagicMock()
        mock_application.updater = mock_updater
        bot.application = mock_application

        bot._polling_tracker._last_get_updates_ts = (
            time.monotonic() - _POLLING_STALL_THRESHOLD - 10
        )

        await self._run_one_iteration(bot)

        # Tracker should have been reset during restart
        assert bot._polling_tracker.elapsed() < 2.0


class TestBotInit:
    def test_tracker_and_task_initialized(self):
        bot = TelegramBot(token="test-token")
        assert isinstance(bot._polling_tracker, _PollingTracker)
        assert bot._health_monitor_task is None

    def test_constants_are_sane(self):
        assert _POLLING_CHECK_INTERVAL < _POLLING_STALL_THRESHOLD
        assert _POLLING_STALL_THRESHOLD < 120
