# #135 — Auto-sync Telegram Forum Topics with Session Lifecycle

## Problem

Telegram forum topics drift out of sync with session manager state:

1. **Orphaned topics** — If the server is down when a tmux session dies, `_cleanup_session` never runs and the topic is never deleted.
2. **Missing topics** — Sessions created via `sm new`/`sm spawn` don't get Telegram topics (only `/new` from Telegram does).

## Design

### Startup Reconciliation

On server startup, after `_load_state()` restores sessions and `load_session_threads()` rebuilds in-memory maps:

1. **Collect dead session topics**: During `_load_state()`, when a session is dropped because its tmux session no longer exists, record its `(telegram_chat_id, telegram_thread_id)` pair in a cleanup list.
2. **Delete orphaned topics**: After the Telegram bot starts, iterate the cleanup list and call `delete_forum_topic()` for each.
3. **Backfill missing chat IDs**: For each live session where `telegram_chat_id` is `None`, set it to `default_forum_chat_id`. This covers pre-existing CLI-created sessions that were never associated with a chat.
4. **Create missing topics**: For each live session that has a `telegram_chat_id` but no `telegram_thread_id`, auto-create a forum topic using the session's friendly name.

```
_load_state()
  ├─ session tmux alive?  → restore
  └─ session tmux dead?   → drop session, add (chat_id, thread_id) to orphan list

startup()
  ├─ load_session_threads()
  ├─ delete orphaned topics from list
  ├─ backfill chat_id = default_forum_chat_id where missing
  └─ for each session with chat_id but no thread_id → create_forum_topic()
```

### Lifecycle Hooks

#### Session Created

In `SessionManager.create_session()`, after the session is persisted:

- Always set `session.telegram_chat_id` to the configured `default_forum_chat_id` (or the creating chat's ID if initiated from Telegram). This ensures notifications work even if topic creation fails.
- Call `create_forum_topic(chat_id, friendly_name or session.name)`. On success, store the returned `topic_id` on `session.telegram_thread_id`. On failure, leave `thread_id` unset — notifications will still be sent to the chat, just without threading.
- Register the topic mapping in the bot's in-memory dicts.

`create_session()` is the **single owner** of topic creation. Callers (including the Telegram `/new` handler in `telegram_bot.py:1322-1343`) must not create their own topics. The `/new` handler should check `session.telegram_thread_id` after `on_new_session` returns and skip topic creation if it is already set.

#### Session Killed / Cleaned Up

Already handled by `_cleanup_session()` in `output_monitor.py` — no changes needed for the active-server path.

### Default Chat ID

A new config field determines which chat to create topics in by default:

```yaml
telegram:
  default_forum_chat_id: -1003506774897  # Forum group for auto-created topics
```

Sessions created from Telegram use the chat they were created in. Sessions created from CLI (`sm new`, `sm spawn`) use `default_forum_chat_id`.

## Key Files to Modify

| File | Change |
|------|--------|
| `src/session_manager.py` | `_load_state()` — collect orphaned topic list; `create_session()` — auto-create topic and always set `chat_id` |
| `src/main.py` | Startup — delete orphaned topics, backfill missing `chat_id`, create missing topics after bot starts |
| `src/telegram_bot.py` | `/new` handler — skip topic creation when `session.telegram_thread_id` already set; possibly `delete_orphaned_topics(list)` batch helper |
| `config.yaml` | Add `telegram.default_forum_chat_id` |

## Edge Cases

- **Bot lacks permissions**: `create_forum_topic` / `delete_forum_topic` can fail if the bot isn't an admin. Log warning, don't crash.
- **Forum disabled on group**: If the group isn't a forum, topic creation fails. `chat_id` is still persisted so notifications fall back to unthreaded messages.
- **Race on startup**: Topics should be deleted/created after the Telegram bot is fully started and polling.
- **Multiple groups**: If `allowed_chat_ids` has multiple forum groups, only use `default_forum_chat_id` for auto-creation. Manual `/new` from other groups still works.
