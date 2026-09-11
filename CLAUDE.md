# Claude Session Manager

Rust multi-agent orchestration system for Claude Code and Codex. Manages
sessions, parent-child agent hierarchies, durable messaging, queue jobs, and
Android/email operator access.

## Architecture

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│ Claude / Codex  │────▶│  Session Manager │────▶│  Android / Email│
│  (in tmux)      │     │  (Rust / Axum)   │     │   (optional)    │
└─────────────────┘     └──────────────────┘     └─────────────────┘
        │                        │
        ▼                        ▼
┌─────────────────┐     ┌──────────────────┐
│  sm CLI         │     │  SQLite DBs      │
│  (commands)     │     │  (state, tools)  │
└─────────────────┘     └──────────────────┘
```

## Key Components

- `crates/sm-server/src/main.rs` - server entry point
- `crates/sm-server/src/runtime.rs` - provider and tmux lifecycle
- `crates/sm-server/src/sessions.rs` - session persistence and lifecycle
- `crates/sm-server/src/http.rs` - HTTP API and hooks
- `crates/sm-server/src/bin/sm.rs` - `sm` CLI
- `crates/sm-server/src/bin/watch/` - native terminal dashboard
- `crates/sm-server/src/queue.rs` - message, reminder, and job queues
- `hooks/log_tool_use.sh` - Claude Code hook for tool logging
- `hooks/context_monitor.sh` - statusLine hook feeding `/hooks/context-usage` (delegates rendering to the previously configured status line)
- `scripts/install_notify_server_hook.sh` - installs and registers every hook in `hooks/`
- `scripts/restart-rust-server.sh` - the only supported way to rebuild and restart the live Rust server

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
sm codex-tui <session>   # Codex-app live state/events/request UI

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

codex_rollout:
  enable_durable_events: true
  enable_structured_requests: true
  enable_observability_projection: true
  enable_codex_tui: true
```

## Development

- Rust 1.86+
- Axum + Tokio
- SQLite for persistence
- tmux for session management

### Running locally

```bash
# Build the server and CLI
cargo build -p sm-server

# Run the isolated test launcher
./scripts/test-rust-isolated.sh
```

### Restarting the live Rust server

```bash
./scripts/restart-rust-server.sh
```

Always use this script. Hand-rolling `cargo build` + `launchctl kickstart -k`
has taken the service down: launchd can pin a launch constraint into the job
registration, and only re-registering the job clears it.

The service runs from an installed copy at `.local/bin/sm-server`, not from
`target/release/sm-server`, so an ordinary `cargo build` never disturbs the
running server. See `specs/1134_rust_restart_procedure.md`.

The `sm` CLI is installed the same way, and for the same reason: `cargo clean`
deletes everything under `target/`, so a CLI that only lives at
`target/release/sm` vanishes with it. `restart-rust-server.sh` reinstalls it
after every successful restart; to refresh it on its own:

```bash
./scripts/install-sm-cli.sh
```

Keep `.local/bin` on `PATH` so `sm` resolves to the installed Rust CLI.

### Where production state lives

Production config and state live outside every checkout, because the primary
checkout is also the `maintainer` service role's working directory - an
autonomous agent branches and builds there, and a `cargo clean` in it once
removed the `sm` CLI.

```
~/.config/session-manager/config.yaml   # the live service's --config
~/.config/session-manager/certs/        # mobile device CA cert + key
~/.local/share/claude-sessions/         # DBs, queue state, app artifacts
```

`restart-rust-server.sh` defaults `SM_CONFIG` to the installed config and only
falls back to the in-repo `config.yaml` when none is installed.

Note that `app_artifacts.root_dir`, `bug_reports.db_path` and the two
`mobile_terminal` CA paths are set explicitly in the installed config. Their
compiled defaults derive from `CARGO_MANIFEST_DIR` or the process CWD - both of
which point at whichever tree the binary happened to be built in, not at a
stable location.

### Testing

```bash
# Rust server tests — the supported launcher also cleans isolated state. Direct
# cargo test has a fail-safe isolation root, but must not be used for normal work.
./scripts/test-rust-isolated.sh

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

Specs and working docs go in `specs/<ticket#>_<descriptive_name>.md`, per AGENTS.md.
`docs/working/` holds the older docs and is not where new ones belong.

Earlier design docs:
- `sm-new-and-attach.md` - CLI session commands
- `tool-usage-logging.md` - Security audit logging

## Common Issues

1. **Hooks not logging**: Check session manager is running (`curl localhost:8420/health`)
2. **sm commands fail**: Ensure `CLAUDE_SESSION_MANAGER_ID` env var is set
3. **Session not found**: Use full session ID or exact friendly name
