#!/usr/bin/env bash
# Rust Session Manager launchd cutover helper.
#
# This script intentionally avoids arbitrary process killing. It only unloads
# known Session Manager launchd labels and refuses to start Rust when the target
# port is already owned by any process.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

COMMAND="${1:-}"
if [[ -z "$COMMAND" ]]; then
  COMMAND="help"
else
  shift
fi

HOST="127.0.0.1"
PORT="8420"
CONFIG="$REPO_ROOT/config.yaml"
LOCAL_ENV=""
BINARY="$REPO_ROOT/target/release/sm-server"
RUST_LABEL="com.rajeshgoli.session-manager-rust"
PYTHON_LABELS=("com.rajeshgoli.session-manager" "com.claude.session-manager")
PLIST_DST="$HOME/Library/LaunchAgents/$RUST_LABEL.plist"
LOG_DIR="$REPO_ROOT/logs"
DOMAIN="gui/$(id -u)"

_resolve_path() {
  case "$1" in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s\n' "$REPO_ROOT/$1" ;;
  esac
}

xml_text() {
  local value="$1"
  value="${value//&/&amp;}"
  value="${value//</&lt;}"
  value="${value//>/&gt;}"
  printf '%s' "$value"
}

usage() {
  cat <<EOF
Usage: scripts/rust-service-cutover.sh <command> [options]

Commands:
  plan             Show Rust service command, launchd paths, port owners, and blockers.
  render-plist     Print the Rust launchd plist to stdout.
  write-plist      Write the Rust launchd plist, but do not load it.
  stop-python      Disable/unload known Python Session Manager launchd labels, then verify the port is free.
  start-rust       Write/load the Rust launchd plist. Refuses if the port is occupied or Python is loaded.
  stop-rust        Unload the Rust launchd label.
  restart-rust     stop-rust then start-rust.
  rollback-python  Stop Rust and bootstrap the existing Python launchd plist if present.
  status           Show launchd, port, and Rust health status.

Options:
  --host HOST          Listen host for Rust (default: $HOST)
  --port PORT          Listen port for Rust (default: $PORT)
  --config PATH        Rust config path (default: config.yaml)
  --local-env PATH     Optional local env overlay path
  --binary PATH        Rust sm-server binary (default: target/release/sm-server)
  --label LABEL        Rust launchd label (default: $RUST_LABEL)
  --plist PATH         Rust plist destination (default: ~/Library/LaunchAgents/<label>.plist)
  --log-dir PATH       Rust launchd stdout/stderr directory (default: logs/)

First canary shape:
  cargo build -p sm-server --release
  ./scripts/rust-service-cutover.sh plan
  ./scripts/rust-service-cutover.sh stop-python
  ./venv/bin/python -m scripts.rust_migration.final_backup --config config.yaml --output-dir .local/rust-final-backup-\$(date -u +%Y%m%dT%H%M%SZ) --ledger .local/rust-cutover-ledger.jsonl --record-ledger --execute --fail-on-blockers
  ./scripts/rust-service-cutover.sh start-rust
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --host)
      HOST="${2:?missing --host value}"
      shift 2
      ;;
    --port)
      PORT="${2:?missing --port value}"
      shift 2
      ;;
    --config)
      CONFIG="$(_resolve_path "${2:?missing --config value}")"
      shift 2
      ;;
    --local-env)
      LOCAL_ENV="$(_resolve_path "${2:?missing --local-env value}")"
      shift 2
      ;;
    --binary)
      BINARY="$(_resolve_path "${2:?missing --binary value}")"
      shift 2
      ;;
    --label)
      RUST_LABEL="${2:?missing --label value}"
      PLIST_DST="$HOME/Library/LaunchAgents/$RUST_LABEL.plist"
      shift 2
      ;;
    --plist)
      PLIST_DST="$(_resolve_path "${2:?missing --plist value}")"
      shift 2
      ;;
    --log-dir)
      LOG_DIR="$(_resolve_path "${2:?missing --log-dir value}")"
      shift 2
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

rust_command_args() {
  printf '%s\n' "$BINARY"
  printf '%s\n' "--host"
  printf '%s\n' "$HOST"
  printf '%s\n' "--port"
  printf '%s\n' "$PORT"
  printf '%s\n' "--config"
  printf '%s\n' "$CONFIG"
  if [[ -n "$LOCAL_ENV" ]]; then
    printf '%s\n' "--local-env"
    printf '%s\n' "$LOCAL_ENV"
  fi
}

rust_command_text() {
  local parts=()
  while IFS= read -r part; do
    parts+=("$part")
  done < <(rust_command_args)
  printf '%q ' "${parts[@]}"
  printf '\n'
}

port_owner_pids() {
  lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null | sort -u || true
}

launchctl_print_label() {
  local label="$1"
  launchctl print "$DOMAIN/$label" >/dev/null 2>&1
}

print_port_owners() {
  local pids
  pids="$(port_owner_pids)"
  if [[ -z "$pids" ]]; then
    echo "port_$PORT: free"
    return
  fi
  echo "port_$PORT: occupied"
  while IFS= read -r pid; do
    [[ -z "$pid" ]] && continue
    ps -p "$pid" -o pid=,ppid=,command= || true
  done <<< "$pids"
}

collect_blockers() {
  local blockers=()
  if [[ ! -x "$BINARY" ]]; then
    blockers+=("rust_binary_not_executable: $BINARY")
  fi
  if [[ ! -r "$CONFIG" ]]; then
    blockers+=("config_not_readable: $CONFIG")
  fi
  if [[ -n "$LOCAL_ENV" && ! -r "$LOCAL_ENV" ]]; then
    blockers+=("local_env_not_readable: $LOCAL_ENV")
  fi
  local pids
  pids="$(port_owner_pids)"
  if [[ -n "$pids" ]]; then
    blockers+=("port_in_use: $PORT is already listening")
  fi
  if [[ "${#blockers[@]}" -eq 0 ]]; then
    return 0
  fi
  printf '%s\n' "${blockers[@]}"
}

loaded_python_labels() {
  for label in "${PYTHON_LABELS[@]}"; do
    if launchctl_print_label "$label"; then
      printf '%s\n' "$label"
    fi
  done
}

is_label_disabled() {
  local label="$1"
  launchctl print-disabled "$DOMAIN" 2>/dev/null \
    | grep -F "\"$label\" => disabled" >/dev/null
}

disable_python_label() {
  local label="$1"
  if ! launchctl disable "$DOMAIN/$label"; then
    echo "failed to disable $label in $DOMAIN" >&2
    exit 1
  fi
  if ! is_label_disabled "$label"; then
    echo "failed to verify disabled override for $label in $DOMAIN" >&2
    exit 1
  fi
}

require_no_python_labels() {
  local labels
  labels="$(loaded_python_labels)"
  if [[ -n "$labels" ]]; then
    echo "known Python Session Manager launchd labels are still loaded:" >&2
    while IFS= read -r label; do
      [[ -z "$label" ]] && continue
      echo "  - $label" >&2
    done <<< "$labels"
    echo "run: $0 stop-python" >&2
    exit 1
  fi
}

print_plan() {
  echo "Rust Session Manager service cutover plan"
  echo "repo_root: $REPO_ROOT"
  echo "launch_domain: $DOMAIN"
  echo "rust_label: $RUST_LABEL"
  echo "rust_plist: $PLIST_DST"
  echo "rust_binary: $BINARY"
  echo "config: $CONFIG"
  echo "local_env: ${LOCAL_ENV:-<none>}"
  echo "listen: $HOST:$PORT"
  echo "stdout_log: $LOG_DIR/rust-launchd.out.log"
  echo "stderr_log: $LOG_DIR/rust-launchd.err.log"
  echo "rust_command: $(rust_command_text)"
  echo
  echo "Known Python labels:"
  for label in "${PYTHON_LABELS[@]}"; do
    if launchctl_print_label "$label"; then
      echo "  loaded: $label"
    else
      echo "  not loaded: $label"
    fi
  done
  echo
  print_port_owners
  echo
  local blockers
  blockers="$(collect_blockers)"
  if [[ -z "$blockers" ]]; then
    echo "blockers: 0"
  else
    echo "blockers:"
    while IFS= read -r blocker; do
      [[ -z "$blocker" ]] && continue
      echo "  - $blocker"
    done <<< "$blockers"
  fi
}

render_plist() {
  cat <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$(xml_text "$RUST_LABEL")</string>

    <key>ProgramArguments</key>
    <array>
$(while IFS= read -r arg; do printf '        <string>%s</string>\n' "$(xml_text "$arg")"; done < <(rust_command_args))
    </array>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <true/>

    <key>WorkingDirectory</key>
    <string>$(xml_text "$REPO_ROOT")</string>

    <key>StandardOutPath</key>
    <string>$(xml_text "$LOG_DIR/rust-launchd.out.log")</string>

    <key>StandardErrorPath</key>
    <string>$(xml_text "$LOG_DIR/rust-launchd.err.log")</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$(xml_text "$REPO_ROOT/target/release:$REPO_ROOT/target/debug:$REPO_ROOT/venv/bin:/Users/rajesh/.cargo/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")</string>
    </dict>

    <key>ThrottleInterval</key>
    <integer>10</integer>
</dict>
</plist>
EOF
}

write_plist() {
  mkdir -p "$(dirname "$PLIST_DST")" "$LOG_DIR"
  render_plist > "$PLIST_DST"
  chmod 644 "$PLIST_DST"
  echo "wrote $PLIST_DST"
}

require_no_blockers() {
  local blockers
  blockers="$(collect_blockers)"
  if [[ -n "$blockers" ]]; then
    print_plan >&2
    exit 1
  fi
}

stop_rust() {
  if launchctl_print_label "$RUST_LABEL"; then
    launchctl bootout "$DOMAIN/$RUST_LABEL" || true
    echo "stopped $RUST_LABEL"
  else
    echo "$RUST_LABEL is not loaded"
  fi
}

start_rust() {
  require_no_blockers
  require_no_python_labels
  write_plist
  if launchctl_print_label "$RUST_LABEL"; then
    launchctl bootout "$DOMAIN/$RUST_LABEL" || true
  fi
  launchctl bootstrap "$DOMAIN" "$PLIST_DST"
  launchctl kickstart -k "$DOMAIN/$RUST_LABEL"
  echo "started $RUST_LABEL"
}

stop_python() {
  for label in "${PYTHON_LABELS[@]}"; do
    disable_python_label "$label"
    if launchctl_print_label "$label"; then
      launchctl bootout "$DOMAIN/$label" || true
      echo "disabled and stopped $label"
    else
      echo "disabled $label"
    fi
  done
  sleep 1
  local pids
  pids="$(port_owner_pids)"
  if [[ -n "$pids" ]]; then
    echo "port $PORT is still occupied after unloading known Python labels" >&2
    print_port_owners >&2
    exit 1
  fi
  echo "port $PORT is free"
}

rollback_python() {
  stop_rust
  local python_plist=""
  for candidate in \
    "$HOME/Library/LaunchAgents/com.rajeshgoli.session-manager.plist" \
    "$HOME/Library/LaunchAgents/com.claude.session-manager.plist"; do
    if [[ -f "$candidate" ]]; then
      python_plist="$candidate"
      break
    fi
  done
  if [[ -z "$python_plist" ]]; then
    echo "no Python Session Manager plist found in ~/Library/LaunchAgents" >&2
    exit 1
  fi
  local label
  label="$(/usr/libexec/PlistBuddy -c 'Print :Label' "$python_plist")"
  launchctl enable "$DOMAIN/$label" 2>/dev/null || true
  launchctl bootstrap "$DOMAIN" "$python_plist" 2>/dev/null || true
  launchctl kickstart -k "$DOMAIN/$label" || true
  echo "bootstrapped Python service from $python_plist"
}

status() {
  echo "Rust label: $RUST_LABEL"
  if launchctl_print_label "$RUST_LABEL"; then
    launchctl print "$DOMAIN/$RUST_LABEL" | sed -n '1,60p'
  else
    echo "not loaded"
  fi
  echo
  print_port_owners
  echo
  echo "Rust health:"
  if curl -sf --connect-timeout 2 --max-time 2 "http://$HOST:$PORT/health"; then
    echo
    echo "health: ok"
  else
    echo "health: failed"
  fi
}

case "$COMMAND" in
  help|-h|--help)
    usage
    ;;
  plan)
    print_plan
    ;;
  render-plist)
    render_plist
    ;;
  write-plist)
    write_plist
    ;;
  stop-python)
    stop_python
    ;;
  start-rust)
    start_rust
    ;;
  stop-rust)
    stop_rust
    ;;
  restart-rust)
    stop_rust
    start_rust
    ;;
  rollback-python)
    rollback_python
    ;;
  status)
    status
    ;;
  *)
    echo "Unknown command: $COMMAND" >&2
    usage >&2
    exit 2
    ;;
esac
