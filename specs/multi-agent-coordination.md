# Multi-Agent Coordination Specification

**Version:** 1.0
**Status:** Draft
**Last Updated:** 2026-01-27

## Overview

This specification describes a comprehensive multi-agent coordination system where Claude Code agents can spawn, monitor, and manage child agents through the Session Manager, treating spawned agents as first-class managed sessions rather than opaque background processes.

## Motivation

### Current Limitations

1. **Hook Reliability**: Claude Code's SubagentStart/SubagentStop hooks are unreliable when `sm` command isn't in PATH
2. **Black Box Operation**: Spawned subagents run in background with no visibility
3. **Limited Control**: Parent agents can't easily monitor, communicate with, or terminate children
4. **No Integration**: Subagents don't appear in Telegram, `sm` commands, or monitoring tools
5. **Debugging Difficulty**: Can't attach to subagent sessions or view their transcripts

### Proposed Solution

Treat spawned agents as **full Session Manager sessions**:
- Create dedicated tmux sessions for each child agent
- Track parent-child relationships in Session Manager
- Provide full visibility and control through existing `sm` commands
- Enable direct access via tmux, Telegram, and APIs
- Support structured progress monitoring and lifecycle management

## Architecture

### Session Hierarchy

```
Parent Session (em-epic1041)
├── Child Session (engineer-task1042)
├── Child Session (engineer-task1043)
└── Child Session (architect-review)
    └── Grandchild Session (engineer-fix)
```

### Data Model Extensions

#### Session Model
```python
@dataclass
class Session:
    # Existing fields...
    parent_session_id: Optional[str] = None  # Parent that spawned this session
    spawn_prompt: Optional[str] = None       # Initial prompt used to spawn
    agent_type: Optional[str] = None         # Engineer, Architect, Explore, etc.
    completion_status: Optional[str] = None  # completed, error, abandoned, killed
    completion_message: Optional[str] = None # Message when completed

    # Lifecycle tracking
    spawned_at: Optional[datetime] = None
    completed_at: Optional[datetime] = None

    # Progress tracking
    tokens_used: int = 0
    tools_used: dict[str, int] = field(default_factory=dict)  # {"Read": 5, "Write": 3}
    checkpoints: list[Checkpoint] = field(default_factory=list)
```

#### Checkpoint Model
```python
@dataclass
class Checkpoint:
    """Progress milestone reported by agent."""
    timestamp: datetime
    message: str
    metadata: Optional[dict] = None  # tokens_at_checkpoint, etc.
```

#### SessionEvent Model
```python
@dataclass
class SessionEvent:
    """Lifecycle event for parent tracking."""
    session_id: str
    event_type: str  # spawned, completed, error, checkpoint, idle
    timestamp: datetime
    message: str
    metadata: Optional[dict] = None
```

## Commands

### `sm spawn` - Create Child Agent

**Syntax:**
```bash
sm spawn <agent-type> "<prompt>" [options]
```

**Arguments:**
- `agent-type`: Engineer, Architect, Explore, general-purpose, or custom
- `prompt`: Initial prompt/task for the child agent

**Options:**
- `--model <model>`: Override default model (opus, sonnet, haiku)
- `--args "<args>"`: Additional Claude Code arguments
- `--preset <name>`: Use named preset from config
- `--parent <session-id>`: Explicit parent (defaults to current session)
- `--working-dir <path>`: Override working directory
- `--name <friendly-name>`: Set friendly name
- `--json`: Return JSON output

**Returns:**
```json
{
  "session_id": "abc123",
  "name": "engineer-abc123",
  "friendly_name": "engineer-task1042",
  "agent_type": "Engineer",
  "working_dir": "/path/to/project",
  "parent_session_id": "1749a2fe",
  "tmux_session": "claude-abc123",
  "created_at": "2026-01-27T15:30:00Z"
}
```

**Example:**
```bash
# Basic spawn
$ sm spawn Engineer "Implement API endpoint for user login"
Spawned engineer-abc123 (abc123) in tmux session claude-abc123

# With options
$ sm spawn Architect "Review PR #1042" --model opus --name architect-pr1042

# With preset
$ sm spawn Engineer "Fix bug" --preset quick-fix
```

### `sm children` - List Child Sessions

**Syntax:**
```bash
sm children [session-id] [--recursive] [--status <status>]
```

**Options:**
- `session-id`: Parent session (defaults to current)
- `--recursive`: Include grandchildren, great-grandchildren, etc.
- `--status <status>`: Filter by status (running, completed, error, all)
- `--json`: Return JSON output

**Example:**
```bash
$ sm children 1749a2fe
engineer-task1042 (abc123) | completed | 5min ago | "Feature implemented"
engineer-task1043 (def456) | running   | 2min ago | In progress
architect-review (ghi789)  | completed | 10min ago | "Approved"

$ sm children 1749a2fe --recursive
engineer-task1042 (abc123) | completed | 5min ago
  └─ engineer-fix (xyz111) | completed | 3min ago
engineer-task1043 (def456) | running   | 2min ago
```

### `sm progress` - Monitor Child Progress

**Syntax:**
```bash
sm progress <session-id> [--json] [--watch]
```

**Output:**
```bash
$ sm progress def456

engineer-task1043 (def456) | working | 3min elapsed
Tokens: 2,450 / ~10,000 est (24%)
Tools: Read(5) Write(3) Bash(2) Edit(1)
Status: Writing unit tests for API endpoint
Last tool: Write → tests/api.test.ts (10s ago)

Checkpoints:
  [15:10] Started: Implement feature X
  [15:12] Completed data model (3/5 tasks)
  [15:14] Writing tests
```

**JSON Output:**
```json
{
  "session_id": "def456",
  "status": "working",
  "elapsed_seconds": 180,
  "tokens_used": 2450,
  "tokens_remaining_estimate": 7550,
  "completion_percentage": 24,
  "tools_used": {
    "Read": 5,
    "Write": 3,
    "Bash": 2,
    "Edit": 1
  },
  "last_tool": {
    "name": "Write",
    "args": {"file_path": "tests/api.test.ts"},
    "timestamp": "2026-01-27T15:20:30Z",
    "seconds_ago": 10
  },
  "current_activity": "Writing unit tests for API endpoint",
  "checkpoints": [
    {"timestamp": "2026-01-27T15:10:00Z", "message": "Started: Implement feature X"},
    {"timestamp": "2026-01-27T15:12:00Z", "message": "Completed data model (3/5 tasks)"}
  ],
  "is_idle": false,
  "is_waiting_input": false,
  "is_complete": false
}
```

**Watch Mode:**
```bash
$ sm progress def456 --watch
# Updates every 5 seconds, shows live progress
```

### `sm checkpoint` - Report Progress Milestone

**Syntax:**
```bash
sm checkpoint "<message>" [--metadata key=value ...]
```

**Example:**
```bash
# In child agent
$ sm checkpoint "Phase 1 complete: Data model implemented (3/5 tasks done)"
Checkpoint recorded

$ sm checkpoint "Starting tests" --metadata coverage=0
Checkpoint recorded
```

### `sm checkpoints` - List Checkpoints

**Syntax:**
```bash
sm checkpoints <session-id> [--json]
```

**Example:**
```bash
$ sm checkpoints def456
[15:10] Started: Implement feature X
[15:12] Phase 1 complete: Data model implemented (3/5 tasks done)
[15:14] Starting tests
[15:16] Tests written, running validation
```

### `sm complete` - Signal Completion

**Syntax:**
```bash
sm complete ["<message>"] [--status <status>]
```

**Options:**
- `message`: Completion message/summary
- `--status`: completed (default), error, abandoned

**Example:**
```bash
# In child agent
$ sm complete "Feature X implemented successfully. All 26 tests passing."
Session marked complete. Notifying parent.

$ sm complete "Unable to proceed without API keys" --status error
Session marked as error. Notifying parent.
```

### `sm events` - View Session Events

**Syntax:**
```bash
sm events <session-id> [--type <type>] [--limit N]
```

**Example:**
```bash
$ sm events 1749a2fe
[15:10] spawned: Child engineer-abc123 (abc123) started
[15:15] completed: Child abc123 completed - "Feature X done"
[15:16] spawned: Child explore-xyz (789abc) started
[15:18] checkpoint: Child 789abc - "Found 15 TypeScript files"
[15:20] idle: Child 789abc idle for 5 minutes
```

### `sm send` - Send Input to Session

**Syntax:**
```bash
sm send <session-id> "<text>"
```

**Example:**
```bash
# Parent sends input to child
$ sm send def456 "Add error handling for network failures"
Input sent to session def456
```

## Configuration

### Global Config (`config.yaml`)

```yaml
claude:
  # Default command and arguments for Claude Code
  default_command: "claude"
  default_args:
    - "--bypass-permissions"

  # Per-agent-type configurations
  agent_configs:
    Engineer:
      model: "sonnet"
      args:
        - "--bypass-permissions"

    Architect:
      model: "opus"
      args:
        - "--plan"

    Explore:
      model: "haiku"
      args:
        - "--bypass-permissions"

    general-purpose:
      model: "sonnet"
      args: []

  # Named presets
  presets:
    quick-fix:
      model: "sonnet"
      args: ["--bypass-permissions", "--compact"]

    deep-analysis:
      model: "opus"
      args: ["--plan"]

child_agents:
  # Lifecycle mode: auto | manual | supervised
  mode: "auto"

  # Auto-completion detection
  auto_complete:
    enabled: true
    idle_timeout: 600  # Seconds (10 minutes)
    detect_completion_phrases: true
    completion_patterns:
      - "complete"
      - "done"
      - "finished"
      - "all tests pass"

  # Cleanup behavior
  cleanup:
    auto_kill_on_complete: false      # Keep tmux session running
    auto_archive_transcript: true     # Save transcript on completion
    archive_path: "/tmp/claude-sessions/archives"

  # Parent notifications
  notifications:
    notify_parent_on_complete: true
    notify_parent_on_error: true
    notify_parent_on_idle: true
    notify_parent_on_checkpoint: false  # Too noisy

  # Progress tracking
  progress:
    enable_token_tracking: true
    enable_tool_tracking: true
    snapshot_interval: 30  # Seconds between progress snapshots

# Session Manager integration
sessions:
  default_working_dir_behavior: "inherit"  # inherit | specify
  inherit_environment_vars:
    - "SSH_AUTH_SOCK"
    - "PATH"

  # Session naming
  naming:
    pattern: "{agent_type}-{friendly_name}"  # e.g., engineer-task1042
    fallback: "{agent_type}-{short_id}"      # e.g., engineer-abc123
```

### Per-Session Override

Child sessions can override config via spawn arguments:

```bash
sm spawn Engineer "Task" --model opus  # Override model
```

## Lifecycle Management

### Lifecycle States

```
spawned → starting → running → [waiting_input | idle] → terminal_state

Terminal States:
- completed: Successfully finished
- error: Failed with error
- abandoned: Parent killed or abandoned
- killed: Force terminated
```

### State Transitions

**spawned → starting:**
- Session created, tmux session starting
- Claude Code process launching

**starting → running:**
- Claude Code started successfully
- Agent begins processing prompt

**running → waiting_input:**
- Agent asks user a question
- Waiting for parent response

**running → idle:**
- No tool use for N seconds
- May indicate completion or stuck

**running → completed:**
- Agent signals completion via `sm complete`
- OR: Auto-detected via completion patterns
- OR: Idle timeout with completion heuristics

**running → error:**
- Agent signals error via `sm complete --status error`
- OR: Session crashes
- OR: tmux session dies unexpectedly

**any → killed:**
- Parent runs `sm kill <id>`
- OR: Session Manager shutdown

**any → abandoned:**
- Parent session ends without cleanup
- OR: Parent-child relationship broken

### Lifecycle Modes

#### Auto Mode (Recommended)

**Behavior:**
- Automatically detects completion via patterns and idle timeout
- Notifies parent on state changes
- Optionally archives transcript and cleans up

**Configuration:**
```yaml
child_agents:
  mode: "auto"
  auto_complete:
    enabled: true
    idle_timeout: 600
    detect_completion_phrases: true
```

**Example Flow:**
1. Child completes work, Claude says "Done. All tests passing."
2. Session Manager detects completion phrase
3. Marks session as completed
4. Notifies parent: "Child abc123 completed"
5. Archives transcript (optional)
6. Keeps tmux session (configurable)

#### Manual Mode

**Behavior:**
- Parent must explicitly manage lifecycle
- No auto-detection or cleanup
- Sessions persist until explicitly killed

**Configuration:**
```yaml
child_agents:
  mode: "manual"
  auto_complete:
    enabled: false
```

**Example Flow:**
1. Parent spawns child
2. Parent periodically checks `sm progress`
3. When satisfied, parent runs `sm kill <id>`
4. Parent manages all cleanup

#### Supervised Mode

**Behavior:**
- Auto-detection enabled
- Requires parent approval before termination
- Session Manager prompts parent for decisions

**Configuration:**
```yaml
child_agents:
  mode: "supervised"
  auto_complete:
    enabled: true
    require_parent_approval: true
```

**Example Flow:**
1. Child appears complete (idle + completion phrase)
2. Session Manager notifies parent: "Child abc123 appears complete. Approve termination?"
3. Parent confirms: `sm approve abc123` or `sm reject abc123`
4. Session terminated or continues

### Termination Handling

**Graceful Shutdown:**
```bash
# Send Escape to interrupt gracefully
sm kill def456

# Or force immediate kill
sm kill def456 --force
```

**Cleanup Options:**
```yaml
cleanup:
  auto_kill_on_complete: false  # Keep tmux session
  auto_archive_transcript: true # Save transcript
  close_tmux_session: false     # Don't destroy tmux
```

**Parent Responsibilities:**
- Check child status before terminating
- Archive important information
- Update own state based on child results

## Progress Monitoring

### Data Sources

**1. Transcript Analysis**
- Parse `.jsonl` transcript for tool uses
- Count tokens from Claude responses
- Extract completion signals

**2. Session Status**
- Current state: running, idle, waiting_input
- Last activity timestamp
- Error messages

**3. tmux Output**
- Recent output (last 50 lines)
- Current activity indicators

**4. Explicit Checkpoints**
- Agent-reported milestones
- Structured progress updates

### Progress Metrics

**Token Usage:**
```json
{
  "tokens_used": 2450,
  "tokens_remaining_estimate": 7550,
  "completion_percentage": 24
}
```

**Tool Usage:**
```json
{
  "tools_used": {
    "Read": 5,
    "Write": 3,
    "Bash": 2,
    "Edit": 1
  },
  "total_tools": 11,
  "last_tool": {
    "name": "Write",
    "timestamp": "2026-01-27T15:20:30Z"
  }
}
```

**Activity Status:**
```json
{
  "current_activity": "Writing unit tests",
  "is_idle": false,
  "idle_seconds": 0,
  "is_waiting_input": false
}
```

### Parent Monitoring Patterns

**Polling Pattern:**
```bash
# Check every 30 seconds
while true; do
  status=$(sm progress def456 --json)

  if [ "$(echo $status | jq -r '.is_complete')" = "true" ]; then
    break
  fi

  sleep 30
done
```

**Event-Driven Pattern:**
```bash
# Listen for events
sm events 1749a2fe --follow | while read event; do
  if echo $event | grep -q "completed"; then
    # Handle child completion
  fi
done
```

**Notification Pattern:**
- Session Manager sends notification when child state changes
- Parent receives via Telegram, webhook, or polling

## Parent-Child Communication

### Parent → Child

**Direct Input:**
```bash
sm send def456 "Add error handling"
```

**Task Update:**
```bash
sm send def456 "Update: Requirements changed. Use JWT instead of sessions."
```

**Query Status:**
```bash
sm send def456 "Status update?"
```

### Child → Parent

**Checkpoints:**
```bash
sm checkpoint "Phase 1 complete"
```

**Completion:**
```bash
sm complete "Task done. All tests passing."
```

**Questions:**
```bash
# Child uses AskUserQuestion tool
# Parent receives notification
# Parent responds via sm send
```

### Shared Context

**File System:**
- Both access same working directory
- Child can read parent's files
- Parent can read child's output

**Session Manager:**
- Shared state via API
- Parent queries child status
- Child reports to parent

## Integration with Existing Features

### Telegram Integration

**Child sessions appear in `/list`:**
```
/list

Active Sessions:
- em-epic1041 (1749a2fe) - Working on epic
- ├─ engineer-task1042 (abc123) - Implementing feature
- └─ architect-review (def456) - Reviewing PR
```

**Separate threads:**
- Each child gets own forum topic (if forum enabled)
- Or reply chain for non-forum groups

**Commands work:**
```
/status (in child thread) → Shows child status
/subagents 1749a2fe → Shows children (same as sm children)
```

### tmux Access

**Direct attachment:**
```bash
tmux attach -t claude-abc123
```

**List sessions:**
```bash
tmux ls | grep claude-
claude-1749a2fe: 1 windows (created Mon Jan 27 15:00:00 2026)
claude-abc123: 1 windows (created Mon Jan 27 15:10:00 2026)
claude-def456: 1 windows (created Mon Jan 27 15:12:00 2026)
```

### Existing `sm` Commands

**All commands work on child sessions:**
```bash
sm me              # Works if run inside child
sm who             # Shows siblings (other children of same parent)
sm what def456     # Get AI summary of child
sm others          # Shows other agents in workspace (including children)
sm all             # Shows all sessions (including children)
sm status          # Shows full status including children
```

## Implementation Phases

### Phase 1: Basic Spawning (MVP)
- [ ] `sm spawn` command implementation
- [ ] Parent-child relationship tracking
- [ ] Basic lifecycle (manual mode)
- [ ] `sm children` command
- [ ] Configurable Claude command/args

**Deliverables:**
- Can spawn child sessions
- Children appear in `sm all`
- Manual lifecycle management
- Parent-child tracking

### Phase 2: Progress Monitoring
- [ ] Transcript parsing for tokens/tools
- [ ] `sm progress` command
- [ ] `sm checkpoint` command
- [ ] `sm checkpoints` command
- [ ] Progress metrics API

**Deliverables:**
- Real-time progress visibility
- Token and tool usage tracking
- Milestone reporting

### Phase 3: Auto Lifecycle
- [ ] Completion detection heuristics
- [ ] Auto mode implementation
- [ ] Parent notifications
- [ ] `sm complete` command
- [ ] Event system

**Deliverables:**
- Automatic completion detection
- Parent event notifications
- Clean lifecycle management

### Phase 4: Advanced Features
- [ ] Supervised mode
- [ ] Progress estimation
- [ ] Anomaly detection
- [ ] `sm events --follow` (streaming)
- [ ] Telegram integration enhancements

**Deliverables:**
- Supervised lifecycle
- Predictive completion
- Alert system

## Open Questions

1. **How to handle nested spawning limits?**
   - Max depth of parent-child hierarchy?
   - Resource limits per branch?

2. **Transcript access patterns?**
   - Should parent automatically read child transcripts?
   - Privacy/isolation considerations?

3. **Cost tracking?**
   - Track tokens/costs per child?
   - Aggregate costs for parent + all children?

4. **Failure handling?**
   - What if child crashes during spawn?
   - How to handle orphaned children?

5. **Concurrent children?**
   - Limits on how many children per parent?
   - Resource management?

6. **Working directory?**
   - Always inherit parent's working_dir?
   - Allow override per spawn?

7. **Environment inheritance?**
   - Which env vars should children inherit?
   - Security implications?

8. **Completion detection accuracy?**
   - How to avoid false positives?
   - Training data needed?

## Success Metrics

- **Visibility**: 100% of spawned agents visible in `sm all`
- **Control**: Parent can kill/monitor any child
- **Reliability**: <1% spawn failure rate
- **Performance**: Spawn latency <2 seconds
- **Accuracy**: >95% correct auto-completion detection
- **Usability**: Parent monitoring requires <5 commands

## Appendix

### Example End-to-End Workflow

**Scenario:** EM spawns multiple engineers for parallel implementation

```bash
# EM session (1749a2fe)
$ sm spawn Engineer "Implement #1042: PivotExtendedEvent" --name eng-1042
Spawned engineer-eng-1042 (abc123)

$ sm spawn Engineer "Implement #1043: SignalResolver" --name eng-1043
Spawned engineer-eng-1043 (def456)

# Check progress
$ sm children 1749a2fe
engineer-eng-1042 (abc123) | running | 2min ago
engineer-eng-1043 (def456) | running | 1min ago

# Monitor detailed progress
$ sm progress abc123
engineer-eng-1042 (abc123) | working | 5min elapsed
Tokens: 1,200 / ~5,000 est (24%)
Tools: Read(3) Write(2) Edit(1)
Status: Implementing PivotExtendedEvent class
Checkpoints:
  [15:10] Started implementation
  [15:13] Created base class structure

# Wait for completion
$ sm events 1749a2fe --follow
[15:18] completed: Child abc123 completed - "PivotExtendedEvent implemented"
[15:20] completed: Child def456 completed - "SignalResolver extracted"

# Review results
$ sm children 1749a2fe
engineer-eng-1042 (abc123) | completed | 8min ago | "PivotExtendedEvent implemented"
engineer-eng-1043 (def456) | completed | 6min ago | "SignalResolver extracted"

# Spawn architect to review
$ sm spawn Architect "Review #1042 and #1043" --name review
Spawned architect-review (ghi789)
```

### References

- Session Manager Architecture: `README.md`
- API Endpoints: `src/server.py`
- CLI Commands: `src/cli/commands.py`
- Configuration: `config.yaml.example`
