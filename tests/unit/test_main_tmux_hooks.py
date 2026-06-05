import asyncio
from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from src.main import SessionManagerApp


@pytest.mark.asyncio
async def test_tmux_client_hooks_wait_for_uvicorn_started():
    app = SessionManagerApp.__new__(SessionManagerApp)
    ensure_hooks = Mock()
    app.session_manager = SimpleNamespace(
        sessions={"session-1": object()},
        tmux=SimpleNamespace(ensure_client_event_hooks=ensure_hooks),
    )
    server = SimpleNamespace(started=False, should_exit=False)

    task = asyncio.create_task(app._install_tmux_client_hooks_after_bind(server))
    await asyncio.sleep(0)

    ensure_hooks.assert_not_called()
    server.started = True
    await asyncio.wait_for(task, timeout=1)

    ensure_hooks.assert_called_once_with()


@pytest.mark.asyncio
async def test_tmux_client_hooks_skip_when_server_exits_before_bind():
    app = SessionManagerApp.__new__(SessionManagerApp)
    ensure_hooks = Mock()
    app.session_manager = SimpleNamespace(
        sessions={"session-1": object()},
        tmux=SimpleNamespace(ensure_client_event_hooks=ensure_hooks),
    )
    server = SimpleNamespace(started=False, should_exit=True)

    await app._install_tmux_client_hooks_after_bind(server)

    ensure_hooks.assert_not_called()
