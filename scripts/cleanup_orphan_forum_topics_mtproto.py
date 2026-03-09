#!/usr/bin/env python3
"""
Enumerate Telegram forum topics via MTProto and delete orphaned session topics.

This is an operator tool for historical cleanup where Session Manager no longer
has the topic mapping. It uses a Telegram user session (MTProto) to enumerate
forum topics, extracts session IDs from topic titles, and deletes topics whose
session IDs are not currently active in Session Manager.

Enumeration uses Telethon. Deletion uses the existing Telegram bot token from
config.yaml, so the bot must still have permission to delete forum topics.

Examples:
    # Dry run against the default forum chat from config.yaml
    TELEGRAM_API_ID=123 TELEGRAM_API_HASH=abc \
      ./venv/bin/python scripts/cleanup_orphan_forum_topics_mtproto.py

    # Execute deletions
    TELEGRAM_API_ID=123 TELEGRAM_API_HASH=abc \
      ./venv/bin/python scripts/cleanup_orphan_forum_topics_mtproto.py --execute

    # Override the forum chat ID and API endpoint
    TELEGRAM_API_ID=123 TELEGRAM_API_HASH=abc \
      ./venv/bin/python scripts/cleanup_orphan_forum_topics_mtproto.py \
        --chat-id -1001234567890 \
        --api-url http://127.0.0.1:8420
"""

from __future__ import annotations

import argparse
import asyncio
import dataclasses
import datetime as dt
import json
import logging
import os
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Iterable, Optional

import yaml

LOG = logging.getLogger(__name__)
SESSION_ID_PATTERN = re.compile(r"\b([0-9a-f]{8})\b")
DEFAULT_API_URL = "http://127.0.0.1:8420"
PAGE_SIZE = 100
DELETE_DELAY_SECONDS = 0.15


@dataclasses.dataclass(frozen=True)
class TopicCandidate:
    topic_id: int
    title: str
    session_id: Optional[str]
    hidden: bool = False


def load_config(config_path: Path) -> dict:
    if not config_path.exists():
        raise FileNotFoundError(f"Config not found: {config_path}")
    return yaml.safe_load(config_path.read_text()) or {}


def extract_session_id(title: str) -> Optional[str]:
    match = SESSION_ID_PATTERN.search(title or "")
    return match.group(1) if match else None


def build_delete_plan(
    topics: Iterable[TopicCandidate],
    active_session_ids: set[str],
) -> list[TopicCandidate]:
    plan: list[TopicCandidate] = []
    for topic in topics:
        if topic.hidden:
            continue
        if not topic.session_id:
            continue
        if topic.session_id in active_session_ids:
            continue
        plan.append(topic)
    return plan


def parse_active_session_ids(payload: dict) -> set[str]:
    sessions = payload.get("sessions") or []
    return {str(session.get("id")) for session in sessions if session.get("id")}


def load_active_session_ids(api_url: str) -> set[str]:
    url = f"{api_url.rstrip('/')}/sessions"
    request = urllib.request.Request(url, method="GET")
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            payload = json.loads(response.read().decode())
    except urllib.error.URLError as exc:
        raise RuntimeError(f"Failed to query Session Manager at {url}: {exc}") from exc
    except Exception as exc:
        raise RuntimeError(f"Failed to parse Session Manager payload from {url}: {exc}") from exc

    return parse_active_session_ids(payload)


async def enumerate_topics(
    chat_id: int,
    *,
    api_id: int,
    api_hash: str,
    session_path: Path,
) -> list[TopicCandidate]:
    try:
        from telethon import TelegramClient, functions
    except ImportError as exc:
        raise RuntimeError(
            "Telethon is required for MTProto topic enumeration. "
            "Install it with: ./venv/bin/pip install telethon"
        ) from exc

    session_path.parent.mkdir(parents=True, exist_ok=True)
    topics: list[TopicCandidate] = []
    offset_date: dt.datetime | int = 0
    offset_id = 0
    offset_topic = 0

    async with TelegramClient(str(session_path), api_id, api_hash) as client:
        entity = await client.get_input_entity(chat_id)
        while True:
            result = await client(
                functions.messages.GetForumTopicsRequest(
                    peer=entity,
                    offset_date=offset_date,
                    offset_id=offset_id,
                    offset_topic=offset_topic,
                    limit=PAGE_SIZE,
                )
            )
            page_topics = list(getattr(result, "topics", []) or [])
            if not page_topics:
                break

            message_dates = {
                getattr(message, "id", None): getattr(message, "date", None)
                for message in (getattr(result, "messages", []) or [])
            }

            for topic in page_topics:
                title = getattr(topic, "title", "") or ""
                topics.append(
                    TopicCandidate(
                        topic_id=int(getattr(topic, "id")),
                        title=title,
                        session_id=extract_session_id(title),
                        hidden=bool(getattr(topic, "hidden", False)),
                    )
                )

            last_topic = page_topics[-1]
            offset_topic = int(getattr(last_topic, "id"))
            offset_id = int(getattr(last_topic, "top_message", 0))
            offset_date = message_dates.get(offset_id) or 0
            if len(page_topics) < PAGE_SIZE:
                break

    return topics


async def delete_topics(
    *,
    bot_token: str,
    chat_id: int,
    topics: list[TopicCandidate],
    execute: bool,
) -> tuple[int, int]:
    from telegram import Bot

    bot = Bot(token=bot_token)
    deleted = 0
    failed = 0
    try:
        for topic in topics:
            if not execute:
                LOG.info("[DRY RUN] Would delete topic %s: %s", topic.topic_id, topic.title)
                deleted += 1
                continue
            try:
                await bot.delete_forum_topic(chat_id=chat_id, message_thread_id=topic.topic_id)
                deleted += 1
            except Exception as exc:  # pragma: no cover - network/API failure
                failed += 1
                LOG.warning("Failed to delete topic %s (%s): %s", topic.topic_id, topic.title, exc)
            await asyncio.sleep(DELETE_DELAY_SECONDS)
    finally:
        await bot.shutdown()
    return deleted, failed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Delete orphaned Telegram forum topics whose session IDs no longer exist in SM.",
    )
    parser.add_argument("--execute", action="store_true", help="Actually delete topics (default: dry run)")
    parser.add_argument("--api-url", default=DEFAULT_API_URL, help="Session Manager API base URL")
    parser.add_argument("--config-path", default="config.yaml", help="Path to Session Manager config file")
    parser.add_argument("--chat-id", type=int, default=None, help="Forum chat ID (defaults to config telegram.default_forum_chat_id)")
    parser.add_argument(
        "--telethon-session",
        default="~/.local/share/claude-sessions/telethon-cleanup.session",
        help="Path to the Telethon user session file",
    )
    return parser.parse_args()


async def main() -> int:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    args = parse_args()

    config = load_config(Path(args.config_path).expanduser())
    telegram_config = config.get("telegram", {})
    bot_token = telegram_config.get("token")
    if not bot_token:
        raise RuntimeError("No telegram.token configured")

    chat_id = args.chat_id if args.chat_id is not None else telegram_config.get("default_forum_chat_id")
    if not chat_id:
        raise RuntimeError("No forum chat id provided and telegram.default_forum_chat_id is unset")

    api_id_raw = os.environ.get("TELEGRAM_API_ID")
    api_hash = os.environ.get("TELEGRAM_API_HASH")
    if not api_id_raw or not api_hash:
        raise RuntimeError("TELEGRAM_API_ID and TELEGRAM_API_HASH must be set for MTProto enumeration")

    active_session_ids = load_active_session_ids(args.api_url)
    if args.execute and not active_session_ids:
        raise RuntimeError(
            "Session Manager returned zero active sessions; refusing to delete every "
            "session-shaped Telegram topic. Re-run as a dry run, or restart SM first."
        )
    LOG.info("Loaded %s active session IDs from %s", len(active_session_ids), args.api_url)

    topics = await enumerate_topics(
        chat_id=chat_id,
        api_id=int(api_id_raw),
        api_hash=api_hash,
        session_path=Path(args.telethon_session).expanduser(),
    )
    LOG.info("Enumerated %s forum topics", len(topics))

    plan = build_delete_plan(topics, active_session_ids)
    LOG.info("Matched %s orphaned session topic(s) for cleanup", len(plan))
    for topic in plan:
        LOG.info("orphan: topic_id=%s session=%s title=%r", topic.topic_id, topic.session_id, topic.title)

    deleted, failed = await delete_topics(
        bot_token=bot_token,
        chat_id=chat_id,
        topics=plan,
        execute=args.execute,
    )
    LOG.info("Finished cleanup: deleted=%s failed=%s execute=%s", deleted, failed, args.execute)
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    try:
        raise SystemExit(asyncio.run(main()))
    except Exception as exc:
        LOG.error("%s", exc)
        raise SystemExit(1)
