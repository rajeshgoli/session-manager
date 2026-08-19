#!/bin/bash

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SHIM="$ROOT/scripts/sm-thin-client.sh"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/sm-thin-client-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/bin" "$TMP/home/.ssh"

cat >"$TMP/bin/nc" <<'EOF'
#!/bin/sh
exit 0
EOF

cat >"$TMP/bin/sleep" <<'EOF'
#!/bin/sh
exit 0
EOF

cat >"$TMP/bin/ssh" <<'EOF'
#!/bin/sh
count=0
if [ -f "$MOCK_SSH_COUNT" ]; then count=$(cat "$MOCK_SSH_COUNT"); fi
count=$((count + 1))
printf '%s' "$count" >"$MOCK_SSH_COUNT"
printf '%s\n' "$*" >>"$MOCK_SSH_LOG"
case ",$MOCK_SSH_RESULTS," in
  *,"$count":255,*) exit 255 ;;
  *) exit 0 ;;
esac
EOF

chmod +x "$TMP/bin/nc" "$TMP/bin/sleep" "$TMP/bin/ssh"

run_shim() {
  local results=$1
  shift
  : >"$TMP/ssh.log"
  rm -f "$TMP/ssh.count"
  HOME="$TMP/home" \
    PATH="$TMP/bin:/usr/bin:/bin" \
    MOCK_SSH_COUNT="$TMP/ssh.count" \
    MOCK_SSH_LOG="$TMP/ssh.log" \
    MOCK_SSH_RESULTS="$results" \
    "$SHIM" "$@"
}

set +e
# shellcheck disable=SC2016 # Literal payload must never execute locally.
mutation_output=$(run_shim '1:255' spawn --name test-agent '$(touch /tmp/must-not-run)' 2>&1)
mutation_rc=$?
set -e
test "$mutation_rc" -eq 255
test "$(cat "$TMP/ssh.count")" -eq 1
grep -Fq "outcome is unknown, refusing to retry" <<<"$mutation_output"
test ! -e /tmp/must-not-run

run_shim '1:255' watch >/dev/null 2>&1
test "$(cat "$TMP/ssh.count")" -eq 2

run_shim '1:255' attach abc123 >/dev/null 2>&1
test "$(cat "$TMP/ssh.count")" -eq 2

run_shim '1:255' --api-url http://127.0.0.1:8420 watch >/dev/null 2>&1
test "$(cat "$TMP/ssh.count")" -eq 2

run_shim '1:255' --api-url=http://127.0.0.1:8420 attach abc123 >/dev/null 2>&1
test "$(cat "$TMP/ssh.count")" -eq 2

run_shim '' all >/dev/null 2>&1
test "$(cat "$TMP/ssh.count")" -eq 1
grep -Fq 'ConnectTimeout=10' "$TMP/ssh.log"

# Run watch under a real pseudo-terminal. A transport drop must emit the local
# mouse-disable sequence even though the remote channel is already gone.
PYTHON_BIN="$(command -v python3)"
export SHIM TMP
"$PYTHON_BIN" <<'PY'
import errno
import os
import pty
import subprocess

master, slave = pty.openpty()
env = os.environ.copy()
env.update(
    {
        "HOME": f"{env['TMP']}/home",
        "PATH": f"{env['TMP']}/bin:/usr/bin:/bin",
        "MOCK_SSH_COUNT": f"{env['TMP']}/pty-ssh.count",
        "MOCK_SSH_LOG": f"{env['TMP']}/pty-ssh.log",
        "MOCK_SSH_RESULTS": "1:255",
    }
)
proc = subprocess.Popen(
    [env["SHIM"], "watch"],
    stdin=slave,
    stdout=slave,
    stderr=slave,
    env=env,
    close_fds=True,
)
os.close(slave)
output = bytearray()
while True:
    try:
        chunk = os.read(master, 4096)
    except OSError as exc:
        if exc.errno == errno.EIO:
            break
        raise
    if not chunk:
        break
    output.extend(chunk)
os.close(master)
assert proc.wait(timeout=10) == 0
reset = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?25h"
assert reset in output, output
PY

echo "sm thin-client tests passed"
