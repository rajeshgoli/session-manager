# Ticket #587: Restore/startup should not block on Telegram topic creation

## Observed behavior
- `sm restore` for a stopped session can take ~16s when the session has `telegram_chat_id` but no thread.
- During restore, tmux runtime is created promptly, but the request remains open while Session Manager waits for Telegram `createForumTopic` and welcome `sendMessage`.
- The event loop watchdog can then trip and kill the daemon, causing `Session manager unavailable` errors in `sm watch` and CLI callers.
- App startup also stays unavailable until missing Telegram topics are backfilled, because `_post_bind_startup()` currently waits for that work.

## Repro notes
- 2026-04-14 PT logs showed:
  - tmux restore completed at `21:56:32`
  - Telegram topic creation/welcome messaging ran afterwards
  - restore request returned at `21:56:46` (`POST /sessions/b921ae66/restore took 15.89s`)
  - watchdog then logged `Event loop did not respond within 10s` and restarted the daemon
- Startup similarly stayed unavailable until `Application startup complete` after session/topic restoration finished.

## Intended fix
- Keep tmux/runtime restore on the synchronous request path.
- Move Telegram topic ensure/backfill off the critical path for:
  - `SessionManager.restore_session()`
  - `SessionManagerApp._reconcile_telegram_topics()`
- Reuse the existing deferred topic task helper so behavior stays consistent with spawn/create flows.

## Validation
- Restore path regression test: restore with `telegram_chat_id` should call deferred topic scheduling instead of awaiting topic creation.
- Startup reconciliation regression test: missing topic backfill should schedule background ensure work instead of awaiting it.
- Live verification after merge: restore returns promptly and Session Manager remains available.
