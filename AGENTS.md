Read .agent-os/agents.md for workflow instructions and persona definitions.

# Session Manager

Rust multi-agent orchestration system for Claude Code and Codex. Manages sessions, parent-child agent hierarchies, durable messaging, queue jobs, and the Android operator app.

## Key Components

- `crates/sm-server/src/main.rs` - server entry point
- `crates/sm-server/src/runtime.rs` - process and tmux lifecycle management
- `crates/sm-server/src/sessions.rs` - session state and lifecycle
- `crates/sm-server/src/http.rs` - HTTP API and hook endpoints
- `crates/sm-server/src/bin/sm.rs` - `sm` CLI
- `crates/sm-server/src/bin/watch/` - native terminal dashboard
- `crates/sm-server/src/queue.rs` - durable message and job queues
- `hooks/log_tool_use.sh` - Claude Code hook for tool logging

## Development

- Rust 1.86+, Axum, Tokio, SQLite, tmux

```bash
# Build and test
cargo build -p sm-server
./scripts/test-rust-isolated.sh

# Manual smoke
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
