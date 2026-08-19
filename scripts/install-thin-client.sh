#!/bin/bash
# install-thin-client — make this laptop a thin client for studio's
# session-manager (issue #1345). Idempotent.
#
#   A  install ~/bin/sm SSH shim (command-agnostic passthrough to studio's sm)
#   B  tear down the old node_agent launchd job + plist
#   C1 install the self-healing local API forward launchd helper
#
# Usage: scripts/install-thin-client.sh [install|uninstall|status]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
UID_NUM="$(id -u)"

SHIM_SRC="$SCRIPT_DIR/sm-thin-client.sh"
SHIM_DST="$HOME/bin/sm"

NODE_LABEL="com.rajeshgoli.session-manager-node-agent.macbook"
NODE_PLIST="$HOME/Library/LaunchAgents/$NODE_LABEL.plist"

FWD_LABEL="com.rajeshgoli.session-manager-api-forward.macbook"
FWD_PLIST_SRC="$SCRIPT_DIR/$FWD_LABEL.plist"
FWD_PLIST_DST="$HOME/Library/LaunchAgents/$FWD_LABEL.plist"
FWD_SCRIPT_SRC="$SCRIPT_DIR/session-manager-api-forward.sh"
# Installed copy must live OUTSIDE ~/Desktop (TCC-protected — launchd agents
# without Full Disk Access get "can't open input file" / exit 127 there).
FWD_SCRIPT_DST="$HOME/bin/session-manager-api-forward"

install_shim() {          # A
  echo "== A: installing sm SSH shim -> $SHIM_DST"
  mkdir -p "$HOME/bin"
  install -m 0755 "$SHIM_SRC" "$SHIM_DST"
  echo "   installed. (ensure ~/bin precedes the venv on PATH)"
}

teardown_node_agent() {   # B
  echo "== B: tearing down node_agent"
  launchctl bootout "gui/$UID_NUM/$NODE_LABEL" 2>/dev/null || true
  rm -f "$NODE_PLIST"
  # Stray in-repo copies from the old node-agent setup.
  rm -f "$SCRIPT_DIR/$NODE_LABEL.plist" \
        "$SCRIPT_DIR/session-manager-node-agent-wrapper.sh"
  # Optional: retire a leftover server-anchor tmux if one lingers.
  tmux kill-session -t __sm_server_anchor 2>/dev/null || true
  echo "   node_agent removed (SM_API_URL / client.yaml left in place)."
}

install_forward() {       # C1
  echo "== C1: installing self-healing API forward"
  mkdir -p "$HOME/bin"
  install -m 0755 "$FWD_SCRIPT_SRC" "$FWD_SCRIPT_DST"
  cp "$FWD_PLIST_SRC" "$FWD_PLIST_DST"
  launchctl bootout "gui/$UID_NUM/$FWD_LABEL" 2>/dev/null || true
  launchctl bootstrap "gui/$UID_NUM" "$FWD_PLIST_DST"
  launchctl enable "gui/$UID_NUM/$FWD_LABEL" 2>/dev/null || true
  echo "   forward loaded. Logs: /tmp/session-manager-api-forward-macbook.log"
}

status() {
  echo "== status =="
  echo "-- shim ($SHIM_DST):"
  if [ -x "$SHIM_DST" ]; then echo "   present"; else echo "   MISSING"; fi
  echo "-- node_agent ($NODE_LABEL):"
  launchctl print "gui/$UID_NUM/$NODE_LABEL" >/dev/null 2>&1 \
    && echo "   STILL LOADED (unexpected)" || echo "   gone"
  echo "-- api forward ($FWD_LABEL):"
  launchctl print "gui/$UID_NUM/$FWD_LABEL" >/dev/null 2>&1 \
    && echo "   loaded" || echo "   not loaded"
  echo "-- health via forward:"
  if curl -fsS --max-time 5 http://127.0.0.1:8420/health >/dev/null 2>&1; then
    echo "   127.0.0.1:8420/health OK"
  else
    echo "   127.0.0.1:8420/health unreachable (forward may still be connecting)"
  fi
}

uninstall() {
  echo "== uninstall (C1 forward + shim) =="
  launchctl bootout "gui/$UID_NUM/$FWD_LABEL" 2>/dev/null || true
  rm -f "$FWD_PLIST_DST" "$FWD_SCRIPT_DST" "$SHIM_DST"
  echo "   removed forward + shim (node_agent stays torn down)."
}

case "${1:-install}" in
  install)   install_shim; teardown_node_agent; install_forward; echo; status ;;
  uninstall) uninstall ;;
  status)    status ;;
  *) echo "Usage: $0 [install|uninstall|status]" >&2; exit 1 ;;
esac
