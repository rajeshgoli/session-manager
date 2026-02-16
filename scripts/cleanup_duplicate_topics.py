#!/usr/bin/env python3
"""
Delete duplicate Telegram forum topics created during the crash-loop (issue #147).

Extracts thread IDs from the session manager log, keeps the one currently
persisted in sessions.json, and deletes all others via the Telegram Bot API.

Usage:
    # Dry run (default) — shows what would be deleted
    ./venv/bin/python scripts/cleanup_duplicate_topics.py

    # Actually delete
    ./venv/bin/python scripts/cleanup_duplicate_topics.py --execute

    # Target a specific session (defaults to c1d607d3)
    ./venv/bin/python scripts/cleanup_duplicate_topics.py --session d1614fc0

    # Use a different log file
    ./venv/bin/python scripts/cleanup_duplicate_topics.py --log /path/to/log
"""

import argparse
import asyncio
import json
import logging
import re
import sys
import time
from pathlib import Path

import yaml

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s - %(levelname)s - %(message)s",
)
logger = logging.getLogger(__name__)

# Telegram rate limit: ~30 requests/second, but be conservative
RATE_LIMIT_DELAY = 0.1  # 100ms between deletions


def load_config() -> dict:
    config_path = Path(__file__).parent.parent / "config.yaml"
    if not config_path.exists():
        logger.error(f"Config not found: {config_path}")
        sys.exit(1)
    with open(config_path) as f:
        return yaml.safe_load(f) or {}


def extract_thread_ids(log_path: str, session_id: str) -> list[tuple[int, int]]:
    """Extract (chat_id, thread_id) pairs from log for a given session.

    Returns:
        List of (chat_id, thread_id) tuples, ordered by appearance in log.
    """
    pattern = re.compile(
        rf"Auto-created topic for session {re.escape(session_id)}: "
        rf"chat=(-?\d+), thread=(\d+)"
    )
    results = []
    with open(log_path) as f:
        for line in f:
            m = pattern.search(line)
            if m:
                chat_id = int(m.group(1))
                thread_id = int(m.group(2))
                results.append((chat_id, thread_id))
    return results


def get_current_topic(session_id: str, state_file: str) -> tuple[int, int] | None:
    """Read the currently-persisted (chat_id, thread_id) for a session."""
    path = Path(state_file)
    if not path.exists():
        return None
    with open(path) as f:
        data = json.load(f)
    for s in data.get("sessions", []):
        if s.get("id") == session_id:
            chat_id = s.get("telegram_chat_id")
            thread_id = s.get("telegram_thread_id")
            if chat_id and thread_id:
                return (chat_id, thread_id)
            return None
    return None


async def delete_topics(
    token: str,
    to_delete: list[tuple[int, int]],
    execute: bool,
) -> tuple[int, int]:
    """Delete forum topics via the Telegram Bot API.

    Returns:
        (deleted_count, failed_count)
    """
    if not execute:
        for i, (chat_id, thread_id) in enumerate(to_delete, 1):
            logger.info(f"[DRY RUN] Would delete topic {thread_id} in chat {chat_id} ({i}/{len(to_delete)})")
        return len(to_delete), 0

    from telegram import Bot

    bot = Bot(token=token)
    deleted = 0
    failed = 0

    for i, (chat_id, thread_id) in enumerate(to_delete, 1):
        try:
            await bot.delete_forum_topic(chat_id=chat_id, message_thread_id=thread_id)
            deleted += 1
            if i % 50 == 0 or i == len(to_delete):
                logger.info(f"Progress: {i}/{len(to_delete)} ({deleted} deleted, {failed} failed)")
        except Exception as e:
            error_str = str(e)
            # Only treat topic-specific "not found" as idempotent success;
            # broader matches like "chat not found" indicate real failures.
            if "TOPIC_NOT_MODIFIED" in error_str or "TOPIC_ID_INVALID" in error_str:
                deleted += 1
            else:
                failed += 1
                logger.warning(f"Failed to delete topic {thread_id} in chat {chat_id}: {e}")

        # Rate limiting
        await asyncio.sleep(RATE_LIMIT_DELAY)

    # Shut down the HTTP client. Do NOT call bot.close() — that invokes the
    # Telegram close API (bot migration control) which can disrupt the
    # active bot instance and trigger 429 errors.
    await bot.shutdown()
    return deleted, failed


async def main():
    parser = argparse.ArgumentParser(
        description="Delete duplicate Telegram forum topics from crash-loop (issue #147)"
    )
    parser.add_argument(
        "--execute",
        action="store_true",
        help="Actually delete topics (default is dry run)",
    )
    parser.add_argument(
        "--session",
        default="c1d607d3",
        help="Session ID to clean up (default: c1d607d3)",
    )
    parser.add_argument(
        "--log",
        default="/tmp/session-manager.log",
        help="Path to session manager log file",
    )
    parser.add_argument(
        "--state-file",
        default="/tmp/claude-sessions/sessions.json",
        help="Path to sessions state file",
    )
    parser.add_argument(
        "--keep",
        default=None,
        help="Explicit chat_id:thread_id to keep, e.g. -1003506774897:8654 (overrides state file lookup)",
    )
    args = parser.parse_args()

    # Load config for bot token
    config = load_config()
    token = config.get("telegram", {}).get("token")
    if not token:
        logger.error("No Telegram bot token found in config.yaml")
        sys.exit(1)

    # Extract all thread IDs from log
    log_path = Path(args.log)
    if not log_path.exists():
        logger.error(f"Log file not found: {args.log}")
        sys.exit(1)

    all_topics = extract_thread_ids(args.log, args.session)
    if not all_topics:
        logger.info(f"No topics found for session {args.session} in {args.log}")
        return

    logger.info(f"Found {len(all_topics)} topic(s) for session {args.session} in log")

    # Determine which (chat_id, thread_id) to keep — abort if unknown
    keep_topic: tuple[int, int] | None = None
    if args.keep:
        try:
            parts = args.keep.split(":")
            keep_topic = (int(parts[0]), int(parts[1]))
        except (ValueError, IndexError):
            logger.error(f"Invalid --keep format: {args.keep!r}. Expected chat_id:thread_id, e.g. -1003506774897:8654")
            sys.exit(1)
    else:
        keep_topic = get_current_topic(args.session, args.state_file)

    logger.info(f"Topic to keep: chat_id={keep_topic[0]}, thread_id={keep_topic[1]}" if keep_topic else "Topic to keep: None")

    if keep_topic is None:
        logger.error(
            f"No persisted (chat_id, thread_id) for session {args.session}. "
            "Cannot determine which topic to keep — aborting to avoid deleting the active topic. "
            "Fix the session state first or pass --keep chat_id:thread_id to specify explicitly."
        )
        sys.exit(1)

    # Deduplicate — same (chat_id, thread_id) may appear multiple times in
    # the log.  Key on the tuple since thread IDs are scoped per chat.
    seen: set[tuple[int, int]] = set()
    unique_topics = []
    for chat_id, thread_id in all_topics:
        key = (chat_id, thread_id)
        if key not in seen:
            seen.add(key)
            unique_topics.append((chat_id, thread_id))

    logger.info(f"Unique topic IDs: {len(unique_topics)}")

    # Filter out the one to keep — compare full (chat_id, thread_id) tuple
    to_delete = [(c, t) for c, t in unique_topics if (c, t) != keep_topic]
    logger.info(f"Topics to delete: {len(to_delete)} (keeping {keep_topic[0]}:{keep_topic[1]})")

    if not to_delete:
        logger.info("Nothing to delete!")
        return

    if not args.execute:
        logger.info("=" * 60)
        logger.info("DRY RUN — pass --execute to actually delete topics")
        logger.info("=" * 60)

    deleted, failed = await delete_topics(token, to_delete, args.execute)

    logger.info("=" * 60)
    mode = "EXECUTED" if args.execute else "DRY RUN"
    logger.info(f"{mode}: {deleted} deleted, {failed} failed, 1 kept ({keep_topic[0]}:{keep_topic[1]})")
    logger.info("=" * 60)

    if failed > 0:
        sys.exit(1)


if __name__ == "__main__":
    asyncio.run(main())
