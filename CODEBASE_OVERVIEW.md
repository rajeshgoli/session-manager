# Session Manager Codebase Overview

Session Manager is a Rust service and CLI for coordinating Claude Code and
Codex sessions in tmux. It owns session lifecycle, durable messaging, queue
jobs, hooks, audit data, mobile access, and restart recovery.

## Directory Structure

```text
session-manager/
├── crates/sm-server/       # Rust server, CLI, and tests
├── android-app/            # Native Android operator app
├── hooks/                  # Claude Code lifecycle and audit hooks
├── scripts/                # Install, restart, smoke, and maintenance tools
├── scripts/rust_migration/ # Cutover evidence and state safety tools
├── specs/                  # Current design and working documents
└── web/sm-watch/           # Browser dashboard assets
```

## Rust Components

- `crates/sm-server/src/main.rs` loads configuration and starts the Axum
  service.
- `crates/sm-server/src/bin/sm.rs` implements the operator and agent CLI.
- `crates/sm-server/src/bin/watch/` implements the native terminal dashboard.
- `crates/sm-server/src/http.rs` defines the HTTP, hook, mobile, and artifact
  routes.
- `crates/sm-server/src/runtime.rs` owns provider processes, tmux sessions,
  restore behavior, and runtime reconciliation.
- `crates/sm-server/src/sessions.rs` loads, validates, and persists session
  state.
- `crates/sm-server/src/queue.rs` implements durable messages, reminders,
  review notifications, and managed queue jobs.
- `crates/sm-server/src/codex_events.rs`, `codex_requests.rs`, and
  `codex_activity.rs` implement Codex event persistence and activity state.
- `crates/sm-server/src/tool_usage.rs`, `usage_ledger.rs`, and
  `mobile_analytics.rs` implement audit and usage reporting.
- `crates/sm-server/src/mobile_devices.rs`, `google_auth.rs`, and
  `cloudflare_access.rs` enforce the mobile authentication boundary.

The server, CLI, and terminal dashboard no longer use Python. Python scripts
under `scripts/` are operational and migration-evidence tools, not an alternate
Session Manager implementation.

## Runtime State

Production configuration and durable state live outside the checkout:

```text
~/.config/session-manager/config.yaml
~/.config/session-manager/certs/
~/.local/share/claude-sessions/
```

The launchd service runs an installed copy at `.local/bin/sm-server`. The `sm`
CLI is installed beside it. Use `scripts/restart-rust-server.sh` for a live
restart; it builds, signs, installs, re-registers, and verifies the service in
the required order.

## Development

```bash
cargo build -p sm-server
./scripts/test-rust-isolated.sh
cargo fmt --check
```

For operational and migration-evidence scripts:

```bash
python3.11 -m venv venv
venv/bin/python -m pip install -r requirements.txt
venv/bin/python -m pytest
```

See `README.md` for setup and operator usage, and `specs/` for behavior and
security decisions.
