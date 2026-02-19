# sm#184: Telegram Notifications Delayed by One Message

**Status:** Investigation complete — spec ready for review
**Issue:** [#184](https://github.com/rajeshgoli/session-manager/issues/184)
**Role:** Scout (root cause analysis, no code changes)

## Problem Statement

Telegram notifications are delayed by one message. When Agent A sends a message to Agent B via `sm send`, the Telegram notification for B's response arrives late — often containing content from the *previous* response rather than the current one.

**Expected:** After `sm send` delivers and Claude responds, a Telegram notification arrives promptly with the current response content.
**Observed:** Notification content lags by one message, or arrives only after the *next* interaction triggers a new Stop hook.

## Architecture: Notification Flow

When `sm send <id> "msg"` is invoked, **up to four Telegram notification paths** can fire at different times:

| # | Path | When it fires | Condition | Content |
|---|------|--------------|-----------|---------|
| 1 | `_notify_sm_send()` | Immediately when API receives the request | Always | "📨 From [sender]: {text}" |
| 2 | `_mirror_to_telegram()` | At actual tmux delivery time | Only for sequential/batch delivery (`_try_deliver_messages`); does NOT fire for urgent delivery (`_deliver_urgent`) | Mirror of delivered message |
| 3 | "response" notification | Stop hook handler in server.py | Requires transcript read to return content | Claude's response text (read from transcript) |
| 4 | `_send_stop_notification()` | `mark_session_idle()` | Always fires when `notify_on_stop=True`; sends generic message if `last_output` is None | "🛑 {name} stopped: {output}" or generic completion notice |

Path 3 depends on the transcript read returning content. Path 4 fires regardless — when `last_output` is None, it sends a generic "{name} completed (Stop hook fired)" message (`message_queue.py:968`).

## Root Cause Analysis

### Primary: Transcript Race Condition (High Confidence)

**Location:** `notify_server.sh` → `server.py:/hooks/claude` → `read_transcript()`

The Stop hook shell script (`~/.claude/hooks/notify_server.sh`) runs curl in a **background subshell**:

```bash
(...curl POST...) </dev/null >/dev/null 2>&1 &
disown
exit 0
```

Claude Code does not wait for the POST to reach the server. The exact timing of when Claude Code writes the current response to the transcript JSONL file relative to the hook lifecycle is **not deterministically established** — transcript entries have been observed with timestamps both before and after Stop hook events. Regardless of the exact ordering, the async curl introduces a non-deterministic delay:

1. Claude finishes generating a response
2. Stop hook fires → `notify_server.sh` starts
3. Script reads stdin (hook payload), spawns background curl, exits immediately
4. Background curl POST arrives at the server (non-deterministic timing)
5. Server calls `read_transcript()` which reads the JSONL file in reverse looking for the last `type: "assistant"` entry

**The race:** If the transcript has not yet been flushed with the current response at step 5, `read_transcript()` finds the **previous** assistant entry, not the current one. The existing staleness guard (`server.py:1333-1337`) only catches the case where the newest assistant entry exists but has empty text — it does **not** detect the case where the newest entry hasn't been appended yet.

**Evidence:**
- Server logs show duplicate "Stored Claude output" entries ~60 seconds apart for the same session — the Stop hook and the subsequent `idle_prompt` Notification hook both read the transcript, and the second read picks up more content.
- The existing staleness check (`server.py` lines 1333-1337) only catches entries with empty `text` — it does **not** detect the case where the newest entry hasn't been written yet.
- Zero "deferring notification" or "empty transcript" log entries across all server logs, suggesting the transcript file *exists* and contains *some* assistant entries — just not the latest one.

**Why "off by one":** The transcript always has the previous response. So the notification reliably sends the *previous* response content, creating the "delayed by one message" symptom.

### Secondary: Deferred Notification Loss (Theoretical — No Production Evidence)

**Location:** `server.py` line 1461

When a Stop hook arrives with `last_message = None` (transcript not yet written), the session ID is added to `pending_stop_notifications`. The deferred notification should fire on the next `idle_prompt` Notification hook.

However, line 1461 unconditionally discards from `pending_stop_notifications`:

```python
app.state.pending_stop_notifications.discard(session_manager_id)
```

This runs whenever a **new** Stop hook arrives with content. If session S had a deferred notification pending, and a new Stop hook for S arrives (with content from a *different* response), the deferred notification is silently dropped. The user never sees the notification for the first response.

**Note:** This is a theoretical loss mode. Server logs show zero occurrences of "deferring notification", "deferred response", or "pending_stop" across all available logs. The race condition in the primary hypothesis means `read_transcript()` almost always returns *something* (just the wrong thing), so the deferred path rarely triggers. The code path exists and the bug is real, but there is no evidence of it activating in production.

### Tertiary: Notification Multiplicity (Low Confidence, Contributes to Confusion)

Four separate Telegram notifications per `sm send` interaction creates timing confusion:

- Path 1 (`_notify_sm_send`) fires immediately — user sees "message sent" confirmation
- Path 2 (`_mirror_to_telegram`) fires at delivery — only for sequential/batch mode, not urgent
- Path 3 ("response") fires at Stop hook — **contains stale content** due to primary bug
- Path 4 (`_send_stop_notification`) fires at `mark_session_idle` — sends generic message if transcript content unavailable

The user sees the stale Path 3 notification and interprets it as the "real" notification, not realizing it contains previous-response content. The correct content may arrive later via Path 4 (if `mark_session_idle` is called with the correct `last_output`), but by then the user has already read the stale one.

## `stop_hook_active` Field — Dead Code

The hook payload includes `stop_hook_active: false` in all observed payloads. The `HookPayload` Pydantic model defines this field, but the `/hooks/claude` handler takes `payload: dict` (`server.py:1282`), not `HookPayload` — so neither the model nor the field is used by the handler. Both are dead code, worth noting for cleanup.

## Proposed Fix

### Fix 1: Synchronous Hook Execution (Experimental — Not Recommended Without Proof)

**Change:** Remove the background execution from `notify_server.sh`. Run curl synchronously:

```bash
curl -s -X POST http://localhost:8420/hooks/claude \
  -H "Content-Type: application/json" \
  -d "$PAYLOAD"
```

**Why (theoretical):** If Claude Code writes to the transcript *after* the hook exits, synchronous curl would ensure the POST reaches the server before the transcript is updated — eliminating the race. However, this sequence is **not proven**. Transcript entries have been observed with timestamps before Stop hook events, suggesting Claude may write to the transcript *before* firing hooks.

**Risk:** If Claude Code writes the transcript before firing the hook (which current evidence suggests may be the case), synchronous execution provides no benefit and adds latency. Additionally, synchronous curl blocks every Claude response until the server responds. This change is also outside the session-manager repo (`~/.claude/hooks/`).

**Recommendation:** Do not implement unless the write/flush order is deterministically established via instrumented testing.

### Fix 2: Server-Side Transcript Retry (Recommended, Complementary)

**Change:** In the Stop hook handler, after `read_transcript()` returns, apply bounded retries if the result appears stale. Since the hook payload has no event timestamp today, use one of these concrete staleness criteria:

- **(a) Entry count comparison:** Track the assistant entry count per session. If `read_transcript()` returns the same count as the previous Stop hook for this session, retry.
- **(b) Content hash comparison:** Store a hash of the last-returned assistant content per session. If the hash matches the previous read, the transcript hasn't changed — retry.
- **(c) Unconditional bounded retry (simplest):** For Stop hooks only, always retry once after 300ms if `last_message` matches the previously stored output for this session.

```python
# Option (c) — simplest implementation
stored_output = app.state.last_claude_output.get(session_manager_id)
found, last_message = await asyncio.to_thread(read_transcript, transcript_path)
if last_message and last_message == stored_output:
    # Transcript hasn't changed — likely stale. Retry once.
    await asyncio.sleep(0.3)
    found, last_message = await asyncio.to_thread(read_transcript, transcript_path)
```

**Why:** This is a robust defense against the race condition regardless of hook execution mode. Even if the hook fires before the transcript is written, the retry gives Claude Code time to flush.

**Risk:** Adds up to 300ms latency in worst case (option c). Options (a) and (b) require maintaining per-session state but provide more precise staleness detection.

### Fix 3: Reduce Notification Multiplicity (Optional, UX Improvement)

Consolidate the four notification paths. Consider:
- Removing `_mirror_to_telegram` (Path 2) since Path 1 already confirms delivery
- Making Path 3 ("response") the single authoritative response notification
- Gating Path 4 (`_send_stop_notification` to sender) behind a flag so it only fires when explicitly requested

This doesn't fix the root cause but reduces the surface area for timing confusion.

## Test Plan

1. **Reproduce the race condition:**
   - Add debug timestamps to `notify_server.sh` (log when curl fires) and `read_transcript()` (log when it reads and what entry it finds)
   - Send 5 consecutive `sm send` messages and compare notification content against actual responses
   - Verify that stale content appears in at least some notifications

2. **Validate Fix 2 (retry):**
   - Implement retry logic in Stop hook handler
   - Re-run the 5-message test
   - Verify all notifications contain current response content
   - Measure added latency (should be 0 when transcript is ready, ≤1.5s when it's not)

3. **Validate Fix 1 (synchronous hook):**
   - Remove `&` and `disown` from `notify_server.sh`
   - Verify Claude Code still functions normally (no hangs or timeouts)
   - Re-run the 5-message test
   - Measure added latency to Claude response completion

4. **Edge cases:**
   - Agent responds with very long output (transcript write takes longer)
   - Agent is sent multiple messages in rapid succession
   - Agent is idle when message arrives (no Stop hook — delivery is direct)

## Classification

**Single ticket.** One agent can implement Fix 2 (server-side retry) without context compaction. Fix 1 (synchronous hook) is a one-line change. Fix 3 (reduce multiplicity) is optional and can be a follow-up ticket.
