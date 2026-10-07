#!/usr/bin/env python3
"""Snapshot a real server's Analytics reports for the demo, with every name invented.

    python3 web-demo/generate/analytics.py               # reads http://127.0.0.1:8420
    python3 web-demo/generate/analytics.py --server URL

The numbers stay real: spend percentages and tokens, active and parked time,
the model and activity splits, and how they divide between repos, tickets
and agents. Every repo, ticket, agent and account name is replaced, agent
links are dropped, and the result is checked for any original name before it
is written to fixtures/static/ (see README.md, "The static layer").
"""
import argparse
import hashlib
import json
import os
import re
import sys
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(os.path.dirname(HERE), "fixtures")

REPORTS = [f"/client/analytics/spend?range={r}{p}" for r in ("week", "last_week", "4w")
           for p in ("", "&provider=claude", "&provider=codex")]
REPORTS += [f"/client/analytics/time?range={r}" for r in ("24h", "7d", "30d")]

# Biggest real repo first gets the first name.
REPO_NAMES = ["pricing-engine", "shop", "infra", "mobile-app", "data-pipeline", "docs-site",
              "design-system", "billing", "search", "warehouse", "support-bot", "status-page",
              "auth", "notifications", "admin", "recommendations", "inventory", "reviews", "loyalty",
              "shipping", "returns", "fraud-checks", "reporting", "partner-api", "web-checkout",
              "marketing-site", "feature-flags", "load-tests", "dev-tools", "sandbox"]
VERBS = ["Speed up", "Fix", "Add", "Retry", "Cache", "Validate", "Simplify", "Log", "Paginate", "Rename",
         "Backfill", "Test", "Document", "Split", "Batch", "Index"]
THINGS = ["the price feed", "order exports", "the nightly import", "refund webhooks", "search ranking",
          "the settings page", "session expiry", "image uploads", "the tax table", "currency rounding",
          "the release pipeline", "flaky checkout tests", "the admin dashboard", "rate limits",
          "inventory sync", "email templates", "the onboarding flow", "audit logs", "API errors",
          "the cost report"]
TEXT_FIELDS = ("id", "label", "history_path", "session_id")


def fetch(server, path):
    with urllib.request.urlopen(server + path, timeout=60) as response:
        return json.load(response)


def walk(node):
    yield node
    for child in node.get("children") or []:
        yield from walk(child)


def repo_of(node_id):
    # r:<repo> and t:<repo>#<number>
    return node_id.split(":", 1)[1].split("#", 1)[0]


class Names:
    def __init__(self, reports):
        size = {}
        for report in reports.values():
            for node in report["root"].get("children") or []:
                if node["kind"] == "repo":
                    key = repo_of(node["id"])
                    size[key] = size.get(key, 0) + node.get("tokens", 0) + node.get("active_seconds", 0)
        ranked = sorted(size, key=lambda k: -size[k])
        self.repos = {}
        for index, real in enumerate(ranked):
            short = REPO_NAMES[index] if index < len(REPO_NAMES) else f"tool-{index + 1}"
            self.repos[real] = short
        self.tickets, self.agents, self.next_number = {}, {}, {}

    def repo(self, real):
        return self.repos[real]

    def ticket(self, real_repo, number):
        key = (real_repo, number)
        if key not in self.tickets:
            n = self.next_number.get(real_repo, 101)
            self.next_number[real_repo] = n + 1
            digest = int(hashlib.sha1(f"{real_repo}#{number}".encode()).hexdigest(), 16)
            title = f"{VERBS[digest % len(VERBS)]} {THINGS[(digest // len(VERBS)) % len(THINGS)]}"
            self.tickets[key] = (n, title)
        return self.tickets[key]

    def agent(self, session_id, base):
        if session_id not in self.agents:
            count = sum(1 for name in self.agents.values() if name.startswith(base))
            self.agents[session_id] = f"{base}-{count + 1}" if count else base
        return self.agents[session_id]


def rename(node, names, repo=None, ticket=None):
    kind = node["kind"]
    if kind == "repo":
        repo = repo_of(node["id"])
        short = names.repo(repo)
        node.update(id=f"r:acme/{short}", label=short)
    elif kind == "thread":
        real_repo = repo_of(node["id"])
        number = node["id"].rsplit("#", 1)[1]
        short = names.repo(real_repo)
        if number.isdigit():
            fake, title = names.ticket(real_repo, number)
            ticket = f"{short}-{fake}"
            node.update(id=f"t:acme/{short}#{fake}", label=f"#{fake} {title}")
        else:
            ticket = f"{short}-agent"
            node.update(id=f"t:acme/{short}#none", label="No ticket")
    elif kind == "agent":
        sid = node.get("session_id") or node["id"]
        name = names.agent(sid, ticket or "agent")
        node.update(id=f"a:{hashlib.sha1(sid.encode()).hexdigest()[:8]}", label=name)
    elif kind not in ("root", "gap"):  # gap: "Not in the ledger", a fixed label
        raise SystemExit(f"unknown node kind {kind!r} at {node['id']}")
    node["history_path"] = None
    node["session_id"] = None
    for child in node.get("children") or []:
        rename(child, names, repo, ticket)


def originals(reports):
    """Every name in the real reports, for the leak check."""
    found = set()
    for report in reports.values():
        for node in walk(report["root"]):
            if node["kind"] in ("root", "gap"):
                continue
            for field in TEXT_FIELDS:
                value = node.get(field)
                if isinstance(value, str):
                    found.add(value)
            if node["kind"] in ("repo", "thread"):
                repo = repo_of(node["id"])
                found.update({repo, repo.split("/")[-1]})
            if node["kind"] == "thread":
                found.update(word for word in re.findall(r"[A-Za-z][\w.-]{6,}", node["label"]))
        for meter in report.get("meters", []):
            found.update({meter.get("label", ""), meter.get("account_key", "")})
    found.update({"rajesh", "goli", "fractal", "gmail"})
    # Short or generic words would match the invented names by chance.
    generic = {w.lower() for w in VERBS + THINGS + REPO_NAMES} | {"no ticket", "claude", "codex"}
    return {text for text in found if len(text) >= 5 and text.lower() not in generic
            and not any(text.lower() in g for g in generic)}


def scrub(reports):
    names = Names(reports)
    real = originals(reports)
    accounts = {}
    out = {}
    for url, report in reports.items():
        report = json.loads(json.dumps(report))
        rename(report["root"], names)
        report["notes"] = []
        for meter in report.get("meters", []):
            provider = meter.get("account_key", "x:").split(":")[0]
            index = accounts.setdefault(meter.get("account_key"), sum(1 for k in accounts if k.startswith(provider)) + 1)
            meter.update(account_key=f"{provider}:demo-{index}", label=f"Team account {index}")
        out[url] = report
    # Only names can leak: the legends and field names are the server's own words.
    text = json.dumps([[node.get(f) for node in walk(r["root"]) for f in TEXT_FIELDS] + r.get("meters", [])
                       for r in out.values()]).lower()
    leaks = sorted(word for word in real if word.lower() in text)
    if leaks:
        raise SystemExit(f"refusing to write: real names survived the scrub: {leaks[:20]}")
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--server", default="http://127.0.0.1:8420")
    parser.add_argument("--out", default=FIXTURES)
    args = parser.parse_args()
    reports = {url: fetch(args.server, url) for url in REPORTS}
    scrubbed = scrub(reports)
    static_dir = os.path.join(args.out, "static")
    os.makedirs(static_dir, exist_ok=True)
    index_path = os.path.join(args.out, "static.json")
    index = {"schema_version": 1, "responses": {}}
    if os.path.exists(index_path):
        with open(index_path) as f:
            index = json.load(f)
    for url, report in scrubbed.items():
        name = re.sub(r"[^A-Za-z0-9]+", "_", url.strip("/")) + ".json"
        with open(os.path.join(static_dir, name), "w") as f:
            json.dump(report, f, separators=(",", ":"))
        index["responses"][url] = {"file": f"static/{name}", "captured_at": report["generated_at"],
                                   "status": 200, "content_type": "application/json"}
    with open(index_path, "w") as f:
        json.dump(index, f, indent=1, sort_keys=True)
    print(f"wrote {len(scrubbed)} reports to {static_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
