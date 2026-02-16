#!/bin/bash
# Wrapper script for session manager with health monitoring
# Used by launchd to run and monitor the service

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
VENV_PYTHON="$SCRIPT_DIR/venv/bin/python"
LOG_FILE="/tmp/session-manager.log"
HEALTH_URL="http://127.0.0.1:8420/health"
HEALTH_CHECK_INTERVAL=30
HEALTH_CHECK_TIMEOUT=5
MAX_UNHEALTHY_COUNT=3

cd "$SCRIPT_DIR"

# Start the server in background
$VENV_PYTHON -m src.main >> "$LOG_FILE" 2>&1 &
SERVER_PID=$!

echo "$(date): Started session manager with PID $SERVER_PID" >> "$LOG_FILE"

# Wait for server to be ready
sleep 3

unhealthy_count=0

# Health check loop
while true; do
    sleep $HEALTH_CHECK_INTERVAL

    # Check if process is still running
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "$(date): Session manager process died unexpectedly" >> "$LOG_FILE"
        exit 1  # launchd will restart us
    fi

    # Check health endpoint
    if curl -sf --connect-timeout $HEALTH_CHECK_TIMEOUT --max-time $HEALTH_CHECK_TIMEOUT "$HEALTH_URL" > /dev/null 2>&1; then
        unhealthy_count=0
    else
        unhealthy_count=$((unhealthy_count + 1))
        echo "$(date): Health check failed ($unhealthy_count/$MAX_UNHEALTHY_COUNT)" >> "$LOG_FILE"

        if [ $unhealthy_count -ge $MAX_UNHEALTHY_COUNT ]; then
            echo "$(date): Max unhealthy count reached, killing frozen process" >> "$LOG_FILE"
            kill -9 $SERVER_PID 2>/dev/null || true
            sleep 1
            exit 1  # launchd will restart us
        fi
    fi
done
