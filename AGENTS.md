Read .agent-os/agents.md for workflow instructions and persona definitions.

# Claude Session Manager

Multi-agent orchestration system for Claude Code. Manages sessions, enables parent-child agent hierarchies, and provides Telegram integration.

## Key Components

- `src/main.py` - FastAPI server entry point
- `src/session_manager.py` - Core session lifecycle management
- `src/tmux_controller.py` - tmux session creation/control
- `src/cli/commands.py` - sm CLI command implementations
- `src/tool_logger.py` - Tool usage logging for security audit
- `src/telegram_bot.py` - Telegram bot integration
- `hooks/log_tool_use.sh` - Claude Code hook for tool logging

## Development

- Python 3.11+, FastAPI + uvicorn, SQLite, tmux

```bash
# Start server
./venv/bin/python -m src.main

# Testing
sm spawn --name test-agent "echo hello and exit"
sqlite3 ~/.local/share/claude-sessions/tool_usage.db "SELECT * FROM tool_usage LIMIT 5"
```

## Specs

Design docs in `specs/`:
- `sm-new-and-attach.md` - CLI session commands
- `tool-usage-logging.md` - Security audit logging

## Working Docs

Specs and working docs go in `specs/`:
```
specs/<ticket#>_<descriptive_name>.md
```
