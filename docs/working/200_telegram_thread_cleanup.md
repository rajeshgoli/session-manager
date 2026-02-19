# sm#200 — Telegram thread cleanup for cleared/dead sessions

## Problem

Telegram notification threads accumulate for sessions that have been cleared
(`sm clear`) or killed (`sm kill`). The user sees growing lists of silent or
stale-looking threads in Telegram with no indication of the session's fate.

## Current Behavior

### sm kill path (`DELETE /sessions/{id}`)

`server.py:1130` calls `output_monitor.cleanup_session(session)`.
`cleanup_session` (`output_monitor.py:482`):

1. Sets session status to STOPPED.
2. If `session.telegram_thread_id` and `session.telegram_chat_id` are set,
   calls `telegram_bot.bot.delete_forum_topic(chat_id, message_thread_id)`.
   - In forum-group sessions, this deletes the topic silently (no "session
     stopped" message is sent first).
   - In reply-thread sessions (non-forum), `telegram_thread_id` holds a regular
     message ID (not a forum topic ID), so `delete_forum_topic` fails with a
     Telegram API error. The error is caught and logged as a warning — the
     thread silently persists.
3. Cleans up `_topic_sessions` and `_session_threads` in memory.

### sm clear path (`POST /sessions/{id}/clear`)

`server.py:1073–1086` calls `session_manager.clear_session()`,
`_invalidate_session_cache()`, and `queue_mgr.cancel_remind()`.

**No Telegram interaction.** The thread persists unchanged; the user has no
indication the session's context was reset or that it will continue working on a
new task.

## Root Causes

1. **Kill path, forum mode**: Topic deleted silently — no goodbye message, so
   user doesn't know why the topic disappeared.
2. **Kill path, reply-thread mode**: `delete_forum_topic()` fails (reply-thread
   message IDs are not forum topics). The in-memory cleanup runs but the thread
   goes silent with no explanation.
3. **Clear path, any mode**: No Telegram notification at all.

## How `telegram_thread_id` is used for both modes

- **Forum mode** (`/new` in a forum group): `telegram_thread_id` = forum topic
  ID; stored in both `_topic_sessions` and `_session_threads`.
- **Reply-thread mode**: `telegram_thread_id` = root reply message ID; stored
  in `_session_threads` only at runtime.

**`_topic_sessions` is not a reliable forum-mode indicator.** `load_session_threads()`
(`telegram_bot.py:205-208`) inserts all sessions with `telegram_thread_id` into
`_topic_sessions` for backward compatibility — including non-forum reply-thread
sessions. After a server restart, reply-thread sessions appear in `_topic_sessions`,
making membership an unreliable signal.

### Reliable mode detection: try-and-fallback

`send_notification()` catches all send errors internally and returns `Optional[int]`
(`msg_id`). A `None` return means the send failed. This gives us a reliable
probe:

1. Attempt forum send (`message_thread_id=telegram_thread_id`).
2. If `msg_id is not None` → forum topic confirmed; proceed with `close_forum_topic`.
3. If `msg_id is None` → not a forum topic; retry as reply-thread
   (`reply_to_message_id=telegram_thread_id`).

No schema changes required.

## Fix

### 1. Kill path — send a final message before cleanup (`output_monitor.py:cleanup_session`)

Before cleaning up in-memory mappings, send a "Session stopped" message to the
thread using try-and-fallback mode detection. Replace the existing
`delete_forum_topic` + in-memory cleanup block (lines 507–527) with:

```python
# output_monitor.py, inside cleanup_session(), replacing lines 507-527

if session.telegram_thread_id and session.telegram_chat_id:
    notifier = getattr(self._session_manager, 'notifier', None) if self._session_manager else None
    telegram_bot = getattr(notifier, 'telegram', None) if notifier else None

    if telegram_bot and telegram_bot.bot:
        stopped_msg = f"Session stopped [{session_id}]"
        thread_id = session.telegram_thread_id
        chat_id = session.telegram_chat_id

        # Try forum-topic delivery first; fall back to reply-thread on failure.
        # send_notification() catches all errors internally and returns None on failure.
        msg_id = await telegram_bot.send_notification(
            chat_id=chat_id,
            message=stopped_msg,
            message_thread_id=thread_id,
        )

        if msg_id is not None:
            # Confirmed forum topic — close it (keeps history visible, marks resolved)
            try:
                await telegram_bot.bot.close_forum_topic(
                    chat_id=chat_id, message_thread_id=thread_id
                )
            except Exception as e:
                logger.warning(f"Could not close forum topic for {session_id}: {e}")
        else:
            # Not a forum topic (or send failed) — try reply-thread mode
            await telegram_bot.send_notification(
                chat_id=chat_id,
                message=stopped_msg,
                reply_to_message_id=thread_id,
            )

        # Clean up in-memory mappings
        telegram_bot._topic_sessions.pop((chat_id, thread_id), None)
        telegram_bot._session_threads.pop(session_id, None)
```

**`close_forum_topic` vs `delete_forum_topic`:** closing preserves history and
marks the topic as resolved, which better reflects a stopped session. It also
requires fewer Telegram permissions than deletion.

### 2. Clear path — send a context-reset marker (`server.py:clear_session`)

After `_invalidate_session_cache()`, send a "Context cleared" marker using the
same try-and-fallback pattern. The session thread is NOT cleaned up — it
continues to receive notifications for the new task.

```python
# server.py, inside clear_session(), after the _invalidate_session_cache() call

notifier = app.state.notifier if hasattr(app.state, 'notifier') else None
telegram_bot = getattr(notifier, 'telegram', None) if notifier else None
if telegram_bot and session.telegram_chat_id and session.telegram_thread_id:
    cleared_msg = f"Context cleared [{session_id}] — ready for new task"
    chat_id = session.telegram_chat_id
    thread_id = session.telegram_thread_id
    # Try forum-topic delivery; fall back to reply-thread if it fails.
    msg_id = await telegram_bot.send_notification(
        chat_id=chat_id,
        message=cleared_msg,
        message_thread_id=thread_id,
    )
    if msg_id is None:
        await telegram_bot.send_notification(
            chat_id=chat_id,
            message=cleared_msg,
            reply_to_message_id=thread_id,
        )
```

## Files

| File | Change |
|------|--------|
| `src/output_monitor.py` | Replace delete block (lines 507–527): send "stopped" message, close forum topic, clean up in-memory mappings |
| `src/server.py` | Add "context cleared" notification after `_invalidate_session_cache()` in `clear_session` |

No schema changes. No new dependencies.

## Test Plan

1. **Kill, forum mode**: Kill a session with a forum topic. Verify the topic
   receives "Session stopped [id]" before being closed (not deleted — history
   remains visible).
2. **Kill, reply-thread mode**: Kill a session in non-forum mode. Verify the
   thread receives "Session stopped [id]". Verify `delete_forum_topic` is no
   longer called (no API error logged).
3. **Clear, forum mode**: Clear a session with a forum topic. Verify the topic
   receives "Context cleared [id] — ready for new task". Verify the topic
   remains open.
4. **Clear, reply-thread mode**: Clear a non-forum session. Verify the reply
   thread receives "Context cleared [id] — ready for new task".
5. **Post-restart reply-thread session**: Simulate a server restart by calling
   `load_session_threads()` with a reply-thread session (non-forum). Kill the
   session. Verify the "Session stopped" message is sent via `reply_to_message_id`
   (not `message_thread_id`) and that `close_forum_topic` is NOT called.
6. **No Telegram configured**: Kill or clear a session with no
   `telegram_chat_id`. Verify no errors and normal cleanup proceeds.
7. **Notification failure**: Simulate both forum and fallback sends failing.
   Verify the failure is logged as warning but cleanup (in-memory mapping
   removal) continues.

## Classification

Single ticket. Changes are localized to two files with no schema or protocol
changes.
