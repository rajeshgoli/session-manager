"""Regression tests for sm#224: Telegram polling timeout and health monitor."""

import asyncio
import time
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.telegram_bot import (
    _PollingTracker,
    _TrackingHTTPXRequest,
    _POLLING_CHECK_INTERVAL,
    _POLLING_STALL_THRESHOLD,
    _POLLING_READ_TIMEOUT,
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
    """_TrackingHTTPXRequest records timestamp on getUpdates calls."""

    @pytest.mark.asyncio
    async def test_records_timestamp_on_get_updates(self):
        tracker = _PollingTracker()
        # Age the tracker
        tracker._last_get_updates_ts = time.monotonic() - 60

        req = _TrackingHTTPXRequest(tracker=tracker, read_timeout=_POLLING_READ_TIMEOUT)

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

        req = _TrackingHTTPXRequest(tracker=tracker, read_timeout=_POLLING_READ_TIMEOUT)

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


class TestPollingHealthMonitorRestartFailure:
    """Health monitor handles restart errors gracefully."""

    @pytest.mark.asyncio
    async def test_loop_continues_after_stop_failure(self):
        """Exception during updater.stop() is logged; monitor keeps running."""
        bot = TelegramBot(token="test-token")
        bot._polling_check_interval = 0

        mock_updater = AsyncMock()
        mock_updater.stop.side_effect = RuntimeError("network error")
        mock_application = MagicMock()
        mock_application.updater = mock_updater
        bot.application = mock_application

        # Age tracker past stall threshold
        bot._polling_tracker._last_get_updates_ts = (
            time.monotonic() - _POLLING_STALL_THRESHOLD - 10
        )

        task = asyncio.create_task(bot._polling_health_monitor())
        await asyncio.sleep(0)
        await asyncio.sleep(0)
        # Task must still be running (not raised or cancelled)
        assert not task.done()
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass

        # stop() was called; start_polling() was NOT (exception aborted restart)
        mock_updater.stop.assert_called_once()
        mock_updater.start_polling.assert_not_called()

    @pytest.mark.asyncio
    async def test_tracker_not_reset_when_stop_fails(self):
        """If stop() raises, tracker.record() is never called — timestamp stays stale."""
        bot = TelegramBot(token="test-token")
        bot._polling_check_interval = 0

        mock_updater = AsyncMock()
        mock_updater.stop.side_effect = RuntimeError("network error")
        mock_application = MagicMock()
        mock_application.updater = mock_updater
        bot.application = mock_application

        stale_ts = time.monotonic() - _POLLING_STALL_THRESHOLD - 10
        bot._polling_tracker._last_get_updates_ts = stale_ts

        task = asyncio.create_task(bot._polling_health_monitor())
        await asyncio.sleep(0)
        await asyncio.sleep(0)
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass

        # Tracker was NOT reset — elapsed should still be large
        assert bot._polling_tracker.elapsed() >= _POLLING_STALL_THRESHOLD


class TestBotStopLifecycle:
    """stop() cancels the health monitor task and clears the reference."""

    @pytest.mark.asyncio
    async def test_stop_cancels_health_monitor_task(self):
        bot = TelegramBot(token="test-token")
        bot._polling_check_interval = 3600  # won't fire during test

        # Simulate a running health monitor task
        bot._health_monitor_task = asyncio.create_task(
            bot._polling_health_monitor()
        )
        task_ref = bot._health_monitor_task

        mock_application = AsyncMock()
        bot.application = mock_application

        await bot.stop()

        # Task must have been cancelled
        assert task_ref.cancelled()
        # Reference must be cleared
        assert bot._health_monitor_task is None
        # Application teardown must have been called
        mock_application.updater.stop.assert_called_once()
        mock_application.stop.assert_called_once()
        mock_application.shutdown.assert_called_once()

    @pytest.mark.asyncio
    async def test_stop_without_task_does_not_raise(self):
        """stop() is safe when no health monitor task was ever started."""
        bot = TelegramBot(token="test-token")
        assert bot._health_monitor_task is None

        mock_application = AsyncMock()
        bot.application = mock_application

        # Should not raise
        await bot.stop()
        assert bot._health_monitor_task is None


class TestBotInit:
    def test_tracker_and_task_initialized(self):
        bot = TelegramBot(token="test-token")
        assert isinstance(bot._polling_tracker, _PollingTracker)
        assert bot._health_monitor_task is None

    def test_constants_are_sane(self):
        assert _POLLING_CHECK_INTERVAL < _POLLING_STALL_THRESHOLD
        assert _POLLING_STALL_THRESHOLD < 120
        # read_timeout + Telegram hold (10s) must be < stall threshold
        assert _POLLING_READ_TIMEOUT + 10 < _POLLING_STALL_THRESHOLD
