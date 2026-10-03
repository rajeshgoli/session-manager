"""Check the relocated prototype profile with harmless operations."""
from pathlib import Path
import subprocess

root = Path('/private/tmp/sm-1955')
checks = [
    ('checkout write', "touch .1955-wall-probe && rm .1955-wall-probe", True),
    ('runtime write', 'touch "$TMPDIR/wall-probe" && rm "$TMPDIR/wall-probe"', True),
    ('outside write', 'touch /private/tmp/sm-1955-outside-probe', False),
    ('credential metadata', 'stat /Users/rajesh/.claude.json >/dev/null', False),
    ('live sm network', 'curl --connect-timeout 2 --max-time 3 -s http://127.0.0.1:8420/health >/dev/null', False),
    ('internet network', 'curl --connect-timeout 2 --max-time 3 -sk https://1.1.1.1 >/dev/null', False),
]
for ticket in [1855, 1913]:
    work = f'/Users/rajesh/worktrees/sm-1955-opencode-{ticket}'
    runtime = root / str(ticket) / 'runtime'
    import os
    env = {**os.environ, 'TMPDIR': str(runtime)}
    for label, command, allow in checks:
        proc = subprocess.run(['sandbox-exec', '-f', str(root / str(ticket) / 'profile.sb'),
                               '/bin/sh', '-c', command], cwd=work, env=env,
                               capture_output=True, timeout=5)
        passed = (proc.returncode == 0) == allow
        print(ticket, label, 'PASS' if passed else 'FAIL', flush=True)
        if not passed:
            raise SystemExit(1)
