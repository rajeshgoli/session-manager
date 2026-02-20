# sm#271 — Telegram Thread Clutter: Investigation & Fix Spec

## Problem

The Telegram forum is accumulating too many open threads, making it impossible
for the user to locate the active EM thread. EM→user paging is degraded because
the user has to hunt for the right topic every time.

## Investigation Findings

### 1. Thread Creation: One Per Session, Always

Every call to `_create_session_common()` (`session_manager.py:270`) triggers
`_ensure_telegram_topic()` (`session_manager.py:410`), which creates a Telegram
forum topic for the new session.

This includes:
- EM sessions (`sm new`, `/new` from Telegram)
- Child agent sessions (`sm spawn`, `sm dispatch`)
- Review sessions (`sm review`, `spawn_review_session`)
- Any session spawned from the CLI

Child sessions receive `telegram_chat_id = default_forum_chat_id` even when
spawned programmatically (not via Telegram). Result: **every single agent
session — engineer, scout, architect, reviewer — gets its own Telegram topic.**

### 2. Thread Closure: Only Three Paths Trigger Cleanup

`output_monitor.cleanup_session()` (lines 482–561) is the only function that
closes a forum topic. It runs on exactly three triggers:

| Trigger | Path | Closes topic? |
|---------|------|---------------|
| `sm kill <id>` | `server.py:1162` → `cleanup_session` | ✅ Yes |
| Explicit API kill | `server.py:2122` → `cleanup_session` | ✅ Yes |
| Tmux session dies (detected by monitor) | `output_monitor._handle_session_died` → `cleanup_session` | ✅ Yes (~30s lag) |
| `sm clear` | `server.py:1080` | ❌ No (sends "Context cleared" message only) |
| Claude Code process exits (Stop hook fires) | None | ❌ No |
| ChildMonitor detects completion | `child_monitor._notify_parent_completion` | ❌ No |

**Critical gap**: When Claude Code finishes work and exits naturally, the
_tmux session stays alive_ at a bash prompt. The output_monitor polls for tmux
session existence (every ~30 polls = ~30 seconds), but since tmux is alive, it
never calls `cleanup_session`. The topic stays open indefinitely.

### 3. Observed Stale State (2026-02-19)

Current sessions.json contains 13 sessions, all with open Telegram topics:

```
thread=12319  engineer-1614          idle  tmux=alive  created=2026-02-18
thread=12326  (unnamed)              idle  tmux=alive  created=2026-02-18
thread=13175  scout-1685-enddate     idle  tmux=alive  created=2026-02-19
thread=13289  spec-reviewer          idle  tmux=alive  created=2026-02-19
thread=13447  em-fractal             idle  tmux=alive  created=2026-02-19
thread=13451  engineer-1702          idle  tmux=alive  created=2026-02-19
thread=13480  architect-pr1706       idle  tmux=alive  created=2026-02-19
thread=13824  em                     idle  tmux=alive  created=2026-02-19
thread=13827  engineer-256           running
thread=13845  scout-269              running
thread=14205  scout-warmup-bugs      running
thread=14434  reviewer-240           running
thread=15325  scout-271              running (this session)
```

Of the 8 idle sessions, 7 are almost certainly done with their tasks — Claude
Code has exited but tmux stays alive. Their topics are open and silent, adding
noise to the forum sidebar.

**Thread accumulation rate**: Thread IDs advanced from 12319 to 15325 in ~2
days. Past sessions (already killed/cleaned up) contributed to this range;
visible accumulation of 13 open topics in a 2-day window confirms the rate.

### 4. The EM Thread Continuity Problem

The current state has two EM sessions with open topics: `em` (13824) and
`em-fractal` (13447). Each `sm em` call on a new session creates a new topic.
When the user runs `sm handoff`, the old EM session's Claude Code exits (tmux
stays alive), and a new EM session is created with a new topic.

`is_em=True` is never set on any current session (all show `is_em=False` in
sessions.json), even on sessions named "em". `sm em` sets `is_em=True` via
`PATCH /sessions/{id}` (`server.py:929`), but the current sessions were either
created before sm#256 or the `sm em` preflight was not run.

Result: the user cannot reliably locate the EM thread because a new one appears
after every `sm handoff`.

### 5. Telegram API Limitation

Telegram Bot API has **no `getForumTopics` endpoint**. We cannot enumerate open
topics programmatically. We can only manage topics by known ID (from
sessions.json). Topics from sessions that were already removed from sessions.json
(e.g., older sessions cleaned up months ago whose topics were never closed)
cannot be discovered or closed automatically.

### 6. What sm#200 Fixed

sm#200 (merged in PR #268) added `close_forum_topic` to the kill and natural-
death paths. This was the correct fix for those paths. The gap it did not
address: Claude Code process exit without tmux death.

---

## Root Cause

Two distinct causes:

**A. Completed sessions keep topics open** — When Claude Code finishes and
exits naturally, the tmux session stays alive. `cleanup_session` only fires on
tmux death or explicit kill, so no topic closure happens. Idle/completed
sessions accumulate open topics until someone runs `sm kill`.

**B. EM has no thread continuity** — Each EM session (created by `sm em` on
a new session) gets a new Telegram topic. The user must hunt for the new EM
thread after every `sm handoff`.

---

## Proposed Solution

### Fix A: Close child session topics on task completion

**Where**: `src/child_monitor.py`, `_notify_parent_completion()` (line 212)

After the ChildMonitor sends the completion notification to the parent and sets
`completion_status = COMPLETED`, trigger `cleanup_session()` on the child
session. This sends "Session completed [id]: <message>" to the topic and closes
it, then removes the session from the sessions dict.

**Implementation**:

1. Add `output_monitor` reference to `ChildMonitor.__init__()` (already has
   `session_manager`; add `output_monitor` via a setter or constructor param).
2. In `_notify_parent_completion()`, after `child_session.completion_status =
   CompletionStatus.COMPLETED`, call:
   ```python
   if self.output_monitor:
       await self.output_monitor.cleanup_session(child_session)
   ```
3. Modify the "stopped" message in `cleanup_session` to use "Session completed"
   instead of "Session stopped" when `completion_status == COMPLETED`.

**Scope**: Only sessions registered with ChildMonitor (spawned with `--wait`).
Sessions not registered with ChildMonitor (no parent, or parent didn't use
`--wait`) are out of scope for this fix and rely on explicit `sm kill`.

### Fix B: EM thread continuity across sessions

**Where**: `src/server.py`, `PATCH /sessions/{id}` handler (line 902)

When `is_em=True` is set on a session, the server should:

1. Find any OTHER session (or recently-stopped session in state) with
   `is_em=True` and matching `telegram_chat_id`.
2. If found, reuse its `telegram_thread_id`:
   - Set `new_session.telegram_thread_id = prev_em.telegram_thread_id`
   - Set `new_session.telegram_chat_id = prev_em.telegram_chat_id`
   - Call `reopenForumTopic(chat_id, thread_id)` (Telegram allows reopening
     closed topics).
   - Post "EM session [new_id] continuing" to the thread.
   - Skip calling `_ensure_telegram_topic()` for the new EM session (it now
     has a thread_id already; `_ensure_telegram_topic` skips creation when
     `thread_id` is already set).
3. If no previous EM topic found, let `_ensure_telegram_topic()` create a new
   one (existing behavior).
4. Clear `is_em` from the previous EM session (transfer ownership).

**Prerequisite**: Sessions need to remain in sessions.json briefly after
their tmux dies or they're killed, OR the previous EM session's
`telegram_thread_id` needs to be preserved in a config/metadata store. The
simplest approach: query `session_manager.sessions` for any live session with
`is_em=True` first (handles EM handoff while old session is still alive), then
check recently-cleaned-up sessions. Since `cleanup_session` removes sessions
from the dict, we need to persist the last EM topic ID separately.

**Recommended implementation**:
Store `last_em_thread_id` and `last_em_chat_id` in a persistent config or
sessions.json header-level field (not per-session). When a new `is_em=True`
session is created, read these fields and inherit if present.

OR (simpler): Add a `SessionManager.em_topic: Optional[tuple[int, int]]`
field (persisted to sessions.json at the top level) that is updated whenever
an EM session is created or destroyed. The new EM session reads this at
`sm em` time.

### Fix C: Backlog cleanup for existing stale sessions

For the current 8 idle sessions with open topics (and any future accumulation),
add a `POST /admin/cleanup-idle-topics` endpoint (or a `sm clean` CLI command)
that:

1. Iterates all sessions where `status == idle` and `last_activity` is older
   than a threshold (e.g., 2 hours).
2. For each, calls `cleanup_session()`.
3. Returns count closed.

This is a one-time maintenance tool, not a permanent behavioral change.

**Alternatively**: On server startup, `_reconcile_telegram_topics()` could be
extended to close topics for sessions that are idle AND whose last activity was
more than N hours ago. But this risks being too aggressive (e.g., closing an EM
session that's been idle waiting for user input). Gate on `completion_status ==
COMPLETED` OR (`parent_session_id is not None` AND idle > 2 hours).

---

## Files to Modify

| File | Change |
|------|--------|
| `src/child_monitor.py` | Add `output_monitor` ref; call `cleanup_session` on completion |
| `src/server.py` | `PATCH /sessions/{id}`: EM thread inheritance when `is_em=True` set |
| `src/session_manager.py` | Persist `last_em_topic` (chat_id, thread_id) at top level |
| `src/output_monitor.py` | Use "Session completed" message when `completion_status == COMPLETED` |

Fix C (backlog cleanup) optionally adds:
| `src/server.py` | `POST /admin/cleanup-idle-topics` endpoint |
| `src/cli/commands.py` | `sm clean` subcommand wrapper |

---

## Test Plan

**Fix A: Child completion closes topic**
1. Spawn a child agent with `--wait 30`. Observe the agent complete its task.
2. Verify: ChildMonitor fires, parent receives completion notification.
3. Verify: Telegram topic for child session receives "Session completed [id]"
   and is marked closed in the forum.
4. Verify: Session removed from sessions.json.

**Fix B: EM thread continuity**
1. Create a session and run `sm em`. Note the Telegram topic thread_id.
2. Run `sm handoff` — this exits the EM's Claude Code process.
3. Create a new session and run `sm em` on it.
4. Verify: The new EM session uses the SAME Telegram topic (no new topic
   created). The topic is reopened if it was closed.
5. Verify: "EM session [new_id] continuing" posted to the thread.
6. Verify: The old EM session's `is_em` is cleared.

**Fix C: Backlog cleanup**
1. Populate state with several idle sessions (with completion_status=COMPLETED
   or idle >2h).
2. Call `POST /admin/cleanup-idle-topics`.
3. Verify: Telegram topics for those sessions are closed.
4. Verify: Active/running sessions are NOT affected.

**Regression: existing kill/clear/natural-death paths**
5. Kill a session — verify topic closed (sm#200 regression check).
6. Clear a session — verify "Context cleared" message sent, topic stays open.
7. Simulate tmux death — verify topic closed after ~30s.

---

## Classification

**Single ticket**. Fixes A, B, and C are independent sub-fixes but all touch
Telegram thread lifecycle and can be delivered by one engineer in a single PR
without context overflow. Fix C is optional (low priority maintenance feature)
and can be deferred.
