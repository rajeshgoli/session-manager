#!/bin/bash
# Install/uninstall session manager as a launchd service

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PLIST_NAME="com.claude.session-manager.plist"
PLIST_SRC="$SCRIPT_DIR/$PLIST_NAME"
PLIST_DST="$HOME/Library/LaunchAgents/$PLIST_NAME"
SERVICE_NAME="com.claude.session-manager"

usage() {
    echo "Usage: $0 [install|uninstall|status|restart]"
    echo ""
    echo "Commands:"
    echo "  install   - Install and start the session manager service"
    echo "  uninstall - Stop and remove the session manager service"
    echo "  status    - Check service status"
    echo "  restart   - Restart the service"
    exit 1
}

install_service() {
    echo "Installing session manager service..."

    # Make wrapper executable
    chmod +x "$SCRIPT_DIR/session-manager-wrapper.sh"

    # Kill any existing process on port 8420
    if lsof -i :8420 -t > /dev/null 2>&1; then
        echo "Killing existing process on port 8420..."
        kill -9 $(lsof -i :8420 -t) 2>/dev/null || true
        sleep 1
    fi

    # Create LaunchAgents directory if needed
    mkdir -p "$HOME/Library/LaunchAgents"

    # Copy plist
    cp "$PLIST_SRC" "$PLIST_DST"

    # Load the service
    launchctl load "$PLIST_DST"

    echo "Service installed and started."
    echo "Logs: /tmp/session-manager.log"
    echo ""
    sleep 2
    status_service
}

uninstall_service() {
    echo "Uninstalling session manager service..."

    # Unload if loaded
    if launchctl list | grep -q "$SERVICE_NAME"; then
        launchctl unload "$PLIST_DST" 2>/dev/null || true
    fi

    # Remove plist
    rm -f "$PLIST_DST"

    # Kill any remaining processes
    pkill -9 -f "session-manager-wrapper" 2>/dev/null || true
    pkill -9 -f "src.main" 2>/dev/null || true

    echo "Service uninstalled."
}

status_service() {
    echo "=== Service Status ==="

    if launchctl list | grep -q "$SERVICE_NAME"; then
        echo "launchd: loaded"
        launchctl list | grep "$SERVICE_NAME"
    else
        echo "launchd: not loaded"
    fi

    echo ""
    echo "=== Process Status ==="
    if pgrep -f "src.main" > /dev/null; then
        ps aux | grep "src.main" | grep -v grep
    else
        echo "No session manager process running"
    fi

    echo ""
    echo "=== Health Check ==="
    if curl -sf --connect-timeout 2 --max-time 2 http://127.0.0.1:8420/health 2>/dev/null; then
        echo ""
        echo "Health: OK"
    else
        echo "Health: FAILED (not responding)"
    fi
}

restart_service() {
    echo "Restarting session manager service..."

    if launchctl list | grep -q "$SERVICE_NAME"; then
        launchctl unload "$PLIST_DST"
        sleep 1
        launchctl load "$PLIST_DST"
        echo "Service restarted."
        sleep 2
        status_service
    else
        echo "Service not installed. Run '$0 install' first."
        exit 1
    fi
}

case "${1:-}" in
    install)
        install_service
        ;;
    uninstall)
        uninstall_service
        ;;
    status)
        status_service
        ;;
    restart)
        restart_service
        ;;
    *)
        usage
        ;;
esac
