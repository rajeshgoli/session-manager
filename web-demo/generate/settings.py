#!/usr/bin/env python3
"""Write the invented Settings and Artifacts responses the recording does not hold.

Settings › Notifications, Devices & access, Worktrees, Reviews and About, and
the Artifacts page, read endpoints the storyline never touches. This writes
one response for each into fixtures/static/, indexed in fixtures/static.json
(see README.md, "The static layer"). Every name is invented; times are relative to the moment it runs, and
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


ROOT, COLD = "/home/demo/artifacts", "/Volumes/Archive/artifacts"


def artifact(now, name, *, state="live", gb, ticket=None, note, days, frozen_gb=None, hot=False, needs_you=None,
             rebuild_minutes=None):
    """One /artifacts row: the server's full field set, invented values."""
    ago = lambda **kw: stamp(now - timedelta(**kw))  # noqa: E731
    frozen = state == "frozen"
    disk, base = ("cold", COLD) if frozen else ("root", ROOT)
    data = int(gb * GB)
    return {
        "aliases": [], "consumer_unverifiable": False, "consumers": [], "dependents": [], "inputs": [], "jobs": [],
        "signals": [], "events": [], "name": name, "set_id": name, "root": ROOT, "repo": "acme/shop", "ticket": ticket,
        "project": 0, "epic": None, "origin": "recorded", "state": state, "location": disk, "hot": hot,
        "note": note, "description": None, "description_html": f"<p>{note}</p>\n", "report": None, "report_html": "",
        "created_at": ago(days=days), "first_seen_at": ago(days=days), "recorded_at": ago(days=days, minutes=-5),
        "last_used_at": ago(hours=3) if hot else None, "size_measured_at": ago(hours=2), "size_error": None,
        "data_bytes": data, "disk_bytes": data + 4096,
        "frozen_at": ago(days=days - 1) if frozen else None, "frozen_bytes": int(frozen_gb * GB) if frozen else None,
        "saved_bytes": data - int(frozen_gb * GB) if frozen else None, "ratio_estimate": gb / frozen_gb if frozen else 3.1,
        "ratio_sampled_at": ago(days=1), "thawed_at": None, "last_thaw": None, "thaw_eta_seconds": gb * 2.5 if frozen else 0,
        "placement": {"disk": disk, "disk_path": base, "path": f"{base}/{'.frozen/' if frozen else ''}{name}"},
        "backed_up_at": ago(hours=9), "backup_due": False, "backup_return_seconds": gb * 14, "backup_ship_bytes": data / 3,
        "backup_stamp": f"1:{data}:demo", "content_stamp": f"1:{data}:demo",
        "rebuildable": rebuild_minutes is not None, "rebuild_seconds": (rebuild_minutes or 0) * 60, "recipe_complete": 0,
        "keep_until": None, "keep_live_until": None, "grace_until": None, "gpu_cost_usd": None, "gpu_hours": None,
        "needs_owner": needs_you is not None, "needs_you": needs_you, "investigating": None, "op": None, "operation": None,
        "owner_session_id": None, "owner_session_name": None, "delete_reason": None, "deleted_at": None, "deleted_by": None,
    }


def artifacts(now):
    """The Artifacts page (saved work from last week's sprint and this one) and
    Settings › Auto PM, on in the demo with nothing shipped yet."""
    ago = lambda **kw: stamp(now - timedelta(**kw))  # noqa: E731
    stale = {"buttons": ["delete"], "name": "search-embeddings-v0", "reasons": ["would_delete"], "root": ROOT}
    rows = [
        artifact(now, "checkout-e2e-recordings", gb=6.4, ticket=46, days=0.2, hot=True, rebuild_minutes=25,
                 note="Browser recordings from the checkout e2e suite, kept for the flaky-step hunt on #46."),
        artifact(now, "shop-perf-baseline", gb=18.2, days=3, hot=True,
                 note="Perf baseline for checkout v2: request traces from the load run before #41 landed."),
        artifact(now, "search-index-qwen", gb=41.7, ticket=43, days=1, rebuild_minutes=95,
                 note="Search index built by shop-43 on the local Qwen model; #43's tests read it."),
        artifact(now, "cart-fixtures", gb=2.1, ticket=41, days=0.5, hot=True, rebuild_minutes=4,
                 note="Generated cart and coupon fixtures for #41."),
        artifact(now, "cart-v1-load-test", state="frozen", gb=27.5, frozen_gb=5.8, ticket=34, days=6,
                 note="Load-test output from Cart v1 (#34). Frozen after the goal closed."),
        artifact(now, "seed-db-snapshots", state="frozen", gb=12.3, frozen_gb=3.1, ticket=31, days=6,
                 note="Database snapshots the Cart v1 agents seeded from."),
        artifact(now, "search-embeddings-v0", gb=9.6, days=5, needs_you=stale,
                 note="First-pass search embeddings, replaced by search-index-qwen. Nothing has read it in five days."),
    ]
    used_root = sum(r["disk_bytes"] for r in rows if r["location"] == "root")
    used_cold = sum(r["frozen_bytes"] for r in rows if r["location"] == "cold")
    activity = {"as_of": stamp(now), "history": [
        {"actor": "a0000034", "actor_name": "cart-34", "created_at": ago(days=5), "started_at": ago(days=5),
         "finished_at": ago(days=5, minutes=-3), "data_bytes": rows[4]["data_bytes"], "frozen_bytes": rows[4]["frozen_bytes"],
         "error": None, "id": 2, "kind": "freezing", "names": ["cart-v1-load-test"], "phase": "done",
         "root": ROOT, "seconds": 171.0, "set_id": "cart-v1-load-test", "state": "done"},
    ], "operations": []}
    frozen = [r for r in rows if r["state"] == "frozen"]
    totals = {"folders": len(frozen), "data_bytes": sum(r["data_bytes"] for r in frozen),
              "frozen_bytes": sum(r["frozen_bytes"] for r in frozen), "saved_bytes": sum(r["saved_bytes"] for r in frozen)}
    activity["savings"] = {**totals, "roots": [{"root": ROOT, "cold_store": COLD, **totals}]}
    return {
        "/client/autopm": {
            "enabled": True, "paused_at": None, "skipped": False, "skip_window_starting": None,
            "changes": [], "ended": [], "waiting": [], "readout": None, "switches": {}, "switches_version": 1,
            "budget": {provider: {
                "allowance": allowance, "spent": spent, "reserved": reserved, "remaining": allowance - spent - reserved,
                "skipped": False, "tickets": [], "window_start": ago(days=3), "resets_at": stamp(now + timedelta(days=4)),
                "pm": {"held": pm, "left": round(pm - spent, 2), "spent": spent},
                "reviews": {"held": reserved - pm, "left": reserved - pm, "spent": 0.0},
            } for provider, allowance, spent, reserved, pm in (("claude", 5.0, 0.6, 3.0, 2.0), ("codex", 20.0, 1.5, 6.0, 1.0))},
        },
        "/artifacts": {
            "artifacts": rows, "can_act": False, "mode": "report_only", "keep_live_days": 30, "epics": [],
            "needs_you": [stale], "needs_you_count": 1,
            "recent": [{"count": 2, "kind": "frozen"}, {"count": 1, "kind": "moved"}],
            "filesystems": [
                {"path": ROOT, "role": "root", "mounted": True, "bytes": 2 * 1000 ** 4, "free": 2 * 1000 ** 4 - used_root - 610 * GB,
                 "floor": 100 * GB, "enough": 100 * GB, "outstanding": 0, "waiting_jobs": []},
                {"path": COLD, "role": "cold", "mounted": True, "bytes": 4 * 1000 ** 4, "free": 4 * 1000 ** 4 - used_cold - 1200 * GB,
                 "floor": 200 * GB, "enough": 200 * GB, "outstanding": 0, "waiting_jobs": []},
            ],
            "backup": {
                "enabled": True, "confirmed_at": ago(hours=9), "update": None, "scope_roots": ["~/projects", "~/.config"],
                "fetch_rate": 31250000.0, "apply_rate": 62500000.0, "probe_rate": 31250000.0, "effective_rate": 31250000.0,
                "card": {"last_night": {"bytes": int(3.8 * GB), "seconds": 412, "confirmed_at": ago(hours=9)},
                         "measured": {"fetch": True, "apply": True, "probe": False}, "nights": [], "weekly_check": None,
                         "disk": {"total_bytes": 4 * 1000 ** 4, "free_bytes": int(2.6 * 1000 ** 4),
                                  "holding_bytes": int(1.2 * 1000 ** 4), "holding_oldest": None, "pruned_on": None,
                                  "health": "verified"}},
            },
        },
        "/artifact-activity": activity,
    }


def main():
    now = datetime.now(timezone.utc).replace(microsecond=0)
    static_dir = os.path.join(FIXTURES, "static")
    os.makedirs(static_dir, exist_ok=True)
    index_path = os.path.join(FIXTURES, "static.json")
    with open(index_path) as f:
        index = json.load(f)
    bodies = {**responses(now), **artifacts(now)}
    for url, body in bodies.items():
        name = "settings_" + "".join(c if c.isalnum() else "_" for c in url.strip("/")) + ".json"
        with open(os.path.join(static_dir, name), "w") as f:
            json.dump(body, f, separators=(",", ":"))
        index["responses"][url] = {"file": f"static/{name}", "captured_at": stamp(now),
                                   "status": 200, "content_type": "application/json"}
    with open(index_path, "w") as f:
        json.dump(index, f, indent=1, sort_keys=True)
    print(f"wrote {len(bodies)} Settings and Artifacts responses to {static_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
