"""The Rust launcher owns a private tmux namespace and removes it on exit."""
import os
from pathlib import Path
import shutil
import subprocess

import pytest

SCRIPT = Path(__file__).resolve().parents[2] / "scripts/test-rust-isolated.sh"


@pytest.mark.parametrize("exit_code", [0, 1])
def test_launcher_reaps_tmux_and_socket_directory(tmp_path, exit_code):
    if shutil.which("tmux") is None:
        pytest.skip("tmux unavailable")
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    cargo = fake_bin / "cargo"
    cargo.write_text('''#!/bin/bash
set -eu
tmux -L sm-1363-fixture new-session -d -s fixture 'sleep 60'
tmux -L sm-1363-fixture display-message -p '#{socket_path}' > "$SOCKET_REPORT"
printf '%s' "$SM_TEST_ISOLATION_ROOT" > "$ROOT_REPORT"
exit "$TEST_EXIT_CODE"
''')
    cargo.chmod(0o755)
    report = tmp_path / "socket"
    root_report = tmp_path / "root"
    # An inherited namespace must not receive any fixture sockets.
    inherited = tmp_path / "inherited"
    inherited.mkdir()
    result = subprocess.run(
        ["bash", str(SCRIPT)],
        env={**os.environ, "PATH": f"{fake_bin}:{os.environ['PATH']}",
             "TMUX_TMPDIR": str(inherited), "SOCKET_REPORT": str(report),
             "ROOT_REPORT": str(root_report), "TEST_EXIT_CODE": str(exit_code)},
        capture_output=True, text=True, timeout=15,
    )
    assert result.returncode == exit_code, result.stderr
    socket = Path(report.read_text().strip())
    assert socket.parent.parent.name.startswith("sm-rust-tmux.")
    assert not socket.parent.parent.exists()
    assert not Path(root_report.read_text()).exists()
    assert list(inherited.iterdir()) == []
