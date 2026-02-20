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
    completion_status: Optional[str] = None  # completed, error, abandoned, killed
    completion_message: Optional[str] = None # Message when completed

    # Lifecycle tracking
    spawned_at: Optional[datetime] = None
    completed_at: Optional[datetime] = None

    # Progress tracking (auto-detected from transcript)
    tokens_used: int = 0
    tools_used: dict[str, int] = field(default_factory=dict)  # {"Read": 5, "Write": 3}
    last_tool_call: Optional[datetime] = None
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

**Essential commands for parent agents:**

1. **`sm spawn "<prompt>" --wait N`** - Spawn child and get notified when done/idle
2. **`sm kill <id>`** - Terminate YOUR child session (only allows killing own children)
3. **`sm what <id>`** - Check status (rarely needed - for fire-and-forget spawns)
4. **`sm children`** - List all YOUR children (optional)
5. **`sm send <id> "text"`** - Send input to any session (optional)

**No special commands needed for child agents** - session manager auto-detects completion, progress, and errors by monitoring the tmux transcript.

### `sm spawn` - Create Child Agent

**Syntax:**
```bash
sm spawn "<prompt>" [options]
```

**Arguments:**
- `prompt`: Full prompt for the child agent (e.g., "As engineer, implement epic #1042 per docs/spec.md")

**Options:**
- `--model <model>`: Optional override of default model (opus, sonnet, haiku). Defaults to your config.yaml setting.
- `--wait <seconds>`: Monitor child and notify parent when complete or idle for N seconds. Command returns immediately.
- `--name <friendly-name>`: Set friendly name for the child session
- `--working-dir <path>`: Override working directory (defaults to parent's directory)
- `--json`: Return JSON output

**Note:** Agents can optionally override the model for task-specific needs. The Claude Code command and arguments are user-controlled via `config.yaml` and cannot be overridden by agents.

**Wait Behavior:**
When `--wait N` is specified:
1. `sm spawn` returns immediately with child ID
2. Session manager monitors child in background
3. When child completes OR is idle for N seconds:
   - Session manager sends notification to parent's Claude input
   - Notification contains: child ID, status, completion message/summary
   - Parent wakes from idle with completion info in context

**Non-blocking pattern:** Parent doesn't burn tokens waiting - it continues working or goes idle, and session manager notifies when child is done.

**Returns:**
```json
{
  "session_id": "abc123",
  "name": "child-abc123",
  "friendly_name": "task1042-impl",
  "working_dir": "/path/to/project",
  "parent_session_id": "1749a2fe",
  "tmux_session": "claude-abc123",
  "created_at": "2026-01-27T15:30:00Z"
}
```

**Example:**
```bash
# Fire-and-forget: spawn without monitoring
$ sm spawn "As engineer, implement user login API endpoint per spec docs/auth.md"
Spawned child-abc123 (abc123) in tmux session claude-abc123

# Spawn with notification: returns immediately, parent notified later (EM pattern)
$ sm spawn "As architect, review and merge PR #1042" --wait 600
Spawned child-xyz789 (xyz789) in tmux session claude-xyz789
# Command returns, parent continues or goes idle
# Later, when child completes or 600s idle:
[Session manager sends to parent's input]:
"Child child-xyz789 completed: PR #1042 approved and merged"

# Agent overrides to opus for complex task requiring deep reasoning
$ sm spawn "As engineer, design and implement distributed lock mechanism for cache layer" --model opus --wait 1200
Spawned child-def456 (def456)

# Uses user's default model from config.yaml
$ sm spawn "Fix typo in README.md line 42" --wait 300
Spawned child-ghi789 (ghi789)
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

### `sm kill` - Terminate Child Session

**Syntax:**
```bash
sm kill <session-id>
```

**Behavior:**
- Terminates the tmux session (ends the shell)
- Equivalent to user typing `/exit` in Claude Code
- Cleanly shuts down the child session

**Security:**
- **ONLY allows killing your own children** - cannot kill arbitrary sessions
- Checks parent-child relationship before terminating
- Prevents accidental/malicious termination of unrelated sessions
- Returns error if session is not your child

**Example:**
```bash
# Terminate your child session
$ sm kill def456
Session def456 terminated

# Trying to kill unrelated session
$ sm kill xyz789
Error: Cannot kill session xyz789 - not your child session
```

**Use Case:**
- Child is stuck or taking too long
- Need to cancel work in progress
- Error detected, need to abort
- Resource cleanup

**Safety:** This prevents a "virtual bloodbath" - misbehaving agents cannot kill other agents' sessions or unrelated work.

### `sm what` - Check Child Status

**Note:** Use existing `sm what` command to check on child sessions.

**Syntax:**
```bash
sm what <session-id> [--deep]
```

**Example:**
```bash
$ sm what def456
Writing unit tests for API endpoint. Last activity 30s ago.

$ sm what def456 --deep
Writing unit tests for API endpoint. Last activity 30s ago.

Recent tools: Write(tests/api.test.ts), Read(src/api.ts), Bash(npm test)
Tokens used: ~2,450
Elapsed: 3min
```

**Use Case:**
Rarely needed - typically use `sm spawn --wait` which notifies parent automatically. Use `sm what` only when:
- Checking on fire-and-forget spawns
- Investigating why child hasn't completed
- Manual debugging

### `sm send` - Send Input to Any Session (Optional)

**Syntax:**
```bash
sm send <session-id> "<text>" [--sequential|--important|--urgent]
```

**Arguments:**
- `session-id`: Any session managed by Session Manager (not just your children)
- `text`: Text to send to the session's Claude input

**Delivery Modes:**

| Mode | Behavior | Use Case |
|------|----------|----------|
| `--sequential` (default) | Wait for agent to be idle/at prompt, then inject | Normal handoff, safe coordination |
| `--important` | Inject immediately, queue behind current work | Time-sensitive but not critical |
| `--urgent` | Interrupt immediately, inject now | Emergency stop, critical correction |

**Default behavior** (`--sequential`): Waits for the agent to finish current work and reach an idle state or input prompt before injecting. Prevents interrupting mid-thought.

**Example:**
```bash
# Normal handoff (waits for idle) - DEFAULT
$ sm send engineer-1042 "Now implement feature Y"
Queued for engineer-1042 (will inject when idle)

# Explicit sequential mode
$ sm send def456 "Add error handling" --sequential
Queued for def456 (will inject when idle)

# Important (inject immediately, but agent may be busy)
$ sm send def456 "Consider using async/await pattern" --important
Input sent to def456

# Urgent (interrupt current work)
$ sm send engineer-1042 "STOP - you're on the wrong branch!" --urgent
Input sent to engineer-1042 (interrupted)

# Any agent can send to any other session
$ sm send 1749a2fe "The database migration is complete, you can proceed"
Queued for 1749a2fe (will inject when idle)
```

**Use Case:**
Rarely needed. Useful for:
- Parent resuming child with new work (default: wait for idle)
- Providing additional context mid-task (use `--important`)
- Emergency course-correction (use `--urgent`)
- Coordinating between parallel agents (default: wait for idle)

## Auto-Detection

**Session manager automatically detects everything by monitoring the child's tmux session transcript. No special commands needed from child agents.**

### Completion Detection

Session manager marks a child as completed when:

1. **Idle timeout reached** (from `--wait N` parameter)
   - No tool calls for N seconds
   - Extracts last message from transcript as completion summary

2. **Completion patterns detected** in transcript:
   - "Done", "Complete", "Finished"
   - "All tests passing", "Tests pass"
   - "PR merged", "Committed and pushed"
   - Session exits cleanly

3. **Error patterns detected** in transcript:
   - "Error:", "Failed:", "Cannot proceed"
   - Exceptions, stack traces
   - Session crashes

**Parent notification:**
When any completion condition triggers, session manager sends message to parent's Claude input:
```
Child child-abc123 completed: Feature X implemented. PR #1042 created. All tests passing.
```

### Progress Tracking

Session manager auto-tracks from transcript:

- **Tool usage**: Count Read, Write, Edit, Bash, etc. calls
- **Last activity**: Time since last tool call
- **Tokens used**: Approximate from transcript length
- **Current status**: Extract from latest AI response

No special commands needed - all extracted automatically.

## Workflow Patterns

### Pattern 1: Spawn with Notification (EM Orchestration)

**Use case:** Parent needs child's result to proceed (spec review, PR merge, investigation)

**How it works:**
1. Parent spawns child with `--wait N` (command returns immediately)
2. Parent continues working or goes idle (doesn't burn tokens)
3. Session manager monitors child in background
4. When child completes OR N seconds idle:
   - Session manager sends notification to parent's Claude input
   - Notification contains: child ID, status, completion summary
   - Parent wakes from idle with result in context and continues

**Example: EM orchestrating Engineer → Architect flow:**
```bash
# EM spawns engineer with notification
$ sm spawn "As engineer, read docs/working/sessionmanager.md and implement ticket #1042. Pay special attention to timeout handling." --wait 600
Spawned child-abc123 (abc123)
# Command returns, EM goes idle (not burning tokens)

# 5 minutes later, engineer finishes
[Session manager sends to EM's input]:
"Child child-abc123 completed: Implemented feature X. PR #1042 created. All tests passing."

# EM wakes from idle, continues with architect review
$ sm spawn "As architect, review PR #1042. Focus on error handling and test coverage." --wait 600
Spawned child-def456 (def456)
# EM goes idle again

# Architect finishes
[Session manager sends to EM's input]:
"Child child-def456 completed: Approved. Merged to dev."

# EM wakes, proceeds to next ticket
```

**Benefits:**
- Parent preserves context without burning tokens (goes idle while child works)
- Automatic notification when work completes
- Simple sequential orchestration
- Timeout handling (gets notified after N seconds even if child stuck)
- Non-blocking: parent can do other work while waiting for notification

### Pattern 2: Spawn and Forget

**Use case:** Fire parallel tasks, collect results later (rare)

**How it works:**
1. Parent spawns multiple children without `--wait`
2. Parent continues immediately
3. Later, parent checks status with `sm children` or `sm what`

**Example:**
```bash
# Spawn multiple exploratory tasks
$ sm spawn "List all API endpoints" --name explore-apis --model haiku
$ sm spawn "Find all database queries" --name explore-db --model haiku
$ sm spawn "Map authentication flow" --name explore-auth

# Continue working on something else...

# Later, check results
$ sm children
explore-apis (abc123) | completed | 5min ago
explore-db (def456) | completed | 3min ago
explore-auth (ghi789) | running | 1min ago

$ sm what abc123
Found 47 API endpoints. Results written to docs/api-inventory.md
```

**Benefits:**
- Parallel exploration
- Don't block parent
- Collect results asynchronously

**Drawbacks:**
- Parent must remember to check
- More context overhead tracking multiple children
- Less common pattern

### Pattern Recommendation

**Default to spawn-with-notification** (`--wait`) for most cases. It's simpler, preserves parent context without burning tokens, and matches the EM orchestration pattern. Only use spawn-and-forget when truly parallelizing independent work where results aren't needed immediately.

## Configuration

### Global Config (`config.yaml`)

```yaml
claude:
  # User-controlled: How to spawn Claude Code sessions
  # Agents CANNOT override command or args - only model
  command: "claude"
  args:
    - "--bypass-permissions"

  # Default model for spawned sessions (agent can override with --model flag)
  default_model: "sonnet"

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

  # Parent notifications (via sending text to parent's Claude input)
  notifications:
    notify_parent_on_complete: true
    notify_parent_on_error: true
    notify_parent_on_idle: true  # When --wait timeout reached

  # Progress tracking (auto-detected from transcript)
  progress:
    enable_token_tracking: true     # Approximate from transcript length
    enable_tool_tracking: true      # Count tool calls in transcript
    snapshot_interval: 30           # Seconds between checks

# Session Manager integration
sessions:
  default_working_dir_behavior: "inherit"  # inherit | specify
  inherit_environment_vars:
    - "SSH_AUTH_SOCK"
    - "PATH"

  # Session naming
  naming:
    pattern: "{friendly_name}"               # e.g., task1042-fix
    fallback: "child-{short_id}"             # e.g., child-abc123
```

### Agent Control vs User Control

**User-controlled (configured in `config.yaml` only):**
- Claude Code command (e.g., `claude`)
- All command-line arguments (e.g., `--bypass-permissions`)
- Default model (e.g., `sonnet`)

**Agent-controlled (optional override at spawn time):**
- Model selection via `--model <model>` flag (defaults to user's config.yaml setting, agent can override for task-specific needs)
- The prompt content (can include persona instructions, task details, etc.)

**Example:**
```bash
# Uses YOUR configured default model (e.g., sonnet from config.yaml)
sm spawn "As engineer, implement API endpoint per docs/spec.md"

# Agent overrides to opus for complex task requiring deeper reasoning
sm spawn "As architect, design distributed consensus algorithm" --model opus

# Agent overrides to haiku for simple, quick task
sm spawn "List all TypeScript files in src/" --model haiku
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

### Lifecycle Behavior

**Auto-detection (always enabled):**
- Session manager monitors child's tmux transcript
- Detects completion via idle timeout (specified in `--wait N`)
- Extracts completion summary from last transcript messages
- Notifies parent by sending message to parent's Claude input
- Archives transcript (optional, configurable)
- Keeps tmux session alive for debugging (configurable)

**Example Flow:**
1. Parent spawns: `sm spawn "Task" --wait 600`
2. Child works for 5 minutes, then goes idle
3. Session manager waits 600 seconds (10 min) of idle
4. Extracts last messages from transcript: "Feature X done. All tests passing."
5. Sends to parent's Claude input: "Child child-abc123 completed: Feature X done. All tests passing."
6. Parent wakes up and continues with completion info in context

### Termination Handling

**Terminating Child Sessions:**
```bash
# Terminate child session (kills tmux session)
sm kill def456
```

This:
- Terminates the tmux session
- Ends the shell/Claude Code process
- Equivalent to user typing `/exit`
- Only works on your own children (parent-child check)

**Cleanup Options:**
```yaml
cleanup:
  auto_kill_on_complete: false  # Keep tmux session after completion
  auto_archive_transcript: true # Save transcript on termination
  close_tmux_session: false     # Don't destroy tmux on auto-complete
```

**Parent Responsibilities:**
- Check child status before terminating
- Archive important information
- Update own state based on child results

## Progress Monitoring

### Data Sources (All Auto-Detected)

**1. Transcript Analysis**
- Parse `.jsonl` transcript for tool uses
- Count tokens from Claude responses
- Extract completion signals from last messages
- Detect error patterns

**2. Session Status**
- Current state: running, idle, waiting_input
- Last activity timestamp (time since last tool call)
- Error messages from transcript

**3. tmux Output**
- Recent output (last 50 lines)
- Current activity indicators
- Detect if session crashed or exited

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

### Resume vs Re-spawn Pattern

**Children stay alive after completing work** - they don't exit, they go idle waiting for more input (normal Claude Code behavior).

**Lifecycle:**
1. `sm spawn "..." --name engineer-1042` → child starts, works on initial prompt
2. Child finishes task → goes idle, waits for input (doesn't exit)
3. `sm send engineer-1042 "More work..."` → child wakes, resumes with full context
4. Repeat as needed → child maintains context across multiple tasks
5. `sm kill engineer-1042` → terminate when done (equivalent to `/exit`)

**Key Points:**

- **Resume with `sm send`** - No special command needed, just send more work
- **Re-spawn only for fresh context** - Use `sm spawn` again if you want a new session without previous context
- **Context preservation** - Resumed sessions keep full conversation history
- **Parent controls lifecycle** - Parent decides when to resume (send), re-spawn (spawn), or terminate (kill)

**Example: Multi-task workflow**
```bash
# Spawn engineer for first task
$ sm spawn "As engineer, implement feature X" --wait 600 --name eng-1042
Spawned eng-1042 (abc123)
# ... wait for completion notification ...

# Resume same engineer for follow-up work (keeps context)
$ sm send eng-1042 "Now implement feature Y using same patterns"
Input sent to eng-1042 (abc123)
# ... engineer continues with full context from feature X ...

# Resume again for third task
$ sm send eng-1042 "Fix the bug in feature X you just implemented"
# ... engineer has context of both X and Y ...

# Done with this engineer
$ sm kill eng-1042
```

**When to resume vs re-spawn:**

| Scenario | Action | Reason |
|----------|--------|--------|
| Related follow-up work | Resume (`sm send`) | Preserve context, faster |
| Unrelated new task | Re-spawn (`sm spawn`) | Fresh context, avoid confusion |
| Bug fix in prior work | Resume (`sm send`) | Needs context of original implementation |
| Context too large | Re-spawn (`sm spawn`) | Start fresh to avoid token limits |
| Different persona | Re-spawn (`sm spawn`) | New role requires different initial prompt |

This matches natural Claude Code session behavior - sessions stay alive until explicitly exited.

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

**No commands needed** - session manager auto-detects completion by monitoring transcript and sends notification to parent's Claude input.

**If child needs to ask parent a question:**
- Child uses Claude's AskUserQuestion tool (blocks child, notifies user)
- User can relay answer via `sm send <child-id> "answer"`
- Or user answers directly if monitoring child session

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
- [ ] Parent-child relationship tracking (store parent_session_id)
- [ ] `sm children` command
- [ ] `sm kill` command with parent-child check (CRITICAL: only allow killing own children)
- [ ] Configurable Claude command/args

**Deliverables:**
- Can spawn child sessions
- Children appear in `sm all`
- Manual lifecycle management via `sm kill`
- Parent-child tracking with safety constraints
- Prevents agents from killing unrelated sessions

### Phase 2: Auto-Detection & Monitoring
- [ ] Transcript parsing for tool usage tracking
- [ ] Token estimation from transcript length
- [ ] Idle detection (time since last tool call)
- [ ] Completion detection from transcript patterns
- [ ] Parent notification via input injection
- [ ] Enhanced `sm what` with progress details

**Deliverables:**
- Automatic completion detection and notification
- Progress visibility via transcript parsing
- Zero child-side commands needed

### Phase 3: Polish & Integration
- [ ] Transcript archiving on completion
- [ ] Telegram notifications for child events
- [ ] Better error handling and recovery
- [ ] Session crash detection
- [ ] Resource cleanup options

**Deliverables:**
- Production-ready reliability
- Full Telegram integration
- Clean session lifecycle

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

# EM spawns engineer and waits (typical pattern)
$ sm spawn "As engineer, implement ticket #1042 per spec" --wait 600 --name eng-1042
Spawned child-abc123 (abc123)
[Waiting... parent not burning tokens...]

# 5 minutes later, engineer completes
[Session manager sends to EM's input]:
"Child child-abc123 (eng-1042) completed: PivotExtendedEvent implemented. All tests passing."

# EM continues with next ticket
$ sm spawn "As engineer, implement ticket #1043 per spec" --wait 600 --name eng-1043
Spawned child-def456 (def456)
[Waiting...]

[Session manager sends to EM's input]:
"Child child-def456 (eng-1043) completed: SignalResolver extracted. PR created."

# EM spawns architect to review
$ sm spawn "As architect, review PRs for tickets #1042 and #1043" --wait 600 --name review
Spawned child-ghi789 (ghi789)
[Waiting...]

[Session manager sends to EM's input]:
"Child child-ghi789 (review) completed: Both PRs approved and merged."

# Check all completed work
$ sm children
child-abc123 (eng-1042) | completed | 15min ago | "PivotExtendedEvent implemented. All tests passing."
child-def456 (eng-1043) | completed | 10min ago | "SignalResolver extracted. PR created."
child-ghi789 (review) | completed | 2min ago | "Both PRs approved and merged."
```

### References

- Session Manager Architecture: `README.md`
- API Endpoints: `src/server.py`
- CLI Commands: `src/cli/commands.py`
- Configuration: `config.yaml.example`
