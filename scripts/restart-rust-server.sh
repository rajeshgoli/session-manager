#!/usr/bin/env bash
# Rebuild, sign, and restart the Rust Session Manager service - sm#1134.
#
# The ordering here is the point of the script, not the individual commands:
# the build and the signature must BOTH fully succeed before anything stops the
# running service, so a broken build can never take the server offline.
#
# Why the restart is bootout -> bootstrap -> kickstart and not `kickstart -k`:
# launchd can pin a launch constraint into the job registration. When it has,
# a rebuilt binary no longer satisfies it and the job is SIGKILLed at exec with
# `namespace = CODESIGNING, indicator = Launch Constraint Violation`. Because
# the plist sets KeepAlive, that becomes a crash loop rather than a single
# failure. `kickstart -k` reuses the existing registration and so keeps
# enforcing the stale constraint; only re-registering the job clears it.
# We do not hand-roll that sequence - scripts/rust-service-cutover.sh already
# implements it correctly and is the single source of truth for it.
#
# Note that `sm-server --help` exiting 0 does NOT prove launchd will accept the
# binary: a launch constraint is enforced by launchd at spawn, not by exec. The
# real pre-restart gate is `codesign --verify`, plus the post-restart checks
# below (health, session count, and a pid that stays put).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Overridable for tests and non-default deployments.
SM_LABEL="${SM_LABEL:-com.rajeshgoli.session-manager-rust}"
SM_BINARY="${SM_BINARY:-$REPO_ROOT/target/release/sm-server}"
SM_CUTOVER="${SM_CUTOVER:-$REPO_ROOT/scripts/rust-service-cutover.sh}"
SM_CONFIG="${SM_CONFIG:-$REPO_ROOT/config.yaml}"
# Space-separated; must match the labels rust-service-cutover.sh refuses to start alongside.
SM_PYTHON_LABELS="${SM_PYTHON_LABELS:-com.rajeshgoli.session-manager com.claude.session-manager}"
# Host/port are owned here and forwarded to the cutover, so the endpoint we
# health-check is by construction the one the service was told to listen on.
SM_HOST="${SM_HOST:-127.0.0.1}"
SM_PORT="${SM_PORT:-8420}"
SM_BASE_URL="http://$SM_HOST:$SM_PORT"
SM_SIGN_IDENTIFIER="${SM_SIGN_IDENTIFIER:-com.rajeshgoli.sm-server}"
SM_HEALTH_TIMEOUT="${SM_HEALTH_TIMEOUT:-60}"
SM_PID_SETTLE_SECONDS="${SM_PID_SETTLE_SECONDS:-20}"
SM_ALLOW_SESSION_DROP="${SM_ALLOW_SESSION_DROP:-0}"
DOMAIN="gui/$(id -u)"

usage() {
  cat <<EOF
Usage: scripts/restart-rust-server.sh [--allow-drop N] [--skip-build] [-h]

Rebuilds, signs, and restarts the Rust Session Manager, then verifies it.
A failing build or a failing signature leaves the running service untouched.

Options:
  --allow-drop N   Tolerate N fewer sessions after the restart (default: 0).
                   Sessions can retire on their own between the before and
                   after samples; raise this only if that is expected.
  --skip-build     Reuse the existing binary. Still signs and verifies.
  -h, --help       Show this help.

Environment overrides: SM_LABEL, SM_BINARY, SM_CUTOVER, SM_CONFIG, SM_HOST,
SM_PORT, SM_PYTHON_LABELS, SM_SIGN_IDENTIFIER, SM_HEALTH_TIMEOUT,
SM_PID_SETTLE_SECONDS, SM_ALLOW_SESSION_DROP.

SM_LABEL, SM_BINARY, SM_CONFIG, SM_HOST, and SM_PORT are forwarded to the
cutover script, so both phases always act on the same deployment.
EOF
}

SKIP_BUILD=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --allow-drop)
      SM_ALLOW_SESSION_DROP="${2:?missing --allow-drop value}"
      shift 2
      ;;
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if ! [[ "$SM_ALLOW_SESSION_DROP" =~ ^[0-9]+$ ]]; then
  echo "--allow-drop must be a non-negative integer, got: $SM_ALLOW_SESSION_DROP" >&2
  exit 2
fi

step() { printf '\n==> %s\n' "$1"; }
fail() { echo "ERROR: $1" >&2; exit 1; }

health_ok() {
  curl -sf --connect-timeout 2 --max-time 5 "$SM_BASE_URL/health" >/dev/null 2>&1
}

# Echoes the session count, or returns nonzero if the server could not be asked.
session_count() {
  local body
  body="$(curl -sf --connect-timeout 2 --max-time 5 "$SM_BASE_URL/sessions" 2>/dev/null)" || return 1
  printf '%s' "$body" | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except ValueError:
    sys.exit(1)
sessions = data.get("sessions")
if not isinstance(sessions, list):
    sys.exit(1)
print(len(sessions))
' 2>/dev/null || return 1
}

# First "key = value" line only: launchctl repeats `state` for nested entries.
launchctl_field() {
  launchctl print "$DOMAIN/$SM_LABEL" 2>/dev/null \
    | awk -v key="$1" -F' = ' '$0 ~ "^[[:space:]]*" key " = " { print $2; exit }'
}

# ---------------------------------------------------------------------------
# Phase 1: everything that can fail without consequence.
# Nothing below this line may touch the running service.
# ---------------------------------------------------------------------------

step "Recording pre-restart state"
BEFORE_HEALTHY=0
BEFORE_SESSIONS=""
if health_ok; then
  BEFORE_HEALTHY=1
  BEFORE_SESSIONS="$(session_count || true)"
  echo "service is up; sessions before: ${BEFORE_SESSIONS:-<unavailable>}"
else
  echo "service is not answering /health; treating this as a recovery restart"
fi

step "Preflight: checking what the restart will require"
# rust-service-cutover.sh restart-rust stops the service and only then validates
# start-rust's preconditions, so a precondition that fails there leaves the
# server down. Check the same things here, while stopping nothing.
[[ -r "$SM_CONFIG" ]] || fail "config not readable: $SM_CONFIG - the running service was not touched"
for label in $SM_PYTHON_LABELS; do
  if launchctl print "$DOMAIN/$label" >/dev/null 2>&1; then
    fail "Python service label $label is still loaded; start-rust would refuse to
       start Rust after stopping it. Run '$SM_CUTOVER stop-python' first.
       The running service was not touched."
  fi
done
echo "preconditions ok"

if [[ "$SKIP_BUILD" -eq 1 ]]; then
  step "Skipping build (--skip-build)"
else
  step "Building sm-server (service still running)"
  cargo build --release -p sm-server \
    || fail "build failed - the running service was not touched"
fi

[[ -x "$SM_BINARY" ]] || fail "binary not found or not executable at $SM_BINARY - the running service was not touched"

step "Signing $SM_BINARY"
# A stable identifier keeps the signing identity from churning per build. Ad-hoc
# signing otherwise derives the identifier from the Mach-O UUID (and the linker
# derives it from cargo's deps/ filename), so it changed on every rebuild.
codesign --force --sign - --identifier "$SM_SIGN_IDENTIFIER" "$SM_BINARY" \
  || fail "codesign failed - the running service was not touched"

step "Verifying signature"
codesign --verify --strict "$SM_BINARY" \
  || fail "signature verification failed - the running service was not touched"
echo "signature ok: $(codesign -dvvv "$SM_BINARY" 2>&1 | awk -F= '/^Identifier=/{print $2}')"

# ---------------------------------------------------------------------------
# Phase 2: from here on the service is affected.
# ---------------------------------------------------------------------------

step "Restarting service (bootout -> bootstrap -> kickstart)"
[[ -x "$SM_CUTOVER" ]] || fail "cutover script not executable: $SM_CUTOVER"
# Forward every deployment value explicitly. The cutover script initialises its
# own defaults and reads none of these variables, so omitting them would let this
# script sign and verify one deployment while restarting a different (default,
# i.e. production) one.
"$SM_CUTOVER" restart-rust \
  --label "$SM_LABEL" \
  --binary "$SM_BINARY" \
  --config "$SM_CONFIG" \
  --host "$SM_HOST" \
  --port "$SM_PORT" \
  || fail "restart failed - see output above; service may be down"

step "Waiting for /health (timeout ${SM_HEALTH_TIMEOUT}s)"
deadline=$((SECONDS + SM_HEALTH_TIMEOUT))
until health_ok; do
  if (( SECONDS >= deadline )); then
    echo "--- launchctl state ---" >&2
    launchctl print "$DOMAIN/$SM_LABEL" 2>&1 | sed -n '1,25p' >&2
    fail "service did not become healthy within ${SM_HEALTH_TIMEOUT}s"
  fi
  sleep 1
done
echo "healthy"

step "Checking the pid stays put for ${SM_PID_SETTLE_SECONDS}s"
# A single check cannot tell a healthy service from one mid-crash-loop: with
# KeepAlive, a rejected binary is restarted and briefly looks alive each time.
first_pid="$(launchctl_field pid)"
[[ -n "$first_pid" ]] || fail "job $SM_LABEL has no pid right after a successful health check"
settle_deadline=$((SECONDS + SM_PID_SETTLE_SECONDS))
while (( SECONDS < settle_deadline )); do
  sleep 2
  state="$(launchctl_field state)"
  now_pid="$(launchctl_field pid)"
  [[ "$state" == "running" ]] || fail "job state is '$state', expected 'running' (crash loop?)"
  [[ "$now_pid" == "$first_pid" ]] \
    || fail "pid changed $first_pid -> $now_pid: the service is restarting under us (crash loop)"
done
echo "pid $first_pid stable, state running"

step "Comparing session count"
AFTER_SESSIONS="$(session_count || true)"
[[ -n "$AFTER_SESSIONS" ]] || fail "could not read session count after restart"
echo "sessions after: $AFTER_SESSIONS"
if [[ "$BEFORE_HEALTHY" -eq 1 && -n "$BEFORE_SESSIONS" ]]; then
  min_expected=$((BEFORE_SESSIONS - SM_ALLOW_SESSION_DROP))
  (( min_expected < 0 )) && min_expected=0
  if (( AFTER_SESSIONS < min_expected )); then
    fail "session count dropped $BEFORE_SESSIONS -> $AFTER_SESSIONS (allowed drop: $SM_ALLOW_SESSION_DROP)"
  fi
  echo "session count ok ($BEFORE_SESSIONS -> $AFTER_SESSIONS)"
else
  echo "no usable before-count; skipping comparison"
fi

step "Done"
echo "$SM_LABEL is healthy on a freshly built binary (pid $first_pid)."
