# sm send v2: Reliable Inter-Agent Messaging

## Problem Statement

The current `sm send` implementation has two fatal flaws:

### Flaw 1: User Input Collision
When a user is typing a message and another agent uses `sm send`, the message gets injected directly into the user's input line via `tmux send-keys`:

```
User is typing: "I want to explain the prob"
Agent sends: sm send user-session "hi from architect"
Result: "I want to explain the prob[Input from: architect (08bc57cf) via sm send]
hi from architect"
```

The user's incomplete message is corrupted and sent to Claude.

### Flaw 2: No IDLE_PROMPT Detection
Current implementation uses timestamp-based `last_activity` checks with arbitrary thresholds. It doesn't know if Claude is actually at the input prompt waiting for input vs. running/generating.

## Solution Overview

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│  Sender Agent   │────▶│  Message Queue   │────▶│  Recipient      │
│  sm send ...    │     │  (per session)   │     │  (at IDLE)      │
└─────────────────┘     └──────────────────┘     └─────────────────┘
                               │                         │
                               ▼                         ▼
                        ┌──────────────┐         ┌──────────────┐
                        │  Scheduler   │         │  Stop Hook   │
                        │  (reminders) │         │  (IDLE sig)  │
                        └──────────────┘         └──────────────┘
```

## Delivery Flow

### Delivery Modes Overview

| Mode | Trigger | User Input Handling | Use Case |
|------|---------|---------------------|----------|
| **sequential** (default) | IDLE_PROMPT (Stop hook) | Save/restore | Normal coordination |
| **important** | Response complete | Save/restore | Time-sensitive, non-disruptive |
| **urgent** | Immediate | Overwrites | Emergencies only |

### Sequential Mode (Default)

#### Step 1: Queue Message
When `sm send` is called:
1. Validate target session exists
2. Add message to target session's `pending_sm_sends` queue
3. Record: sender_id, text, timestamp, options (timeout, notify settings)
4. Return immediately with "queued" status

#### Step 2: Wait for IDLE_PROMPT
The message queue processor:
1. Monitors `is_idle` state per session (set by Stop hook)
2. When Stop hook fires for a session → session becomes idle
3. Check if session has pending messages

#### Step 3: Check for User Input
Before injecting into an idle session:
1. Capture tmux pane's last line: `tmux capture-pane -p -t {session} | tail -n 1`
2. Extract text after prompt: `sed 's/^> //'`
3. If non-empty → user has pending input

#### Step 4: Handle Pending User Input
If user has typed something:
1. Record the pending text and timestamp
2. Poll every `input_poll_interval` seconds (default: 5s)
3. If text unchanged after `input_stale_timeout` (default: 2min):
   - Save text to `saved_user_input`
   - Clear the line: `tmux send-keys -t {session} C-u`
4. If text changes during polling → reset timeout (user is actively typing)

#### Step 5: Batch Deliver Messages
Once clear to deliver:
1. Concatenate ALL pending messages for this session
2. Format as single payload:
   ```
   [Input from: agent-a (abc123) via sm send]
   message 1

   [Input from: agent-b (def456) via sm send]
   message 2
   ```
3. **FINAL GATE: Re-check for user input immediately before injection**
   - Race condition exists: user may have started typing after Step 3
   - If input detected now → abort injection, go back to Step 4 (poll for stale)
4. Inject via `tmux send-keys`
5. Mark session as non-idle (`is_idle = false`)
6. Clear delivered messages from queue

> **Why the final gate?** There's a race window between detecting idle and injecting.
> User could start typing in that window. The final check right before
> `tmux send-keys` minimizes this window to near-zero.

#### Step 6: Wait for Response Complete
After injection:
1. Wait for next Stop hook (Claude finished responding)
2. Session becomes idle again

#### Step 7: Restore User Input
If we saved user input:
1. Inject saved text: `tmux send-keys -t {session} "{saved_text}"`
2. Do NOT press Enter (let user continue editing)
3. Clear `saved_user_input`

### Important Mode

Skips IDLE_PROMPT wait. Delivers as soon as Claude finishes its current response:
1. Queue message with `important` flag
2. Monitor for any Stop hook (response complete, not necessarily idle)
3. Steps 3-7 same as sequential (still handles user input safely)

Use when message is time-sensitive but shouldn't interrupt active work.

### Urgent Mode

Immediate injection, interrupts Claude:
1. Send Escape key to interrupt any streaming: `tmux send-keys -t {session} Escape`
2. Brief delay (500ms) for interrupt to process
3. Inject message directly (no user input save/restore - this is an emergency)
4. Press Enter

Use sparingly - for genuine emergencies only. Will overwrite any user input.

## Session State Model

```python
class SessionDeliveryState:
    # IDLE tracking
    is_idle: bool = False  # Set True by Stop hook, False on input injection
    last_idle_at: Optional[datetime] = None

    # Message queue
    pending_sm_sends: List[QueuedMessage] = []

    # User input handling
    saved_user_input: Optional[str] = None
    pending_user_input: Optional[str] = None  # Currently detected
    pending_input_first_seen: Optional[datetime] = None


class QueuedMessage:
    sender_session_id: str
    sender_name: str
    text: str
    queued_at: datetime

    # Delivery mode
    delivery_mode: str = "sequential"  # sequential, important, urgent

    # Options
    timeout: Optional[timedelta] = None  # Drop if not delivered by this time
    notify_on_delivery: bool = False
    notify_after: Optional[timedelta] = None  # Notify sender X time after delivery

    # State
    delivered_at: Optional[datetime] = None
```

## CLI Interface

### Delivery Modes

**Default (sequential)**: Wait for IDLE_PROMPT, then deliver safely
```bash
sm send <session> "message"
```

**Important**: Deliver when Claude finishes current response (doesn't wait for full idle)
```bash
sm send <session> "need this soon" --important
```

**Urgent**: Interrupt immediately (Escape + inject)
```bash
sm send <session> "STOP! critical issue" --urgent
```

### With timeout (drop if not delivered in time)
```bash
sm send <session> "coordinate on X" --timeout 5m
sm send <session> "urgent coord" --important --timeout 2m
```

### With delivery notification
```bash
# Notify when delivered
sm send <session> "do task X" --notify-on-delivery

# Notify 5 minutes after delivery
sm send <session> "do task X" --notify-after 5m

# Both
sm send <session> "do task X" --notify-on-delivery --notify-after 5m
```

### Self-reminder (wake without sleep)
```bash
# Wake me up in 5 minutes with this message
sm remind 5m "check on engineer progress"

# Equivalent to sm send to self with delay
sm wake 10m "time to review architect's work"
```

## Notification Messages

### Delivery notification (to sender)
```
[sm] Message delivered to {recipient_name} ({recipient_id})
Original: "{truncated_message}..."
```

### Post-delivery notification (to sender)
```
[sm] Reminder: 5m since your message to {recipient_name} was delivered
Original: "{truncated_message}..."
You can check status with: sm output {recipient_id}
```

### Self-reminder
```
[sm] Scheduled reminder:
{message}
```

## Configuration

In `config.yaml`:
```yaml
sm_send:
  # Polling interval when waiting for user input to become stale
  input_poll_interval: 5  # seconds

  # How long unchanged user input must be before we consider it stale
  input_stale_timeout: 120  # seconds (2 minutes)

  # Default timeout for messages (0 = no timeout)
  default_timeout: 0

  # Maximum messages to batch in single delivery
  max_batch_size: 10
```

### Configuration Rationale

| Setting | Default | Rationale |
|---------|---------|-----------|
| `input_poll_interval` | 5s | Balance between responsiveness and overhead. 1s = wasteful polling. 30s = slow to detect stale input. 5s catches most "user walked away" cases within one poll cycle. |
| `input_stale_timeout` | 120s | Long enough to cover "user is thinking" pauses (most people resume typing within 2 min if actively working). Short enough that coordination isn't blocked for too long. Tunable per-deployment based on user behavior. |
| `max_batch_size` | 10 | Prevents single injection from being overwhelming. At limit, excess messages stay queued for next delivery cycle. May be unnecessary - consider removing if batching all is always preferred. |

### Urgent Mode Timing

The 500ms delay after Escape in urgent mode:
- **Why needed:** Claude needs time to process interrupt and stop streaming
- **Why 500ms:** Empirically, Claude's interrupt handling completes within 200-300ms. 500ms provides margin for slow systems.
- **Tradeoff:** Shorter = risk of injecting before interrupt processed. Longer = defeats "urgent" purpose.
- **Note:** This may need tuning based on system performance. Consider making configurable if 500ms proves wrong.

## Persistence (SQLite)

Message queue is persisted for crash recovery and idempotent delivery.

### Schema

```sql
CREATE TABLE message_queue (
    id TEXT PRIMARY KEY,              -- UUID for idempotency
    target_session_id TEXT NOT NULL,
    sender_session_id TEXT,
    sender_name TEXT,
    text TEXT NOT NULL,
    delivery_mode TEXT DEFAULT 'sequential',
    queued_at TIMESTAMP NOT NULL,
    timeout_at TIMESTAMP,             -- NULL = no timeout
    notify_on_delivery INTEGER DEFAULT 0,
    notify_after_seconds INTEGER,     -- NULL = no post-delivery notification
    delivered_at TIMESTAMP            -- NULL = pending
);

CREATE INDEX idx_pending ON message_queue(target_session_id, delivered_at)
    WHERE delivered_at IS NULL;
```

### Delivery Flow with Persistence

1. **sm send** → `INSERT` with unique ID, `delivered_at = NULL`
2. **Before injection** → Verify `delivered_at IS NULL` (idempotency check)
3. **After injection** → `UPDATE delivered_at = NOW()`
4. **On restart** → `SELECT WHERE delivered_at IS NULL AND (timeout_at IS NULL OR timeout_at > NOW())`

### Crash Recovery

| Crash Point | Result | Recovery |
|-------------|--------|----------|
| Before INSERT | Message lost | Sender retries (acceptable) |
| After INSERT, before injection | Message pending | Recovered on restart |
| After injection, before UPDATE | Possible duplicate | Acceptable - better than lost |
| After UPDATE | Clean | No action needed |

### What NOT to Persist

**`saved_user_input`** - Don't persist this. If crash happens mid-delivery:
- tmux state is gone anyway (session may have died)
- User input was in volatile terminal buffer
- On recovery, we deliver pending messages fresh; user re-types if needed

## API Endpoints

### POST /sessions/{session_id}/send
Queue a message for delivery.

Request:
```json
{
  "text": "message content",
  "sender_session_id": "abc123",
  "delivery_mode": "sequential",
  "timeout_seconds": 300,
  "notify_on_delivery": true,
  "notify_after_seconds": 300
}
```

Response:
```json
{
  "status": "queued",
  "queue_position": 2,
  "delivery_mode": "sequential",
  "estimated_delivery": "waiting_for_idle"
}
```

For `urgent` mode, response is immediate:
```json
{
  "status": "delivered",
  "delivery_mode": "urgent",
  "interrupted": true
}
```

### GET /sessions/{session_id}/send-queue
Check pending messages for a session.

Response:
```json
{
  "session_id": "abc123",
  "is_idle": true,
  "pending_count": 2,
  "pending_messages": [
    {
      "id": "msg-001",
      "sender": "architect",
      "queued_at": "2024-01-15T10:00:00Z",
      "timeout_at": "2024-01-15T10:05:00Z"
    }
  ],
  "saved_user_input": null
}
```

### POST /scheduler/remind
Schedule a self-reminder.

Request:
```json
{
  "session_id": "abc123",
  "delay_seconds": 300,
  "message": "check on engineer"
}
```

## Hook Integration

See official Claude Code hooks documentation: https://docs.anthropic.com/en/docs/claude-code/hooks

### Stop Hook Handler

The **Stop hook** fires when Claude finishes responding. Important: it does NOT fire on user interrupt (Escape/Ctrl+C).

```python
def handle_stop_hook(session_id: str):
    state = get_delivery_state(session_id)
    state.is_idle = True
    state.last_idle_at = datetime.now()

    # Trigger delivery check
    asyncio.create_task(try_deliver_messages(session_id))
```

> **Note:** If user interrupts Claude mid-response, Stop hook won't fire.
> The session won't be marked idle until Claude completes a full response.
> This is correct behavior - we don't want to inject into a session where
> the user is actively interrupting/controlling Claude.

### Detecting User Input
```python
def get_pending_user_input(tmux_session: str) -> Optional[str]:
    """Check if user has typed something at the prompt."""
    output = subprocess.run(
        ["tmux", "capture-pane", "-p", "-t", tmux_session],
        capture_output=True, text=True
    ).stdout

    last_line = output.strip().split('\n')[-1]

    # Check for prompt pattern (adjust regex as needed)
    if last_line.startswith('> '):
        user_text = last_line[2:]  # Remove "> "
        if user_text.strip():  # Has non-whitespace content
            return user_text

    return None
```

### Clearing and Restoring Input
```python
def clear_user_input(tmux_session: str) -> bool:
    """Clear the current input line."""
    # Ctrl+U clears line in most shells/readline
    subprocess.run(["tmux", "send-keys", "-t", tmux_session, "C-u"])
    return True

def restore_user_input(tmux_session: str, text: str):
    """Restore previously saved user input."""
    # Send text without Enter so user can continue editing
    # Use list-based subprocess (no shell) for security - avoids metacharacter interpretation
    # The "--" signals end of options, so text starting with "-" won't be parsed as flags
    subprocess.run(["tmux", "send-keys", "-t", tmux_session, "--", text])
```

## Scheduler Implementation

For `sm remind` and post-delivery notifications:

```python
class MessageScheduler:
    def __init__(self, session_manager):
        self.session_manager = session_manager
        self.scheduled_tasks: Dict[str, List[ScheduledTask]] = {}

    async def schedule(
        self,
        target_session_id: str,
        delay: timedelta,
        message: str,
        task_type: str = "reminder"  # or "delivery_followup"
    ):
        task = ScheduledTask(
            target_session_id=target_session_id,
            fire_at=datetime.now() + delay,
            message=message,
            task_type=task_type,
        )

        # Store and schedule
        if target_session_id not in self.scheduled_tasks:
            self.scheduled_tasks[target_session_id] = []
        self.scheduled_tasks[target_session_id].append(task)

        # Create async task to fire at the right time
        asyncio.create_task(self._fire_when_ready(task))

    async def _fire_when_ready(self, task: ScheduledTask):
        delay = (task.fire_at - datetime.now()).total_seconds()
        if delay > 0:
            await asyncio.sleep(delay)

        # Queue the reminder as an sm send
        self.session_manager.queue_message(
            target_session_id=task.target_session_id,
            sender_session_id=None,  # System message
            text=f"[sm] Scheduled reminder:\n{task.message}",
            is_system=True,
        )
```

## Migration Path

### Phase 1: Core reliability
- Implement IDLE_PROMPT detection via Stop hook
- Add message queue per session
- Batch delivery on idle

### Phase 2: User input handling
- Detect pending user input
- Implement stale input detection with polling
- Save/restore user input around delivery

### Phase 3: Timeouts and notifications
- Add --timeout option
- Add --notify-on-delivery
- Add --notify-after

### Phase 4: Scheduler
- Implement `sm remind` command
- Add `sm wake` as alias
- Scheduled task persistence (survive restarts)

## Q&A: Delivery Mode Distinctions

### Q: What's the difference between Sequential and Important?

**Sequential (default):**
- Waits for **IDLE_PROMPT** - Claude is completely done working and waiting for user input
- Claude has finished all tool calls, subagents, and follow-up responses
- Safest option, message arrives when agent is truly ready for new work

**Important:**
- Waits for **response complete** - Claude finished current response but may continue working
- Claude may still be in multi-step workflow (tool calls, subagents pending)
- Message injects between responses, agent sees it sooner

### Q: When should I use Important mode?

**Example scenario:**
1. EM spawns engineer: `sm spawn "implement user login"`
2. EM realizes: "oh wait, I forgot to mention we need OAuth"
3. With `--important`: Message injects early, engineer sees it before implementing the wrong auth
4. With default: Message waits until engineer finishes entire task - too late!

**Use Important when:**
- You need to add context to an agent already working
- Course corrections mid-task
- Time-sensitive additions that shouldn't wait for full completion

**Use Sequential (default) when:**
- Normal coordination between idle agents
- Messages that can wait for agent to finish current work
- You want to ensure agent is fully ready for new input

### Q: Why does Urgent mode exist?

For genuine emergencies:
- "STOP - critical bug discovered, don't deploy"
- "Abort - requirements changed completely"
- System-level alerts

Urgent overwrites user input and interrupts Claude mid-stream. Use sparingly.

### Q: What if urgent mode interrupts Claude mid-edit?

**Known risk, accepting it.**

Worst case: Claude is mid-file-write, interrupt leaves partial file state.

**Why this is acceptable:**
- Recoverable via git (uncommitted changes can be reset)
- Urgent mode is for genuine emergencies where stopping is more important than clean state
- If you're using urgent, you've decided the interruption cost is worth it

**Our approach:** Learn from real usage. When/if this causes actual problems, we'll improve the protocol based on observed failure modes rather than speculating upfront. Premature optimization of edge cases we haven't seen yet.

## Tradeoffs and UX Considerations

### Stale Input Timeout Behavior
When user input is unchanged for `input_stale_timeout` (default 2min), we save it, deliver queued messages, then restore it after Claude responds.

**What user sees when they return:**
1. Their typed input is back at the prompt (restored)
2. Claude has responded to message(s) they didn't send
3. Potentially confusing - "I didn't ask this?"

**Mitigation options:**
- Add visual marker when restoring: `[restored] > their original text`
- Log restoration events for debugging
- Consider a config option to notify user: `[sm] Your input was preserved while delivering 2 queued messages`

**Why this tradeoff:**
- Alternative (block forever) is worse - stale input shouldn't block coordination
- Alternative (discard input) loses user work
- Save/restore preserves user work while allowing system to function

**Tuning:**
- Increase `input_stale_timeout` if users frequently step away mid-typing
- Decrease if coordination speed is more critical

## Edge Cases

### Race condition: User types between idle detection and injection
Mitigated by final gate check immediately before `tmux send-keys`. If user input detected at final gate:
- Abort injection
- Return to polling loop (Step 4)
- Wait for input to become stale or be submitted

Window is minimized but not eliminated (microseconds between check and send-keys). Acceptable tradeoff - complete elimination would require kernel-level locking.

### Multiple rapid sends from same sender
Messages are batched, so rapid sends become one delivery.

### Recipient session crashes during delivery
Message marked as delivered (we can't know if Claude processed it). Sender can use `sm output` to check.

### Sender goes offline before notification
Notifications are queued like regular messages. If sender session is gone, notification is dropped.

### Very long user input
Save/restore works for any length. tmux send-keys handles long strings via shell quoting.

### User input contains special characters
Proper shell escaping via `shlex.quote()` handles this.

### Timeout expires while user is typing
Message is dropped. Sender can check queue status if needed.

## Testing

### Manual test: Basic queue and delivery
```bash
# Terminal 1: Start recipient session
sm new

# Terminal 2: Send while recipient is busy
sm send <recipient-id> "test message"
# Should see "queued" response

# Terminal 1: Let Claude finish responding
# Message should auto-deliver when idle
```

### Manual test: User input preservation
```bash
# Terminal 1: Start typing (don't hit Enter)
> I am typing something

# Terminal 2: Send message
sm send <recipient-id> "interrupting message"

# Wait 2 minutes (stale timeout)
# Input should be cleared, message delivered
# After Claude responds, "I am typing something" should reappear
```

### Manual test: Timeout
```bash
# Keep recipient session busy continuously
sm send <recipient-id> "urgent" --timeout 30s

# After 30s, message should be dropped (check logs)
```

### Manual test: Reminder
```bash
sm remind 1m "test reminder"
# Go idle
# After 1 minute, should receive reminder injection
```
