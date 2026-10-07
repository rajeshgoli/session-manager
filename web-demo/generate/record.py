#!/usr/bin/env python3
"""Records the demo sprint: a scripted team on a throwaway sm-server.

Starts a scratch sm-server on fresh state under ROOT, plays the storyline in
real time (ticks of --tick seconds), and after each tick saves what every
endpoint the web UI reads returns. Fake agents act through the real `sm` CLI
where it works with the server's runtime off; the rest is written straight
into the server's state files. See ../README.md for the output format.

Run it through the queue (it takes about ten minutes):
    sm queue run --type background --max-wait 2h --timeout 30m --label demo-record \
        --cwd <repo> -- python3 web-demo/generate/record.py
"""
import argparse
import datetime
import getpass
import hashlib
import json
import os
import re
import shutil
import socket
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
WEB_DEMO = os.path.dirname(HERE)
FAKE_BIN = os.path.join(HERE, "fake-bin")
# Short and fixed: the queue authority socket path must fit SUN_LEN, and the
# scrub step rewrites this prefix away so paths read /home/demo/...
ROOT = "/tmp/smdemo-rec"
REPO = "acme/shop"
OWNER = "Alex"
HUMAN = "alex"
HOME = f"{ROOT}/home/demo"
SHOP = f"{HOME}/shop"


def now_utc():
    return datetime.datetime.now(datetime.timezone.utc)


def iso(moment=None):
    """Session-file format: microseconds, Z."""
    return (moment or now_utc()).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def iso_s(moment=None):
    """Seconds, Z: owner messages, docs, board, claims."""
    return (moment or now_utc()).strftime("%Y-%m-%dT%H:%M:%SZ")


def ago(seconds):
    return now_utc() - datetime.timedelta(seconds=seconds)


def sha(text):
    return hashlib.sha1(text.encode()).hexdigest()


# ---- the world: tickets, PRs and agents ------------------------------------

TICKETS = {
    40: ("Checkout v2", None, []),
    41: ("Cart totals service with tax and discounts", 40, []),
    42: ("Coupon rules engine", 40, []),
    43: ("Product search index", 40, []),
    44: ("Payment provider adapter", 40, [41]),
    45: ("Order confirmation emails", 40, [42]),
    46: ("Checkout page", 40, [44, 45]),
}
# Last week's sprint, finished before the recording starts. Its agents are the
# retired ones History offers to bring back.
PAST_TICKETS = {
    30: ("Cart v1", None, []),
    31: ("Cart line items and quantities", 30, []),
    32: ("Keep a cart across devices", 30, []),
    33: ("Cart page empty and loading states", 30, []),
    34: ("Format prices for the shopper's locale", 30, []),
}
BODIES = {
    40: "Ship the new checkout: totals, coupons, search, payments, emails and the page itself.",
    30: "A cart shoppers can trust: items, quantities, prices and the same cart on every device.",
}

AGENTS = {
    # id: (name, provider, model, effort, parent, ticket)
    "a0000001": ("lead", "claude", "fable", "xhigh", None, None),
    "a0000041": ("shop-41", "claude", "opus[1m]", "high", "a0000001", 41),
    "a0000042": ("shop-42", "codex", "gpt-5.5", "high", "a0000001", 42),
    # The local-model seat: the Codex harness driving a local Qwen model.
    "a0000043": ("shop-43", "codex", "qwen3-coder-next", "high", "a0000001", 43),
    "a0000044": ("shop-44", "claude", "opus[1m]", "high", "a0000001", 44),
    "a0000045": ("shop-45", "codex", "gpt-5.5", "high", "a0000001", 45),
    "a0000046": ("shop-46", "claude", "sonnet", "high", "a0000001", 46),
}
PAST_AGENTS = {
    # id: (name, provider, model, effort, parent, ticket, PR, days ago it retired, last turn)
    "a0000030": ("planner", "claude", "fable", "xhigh", None, None, None, 4.2,
                 "Cart v1 is done: all four tickets merged. Closing the goal."),
    "a0000031": ("cart-31", "claude", "opus[1m]", "high", "a0000030", 31, 35, 6.1,
                 "PR #35 merged. Line items now carry quantity limits and a stock check."),
    "a0000032": ("cart-32", "codex", "gpt-5.5", "high", "a0000030", 32, 36, 5.3,
                 "Merged #36. Carts sync through the account; guest carts merge on sign-in."),
    "a0000033": ("cart-33", "claude", "sonnet", "high", "a0000030", 33, 37, 4.9,
                 "Empty and loading states are in (#37). Screenshots are on the PR."),
    "a0000034": ("cart-34", "codex", "qwen3-coder-next", "high", "a0000030", 34, 38, 4.4,
                 "Prices format by locale (#38): currency symbol, separators and rounding."),
    "a0000029": ("scout", "claude", "sonnet", "high", None, None, None, 2.0,
                 "Compared three payment providers; my notes are in the Checkout v2 plan."),
}
AGENTS.update({agent_id: spec[:6] for agent_id, spec in PAST_AGENTS.items()})
BY_NAME = {spec[0]: agent_id for agent_id, spec in AGENTS.items()}


class World:
    def __init__(self, args):
        self.args = args
        self.base = f"http://127.0.0.1:{args.port}"
        self.state_file = f"{ROOT}/state/sessions.json"
        self.mq = f"{ROOT}/state/message_queue.db"
        self.qdb = f"{ROOT}/q/queue_runner.db"
        self.tool_db = f"{ROOT}/state/tool_usage.db"
        self.world_file = f"{ROOT}/gh-world.json"
        self.live = {}  # agent id -> {"working": bool, "since": iso}
        self.gh = {"repos": {REPO: {"issues": {}, "prs": {}}}}
        for tickets, goal in ((TICKETS, "Checkout v2"), (PAST_TICKETS, "Cart v1")):
            for number, (title, parent, blocked_by) in tickets.items():
                self.gh["repos"][REPO]["issues"][str(number)] = {
                    "title": title, "state": "open", "parent": parent, "blocked_by": blocked_by,
                    "sub_issues": [n for n, t in tickets.items() if t[1] == number],
                    "body": BODIES.get(number) or f"Part of the {goal} goal (#{parent}).",
                    "author": "alex-demo", "created_at": iso_s(ago(9 * 86400)),
                    "updated_at": iso_s(ago(3600)),
                }
        self.jobs = {}  # label -> job id
        self.messages = {}  # key -> owner message id
        self.reviews = {}  # key -> registration id
        self.doc_id = "d0c5e7a1"
        self.server = None

    # ---- plumbing ---------------------------------------------------------

    def save_gh(self):
        tmp = self.world_file + ".tmp"
        with open(tmp, "w") as out:
            json.dump(self.gh, out, indent=1)
        os.replace(tmp, self.world_file)

    def db(self, path=None):
        conn = sqlite3.connect(path or self.mq, timeout=30)
        conn.row_factory = sqlite3.Row
        return conn

    def sql(self, statement, params=(), path=None):
        with self.db(path) as conn:
            conn.execute(statement, params)

    def sm(self, agent_id, *argv, stdin=None, check=True):
        env = dict(os.environ, SM_API_URL=self.base, HOME=f"{ROOT}/home",
                   PATH=f"{FAKE_BIN}:{os.environ['PATH']}", SMDEMO_WORLD=self.world_file)
        env.pop("SESSION_MANAGER_ID", None)
        env.pop("CLAUDE_SESSION_MANAGER_ID", None)
        env.pop("SM_SESSION_CREDENTIAL", None)
        if agent_id:
            env["CLAUDE_SESSION_MANAGER_ID"] = agent_id
            env["SM_SESSION_CREDENTIAL"] = f"demo-credential-{agent_id}"
        cwd = self.worktree(agent_id) if agent_id else SHOP
        result = subprocess.run([self.args.sm, *argv], input=stdin, capture_output=True, text=True,
                                env=env, cwd=cwd, timeout=60)
        self.log(f"sm {' '.join(argv)} [{agent_id}] -> {result.returncode}: "
                 f"{(result.stdout + result.stderr).strip()[:300]}")
        if check and result.returncode != 0:
            raise RuntimeError(f"sm {' '.join(argv)} failed: {result.stderr.strip()}")
        return result.stdout

    def http(self, method, path, body=None):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method,
                                         headers={"Content-Type": "application/json",
                                                  "Accept": "application/json"})
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                text = response.read().decode()
        except urllib.error.HTTPError as error:
            text = error.read().decode()
            self.log(f"{method} {path} -> {error.code}: {text[:300]}")
            raise
        self.log(f"{method} {path} -> ok")
        return json.loads(text) if text else None

    def log(self, line):
        with open(f"{ROOT}/record.log", "a") as out:
            out.write(f"{iso_s()} {line}\n")

    def worktree(self, agent_id):
        name = AGENTS[agent_id][0]
        return f"{HOME}/worktrees/{name}" if "-" in name else SHOP

    # ---- sessions -----------------------------------------------------------

    def load_sessions(self):
        with open(self.state_file) as src:
            return json.load(src)

    def write_sessions(self, state):
        tmp = self.state_file + ".tmp"
        with open(tmp, "w") as out:
            json.dump(state, out, indent=1)
        os.replace(tmp, self.state_file)

    def patch_session(self, agent_id, **fields):
        state = self.load_sessions()
        for record in state["sessions"]:
            if record["id"] == agent_id:
                record.update(fields)
        self.write_sessions(state)

    def spawn(self, agent_id, minutes_ago=0):
        name, provider, model, effort, parent, ticket = AGENTS[agent_id]
        started = ago(minutes_ago * 60)
        os.makedirs(self.worktree(agent_id), exist_ok=True)
        state = self.load_sessions()
        state["sessions"].append({
            "id": agent_id, "name": name, "friendly_name": name, "friendly_name_is_explicit": True,
            "provider": provider, "model": model, "reasoning_effort": effort, "status": "running",
            "parent_session_id": parent, "working_dir": self.worktree(agent_id),
            # The scratch server must never probe a real pane. Claude agents get
            # no tmux session (their state comes from the hook times below); the
            # server drops a Codex session without one, so Codex agents name a
            # session on a socket no tmux server listens on.
            "tmux_session": f"demo-{agent_id}" if provider == "codex" else "",
            "tmux_socket_name": "demo-none", "node": "primary", "started_by_sm": True,
            "created_at": iso(started), "spawned_at": iso(started), "last_activity": iso(),
            "activity_turn_start_hook_at": iso(), "activity_hook_at": iso(),
            "session_credential_sha256": hashlib.sha256(
                f"demo-credential-{agent_id}".encode()).hexdigest(),
            "context_used_percentage": 4.0, "context_window_tokens": 1000000 if provider == "claude" else 400000,
            "tokens_used": 12000, "turns_completed": 0,
        })
        self.write_sessions(state)
        self.live[agent_id] = {"working": True, "since": iso(), "context": 4.0}
        # The server can briefly answer from its cached parse of the old file;
        # the agent's first command must find its own session.
        for _ in range(50):
            if fetch(self.base, f"/sessions/{agent_id}")[0] == 200:
                return
            time.sleep(0.1)
        raise RuntimeError(f"server never saw session {agent_id}")

    def work(self, agent_id, working=True):
        self.live[agent_id].update(working=working, since=iso())

    def heartbeat(self):
        """Keeps each live agent's activity fields current, as hooks would."""
        state = self.load_sessions()
        for record in state["sessions"]:
            live = self.live.get(record["id"])
            if not live:
                continue
            if live["working"]:
                live["context"] = min(live["context"] + 0.6, 31.0)
                record.update(status="running", activity_turn_start_hook_at=live["since"],
                              activity_hook_at=iso(ago(1)), last_activity=iso(ago(1)))
            else:
                record.update(status="idle", activity_turn_start_hook_at=live["since"],
                              activity_hook_at=live["since"], last_activity=live["since"])
            record["context_used_percentage"] = round(live["context"], 1)
            record["context_total_input_tokens"] = int(live["context"] * record["context_window_tokens"] / 100)
            record["tokens_used"] = record["context_total_input_tokens"]
        self.write_sessions(state)

    def retire(self, agent_id):
        self.live.pop(agent_id, None)
        self.patch_session(agent_id, status="stopped", stopped_at=iso(), completed_at=iso(),
                           completion_status="completed", completion_message="Retired by lead")
        self.sql("UPDATE work_claims SET ended_at = ?, end_reason = 'retired' "
                 "WHERE session_id = ? AND ended_at IS NULL", (iso_s(), agent_id))

    def turn(self, agent_id, text, finished=False):
        """The agent's last-turn message; a finished turn also lands in the inbox."""
        provider = AGENTS[agent_id][1]
        self.sql("INSERT INTO turn_messages (session_id, provider, at, text) VALUES (?, ?, ?, ?) "
                 "ON CONFLICT(session_id) DO UPDATE SET provider = excluded.provider, at = excluded.at, "
                 "text = excluded.text", (agent_id, provider, iso(), text))
        if finished:
            self.sql("INSERT OR REPLACE INTO finished (session_id, completed_at, text, text_at) "
                     "VALUES (?, ?, ?, ?)", (agent_id, iso(), text, iso()))
            self.work(agent_id, False)

    def tools(self, agent_id, *names):
        name = AGENTS[agent_id][0]
        with self.db(self.tool_db) as conn:
            for offset, tool in enumerate(reversed(names)):
                conn.execute(
                    "INSERT INTO tool_usage (timestamp, session_id, session_name, hook_type, tool_name, cwd) "
                    "VALUES (?, ?, ?, 'PreToolUse', ?, ?)",
                    (ago(offset * 2).strftime("%Y-%m-%d %H:%M:%S"), agent_id, name, tool,
                     self.worktree(agent_id)))

    def status(self, agent_id, text):
        self.sm(agent_id, "status", text)

    # ---- GitHub-side changes ------------------------------------------------

    def open_pr(self, agent_id, pr, ticket, title):
        self.gh["repos"][REPO]["prs"][str(pr)] = {
            "title": title, "state": "open", "head_ref": f"{ticket}-{AGENTS[agent_id][0]}",
            "head_sha": sha(f"pr{pr}-1"), "closes": [ticket]}
        self.save_gh()
        self.sql("INSERT OR REPLACE INTO board_prs (repo, issue_number, pr_repo, pr_number, pr_state, url) "
                 "VALUES (?, ?, ?, ?, 'OPEN', ?)", (REPO, ticket, REPO, pr, f"https://github.com/{REPO}/pull/{pr}"))
        self.sm(agent_id, "pr", str(pr), "--repo", REPO, "--ticket", str(ticket))

    def push_fix(self, pr):
        self.gh["repos"][REPO]["prs"][str(pr)]["head_sha"] = sha(f"pr{pr}-2")
        self.save_gh()
        self.sql("UPDATE work_items SET head_sha = ? WHERE repo = ? AND number = ?", (sha(f"pr{pr}-2"), REPO, pr))

    def merge(self, pr, ticket):
        stamp = iso_s()
        self.gh["repos"][REPO]["prs"][str(pr)].update(state="merged", merged_at=stamp, closed_at=stamp)
        self.close(ticket)
        self.sql("UPDATE board_prs SET pr_state = 'MERGED' WHERE pr_number = ?", (pr,))
        self.sql("UPDATE work_items SET state = 'merged', merged_at = ?, closed_at = ? "
                 "WHERE repo = ? AND number = ?", (stamp, stamp, REPO, pr))

    def close(self, ticket):
        stamp = iso_s()
        issue = self.gh["repos"][REPO]["issues"][str(ticket)]
        issue.update(state="closed", state_reason="COMPLETED", closed_at=stamp, updated_at=stamp)
        self.save_gh()
        self.sql("UPDATE board_items SET state = 'closed', state_reason = 'COMPLETED', closed_at = ?, "
                 "updated_at = ? WHERE repo = ? AND number = ?", (stamp, stamp, REPO, ticket))
        self.sql("UPDATE work_items SET state = 'closed', state_reason = 'COMPLETED', closed_at = ? "
                 "WHERE repo = ? AND number = ?", (stamp, REPO, ticket))
        self.board_event("closed", ticket)

    def board_event(self, kind, ticket, actor="github", actor_name="GitHub"):
        self.sql("INSERT INTO board_events (ts, kind, lane_id, repo, number, actor, actor_name) "
                 "VALUES (?, ?, (SELECT id FROM board_lanes WHERE ended_at IS NULL LIMIT 1), ?, ?, ?, ?)",
                 (iso_s(), kind, REPO, ticket, actor, actor_name))

    # ---- queue ----------------------------------------------------------------

    def queue(self, agent_id, label, job_type="tests", seconds=40):
        """Submits through `sm queue run`; the runtime is off, so it stays pending."""
        self.sm(agent_id, "queue", "run", "--type", job_type, "--label", label,
                "--cwd", self.worktree(agent_id), "--", "cargo", "test", "--workspace")
        with self.db(self.qdb) as conn:
            job_id = conn.execute("SELECT id FROM queue_jobs WHERE label = ? ORDER BY queued_at DESC",
                                  (label,)).fetchone()[0]
        self.jobs[label] = job_id
        self.sql("UPDATE queue_jobs SET holding_reason = 'concurrency_cap' WHERE id = ?", (job_id,), self.qdb)
        return job_id

    def start_job(self, label, lines):
        job_id = self.jobs[label]
        log_path = f"{ROOT}/q/logs/{job_id}.log"
        os.makedirs(os.path.dirname(log_path), exist_ok=True)
        with open(log_path, "w") as out:
            out.write("\n".join(lines) + "\n")
        self.sql("UPDATE queue_jobs SET state = 'running', holding_reason = NULL, started_at = ?, "
                 "started_notified_at = ?, pid = ?, log_path = ? WHERE id = ?",
                 (iso(), iso(), 40000 + len(self.jobs), log_path, job_id), self.qdb)

    def log_job(self, label, lines):
        with open(f"{ROOT}/q/logs/{self.jobs[label]}.log", "a") as out:
            out.write("\n".join(lines) + "\n")

    def finish_job(self, label, lines, exit_code=0):
        self.log_job(label, lines)
        self.sql("UPDATE queue_jobs SET state = ?, finished_at = ?, exit_code = ?, "
                 "completion_notified_at = ? WHERE id = ?",
                 ("succeeded" if exit_code == 0 else "failed", iso(), exit_code, iso(), self.jobs[label]),
                 self.qdb)

    # ---- reviews ------------------------------------------------------------

    def request_review(self, key, agent_id, pr, round_number, head):
        """A Codex review of the PR, run as a review job in the queue."""
        request_id = f"rr_{sha(key)[:12]}"
        self.reviews[key] = request_id
        label = f"codex review {REPO}#{pr} r{round_number}"
        job_id = f"job_{sha(label)[:12]}"
        self.jobs[label] = job_id
        with self.db(self.qdb) as conn:
            conn.execute(
                "INSERT INTO queue_jobs (id, type, label, requester_session_id, notify_session_id, cwd, "
                "argv_json, env_json, timeout_seconds, state, queued_at, owner) "
                "VALUES (?, 'review', ?, ?, ?, ?, ?, '{}', 1800, 'pending', ?, ?)",
                (job_id, label, agent_id, agent_id, self.worktree(agent_id),
                 json.dumps(["codex", "review", "--pr", str(pr)]), iso(), agent_id))
        self.sql(
            "INSERT INTO codex_review_request_registrations (id, repo, pr_number, requester_session_id, "
            "notify_session_id, requested_head_sha, requested_at, attempt_count, poll_interval_seconds, "
            "retry_interval_seconds, state, is_active, step_state, step_started_at, reviewer_label, "
            "run_job_id, round, chain_json, policy_source) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, 1, 30, 60, 'active', 1, 'running', ?, 'Codex', ?, ?, ?, 'repo')",
            (request_id, REPO, pr, agent_id, agent_id, head, iso(), iso(), job_id, round_number,
             json.dumps([{"kind": "codex_run", "model": "gpt-5.5", "effort": "high"}])))
        self.start_job(label, [f"codex review {REPO}#{pr} at {head[:7]}", "reading diff (14 files)..."])

    def land_review(self, key, pr, findings):
        request_id = self.reviews[key]
        url = f"https://github.com/{REPO}/pull/{pr}#pullrequestreview-{int(sha(key)[:6], 16)}"
        self.sql(
            "UPDATE codex_review_request_registrations SET state = 'completed', is_active = 0, "
            "step_state = 'posted', review_landed_at = ?, review_source = 'codex_run', review_url = ?, "
            "findings_json = ? WHERE id = ?", (iso(), url, json.dumps(findings), request_id))
        label = next(label for label, job in self.jobs.items()
                     if job == self.db().execute("SELECT run_job_id FROM codex_review_request_registrations "
                                                 "WHERE id = ?", (request_id,)).fetchone()[0])
        summary = f"{len(findings)} finding(s)" if findings else "no findings"
        self.finish_job(label, [f"review posted: {summary}", url])

    # ---- owner messages -------------------------------------------------------

    def ask(self, key, agent_id, title, body):
        self.sm(agent_id, "send", HUMAN, "--blocking", "--title", title, stdin=body)
        row = self.db().execute("SELECT id FROM owner_messages WHERE sender_session_id = ? "
                                "ORDER BY created_at DESC", (agent_id,)).fetchone()
        self.messages[key] = row[0]
        self.work(agent_id, False)

    def answer(self, key, agent_id, text):
        message_id = self.messages[key]
        self.sql("UPDATE owner_messages SET first_viewed_at = COALESCE(first_viewed_at, ?), handled_at = ?, "
                 "handled_via = 'reply' WHERE id = ?", (iso_s(), iso_s(), message_id))
        self.sql("INSERT INTO owner_message_replies (id, message_id, body, comments_json, delivered_text, "
                 "delivered_to_session_id, created_at) VALUES (?, ?, ?, '[]', ?, ?, ?)",
                 (f"rep_{sha(key)[:8]}", message_id, text, f"{OWNER} replied: {text}", agent_id, iso_s()))
        self.work(agent_id, True)

    # ---- docs -------------------------------------------------------------------

    def publish_doc(self, revision, review_requested=True):
        path = "docs/checkout_v2_plan.html"
        with open(os.path.join(HERE, "doc", f"checkout_v2_plan.r{revision}.html"), "rb") as src:
            body = src.read()
        blob = hashlib.sha1(b"blob %d\0" % len(body) + body).hexdigest()
        commit = sha(f"doc-commit-{revision}")
        cache = f"{ROOT}/state/doc_cache/acme/shop/{commit}/{path}"
        os.makedirs(os.path.dirname(cache), exist_ok=True)
        with open(cache, "wb") as out:
            out.write(body)
        stamp = iso_s()
        self.sql("INSERT OR IGNORE INTO owner_docs (id, repo, path, pr_number, author_session_id, "
                 "author_session_name, title, created_at, updated_at) VALUES (?, ?, ?, NULL, ?, 'lead', ?, ?, ?)",
                 (self.doc_id, REPO, path, "a0000001", "Checkout v2 plan", stamp, stamp))
        self.sql("UPDATE owner_docs SET updated_at = ? WHERE id = ?", (stamp, self.doc_id))
        self.sql("INSERT INTO owner_doc_publishes (doc_id, commit_sha, blob_sha, session_id, review_requested, "
                 "published_at) VALUES (?, ?, ?, 'a0000001', ?, ?)",
                 (self.doc_id, commit, blob, int(review_requested), stamp))
        self.blob = (commit, blob)

    def review_doc(self, revision, verdict, body, line_comments):
        commit, blob = self.blob
        self.sql("INSERT INTO owner_doc_reviews (id, status, doc_id, commit_sha, blob_sha, verdict, body, "
                 "line_comment_count, file_comment_count, submitted_at, delivered_to_session_id, posted_at) "
                 "VALUES (?, 'posted', ?, ?, ?, ?, ?, ?, 0, ?, 'a0000001', ?)",
                 (f"rv_{revision}{sha(body)[:6]}", self.doc_id, commit, blob, verdict, body, line_comments,
                  iso_s(), iso_s()))
        self.sql("INSERT OR IGNORE INTO owner_doc_views (doc_id, blob_sha, viewed_at) VALUES (?, ?, ?)",
                 (self.doc_id, blob, iso_s()))


# ---- the storyline ------------------------------------------------------------
# (seconds into the sprint, chapter caption or None, action)

def prologue(w):
    """Last week's Cart v1 sprint, played through the real CLI and then moved
    into the past, so History has retired agents and finished tickets."""
    def backdate(agent_id, days, pr, ticket):
        end, start = ago(days * 86400), ago(days * 86400 + 95 * 60)
        w.patch_session(agent_id, created_at=iso(start), spawned_at=iso(start), stopped_at=iso(end),
                        completed_at=iso(end), last_activity=iso(end), activity_hook_at=iso(end),
                        activity_turn_start_hook_at=iso(end))
        w.sql("UPDATE work_claims SET claimed_at = ?, ended_at = ? WHERE session_id = ?",
              (iso_s(start), iso_s(end), agent_id))
        w.sql("UPDATE turn_messages SET at = ? WHERE session_id = ?", (iso(end), agent_id))
        if pr:
            w.sql("UPDATE work_items SET merged_at = ?, closed_at = ? WHERE repo = ? AND number = ?",
                  (iso_s(end), iso_s(end), REPO, pr))
            w.gh["repos"][REPO]["prs"][str(pr)].update(merged_at=iso_s(end), closed_at=iso_s(end))
            w.gh["repos"][REPO]["issues"][str(ticket)].update(closed_at=iso_s(end), updated_at=iso_s(end))
            w.save_gh()

    for agent_id, spec in sorted(PAST_AGENTS.items(), key=lambda kv: -kv[1][7]):
        name, _, _, _, _, ticket, pr, days, last = spec
        w.spawn(agent_id, minutes_ago=int(days * 1440) + 95)
        if ticket:
            w.sm(agent_id, "ticket", str(ticket), "--repo", REPO)
        if pr:
            w.open_pr(agent_id, pr, ticket, PAST_TICKETS[ticket][0])
            w.merge(pr, ticket)
        w.turn(agent_id, last)
        w.retire(agent_id)
        backdate(agent_id, days, pr, ticket)
    w.close(30)
    w.gh["repos"][REPO]["issues"]["30"].update(closed_at=iso_s(ago(4.2 * 86400)), updated_at=iso_s(ago(4.2 * 86400)))
    w.save_gh()


def storyline(w):
    lead, a41, a42, a43, a44, a45, a46 = (BY_NAME[n] for n in
                                          ("lead", "shop-41", "shop-42", "shop-43", "shop-44", "shop-45", "shop-46"))
    head51 = sha("pr51-1")

    def start_agent(agent_id, status_text, *tools):
        w.spawn(agent_id)
        w.sm(agent_id, "ticket", str(AGENTS[agent_id][5]), "--repo", REPO)
        w.status(agent_id, status_text)
        w.tools(agent_id, *tools)

    def auto_start(agent_id, status_text):
        ticket = AGENTS[agent_id][5]
        w.sql("UPDATE auto_starts SET state = 'started', session_id = ?, attempts = 1, updated_at = ? "
              "WHERE repo = ? AND number = ?", (agent_id, iso_s(), REPO, ticket))
        w.board_event("auto_started", ticket, "owner", OWNER)
        start_agent(agent_id, status_text, "Read", "Bash", "Grep")

    def finish(agent_id, guestbook=None):
        if guestbook:
            w.sm(agent_id, "task-complete", "--sign-guestbook", "-", stdin=guestbook)
        else:
            w.sm(agent_id, "task-complete")

    return [
        (0, "A goal with six tickets lands on the Board", lambda: (
            w.turn(lead, "I split **Checkout v2** into six tickets. #44 waits on #41, #45 on #42, "
                         "and #46 on both. Adding the lane now."),
            w.status(lead, "Planning Checkout v2"),
            w.tools(lead, "Read", "Bash", "Write"))),
        (5, None, lambda: w.http("POST", "/board/lanes", {"repo": REPO, "number": 40, "session_id": lead})),
        (15, "The lead publishes the plan for review", lambda: (
            w.publish_doc(1),
            w.turn(lead, "Published the Checkout v2 plan for review. Starting the three independent tickets "
                         "while you read it."))),
        # The owner arms these in the web UI; that route needs a signed-in owner,
        # so the rows it would write are written directly.
        (20, "Dependent tickets are armed with Start when ready", lambda: [
            w.sql("INSERT INTO auto_starts (repo, number, agent_type, provider, model, effort, state, "
                  "authorized_at, updated_at) VALUES (?, ?, ?, ?, ?, 'high', 'waiting', ?, ?)",
                  (REPO, n, kind, p, m, iso_s(), iso_s()))
            for n, kind, p, m in ((44, "Mid", "claude", "opus[1m]"), (45, "Codex", "codex", "gpt-5.5"),
                                  (46, None, "claude", "sonnet"))]),
        (25, "Three tickets start in parallel: Claude, Codex and a local model", lambda: (
            start_agent(a41, "Reading the pricing module", "Read", "Grep", "Read"),
            start_agent(a42, "Mapping existing coupon rules", "Bash", "Read"),
            start_agent(a43, "Indexing the product catalog", "Bash", "Read", "Bash"),
            w.status(lead, "Watching three tickets"),
            w.turn(lead, "Spawned shop-41 (Claude), shop-42 (Codex) and shop-43 (local Qwen).", finished=True))),
        (40, None, lambda: (w.tools(a41, "Edit", "Edit", "Bash"), w.status(a41, "Writing cart total tests"),
                            w.tools(a43, "Write", "Edit"))),
        (55, None, lambda: (w.tools(a42, "Edit", "Bash"), w.status(a42, "Drafting the rules engine"))),
        (60, "A test job holds the tests slot while another waits", lambda: (
            w.queue(a41, "shop-41 cart tests"),
            w.start_job("shop-41 cart tests", ["   Compiling shop v0.4.0", "   Running tests/cart_totals.rs"]),
            w.status(a41, "Running cart tests"),
            w.work(a41, False))),
        (75, None, lambda: (
            w.queue(a43, "shop-43 search tests"),
            w.status(a43, "Waiting for the tests slot"),
            w.work(a43, False))),
        (90, "shop-42 asks a question: it shows as Needs you and in the Inbox", lambda: (
            w.ask("coupons", a42, "Should coupons stack with sale prices?",
                  "The old checkout applies a coupon **after** sale prices, so a 20% coupon on a 30% sale "
                  "item gives 44% off. The new rules engine can do either.\n\n"
                  "- **Stack** (today's behaviour): simpler to explain, costs more margin.\n"
                  "- **Best of**: the customer gets the larger discount only.\n\n"
                  "I recommend **best of**, because the margin report shows stacked discounts on 6% of "
                  "orders. Confirm?"),
            w.status(a42, "Waiting on Alex: coupon stacking"),
            w.board_event("became_needs_you", 42, "sm:" + a42, "shop-42"))),
        (100, None, lambda: w.log_job("shop-41 cart tests", ["test cart::totals_with_tax ... ok",
                                                             "test cart::discount_rounding ... ok"])),
        (120, None, lambda: (
            w.finish_job("shop-41 cart tests", ["test result: ok. 48 passed; 0 failed"]),
            w.start_job("shop-43 search tests", ["   Compiling search v0.2.1", "   Running tests/index.rs"]),
            w.work(a41, True), w.work(a43, False),
            w.status(a41, "Opening the PR"), w.status(a43, "Running search tests"),
            w.turn(a41, "All 48 cart tests pass. Opening the PR."))),
        (140, "Codex reviews shop-41's pull request", lambda: (
            w.open_pr(a41, 51, 41, "Cart totals service with tax and discounts"),
            w.request_review("pr51-r1", a41, 51, 1, head51),
            w.status(a41, "Waiting on Codex review of #51"),
            w.work(a41, False))),
        (160, None, lambda: w.log_job("codex review acme/shop#51 r1", ["checking tax rounding paths..."])),
        (185, "Alex answers; shop-42 carries on", lambda: (
            w.answer("coupons", a42, "Best of. Keep stacking behind a flag for the spring sale."),
            w.status(a42, "Implementing best-of discounts"),
            w.tools(a42, "Edit", "Edit", "Bash"))),
        (190, None, lambda: (
            w.finish_job("shop-43 search tests", ["test result: ok. 31 passed; 0 failed"]),
            w.work(a43, True),
            w.open_pr(a43, 53, 43, "Product search index"),
            w.status(a43, "PR #53 up; lead reviewing"))),
        (200, "The review comes back with one finding", lambda: (
            w.land_review("pr51-r1", 51, [{
                "severity": "P1", "path": "src/cart/totals.rs", "line": 88,
                "title": "Discount applied after tax",
                "body": "Percentage discounts are applied to the taxed total, so tax is charged on the "
                        "discounted-away amount. Apply discounts before tax."}]),
            w.turn(a41, "Codex found one P1: discounts were applied after tax. Fixing it now."),
            w.status(a41, "Fixing review finding: discount before tax"),
            w.work(a41, True),
            w.tools(a41, "Read", "Edit", "Bash"))),
        (230, None, lambda: (
            w.push_fix(51),
            w.request_review("pr51-r2", a41, 51, 2, sha("pr51-2")),
            w.status(a41, "Fix pushed; Codex re-reviewing #51"),
            w.work(a41, False))),
        (240, "Alex reviews the plan and asks for changes", lambda: (
            w.review_doc(1, "changes_requested",
                         "Good split. Two changes: say what happens to carts mid-checkout at cutover, and move "
                         "search (#43) out of the critical path.", 2),
            w.status(lead, "Revising the plan"),
            w.work(lead, True))),
        (255, None, lambda: (w.merge(53, 43), w.turn(a43, "#53 merged; search index ships.", finished=True))),
        (260, None, lambda: (
            w.land_review("pr51-r2", 51, []),
            w.turn(a41, "Codex re-review is clean. Merging #51."))),
        (270, "The fix lands; shop-41 signs the guestbook", lambda: (
            w.merge(51, 41),
            finish(a41, "Clear ticket, and the Codex review caught a real tax bug before it shipped. "
                        "The queue kept my tests off shop-43's toes."),
            w.turn(a41, "#41 is done: cart totals with tax and discounts merged in #51.", finished=True))),
        (275, None, lambda: (
            w.publish_doc(2),
            w.turn(lead, "Revised the plan: added the cutover section and took #43 off the critical path."),
            w.work(lead, False))),
        (280, "#44 starts by itself now that #41 is done", lambda: (
            w.retire(a43),
            auto_start(a44, "Reading the payment provider docs"))),
        (290, None, lambda: w.retire(a41)),
        (300, None, lambda: (
            w.open_pr(a42, 52, 42, "Coupon rules engine with best-of discounts"),
            w.status(a42, "PR #52 up"))),
        (330, None, lambda: (w.tools(a44, "Edit", "Bash", "Edit"), w.status(a44, "Wiring the payment adapter"))),
        (340, None, lambda: (
            w.merge(52, 42),
            finish(a42),
            w.turn(a42, "#42 merged in #52: best-of discounts, stacking behind a flag.", finished=True))),
        (345, "#45 starts after #42", lambda: auto_start(a45, "Drafting confirmation email templates")),
        (355, None, lambda: w.retire(a42)),
        (380, "Alex approves the revised plan", lambda: (
            w.review_doc(2, "approve", "Approved. Ship it.", 0),
            w.status(lead, "Coordinating #44 and #45"),
            w.turn(lead, "Plan approved. #44 and #45 are underway; #46 starts when both land.", finished=True))),
        (400, None, lambda: (w.tools(a45, "Write", "Edit", "Bash"), w.status(a45, "Rendering email previews"))),
        (420, None, lambda: (w.open_pr(a44, 54, 44, "Payment provider adapter"), w.status(a44, "PR #54 up"))),
        (440, None, lambda: (
            w.merge(54, 44), finish(a44),
            w.turn(a44, "#44 merged in #54.", finished=True))),
        (450, None, lambda: w.retire(a44)),
        (470, None, lambda: (w.open_pr(a45, 55, 45, "Order confirmation emails"), w.status(a45, "PR #55 up"))),
        (480, None, lambda: (
            w.merge(55, 45), finish(a45),
            w.turn(a45, "#45 merged in #55.", finished=True))),
        (485, "Both prerequisites done: #46 starts", lambda: (
            w.retire(a45),
            auto_start(a46, "Building the checkout page"),
            w.status(lead, "Watching #46, the last ticket"))),
        (515, None, lambda: (
            w.queue(a46, "shop-46 checkout e2e"),
            w.start_job("shop-46 checkout e2e", ["   Compiling shop-web v0.9.0", "   Running e2e/checkout.rs"]),
            w.status(a46, "Running checkout end-to-end tests"),
            w.work(a46, False))),
        (545, None, lambda: (
            w.finish_job("shop-46 checkout e2e", ["test result: ok. 12 passed; 0 failed"]),
            w.work(a46, True),
            w.open_pr(a46, 56, 46, "Checkout page"),
            w.status(a46, "PR #56 up"))),
        (560, "The last ticket merges", lambda: (
            w.merge(56, 46), finish(a46),
            w.turn(a46, "#46 merged in #56.", finished=True),
            w.status(lead, "Checkout v2 shipped"),
            w.turn(lead, "**Checkout v2 is done.** Six tickets, five agents, one review finding fixed "
                         "before merge.", finished=True))),
        (575, None, lambda: (
            w.retire(a46),
            w.sm(lead, "task-complete", "--sign-guestbook", "-",
                 stdin="Six tickets in ten minutes. Start when ready did the sequencing for me."))),
        # Last, because a lane leaves the Board once its goal closes.
        (590, "The goal completes", lambda: w.close(40)),
    ]


# ---- capture ------------------------------------------------------------------

GLOBAL = [
    "/watch/state", "/watch/state?stopped=1",
    "/client/board", "/client/board?clock_hours=3", "/client/board?clock_hours=6", "/client/board?clock_hours=24",
    "/client/board/badge",
    "/client/queue", "/client/queue?ended_hours=24",
    "/client/queue/stats?hours=24", "/client/queue/stats?hours=168", "/client/queue/stats?hours=720",
    "/client/utilization/series?hours=1", "/client/utilization/series?hours=24",
    "/client/utilization/series?hours=168", "/client/utilization/series?hours=720",
    "/client/host-status", "/client/usage/meters", "/client/follows",
    "/inbox?format=json", "/inbox?format=json&filter=open", "/inbox?format=json&filter=docs",
    "/inbox?format=json&filter=done",
    "/guestbook?format=json&repo=&before=",
    "/history?format=json&repo=&before=", "/history/agents?format=json&q=&before=",
    "/docs", "/client/sessions", "/client/settings",
    "/client/session-models?provider=claude", "/client/session-models?provider=codex",
    "/client/session-models?provider=codex-fork",
    "/handoff-defaults", "/review-policies",
]
SYNTHETIC = ("/client/host-status", "/client/usage/meters", "/client/utilization/series",
             "/client/queue/stats")
MEMORY_TOTAL = 128 * 1024 ** 3


def synthetic(url, t, w):
    """Responses a runtime-off server can't give, or would give from the real host."""
    def jobs(state):
        try:
            return w.db(w.qdb).execute("SELECT COUNT(*) FROM queue_jobs WHERE state = ?", (state,)).fetchone()[0]
        except sqlite3.OperationalError:  # no job submitted yet, so no table
            return 0
    running, pending = jobs("running"), jobs("pending")
    cpu = round(18 + 22 * running + 6 * ((t // 5) % 3), 1)
    host = {"available": True, "cpu_percent": cpu, "gpu_percent": 4.0 if running else 1.0, "host": "demo-mac",
            "memory_available_bytes": MEMORY_TOTAL - int((38 + 9 * running) * 1024 ** 3),
            "memory_pressure": "Normal", "memory_total_bytes": MEMORY_TOTAL,
            "memory_used_bytes": int((38 + 9 * running) * 1024 ** 3), "sampled_at": iso(), "source": "live"}
    if url == "/client/host-status":
        return host
    if url == "/client/usage/meters":
        resets = lambda hours: iso_s(now_utc() + datetime.timedelta(hours=hours))
        return {"meters": [
            {"account_key": "claude:demo", "label": "demo team (Claude)", "observed_at": iso_s(),
             "pace": {"kind": "on_pace", "percent": 41.0}, "percent": round(22 + t / 40, 1),
             "provider": "claude", "resets_at": resets(3), "scope": None, "window": "five_hour"},
            {"account_key": "claude:demo", "label": "demo team (Claude)", "observed_at": iso_s(),
             "pace": {"kind": "on_pace", "percent": 63.0}, "percent": round(48 + t / 120, 1),
             "provider": "claude", "resets_at": resets(70), "scope": None, "window": "week"},
            {"account_key": "codex:demo", "label": "demo team (Codex)", "observed_at": iso_s(),
             "pace": {"kind": "on_pace", "percent": 37.0}, "percent": round(29 + t / 150, 1),
             "provider": "codex", "resets_at": resets(90), "scope": None, "window": "week"}]}
    if url.startswith("/client/utilization/series"):
        hours = int(url.split("hours=")[1])
        # The server's bucket size and count for each range (series_bucket_seconds).
        seconds, count = {1: (15, 240), 24: (300, 288), 168: (1800, 336), 720: (7200, 360)}[hours]
        epoch = int(now_utc().timestamp())
        end = datetime.datetime.fromtimestamp(epoch - epoch % seconds, datetime.timezone.utc)
        buckets = []
        for i in range(count):
            start = end - datetime.timedelta(seconds=seconds * (count - 1 - i))
            hour = start.hour + start.minute / 60
            busy = 1 if 9 <= (hour + 7) % 24 <= 19 else 0  # a working day, in the viewer's past
            wave = (hash((i * 7919) % 104729) % 100) / 100
            buckets.append({
                "start": iso_s(start), "samples": max(1, seconds // 5),
                "cpu_avg": round(12 + busy * (30 + 25 * wave), 1), "cpu_max": round(min(100, 20 + busy * (55 + 40 * wave)), 1),
                "gpu_avg": round(busy * 6 * wave, 1), "gpu_max": round(busy * 15 * wave, 1),
                "local_model_memory_avg": busy * 18 * 1024 ** 3, "local_model_memory_max": busy * 18 * 1024 ** 3,
                "mem_available_min": MEMORY_TOTAL - int((30 + busy * 40 * wave) * 1024 ** 3),
                "mem_used_avg": int((28 + busy * 30 * wave) * 1024 ** 3),
                "mem_used_max": int((30 + busy * 40 * wave) * 1024 ** 3),
                "pending_max": busy * int(3 * wave), "pressure_max": 0,
                "queue_cpu_avg": round(busy * 20 * wave, 1), "queue_gpu_avg": 0.0,
                "queue_memory_avg": int(busy * 6 * wave * 1024 ** 3),
                "running": {"background": round(busy * wave, 2), "perf": 0.0, "service": 0.0,
                            "tests": round(busy * 1.5 * wave, 2)}})
        buckets[-1].update(pending_max=pending, running={"background": 0.0, "perf": 0.0, "service": 0.0,
                                                         "tests": float(running)})
        return {"available": True, "bucket_seconds": seconds, "buckets": buckets, "end": iso_s(end),
                "hours": hours, "memory_total_bytes": MEMORY_TOTAL, "start": buckets[0]["start"],
                "summary": {"covered_seconds": hours * 3600, "cpu_avg": 31.4, "cpu_busy_seconds": 9200,
                            "gpu_avg": 2.1, "headroom_seconds": 71000,
                            "mem_used_max": 70 * 1024 ** 3, "pressure_elevated_seconds": 0,
                            "unknown_seconds": 0}}
    if url.startswith("/client/queue/stats"):
        hours = int(url.split("hours=")[1])
        gib = 1024 ** 3
        return {"available": True, "hours": hours, "window_seconds": hours * 3600, "covered_seconds": hours * 3600,
                "thresholds": {"available_memory_at_least_fraction": 0.25, "cpu_busy_below_pct": 60.0,
                               "pressure": "normal"},
                "by_type": [
                    {"type": "tests", "jobs": 46, "cpu_cores_p95": 7.5, "peak_rss_p50_bytes": 3 * gib,
                     "peak_rss_p95_bytes": 9 * gib, "peak_rss_max_bytes": 14 * gib},
                    {"type": "review", "jobs": 18, "cpu_cores_p95": 1.2, "peak_rss_p50_bytes": gib,
                     "peak_rss_p95_bytes": 2 * gib, "peak_rss_max_bytes": 2 * gib},
                    {"type": "background", "jobs": 7, "cpu_cores_p95": 3.1, "peak_rss_p50_bytes": 4 * gib,
                     "peak_rss_p95_bytes": 11 * gib, "peak_rss_max_bytes": 12 * gib}],
                "waiting": [
                    {"group": "limits", "job_seconds": 2400, "headroom_job_seconds": 900, "unknown_job_seconds": 0},
                    {"group": "perf_rules", "job_seconds": 0, "headroom_job_seconds": 0, "unknown_job_seconds": 0},
                    {"group": "memory", "job_seconds": 0, "headroom_job_seconds": 0, "unknown_job_seconds": 0},
                    {"group": "other", "job_seconds": 120, "headroom_job_seconds": 0, "unknown_job_seconds": 0}]}
    raise KeyError(url)


def fetch(base, url, accept="application/json"):
    request = urllib.request.Request(base + url, headers={"Accept": accept})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            final = response.geturl()[len(base):]
            return response.status, response.headers.get("Content-Type", ""), response.read(), final
    except urllib.error.HTTPError as error:
        return error.code, error.headers.get("Content-Type", ""), error.read(), url


def detail_urls(captured):
    """URLs that depend on what the global responses list: agents, jobs, threads, docs."""
    urls = []
    watch = json.loads(captured.get("/watch/state?stopped=1", b"{}") or b"{}")
    for session in watch.get("sessions", []):
        sid = urllib.parse.quote(session["id"])
        urls += [f"/watch/state?session={sid}", f"/inbox/agent/{sid}?format=json",
                 f"/sessions/{sid}/last-turn", f"/sessions/{sid}/tool-calls?limit=10"]
    queue = json.loads(captured.get("/client/queue?ended_hours=24", b"{}") or b"{}")
    for job in queue.get("running", []) + queue.get("queued", []) + queue.get("ended", []):
        jid = urllib.parse.quote(job["id"])
        urls += [f"/queue-jobs/{jid}", f"/queue-jobs/{jid}/log?lines=40", f"/client/queue/jobs/{jid}/usage"]
    keys = set()
    for name in ("/inbox?format=json&filter=open", "/inbox?format=json&filter=docs", "/inbox?format=json&filter=done"):
        for row in json.loads(captured.get(name, b"{}") or b"{}").get("rows", []):
            if row.get("thread_key"):
                keys.add(row["thread_key"])
    urls += [f"/inbox/thread/{urllib.parse.quote(key, safe='')}?format=json" for key in sorted(keys)]
    for doc in json.loads(captured.get("/docs", b"{}") or b"{}").get("docs", []):
        did = doc["id"]
        urls += [f"/docs/{did}?format=json", f"/docs/{did}/ask-target", f"/docs/{did}/ask-items"]
    # The ticket panel (History rows, Board cards, PR links): GitHub view and sm history.
    numbers = set()
    for row in json.loads(captured.get("/history?format=json&repo=&before=", b"{}") or b"{}").get("rows", []):
        numbers.add(row["number"])
        numbers.update(pr["number"] for pr in row.get("prs", []))
        urls.append(f"/t/{REPO.split('/')[1]}/{row['number']}?format=json")
    board = json.loads(captured.get("/client/board?clock_hours=3", b"{}") or b"{}")
    for lane in board.get("lanes", []):
        numbers.update(ticket["number"] for ticket in lane.get("tickets", []))
        if (lane.get("goal") or {}).get("number"):
            numbers.add(lane["goal"]["number"])
    urls += [f"/client/github/{REPO}/{number}" for number in sorted(numbers)]
    for lane in board.get("lanes", []):
        for ticket in lane.get("tickets", []):
            if ticket.get("state") not in ("done", "closed"):
                urls.append("/client/board/start-options?" + urllib.parse.urlencode(
                    {"repo": ticket["repo"], "number": ticket["number"]}))
    return urls


def doc_pages(base, captured):
    """The reader page for each published revision, as the doc iframe loads it."""
    pages = []
    for doc in json.loads(captured.get("/docs", b"{}") or b"{}").get("docs", []):
        detail = json.loads(captured.get(f"/docs/{doc['id']}?format=json", b"{}") or b"{}")
        for publish in detail.get("publishes", []):
            path = doc["reader_path"].split("?")[0]
            pages.append(f"{path}?version={publish['commit_sha'][:12]}")
        pages.append(doc["reader_path"].split("?")[0])  # the bare path: newest revision
    return pages


def file_name(url):
    name = re.sub(r"[^A-Za-z0-9._-]+", "_", url.strip("/")) or "root"
    return name[:150] + "-" + hashlib.sha1(url.encode()).hexdigest()[:8]


def scrub(data):
    for prefix in ("/private" + ROOT, ROOT):
        data = data.replace(prefix.encode(), b"")
    return data


def denylist():
    words = {getpass.getuser(), socket.gethostname().split(".")[0], "rajesh", "goli", "fractal", "/Users/",
             "gmail", "smdemo", "studio.local"}
    return [word.lower() for word in words if len(word) >= 4]


SAVED = {}  # url -> {"digest", "entry"} of the last file written for it
SERIES = {}  # url -> the utilization series body, built at the first tick


def capture_tick(w, index, t, chapter, out_dir):
    tick_dir = os.path.join(out_dir, "ticks", f"{index:04d}")
    os.makedirs(tick_dir, exist_ok=True)
    captured, responses = {}, {}
    captured_at = iso()

    def save(url, status, content_type, body, ext="json"):
        body = scrub(body)
        captured[url] = body
        digest = hashlib.sha1(body).hexdigest()
        prior = SAVED.get(url)
        if prior and prior["digest"] == digest:
            # Unchanged since an earlier tick: point at that file, and keep its
            # capture time, which its timestamps are relative to.
            entry = dict(prior["entry"])
        else:
            name = f"{file_name(url)}.{ext}"
            with open(os.path.join(tick_dir, name), "wb") as out:
                out.write(body)
            entry = {"file": f"ticks/{index:04d}/{name}", "captured_at": captured_at}
            SAVED[url] = {"digest": digest, "entry": entry}
        entry.update(status=status, content_type=content_type.split(";")[0] or "application/json")
        responses[url] = entry

    def grab(url):
        if url.startswith("/client/utilization/series") and url in SAVED:
            save(url, 200, "application/json", SERIES[url])
            return
        if url.startswith(SYNTHETIC):
            body = json.dumps(synthetic(url, t, w)).encode()
            if url.startswith("/client/utilization/series"):
                # A day of history: built once, so every tick shares one file.
                SERIES[url] = body
            save(url, 200, "application/json", body)
            return
        status, content_type, body, final = fetch(w.base, url)
        if url == "/client/queue" or url.startswith("/client/queue?"):
            doc = json.loads(body)
            doc["host"] = synthetic("/client/host-status", t, w)
            body = json.dumps(doc).encode()
        save(url, status, content_type, body)
        if final != url:
            responses[url]["redirect"] = final
            save(final, status, content_type, body)

    for url in GLOBAL:
        grab(url)
    for url in detail_urls(captured):
        if url not in responses:
            grab(url)
    for url in doc_pages(w.base, captured):
        status, content_type, body, _ = fetch(w.base, url, accept="text/html")
        save(url, status, content_type, body, ext="html")
    return {"index": index, "t": t, "captured_at": captured_at, "chapter": chapter, "responses": responses}


# ---- setup and main -------------------------------------------------------------

def write_config(args):
    config = f"""owner_name: {OWNER}
paths:
  state_file: {ROOT}/state/sessions.json
  log_dir: {ROOT}/logs
  notes_db: {ROOT}/state/notes.db
sm_send:
  db_path: {ROOT}/state/message_queue.db
queue_runner:
  state_dir: {ROOT}/q
tool_logging:
  db_path: {ROOT}/state/tool_usage.db
usage:
  enabled: false
  db_path: {ROOT}/state/usage.db
activity:
  db_path: {ROOT}/state/activity.db
push:
  db_path: {ROOT}/state/owner_push.db
utilization:
  enabled: false
  db_path: {ROOT}/state/utilization.db
bug_reports:
  db_path: {ROOT}/state/bug_reports.db
email:
  bridge_config: {ROOT}/email_bridge.yaml
codex:
  command: {FAKE_BIN}/codex
codex_fork:
  command: {FAKE_BIN}/codex
tmux:
  socket_name: demo-none
board:
  repos: [{REPO}]
  checkouts:
    {REPO}: {SHOP}
rust_core:
  runtime_enabled: false
  fixture_writes_enabled: true
"""
    with open(f"{ROOT}/config.yaml", "w") as out:
        out.write(config)
    with open(f"{ROOT}/email_bridge.yaml", "w") as out:
        out.write(f"humans:\n  {HUMAN}:\n    display_name: {OWNER}\n")


def owner_settings():
    stamp = iso()
    return {
        "new_agent": {"value": {
            "workspaces": ["/home/demo/shop"], "repo_short": {REPO: "shop"},
            "agent_types": [
                {"name": "Top", "provider": "claude", "model": "fable", "effort": "xhigh"},
                {"name": "Mid", "provider": "claude", "model": "opus[1m]", "effort": "high"},
                {"name": "Codex", "provider": "codex-fork", "model": "gpt-5.5", "effort": "high"},
                {"name": "Local", "provider": "codex-fork", "model": "qwen3-coder-next", "effort": "high"}]},
            "updated_at": stamp},
        "queue_limits": {"value": {"tests": 1, "review": 2}, "updated_at": stamp},
    }


def seed(w):
    os.makedirs(f"{ROOT}/state", exist_ok=True)
    os.makedirs(f"{ROOT}/q/logs", exist_ok=True)
    os.makedirs(f"{ROOT}/home/.claude", exist_ok=True)
    os.makedirs(SHOP, exist_ok=True)
    for agent_id in AGENTS:
        os.makedirs(w.worktree(agent_id), exist_ok=True)
        subprocess.run(["git", "init", "-q", w.worktree(agent_id)], check=True)
        subprocess.run(["git", "-C", w.worktree(agent_id), "remote", "add", "origin",
                        f"https://github.com/{REPO}.git"], check=False)
    w.save_gh()
    w.write_sessions({"sessions": [], "owner_settings": owner_settings()})
    # The message db must exist before start, or the server skips creating its tables.
    sqlite3.connect(w.mq).close()
    with sqlite3.connect(w.tool_db) as conn:
        conn.execute("""CREATE TABLE IF NOT EXISTS tool_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
            session_id TEXT, session_name TEXT, parent_session_id TEXT, claude_session_id TEXT,
            tool_use_id TEXT, cwd TEXT, project_name TEXT, agent_id TEXT, hook_type TEXT NOT NULL,
            tool_name TEXT NOT NULL, tool_input TEXT, tool_response TEXT)""")


def start_server(w, args):
    env = dict(os.environ, PATH=f"{FAKE_BIN}:{os.environ['PATH']}", SMDEMO_WORLD=w.world_file,
               SM_TEST_ISOLATION_ROOT=f"{ROOT}/iso", HOME=f"{ROOT}/home",
               CLAUDE_CONFIG_DIR=f"{ROOT}/home/.claude")
    log = open(f"{ROOT}/server.log", "w")
    w.server = subprocess.Popen([args.server, "--config", f"{ROOT}/config.yaml", "--port", str(args.port)],
                                env=env, stdout=log, stderr=subprocess.STDOUT, cwd=ROOT)
    for _ in range(100):
        try:
            if fetch(w.base, "/health")[0] == 200:
                return
        except OSError:
            pass
        time.sleep(0.2)
    raise RuntimeError(f"scratch server did not come up; see {ROOT}/server.log")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--out", default=os.path.join(WEB_DEMO, "fixtures"))
    parser.add_argument("--tick", type=float, default=5.0, help="seconds per tick (5 for the real recording)")
    parser.add_argument("--duration", type=int, default=600, help="storyline seconds")
    parser.add_argument("--port", type=int, default=8431)
    parser.add_argument("--server", default=shutil.which("sm-server") or "sm-server")
    parser.add_argument("--sm", default=shutil.which("sm") or "sm")
    args = parser.parse_args()

    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)
    w = World(args)
    write_config(args)
    seed(w)
    start_server(w, args)
    # The lead was already planning when the recording starts.
    prologue(w)
    w.spawn("a0000001", minutes_ago=18)
    # Record beside the destination and swap it in only after the leak scan.
    final_out, args.out = args.out, args.out.rstrip("/") + ".partial"
    shutil.rmtree(args.out, ignore_errors=True)
    os.makedirs(args.out)
    events = storyline(w)
    step = 5  # storyline seconds per tick; --tick only changes how long a tick takes to play
    ticks, chapter, done = [], None, 0
    start = time.monotonic()
    try:
        for index in range(args.duration // step):
            t = index * step
            while done < len(events) and events[done][0] <= t:
                at, caption, action = events[done]
                chapter = caption or chapter
                w.log(f"t={at} {caption or ''}")
                action()
                done += 1
            w.heartbeat()
            began = time.monotonic()
            ticks.append(capture_tick(w, index, t, chapter, args.out))
            print(f"tick {index} t={t}s {len(ticks[-1]['responses'])} responses in "
                  f"{time.monotonic() - began:.1f}s: {chapter}", flush=True)
            delay = start + (index + 1) * args.tick - time.monotonic()
            if delay > 0:
                time.sleep(delay)
    finally:
        w.server.terminate()
        w.server.wait(timeout=30)

    timeline = {
        "schema_version": 1, "tick_seconds": step, "play_seconds_per_tick": args.tick,
        "duration_seconds": args.duration, "loop": True, "owner_name": OWNER, "repo": REPO,
        "recorded_at": ticks[0]["captured_at"], "generator": "web-demo/generate/record.py",
        "chapters": [{"t": at, "caption": caption} for at, caption, _ in events if caption],
        "ticks": ticks,
    }
    with open(os.path.join(args.out, "timeline.json"), "w") as out:
        json.dump(timeline, out, indent=1)
    # The static layer (Analytics, notes) is not part of the recording; keep it.
    for name in ("static", "static.json"):
        src = os.path.join(final_out, name)
        if os.path.isdir(src):
            shutil.copytree(src, os.path.join(args.out, name))
        elif os.path.exists(src):
            shutil.copy(src, args.out)

    leaks = []
    words = denylist()
    for folder, _, files in os.walk(args.out):
        for name in files:
            text = open(os.path.join(folder, name), "rb").read().decode(errors="replace").lower()
            leaks += [f"{os.path.join(folder, name)}: {word}" for word in words if word in text]
    if leaks:
        shutil.rmtree(args.out)
        sys.exit(f"fixtures contain real names; {final_out} is unchanged:\n" + "\n".join(leaks[:40]))
    shutil.rmtree(final_out, ignore_errors=True)
    os.replace(args.out, final_out)
    print(f"recorded {len(ticks)} ticks into {final_out}")


if __name__ == "__main__":
    main()
