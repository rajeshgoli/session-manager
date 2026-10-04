#!/usr/bin/env python3
"""#1966: sample the scratch sm's view of a1966oc0 every 0.25 s; print one line per change."""
import json, sys, time, urllib.request
keys = ("status", "activity_state", "last_tool_name", "last_action_summary", "context_percent", "tokens_used", "provider_resume_id")
prev = None; end = time.time() + float(sys.argv[2] if len(sys.argv) > 2 else 60)
out = open(sys.argv[1], "a")
while time.time() < end:
    try:
        d = json.load(urllib.request.urlopen("http://127.0.0.1:18450/sessions/a1966oc0", timeout=2))
        d = d.get("session", d)
        cur = {k: d.get(k) for k in keys if k in d}
    except Exception as e:
        cur = {"error": str(e)[:80]}
    if cur != prev:
        out.write(json.dumps({"t": round(time.time(), 2), **cur}) + "\n"); out.flush(); prev = cur
    time.sleep(0.25)
