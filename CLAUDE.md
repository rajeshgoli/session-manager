# Claude Session Manager

Multi-agent orchestration system for Claude Code. Manages sessions, enables parent-child agent hierarchies, and provides Telegram integration.

## Architecture

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│  Claude Code    │────▶│  Session Manager │────▶│  Telegram Bot   │
│  (in tmux)      │     │  (FastAPI)       │     │  (optional)     │
└─────────────────┘     └──────────────────┘     └─────────────────┘
        │                        │
        ▼                        ▼
┌─────────────────┐     ┌──────────────────┐
│  sm CLI         │     │  SQLite DBs      │
│  (commands)     │     │  (state, tools)  │
└─────────────────┘     └──────────────────┘
```

## Key Components

- `src/main.py` - FastAPI server entry point
- `src/session_manager.py` - Core session lifecycle management
- `src/tmux_controller.py` - tmux session creation/control
- `src/cli/commands.py` - sm CLI command implementations
- `src/tool_logger.py` - Tool usage logging for security audit
- `src/telegram_bot.py` - Telegram bot integration
- `hooks/log_tool_use.sh` - Claude Code hook for tool logging

## sm CLI Commands

```bash
# Session info
sm me              # Current session info
sm status          # All sessions
sm who             # Who am I (name only)

# Session management
sm new [dir]       # Create and attach to new session
sm attach [session] # Attach to existing session
sm spawn "prompt"  # Spawn child agent
sm kill <session>  # Kill session (children only)

# Communication
sm send <session> "msg"  # Send message to session
sm output <session>      # View session output

# Agent coordination
sm children        # List child sessions
sm name <name>     # Rename self
sm clear <session> # Clear child context for reuse
```

## Config (config.yaml)

```yaml
claude:
  command: "claude"
  args: ["--dangerously-skip-permissions"]
  default_model: "sonnet"

server:
  host: "0.0.0.0"
  port: 8420
```

## Development

- Python 3.11+
- FastAPI + uvicorn
- SQLite for persistence
- tmux for session management

### Running locally

```bash
# Start server
./venv/bin/python -m src.main

# Or use the CLI
./venv/bin/sm status
```

### Testing

```bash
# Manual testing - spawn a child agent
sm spawn --name test-agent "echo hello and exit"

# Check tool logging
sqlite3 ~/.local/share/claude-sessions/tool_usage.db "SELECT * FROM tool_usage LIMIT 5"
```

## Conventions

- Session IDs are 8-char UUIDs (e.g., `a4af4272`)
- tmux sessions always named `claude-{session_id}`
- Friendly names are separate from tmux names
- Parent-child relationships enforced for security (can only kill/clear own children)
- Tool logging always enabled, no sampling

## Environment Variables

- `CLAUDE_SESSION_MANAGER_ID` - Set by tmux_controller, identifies session to hooks
- `ENABLE_TOOL_SEARCH=false` - Workaround for Claude Code bug

## Specs

Design docs in `specs/`:
- `sm-new-and-attach.md` - CLI session commands
- `tool-usage-logging.md` - Security audit logging

## Common Issues

1. **Hooks not logging**: Check session manager is running (`curl localhost:8420/health`)
2. **sm commands fail**: Ensure `CLAUDE_SESSION_MANAGER_ID` env var is set
3. **Session not found**: Use full session ID or exact friendly name
