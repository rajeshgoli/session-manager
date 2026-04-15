"""Regression tests for issue #348: codex spawn launcher must actually press Enter."""

from unittest.mock import patch

from src.tmux_controller import TmuxController


def test_create_session_with_command_sends_launch_and_enter_separately(tmp_path):
    controller = TmuxController(log_dir=str(tmp_path))
    run_calls = []
    sleep_calls = []

    def mock_run_tmux(*args, **kwargs):
        run_calls.append((args, kwargs))
        return None

    with patch.object(controller, "session_exists", return_value=False), \
         patch.object(controller, "_run_tmux", side_effect=mock_run_tmux), \
         patch("time.sleep", side_effect=lambda secs: sleep_calls.append(secs)):
        ok = controller.create_session_with_command(
            session_name="codex-test",
            working_dir=str(tmp_path),
            log_file=str(tmp_path / "codex-test.log"),
            session_id="sess348",
            command="codex",
            args=["--dangerously-bypass-approvals-and-sandbox"],
            initial_prompt="launch codex safely",
        )

    assert ok is True
    launch_call = next(
        call for call in run_calls
        if call[0][:4] == ("send-keys", "-t", "codex-test", "--")
    )
    assert launch_call[0][4] == "codex --dangerously-bypass-approvals-and-sandbox -- 'launch codex safely'"
    enter_call = next(
        call for call in run_calls
        if call[0] == ("send-keys", "-t", "codex-test", "Enter")
    )
    launch_index = run_calls.index(launch_call)
    enter_index = run_calls.index(enter_call)
    assert enter_index > launch_index
    assert sleep_calls[0] == controller.shell_export_settle_seconds
    assert sleep_calls[1] == controller._compute_settle_delay_seconds(launch_call[0][4])


def test_create_session_with_command_without_prompt_keeps_single_enter(tmp_path):
    controller = TmuxController(log_dir=str(tmp_path))
    run_calls = []
    sleep_calls = []

    def mock_run_tmux(*args, **kwargs):
        run_calls.append((args, kwargs))
        return None

    with patch.object(controller, "session_exists", return_value=False), \
         patch.object(controller, "_run_tmux", side_effect=mock_run_tmux), \
         patch("time.sleep", side_effect=lambda secs: sleep_calls.append(secs)):
        ok = controller.create_session_with_command(
            session_name="codex-test",
            working_dir=str(tmp_path),
            log_file=str(tmp_path / "codex-test.log"),
            session_id="sess348",
            command="codex",
            args=["--dangerously-bypass-approvals-and-sandbox"],
            initial_prompt=None,
        )

    assert ok is True
    launch_call = next(
        call for call in run_calls
        if call[0][:4] == ("send-keys", "-t", "codex-test", "--")
    )
    assert launch_call[0][4] == "codex --dangerously-bypass-approvals-and-sandbox"
    enter_call = next(
        call for call in run_calls
        if call[0] == ("send-keys", "-t", "codex-test", "Enter")
    )
    assert run_calls.index(enter_call) > run_calls.index(launch_call)
    assert sleep_calls[0] == controller.shell_export_settle_seconds
    assert sleep_calls[1] == controller._compute_settle_delay_seconds(launch_call[0][4])
    assert sleep_calls[2] == controller.claude_init_no_prompt_seconds


def test_create_session_with_command_seeds_neutral_pane_title_before_launch(tmp_path):
    controller = TmuxController(log_dir=str(tmp_path))
    run_calls = []

    def mock_run_tmux(*args, **kwargs):
        run_calls.append((args, kwargs))
        return None

    with patch.object(controller, "session_exists", return_value=False), \
         patch.object(controller, "_run_tmux", side_effect=mock_run_tmux), \
         patch("time.sleep", side_effect=lambda secs: None):
        ok = controller.create_session_with_command(
            session_name="claude-test",
            working_dir=str(tmp_path),
            log_file=str(tmp_path / "claude-test.log"),
            session_id="sess547",
            command="claude",
            args=["--dangerously-skip-permissions"],
            initial_prompt=None,
        )

    assert ok is True
    new_session_call = next(
        call for call in run_calls
        if call[0][:4] == ("new-session", "-d", "-s", "claude-test")
    )
    pane_title_call = next(
        call for call in run_calls
        if call[0] == ("select-pane", "-t", "claude-test", "-T", "claude-test")
    )
    pipe_pane_call = next(
        call for call in run_calls
        if call[0][:3] == ("pipe-pane", "-t", "claude-test")
    )

    assert run_calls.index(new_session_call) < run_calls.index(pane_title_call) < run_calls.index(pipe_pane_call)


def test_create_session_with_command_rejects_missing_filesystem_command(tmp_path):
    controller = TmuxController(log_dir=str(tmp_path))
    missing_command = tmp_path / "missing" / "codex"

    with patch.object(controller, "session_exists", return_value=False), \
         patch.object(controller, "_run_tmux") as run_tmux:
        ok = controller.create_session_with_command(
            session_name="codex-test",
            working_dir=str(tmp_path),
            log_file=str(tmp_path / "codex-test.log"),
            session_id="sess582",
            command=str(missing_command),
            args=["--dangerously-bypass-approvals-and-sandbox"],
        )

    assert ok is False
    assert controller.last_error_message == f"Launch command does not exist: {missing_command}"
    run_tmux.assert_not_called()
