#!/usr/bin/env bash
# Rebuild, sign, and restart the Rust Session Manager service - sm#1134.
#
# The ordering here is the point of the script, not the individual commands:
# the build and the signature must BOTH fully succeed before anything stops the
# running service, so a broken build can never take the server offline.
#
# "Untouched" has to include the binary on disk, not just the running process.
# A rebuild replaces the registered executable while the old process keeps
# running from its own inode, so a later failure would leave a process that is
# alive now but whose next KeepAlive respawn uses an executable the current
# registration may reject. Phase 1 therefore snapshots the binary and restores
# it if anything fails before the restart commits.
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
# Passed to cargo as --target-dir, which outranks CARGO_TARGET_DIR and
# build.target-dir. Without pinning it, a redirected target dir would put the
# new build somewhere else while we signed and restarted a stale binary here.
SM_TARGET_DIR="${SM_TARGET_DIR:-$REPO_ROOT/target}"
SM_CARGO_OUTPUT="${SM_CARGO_OUTPUT:-$SM_TARGET_DIR/release/sm-server}"
SM_CUTOVER="${SM_CUTOVER:-$REPO_ROOT/scripts/rust-service-cutover.sh}"
SM_CONFIG="${SM_CONFIG:-$REPO_ROOT/config.yaml}"
SM_LOCAL_ENV="${SM_LOCAL_ENV:-}"
SM_PLIST="${SM_PLIST:-$HOME/Library/LaunchAgents/$SM_LABEL.plist}"
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
Usage: scripts/restart-rust-server.sh [options]

Rebuilds, signs, and restarts the Rust Session Manager, then verifies it.
A failing build or a failing signature leaves the running service untouched,
including the binary on disk.

Options:
  --allow-drop N        Tolerate N fewer sessions after the restart (default: 0).
                        Sessions can retire on their own between the before and
                        after samples; raise this only if that is expected.
  --allow-plist-change  Proceed even though restarting would rewrite the launchd
                        plist with different contents. Read the printed diff
                        first: this is how a deployment setting gets dropped.
  --skip-build          Reuse the existing binary. Still signs and verifies.
  -h, --help            Show this help.

Environment overrides: SM_LABEL, SM_BINARY, SM_TARGET_DIR, SM_CARGO_OUTPUT,
SM_CUTOVER, SM_CONFIG, SM_LOCAL_ENV, SM_PLIST, SM_HOST, SM_PORT,
SM_PYTHON_LABELS, SM_SIGN_IDENTIFIER, SM_HEALTH_TIMEOUT,
SM_PID_SETTLE_SECONDS, SM_ALLOW_SESSION_DROP.

SM_LABEL, SM_BINARY, SM_CONFIG, SM_LOCAL_ENV, SM_HOST, and SM_PORT are forwarded
to the cutover script, so both phases always act on the same deployment.
EOF
}

SKIP_BUILD=0
ALLOW_PLIST_CHANGE=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --allow-drop)
      SM_ALLOW_SESSION_DROP="${2:?missing --allow-drop value}"
      shift 2
      ;;
    --allow-plist-change)
      ALLOW_PLIST_CHANGE=1
      shift
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

# Every deployment value the cutover needs. It initialises its own defaults and
# reads none of our variables, so anything not forwarded here silently reverts
# to the default (production) deployment.
# --plist must follow --label: the cutover recomputes the plist path from the
# label, so passing them the other way round would discard our value.
cutover_args=(
  --label "$SM_LABEL"
  --plist "$SM_PLIST"
  --binary "$SM_BINARY"
  --config "$SM_CONFIG"
  --host "$SM_HOST"
  --port "$SM_PORT"
)
[[ -n "$SM_LOCAL_ENV" ]] && cutover_args+=(--local-env "$SM_LOCAL_ENV")

# --- binary rollback --------------------------------------------------------
# Armed before the build, disarmed once the restart is committed.
BINARY_BACKUP=""
BINARY_WAS_ABSENT=0
RESTORE_BINARY=0
RENDERED_PLIST=""

# Restore SM_BINARY to exactly what it was before this run - including having
# been absent. Leaving a newly built but unverified binary behind is the same
# deferred outage as leaving a modified one: the next KeepAlive respawn runs it.
cleanup() {
  local rc=$?
  if [[ "$RESTORE_BINARY" -eq 1 && $rc -ne 0 ]]; then
    if [[ -n "$BINARY_BACKUP" && -f "$BINARY_BACKUP" ]]; then
      if mv -f "$BINARY_BACKUP" "$SM_BINARY"; then
        echo "rolled back $SM_BINARY to the previously registered build" >&2
      else
        echo "WARNING: could not restore $SM_BINARY from $BINARY_BACKUP" >&2
      fi
    elif [[ "$BINARY_WAS_ABSENT" -eq 1 && -e "$SM_BINARY" ]]; then
      if rm -f "$SM_BINARY"; then
        echo "removed the unverified binary this run created at $SM_BINARY" >&2
      else
        echo "WARNING: could not remove the unverified binary at $SM_BINARY" >&2
      fi
    fi
  fi
  [[ -n "$BINARY_BACKUP" ]] && rm -f "$BINARY_BACKUP"
  [[ -n "$RENDERED_PLIST" ]] && rm -f "$RENDERED_PLIST"
  return $rc
}
trap cleanup EXIT

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
# Nothing below this line may touch the running service, and anything it
# changes on disk must be undone by cleanup().
# ---------------------------------------------------------------------------

step "Recording pre-restart state"
BEFORE_HEALTHY=0
BEFORE_SESSIONS=""
if health_ok; then
  BEFORE_HEALTHY=1
  BEFORE_SESSIONS="$(session_count || true)"
  # Without a baseline the post-restart comparison would be silently skipped,
  # so a restart that dropped the whole registry would still report success.
  [[ -n "$BEFORE_SESSIONS" ]] \
    || fail "server is healthy but its session list could not be read; refusing to
       restart without a baseline to compare against. The running service was
       not touched."
  echo "service is up; sessions before: $BEFORE_SESSIONS"
else
  echo "service is not answering /health; treating this as a recovery restart"
fi

step "Preflight: checking what the restart will require"
# rust-service-cutover.sh restart-rust stops the service and only then validates
# start-rust's preconditions, so a precondition that fails there leaves the
# server down. Check the same things here, while stopping nothing.
# Checked here, before anything writes to the binary: the plist comparison below
# only runs when a live plist exists, so this would otherwise not be caught until
# after the rebuild.
[[ -x "$SM_CUTOVER" ]] || fail "cutover script not executable: $SM_CUTOVER - the running service was not touched"
[[ -r "$SM_CONFIG" ]] || fail "config not readable: $SM_CONFIG - the running service was not touched"
if [[ -n "$SM_LOCAL_ENV" && ! -r "$SM_LOCAL_ENV" ]]; then
  fail "local env overlay not readable: $SM_LOCAL_ENV - the running service was not touched"
fi
for label in $SM_PYTHON_LABELS; do
  if launchctl print "$DOMAIN/$label" >/dev/null 2>&1; then
    fail "Python service label $label is still loaded; start-rust would refuse to
       start Rust after stopping it. Run '$SM_CUTOVER stop-python' first.
       The running service was not touched."
  fi
done

if [[ "$SKIP_BUILD" -eq 0 && "$SM_BINARY" != "$SM_CARGO_OUTPUT" ]]; then
  fail "SM_BINARY is $SM_BINARY but cargo builds $SM_CARGO_OUTPUT, so a build would
       not produce the binary we are about to sign and restart - the run would
       deploy stale code while reporting a fresh build. Re-run with --skip-build
       to deploy the existing binary. The running service was not touched."
fi

# Restarting rewrites the plist. Anything in the live plist that we would not
# regenerate is a deployment setting about to be silently dropped - a custom
# --local-env carrying auth secrets, for instance.
if [[ -f "$SM_PLIST" ]]; then
  RENDERED_PLIST="$(mktemp)"
  "$SM_CUTOVER" render-plist "${cutover_args[@]}" > "$RENDERED_PLIST" \
    || fail "could not render the plist for comparison - the running service was not touched"
  if ! diff -u "$SM_PLIST" "$RENDERED_PLIST" > "$RENDERED_PLIST.diff" 2>&1; then
    echo "--- live plist vs what the restart would write ---" >&2
    cat "$RENDERED_PLIST.diff" >&2
    rm -f "$RENDERED_PLIST.diff"
    if [[ "$ALLOW_PLIST_CHANGE" -eq 0 ]]; then
      fail "restarting would rewrite $SM_PLIST with different contents (diff above).
       Pass the missing settings (for example SM_LOCAL_ENV) so the rendered plist
       matches, or re-run with --allow-plist-change to accept the rewrite.
       The running service was not touched."
    fi
    echo "WARNING: proceeding with a plist rewrite because --allow-plist-change was given" >&2
  fi
  rm -f "$RENDERED_PLIST.diff"
fi
echo "preconditions ok"

# Arm the rollback before anything writes to the registered binary. Both the
# build and the signature replace it, and either can be followed by a failure.
if [[ -e "$SM_BINARY" ]]; then
  BINARY_BACKUP="$SM_BINARY.restart-backup.$$"
  cp -p "$SM_BINARY" "$BINARY_BACKUP" \
    || fail "could not snapshot $SM_BINARY before rebuilding - the running service was not touched"
else
  # Nothing to snapshot, but the build is about to create one. If phase 1 then
  # fails, that unverified binary has to go rather than sit in the registered
  # path waiting for the next respawn.
  BINARY_WAS_ABSENT=1
fi
RESTORE_BINARY=1

if [[ "$SKIP_BUILD" -eq 1 ]]; then
  step "Skipping build (--skip-build)"
else
  step "Building sm-server (service still running)"
  cargo build --release -p sm-server --target-dir "$SM_TARGET_DIR" \
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
# Phase 2: from here on the service is affected, and the new binary stays.
# ---------------------------------------------------------------------------

step "Restarting service (bootout -> bootstrap -> kickstart)"
# Disarm only on the line before the restart itself. Anything that can still
# fail while the rebuilt binary sits in the registered path must be able to roll
# it back, or the next KeepAlive respawn boots a build this registration may
# reject - the deferred outage this script exists to prevent.
RESTORE_BINARY=0
"$SM_CUTOVER" restart-rust "${cutover_args[@]}" \
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
