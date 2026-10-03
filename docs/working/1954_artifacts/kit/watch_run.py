#!/usr/bin/env python3
"""#1954 driver watch: exits with one line at the first event the driver must act on.

Events: the agent ended its turn (Stop hook), a denial for this agent (judge log), a permission
prompt on the agent's screen (must never happen), a model request error, 10 min with no activity.
usage: watch_run.py <run>   (reads runs-1954/proxy.jsonl, the judge log, tmux -L l1954 pane <run>)
"""
import json, os, subprocess, sys, time

RUN = sys.argv[1]
RUNS = os.path.expanduser("~/.local/share/local-agents-proto/runs-1954")
PROXY = f"{RUNS}/proxy.jsonl"
JUDGE = os.path.expanduser("~/.local/share/claude-sessions/local_judge.jsonl")
AGENT = f"local-{RUN}"
PROMPT_MARKS = ("Do you want to proceed", "Do you want to make this edit", "Do you want to create",
                "❯ 1. Yes", "Yes, and don't ask again")


def tail_from_end(path):
    return os.path.getsize(path) if os.path.exists(path) else 0


pos = {PROXY: tail_from_end(PROXY), JUDGE: tail_from_end(JUDGE)}
last = time.time()


def new_lines(path):
    if not os.path.exists(path):
        return []
    with open(path) as f:
        f.seek(pos[path])
        lines = f.readlines()
        pos[path] = f.tell()
    out = []
    for l in lines:
        try:
            out.append(json.loads(l))
        except ValueError:
            pass
    return out


def done(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {RUN}: {msg}", flush=True)
    sys.exit(0)


while True:
    for r in new_lines(PROXY):
        last = time.time()
        if r.get("kind") == "hook" and r.get("event") == "Stop":
            done("agent ended its turn (Stop hook)")
        if r.get("kind") == "model" and (r.get("error") or r.get("client_error") or r.get("status") not in (200, None)):
            done(f"model request error: {r.get('error') or r.get('client_error') or r.get('status')}")
    for r in new_lines(JUDGE):
        if r.get("session_id") == AGENT and r.get("decision") == "deny":
            done(f"DENY {r.get('denial_id')} [{r.get('stage')}] {str(r.get('command'))[:200]!r} -> {r.get('reason')}")
    pane = subprocess.run(["tmux", "-L", "l1954", "capture-pane", "-p", "-t", RUN],
                          capture_output=True, text=True).stdout
    if any(m in pane for m in PROMPT_MARKS):
        done("PERMISSION PROMPT on screen")
    if "CLAUDE-EXITED" in pane:
        done("Claude Code exited")
    if time.time() - last > 600:
        done("STALL: no model request or hook for 10 min")
    time.sleep(15)
