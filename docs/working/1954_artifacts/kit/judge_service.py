#!/usr/bin/env python3
"""#1954 judge service (memo appendix K.3, K.4, K.8). Runs outside the wall on loopback.

POST /decide   body: Claude Code's PreToolUse JSON; header X-Local-Agent: <agent id>.
               Answers the hook's JSON with permissionDecision allow or deny, never ask.
POST /allow    body: {"denial_id": "d-7f3a"}. The owner allows one denied call once (K.8).
GET  /health

Decision order: an unused owner allow for this exact call; the rules (K.3); the judge (K.4).
No judge answer within JUDGE_TIMEOUT, or an answer that does not parse, is a deny.

usage: judge_service.py <agents.json> [--port 8431] [--model-url http://127.0.0.1:8000]
       [--log ~/.local/share/claude-sessions/local_judge.jsonl] [--timeout 30] [--no-model]
agents.json: {"<agent id>": {"name", "ticket", "title", "branch", "checkout", "tmp", "parent",
              "sm_url"}}, re-read on every call so agents can be added while it runs.
"""
import argparse
import http.client
import json
import os
import re
import secrets
import shlex
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

HOME = os.path.expanduser("~")
ap = argparse.ArgumentParser()
ap.add_argument("agents")
ap.add_argument("--port", type=int, default=8431)
ap.add_argument("--model-url", default="http://127.0.0.1:8000")
ap.add_argument("--log", default=f"{HOME}/.local/share/claude-sessions/local_judge.jsonl")
ap.add_argument("--allow-file", default=f"{HOME}/.local/share/claude-sessions/local_judge_allow.jsonl")
ap.add_argument("--timeout", type=float, default=30.0)
ap.add_argument("--proxy-port", type=int, default=8432)
ap.add_argument("--policy", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "judge_policy.md"))
ap.add_argument("--no-model", action="store_true", help="every judge-stage call is a deny (rule-stage tests)")
ARGS = ap.parse_args()
LOCK = threading.Lock()

DENY_TAIL = ("Denied ({id}). Do not retry this in another form. If the work needs it, tell your parent "
             "with sm send, then continue with what you can do, or stop.")

# --- rule stage (K.3) -------------------------------------------------------------------------

ALWAYS_ALLOWED = {"Read", "Glob", "Grep", "TodoWrite"}
PATH_TOOLS = {"Edit": "file_path", "Write": "file_path", "NotebookEdit": "notebook_path"}
# A word counts only as a whole shell token: "sm" matches `sm send`, not `crates/sm-server`.
_B, _A = r"(?<![\w./:@-])", r"(?![\w./:@-])"
GIT_EGRESS = re.compile(_B + r"git" + _A + r".*?" + _B
                        + r"(push|fetch|pull|clone|remote|ls-remote|submodule)" + _A, re.S)
# Text that builds a command at run time, so the words above cannot be checked: always judged.
OBFUSCATION = re.compile(r"(?<![\w-])(eval|base64|xxd|uudecode)(?![\w-])|\\x[0-9a-fA-F]{2}|\$'|\\[0-7]{3}")
# Credentials the wall leaves readable so gh works: a command that names one is always judged,
# and the Read tool on them is denied.
CREDENTIAL = re.compile(r"\.config/gh|hosts\.yml|GH_TOKEN|GITHUB_TOKEN|GH_ENTERPRISE_TOKEN|oauth_token"
                        r"|\.netrc|\.ssh/|Keychains|security\s+find-|\.git-credentials")
CREDENTIAL_PATHS = [os.path.join(HOME, ".config", "gh"), os.path.join(HOME, ".git-credentials")]
# Running a file the agent can write: its text is checked as if it were part of the command.
SCRIPT_RUN = re.compile(r"(?:^|[\s;&|(])(?:(?:ba|z|da|k)?sh|python3?|perl|ruby|node|source|\.)\s+([^\s;&|)]+)"
                        r"|(?:^|[;&|(]|&&|\|\|)\s*(\.{0,2}/[^\s;&|)]+)")  # a path in command position


def egress_words(agent):
    words = ["gh", "sm", "curl", "wget", "nc", "ssh", "scp", "rsync", "open", "osascript", "8420",
             str(ARGS.proxy_port)]
    port = urlparse(agent.get("sm_url") or "").port
    if port and str(port) not in words:
        words.append(str(port))
    return re.compile(_B + "(" + "|".join(map(re.escape, words)) + ")" + _A)


def normalise(text):
    """Drop quoting and line continuations so `'gi''t' pu"sh"` reads as `git push`."""
    return re.sub(r"\\\n", "", text).replace("'", "").replace('"', "").replace("\\", "")


def script_texts(command, cwd, agent):
    """Text of each script file the command runs, read only inside the checkout or temp folder."""
    out = []
    for m in SCRIPT_RUN.finditer(command):
        p = m.group(1) or m.group(2)
        if not p or p.startswith("-"):
            continue
        p = os.path.normpath(os.path.join(cwd or agent["checkout"], os.path.expanduser(p)))
        if inside(p, agent) and os.path.isfile(p):
            try:
                with open(p, errors="replace") as f:
                    out.append((p, f.read(200_000)))
            except OSError:
                pass
    return out


def egress_hits(text, agent):
    t = normalise(text)
    hits = [m.group(0) for m in GIT_EGRESS.finditer(t)][:1]
    hits += sorted({m.group(1) for m in egress_words(agent).finditer(t)})
    if OBFUSCATION.search(text):
        hits.append("obfuscation")
    if CREDENTIAL.search(t):
        hits.append("credential")
    return hits


def inside(path, agent):
    real = os.path.realpath(path)
    return any(real == r or real.startswith(r + os.sep)
               for r in (os.path.realpath(agent["checkout"]), os.path.realpath(agent["tmp"])))


def rule_stage(tool, tin, cwd, agent):
    """('allow'|'deny', reason) when the rules decide, or ('judge', why) when they do not."""
    if tool in ALWAYS_ALLOWED:
        path = os.path.realpath(os.path.expanduser(tin.get("file_path") or tin.get("path") or ""))
        if any(path == c or path.startswith(c + os.sep) for c in CREDENTIAL_PATHS):
            return "deny", "reading GitHub credentials is not allowed"
        return "allow", f"{tool} is always allowed"
    if tool in PATH_TOOLS:
        path = tin.get(PATH_TOOLS[tool]) or ""
        if inside(path, agent):
            return "allow", f"{tool} inside the checkout or temp folder"
        return "deny", (f"{tool} outside your checkout and temp folder is not allowed; the sandbox "
                        f"would block the write anyway. Write only under {agent['checkout']} or {agent['tmp']}.")
    if tool == "Bash":
        cmd = tin.get("command") or ""
        hits = egress_hits(cmd, agent)
        for path, text in script_texts(cmd, cwd, agent):
            more = egress_hits(text, agent)
            hits += [f"{os.path.basename(path)}:{h}" for h in more]
        if not hits:
            return "allow", "no egress word in the command"
        return "judge", "egress words: " + ", ".join(hits)
    return "judge", f"tool {tool} has no rule"


# --- judge stage (K.4) ------------------------------------------------------------------------

def judge_message(tool, tin, agent):
    if tool == "Bash":
        action = tin.get("command") or ""
    else:
        action = json.dumps(tin, indent=1)[:4000]
    return (f"Agent: {agent['name']} working ticket #{agent['ticket']} \"{agent['title']}\" "
            f"on branch {agent['branch']} in {agent['checkout']}\n"
            f"Parent: {agent['parent']}\n"
            f"sm server for this agent: {agent.get('sm_url') or 'http://127.0.0.1:8420'}\n"
            f"Tool: {tool}\nCommand:\n{action}\n"
            "Answer with one line: ALLOW or DENY, a colon, and a reason under 20 words.")


VERDICT = re.compile(r"^\W*(ALLOW|DENY)\W*:?\s*(.*)$", re.I)


def ask_judge(tool, tin, agent):
    """(decision, reason, raw) or raises on timeout / unparseable reply."""
    with open(ARGS.policy) as f:
        policy = f.read()
    body = json.dumps({
        "model": "judge", "max_tokens": 200, "temperature": 0, "stream": True,
        "thinking": {"type": "disabled"}, "enable_thinking": False,
        "system": policy,
        "messages": [{"role": "user", "content": judge_message(tool, tin, agent)}],
    })
    u = urlparse(ARGS.model_url)
    deadline = time.time() + ARGS.timeout
    conn = http.client.HTTPConnection(u.hostname, u.port, timeout=ARGS.timeout)
    try:
        conn.request("POST", "/v1/messages", body, {"Content-Type": "application/json",
                                                    "x-api-key": "local", "anthropic-version": "2023-06-01"})
        resp = conn.getresponse()
        if resp.status != 200:
            raise RuntimeError(f"model HTTP {resp.status}: {resp.read(300)!r}")
        text = ""
        while time.time() < deadline:
            line = resp.fp.readline()
            if not line:
                break
            if not line.startswith(b"data:"):
                continue
            try:
                ev = json.loads(line[5:])
            except ValueError:
                continue
            d = ev.get("delta") or {}
            if d.get("type") == "text_delta":
                text += d.get("text", "")
                # Act on the answer line as soon as it is complete; MTPLX may hold the stream open.
                for ln in text.split("\n")[:-1]:
                    m = VERDICT.match(ln.strip())
                    if m:
                        return m.group(1).lower(), m.group(2).strip(), text
            if ev.get("type") == "message_stop":
                break
        for ln in text.split("\n"):
            m = VERDICT.match(ln.strip())
            if m:
                return m.group(1).lower(), m.group(2).strip(), text
        if time.time() >= deadline:
            raise TimeoutError("judge did not answer in time")
        raise ValueError(f"unparseable judge reply: {text[:200]!r}")
    finally:
        conn.close()


# --- owner allows (K.8) -----------------------------------------------------------------------

def read_jsonl(path):
    try:
        with open(path) as f:
            return [json.loads(l) for l in f if l.strip()]
    except FileNotFoundError:
        return []


def append(path, rec):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "a") as f:
        f.write(json.dumps(rec) + "\n")


def action_text(tool, tin):
    return tin.get("command") if tool == "Bash" else (tin.get("file_path") or json.dumps(tin, sort_keys=True))


def take_owner_allow(agent_id, tool, text):
    """Consume an unused allow for this exact call; returns its denial id or None. Caller holds LOCK."""
    rows = read_jsonl(ARGS.allow_file)
    used = {r["denial_id"] for r in rows if r.get("used_at")}
    for r in rows:
        if (not r.get("used_at") and r["denial_id"] not in used and r.get("session_id") == agent_id
                and r.get("tool") == tool and r.get("command") == text):
            append(ARGS.allow_file, {"denial_id": r["denial_id"], "used_at": time.time()})
            return r["denial_id"]
    return None


def owner_allow(denial_id):
    with LOCK:
        hit = next((r for r in read_jsonl(ARGS.log) if r.get("denial_id") == denial_id), None)
        if not hit:
            return None
        rec = {"denial_id": denial_id, "session_id": hit["session_id"], "tool": hit["tool"],
               "command": hit["command"], "allowed_at": time.time()}
        append(ARGS.allow_file, rec)
        return rec


# --- service ----------------------------------------------------------------------------------

def decide(hook, agent_id):
    t0 = time.time()
    agents = json.load(open(ARGS.agents))
    agent = agents.get(agent_id)
    tool, tin = hook.get("tool_name") or "", hook.get("tool_input") or {}
    text = action_text(tool, tin)
    rec = {"t": t0, "session_id": agent_id, "claude_session": hook.get("session_id"), "tool": tool,
           "command": text}
    if agent is None:
        stage, decision, reason = "rule", "deny", f"unknown agent {agent_id!r}"
    else:
        with LOCK:
            owner = take_owner_allow(agent_id, tool, text)
        if owner:
            stage, decision, reason = "owner", "allow", f"allowed once by the owner after denial {owner}"
        else:
            stage = "rule"
            decision, reason = rule_stage(tool, tin, hook.get("cwd"), agent)
            if decision == "judge":
                stage, rec["judge_why"] = "judge", reason
                if ARGS.no_model:
                    decision, reason = "deny", "judge unavailable; retry this command in a minute"
                else:
                    try:
                        decision, reason, raw = ask_judge(tool, tin, agent)
                        rec["judge_raw"] = raw[:500]
                    except Exception as e:  # timeout, connection, parse: all deny
                        rec["judge_error"] = f"{type(e).__name__}: {e}"[:300]
                        decision, reason = "deny", "judge unavailable; retry this command in a minute"
    rec.update(stage=stage, decision=decision, reason=reason, ms=round((time.time() - t0) * 1000))
    if decision == "deny":
        rec["denial_id"] = "d-" + secrets.token_hex(2)
        if stage == "rule" and tool in PATH_TOOLS:  # a wrong place, not a forbidden action
            shown = f"{reason.rstrip('.')}. Denied ({rec['denial_id']}); write it there instead."
        else:
            shown = f"{reason.rstrip('.')}. {DENY_TAIL.format(id=rec['denial_id'])}"
    else:
        shown = reason
    with LOCK:
        append(ARGS.log, rec)
    return {"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": decision,
                                   "permissionDecisionReason": shown}}


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _reply(self, code, payload):
        data = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self._reply(200, {"ok": True})

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        try:
            body = json.loads(raw or b"{}")
        except ValueError:
            body = {}
        if self.path == "/decide":
            return self._reply(200, decide(body, self.headers.get("X-Local-Agent") or ""))
        if self.path == "/allow":
            rec = owner_allow(body.get("denial_id") or "")
            return self._reply(200 if rec else 404, rec or {"error": "no such denial"})
        self._reply(404, {"error": "not found"})


if __name__ == "__main__":
    print(f"judge service on 127.0.0.1:{ARGS.port}, log {ARGS.log}", flush=True)
    ThreadingHTTPServer(("127.0.0.1", ARGS.port), H).serve_forever()
