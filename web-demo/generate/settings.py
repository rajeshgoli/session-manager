#!/usr/bin/env python3
"""Write the invented Settings responses the recording does not hold.

Settings › Notifications, Devices & access, Worktrees, Reviews and About read
endpoints the storyline never touches. This writes one response for each into
fixtures/static/, indexed in fixtures/static.json (see README.md, "The static
layer"). Every name is invented; times are relative to the moment it runs, and
the worker moves them to the visitor's clock.
"""
import json
import os
import sys
from datetime import datetime, timedelta, timezone

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "fixtures")
GB = 1024 ** 3
MB = 1024 ** 2


def stamp(at):
    return at.strftime("%Y-%m-%dT%H:%M:%SZ")


def responses(now):
    ago = lambda **kw: stamp(now - timedelta(**kw))  # noqa: E731
    return {
        "/client/push/status": {"configured": True, "devices": [{"device_name": "Pixel 9"}]},
        "/client/devices": {
            "browser_sign_in": None, "owner_view": True, "runtime_only_revocations": False,
            "devices": [
                {"device_name": "Pixel 9", "kind": "phone", "last_seen_at": ago(minutes=4), "user_id": "alex",
                 "device_key_id": "android-5d1e7c20a9b3f441", "enabled": True, "revoked": False},
                {"device_name": "macbook", "kind": "computer", "last_seen_at": ago(minutes=1), "user_id": "alex",
                 "device_key_id": "macbook", "enabled": True, "revoked": False},
                {"device_name": "workstation", "kind": "computer", "last_seen_at": ago(hours=20), "user_id": "alex",
                 "device_key_id": "workstation", "enabled": True, "revoked": False},
            ],
        },
        "/client/reviews/status": {
            "github_codex": {"next_check_at": None, "paused_at": None, "quota_resets_at": stamp(now + timedelta(days=6)),
                             "refusal_url": None, "state": "available"},
            "last_24h": {"claude_runs": 2, "codex_runs": 5, "github_codex": 14, "no_reviewer": 0},
            "meters": {"claude": 20.0, "codex": 5.0}, "needs_you": [], "running": [],
        },
        "/apps/session-manager-android/meta.json": {
            "artifact_hash": "5a0e91c4", "release_notes": "Board tickets show Waits until and offer Start when ready.",
            "size_bytes": 28533598, "uploaded_at": ago(days=1, hours=3), "uploaded_by": "alex",
            "version_code": 1133, "version_name": "0.5.46",
        },
        # The worker keeps these per visit, so Delete and Keep work (sw.js).
        "/worktrees/leftover": {"worktrees": [
            {"path": "/home/demo/worktrees/shop-34-format-prices", "repo": "acme/shop", "ticket": 34, "pr": None,
             "sessions": ["cart-34"], "reason": "2 uncommitted changes", "kept": None,
             "retired_at": ago(days=4, hours=2), "checked_at": ago(minutes=40),
             "bytes": int(3.4 * GB), "build_bytes": int(3.1 * GB)},
            {"path": "/home/demo/worktrees/shop-32-empty-cart", "repo": "acme/shop", "ticket": 32, "pr": 36,
             "sessions": ["cart-32", "cart-32-h2"], "reason": "commits not pushed: 1", "kept": None,
             "retired_at": ago(days=5), "checked_at": ago(minutes=40),
             "bytes": int(1.9 * GB), "build_bytes": int(1.7 * GB)},
            {"path": "/home/demo/worktrees/shop-perf-baseline", "repo": "acme/shop", "ticket": None, "pr": None,
             "sessions": ["scout"], "reason": "kept: perf baseline for checkout v2", "kept": "perf baseline for checkout v2",
             "retired_at": ago(days=2, hours=5), "checked_at": ago(minutes=40),
             "bytes": int(212 * MB), "build_bytes": 0},
        ]},
    }


def main():
    now = datetime.now(timezone.utc).replace(microsecond=0)
    static_dir = os.path.join(FIXTURES, "static")
    os.makedirs(static_dir, exist_ok=True)
    index_path = os.path.join(FIXTURES, "static.json")
    with open(index_path) as f:
        index = json.load(f)
    for url, body in responses(now).items():
        name = "settings_" + "".join(c if c.isalnum() else "_" for c in url.strip("/")) + ".json"
        with open(os.path.join(static_dir, name), "w") as f:
            json.dump(body, f, separators=(",", ":"))
        index["responses"][url] = {"file": f"static/{name}", "captured_at": stamp(now),
                                   "status": 200, "content_type": "application/json"}
    with open(index_path, "w") as f:
        json.dump(index, f, indent=1, sort_keys=True)
    print(f"wrote {len(responses(now))} Settings responses to {static_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
