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

run_shim '' all >/dev/null 2>&1
test "$(cat "$TMP/ssh.count")" -eq 1
grep -Fq 'ConnectTimeout=10' "$TMP/ssh.log"

echo "sm thin-client tests passed"
