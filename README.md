# Session Manager
## Dispatch teams of claude and codex agents
**Distributed infrastructure for AI agent swarms.** Spawn Claude agents, orchestrate workflows, coordinate without burning tokens. Watch it all unfold from your phone.

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
              ┌───────────────┼───────────────┐
              ▼                               ▼
    ┌─────────────────┐             ┌─────────────────┐
    │  EM wakes up,   │             │  📱 Telegram    │
    │  routes to next │             │  YOU see it too │
    │  agent          │             │                 │
    └─────────────────┘             └─────────────────┘
```

---

## Agent Nirvana

**Let agents swarm loose on your problems while you sip tea on a beach.**

No more opaque subagents you can't follow. Every agent is a full Claude Code session. Full transparency:

- **Watch from anywhere** — Every `sm send` between agents auto-forwards to your Telegram
- **Jump in anytime** — `sm attach engineer` opens the session in your terminal
- **Or stay remote** — Reply to Telegram messages to inject commands
- **Real sessions** — Not abstractions. Real tmux. Real Claude Code. `sm attach` and you're there.

```
📱 Your Phone                          🖥️ Your Agents
─────────────                          ────────────────
                                       EM: "Spawning engineer for #123"
[EM spawned engineer-standby]    ←────
                                       Engineer: *working*
                                       Engineer: "done: PR #456 created"
[engineer → EM: done: PR #456]   ←────
                                       EM: "Routing to architect"
[EM → architect: Review PR #456] ←────
                                       Architect: *reviewing*
[architect → EM: approved]       ←────
                                       EM: "Merging..."
[PR #456 merged to main]         ←────

You: *sips tea* ☕
```

---

## Why This Exists

**Problem:** Claude agents burn tokens while waiting. Spawn a worker, wait for completion, context grows, costs explode. And you can't see what subagents are doing.

**Solution:** A central manager that lets agents go idle and gives you full visibility. Spawn workers → go to sleep → wake on notification. Zero tokens burned while waiting. Every message mirrored to your Telegram.

```bash
# EM dispatches engineer, goes idle (no tokens burned)
sm dispatch engineer --role engineer --urgent --task "Implement ticket #123"

# Engineer works autonomously...
# ...finishes, notifies EM (AND you get a Telegram message)
sm send em-main "done: PR #456 created"

# EM wakes up (via notify-on-stop), routes PR to architect
sm dispatch architect --role architect --urgent --pr 456
```

**Result:** Complex multi-agent workflows at a fraction of the token cost. Full visibility from anywhere.

---

## What It Enables

### Agent Swarms
Spawn specialized agents that work in parallel. Engineer implements while Architect reviews while Scout investigates. All visible to you.

### Full Transparency
Every agent is a real Claude Code session. No black boxes. `sm attach` to any session. Or watch the conversation flow on Telegram.

### Async Orchestration
The EM (Engineering Manager) pattern: spawn workers, dispatch tasks, collect results. Never wait synchronously.

### Remote Control
On the go? Reply to Telegram messages to send input. Need to debug? `sm attach` from any terminal.

### Workspace Coordination
Auto-locking on file writes. Conflict detection. Multiple agents, one codebase, zero collisions.

### Token Efficiency
Agents sleep while waiting. Central manager handles coordination. Pay only for actual work.

---

## Quick Start

```bash
# Install
git clone https://github.com/rajeshgoli/session-manager
cd session-manager
./setup.sh

# Configure (add your Telegram bot token!)
cp config.yaml.example config.yaml
vim config.yaml

# Run
source venv/bin/activate
python -m src.server
```

### Setting Up Telegram (Recommended)

This is where the magic happens. 5 minutes to agent nirvana:

1. **Create a bot**: Message `@BotFather` on Telegram → `/newbot` → copy the token
2. **Get your chat ID**: Message your bot, then visit `https://api.telegram.org/bot<TOKEN>/getUpdates`
3. **Configure**:
   ```yaml
   telegram:
     token: "123456789:ABCdefGHIjklMNOpqrsTUVwxyz"
     allowed_chat_ids:
       - 123456789  # Your chat ID
   ```
4. **Restart the server**

Now every agent message flows to your phone. Reply to inject commands. True remote control.

---

## The SM CLI

Every managed session gets the `sm` command. This is how agents coordinate.

### Core Commands

| Command | Purpose |
|---------|---------|
| `sm spawn claude "<prompt>" --name X` | Spawn child agent |
| `sm send <id> "<text>"` | Send message to agent (+ Telegram) |
| `sm dispatch <id> --role X --task "..."` | Dispatch with auto-clear (primary EM command) |
| `sm em [name]` | EM pre-flight: set name + enable context monitoring |
| `sm clear <id>` | Clear agent context for reuse |
| `sm attach <id>` | Open agent session in your terminal |
| `sm children` | List your spawned agents |
| `sm tail <id>` | Recent tool actions with timestamps |
| `sm what <id>` | AI summary of what agent is doing |
| `sm kill <id>` | Terminate an agent |
| `sm output <id>` | See agent's recent output |

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

All modes forward to Telegram. You always see what's happening.

---

## The EM Pattern

The Engineering Manager orchestrates without doing implementation work.

```bash
# 1. Pre-flight (run first — sets name, context monitoring, registers children)
sm em epic-987

# 2. Spawn standby agents if none exist
sm spawn claude "As engineer, await tasks" --name engineer-standby
sm spawn claude "As architect, await tasks" --name architect-standby

# 3. Dispatch work (auto-clears before sending, arms periodic wake-up)
sm dispatch engineer-standby --role engineer --urgent --task "Implement ticket #123"
# EM goes idle — woken via notify-on-stop (done) or sm remind (still running, every ~210s)

# 4. Wake on notification, route to next agent
sm dispatch architect-standby --role architect --urgent --pr 456

# 5. Repeat until workflow complete
```

**Key insight:** EM's context is preserved across worker completions. Workers are disposable; EM maintains state. No polling — the system pages you.

---

## Telegram Commands

Control your swarm from anywhere.

| Command | Action |
|---------|--------|
| `/new [path]` | Spawn new session |
| `/list` | List active sessions |
| `/status <id>` | Get session status |
| `/kill <id>` | Terminate session |
| `/open <id>` | Open in Terminal.app (macOS) |
| Reply to message | Send input to that session |

**Session lifecycle notifications:**
- **Kill** → "Session stopped [id]" message sent to the thread, then topic closed (history preserved)
- **Clear** → "Context cleared [id] — ready for new task" message sent; thread stays open for the new task

**Pro tip:** Each session gets its own Telegram thread. Conversations stay organized even with 10 agents running.

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
│         ┌────────────────┼────────────────┐                 │
│         ▼                ▼                ▼                 │
│  ┌────────────┐   ┌────────────┐   ┌────────────┐          │
│  │   tmux     │   │  Telegram  │   │   Output   │          │
│  │ Controller │   │    Bot     │   │  Monitor   │          │
│  └─────┬──────┘   └─────┬──────┘   └────────────┘          │
└────────┼────────────────┼───────────────────────────────────┘
         │                │
         │                ▼
         │          📱 Your Phone
         │
         ▼
   ┌───────────────────────────────┐
   │      tmux sessions            │
   │  ┌─────────┐ ┌─────────┐     │
   │  │ Claude  │ │ Claude  │ ... │
   │  │ Agent 1 │ │ Agent 2 │     │
   │  └─────────┘ └─────────┘     │
   └───────────────────────────────┘
```

**Components:**
- **FastAPI Server** — REST API for session control
- **Message Queue** — SQLite-backed reliable delivery
- **Lock Manager** — Workspace coordination
- **tmux Controller** — Session lifecycle management
- **Telegram Bot** — Remote visibility and control
- **Output Monitor** — Detects idle, errors, permission prompts

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

telegram:
  token: "BOT_TOKEN"           # From @BotFather
  allowed_chat_ids: [123456789] # Your chat ID
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

## Example Workflows (Add to Your CLAUDE.md)

Copy these patterns into your project's `CLAUDE.md` to enable agent swarms.

### The EM (Engineering Manager) Pattern

```markdown
## Multi-Agent Workflows

When asked to work "as EM" or orchestrate complex tasks, use the session manager.

### Core Principle

**Dispatch and go idle.** Spawn workers, go idle, wake on notification. Never poll.

### Pre-Flight

```bash
sm em <epic>   # Mandatory first step: sets name, enables context monitoring for self + children
sm children    # Check existing agents (reuse before spawning)
```

### Spawn Templates (only if no agent of that type exists)

**Scout (Investigation):**
```bash
sm spawn claude "As scout, await tasks" --name "scout-<task>"
sm dispatch scout-<task> --role scout --urgent \
  --task "Investigate <problem>. Write findings to docs/working/<name>.md. Do NOT fix code." \
  --repo <path>
```

**Engineer (Implementation):**
```bash
sm spawn claude "As engineer, await tasks" --name "engineer-ticket<N>"
sm dispatch engineer-ticket<N> --role engineer --urgent \
  --task "Implement ticket #<N>. Read spec at docs/working/<spec>.md." \
  --repo <path> --spec docs/working/<spec>.md
```

**Architect (Review):**
```bash
sm spawn claude "As architect, await tasks" --name "architect-pr<N>"
sm dispatch architect-pr<N> --role architect --urgent --pr <N> --repo <path>
```

`sm dispatch` handles clear-before-send automatically. EM goes idle after dispatch — woken via notify-on-stop (agent done) or sm remind (agent still running, ~210s intervals).

### Workflow: Implement an Epic

```
"As EM, implement epic #<number>"
```

1. `sm em <epic>` — pre-flight
2. Read epic to understand scope
3. For each ticket (ONE AT A TIME):
   - Dispatch Engineer → implement, create PR
   - Dispatch Architect → review PR
   - If changes needed → clear engineer, re-dispatch fresh with findings baked in
   - If approved → Architect merges
4. Close epic: `gh issue close <epic#> --comment "All sub-issues complete: ..."`

### Workflow: Investigate and Spec

```
"As EM, investigate <problem> and create a spec"
```

1. `sm em <task>` — pre-flight
2. Dispatch Scout → investigation, write spec at `docs/working/<n>_<name>.md`
3. Scout sends spec to Architect agent for review via `sm send`
4. Scout and Architect iterate directly (EM tiebreaks only if escalated)
5. On convergence, Scout pushes spec, notifies EM
6. Notify human: ready for review

### Communication Patterns

- **Status updates:** `sm send $EM_ID "done: PR #123 created"`
- **Urgent corrections:** `sm send $ID "UPDATE: use X instead" --urgent`
- **Reports go in files:** Write to `docs/working/`, then notify

### Circuit Breaker

Pause and alert human when:
- Tests fail unexpectedly
- Agent stuck in loop (multiple reminds, no progress)
- Unclear how to proceed

```
EM: "Circuit breaker triggered. <reason>. Awaiting guidance."
```
```

### Workspace Coordination

Add to your `.claude/settings.json` for auto-locking:

```json
{
  "hooks": {
    "PreToolUse": [{
      "matcher": { "tool_name": "write|edit" },
      "hooks": [{ "type": "command", "command": "sm auto-lock" }]
    }]
  }
}
```

### Quick Reference

| Command | When to Use |
|---------|-------------|
| `sm em [name]` | EM pre-flight: name + context monitoring |
| `sm spawn claude "..." --name X` | Start a worker agent |
| `sm dispatch <id> --role X --task "..."` | Dispatch task to agent (auto-clears) |
| `sm send <id> "..." --urgent` | Manual send (follow-ups, one-liners) |
| `sm clear <id>` | Reset agent for new task (sm dispatch does this) |
| `sm children` | See your spawned agents + status |
| `sm tail <id>` | Recent tool actions (fast, no haiku) |
| `sm what <id>` | AI summary of agent activity (last resort) |
| `sm attach <id>` | Jump into agent's session |
| `sm output <id>` | See agent's recent output |
| `sm kill <id>` | Terminate agent |

---

## Requirements

- macOS (Linux support planned)
- Python 3.11+
- tmux (`brew install tmux`)
- Claude Code CLI
- Telegram account (for remote visibility)

---

## License

MIT

---

## Contributing

Issues and PRs welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

---

**Built for the age of AI agents.** When one Claude isn't enough.

*Let them swarm. Watch from anywhere. Jump in when needed. This is agent nirvana.* 🏖️
