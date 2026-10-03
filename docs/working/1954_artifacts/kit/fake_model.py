#!/usr/bin/env python3
"""#1954 step 1: a scripted Anthropic /v1/messages server, so the hook check needs no model.

Each main-loop request (one carrying tools) gets the next scripted tool call, chosen by how many
tool_use blocks the conversation already holds; after the script it ends the turn.
usage: fake_model.py <port> <script.json>   script: [{"tool": "Bash", "input": {...}}, ...]
"""
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]); SCRIPT = json.load(open(sys.argv[2]))


def sse(events):
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()


def reply(block, stop):
    msg = {"id": "msg_fake", "type": "message", "role": "assistant", "model": "fake", "content": [],
           "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 10, "output_tokens": 1}}
    ev = [{"type": "message_start", "message": msg}]
    if block["type"] == "tool_use":
        ev.append({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "tool_use", "id": block["id"], "name": block["name"], "input": {}}})
        ev.append({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "input_json_delta", "partial_json": json.dumps(block["input"])}})
    else:
        ev.append({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
        ev.append({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": block["text"]}})
    ev += [{"type": "content_block_stop", "index": 0},
           {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None}, "usage": {"output_tokens": 5}},
           {"type": "message_stop"}]
    return sse(ev)


class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass

    def do_GET(self):
        self._send(200, b'{"data":[{"id":"fake","type":"model"}]}', "application/json")

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        if "count_tokens" in self.path:
            return self._send(200, b'{"input_tokens":100}', "application/json")
        if not body.get("tools"):
            return self._send(200, reply({"type": "text", "text": "ok"}, "end_turn"), "text/event-stream")
        done = sum(1 for m in body.get("messages", []) if m.get("role") == "assistant"
                   for c in (m.get("content") if isinstance(m.get("content"), list) else []) if c.get("type") == "tool_use")
        if done < len(SCRIPT):
            s = SCRIPT[done]
            block = {"type": "tool_use", "id": f"toolu_{done}", "name": s["tool"], "input": s["input"]}
            print(f"step {done}: {s['tool']} {json.dumps(s['input'])[:120]}", flush=True)
            return self._send(200, reply(block, "tool_use"), "text/event-stream")
        print("script done", flush=True)
        self._send(200, reply({"type": "text", "text": "SCRIPT-DONE"}, "end_turn"), "text/event-stream")

    def _send(self, code, data, ctype):
        self.send_response(code); self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
