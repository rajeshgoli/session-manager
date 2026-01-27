#!/bin/bash

# Kill existing server on port 8420
echo "Checking for existing server on port 8420..."
PID=$(lsof -ti:8420)
if [ ! -z "$PID" ]; then
    echo "Killing existing server (PID: $PID)..."
    kill -9 $PID
    sleep 1
fi

# Create logs directory if it doesn't exist
mkdir -p logs

# Generate log filename with current date-time
LOGFILE="logs/log-$(date +%Y%m%d-%H%M%S).log"

# Activate venv and run server in background
echo "Starting server..."
echo "Logs will be written to: $LOGFILE"

source venv/bin/activate && nohup python -m src.main > "$LOGFILE" 2>&1 &

# Get the PID of the background process
SERVER_PID=$!
echo "Server started with PID: $SERVER_PID"
echo "To view logs: tail -f $LOGFILE"
echo "To stop server: kill $SERVER_PID"
