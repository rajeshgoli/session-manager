#!/usr/bin/env python3
"""#1966: per-run metrics for an opencode benchmark run. usage: metrics.py <run dir> <runs dir>"""
import json, statistics, sys, urllib.request
from collections import Counter
R, RU = sys.argv[1:]
t0 = float(open(f"{R}/t0").read())
cfg = json.load(open(f"{R}/bridge.json"))
try:
    ms = json.load(urllib.request.urlopen(f"{cfg['oc_url']}/session/{cfg['oc_session']}/message"))
    json.dump(ms, open(f"{R}/messages.json", "w"))
except OSError:  # opencode no longer serves this run: use the copy saved at the end of the run
    ms = json.load(open(f"{R}/messages.json"))
steps = [p for m in ms for p in m["parts"] if p["type"] == "step-finish"]
tools = [p for m in ms for p in m["parts"] if p["type"] == "tool"]
first = min(m["info"]["time"]["created"] for m in ms)
last = max(m["info"]["time"].get("completed") or m["info"]["time"]["created"] for m in ms)
tk = [s["tokens"] for s in steps]
inp = sum(t["input"] for t in tk); cr = sum(t["cache"]["read"] for t in tk); out = sum(t["output"] + t["reasoning"] for t in tk)
dec = [json.loads(l) for l in open(f"{RU}/local_judge.jsonl") if json.loads(l)["t"] > t0 and json.loads(l)["t"] < last / 1000 + 5]
js = [d["ms"] for d in dec if d["stage"] == "judge"]
ctx = [json.loads(l).get("context_percent") for l in open(f"{R}/sm_view.jsonl")]
out_d = {"user_turns": sum(m["info"]["role"] == "user" for m in ms), "active_min": round((last - first) / 60000, 2),
         "model_requests": len(steps), "tool_calls": len(tools),
         "tool_errors": sum(p["state"]["status"] == "error" for p in tools),
         "first_prompt": tk[0]["input"] + tk[0]["cache"]["read"] + tk[0]["cache"]["write"],
         "input": inp, "cache_read": cr, "output_incl_reasoning": out, "reuse": round(cr / (inp + cr), 3),
         "largest_prompt": max(t["input"] + t["cache"]["read"] for t in tk),
         "decisions": {f"{a}/{b}": n for (a, b), n in Counter((d["stage"], d["decision"]) for d in dec).items()},
         "judge_median_ms": statistics.median(js) if js else None, "judge_max_ms": max(js) if js else None,
         "sm_context_percent_max": max(c for c in ctx if c is not None),
         "denials": [{"id": d.get("denial_id"), "stage": d["stage"], "command": d["command"][:200], "reason": d["reason"][:200]}
                     for d in dec if d["decision"] == "deny"]}
json.dump(out_d, open(f"{R}/metrics.json", "w"), indent=1)
print(json.dumps(out_d, indent=1))
