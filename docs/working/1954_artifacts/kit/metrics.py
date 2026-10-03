#!/usr/bin/env python3
"""#1954 per-run metrics from the logging proxy, the judge log and the memory sampler.

Runs were sequential, so each run's records are those between its t0 and t_end. Active time is
the sum of its turns (UserPromptSubmit -> Stop); time waiting for a review between turns is not
active. Writes runs-1954/metrics.json and prints one block per run.
usage: metrics.py
"""
import json, os, statistics

R = os.path.expanduser("~/.local/share/local-agents-proto/runs-1954")
J = os.path.expanduser("~/.local/share/claude-sessions/local_judge.jsonl")
proxy = [json.loads(l) for l in open(f"{R}/proxy.jsonl")]
judge = [json.loads(l) for l in open(J)]
mem = [l.split("\t") for l in open(f"{R}/mem.tsv")]


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))] if xs else None


out = {}
for run in ("1855", "1913", "1892"):
    t0 = float(open(f"{R}/{run}/t0").read())
    t1 = float(open(f"{R}/{run}/t_end").read())
    hooks = [r for r in proxy if r.get("kind") == "hook" and t0 - 5 <= r["t"] <= t1]
    model = [r for r in proxy if r.get("kind") == "model" and t0 - 5 <= r["t_start"] <= t1]
    turns, start = [], t0
    for h in hooks:
        if h.get("event") == "UserPromptSubmit" and start is None:
            start = h["t"]
        if h.get("event") == "Stop" and start is not None:
            turns.append((start, h["t"]))
            start = None
    main = [r for r in model if (r.get("n_tools") or 0) > 0]
    ttft = [r["t_first_token"] - r["t_start"] for r in main if r.get("t_first_token")]
    reused = sum((r.get("usage") or {}).get("cache_read_input_tokens", 0) for r in main)
    fresh = sum((r.get("usage") or {}).get("input_tokens", 0) for r in main)
    first = main[0].get("usage") or {} if main else {}
    jd = [r for r in judge if r["session_id"] == f"local-{run}"]
    judged = [r["ms"] / 1000 for r in jd if r["stage"] == "judge"]
    m = [(float(a[0]), float(a[2]), float(a[3])) for a in mem if len(a) >= 4 and t0 <= float(a[0]) <= t1]
    o = {
        "wall_min": round((t1 - t0) / 60, 1),
        "turns": len(turns),
        "active_min": round(sum(b - a for a, b in turns) / 60, 1),
        "first_turn_min": round((turns[0][1] - turns[0][0]) / 60, 1) if turns else None,
        "model_requests": len(model), "main_requests": len(main),
        "first_prompt_tokens": sum(first.get(k, 0) for k in ("input_tokens", "cache_read_input_tokens",
                                                            "cache_creation_input_tokens")),
        "prompt_reused": round(reused / (reused + fresh), 3) if reused + fresh else None,
        "ttft_median_s": round(statistics.median(ttft), 1) if ttft else None,
        "ttft_p90_s": round(pct(ttft, 0.9), 1) if ttft else None,
        "tool_calls": len(jd),
        "rule_allow": sum(r["stage"] == "rule" and r["decision"] == "allow" for r in jd),
        "rule_deny": sum(r["stage"] == "rule" and r["decision"] == "deny" for r in jd),
        "judge_allow": sum(r["stage"] == "judge" and r["decision"] == "allow" for r in jd),
        "judge_deny": sum(r["stage"] == "judge" and r["decision"] == "deny" for r in jd),
        "judge_errors": sum(bool(r.get("judge_error")) for r in jd),
        "judge_median_s": round(statistics.median(judged), 2) if judged else None,
        "judge_p90_s": round(pct(judged, 0.9), 2) if judged else None,
        "judge_max_s": round(max(judged), 2) if judged else None,
        "denials": [{"id": r["denial_id"], "stage": r["stage"], "command": (r["command"] or "")[:160],
                     "reason": r["reason"]} for r in jd if r["decision"] == "deny"],
        "compactions": sum(1 for h in hooks if h.get("event") == "compaction"),
        "request_errors": sum(1 for r in model if r.get("error") or r.get("client_error")
                              or r.get("status") not in (200, None)),
        "peak_wired_gb": round(max(x[1] for x in m), 1) if m else None,
        "lowest_available_gb": round(min(x[2] for x in m), 1) if m else None,
    }
    out[run] = o
    print(run, json.dumps({k: v for k, v in o.items() if k != "denials"}))
    for d in o["denials"]:
        print("   ", d["id"], d["stage"], d["command"][:90].replace("\n", " "), "|", d["reason"][:70])
json.dump(out, open(f"{R}/metrics.json", "w"), indent=1)
