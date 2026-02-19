#!/bin/bash
# Setup script for Claude Session Manager

set -e

echo "Setting up Claude Session Manager..."

# Check for Python 3.11+
if ! command -v python3 &> /dev/null; then
    echo "Error: Python 3 is required but not installed."
    exit 1
fi

PYTHON_VERSION=$(python3 -c 'import sys; print(f"{sys.version_info.major}.{sys.version_info.minor}")')
REQUIRED_VERSION="3.11"

if [ "$(printf '%s\n' "$REQUIRED_VERSION" "$PYTHON_VERSION" | sort -V | head -n1)" != "$REQUIRED_VERSION" ]; then
    echo "Error: Python $REQUIRED_VERSION or higher is required (found $PYTHON_VERSION)"
    exit 1
fi

# Check for tmux
if ! command -v tmux &> /dev/null; then
    echo "Error: tmux is required but not installed."
    echo "Install with: brew install tmux"
    exit 1
fi

# Create virtual environment
if [ ! -d "venv" ]; then
    echo "Creating virtual environment..."
    python3 -m venv venv
fi

# Activate virtual environment
source venv/bin/activate

# Install dependencies
echo "Installing dependencies..."
pip install --upgrade pip
pip install -r requirements.txt

# Create config file if it doesn't exist
if [ ! -f "config.yaml" ]; then
    echo "Creating config.yaml from template..."
    cp config.yaml.example config.yaml
    echo "Please edit config.yaml with your Telegram bot token."
fi

# Create log directory
mkdir -p /tmp/claude-sessions

# Install default dispatch templates (only if not already present)
if [ ! -f "$HOME/.sm/dispatch_templates.yaml" ]; then
    echo "Installing default dispatch templates..."
    sm setup 2>/dev/null || python -m src.cli.main setup 2>/dev/null || true
fi

echo ""
echo "Setup complete!"
echo ""
echo "Next steps:"
echo "1. Edit config.yaml with your Telegram bot token"
echo "2. Run: source venv/bin/activate"
echo "3. Run: python -m src.main"
echo ""
echo "Or install as a package:"
echo "  pip install -e ."
echo "  session-manager"
