#!/usr/bin/env python3
"""#1966: opencode <-> sm bridge. Stands in for the opencode provider arm sm does not have yet.

Subcommands
  brief    Submit the spawn brief exactly once and wait for opencode's acknowledgement.
  submit   Submit one message exactly once (used by oc_pane.py for sm deliveries).
  project  Follow opencode's event stream and project it into sm through sm's existing hook routes:
           busy -> /hooks/claude UserPromptSubmit, idle -> /hooks/claude Stop, tool running ->
           /hooks/tool-use PreToolUse, and one Claude-format usage line per model request
           (opencode step-finish) appended to the transcript sm's usage ledger scans.

Exactly once: every submission has a key (brief: sha256 of the text; pane: a fresh uuid per Enter).
The first attempt writes {key, messageID, partID} to the ledger (state_dir/ledger.jsonl) BEFORE it
posts, and every retry reuses those ids. Before posting, the bridge asks opencode whether that
messageID already exists; if it does, the submission is already accepted and nothing is sent.
opencode itself does not de-duplicate a reposted messageID: it appends the text again (x1a), and
with a fixed partID it overwrites the part instead (x1b); the existence check makes either moot.

Config is one JSON file (state_dir/bridge.json):
  {"oc_url", "oc_session", "sm_url", "sm_id", "state_dir", "model_label", "context_window",
   "cwd"}
"""
import argparse, datetime, hashlib, json, os, secrets, string, sys, time, urllib.error, urllib.request

B62 = string.digits + string.ascii_uppercase + string.ascii_lowercase
_last_ms, _counter = 0, 0


def oc_id(prefix):
    """opencode's ascending id: 12 hex chars of (ms * 0x1000 + counter) mod 2^48, then 14 base62."""
    global _last_ms, _counter
    ms = int(time.time() * 1000)
    _counter = _counter + 1 if ms == _last_ms else 1
    _last_ms = ms
    n = (ms * 0x1000 + _counter) & ((1 << 48) - 1)
    return f"{prefix}_{n:012x}" + "".join(secrets.choice(B62) for _ in range(14))


def now_iso():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def http(method, url, body=None, timeout=10):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        return e.code, None


def log(cfg, kind, **kw):
    rec = {"t": time.time(), "kind": kind, **kw}
    with open(os.path.join(cfg["state_dir"], "bridge_events.jsonl"), "a") as f:
        f.write(json.dumps(rec) + "\n")
    return rec


# --- exactly-once submission --------------------------------------------------------------------

def ledger_get(cfg, key):
    path = os.path.join(cfg["state_dir"], "ledger.jsonl")
    if os.path.exists(path):
        for line in open(path):
            r = json.loads(line)
            if r["key"] == key and "messageID" in r:
                return r
    return None


def submit(cfg, key, text, source, give_up_after=None):
    """Post `text` once under `key`. Returns (messageID, 'sent' | 'already-accepted').

    The ledger entry, text included, is written before the first attempt: it is the durable outbox.
    Attempts repeat until opencode accepts (or `give_up_after` seconds pass); `replay_outbox` resends
    any entry without an "accepted" record after the bridge or pane itself restarts."""
    rec = ledger_get(cfg, key)
    if rec is None:
        rec = {"key": key, "messageID": oc_id("msg"), "partID": oc_id("prt"), "source": source,
               "chars": len(text), "text": text, "t": time.time()}
        ledger_append(cfg, rec)
    base = f"{cfg['oc_url']}/session/{cfg['oc_session']}"
    t0, attempt = time.time(), 0
    while give_up_after is None or time.time() - t0 < give_up_after:
        try:
            code, _ = http("GET", f"{base}/message/{rec['messageID']}")
            if code == 200:
                result = "already-accepted"
            else:
                code, _ = http("POST", f"{base}/prompt_async",
                               {"messageID": rec["messageID"],
                                "parts": [{"id": rec["partID"], "type": "text", "text": text}]})
                result = "sent" if code in (200, 204) else None
            if result:
                ledger_append(cfg, {"key": key, "accepted": time.time(), "result": result})
                log(cfg, "submit", key=key, messageID=rec["messageID"], result=result, attempt=attempt,
                    source=source)
                return rec["messageID"], result
        except OSError as e:
            if attempt % 10 == 0:
                log(cfg, "submit-retry", key=key, error=str(e)[:200], attempt=attempt)
        attempt += 1
        time.sleep(1)
    raise SystemExit(f"submit {key}: opencode did not accept within {give_up_after} s")


def ledger_append(cfg, rec):
    with open(os.path.join(cfg["state_dir"], "ledger.jsonl"), "a") as f:
        f.write(json.dumps(rec) + "\n")


def replay_outbox(cfg):
    """Entries written but never accepted, oldest first (a crash between outbox write and accept)."""
    path = os.path.join(cfg["state_dir"], "ledger.jsonl")
    if not os.path.exists(path):
        return []
    recs = [json.loads(l) for l in open(path) if l.strip()]
    done = {r["key"] for r in recs if "accepted" in r}
    return [r for r in recs if "messageID" in r and r["key"] not in done and "text" in r]


def wait_ack(cfg, message_id, timeout=30):
    """Acknowledged = opencode returns the user message by its id, carrying the text part."""
    t0 = time.time()
    while time.time() - t0 < timeout:
        code, msg = http("GET", f"{cfg['oc_url']}/session/{cfg['oc_session']}/message/{message_id}")
        if code == 200 and msg and msg["info"]["role"] == "user" and msg["parts"]:
            return round(time.time() - t0, 3)
        time.sleep(0.1)
    return None


# --- projection into sm ---------------------------------------------------------------------------

def sm_post(cfg, path, body):
    try:
        code, out = http("POST", cfg["sm_url"] + path, body, timeout=5)
    except OSError as e:
        code, out = None, str(e)
    log(cfg, "sm", path=path, event=body.get("hook_event_name") or body.get("event"), code=code)
    return code


def last_text(cfg, role):
    code, msgs = http("GET", f"{cfg['oc_url']}/session/{cfg['oc_session']}/message")
    for m in reversed(msgs or []):
        if m["info"]["role"] == role:
            t = "\n".join(p.get("text", "") for p in m["parts"] if p.get("type") == "text" and not p.get("synthetic"))
            if t.strip():
                return t.strip()
    return ""


def transcript_path(cfg):
    return os.path.join(cfg["state_dir"], "transcript", f"{cfg['oc_session']}.jsonl")


def usage_line(cfg, part, model_id):
    """One Claude-format transcript line per model request, as sm's ledger parser reads it."""
    tk = part.get("tokens") or {}
    cache = tk.get("cache") or {}
    return {"type": "assistant", "timestamp": now_iso(), "sessionId": cfg["oc_session"], "cwd": cfg["cwd"],
            "requestId": part["id"],
            "message": {"id": part["id"], "model": model_id, "role": "assistant", "content": [],
                        "usage": {"input_tokens": tk.get("input", 0),
                                  "output_tokens": tk.get("output", 0) + tk.get("reasoning", 0),
                                  "cache_read_input_tokens": cache.get("read", 0),
                                  "cache_creation_input_tokens": cache.get("write", 0)}}}


def project(cfg):
    os.makedirs(os.path.dirname(transcript_path(cfg)), exist_ok=True)
    seen_steps, models = set(), {}
    tp = transcript_path(cfg)
    if os.path.exists(tp):
        seen_steps = {json.loads(l)["requestId"] for l in open(tp) if l.strip()}
    status = None
    while True:
        try:
            req = urllib.request.Request(cfg["oc_url"] + "/event")
            with urllib.request.urlopen(req, timeout=None) as resp:
                log(cfg, "event-stream", state="connected")
                # Resynchronise on (re)connect: whatever status opencode reports now is projected.
                code, st = http("GET", cfg["oc_url"] + "/session/status")
                cur = ((st or {}).get(cfg["oc_session"]) or {"type": "idle"})["type"]
                status = on_status(cfg, status, cur, resync=True)
                for raw in resp:
                    if not raw.startswith(b"data:"):
                        continue
                    ev = json.loads(raw[5:])
                    p = ev.get("properties") or {}
                    if p.get("sessionID") != cfg["oc_session"]:
                        continue
                    t = ev.get("type")
                    if t == "session.status":
                        status = on_status(cfg, status, p["status"]["type"])
                    elif t == "message.updated" and p["info"]["role"] == "assistant":
                        models[p["info"]["id"]] = f"{p['info'].get('providerID')}/{p['info'].get('modelID')}"
                    elif t == "message.part.updated":
                        part = p["part"]
                        if part.get("type") == "tool" and part["state"]["status"] == "running":
                            sm_post(cfg, "/hooks/tool-use", {
                                "hook_event_name": "PreToolUse", "session_manager_id": cfg["sm_id"],
                                "session_id": cfg["oc_session"], "tool_name": part["tool"],
                                "tool_input": part["state"].get("input") or {}, "tool_use_id": part["callID"],
                                "cwd": cfg["cwd"]})
                        elif part.get("type") == "step-finish" and part["id"] not in seen_steps:
                            seen_steps.add(part["id"])
                            label = cfg.get("model_label") or models.get(part["messageID"], "local")
                            line = usage_line(cfg, part, label)
                            with open(tp, "a") as f:
                                # Compact: sm's parser pre-filters on the bytes "usage":{ with no space.
                                f.write(json.dumps(line, separators=(",", ":")) + "\n")
                            tk = part.get("tokens") or {}
                            ctx = tk.get("input", 0) + (tk.get("cache") or {}).get("read", 0) + \
                                (tk.get("cache") or {}).get("write", 0)
                            win = cfg.get("context_window", 200000)
                            sm_post(cfg, "/hooks/context-usage", {
                                "session_id": cfg["sm_id"], "used_percentage": round(100 * ctx / win, 1),
                                "total_input_tokens": ctx, "context_window_tokens": win, "model_id": label,
                                "sm_hook_emitted_at": now_iso()})
                    elif t == "session.compacted":
                        sm_post(cfg, "/hooks/context-usage", {"session_id": cfg["sm_id"],
                                                              "event": "compaction_complete"})
        except (OSError, ValueError) as e:
            log(cfg, "event-stream", state="lost", error=str(e)[:200])
            time.sleep(1)


def on_status(cfg, prev, cur, resync=False):
    """busy/retry count as working. Each transition is projected once; a resync re-sends the current state."""
    working = cur in ("busy", "retry")
    was = None if prev is None else prev in ("busy", "retry")
    if was == working and not resync:
        return cur
    hook = {"session_manager_id": cfg["sm_id"], "session_id": cfg["oc_session"],
            "sm_hook_emitted_at": now_iso(), "transcript_path": transcript_path(cfg)}
    if working:
        hook.update(hook_event_name="UserPromptSubmit", prompt=last_text(cfg, "user"))
    else:
        hook.update(hook_event_name="Stop", last_assistant_message=last_text(cfg, "assistant"))
    log(cfg, "status", opencode=cur, projected=hook["hook_event_name"], resync=resync)
    sm_post(cfg, "/hooks/claude", hook)
    return cur


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["brief", "submit", "project"])
    ap.add_argument("config")
    ap.add_argument("--file")
    ap.add_argument("--text")
    ap.add_argument("--key")
    a = ap.parse_args()
    cfg = json.load(open(a.config))
    if a.cmd == "project":
        return project(cfg)
    text = open(a.file).read() if a.file else a.text
    key = a.key or ("brief-" + hashlib.sha256(text.encode()).hexdigest()[:16])
    mid, result = submit(cfg, key, text, a.cmd)
    ack = wait_ack(cfg, mid)
    print(json.dumps({"key": key, "messageID": mid, "result": result, "ack_seconds": ack}))
    sys.exit(0 if ack is not None else 1)


if __name__ == "__main__":
    main()
