#!/usr/bin/env python3
"""Production local judge, lifted from #1954. Only /decide is agent-facing.

The private Unix control socket owns registrations and owner grants. The wall must
block the whole state directory (including its sockets) from every local agent.
The daemon detaches from sm so sm's blue/green restart never interrupts decisions.
"""
import argparse
import http.client
import json
import os
import re
import secrets
import shlex
import shutil
import hashlib
import fcntl
import socketserver
import queue
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

HOME = os.path.expanduser("~")
ap = argparse.ArgumentParser()
ap.add_argument("--root", required=True)
ap.add_argument("--generation", default="standalone")
ap.add_argument("--model-auth-file")
ap.add_argument("--port", type=int, default=8431)
ap.add_argument("--model-url", default="http://127.0.0.1:8000")
ap.add_argument("--timeout", type=float, default=30.0)
ap.add_argument("--proxy-port", type=int, default=8432)
ap.add_argument("--policy", required=True)
ap.add_argument("--no-model", action="store_true")
ARGS = ap.parse_args()
MODEL_TOKEN = "local"
if ARGS.model_auth_file:
    with open(ARGS.model_auth_file) as f:
        MODEL_TOKEN = json.load(f)["auth_token"]
ARGS.agents = os.path.join(ARGS.root, "agents.json")
ARGS.log = os.path.join(ARGS.root, "decisions.jsonl")
ARGS.allow_file = os.path.join(ARGS.root, "allows.jsonl")
CONTROL = os.path.join(ARGS.root, "control.sock")
LOCK = threading.RLock()
STOPPING = False
ACTIVE = 0
DRAIN = threading.Condition(LOCK)
STOP_EVENT = threading.Event()

DENY_TAIL = ("Denied ({id}). Do not retry this in another form. If the work needs it, tell your parent "
             "with sm send, then continue with what you can do, or stop.")

# --- rule stage (K.3) -------------------------------------------------------------------------

ALWAYS_ALLOWED = {"Read", "Glob", "Grep", "TodoWrite"}
PATH_TOOLS = {"Edit": "file_path", "Write": "file_path", "NotebookEdit": "notebook_path"}
# A word counts only as a whole shell token: "sm" matches `sm send`, not `crates/sm-server`.
_B, _A = r"(?<![\w.:@-])", r"(?![\w./:@-])"
GIT_EGRESS = re.compile(_B + r"git" + _A + r".*?" + _B
                        + r"(push|fetch|pull|clone|remote|ls-remote|submodule)" + _A, re.S)
# Text that builds a command at run time, so the words above cannot be checked: always judged.
OBFUSCATION = re.compile(r"(?<![\w-])(eval|base64|xxd|uudecode)(?![\w-])|\\x[0-9a-fA-F]{2}|\$'|\\[0-7]{3}")
# Credentials the wall leaves readable so gh works: a command that names one is always judged,
# and the Read tool on them is denied.
CREDENTIAL = re.compile(r"\.config/gh|hosts\.yml|GH_TOKEN|GITHUB_TOKEN|GH_ENTERPRISE_TOKEN|oauth_token"
                        r"|\.netrc|\.ssh/|Keychains|security\s+find-|\.git-credentials|\.aws/"
                        r"|\.claude(?:/|\.json)|\.codex/|\.config/session-manager"
                        r"|\.local/share/claude-sessions/local-judge", re.I)
CREDENTIAL_PATHS = [os.path.realpath(os.path.join(HOME, path)) for path in (
    ".config/gh", ".git-credentials", ".ssh", ".netrc", "Library/Keychains",
    ".aws", ".claude", ".claude.json", ".codex", ".config/session-manager",
    ".local/share/claude-sessions/local-judge",
)]


def egress_words(agent):
    words = ["git", "gh", "sm", "curl", "wget", "nc", "ssh", "scp", "rsync", "open", "osascript"]
    ports = {8420, ARGS.proxy_port}
    if agent.get("proxy_port"):
        ports.add(agent["proxy_port"])
    port = urlparse(agent.get("sm_url") or "").port
    if port:
        ports.add(port)
    # Numeric ports also occur after ':' in socket addresses. Command-word
    # boundaries deliberately reject ':', so they cannot be reused for ports.
    command_pattern = _B + "(?:" + "|".join(map(re.escape, words)) + ")" + _A
    port_pattern = r"(?<![0-9])(?:" + "|".join(str(port) for port in sorted(ports)) + r")(?![0-9])"
    return re.compile("(" + command_pattern + "|" + port_pattern + ")")


def normalise(text):
    """Drop quoting and line continuations so `'gi''t' pu"sh"` reads as `git push`."""
    return re.sub(r"\\\n", "", text).replace("'", "").replace('"', "").replace("\\", "")


def script_texts(command, cwd, agent, seen=None, depth=0):
    """Text of each script file the command runs, read only inside the checkout or temp folder."""
    out = []
    if seen is None:
        seen = set()
    if depth >= 8 or len(seen) >= 64:
        return [("script-scan-limit", "eval")]
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|()<>")
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        # An uncertain shell parse must not silently skip executable content.
        return [("shell-parse", "eval")]
    interpreters = {"sh", "bash", "zsh", "dash", "ksh", "python", "python3",
                    "perl", "ruby", "node", "lua", "php", "Rscript", "deno", "bun", "source", "."}
    candidates = set()
    if len(tokens) > 10000:
        return [("script-scan-limit", "eval")]
    for index, token in enumerate(tokens):
        # Check explicit path tokens conservatively even when a shell operator
        # or quoting obscures command position. shlex removes/joins shell quotes
        # and escapes, including paths containing spaces.
        if token == "cd" or token.startswith("PATH="):
            out.append(("script-working-directory", "eval"))
        if not token.startswith("-"):
            candidates.add(token)
        if os.path.basename(token) in (interpreters - {"sh", "bash", "zsh", "dash", "ksh", "source", "."}):
            # Language runtimes can make network calls without ever invoking
            # curl/git/etc. Absence of those words cannot establish no egress.
            out.append(("interpreted-program", "eval"))
        if os.path.basename(token) in interpreters:
            for arg in tokens[index + 1:]:
                if arg in (";", "&&", "||", "|", "(", ")", "<", ">"):
                    break
                if arg in ("-c", "-e", "-m", "--eval", "--command"):
                    # Inline interpreter programs can invoke another script;
                    # never treat an unscanned nested program as a rule allow.
                    out.append(("inline-program", "eval"))
                    break
                if not arg.startswith("-"):
                    candidates.add(arg)
    for candidate in candidates:
        path = os.path.normpath(os.path.join(cwd or agent["checkout"], os.path.expanduser(candidate)))
        resolved = os.path.realpath(path)
        protected_executables = {"git", "gh", "sm", "curl", "wget", "nc", "ssh", "scp", "rsync", "open", "osascript"}
        if os.path.basename(resolved) in protected_executables:
            out.append(("resolved-executable", "eval"))
        if inside(path, agent) and os.path.isfile(path):
            path = os.path.realpath(path)
            if path in seen:
                out.append(("script-cycle", "eval"))
                continue
            seen.add(path)
            try:
                with open(path, errors="replace") as f:
                    text = f.read(200_001)
                if len(text) > 200_000 or "\x00" in text:
                    out.append((path, "eval"))
                else:
                    out.append((path, text))
                    out.extend(script_texts(text, cwd, agent, seen, depth + 1))
            except OSError:
                out.append((path, "eval"))
    return out


def unrecognized_commands(command, agent):
    # Restrict rule allowances to simple shell/file operations. Package tools,
    # compilers, unknown binaries and language runtimes can perform networking
    # without containing any literal network word; the model decides those.
    safe = {"echo", "printf", "pwd", "ls", "cat", "head", "tail", "wc", "grep",
            "true", "false", "test", ":", "cd", "mkdir", "rmdir", "touch", "cp",
            "mv", "rm", "ln", "chmod", "sh", "bash", "zsh", "dash", "ksh", "source", "."}
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|()<>\n")
        lexer.whitespace = " \t\r"
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return True
    command_position = True
    for token in tokens:
        if not token.strip() or any(char in token for char in ";&|()") and set(token) <= set(";&|()\n"):
            command_position = True
            continue
        if token in ("<", ">", ">>", "<<", "<<<", "<&", ">&"):
            # Redirections include shell /dev/tcp transport and inline input;
            # never infer no egress from the executable name alone.
            return True
        if token == "PATH" or token.startswith(("PATH=", "BASH_ENV=", "ENV=", "LD_PRELOAD=", "DYLD_")):
            return True
        if not command_position:
            continue
        if token in {"command", "exec", "env", "sudo", "builtin", "time"} or "=" in token:
            continue
        command_position = False
        local_path = resolve(token, agent)
        if "/" in token or os.path.lexists(os.path.join(agent["checkout"], token)):
            path = local_path
        else:
            path = os.path.realpath(shutil.which(token) or local_path)
        builtins = {"echo", "printf", "pwd", "true", "false", "test", ":", "cd", "source", "."}
        is_builtin = token in builtins and "/" not in token
        trusted_system = os.path.dirname(path) in {"/bin", "/usr/bin"} and os.path.basename(path) in safe
        # A link's spelling is never its executable identity. Only trusted
        # system paths or shell builtins qualify by name; local source files
        # still require recursive inspection, including binary detection.
        if not is_builtin and not trusted_system:
            if not inside(path, agent) or not os.path.isfile(path):
                return True
    return False


def egress_hits(text, agent):
    t = normalise(text)
    hits = [m.group(0) for m in GIT_EGRESS.finditer(t)][:1]
    hits += sorted({m.group(1) for m in egress_words(agent).finditer(t)})
    if unrecognized_commands(text, agent):
        hits.append("unrecognized executable or shell operation")
    if OBFUSCATION.search(text):
        hits.append("obfuscation")
    # Shell expansions can synthesize a command name without any literal
    # egress word (a=g; b=it; "$a$b" push ...). Treat expansions conservatively;
    # the model policy distinguishes literal/data use from hidden commands.
    if re.search(r"\$[A-Za-z0-9_{(*@#?!-]|`", text):
        hits.append("dynamic shell expansion")
    if re.search(r"[*?\[{}]", text):
        hits.append("shell wildcard")
    if CREDENTIAL.search(t) or command_protected_paths(text, agent):
        hits.append("credential")
    return hits


def resolve(path, agent):
    return os.path.realpath(os.path.join(agent["checkout"], os.path.expanduser(path)))


def inside(path, agent):
    real = resolve(path, agent)
    return any(real == r or real.startswith(r + os.sep)
               for r in (os.path.realpath(agent["checkout"]), os.path.realpath(agent["tmp"])))


def protected_path(path, agent, ancestors=False):
    path = resolve(path, agent).casefold()
    return any(path == credential.casefold() or path.startswith(credential.casefold() + os.sep)
               or ancestors and (credential.casefold().startswith(path.rstrip(os.sep) + os.sep))
               for credential in CREDENTIAL_PATHS)


def command_protected_paths(command, agent):
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|()<>")
        lexer.whitespace_split = True
        tokens = list(lexer)
        recursive = any(token in {"--recursive", "--archive", "-a"}
                        or re.match(r"^-[^-]*[rR]", token) for token in tokens)
        # Option values can carry a path too (e.g. --input=/path). Recursive
        # readers/copiers must also protect parents containing credentials.
        return any(protected_path(token.split("=", 1)[-1], agent, recursive) for token in tokens)
    except ValueError:
        return False  # script_texts sends uncertain parsing to the judge


def git_credential_request(command, agent):
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|()<>")
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return False
    separators = {";", "&&", "||", "|", "(", ")"}
    command_position = True
    for index, token in enumerate(tokens):
        if token in separators:
            command_position = True
            continue
        if not command_position:
            continue
        if token in {"command", "exec", "env", "sudo"} or "=" in token:
            continue
        command_position = False
        if os.path.basename(resolve(token, agent)) == "git":
            for arg in tokens[index + 1:]:
                if arg in separators:
                    break
                if arg == "credential" or arg.startswith("credential-"):
                    return True
    return False


def current_branch(agent):
    """Read live Git metadata, including linked worktrees; registration is not HEAD."""
    try:
        git_dir = os.path.join(agent["checkout"], ".git")
        if os.path.isfile(git_dir):
            with open(git_dir) as f:
                link = f.read(4096).strip()
            if not link.startswith("gitdir: "):
                return None
            git_dir = os.path.realpath(os.path.join(agent["checkout"], link[8:]))
        with open(os.path.join(git_dir, "HEAD")) as f:
            head = f.read(4096).strip()
        prefix = "ref: refs/heads/"
        if head.startswith(prefix) and re.fullmatch(r"[\w./-]+", head[len(prefix):]):
            return head[len(prefix):]
    except (OSError, ValueError):
        pass
    return None


def rule_stage(tool, tin, cwd, agent):
    """('allow'|'deny', reason) when the rules decide, or ('judge', why) when they do not."""
    if tool in ALWAYS_ALLOWED:
        path = resolve(tin.get("file_path") or tin.get("path") or "", agent)
        if protected_path(path, agent, tool in {"Glob", "Grep"}):
            return "deny", "reading protected credentials is not allowed"
        return "allow", f"{tool} is always allowed"
    if tool in PATH_TOOLS:
        path = tin.get(PATH_TOOLS[tool]) or ""
        if path and inside(path, agent):
            return "allow", f"{tool} inside the checkout or temp folder"
        return "deny", (f"{tool} outside your checkout and temp folder is not allowed; the sandbox "
                        f"would block the write anyway. Write only under {agent['checkout']} or {agent['tmp']}.")
    if tool == "Bash":
        cmd = tin.get("command") or ""
        if not agent.get("proxy_port"):
            return "judge", "registration lacks an authoritative egress proxy port"
        if git_credential_request(cmd, agent):
            return "deny", "retrieving or changing Git credentials is not allowed"
        if command_protected_paths(cmd, agent):
            return "deny", "reading protected credentials is not allowed"
        hits = egress_hits(cmd, agent)
        texts = [cmd]
        for path, text in script_texts(cmd, cwd, agent):
            texts.append(text)
            more = egress_hits(text, agent)
            hits += [f"{os.path.basename(path)}:{h}" for h in more]
        if any(match.group(1) == "push" for text in texts
               for match in GIT_EGRESS.finditer(normalise(text))):
            if current_branch(agent) != agent["branch"]:
                return "deny", "checkout HEAD does not name the registered branch"
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
            f"Current checkout branch: {json.dumps(current_branch(agent))}\n"
            f"Parent: {agent['parent']}\n"
            f"sm server for this agent: {agent.get('sm_url') or 'http://127.0.0.1:8420'}\n"
            f"Egress proxy port: {agent.get('proxy_port', 'unknown')}\n"
            f"Tool: {tool}\nCommand:\n{action}\n"
            "Answer with one line: ALLOW or DENY, a colon, and a reason under 20 words.")


VERDICT = re.compile(r"^(ALLOW|DENY): ([^\r\n]+)$")


def ask_judge_stream(tool, tin, agent):
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
    deadline = time.monotonic() + ARGS.timeout
    conn = http.client.HTTPConnection(u.hostname, u.port, timeout=ARGS.timeout)
    try:
        conn.request("POST", "/v1/messages", body, {"Content-Type": "application/json",
                                                    "x-api-key": MODEL_TOKEN, "anthropic-version": "2023-06-01"})
        resp = conn.getresponse()
        if resp.status != 200:
            raise RuntimeError(f"model HTTP {resp.status}: {resp.read(300)!r}")
        text, pending = "", b""
        stream_socket = resp.fp.raw._sock
        while time.monotonic() < deadline:
            stream_socket.settimeout(max(0.001, deadline - time.monotonic()))
            # HTTPResponse decodes chunk framing; raw fp.readline does not.
            chunk = resp.read1(4096)
            if not chunk:
                break
            pending += chunk
            if len(pending) > 65536:
                raise ValueError("judge event too large")
            while b"\n" in pending:
                line, pending = pending.split(b"\n", 1)
                if not line.startswith(b"data:"):
                    continue
                try:
                    ev = json.loads(line[5:])
                except ValueError:
                    continue
                delta = ev.get("delta") or {}
                if delta.get("type") == "text_delta":
                    text += delta.get("text", "")
                    if len(text) > 4096:
                        raise ValueError("judge reply too large")
                    # The answer line can finish before MTPLX closes the stream.
                    if "\n" in text:
                        match = VERDICT.fullmatch(text.split("\n", 1)[0].strip())
                        if match and len(match.group(2).split()) < 20:
                            return match.group(1).lower(), match.group(2).strip(), text
                        raise ValueError("unparseable judge reply")
                if ev.get("type") == "message_stop":
                    match = VERDICT.fullmatch(text.strip())
                    if match and len(match.group(2).split()) < 20:
                        return match.group(1).lower(), match.group(2).strip(), text
                    raise ValueError("unparseable judge reply")
        for ln in text.split("\n")[:1]:
            m = VERDICT.match(ln.strip())
            if m and len(m.group(2).split()) < 20:
                return m.group(1).lower(), m.group(2).strip(), text
        if time.monotonic() >= deadline:
            raise TimeoutError("judge did not answer in time")
        raise ValueError(f"unparseable judge reply: {text[:200]!r}")
    finally:
        conn.close()


# Bound the whole model call, including slow headers or a trickling stream.
# A bounded pool prevents a failing model from exhausting host threads.
MODEL_SLOTS = threading.BoundedSemaphore(32)


def ask_judge(tool, tin, agent):
    if not MODEL_SLOTS.acquire(blocking=False):
        raise RuntimeError("judge overloaded")
    result = queue.Queue(maxsize=1)
    def run():
        try:
            result.put((True, ask_judge_stream(tool, tin, agent)))
        except Exception as error:
            result.put((False, error))
        finally:
            MODEL_SLOTS.release()
    threading.Thread(target=run, daemon=True).start()
    try:
        ok, answer = result.get(timeout=ARGS.timeout)
    except queue.Empty:
        raise TimeoutError("judge did not answer in time") from None
    if not ok:
        raise answer
    return answer


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
        f.flush()
        os.fsync(f.fileno())
    fd = os.open(ARGS.root, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def action_text(tool, tin):
    return tin.get("command") if tool == "Bash" else (tin.get("file_path") or json.dumps(tin, sort_keys=True))


def take_owner_allow(agent_id, tool, text, action_key):
    """Consume an unused allow for this exact call; returns its denial id or None. Caller holds LOCK."""
    rows = read_jsonl(ARGS.allow_file)
    used = {r["denial_id"] for r in rows if r.get("used_at")}
    for r in rows:
        if (not r.get("used_at") and r["denial_id"] not in used and r.get("session_id") == agent_id
                and r.get("tool") == tool and r.get("command") == text
                and r.get("action_key") == action_key):
            append(ARGS.allow_file, {"denial_id": r["denial_id"], "used_at": time.time()})
            return r["denial_id"]
    return None


def owner_allow(denial_id):
    with LOCK:
        hit = next((r for r in read_jsonl(ARGS.log) if r.get("denial_id") == denial_id), None)
        if not hit:
            return None
        # Repeated owner requests never replenish a consumed grant.
        existing = next((r for r in read_jsonl(ARGS.allow_file)
                         if r.get("denial_id") == denial_id and "allowed_at" in r), None)
        if existing:
            return existing
        rec = {"denial_id": denial_id, "session_id": hit["session_id"], "tool": hit["tool"],
               "command": hit["command"], "action_key": hit["action_key"], "allowed_at": time.time()}
        append(ARGS.allow_file, rec)
        return rec


# --- service ----------------------------------------------------------------------------------

def decide(hook, agent_id, token):
    t0 = time.time()
    with LOCK:
        agents = read_agents()
    agent = agents.get(agent_id)
    tool, tin = hook.get("tool_name") or "", hook.get("tool_input") or {}
    text = action_text(tool, tin)
    action_key = hashlib.sha256(json.dumps([tool, tin, hook.get("cwd")], sort_keys=True,
                                          separators=(",", ":")).encode()).hexdigest()
    rec = {"t": t0, "session_id": agent_id, "claude_session": hook.get("session_id"), "tool": tool,
           "command": text, "action_key": action_key, "cwd": hook.get("cwd")}
    if agent is None or not secrets.compare_digest(agent["decide_token"], token):
        stage, decision, reason = "rule", "deny", f"unknown agent {agent_id!r}"
    else:
        with LOCK:
            owner = take_owner_allow(agent_id, tool, text, action_key)
        if owner:
            stage, decision, reason = "owner", "allow", f"allowed once by the owner after denial {owner}"
        else:
            stage = "rule"
            decision, reason = rule_stage(tool, tin, agent["checkout"], agent)
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
        rec["denial_id"] = new_denial_id()
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


def read_agents():
    try:
        with open(ARGS.agents) as f:
            return json.load(f)
    except FileNotFoundError:
        return {}


def write_agents(agents):
    path = ARGS.agents + ".new"
    with open(path, "w") as f:
        json.dump(agents, f)
        f.flush()
        os.fsync(f.fileno())
    os.replace(path, ARGS.agents)
    fd = os.open(ARGS.root, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def new_denial_id():
    # Preserve the d- prefix; use 64 random bits for the durable production
    # namespace, and keep old four-hex ids valid without reusing them.
    with LOCK:
        used = {r.get("denial_id") for r in read_jsonl(os.path.join(ARGS.root, "denial-ids.jsonl"))}
        for _ in range(65536):
            candidate = "d-" + secrets.token_hex(8)
            if candidate not in used:
                # Reserve before another concurrent request chooses its id.
                append(os.path.join(ARGS.root, "denial-ids.jsonl"), {"denial_id": candidate})
                return candidate
    raise RuntimeError("unable to allocate unique denial id")


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
        self._reply(200 if self.path == "/health" else 404,
                    {"ok": True} if self.path == "/health" else {"error": "not found"})

    def do_POST(self):
        global ACTIVE
        admitted = False
        if self.path != "/decide":
            self.close_connection = True
            return self._reply(404, {"error": "not found"})
        try:
            with LOCK:
                if STOPPING:
                    raise RuntimeError("judge stopping")
                ACTIVE += 1
                admitted = True
            size = int(self.headers.get("Content-Length") or 0)
            if not 0 < size <= 2_000_000:
                raise ValueError("invalid request length")
            self.connection.settimeout(5)
            body = json.loads(self.rfile.read(size))
            if not isinstance(body, dict) or not isinstance(body.get("tool_input"), dict):
                raise ValueError("invalid tool request")
            result = decide(body, self.headers.get("X-Local-Agent") or "",
                            self.headers.get("X-Local-Judge-Token") or "")
            self._reply(200, result)
        except Exception:
            # A broken log or malformed input must never authorize an action.
            self.close_connection = True
            self._reply(200, {"hookSpecificOutput": {
                "hookEventName": "PreToolUse", "permissionDecision": "deny",
                "permissionDecisionReason": "judge unavailable; retry this command in a minute"}})
        finally:
            if admitted:
                with DRAIN:
                    ACTIVE -= 1
                    DRAIN.notify_all()


class Control(socketserver.StreamRequestHandler):
    def handle(self):
        global STOPPING
        self.request.settimeout(5)
        try:
            body = json.loads(self.rfile.readline(2_000_001))
            op = body.get("op")
            if STOPPING:
                raise RuntimeError("judge stopping")
            if op == "health":
                result = {"ok": True, "pid": os.getpid(), "port": HTTP.server_port, "generation": ARGS.generation}
            elif op == "shutdown":
                with LOCK:
                    STOPPING = True
                    STOP_EVENT.set()
                result = {"ok": True}
            elif op == "allow":
                result = owner_allow(body["denial_id"])
                if result is None:
                    raise ValueError("no such denial")
            else:
                with LOCK:
                    agents = read_agents()
                    if STOPPING:
                        raise RuntimeError("judge stopping")
                    if op == "register":
                        agent_id, agent = body["session_id"], body["agent"]
                        if not isinstance(agent_id, str) or not agent_id:
                            raise ValueError("empty session id")
                        for key in ("name", "title", "branch", "checkout", "tmp", "parent", "sm_url"):
                            if not isinstance(agent.get(key), str) or not agent[key]:
                                raise ValueError("missing " + key)
                        if not isinstance(agent.get("ticket"), int):
                            raise ValueError("missing ticket")
                        if type(agent.get("proxy_port")) is not int or not 1 <= agent["proxy_port"] <= 65535:
                            raise ValueError("missing or invalid proxy_port")
                        for key in ("checkout", "tmp"):
                            if not os.path.isabs(agent[key]) or not os.path.isdir(agent[key]):
                                raise ValueError("invalid " + key)
                            agent[key] = os.path.realpath(agent[key])
                            state = os.path.realpath(ARGS.root)
                            if state == agent[key] or state.startswith(agent[key] + os.sep):
                                raise ValueError("judge state cannot be inside agent folders")
                        agent["decide_token"] = agents.get(agent_id, {}).get("decide_token") or secrets.token_hex(32)
                        agents[agent_id] = agent
                        write_agents(agents)
                        result = {"url": f"http://127.0.0.1:{HTTP.server_port}/decide",
                                  "token": agent["decide_token"]}
                    elif op == "unregister":
                        agents.pop(body["session_id"], None)
                        write_agents(agents)
                        result = {"ok": True}
                    elif op == "registrations":
                        result = agents
                    else:
                        raise ValueError("unknown control operation")
            reply = {"result": result}
        except Exception as e:
            reply = {"error": str(e)}
        self.wfile.write(json.dumps(reply).encode() + b"\n")


class ControlServer(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True


if __name__ == "__main__":
    os.umask(0o077)
    os.makedirs(ARGS.root, mode=0o700, exist_ok=True)
    # Lifetime lock: overlapping sm restarts/start requests create one daemon.
    lifetime = open(os.path.join(ARGS.root, "service.lock"), "a")
    try:
        fcntl.flock(lifetime, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        # An idle daemon may be releasing the port/socket. Wait only for that
        # transition; do not start a duplicate beside a live service.
        until = time.monotonic() + 3
        while True:
            if os.path.exists(CONTROL) or time.monotonic() >= until:
                raise SystemExit(0)
            time.sleep(0.025)
            try:
                fcntl.flock(lifetime, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                pass
    HTTP = ThreadingHTTPServer(("127.0.0.1", ARGS.port), H)
    HTTP.daemon_threads = True
    if os.path.exists(CONTROL):
        os.unlink(CONTROL)
    control = ControlServer(CONTROL, Control)
    threading.Thread(target=control.serve_forever, daemon=True).start()
    threading.Thread(target=HTTP.serve_forever, daemon=True).start()
    # Stop after the final unregister, with a grace period for a replacement
    # agent's register-before-launch operation. Startup can also be pre-launch.
    empty_since = time.monotonic()
    while True:
        STOP_EVENT.wait(1)
        with LOCK:
            if STOPPING:
                os.unlink(CONTROL)
                break
            if read_agents():
                empty_since = time.monotonic()
            elif time.monotonic() - empty_since >= 60:
                # Close control admission while holding the registration lock.
                # A blocked register fails and the host retries through ensure.
                STOPPING = True
                os.unlink(CONTROL)
                break
    # No new decisions can enter after STOPPING. Finish and durably log every
    # admitted decision before releasing the lifetime lock for the replacement.
    with DRAIN:
        while ACTIVE:
            DRAIN.wait()
    HTTP.shutdown()
    HTTP.server_close()
    control.shutdown()
    control.server_close()
