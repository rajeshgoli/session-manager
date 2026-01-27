# Claude Session Manager - Codebase Overview

## Project Purpose

**Claude Session Manager** is a local macOS server that manages Claude Code sessions running in tmux, with bidirectional communication via Telegram and Email. The system enables users to interact with long-running Claude sessions remotely while seeing real-time progress in Terminal.app windows locally.

### Key Benefits
- Never lose access to Claude sessions (no more timeouts)
- View Claude's work in real Terminal windows when at laptop
- Get notified and respond via Telegram when away
- Support multiple concurrent sessions
- Allow Claude to explicitly request email for longer-form communication

## Directory Structure

```
claude-session-manager/
├── src/
│   ├── main.py                 # Entry point, orchestrates all components
│   ├── models.py               # Data models (Session, SessionStatus, NotificationEvent)
│   ├── session_manager.py      # Session lifecycle management
│   ├── tmux_controller.py      # tmux command interface
│   ├── server.py               # FastAPI server with HTTP endpoints and Claude hooks
│   ├── output_monitor.py       # Async log tailing and pattern detection
│   ├── telegram_bot.py         # Telegram bot for command handling
│   ├── notifier.py             # Routes notifications to Telegram/Email
│   ├── email_handler.py        # Email wrapper around external harness
│   └── __init__.py
├── config.yaml                 # Runtime configuration
├── config.yaml.example         # Configuration template
├── pyproject.toml              # Python project metadata
├── requirements.txt            # Dependencies
├── setup.sh                    # Setup script
├── attach-session              # Utility script
├── README.md                   # User documentation
└── claude-session-manager-spec.md  # Detailed specification
```

## Core Components

### 1. main.py
**Purpose:** Application orchestrator and entry point

**Key Functions:**
- `SessionManagerApp` - Main application class
- `async main()` - Async entry point
- `run()` - Console script entry point (defined in pyproject.toml)

**Initialization Order:**
1. Load configuration from `config.yaml`
2. Initialize `SessionManager` (loads persisted sessions)
3. Initialize `OutputMonitor` (async pattern detection)
4. Initialize `EmailHandler` (optional)
5. Initialize `TelegramBot` (if token configured)
6. Initialize `Notifier` (routes to Telegram/Email)
7. Create FastAPI app with all dependencies
8. Set up signal handlers for graceful shutdown
9. Start Telegram bot
10. Restore monitoring for existing sessions
11. Start Uvicorn server on port 8420

### 2. models.py
**Purpose:** Data models and state representation

**Key Classes:**
- `SessionStatus` - Enum (STARTING, RUNNING, WAITING_INPUT, WAITING_PERMISSION, IDLE, STOPPED, ERROR)
- `Session` - Main session data model
  - Fields: id, name, tmux_session, status, created_at, log_file, last_output, last_message, telegram_thread_id, telegram_chat_id, transcript_path
  - Methods: `to_dict()`, `from_dict()`
- `NotificationEvent` - Event data for notifications
  - Fields: session_id, event_type, message, timestamp, use_email
- `UserInput` - User input payload for API
  - Fields: text, auto_approve

### 3. session_manager.py
**Purpose:** Session lifecycle and persistence

**Key Class:** `SessionManager`

**Responsibilities:**
- CRUD operations on sessions
- State persistence (JSON file at `/tmp/claude-sessions/sessions.json`)
- tmux integration via `TmuxController`
- Session restoration on startup

**Key Methods:**
- `create_session(name, initial_prompt, use_email)` - Create new Claude session
- `get_session(session_id)` - Retrieve session by ID
- `list_sessions()` - List all sessions
- `kill_session(session_id)` - Terminate session
- `send_input(session_id, text)` - Send input to session
- `send_key(session_id, key)` - Send special key (y/n)
- `capture_output(session_id, lines)` - Get recent output
- `update_session_status(session_id, status)` - Update status
- `_save_sessions()` - Persist to JSON
- `_restore_sessions()` - Load from JSON on startup

### 4. tmux_controller.py
**Purpose:** tmux abstraction layer

**Key Class:** `TmuxController`

**Responsibilities:**
- Create/kill tmux sessions
- Send input/keys to tmux panes
- Capture output from tmux buffers
- Terminal.app integration for macOS

**Key Methods:**
- `create_session(name, log_file)` - Create tmux session with logging
- `send_input(session_name, text)` - Send text + Enter
- `send_keys(session_name, keys)` - Send special keys
- `capture_output(session_name, lines)` - Get last N lines
- `kill_session(session_name)` - Terminate session
- `session_exists(session_name)` - Check if session is running
- `open_in_terminal(session_name)` - Open Terminal.app window
- `_get_pane_id(session_name)` - Get tmux pane ID

### 5. server.py
**Purpose:** FastAPI REST API and Claude Code webhook handler

**Key Components:**
- FastAPI app with dependency injection
- REST endpoints for session management
- Claude Code webhook receiver at `/hooks/claude`
- Structured output extraction from transcripts

**REST API Endpoints:**
- `POST /sessions` - Create session
- `GET /sessions` - List sessions
- `GET /sessions/{id}` - Get session details
- `POST /sessions/{id}/input` - Send input
- `POST /sessions/{id}/key` - Send key (y/n)
- `DELETE /sessions/{id}` - Kill session
- `POST /sessions/{id}/open` - Open in Terminal.app
- `GET /sessions/{id}/output` - Capture output
- `GET /sessions/{id}/last-message` - Get last Claude output
- `POST /notify` - Send notification
- `POST /hooks/claude` - Claude Code webhook

**Hook Processing:**
- Receives transcript path and hook data
- Extracts last assistant message from JSONL transcript
- Matches hooks to sessions via `CLAUDE_SESSION_MANAGER_ID` env var
- Stores last message for `/status` command
- Sends Stop hook (response completion) to Telegram

### 6. output_monitor.py
**Purpose:** Async log tailing and pattern detection

**Key Class:** `OutputMonitor`

**Responsibilities:**
- Async file tailing for session logs
- Pattern matching for various events
- Event emission to registered callbacks
- Idle detection with configurable timeout

**Pattern Detection:**
- **Permission prompts:** `[Y/n]`, `Allow`, `Do you want to proceed?`
- **Errors:** `Error:`, `Exception:`, `Permission denied`, `command not found`
- **Completion:** `Task complete`, `Done.`, `All tests passed`
- **Idle:** No output for configured timeout (default 300s)

**Key Methods:**
- `start_monitoring(session_id, log_file)` - Start monitoring session
- `stop_monitoring(session_id)` - Stop monitoring session
- `on_event(callback)` - Register event callback
- `_tail_log_file(session_id, log_file)` - Async tail loop
- `_check_patterns(line)` - Pattern matching

**Grace Periods:**
- 5-minute idle cooldown after response sent
- 30-second debounce for permission prompts
- Grace period for restored sessions on startup

### 7. telegram_bot.py
**Purpose:** Telegram bot interface

**Key Class:** `TelegramBot`

**Responsibilities:**
- Command handling (/new, /list, /status, /kill, etc)
- Thread/topic management for session organization
- Input handling via message replies
- Authorization checks (chat_id, user_id filtering)

**Commands:**
- `/new [prompt]` - Create new session
- `/list` - List all sessions
- `/status <id>` - Get session status
- `/kill <id>` - Terminate session
- `/open <id>` - Open in Terminal.app
- `/stop` - Stop monitoring (placeholder)
- `/name <id> <name>` - Rename session
- `/password <service>` - Get password (external service)
- `/help` - Show help

**Threading:**
- Forum groups: Creates dedicated topic per session
- Regular groups: Uses reply threads to root message
- Tracks session → (chat_id, message_id/topic_id) mapping

**Key Methods:**
- `initialize()` - Set up bot handlers
- `start_polling()` - Start bot
- `stop_polling()` - Stop bot
- `send_notification(chat_id, thread_id, message)` - Send to thread
- Handler setters for all operations (e.g., `set_create_session_handler()`)

### 8. notifier.py
**Purpose:** Notification routing and formatting

**Key Class:** `Notifier`

**Responsibilities:**
- Routes notifications to Telegram or Email
- Formats messages appropriately for each channel
- ANSI escape code stripping
- MarkdownV2 escaping for Telegram

**Key Methods:**
- `notify(event)` - Main notification dispatcher
- `_notify_telegram(event)` - Send to Telegram
- `_notify_email(event)` - Send to Email
- `_strip_ansi(text)` - Remove ANSI codes
- `_escape_markdown(text)` - Escape for MarkdownV2

### 9. email_handler.py
**Purpose:** Email integration wrapper

**Key Class:** `EmailHandler`

**Responsibilities:**
- Wraps external `claude-email-automation` harness
- Lazy-loads email modules
- Graceful fallback if unavailable

**Key Methods:**
- `send_email(to, subject, body)` - Send email
- `_lazy_load_modules()` - Import email harness on demand

## Technology Stack

### Core Framework
- **FastAPI** (>=0.104.0) - REST API server
- **Uvicorn** (>=0.24.0) - ASGI server
- **python-telegram-bot** (>=20.0) - Telegram bot API

### Supporting Libraries
- **Pydantic** (>=2.0.0) - Data validation
- **PyYAML** (>=6.0) - Configuration parsing
- **aiofiles** (>=23.0) - Async file operations
- **asyncio** - Built-in async framework

### External Dependencies
- **tmux** - Terminal multiplexer for session management
- **Claude Code CLI** - For running sessions
- **macOS Terminal.app** - For visible session windows
- **Telegram Bot API** - For messaging
- **SMTP/IMAP** - For email notifications (via external harness)

### Python Version
- 3.11+ (3.12 supported)

## Architecture and Data Flow

```
┌─────────────────────────────────────────────────────────┐
│                    Telegram Bot User                    │
└────────────────────┬────────────────────────────────────┘
                     │ Commands (/new, /list, /status)
                     ↓
┌─────────────────────────────────────────────────────────┐
│                   Telegram Bot Handler                  │
│         (authorization, command parsing, threads)       │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
┌─────────────────────────────────────────────────────────┐
│                    Session Manager                       │
│  • Create/Kill tmux sessions                            │
│  • Send input/keys to tmux                              │
│  • Maintain session state (JSON persistence)            │
│  • Coordinate with TmuxController                       │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
┌─────────────────────────────────────────────────────────┐
│              Output Monitor (async loop)                │
│  • Tail log files continuously                          │
│  • Detect patterns (permissions, errors, idle)          │
│  • Emit events to callbacks                             │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
┌─────────────────────────────────────────────────────────┐
│                       Notifier                           │
│  • Route to Telegram or Email                           │
│  • Format messages appropriately                        │
│  • Strip ANSI codes, escape markdown                    │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
┌─────────────────────────────────────────────────────────┐
│              Telegram/Email Channels                    │
└─────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────┐
│          Claude Code Hooks (HTTP POST)                  │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
┌─────────────────────────────────────────────────────────┐
│         FastAPI Server (/hooks/claude)                  │
│  • Extract transcript data                              │
│  • Find matching session                                │
│  • Parse last Claude message                            │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
┌─────────────────────────────────────────────────────────┐
│           Notifier (send to Telegram)                   │
└─────────────────────────────────────────────────────────┘
```

## Configuration

### config.yaml Structure

```yaml
server:
  host: "127.0.0.1"        # Bind address
  port: 8420               # Server port

paths:
  log_dir: "/tmp/claude-sessions"
  state_file: "/tmp/claude-sessions/sessions.json"

monitor:
  idle_timeout: 300        # Seconds before idle notification
  poll_interval: 1.0       # Output check frequency

telegram:
  token: "BOT_TOKEN"       # From @BotFather
  allowed_chat_ids: []     # Empty = allow all
  allowed_user_ids: []     # Empty = allow all

email:
  smtp_config: ""          # Path to email.yaml (optional)
  imap_config: ""          # Path to imap.yaml (optional)

services:
  office_automate_url: "http://192.168.5.140:8080"  # For utilities
```

## Session Lifecycle

### States
1. **STARTING** - Session being created
2. **RUNNING** - Claude actively processing
3. **WAITING_INPUT** - Needs user input
4. **WAITING_PERMISSION** - Needs permission approval
5. **IDLE** - No output for idle_timeout seconds
6. **STOPPED** - Session terminated normally
7. **ERROR** - Session encountered error

### Session Data
- Stored at: `/tmp/claude-sessions/sessions.json`
- Session ID: 8-character hex UUID
- tmux session name: `claude-{session_id}`
- Log file: `/tmp/claude-sessions/{session_name}.log`
- Transcript path: Set by Claude Code via hook

### Lifecycle Flow
```
Create → STARTING → RUNNING ─┬→ WAITING_INPUT → (input sent) → RUNNING
                              ├→ WAITING_PERMISSION → (approved) → RUNNING
                              ├→ IDLE → (activity) → RUNNING
                              ├→ ERROR (if crash)
                              └→ STOPPED (if killed)
```

## Claude Code Integration

### Webhook Setup
- Endpoint: `POST /hooks/claude`
- Configured in Claude Code settings
- Environment variable passed: `CLAUDE_SESSION_MANAGER_ID`

### Hook Matching Strategy
1. **Primary:** Match via `CLAUDE_SESSION_MANAGER_ID` env var (most reliable)
2. **Fallback 1:** Match by transcript path
3. **Fallback 2:** Match by `session_id` in hook payload

### Hook Types Processed
- **Stop hook:** Claude finished response
  - Extracts last assistant message from transcript
  - Sends formatted markdown to Telegram
  - Updates session's `last_message` field

### Transcript Format
- JSONL file with conversation turns
- Each line: `{"type": "assistant"|"user", "content": "..."}`
- Parsed to extract last assistant message

## Telegram Integration

### Forum vs Regular Groups

**Forum Groups (Recommended):**
- Creates dedicated topic per session
- Topic name: Session name or ID
- All session messages in same topic
- Cleaner organization

**Regular Groups:**
- Uses reply threads
- Root message created for session
- Replies to root message for updates

### Message Threading
- Tracks: `session_id → (chat_id, thread_id)`
- thread_id is:
  - `message_thread_id` for forum topics
  - `message_id` for reply threads
- Stored in session data for persistence

### Authorization
- Optional `allowed_chat_ids` filter
- Optional `allowed_user_ids` filter
- Empty list = allow all
- Checked on every command

## Email Integration

### External Harness
- Reuses `claude-email-automation` project
- Located at: `../claude-email-automation/`
- Lazy-loaded on demand

### Configuration
- `email.yaml` - SMTP settings
- `imap.yaml` - IMAP settings (for future reply handling)
- Paths specified in main `config.yaml`

### Usage
- Sessions created with `use_email=True` route notifications to email
- Email subject: "Claude Session [session_name]"
- Body: Plain text with ANSI codes stripped

## Notable Patterns

### Async/Await Throughout
- All I/O operations are async
- Event callbacks use async handlers
- Clean separation between sync (tmux) and async (monitoring)

### Callback Architecture
- `OutputMonitor` emits events to registered callbacks
- `TelegramBot` has handler setters for all operations
- Decoupled components communicate via callbacks

### State Persistence
- JSON-based session state
- Atomic writes with temp file + rename
- Sessions reloaded on startup
- Dead sessions (no longer in tmux) marked as stopped

### Markdown Handling
- ANSI escape code stripping for notifications
- MarkdownV2 escaping for Telegram
- Preserves code blocks during escape

### Error Handling
- Graceful degradation (email optional, Terminal.app optional)
- Comprehensive logging at all levels
- Try/except blocks with fallbacks
- Never crash on external service failures

## Security Considerations

- **Authorization:** Optional chat_id and user_id filtering
- **Local-only:** Server binds to 127.0.0.1 by default
- **No secrets in state:** Session IDs are UUIDs, no tokens stored
- **External config:** Telegram token, SMTP credentials in separate files
- **Permission checks:** All Telegram commands check authorization

## Development Notes

### Code Quality
- Type hints throughout (Python 3.11+)
- Dataclass-based models
- Single-responsibility modules
- Comprehensive logging
- Async best practices

### Entry Points
- `python -m src.main` - Direct invocation
- `claude-session-manager` - Console script (after pip install)
- `python src/main.py` - Alternative

### Testing
- Start server: `python -m src.main`
- Test endpoints: `curl http://localhost:8420/sessions`
- Test Telegram: Send `/help` to bot
- Check logs: `tail -f /tmp/claude-sessions/*.log`

### Debugging
- Set `LOG_LEVEL=DEBUG` environment variable
- Watch state file: `watch -n 1 cat /tmp/claude-sessions/sessions.json`
- Monitor tmux: `tmux ls` and `tmux attach -t claude-{id}`
- Check webhook calls: Look for "Received hook" in logs

## Future Enhancement Ideas

### Potential Improvements
- Web UI for session management
- Session recording/playback
- Multi-user support with authentication
- Session sharing/collaboration
- Cost tracking per session
- Session templates/presets
- Email reply handling (IMAP integration)
- Slack integration
- Discord integration
- Session auto-archive after N days
- Metrics and analytics dashboard

### Known Limitations
- macOS only (Terminal.app integration)
- Single server instance (no clustering)
- No session migration between machines
- No built-in authentication (relies on Telegram auth)
- Email is send-only (no reply parsing yet)

---

**Last Updated:** 2026-01-26
**Version:** Current codebase snapshot
