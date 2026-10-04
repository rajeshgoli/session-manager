#!/usr/bin/env python3
"""#1966: scripted OpenAI-compatible /v1/chat/completions server, so opencode lifecycle checks need no model.

Each request carrying tools gets the next scripted step for the *current user turn*: the number of
assistant tool calls after the last user message picks the step. A step is {"tool": name, "args": {...}}
or {"text": "..."}. The script is a list, or a dict of lists keyed by a marker: the first
key found in the user text picks the list, else "default". Past the script the turn ends with text "done: <last user text first 40 chars>".
Every request is appended to <log> as one JSON line (time, n_messages, last user text, step).
usage: fake_oai.py <port> <script.json> <log.jsonl>
"""
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]); SCRIPT = json.load(open(sys.argv[2])); LOG = sys.argv[3]


def chunk(delta, finish=None, usage=None):
    c = {"id": "chatcmpl-fake", "object": "chat.completion.chunk", "created": int(time.time()), "model": "fake",
         "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
    if usage:
        c["usage"] = usage
    return f"data: {json.dumps(c)}\n\n"


def text_of(m):
    c = m.get("content")
    if isinstance(c, list):
        return " ".join(p.get("text", "") for p in c if isinstance(p, dict))
    return c or ""


class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass

    def do_GET(self):
        self._send(200, json.dumps({"object": "list", "data": [{"id": "fake", "object": "model"}]}).encode())

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        msgs = body.get("messages", [])
        last_user = max((i for i, m in enumerate(msgs) if m.get("role") == "user"), default=-1)
        utext = text_of(msgs[last_user]) if last_user >= 0 else ""
        done = sum(len(m.get("tool_calls") or []) for m in msgs[last_user + 1:] if m.get("role") == "assistant")
        steps = SCRIPT if isinstance(SCRIPT, list) else next(
            (v for k, v in SCRIPT.items() if k != "default" and k in utext), SCRIPT.get("default", []))
        step = steps[done] if body.get("tools") and done < len(steps) else {"text": "done: " + utext[:40]}
        if not body.get("tools"):
            step = {"text": "title"}
        with open(LOG, "a") as f:
            f.write(json.dumps({"t": time.time(), "n": len(msgs), "tools": bool(body.get("tools")),
                                "user": utext[:200], "step": step}) + "\n")
        usage = {"prompt_tokens": 1000 + 10 * len(msgs), "completion_tokens": 7, "total_tokens": 1007 + 10 * len(msgs),
                 "prompt_tokens_details": {"cached_tokens": 900}}
        out = chunk({"role": "assistant", "content": ""})
        if "tool" in step:
            out += chunk({"tool_calls": [{"index": 0, "id": f"call_{int(time.time()*1000)}", "type": "function",
                                          "function": {"name": step["tool"], "arguments": json.dumps(step["args"])}}]})
            out += chunk({}, "tool_calls")
        else:
            out += chunk({"content": step["text"]})
            out += chunk({}, "stop")
        out += chunk({}, None, usage) if body.get("stream_options", {}).get("include_usage") else ""
        out += "data: [DONE]\n\n"
        if body.get("stream"):
            return self._send(200, out.encode(), "text/event-stream")
        msg = {"role": "assistant", "content": step.get("text")}
        if "tool" in step:
            msg["tool_calls"] = [{"id": "call_x", "type": "function",
                                  "function": {"name": step["tool"], "arguments": json.dumps(step["args"])}}]
        self._send(200, json.dumps({"id": "x", "object": "chat.completion", "model": "fake", "usage": usage,
                                    "choices": [{"index": 0, "message": msg,
                                                 "finish_reason": "tool_calls" if "tool" in step else "stop"}]}).encode())

    def _send(self, code, data, ctype="application/json"):
        self.send_response(code); self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
