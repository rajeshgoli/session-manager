"""Exercise the key codec used by the mobile terminal against both cursor modes."""
import json
from pathlib import Path
import subprocess

KEYS = Path(__file__).resolve().parents[2] / "android-app/app/src/main/assets/sm_terminal/terminal_keys.js"


def test_terminal_navigation_matches_normal_and_application_cursor_protocols():
    source = KEYS.read_text()
    script = source + "\nconsole.log(JSON.stringify([false, true].map(mode => ['up','down','left','right','enter','esc','tab','shift-tab','backspace','ctrl-c','unknown','constructor'].map(key => terminalKeySequence(key, mode)))));"
    normal, application = json.loads(subprocess.check_output(["node", "-e", script], text=True))
    assert normal[:4] == ["\x1b[A", "\x1b[B", "\x1b[D", "\x1b[C"]
    assert application[:4] == ["\x1bOA", "\x1bOB", "\x1bOD", "\x1bOC"]
    assert normal[4:] == application[4:] == ["\r", "\x1b", "\t", "\x1b[Z", "\x7f", "\x03", "", ""]


def test_every_terminal_script_is_served_by_the_local_asset_allowlist():
    import re
    root = Path(__file__).resolve().parents[2]
    html = KEYS.with_name("terminal.html").read_text()
    screen = (root / "android-app/app/src/main/java/li/rajeshgo/sm/ui/watch/WatchScreen.kt").read_text()
    for script in re.findall(r'<script src="([^"]+)"', html):
        assert KEYS.parent.joinpath(script).is_file()
        assert f'"/{script}" -> "sm_terminal/{script}"' in screen
