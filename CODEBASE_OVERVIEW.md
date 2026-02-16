# Claude Session Manager - Codebase Overview

## Project Purpose

**Claude Session Manager** is a local macOS service that manages Claude Code sessions running inside `tmux`, with bidirectional control and notifications via Telegram and Email. It supports multi-agent workflows (parent/child sessions), reliable inter-agent messaging, and remote visibility into session progress.

## Directory Structure

```
session-manager/
├── src/
│   ├── main.py                 # Orchestrator entry point
│   ├── server.py               # FastAPI API + Claude hooks
│   ├── session_manager.py      # Session lifecycle + persistence
│   ├── tmux_controller.py      # tmux interface
│   ├── output_monitor.py       # Log tailing + pattern detection
│   ├── message_queue.py        # Reliable message queue (sm send v2)
│   ├── child_monitor.py        # Background monitor for --wait
│   ├── tool_logger.py          # Tool-use logging for audit
│   ├── lock_manager.py         # Workspace lock files
│   ├── telegram_bot.py         # Telegram bot integration
│   ├── notifier.py             # Notification routing + formatting
│   ├── email_handler.py        # Email harness wrapper
│   ├── models.py               # Data models + enums
│   └── cli/                    # sm CLI entry + commands
├── config.yaml                 # Runtime configuration
├── config.yaml.example         # Config template
├── README.md                   # User documentation
└── CODEBASE_OVERVIEW.md        # This file
```

## Core Components

### 1. `src/main.py`
**Purpose:** Application orchestrator and entry point

**Key responsibilities:**
- Load configuration from `config.yaml`
- Initialize core services: `SessionManager`, `OutputMonitor`, `EmailHandler`, `TelegramBot` (optional), `Notifier`, `ChildMonitor`, `MessageQueueManager`, `ToolLogger`
- Create FastAPI app via `create_app()`
- Restore monitoring for existing sessions
- Start Uvicorn server
- Event loop watchdog to detect freezes

### 2. `src/models.py`
**Purpose:** Data models and enums

**Key models:**
- `Session`, `SessionStatus`
- `DeliveryMode`, `DeliveryResult`
- `NotificationEvent`, `NotificationChannel`
- `Subagent`, `SubagentStatus`
- `CompletionStatus` (child sessions)
- `QueuedMessage`, `SessionDeliveryState`
- `UserInput`

### 3. `src/session_manager.py`
**Purpose:** Session lifecycle and persistence

**Key responsibilities:**
- Create/kill sessions in tmux
- Persist session state to JSON (default `/tmp/claude-sessions/sessions.json`)
- Spawn child sessions with initial prompts
- Send input with delivery modes via message queue
- Update status, friendly name, task

### 4. `src/message_queue.py`
**Purpose:** Reliable message delivery (sm send v2)

**Key responsibilities:**
- SQLite-backed queue (`message_queue` table)
- Delivery modes: `sequential` (deliver when idle), `important` (deliver after current response), `urgent` (interrupt and deliver immediately)
- Detect stale user input and temporarily clear/restore
- Reminders (`/scheduler/remind`) and session watching (`/sessions/{id}/watch`)
- Sender notifications (delivery + Stop hook)

### 5. `src/child_monitor.py`
**Purpose:** Monitor child sessions when `--wait` is used

**Key responsibilities:**
- Poll child sessions for idle/complete
- Notify parent session via input message
- Update `completion_status` and `completion_message`

### 6. `src/output_monitor.py`
**Purpose:** Async log tailing and pattern detection

**Detects:**
- Permission prompts
- Errors
- Completion hints
- Idle states

**Notes:**
- Debounces permission prompts
- Suppresses idle notifications for a cooldown after responses
- Detects dead tmux sessions and triggers cleanup

### 7. `src/tmux_controller.py`
**Purpose:** tmux abstraction

**Key responsibilities:**
- Create tmux sessions with custom command and model
- Async input injection (`send_input_async`)
- Capture output
- Open session in Terminal.app (macOS)

### 8. `src/server.py`
**Purpose:** FastAPI API and Claude hooks

**Key endpoints:**
- Health: `GET /`, `GET /health`, `GET /health/detailed`
- Sessions: `POST /sessions`, `GET /sessions`, `GET /sessions/{id}`, `PATCH /sessions/{id}`
- Legacy create: `POST /sessions/create?working_dir=...`
- Input: `POST /sessions/{id}/input`, `POST /sessions/{id}/key`
- Output: `GET /sessions/{id}/output`, `GET /sessions/{id}/last-message`, `GET /sessions/{id}/summary`
- Tasks: `PUT /sessions/{id}/task`
- Kill/open: `DELETE /sessions/{id}`, `POST /sessions/{id}/open`
- Subagents: `POST /sessions/{id}/subagents`, `POST /sessions/{id}/subagents/{agent_id}/stop`, `GET /sessions/{id}/subagents`
- Child sessions: `POST /sessions/spawn`, `GET /sessions/{parent_id}/children`
- Message queue: `GET /sessions/{id}/send-queue`, `POST /scheduler/remind`, `POST /sessions/{id}/watch`
- Hooks: `POST /hooks/claude`, `POST /hooks/tool-use`
- Notifications: `POST /notify`

**Hook behavior highlights:**
- Stop hook stores last response and marks session idle for queued delivery
- Auto-release workspace locks on Stop
- Optional cleanup prompts for worktrees with uncommitted changes
- Notification hook forwards permission prompts/errors to Telegram

### 9. `src/telegram_bot.py`
**Purpose:** Telegram bot interface

**Commands:**
- `/start`, `/help`
- `/new [path]`, `/session` (project picker)
- `/list`, `/status`, `/subagents`
- `/message` (last Claude message), `/summary` (AI summary)
- `/kill <id>`, `/stop` (interrupt), `/force <msg>`
- `/open <id>`, `/name <name>`
- `/password`, `/follow`

**Features:**
- Forum topic mode for per-session threads
- Inline keyboard for permission prompts
- Reply-to-thread input handling

### 10. `src/notifier.py`
**Purpose:** Notification routing

**Key responsibilities:**
- Telegram and Email routing
- MarkdownV2 escaping for Claude responses
- ANSI stripping for non-response events
- Stores last response message ID for idle replies

### 11. `src/lock_manager.py`
**Purpose:** Workspace lock files for coordination

**Notes:**
- Lock file path: `.claude/workspace.lock`
- Auto-acquired on file writes via hook
- Used as fallback when session manager unavailable

### 12. `src/tool_logger.py`
**Purpose:** Tool usage logging for audit

**Notes:**
- SQLite DB (default `~/.local/share/claude-sessions/tool_usage.db`)
- Detects destructive operations and sensitive file access
- Populated by `POST /hooks/tool-use`

## Architecture and Data Flow

```
Telegram / CLI
    │
    ▼
FastAPI Server (server.py)
    │
    ├─ SessionManager ── tmux_controller ── tmux sessions
    │
    ├─ OutputMonitor ── pattern detection ── Notifier → Telegram/Email
    │
    ├─ MessageQueue ── queued delivery / reminders / watches
    │
    └─ ToolLogger ── audit log

Claude Code Hooks
    │
    ├─ /hooks/claude → Stop/Notification handling
    └─ /hooks/tool-use → tool logging + auto-lock
```

## Configuration (Used by Code)

**Key sections:**
- `server`: host/port
- `paths`: `log_dir`, `state_file`
- `monitor`: idle timeout, poll interval, notification toggles
- `timeouts`: tmux, output_monitor, message_queue, server
- `telegram`: bot token, allowed chat/user IDs
- `email`: SMTP/IMAP config paths
- `services`: `office_automate_url` for `/password`
- `claude`: command, args, default model
- `sm_send`: message queue tuning
- `tool_logging`: optional `db_path`

## Session Lifecycle

**States:**
`STARTING` → `RUNNING` → `WAITING_INPUT` / `WAITING_PERMISSION` / `IDLE` → `STOPPED` / `ERROR`

**Child sessions:**
- Marked with `parent_session_id`
- `completion_status` and `completion_message` set by `ChildMonitor`

## Claude Code Integration

**Hooks:**
- `POST /hooks/claude` (Stop + Notification)
- `POST /hooks/tool-use` (PreToolUse/PostToolUse + subagent events)

**Matching strategy:**
- Prefer `CLAUDE_SESSION_MANAGER_ID`
- Fallback by transcript path or Claude session ID

## Development Notes

**Entry points:**
- `python -m src.main`
- Console script: `claude-session-manager`

**Python version:** 3.11+

**Testing:**
- `pytest tests/ -v`
