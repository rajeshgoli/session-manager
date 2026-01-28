# Claude Session Manager

A local macOS server that manages Claude Code sessions in tmux, with bidirectional communication via Telegram and Email.

## Features

- **Session Management**: Create, list, kill Claude Code sessions running in tmux
- **Telegram Bot**: Control sessions via Telegram commands, reply to threads to send input
- **Email Notifications**: Reuses existing email harness for urgent notifications
- **Output Monitoring**: Detects permission prompts, errors, and idle sessions
- **Terminal Integration**: Open sessions in Terminal.app windows

## Prerequisites

- macOS
- Python 3.11+
- tmux (`brew install tmux`)
- Claude Code CLI installed

## Quick Setup

```bash
# Clone and enter directory
cd claude-session-manager

# Run setup script
chmod +x setup.sh
./setup.sh

# Edit configuration
vim config.yaml

# Start the server
source venv/bin/activate
python -m src.main
```

## Setting Up the Telegram Bot

1. **Create a bot with BotFather**:
   - Open Telegram and search for `@BotFather`
   - Send `/newbot`
   - Choose a name (e.g., "Claude Session Manager")
   - Choose a username (e.g., "my_claude_sessions_bot")
   - Copy the token provided

2. **Get your chat ID**:
   - Send a message to your new bot
   - Visit `https://api.telegram.org/bot<YOUR_TOKEN>/getUpdates`
   - Find your chat ID in the response

3. **Configure**:
   ```yaml
   telegram:
     token: "123456789:ABCdefGHIjklMNOpqrsTUVwxyz"
     allowed_chat_ids:
       - 123456789  # Your chat ID
   ```

## Usage

### Telegram Commands

| Command | Description |
|---------|-------------|
| `/new [path]` | Create new Claude session in directory |
| `/list` | List active sessions |
| `/status <id>` | Get session status |
| `/kill <id>` | Kill a session |
| `/open <id>` | Open session in Terminal.app |
| `/help` | Show help message |

**Reply to a session message** to send input to Claude.

### API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/sessions` | POST | Create new session |
| `/sessions` | GET | List all sessions |
| `/sessions/{id}` | GET | Get session details |
| `/sessions/{id}/input` | POST | Send input to session |
| `/sessions/{id}/key` | POST | Send key (y/n) to session |
| `/sessions/{id}` | DELETE | Kill session |
| `/sessions/{id}/open` | POST | Open in Terminal.app |
| `/sessions/{id}/output` | GET | Capture recent output |
| `/notify` | POST | Send notification |

### Example API Usage

```bash
# Create a session
curl -X POST http://localhost:8420/sessions \
  -H "Content-Type: application/json" \
  -d '{"working_dir": "~/projects/myapp"}'

# List sessions
curl http://localhost:8420/sessions

# Send input
curl -X POST http://localhost:8420/sessions/abc123/input \
  -H "Content-Type: application/json" \
  -d '{"text": "Fix the bug in login.py"}'

# Send permission response
curl -X POST http://localhost:8420/sessions/abc123/key \
  -H "Content-Type: application/json" \
  -d '{"key": "y"}'

# Request email notification (from Claude hook)
curl -X POST http://localhost:8420/notify \
  -H "Content-Type: application/json" \
  -d '{"message": "Task complete!", "channel": "email", "urgent": true}'
```

## SM CLI - Multi-Agent Coordination

The `sm` CLI tool enables Claude sessions to coordinate with each other. It's automatically available inside sessions managed by the Session Manager.

### Commands

| Command | Description |
|---------|-------------|
| `sm name <name>` | Set friendly name for current session |
| `sm me` | Show current session info |
| `sm who` | List other sessions in same workspace |
| `sm what <session-id> [--deep]` | Get AI summary of what a session is doing |
| `sm others [--repo]` | List others with summaries |
| `sm all [--summaries]` | List all sessions system-wide across all directories |
| `sm alone` | Check if you're the only agent (exit code 0=alone, 1=others) |
| `sm task "<description>"` | Register what you're working on |
| `sm status` | Full status: you + others + lock file |
| `sm subagents <session-id>` | List subagents spawned by a session |
| `sm send <session-id> "<text>"` | Send input to any session |

### Subagent Tracking

When Claude spawns subagents (e.g., "As EM, implement epic #987"), the Session Manager can track them automatically via Claude Code hooks.

**To enable subagent tracking in your project**, add this to your project's `.claude/settings.json`:

```json
{
  "hooks": {
    "SubagentStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "sm subagent-start"
          }
        ]
      }
    ],
    "SubagentStop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "sm subagent-stop"
          }
        ]
      }
    ]
  }
}
```

**Note**: The `.claude/settings.json` file in this repository is for development/testing of the Session Manager itself. Each project that wants subagent tracking needs its own hooks configuration.

### Example Workflows

**Coordinating with other agents:**
```bash
# Check if others are working in same workspace
$ sm who
engineer-db (a1b2c3d4) | running | 5min ago

# See what they're doing
$ sm what a1b2c3d4
Working on database migration script for users table.

# List all sessions across all projects
$ sm all
office-automate (fc7d7dbc) | idle | ~/Desktop/automation/office-automate
engineer-db (a1b2c3d4) | running | ~/projects/myapp

# Register your task to avoid conflicts
$ sm task "Implementing user authentication API"

# Send input to another agent
$ sm send a1b2c3d4 "Database migration complete, you can proceed"
Input sent to engineer-db (a1b2c3d4)
```

**Tracking subagents:**
```bash
# List subagents spawned by a session
$ sm subagents 1749a2fe
em-epic987 (1749a2fe) subagents:
  → engineer (abc123) | running | 3min ago
  ✓ architect (def456) | completed | 10min ago

# Get deep summary with subagent activity
$ sm what 1749a2fe --deep
EM orchestrating epic #987. Currently coordinating Engineer on #984.

Subagents:
  → engineer (abc123) | running | 3min ago
  ✓ architect (def456) | completed | 10min ago
     Designed pivot detection architecture
```

## Email Integration

The session manager reuses the existing email harness from `../claude-email-automation/`. Ensure that directory contains:
- `email.yaml` - SMTP configuration
- `imap.yaml` - IMAP configuration

Claude can request email notifications via the `/notify` endpoint with `"channel": "email"`.

## Architecture

```
┌─────────────────┐     ┌──────────────────┐
│  Telegram Bot   │────▶│  Session Manager │
└─────────────────┘     └────────┬─────────┘
                                 │
┌─────────────────┐              │
│   FastAPI       │◀─────────────┤
│   Server        │              │
└─────────────────┘              ▼
                        ┌──────────────────┐
┌─────────────────┐     │  tmux Controller │
│ Output Monitor  │────▶│                  │
└─────────────────┘     └────────┬─────────┘
        │                        │
        ▼                        ▼
┌─────────────────┐     ┌──────────────────┐
│   Notifier      │     │  tmux sessions   │
│ (Telegram/Email)│     │  (Claude Code)   │
└─────────────────┘     └──────────────────┘
```

## Configuration Reference

```yaml
server:
  host: "127.0.0.1"      # Bind address
  port: 8420             # Server port

paths:
  log_dir: "/tmp/claude-sessions"
  state_file: "/tmp/claude-sessions/sessions.json"

monitor:
  idle_timeout: 300      # Seconds before idle notification
  poll_interval: 1.0     # Output check frequency

telegram:
  token: "BOT_TOKEN"     # From @BotFather
  allowed_chat_ids: []   # Empty = allow all

email:
  smtp_config: ""        # Path to email.yaml (optional)
  imap_config: ""        # Path to imap.yaml (optional)
```

## Troubleshooting

**Bot not responding**: Verify the token is correct and the bot is started.

**Session not created**: Check that tmux is installed and Claude Code CLI is available.

**No notifications**: Ensure the session has a Telegram chat ID associated (create via `/new`).

**Email not sending**: Verify the email harness at `../claude-email-automation/` is configured.

## Development

```bash
# Install dev dependencies
pip install -e ".[dev]"

# Run tests
pytest

# Run with debug logging
LOG_LEVEL=DEBUG python -m src.main
```
