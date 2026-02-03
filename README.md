# Claude Session Manager

**Distributed infrastructure for AI agent swarms.** Spawn Claude agents, orchestrate workflows, coordinate without burning tokens.

```
┌─────────────────────────────────────────────────────────────────┐
│                         ORCHESTRATOR (EM)                       │
│                    "Implement Epic #987"                        │
└──────────────┬────────────────┬────────────────┬───────────────┘
               │                │                │
         sm spawn          sm spawn          sm spawn
               │                │                │
               ▼                ▼                ▼
        ┌──────────┐     ┌──────────┐     ┌──────────┐
        │ Engineer │     │ Architect│     │  Scout   │
        │  Agent   │     │  Agent   │     │  Agent   │
        └────┬─────┘     └────┬─────┘     └────┬─────┘
             │                │                │
             └────────────────┼────────────────┘
                              │
                      sm send em-main
                       "done: PR #42"
                              │
                              ▼
                    ┌─────────────────┐
                    │  EM wakes up,   │
                    │  routes to next │
                    │  agent          │
                    └─────────────────┘
```

## Why This Exists

**Problem:** Claude agents burn tokens while waiting. Spawn a worker, wait for completion, context grows, costs explode.

**Solution:** A central manager that lets agents go idle. Spawn workers → go to sleep → wake on notification. Zero tokens burned while waiting.

```bash
# EM spawns engineer, goes idle (no tokens burned)
sm spawn "Implement ticket #123" --name engineer --wait 600

# Engineer works autonomously...
# ...finishes, notifies EM
sm send em-main "done: PR #456 created"

# EM wakes up, routes PR to architect
sm send architect "Review PR #456"
```

**Result:** Complex multi-agent workflows at a fraction of the token cost.

---

## What It Enables

### Agent Swarms
Spawn specialized agents that work in parallel. Engineer implements while Architect reviews while Scout investigates.

### Async Orchestration
The EM (Engineering Manager) pattern: spawn workers, dispatch tasks, collect results. Never wait synchronously.

### Workspace Coordination
Auto-locking on file writes. Conflict detection. Multiple agents, one codebase, zero collisions.

### Message Queuing
Reliable delivery with priority levels. Sequential (wait for idle), Important (queue behind), Urgent (interrupt now).

### Token Efficiency
Agents sleep while waiting. Central manager handles coordination. Pay only for actual work.

---

## Quick Start

```bash
# Install
git clone https://github.com/rajeshgoli/claude-session-manager
cd claude-session-manager
./setup.sh

# Configure
cp config.yaml.example config.yaml
vim config.yaml  # Add Telegram token if desired

# Run
source venv/bin/activate
python -m src.server
```

---

## The SM CLI

Every managed session gets the `sm` command. This is how agents coordinate.

### Core Commands

| Command | Purpose |
|---------|---------|
| `sm spawn "<prompt>" --name X` | Spawn child agent |
| `sm send <id> "<text>"` | Send message to agent |
| `sm wait <id> N` | Async wait, notify after N seconds |
| `sm clear <id>` | Clear agent context for reuse |
| `sm children` | List your spawned agents |
| `sm what <id>` | AI summary of what agent is doing |
| `sm kill <id>` | Terminate an agent |

### Coordination Commands

| Command | Purpose |
|---------|---------|
| `sm name "<name>"` | Set your friendly name |
| `sm status` | Your status + others + locks |
| `sm alone` | Check if you're the only agent |
| `sm others` | List other agents in workspace |
| `sm lock "<reason>"` | Acquire workspace lock |
| `sm unlock` | Release lock |

### Message Delivery Modes

```bash
sm send agent "message"              # Sequential: wait for idle
sm send agent "message" --important  # Queue behind current work
sm send agent "message" --urgent     # Interrupt immediately
```

---

## The EM Pattern

The Engineering Manager orchestrates without doing implementation work.

```bash
# 1. Spawn standby agents at session start
sm spawn "As engineer, await tasks" --name engineer-standby --wait 600
sm spawn "As architect, await tasks" --name architect-standby --wait 300

# 2. Dispatch work
sm clear engineer-standby
sm send engineer-standby "Implement ticket #123. When done: sm send $EM_ID 'done: PR created'" --urgent
sm wait engineer-standby 600  # Async - EM goes idle

# 3. Wake on notification, route to next agent
sm send architect-standby "Review PR #456" --urgent

# 4. Repeat until workflow complete
```

**Key insight:** EM's context is preserved across worker completions. Workers are disposable; EM maintains state.

---

## Auto-Locking

File writes automatically acquire workspace locks via Claude Code hooks.

```json
// .claude/settings.json
{
  "hooks": {
    "PreToolUse": [{
      "matcher": { "tool_name": "write|edit" },
      "hooks": [{ "type": "command", "command": "sm auto-lock" }]
    }]
  }
}
```

Multiple agents, same repo, no conflicts. If another agent holds the lock, your write blocks until they release.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    SESSION MANAGER                          │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │   FastAPI   │  │   Message   │  │   Lock Manager      │  │
│  │   Server    │  │   Queue     │  │   (workspace locks) │  │
│  └──────┬──────┘  └──────┬──────┘  └──────────┬──────────┘  │
│         │                │                    │             │
│         └────────────────┼────────────────────┘             │
│                          │                                  │
│                   ┌──────▼──────┐                           │
│                   │    tmux     │                           │
│                   │ Controller  │                           │
│                   └──────┬──────┘                           │
└──────────────────────────┼──────────────────────────────────┘
                           │
           ┌───────────────┼───────────────┐
           │               │               │
     ┌─────▼─────┐   ┌─────▼─────┐   ┌─────▼─────┐
     │  Claude   │   │  Claude   │   │  Claude   │
     │  Agent 1  │   │  Agent 2  │   │  Agent 3  │
     └───────────┘   └───────────┘   └───────────┘
```

**Components:**
- **FastAPI Server** - REST API for session control
- **Message Queue** - SQLite-backed reliable delivery
- **Lock Manager** - Workspace coordination
- **tmux Controller** - Session lifecycle management
- **Output Monitor** - Detects idle, errors, permission prompts

---

## Telegram Integration (Optional)

Control your swarm from your phone.

| Command | Action |
|---------|--------|
| `/new [path]` | Spawn new session |
| `/list` | List active sessions |
| `/kill <id>` | Terminate session |
| Reply to message | Send input to that session |

Sessions can notify you on completion, errors, or when they need input.

---

## API Reference

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/sessions` | POST | Create session |
| `/sessions` | GET | List sessions |
| `/sessions/{id}/input` | POST | Send input |
| `/sessions/{id}/watch` | POST | Watch for completion |
| `/sessions/{id}` | DELETE | Kill session |
| `/health` | GET | Server health check |

Full API docs at `http://localhost:8420/docs` when running.

---

## Configuration

```yaml
server:
  host: "127.0.0.1"
  port: 8420

paths:
  state_file: "~/.claude-sessions/state.json"

monitor:
  idle_timeout: 300      # Notify after 5min idle
  poll_interval: 1.0

telegram:  # Optional
  token: "BOT_TOKEN"
  allowed_chat_ids: [123456789]
```

---

## Testing

```bash
# Run test suite (194 tests)
pytest tests/ -v

# Run with coverage
pytest tests/ --cov=src
```

---

## Requirements

- macOS (Linux support planned)
- Python 3.11+
- tmux (`brew install tmux`)
- Claude Code CLI

---

## License

MIT

---

## Contributing

Issues and PRs welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

---

**Built for the age of AI agents.** When one Claude isn't enough.
