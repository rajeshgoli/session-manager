import subprocess
from unittest.mock import AsyncMock, MagicMock

import pytest

from src.tmux_controller import TmuxController


def test_set_status_bar_passes_timeout_to_tmux(monkeypatch):
    controller = TmuxController()
    monkeypatch.setattr(controller, "session_exists", lambda _: True)
    run_tmux = MagicMock(return_value=MagicMock(returncode=0))
    monkeypatch.setattr(controller, "_run_tmux_for_session", run_tmux)

    ok = controller.set_status_bar("claude-test123", "friendly", timeout_seconds=1.0)

    assert ok is True
    run_tmux.assert_called_once_with(
        "claude-test123",
        "set-option",
        "-t",
        "claude-test123",
        "status-left",
        "[friendly] ",
        timeout=1.0,
    )


def test_set_status_bar_returns_false_on_timeout(monkeypatch):
    controller = TmuxController()
    monkeypatch.setattr(controller, "session_exists", lambda _: True)

    def _raise_timeout(*args, **kwargs):
        raise subprocess.TimeoutExpired(cmd=["tmux", "set-option"], timeout=1.0)

    monkeypatch.setattr(controller, "_run_tmux_for_session", _raise_timeout)

    ok = controller.set_status_bar("claude-test123", "friendly", timeout_seconds=1.0)

    assert ok is False


def test_create_session_with_command_bootstraps_history_before_provider_window(tmp_path, monkeypatch):
    controller = TmuxController(
        log_dir=str(tmp_path),
        config={
            "tmux": {
                "socket_name": "session-manager-test",
                "native_scrollback": True,
                "history_limit": 12345,
            },
            "timeouts": {"tmux": {"shell_export_settle_seconds": 0}},
        },
    )
    calls = []

    def _fake_run_tmux(*args, **kwargs):
        calls.append(args)
        if args[:3] == ("show-options", "-gqv", "terminal-overrides"):
            return MagicMock(returncode=0, stdout="")
        return MagicMock(returncode=0, stdout="")

    monkeypatch.setattr(controller, "session_exists", lambda _: False)
    monkeypatch.setattr(controller, "_session_exists_on_socket", lambda *_: False)
    monkeypatch.setattr(controller, "_run_tmux", _fake_run_tmux)
    monkeypatch.setattr("time.sleep", lambda _: None)

    ok = controller.create_session_with_command(
        "claude-test123",
        str(tmp_path),
        str(tmp_path / "claude-test123.log"),
        command="sh",
        args=["-lc", "sleep 1"],
    )

    assert ok is True
    assert calls[:12] == [
        (
            "new-session",
            "-d",
            "-s",
            TmuxController.SERVER_ANCHOR_SESSION,
            "-n",
            "anchor",
            "-c",
            str(tmp_path),
            "sleep 315360000",
        ),
        ("new-session", "-d", "-s", "claude-test123", "-c", str(tmp_path), "-n", "__sm_bootstrap"),
        ("set-option", "-g", "focus-events", "on"),
        (
            "set-hook",
            "-g",
            "client-attached[90]",
            TmuxController._tmux_client_event_hook_command(
                "client-attached",
                "http://127.0.0.1:8420/hooks/tmux-client",
            ),
        ),
        (
            "set-hook",
            "-g",
            "client-detached[90]",
            TmuxController._tmux_client_event_hook_command(
                "client-detached",
                "http://127.0.0.1:8420/hooks/tmux-client",
            ),
        ),
        (
            "set-hook",
            "-g",
            "client-session-changed[90]",
            TmuxController._tmux_client_event_hook_command(
                "client-session-changed",
                "http://127.0.0.1:8420/hooks/tmux-client",
            ),
        ),
        ("show-options", "-gqv", "terminal-overrides"),
        ("set-option", "-as", "terminal-overrides", ",*:smcup@:rmcup@"),
        ("set-option", "-t", "claude-test123", "history-limit", "12345"),
        ("new-window", "-d", "-t", "claude-test123", "-n", "main", "-c", str(tmp_path)),
        ("kill-window", "-t", "claude-test123:__sm_bootstrap"),
        ("select-window", "-t", "claude-test123:main"),
    ]


def test_create_session_with_command_uses_existing_server_anchor(tmp_path, monkeypatch):
    controller = TmuxController(
        log_dir=str(tmp_path),
        config={
            "tmux": {
                "socket_name": "session-manager-test",
                "history_limit": 12345,
            },
            "timeouts": {"tmux": {"shell_export_settle_seconds": 0}},
        },
    )
    calls = []

    def _fake_run_tmux(*args, **kwargs):
        calls.append(args)
        return MagicMock(returncode=0, stdout="")

    monkeypatch.setattr(controller, "session_exists", lambda _: False)
    monkeypatch.setattr(
        controller,
        "_session_exists_on_socket",
        lambda session_name, socket_name: session_name == TmuxController.SERVER_ANCHOR_SESSION,
    )
    monkeypatch.setattr(controller, "_run_tmux", _fake_run_tmux)
    monkeypatch.setattr("time.sleep", lambda _: None)

    ok = controller.create_session_with_command(
        "claude-testanchor",
        str(tmp_path),
        str(tmp_path / "claude-testanchor.log"),
        command="sh",
        args=["-lc", "sleep 1"],
    )

    assert ok is True
    assert not any(
        call[:4] == ("new-session", "-d", "-s", TmuxController.SERVER_ANCHOR_SESSION)
        for call in calls
    )


def test_create_session_with_command_enables_focus_events_without_native_scrollback(tmp_path, monkeypatch):
    controller = TmuxController(
        log_dir=str(tmp_path),
        config={
            "tmux": {
                "socket_name": "session-manager-test",
                "native_scrollback": False,
            },
            "timeouts": {"tmux": {"shell_export_settle_seconds": 0}},
        },
    )
    calls = []

    def _fake_run_tmux(*args, **kwargs):
        calls.append(args)
        return MagicMock(returncode=0, stdout="")

    monkeypatch.setattr(controller, "session_exists", lambda _: False)
    monkeypatch.setattr(controller, "_session_exists_on_socket", lambda *_: False)
    monkeypatch.setattr(controller, "_run_tmux", _fake_run_tmux)
    monkeypatch.setattr("time.sleep", lambda _: None)

    ok = controller.create_session_with_command(
        "claude-focus",
        str(tmp_path),
        str(tmp_path / "claude-focus.log"),
        command="sh",
        args=["-lc", "sleep 1"],
    )

    assert ok is True
    assert ("set-option", "-g", "focus-events", "on") in calls
    assert any(call[:3] == ("set-hook", "-g", "client-attached[90]") for call in calls)
    assert any(call[:3] == ("set-hook", "-g", "client-detached[90]") for call in calls)
    assert ("show-options", "-gqv", "terminal-overrides") not in calls
    assert ("set-option", "-as", "terminal-overrides", ",*:smcup@:rmcup@") not in calls


def test_client_event_hook_command_posts_tmux_formats():
    command = TmuxController._tmux_client_event_hook_command(
        "client-attached",
        "http://127.0.0.1:8420/hooks/tmux-client",
    )

    assert command.startswith("run-shell -b ")
    assert "client-attached" in command
    assert "session=#{hook_session_name}" in command
    assert "tty=#{client_tty}" in command
    assert "client_pid=#{client_pid}" in command
    assert "/hooks/tmux-client" in command


def test_create_session_with_command_enables_exit_diagnostics(tmp_path, monkeypatch):
    controller = TmuxController(
        log_dir=str(tmp_path),
        config={"timeouts": {"tmux": {"shell_export_settle_seconds": 0}}},
    )
    calls = []

    def _fake_run_tmux(*args, **kwargs):
        calls.append(args)
        if args[:3] == ("display-message", "-p", "-t"):
            return MagicMock(returncode=0, stdout="%main\n")
        return MagicMock(returncode=0, stdout="")

    monkeypatch.setattr(controller, "session_exists", lambda _: False)
    monkeypatch.setattr(controller, "_run_tmux", _fake_run_tmux)
    monkeypatch.setattr("time.sleep", lambda _: None)

    ok = controller.create_session_with_command(
        "claude-exitdiag",
        str(tmp_path),
        str(tmp_path / "claude-exitdiag.log"),
        command="sh",
        args=["-lc", "sleep 1"],
    )

    assert ok is True
    assert (
        "set-window-option",
        "-t",
        "claude-exitdiag:main",
        "remain-on-exit",
        "on",
    ) in calls
    assert (
        "set-option",
        "-t",
        "claude-exitdiag",
        "@sm_main_pane_id",
        "%main",
    ) in calls


def test_create_session_with_command_uses_remote_resolved_working_dir(tmp_path, monkeypatch):
    controller = TmuxController(
        log_dir=str(tmp_path),
        config={"timeouts": {"tmux": {"shell_export_settle_seconds": 0}}},
    )
    calls = []
    resolve_directory = MagicMock(return_value="/home/dev/repo")
    command_available = MagicMock(return_value=True)
    ensure_file = MagicMock(return_value=True)
    monkeypatch.setattr(controller.node_runner, "resolve_directory", resolve_directory)
    monkeypatch.setattr(controller.node_runner, "command_available", command_available)
    monkeypatch.setattr(controller.node_runner, "ensure_file", ensure_file)

    def _fake_run_tmux(*args, **kwargs):
        calls.append(args)
        if args[:3] == ("display-message", "-p", "-t"):
            return MagicMock(returncode=0, stdout="%main\n")
        return MagicMock(returncode=0, stdout="")

    monkeypatch.setattr(controller, "_session_exists_for_node", lambda *_args, **_kwargs: False)
    monkeypatch.setattr(controller, "_run_tmux", _fake_run_tmux)
    monkeypatch.setattr("time.sleep", lambda _: None)

    ok = controller.create_session_with_command(
        "claude-remote",
        "~/repo",
        "/remote/logs/claude-remote.log",
        command="claude",
        node="worker",
    )

    assert ok is True
    resolve_directory.assert_called_once_with("worker", "~/repo")
    command_available.assert_called_once_with("worker", "claude", cwd="/home/dev/repo")
    ensure_file.assert_called_once_with("worker", "/remote/logs/claude-remote.log")
    assert (
        "new-session",
        "-d",
        "-s",
        "claude-remote",
        "-c",
        "/home/dev/repo",
        "-n",
        "__sm_bootstrap",
    ) in calls
    assert ("new-window", "-d", "-t", "claude-remote", "-n", "main", "-c", "/home/dev/repo") in calls


def test_create_session_reports_remote_preflight_unreachable(tmp_path, monkeypatch):
    controller = TmuxController(log_dir=str(tmp_path))

    def _raise_unreachable(*_args, **_kwargs):
        raise RuntimeError("Node worker unreachable: ssh transport failed")

    monkeypatch.setattr(controller, "_session_exists_for_node", _raise_unreachable)

    ok = controller.create_session(
        "claude-remote",
        "/home/dev/repo",
        str(tmp_path / "claude-remote.log"),
        node="worker",
    )

    assert ok is False
    assert controller.last_error_message == "Node worker unreachable: ssh transport failed"


def test_create_session_with_command_reports_remote_preflight_unreachable(tmp_path, monkeypatch):
    controller = TmuxController(log_dir=str(tmp_path))

    def _raise_unreachable(*_args, **_kwargs):
        raise RuntimeError("Node worker unreachable: ssh transport failed")

    monkeypatch.setattr(controller, "_session_exists_for_node", _raise_unreachable)

    ok = controller.create_session_with_command(
        "claude-remote",
        "/home/dev/repo",
        str(tmp_path / "claude-remote.log"),
        command="claude",
        node="worker",
    )

    assert ok is False
    assert controller.last_error_message == "Node worker unreachable: ssh transport failed"


def test_get_session_exit_diagnostics_reports_dead_pane(monkeypatch):
    controller = TmuxController(config={"tmux": {"socket_name": "session-manager-test"}})
    monkeypatch.setattr(
        controller,
        "_session_exists_on_socket",
        lambda session_name, socket_name: socket_name == "session-manager-test",
    )

    def _fake_run_tmux(*args, **kwargs):
        if args[:2] == ("list-sessions", "-F"):
            return MagicMock(returncode=0, stdout="codex-fork-dead\n")
        if args[:3] == ("show-options", "-qv", "-t"):
            return MagicMock(returncode=0, stdout="")
        if args[:2] == ("list-panes", "-t"):
            return MagicMock(
                returncode=0,
                stdout="%1\t1\t2\t0\tcodex\t12345\t/dev/ttys001\n",
            )
        return MagicMock(returncode=1, stdout="", stderr="unexpected")

    monkeypatch.setattr(controller, "_run_tmux", _fake_run_tmux)

    diagnostics = controller.get_session_exit_diagnostics("codex-fork-dead")

    assert diagnostics["exists"] is True
    assert diagnostics["pane_dead"] is True
    assert diagnostics["pane_dead_status"] == "2"
    assert diagnostics["pane_dead_signal"] == "0"
    assert diagnostics["pane_current_command"] == "codex"
    assert diagnostics["socket_name"] == "session-manager-test"


def test_get_session_exit_diagnostics_ignores_dead_auxiliary_pane(monkeypatch):
    controller = TmuxController(config={"tmux": {"socket_name": "session-manager-test"}})
    monkeypatch.setattr(
        controller,
        "_session_exists_on_socket",
        lambda session_name, socket_name: socket_name == "session-manager-test",
    )

    def _fake_run_tmux(*args, **kwargs):
        if args[:2] == ("list-sessions", "-F"):
            return MagicMock(returncode=0, stdout="claude-live\n")
        if args[:3] == ("show-options", "-qv", "-t"):
            return MagicMock(returncode=0, stdout="%main\n")
        if args[:2] == ("list-panes", "-t"):
            return MagicMock(
                returncode=0,
                stdout=(
                    "%main\t0\t\t\tclaude\t111\t/dev/ttys001\t1\tclaude-live\n"
                    "%aux\t1\t0\t\tzsh\t222\t/dev/ttys002\t0\taux-shell\n"
                ),
            )
        return MagicMock(returncode=1, stdout="", stderr="unexpected")

    monkeypatch.setattr(controller, "_run_tmux", _fake_run_tmux)

    diagnostics = controller.get_session_exit_diagnostics("claude-live")

    assert diagnostics["exists"] is True
    assert diagnostics["pane_dead"] is False
    assert diagnostics["pane_id"] == "%main"
    assert diagnostics["dead_panes"][0]["pane_id"] == "%aux"


def test_get_session_exit_diagnostics_reports_dead_main_pane(monkeypatch):
    controller = TmuxController(config={"tmux": {"socket_name": "session-manager-test"}})
    monkeypatch.setattr(
        controller,
        "_session_exists_on_socket",
        lambda session_name, socket_name: socket_name == "session-manager-test",
    )

    def _fake_run_tmux(*args, **kwargs):
        if args[:2] == ("list-sessions", "-F"):
            return MagicMock(returncode=0, stdout="claude-dead-main\n")
        if args[:3] == ("show-options", "-qv", "-t"):
            return MagicMock(returncode=0, stdout="%main\n")
        if args[:2] == ("list-panes", "-t"):
            return MagicMock(
                returncode=0,
                stdout=(
                    "%main\t1\t9\t\tclaude\t111\t/dev/ttys001\t0\tclaude-dead-main\n"
                    "%aux\t0\t\t\tzsh\t222\t/dev/ttys002\t1\taux-shell\n"
                ),
            )
        return MagicMock(returncode=1, stdout="", stderr="unexpected")

    monkeypatch.setattr(controller, "_run_tmux", _fake_run_tmux)

    diagnostics = controller.get_session_exit_diagnostics("claude-dead-main")

    assert diagnostics["pane_dead"] is True
    assert diagnostics["pane_id"] == "%main"
    assert diagnostics["pane_dead_status"] == "9"


def test_get_session_exit_diagnostics_snapshots_missing_session(monkeypatch):
    controller = TmuxController(config={"tmux": {"socket_name": "session-manager-test"}})
    monkeypatch.setattr(controller, "_session_exists_on_socket", lambda *_: False)

    def _fake_run_tmux(*args, **kwargs):
        socket_name = kwargs.get("socket_name")
        if socket_name == "session-manager-test":
            return MagicMock(returncode=0, stdout="other-managed\n")
        if socket_name is None:
            return MagicMock(returncode=0, stdout="legacy-session\n")
        return MagicMock(returncode=1, stdout="", stderr="")

    monkeypatch.setattr(controller, "_run_tmux", _fake_run_tmux)

    diagnostics = controller.get_session_exit_diagnostics("missing-session")

    assert diagnostics["exists"] is False
    assert diagnostics["pane_dead"] is False
    assert diagnostics["sessions_on_configured_socket"] == ["other-managed"]
    assert diagnostics["sessions_on_default_socket"] == ["legacy-session"]


def test_codex_rename_prompt_detection():
    controller = TmuxController()

    assert controller._looks_like_codex_rename_prompt("Name thread\nPress enter to confirm or esc to go back")
    assert controller._looks_like_codex_rename_prompt("Rename thread\nPress enter to confirm or esc to go back")
    assert not controller._looks_like_codex_rename_prompt("› /rename worker")


def test_codex_rename_prompt_detection_uses_prompt_region_only():
    controller = TmuxController()
    pane_text = """Name thread
old-name
Press enter to confirm or esc to go back

› normal prompt text

  gpt-5.5 xhigh · ~/repo
"""

    prompt_region = controller._extract_active_codex_prompt_region(pane_text)

    assert prompt_region is not None
    assert "normal prompt text" in prompt_region
    assert "Name thread" not in prompt_region
    assert not controller._looks_like_codex_rename_prompt(prompt_region)


def test_codex_active_region_keeps_deferred_banner_above_prompt():
    controller = TmuxController()
    pane_text = """• running tool output

Submitted after next tool call
WAKE payload preview

› queued prompt text

  gpt-5.5 xhigh · ~/repo
"""

    active_region = controller._extract_active_codex_region(pane_text)

    assert active_region is not None
    assert "Submitted after next tool call" in active_region
    assert "WAKE payload preview" in active_region
    assert controller._looks_like_codex_deferred_send_banner(active_region)


@pytest.mark.asyncio
async def test_rename_codex_thread_uses_interactive_dialog(monkeypatch):
    controller = TmuxController()
    monkeypatch.setattr(controller, "session_exists", lambda _: True)
    exit_copy = AsyncMock(return_value=(0, 0))
    send_key = AsyncMock(return_value=True)
    send_input = AsyncMock(return_value=True)
    capture = AsyncMock(return_value="Rename thread\nworker-old\nPress enter to confirm or esc to go back")
    monkeypatch.setattr(controller, "_exit_copy_mode_if_needed_async", exit_copy)
    monkeypatch.setattr(controller, "send_key_async", send_key)
    monkeypatch.setattr(controller, "send_input_async", send_input)
    monkeypatch.setattr(controller, "_capture_pane_async", capture)

    ok = await controller.rename_codex_thread_async("codex-test", "worker-new")

    assert ok is True
    exit_copy.assert_awaited_once_with("codex-test")
    assert [call.args for call in send_key.await_args_list] == [("codex-test", "C-u"), ("codex-test", "C-u")]
    assert [call.args for call in send_input.await_args_list] == [("codex-test", "/rename"), ("codex-test", "worker-new")]
