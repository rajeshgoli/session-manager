#!/usr/bin/env python3
"""Snapshot a real server's queue history and host load for the demo, with every name invented.

    python3 web-demo/generate/queue.py               # reads http://127.0.0.1:8420
    python3 web-demo/generate/queue.py --server URL

The numbers stay real: when each job queued, started and ended, how it ended,
its memory, CPU and process counts, and the host's load. Labels, paths,
agent names, session and job ids, and lane goals are replaced; argv becomes
an invented script. The result is checked for any original name before it is
written to fixtures/static/ (see README.md, "The static layer"):

- queue_history.json: the last 24 hours of ended jobs. The worker adds them
  to the storyline's own ended jobs on the Queue page.
- host_status.json: the host's load, which record.py starts its storyline
  host from.
- usage_meters.json: the Claude and Codex usage meters, which record.py
  starts its storyline meters from.
"""
import argparse
import hashlib
import json
import os
import re
import sys
import urllib.request
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(os.path.dirname(HERE), "fixtures")
HOME = "/home/demo"
LOGS = f"{HOME}/.local/share/claude-sessions/queue-runner/logs"

WORDS = ["reindex", "backfill", "replay", "embed", "rollup", "import", "sync", "scan", "train", "bench",
         "export", "resize", "compact", "warm", "audit", "score", "fuzz", "rank", "diff", "load"]
# Kept as they are: lane and shard suffixes, handoff marks, short letters.
GENERIC = re.compile(r"^(lane\d+|h\d+|[A-Za-z]\d?|\d{1,2}|v\d+)$")
# The server's own words in fields that are not names.
SERVER_TEXT = re.compile(r"^(Stopped: used [\d.]+ GiB against its [\d.]+ GiB memory limit"
                         r"|Stopped to make room for a perf run; not restarted"
                         r"|Never started; gave up after \d+[smh]"
                         r"|Stopped: ran past its \d+[smh] limit"
                         r"|Exited with code \d+|Cancelled|Succeeded|Failed)$")


def fetch(server, path):
    with urllib.request.urlopen(server + path, timeout=60) as response:
        return json.load(response)


def digest(text):
    return hashlib.sha1(text.encode()).hexdigest()


class Names:
    def __init__(self):
        self.numbers, self.words = {}, {}

    def number(self, real):
        return self.numbers.setdefault(real, 60 + len(self.numbers))

    def word(self, real):
        return self.words.setdefault(real.lower(), WORDS[len(self.words) % len(WORDS)])

    def label(self, real):
        """`1978-sum-lane0` becomes `61-reindex-lane0`: same shape, invented words."""
        if not real:
            return real
        out = []
        for index, token in enumerate(real.split("-")):
            if token.isdigit() and len(token) >= 3:
                out.append(str(self.number(token)))
            elif index and GENERIC.match(token):
                out.append(token)
            else:
                out.append(self.word(token))
        return "-".join(out)

    def agent(self, real):
        """`sm-2053-h2` becomes `shop-62-h2`."""
        if not real:
            return real
        tokens = real.split("-")
        numbers = [t for t in tokens if t.isdigit() and len(t) >= 3]
        rest = [t for t in tokens[1:] if GENERIC.match(t) and not t.isdigit()]
        return "-".join(["shop", *(str(self.number(n)) for n in numbers[:1]), *rest]) if numbers else "shop-agent"


def job_id(real):
    return f"job_{digest(real)[:12]}" if real else real


def session_id(real):
    return digest(real)[:8] if real else real


def scrub_job(job, names):
    job = json.loads(json.dumps(job))
    label = names.label(job.get("label"))
    fake_id = job_id(job["id"])
    ticket = label.split("-")[0] if label and label.split("-")[0].isdigit() else "60"
    job.update(
        id=fake_id, label=label,
        argv=["bash", f"scripts/{'-'.join(label.split('-')[1:2]) or 'job'}.sh"],
        cwd=f"{HOME}/worktrees/shop-{ticket}",
        log_path=f"{LOGS}/{fake_id}.log",
        readable_log_path=f"{LOGS}/{label}--{fake_id}.log",
        notify_name=names.agent(job.get("notify_name")),
        requester_name=names.agent(job.get("requester_name")),
        notify_session_id=session_id(job.get("notify_session_id")),
        requester_session_id=session_id(job.get("requester_session_id")),
        script_path=None, cancel_detail=None, review=None, local_agent_id=None,
    )
    if job.get("lane_goal"):
        job["lane_goal"] = {"number": names.number(str(job["lane_goal"]["number"])), "repo": "acme/shop",
                            "title": "Faster price feed"}
    if job.get("ended_summary") and not SERVER_TEXT.match(job["ended_summary"]):
        job["ended_summary"] = None
    blockers = job.get("wait_blockers") or {}
    for key in ("queued_ahead", "running"):
        for other in blockers.get(key) or []:
            other.update(id=job_id(other.get("id")), label=names.label(other.get("label")))
    return job


def originals(doc):
    """Every name in the real response, for the leak check."""
    found = {"rajesh", "goli", "fractal", "studio.local", "/Users/"}
    for job in doc["ended"] + doc["running"] + doc["queued"]:
        for field in ("label", "notify_name", "requester_name", "cwd", "id",
                      "notify_session_id", "requester_session_id"):
            value = job.get(field)
            if isinstance(value, str):
                found.add(value)
                if field in ("label", "notify_name", "requester_name"):
                    found.update(t for t in value.split("-") if not t.isdigit() and not GENERIC.match(t))
        found.update(arg for arg in job.get("argv") or [] if "/" in arg or len(arg) >= 12)
        if job.get("lane_goal"):
            found.update({job["lane_goal"]["repo"], job["lane_goal"]["title"]})
    generic = set(WORDS) | {"home", "demo", "shop", "worktrees", "scripts", "background", "tests", "perf", "review", "provider",
                                       "bash"}
    return {text.lower() for text in found if len(text) >= 4 and text.lower() not in generic}


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--server", default="http://127.0.0.1:8420")
    parser.add_argument("--out", default=FIXTURES)
    args = parser.parse_args()
    doc = fetch(args.server, "/client/queue?ended_hours=24")
    host = fetch(args.server, "/client/host-status")
    meters = fetch(args.server, "/client/usage/meters")
    for meter in meters["meters"]:
        provider = meter["provider"]
        meter.update(account_key=f"{provider}:demo", label=f"Alex ({provider.capitalize()})", scope=None)
    names = Names()
    now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    history = {"captured_at": now, "ended": [scrub_job(job, names) for job in doc["ended"]]}
    host.update(host="dev-box", sampled_at=now)
    real = originals(doc)
    real |= {m["account_key"].split(":", 1)[1].lower() for m in fetch(args.server, "/client/usage/meters")["meters"]}
    real.add("gmail")
    text = json.dumps([history, host, meters]).lower()
    leaks = sorted(word for word in real if word in text)
    if leaks:
        raise SystemExit(f"refusing to write: real names survived the scrub: {leaks[:20]}")
    static_dir = os.path.join(args.out, "static")
    os.makedirs(static_dir, exist_ok=True)
    for name, body in (("queue_history.json", history), ("host_status.json", host), ("usage_meters.json", meters)):
        with open(os.path.join(static_dir, name), "w") as f:
            json.dump(body, f, separators=(",", ":"))
    print(f"wrote {len(history['ended'])} ended jobs, the host's load and the usage meters to {static_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
