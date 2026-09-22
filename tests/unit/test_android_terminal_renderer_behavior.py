from pathlib import Path
import shutil
import subprocess

import pytest


def test_shipped_xterm_stream_delivery_and_scroll_behavior():
    node = shutil.which("node")
    if node is None:
        pytest.skip("Node is required to exercise the bundled xterm parser")
    script = Path(__file__).with_name("android_terminal_renderer_behavior.cjs")
    result = subprocess.run(
        [node, "--test", str(script)], capture_output=True, text=True, timeout=30
    )
    assert result.returncode == 0, result.stdout + result.stderr
