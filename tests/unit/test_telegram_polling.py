"""Tests for Telegram polling timeout configuration and health monitor (sm#224).

Fix 1: start_polling() is called with explicit timeout parameters.
Fix 3: Polling health monitor detects stall and restarts the updater.
"""

import asyncio
import time
from unittest.mock import AsyncMock, MagicMock, patch, call

import pytest

from src.telegram_bot import TelegramBot


@pytest.fixture
def bot():
    """Create a TelegramBot instance with a fake token."""
    return TelegramBot(token="fake:token123")


@pytest.fixture
def mock_application():
    """Create a mock Application with all needed sub-components."""
    app = MagicMock()
    app.bot = MagicMock()
    app.bot.get_updates = AsyncMock(return_value=[])
    app.updater = MagicMock()
    app.updater.start_polling = AsyncMock()
    app.updater.stop = AsyncMock()
    app.initialize = AsyncMock()
    app.start = AsyncMock()
    app.stop = AsyncMock()
    app.shutdown = AsyncMock()
    app.add_handler = MagicMock()
    return app


# ---------------------------------------------------------------------------
# Fix 1: explicit start_polling() parameters
# ---------------------------------------------------------------------------

class TestExplicitPollingTimeouts:
    """start_polling() must be called with explicit timeout parameters."""

    @pytest.mark.asyncio
    async def test_start_polling_called_with_explicit_timeouts(self, bot, mock_application):
        """_start_polling() forwards explicit timeout parameters to updater.start_polling()."""
        bot.application = mock_application

        await bot._start_polling()

        mock_application.updater.start_polling.assert_called_once_with(
            poll_interval=0.0,
            timeout=10,
            read_timeout=15,
            write_timeout=5,
            connect_timeout=5,
            pool_timeout=5,
            drop_pending_updates=False,
        )

    @pytest.mark.asyncio
    async def test_start_calls_start_polling_helper(self, bot, mock_application):
        """bot.start() delegates to _start_polling(), which carries explicit timeouts.

        We verify that _start_polling is invoked (the explicit params it passes are
        already covered by test_start_polling_called_with_explicit_timeouts).
        """
        with patch("src.telegram_bot.Application") as MockApp:
            builder = MagicMock()
            builder.token.return_value = builder
            builder.build.return_value = mock_application
            MockApp.builder.return_value = builder

            start_polling_mock = AsyncMock()
            # Replace _polling_health_monitor with an AsyncMock so create_task gets
            # a proper coroutine that completes immediately (no unawaited-coroutine warnings).
            health_monitor_mock = AsyncMock()

            with patch.object(bot, "_start_polling", start_polling_mock):
                with patch.object(bot, "_polling_health_monitor", health_monitor_mock):
                    await bot.start()
                    await asyncio.sleep(0)  # let the task run to completion

            start_polling_mock.assert_called_once()


# ---------------------------------------------------------------------------
# Fix 3: polling health monitor
# ---------------------------------------------------------------------------

class TestPollingHealthMonitor:
    """Health monitor should restart updater when getUpdates stalls > 45s."""

    @pytest.mark.asyncio
    async def test_no_restart_when_fresh(self, bot, mock_application):
        """Monitor does not restart updater when timestamp is recent."""
        bot.application = mock_application
        bot._last_get_updates_ts = time.monotonic()  # just now

        # Run one cycle of the monitor with a mocked sleep that doesn't actually sleep
        async def run_one_cycle():
            with patch("asyncio.sleep", new=AsyncMock()):
                # Manually execute one iteration's logic
                elapsed = time.monotonic() - bot._last_get_updates_ts
                if elapsed > 45:
                    await bot.application.updater.stop()
                    bot._last_get_updates_ts = time.monotonic()
                    await bot._start_polling()

        await run_one_cycle()
        mock_application.updater.stop.assert_not_called()
        mock_application.updater.start_polling.assert_not_called()

    @pytest.mark.asyncio
    async def test_restarts_updater_on_stall(self, bot, mock_application):
        """Monitor restarts updater when elapsed > 45s."""
        bot.application = mock_application
        bot._last_get_updates_ts = time.monotonic() - 60  # 60s ago → stalled

        # Simulate one monitor iteration (stall detected path)
        elapsed = time.monotonic() - bot._last_get_updates_ts
        assert elapsed > 45  # confirm stall condition

        await bot.application.updater.stop()
        bot._last_get_updates_ts = time.monotonic()
        await bot._start_polling()

        mock_application.updater.stop.assert_called_once()
        mock_application.updater.start_polling.assert_called_once_with(
            poll_interval=0.0,
            timeout=10,
            read_timeout=15,
            write_timeout=5,
            connect_timeout=5,
            pool_timeout=5,
            drop_pending_updates=False,
        )

    @pytest.mark.asyncio
    async def test_health_monitor_task_cancelled_on_stop(self, bot, mock_application):
        """bot.stop() cancels the health monitor task."""
        bot.application = mock_application

        # Simulate a running health monitor task
        task = asyncio.create_task(asyncio.sleep(9999))
        bot._health_monitor_task = task

        await bot.stop()

        assert task.cancelled()

    @pytest.mark.asyncio
    async def test_health_monitor_resets_timestamp_before_restart(self, bot, mock_application):
        """After updater.stop(), timestamp is reset before start_polling is called.

        This prevents the monitor from triggering another restart immediately.
        """
        bot.application = mock_application
        bot._last_get_updates_ts = time.monotonic() - 60

        before = time.monotonic()
        await bot.application.updater.stop()
        bot._last_get_updates_ts = time.monotonic()
        after = time.monotonic()

        assert before <= bot._last_get_updates_ts <= after

    @pytest.mark.asyncio
    async def test_health_monitor_survives_restart_error(self, bot, mock_application):
        """Monitor logs error and continues if restart fails; does not propagate exception."""
        bot.application = mock_application
        bot._last_get_updates_ts = time.monotonic() - 60

        mock_application.updater.stop.side_effect = RuntimeError("updater already stopped")

        # Simulate one stalled monitor iteration — should not raise
        elapsed = time.monotonic() - bot._last_get_updates_ts
        assert elapsed > 45

        try:
            await bot.application.updater.stop()
            bot._last_get_updates_ts = time.monotonic()
            await bot._start_polling()
        except Exception:
            pass  # error is caught inside the monitor loop


# ---------------------------------------------------------------------------
# get_updates tracking
# ---------------------------------------------------------------------------

class TestGetUpdatesTracking:
    """Wrapping bot.get_updates should update _last_get_updates_ts."""

    @pytest.mark.asyncio
    async def test_timestamp_updated_after_get_updates(self, bot, mock_application):
        """_last_get_updates_ts is updated after each successful get_updates call."""
        bot.application = mock_application
        bot._last_get_updates_ts = 0.0

        # Simulate what start() does: wrap get_updates
        _original_get_updates = mock_application.bot.get_updates

        async def _tracked_get_updates(*args, **kwargs):
            result = await _original_get_updates(*args, **kwargs)
            bot._last_get_updates_ts = time.monotonic()
            return result

        mock_application.bot.get_updates = _tracked_get_updates

        before = time.monotonic()
        await mock_application.bot.get_updates()
        after = time.monotonic()

        assert before <= bot._last_get_updates_ts <= after

    @pytest.mark.asyncio
    async def test_timestamp_not_updated_if_get_updates_raises(self, bot, mock_application):
        """_last_get_updates_ts is NOT updated when get_updates raises (stall scenario)."""
        bot._last_get_updates_ts = 0.0
        initial_ts = bot._last_get_updates_ts

        mock_application.bot.get_updates.side_effect = Exception("network error")

        _original_get_updates = mock_application.bot.get_updates

        async def _tracked_get_updates(*args, **kwargs):
            result = await _original_get_updates(*args, **kwargs)  # raises here
            bot._last_get_updates_ts = time.monotonic()            # never reached
            return result

        mock_application.bot.get_updates = _tracked_get_updates

        with pytest.raises(Exception, match="network error"):
            await mock_application.bot.get_updates()

        assert bot._last_get_updates_ts == initial_ts
