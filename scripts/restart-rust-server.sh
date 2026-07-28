#!/usr/bin/env bash
# Rebuild, sign, and restart the Rust Session Manager service - sm#1134.
#
# The ordering here is the point of the script, not the individual commands:
# the build and the signature must BOTH fully succeed before anything stops the
# running service, so a broken build can never take the server offline.
#
# That guarantee only holds if the service does not run out of the build
# directory. cargo writes to target/release/sm-server; launchd is registered
# against an installed copy under .local/bin. Nothing in phase 1 writes the
# registered path at all, so there is no window in which launchd could exec a
# build its current registration has not accepted - and an ordinary
# `cargo build` by anyone working in this repo no longer touches the live
# server's binary either. The registered path is written exactly once, by an
# atomic rename, while the job is booted out.
#
# Why the restart is bootout -> bootstrap -> kickstart and not `kickstart -k`:
# launchd can pin a launch constraint into the job registration. When it has,
# a rebuilt binary no longer satisfies it and the job is SIGKILLed at exec with
# `namespace = CODESIGNING, indicator = Launch Constraint Violation`. Because
# the plist sets KeepAlive, that becomes a crash loop rather than a single
# failure. `kickstart -k` reuses the existing registration and so keeps
# enforcing the stale constraint; only re-registering the job clears it.
# We do not hand-roll that sequence - scripts/rust-service-cutover.sh owns it.
# `restart-rust` is exactly `stop-rust` then `start-rust`, so calling those two
# halves with the install between them runs the same code, not a third path.
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
# The installed path launchd is registered against. Deliberately NOT cargo's
# output path - see the header.
SM_BINARY="${SM_BINARY:-$REPO_ROOT/.local/bin/sm-server}"
# Passed to cargo as --target-dir, which outranks CARGO_TARGET_DIR and
# build.target-dir. Without pinning it, a redirected target dir would leave us
# installing a stale artifact from the expected location.
SM_TARGET_DIR="${SM_TARGET_DIR:-$REPO_ROOT/target}"
SM_CARGO_OUTPUT="${SM_CARGO_OUTPUT:-$SM_TARGET_DIR/release/sm-server}"
SM_CUTOVER="${SM_CUTOVER:-$REPO_ROOT/scripts/rust-service-cutover.sh}"
SM_CONFIG="${SM_CONFIG:-$REPO_ROOT/config.yaml}"
SM_LOCAL_ENV="${SM_LOCAL_ENV:-}"
SM_PLIST="${SM_PLIST:-$HOME/Library/LaunchAgents/$SM_LABEL.plist}"
# The set rust-service-cutover.sh enforces in start_rust. It is hard-coded there
# with no CLI override, so it is always checked here no matter what
# SM_PYTHON_LABELS says: otherwise a narrowed override would let the preflight
# pass, the service be stopped, and start-rust then refuse - leaving it down.
# tests/unit/test_restart_rust_server.py guards this against drift.
CUTOVER_PYTHON_LABELS="com.rajeshgoli.session-manager com.claude.session-manager"
# Space-separated extra labels to check on top of the set above.
SM_PYTHON_LABELS="${SM_PYTHON_LABELS:-}"
# Host/port are owned here and forwarded to the cutover, so the endpoint we
# health-check is by construction the one the service was told to listen on.
SM_HOST="${SM_HOST:-127.0.0.1}"
SM_PORT="${SM_PORT:-8420}"
SM_BASE_URL="http://$SM_HOST:$SM_PORT"
SM_SIGN_IDENTIFIER="${SM_SIGN_IDENTIFIER:-com.rajeshgoli.sm-server}"
SM_HEALTH_TIMEOUT="${SM_HEALTH_TIMEOUT:-60}"
SM_PID_SETTLE_SECONDS="${SM_PID_SETTLE_SECONDS:-20}"
SM_UNLOAD_TIMEOUT="${SM_UNLOAD_TIMEOUT:-10}"
SM_ALLOW_SESSION_DROP="${SM_ALLOW_SESSION_DROP:-0}"
DOMAIN="gui/$(id -u)"

usage() {
  cat <<EOF
Usage: scripts/restart-rust-server.sh [options]

Rebuilds, signs, and restarts the Rust Session Manager, then verifies it.
A failing build or a failing signature leaves the running service untouched,
including the binary launchd is registered against.

The service runs from an installed copy ($SM_BINARY),
not from cargo's output, so a build never disturbs the running server. The
first run after adopting this script rewrites the plist to point at the
installed path and therefore needs --allow-plist-change once.

Options:
  --allow-drop N        Tolerate N fewer sessions after the restart (default: 0).
                        Sessions can retire on their own between the before and
                        after samples; raise this only if that is expected.
  --allow-plist-change  Proceed even though restarting would rewrite the launchd
                        plist with different contents. Read the printed diff
                        first: this is how a deployment setting gets dropped.
  --skip-build          Install the currently installed binary again, re-signed.
  --adopt               Migration step for a deployment still registered against
                        cargo's output path. Installs the build already at
                        $SM_CARGO_OUTPUT
                        without rebuilding, and re-registers against the
                        installed path. Building while the live registration
                        still points at cargo's output would overwrite the
                        running service's own executable, so that is refused;
                        run --adopt once, then restart normally.
  -h, --help            Show this help.

Environment overrides: SM_LABEL, SM_BINARY, SM_TARGET_DIR, SM_CARGO_OUTPUT,
SM_CUTOVER, SM_CONFIG, SM_LOCAL_ENV, SM_PLIST, SM_HOST, SM_PORT,
SM_PYTHON_LABELS (extra labels), SM_SIGN_IDENTIFIER, SM_HEALTH_TIMEOUT,
SM_PID_SETTLE_SECONDS, SM_UNLOAD_TIMEOUT, SM_ALLOW_SESSION_DROP, SM_LOCK.

SM_LABEL, SM_BINARY, SM_CONFIG, SM_LOCAL_ENV, SM_PLIST, SM_HOST, and SM_PORT are
forwarded to the cutover script, so both phases act on the same deployment.
EOF
}

SKIP_BUILD=0
ALLOW_PLIST_CHANGE=0
ADOPT=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --adopt)
      ADOPT=1
      shift
      ;;
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

if [[ "$ADOPT" -eq 1 && "$SKIP_BUILD" -eq 1 ]]; then
  echo "--adopt and --skip-build take their source from different places; pick one" >&2
  exit 2
fi
[[ "$ADOPT" -eq 1 ]] && SKIP_BUILD=1

step() { printf '\n==> %s\n' "$1"; }
fail() { echo "ERROR: $1" >&2; exit 1; }

# Relative paths must resolve exactly as rust-service-cutover.sh resolves them
# (against the repo root, not the caller's cwd). Otherwise we would stage and
# install one file while registering another, and still pass every health check.
resolve_path() {
  case "$1" in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s\n' "$REPO_ROOT/$1" ;;
  esac
}

# Follows symlinks and normalises `..`, and works on paths that do not exist yet.
canonical_path() {
  python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$1"
}

SM_BINARY="$(resolve_path "$SM_BINARY")"
SM_CARGO_OUTPUT="$(resolve_path "$SM_CARGO_OUTPUT")"
SM_TARGET_DIR="$(resolve_path "$SM_TARGET_DIR")"
SM_CONFIG="$(resolve_path "$SM_CONFIG")"
SM_PLIST="$(resolve_path "$SM_PLIST")"
SM_CUTOVER="$(resolve_path "$SM_CUTOVER")"
[[ -n "$SM_LOCAL_ENV" ]] && SM_LOCAL_ENV="$(resolve_path "$SM_LOCAL_ENV")"

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

# Staging lives beside the installed binary so the install is an atomic rename.
SM_STAGING="$SM_BINARY.staging.$$"
RENDERED_PLIST=""
SM_LOCK="${SM_LOCK:-${TMPDIR:-/tmp}/sm-restart-$SM_LABEL.lock}"
LOCK_OWNED=0

cleanup() {
  local rc=$?
  rm -f "$SM_STAGING"
  [[ -n "$RENDERED_PLIST" ]] && rm -f "$RENDERED_PLIST" "$RENDERED_PLIST.diff"
  [[ "$LOCK_OWNED" -eq 1 ]] && rm -rf "$SM_LOCK"
  return $rc
}
trap cleanup EXIT

# Two concurrent restarts can both see the job unloaded, after which one installs
# and starts while the other renames its staged binary over the now-live
# registered path - leaving the service running a binary that was replaced
# outside its registration. mkdir is the atomic primitive available here; macOS
# has no flock(1). Held for the whole run, verification included.
acquire_lock() {
  local holder
  if mkdir "$SM_LOCK" 2>/dev/null; then
    LOCK_OWNED=1
    echo $$ > "$SM_LOCK/pid"
    return 0
  fi
  holder="$(cat "$SM_LOCK/pid" 2>/dev/null || true)"
  if [[ -n "$holder" ]] && kill -0 "$holder" 2>/dev/null; then
    fail "another restart of $SM_LABEL is already running (pid $holder).
       Wait for it to finish; concurrent restarts can leave the service running a
       binary that was replaced outside its registration. The running service was
       not touched."
  fi
  # A dead holder is NOT reclaimed automatically. Testing liveness and then
  # removing the directory cannot be made atomic in shell: two invocations can
  # both read the same dead pid, and the second `rm -rf` deletes the lock the
  # first just created, leaving both believing they own it - which is precisely
  # the interleaving this lock exists to stop. Nothing here removes a lock it
  # does not own.
  fail "a restart lock is present at $SM_LOCK but its holder
       (${holder:-unknown}) is not running, so it was probably left behind by a
       restart that was killed. Confirm no restart is in progress, then remove it
       and re-run:
         rm -rf $SM_LOCK
       The running service was not touched."
}

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

job_loaded() {
  launchctl print "$DOMAIN/$SM_LABEL" >/dev/null 2>&1
}

# True when the service is currently set up to run from cargo's output, whether
# that is visible in the loaded registration or only in the plist on disk. The
# two can disagree - an edited plist that has not been reloaded still leaves
# launchd executing the old program - so both are checked, and the loaded job is
# the authoritative one.
registration_runs_cargo_output() {
  local target program
  target="$(canonical_path "$SM_CARGO_OUTPUT")"
  if job_loaded; then
    program="$(launchctl_field program)"
    if [[ -n "$program" && "$(canonical_path "$program")" == "$target" ]]; then
      return 0
    fi
    # The loaded registration is what launchd will actually exec, so it settles
    # the question on its own. Falling through to the plist here would refuse a
    # perfectly safe rebuild whenever an edited plist had not been reloaded, and
    # send the operator to --adopt, which would redeploy an older artifact. A
    # stale plist is the plist-divergence guard's business, not this check's.
    return 1
  fi
  if [[ -f "$SM_PLIST" ]] && plist_runs_cargo_output; then
    return 0
  fi
  return 1
}

# True when any program argument in the live plist resolves to cargo's output.
# Compared canonically rather than as a literal string, for the same reason as
# the SM_BINARY check: an alias would otherwise hide the migration state.
plist_runs_cargo_output() {
  python3 - "$SM_PLIST" "$SM_CARGO_OUTPUT" <<'PY'
import os, re, sys

plist_path, target = sys.argv[1], sys.argv[2]
try:
    with open(plist_path, encoding="utf-8", errors="replace") as handle:
        text = handle.read()
except OSError:
    sys.exit(1)

target = os.path.realpath(target)
for value in re.findall(r"<string>(.*?)</string>", text, re.S):
    value = value.strip()
    if not value or os.pathsep in value:  # skip PATH-style joined values
        continue
    if os.path.realpath(value) == target:
        sys.exit(0)
sys.exit(1)
PY
}

# First "key = value" line only: launchctl repeats `state` for nested entries.
# awk deliberately reads to EOF rather than `exit`ing on the first match: exiting
# early closes the pipe under launchctl, and with `pipefail` that SIGPIPE becomes
# a 141 exit status for the whole function.
launchctl_field() {
  launchctl print "$DOMAIN/$SM_LABEL" 2>/dev/null \
    | awk -v key="$1" -F' = ' \
        '!found && $0 ~ "^[[:space:]]*" key " = " { print $2; found = 1 }'
}

# ---------------------------------------------------------------------------
# Phase 1: everything that can fail without consequence.
# Nothing below this line may touch the running service, and nothing may write
# to $SM_BINARY - the path launchd is registered against.
# ---------------------------------------------------------------------------

step "Taking the restart lock"
acquire_lock
echo "holding $SM_LOCK"

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
# rust-service-cutover.sh stops the service and only then validates start-rust's
# preconditions, so a precondition that fails there leaves the server down.
# Check the same things here, while stopping nothing. The cutover itself is
# checked first: the plist comparison below only runs when a live plist exists,
# so a missing cutover would otherwise not surface until much later.
[[ -x "$SM_CUTOVER" ]] || fail "cutover script not executable: $SM_CUTOVER - the running service was not touched"
[[ -r "$SM_CONFIG" ]] || fail "config not readable: $SM_CONFIG - the running service was not touched"
if [[ -n "$SM_LOCAL_ENV" && ! -r "$SM_LOCAL_ENV" ]]; then
  fail "local env overlay not readable: $SM_LOCAL_ENV - the running service was not touched"
fi
for label in $CUTOVER_PYTHON_LABELS $SM_PYTHON_LABELS; do
  if launchctl print "$DOMAIN/$label" >/dev/null 2>&1; then
    fail "Python service label $label is still loaded; start-rust would refuse to
       start Rust after stopping it. Run '$SM_CUTOVER stop-python' first.
       The running service was not touched."
  fi
done

# Unconditional: even a run that does not build must not leave the service
# registered against cargo's output, or the next ordinary `cargo build` replaces
# the live binary. Compared canonically, because a symlink or a `..` alias would
# otherwise slip past and leave cargo writing the very executable launchd runs.
if [[ "$(canonical_path "$SM_BINARY")" == "$(canonical_path "$SM_CARGO_OUTPUT")" ]]; then
  fail "$SM_BINARY resolves to the same file as cargo's output
       ($SM_CARGO_OUTPUT). The service must not be registered against cargo's
       output path: a build would then write the registered binary directly, and
       a server that exited before the restart would be respawned by KeepAlive
       onto an unverified build. The running service was not touched."
fi

# Same hazard, one step removed: the configuration above may already be correct
# while the *live registration* still runs from cargo's output - which is exactly
# the state a first adoption starts in. Building then would overwrite the
# executable the loaded job is using, before we have re-registered it.
if [[ "$SKIP_BUILD" -eq 0 ]] && registration_runs_cargo_output; then
  fail "the service is still set up to run from cargo's output
       ($SM_CARGO_OUTPUT), according to the loaded job or $SM_PLIST.
       Building now would overwrite the executable the loaded job is using, and a
       server that exited before the restart would be respawned onto it under the
       old registration. Run this once to migrate without building:
         $0 --adopt --allow-plist-change
       then restart normally. The running service was not touched."
fi

# start-rust rewrites the plist, and it does so after the service has been
# stopped, so a destination we cannot write would leave it down.
plist_dir="$(dirname "$SM_PLIST")"
mkdir -p "$plist_dir" 2>/dev/null || true
[[ -d "$plist_dir" && -w "$plist_dir" ]] \
  || fail "cannot write the launchd plist directory $plist_dir; start-rust would fail
       after the service had already been stopped. The running service was not
       touched."
if [[ -e "$SM_PLIST" && ! -w "$SM_PLIST" ]]; then
  fail "launchd plist is not writable: $SM_PLIST; start-rust rewrites it after the
       service has been stopped, so this would leave it down. The running service
       was not touched."
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
    if [[ "$ALLOW_PLIST_CHANGE" -eq 0 ]]; then
      fail "restarting would rewrite $SM_PLIST with different contents (diff above).
       If this is the first run after adopting this script, that diff should be
       the program path moving to the installed binary - re-run with
       --allow-plist-change to accept it. Otherwise pass the missing settings
       (for example SM_LOCAL_ENV) so the rendered plist matches.
       The running service was not touched."
    fi
    echo "WARNING: proceeding with a plist rewrite because --allow-plist-change was given" >&2
  fi
fi
echo "preconditions ok"

if [[ "$ADOPT" -eq 1 ]]; then
  step "Adopting the existing build (--adopt, no rebuild)"
  # Adoption only makes sense while the service still runs from cargo's output.
  # Repeated afterwards it would install whatever stale artifact is left in the
  # target directory - a silent downgrade that every check below would pass.
  if ! registration_runs_cargo_output; then
    fail "--adopt is only for a service still registered against cargo's output.
       This one already runs from $SM_BINARY, so adopting would install whatever
       build happens to be left at $SM_CARGO_OUTPUT, which may be older than what
       is running. Use a normal restart instead.
       The running service was not touched."
  fi
  # Read-only source: the binary the live registration is already running.
  [[ -x "$SM_CARGO_OUTPUT" ]] \
    || fail "nothing to adopt at $SM_CARGO_OUTPUT - the running service was not touched"
  SOURCE_BINARY="$SM_CARGO_OUTPUT"
elif [[ "$SKIP_BUILD" -eq 1 ]]; then
  step "Skipping build (--skip-build)"
  [[ -x "$SM_BINARY" ]] || fail "no installed binary at $SM_BINARY to re-deploy - the running service was not touched"
  SOURCE_BINARY="$SM_BINARY"
else
  step "Building sm-server (service still running)"
  cargo build --release -p sm-server --target-dir "$SM_TARGET_DIR" \
    || fail "build failed - the running service was not touched"
  [[ -x "$SM_CARGO_OUTPUT" ]] \
    || fail "build reported success but produced no executable at $SM_CARGO_OUTPUT - the running service was not touched"
  SOURCE_BINARY="$SM_CARGO_OUTPUT"
fi

step "Staging and signing"
mkdir -p "$(dirname "$SM_BINARY")" \
  || fail "could not create $(dirname "$SM_BINARY") - the running service was not touched"
cp -p "$SOURCE_BINARY" "$SM_STAGING" \
  || fail "could not stage $SOURCE_BINARY - the running service was not touched"
# A stable identifier keeps the signing identity from churning per build. Ad-hoc
# signing otherwise derives the identifier from the Mach-O UUID (and the linker
# derives it from cargo's deps/ filename), so it changed on every rebuild.
codesign --force --sign - --identifier "$SM_SIGN_IDENTIFIER" "$SM_STAGING" \
  || fail "codesign failed - the running service was not touched"

step "Verifying signature"
codesign --verify --strict "$SM_STAGING" \
  || fail "signature verification failed - the running service was not touched"
echo "signature ok: $(codesign -dvvv "$SM_STAGING" 2>&1 | awk -F= '/^Identifier=/{print $2}')"

step "Validating the configuration with the new binary"
# Readability is not validity. A malformed config.yaml or local-env overlay would
# let the old process keep running on what it parsed at startup, and only fail
# once the new one starts - after the service has been stopped, which KeepAlive
# turns into a crash loop until the health timeout. --check-config runs the same
# loader the server uses and exits before binding or touching any state.
# Host and port are included so the exact address launchd will be told to use is
# parsed by the same code that will have to bind it. Validation happens before
# the bind, so it is safe while the old server still holds the port.
check_config_args=(--check-config --config "$SM_CONFIG" --host "$SM_HOST" --port "$SM_PORT")
[[ -n "$SM_LOCAL_ENV" ]] && check_config_args+=(--local-env "$SM_LOCAL_ENV")
if "$SM_STAGING" --help 2>&1 | grep -q -- '--check-config'; then
  "$SM_STAGING" "${check_config_args[@]}" \
    || fail "the new binary rejected the configuration or the listen address; see the
       error above and fix $SM_CONFIG${SM_LOCAL_ENV:+ / $SM_LOCAL_ENV} or the
       host/port ($SM_HOST:$SM_PORT) before restarting.
       The running service was not touched."
else
  # --skip-build/--adopt can be deploying a build from before the flag existed.
  echo "WARNING: this binary has no --check-config, so the configuration was not validated" >&2
fi

# ---------------------------------------------------------------------------
# Phase 2: from here on the service is affected.
# ---------------------------------------------------------------------------

step "Stopping the service (bootout)"
"$SM_CUTOVER" stop-rust "${cutover_args[@]}" \
  || fail "could not stop $SM_LABEL - the installed binary is untouched"

step "Confirming the job is really unloaded"
# stop_rust runs `launchctl bootout ... || true` and reports success either way,
# so a job that refused to unload would otherwise let us install while KeepAlive
# can still respawn it.
unload_deadline=$((SECONDS + SM_UNLOAD_TIMEOUT))
while job_loaded; do
  if (( SECONDS >= unload_deadline )); then
    fail "$SM_LABEL is still loaded ${SM_UNLOAD_TIMEOUT}s after stop-rust; refusing to
       install while launchd can still respawn it. The installed binary is
       untouched, so the service is still running its previous build."
  fi
  sleep 1
done
echo "job is unloaded"

step "Installing the verified build"
# Nothing can exec the registered path right now, so this is the one safe moment
# to replace it. Atomic rename, and the only write to $SM_BINARY in the script.
mv -f "$SM_STAGING" "$SM_BINARY" \
  || fail "could not install the new binary at $SM_BINARY; the previous build is
       still in place - re-run '$SM_CUTOVER start-rust' to bring the service back"

step "Starting the service (bootstrap -> kickstart)"
"$SM_CUTOVER" start-rust "${cutover_args[@]}" \
  || fail "start failed - see output above; service may be down"

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
