from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

import pytest


SCRIPT_PATH = (
    Path(__file__).resolve().parents[2]
    / "scripts"
    / "cleanup_orphan_forum_topics_mtproto.py"
)
SPEC = importlib.util.spec_from_file_location("cleanup_orphan_forum_topics_mtproto", SCRIPT_PATH)
assert SPEC and SPEC.loader
cleanup_script = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = cleanup_script
SPEC.loader.exec_module(cleanup_script)


def test_extract_session_id_matches_hex_token() -> None:
    assert cleanup_script.extract_session_id("engineer-2005 (f968140d)") == "f968140d"
    assert cleanup_script.extract_session_id("no session id here") is None


def test_build_delete_plan_filters_hidden_and_live_topics() -> None:
    topics = [
        cleanup_script.TopicCandidate(topic_id=1, title="live aaaaaaaa", session_id="aaaaaaaa"),
        cleanup_script.TopicCandidate(topic_id=2, title="orphan bbbbbbbb", session_id="bbbbbbbb"),
        cleanup_script.TopicCandidate(topic_id=3, title="hidden cccccccc", session_id="cccccccc", hidden=True),
        cleanup_script.TopicCandidate(topic_id=4, title="plain topic", session_id=None),
    ]

    plan = cleanup_script.build_delete_plan(topics, {"aaaaaaaa"})

    assert [topic.topic_id for topic in plan] == [2]


def test_parse_active_session_ids_ignores_missing_ids() -> None:
    payload = {
        "sessions": [
            {"id": "aaaaaaaa"},
            {"id": None},
            {},
            {"id": "bbbbbbbb"},
        ]
    }

    assert cleanup_script.parse_active_session_ids(payload) == {"aaaaaaaa", "bbbbbbbb"}


def test_load_active_session_ids_parses_sessions_payload(monkeypatch: pytest.MonkeyPatch) -> None:
    class FakeResponse:
        def __enter__(self) -> "FakeResponse":
            return self

        def __exit__(self, exc_type, exc, tb) -> None:
            return None

        def read(self) -> bytes:
            return json.dumps({"sessions": [{"id": "aaaaaaaa"}, {"id": "bbbbbbbb"}]}).encode()

    monkeypatch.setattr(cleanup_script.urllib.request, "urlopen", lambda request, timeout=5: FakeResponse())

    assert cleanup_script.load_active_session_ids("http://127.0.0.1:8420") == {"aaaaaaaa", "bbbbbbbb"}


def test_load_active_session_ids_wraps_transport_errors(monkeypatch: pytest.MonkeyPatch) -> None:
    def _boom(request, timeout=5):
        raise cleanup_script.urllib.error.URLError("connection refused")

    monkeypatch.setattr(cleanup_script.urllib.request, "urlopen", _boom)

    with pytest.raises(RuntimeError, match="Failed to query Session Manager"):
        cleanup_script.load_active_session_ids("http://127.0.0.1:8420")
