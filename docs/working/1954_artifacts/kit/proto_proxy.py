#!/usr/bin/env python3
"""#1926 prototype proxy: harness -> :1235 -> LM Studio :1234.

Logs one JSON line per model request (start, time to first token, end, usage,
prompt size) and records sm hook posts (/hooks/*) instead of forwarding them,
so prototype sessions never reach the live sm server.

usage: proto_proxy.py <log.jsonl> [listen_port] [upstream_port]
"""
import hashlib
import http.client
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = sys.argv[1]
# Request bodies are saved here (one file per model request) so turns can be diffed.
DUMP = os.path.join(os.path.dirname(os.path.abspath(LOG)), "bodies")
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 1235
UP = int(sys.argv[3]) if len(sys.argv) > 3 else 1234
lock = threading.Lock()
seq = 0


def emit(rec):
    with lock:
        with open(LOG, "a") as f:
            f.write(json.dumps(rec) + "\n")


def prefix_hashes(body):
    """Hash of each message prefix, to see whether turn N extends turn N-1 exactly."""
    msgs = body.get("messages") or []
    h = hashlib.sha1(json.dumps(body.get("system", ""), sort_keys=True).encode())
    out = []
    for m in msgs:
        h.update(json.dumps(m, sort_keys=True).encode())
        out.append(h.hexdigest()[:10])
    return out


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def _reply(self, code, payload):
        data = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self._forward(b"")

    def do_POST(self):
        raw = self._body()
        if self.path.startswith("/hooks/"):
            try:
                body = json.loads(raw or b"{}")
            except ValueError:
                body = {"unparsed": raw[:200].decode("utf-8", "replace")}
            emit({"kind": "hook", "t": time.time(), "path": self.path,
                  "event": body.get("hook_event_name") or body.get("event") or body.get("tool_name"),
                  "keys": sorted(body)[:20], "tool": body.get("tool_name"),
                  "input": json.dumps(body.get("tool_input"))[:300] if body.get("tool_input") else None})
            return self._reply(200, {})
        self._forward(raw)

    def _forward(self, raw):
        global seq
        with lock:
            seq += 1
            n = seq
        rec = {"kind": "model", "n": n, "path": self.path, "t_start": time.time(),
               "req_bytes": len(raw)}
        if len(raw) >= 5000:
            os.makedirs(DUMP, exist_ok=True)
            with open(os.path.join(DUMP, f"{n:05d}.json"), "wb") as f:
                f.write(raw)
        try:
            body = json.loads(raw) if raw else {}
            rec.update(model=body.get("model"), stream=body.get("stream"),
                       n_messages=len(body.get("messages") or []),
                       n_tools=len(body.get("tools") or []),
                       prefix=prefix_hashes(body)[-3:])
        except ValueError:
            pass
        hdrs = {k: v for k, v in self.headers.items()
                if k.lower() not in ("host", "content-length", "connection", "accept-encoding")}
        conn = http.client.HTTPConnection("127.0.0.1", UP, timeout=3600)
        try:
            conn.request(self.command, self.path, body=raw or None, headers=hdrs)
            resp = conn.getresponse()
        except Exception as e:  # upstream down or model unloaded
            rec.update(error=str(e), t_end=time.time())
            emit(rec)
            return self._reply(502, {"error": str(e)})
        rec["status"] = resp.status
        self.send_response(resp.status)
        for k, v in resp.getheaders():
            if k.lower() not in ("transfer-encoding", "connection", "content-length"):
                self.send_header(k, v)
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        usage = {}
        buf = b""
        try:
            while True:
                chunk = resp.read1(65536)
                if not chunk:
                    break
                buf += chunk
                if "t_first_token" not in rec and (b"_delta" in chunk or b'"delta"' in chunk):
                    rec["t_first_token"] = time.time()
                self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n")
        except (BrokenPipeError, ConnectionResetError) as e:
            rec["client_error"] = str(e)
        rec["t_end"] = time.time()
        # Usage: Anthropic SSE (message_start / message_delta), OpenAI SSE final chunk, or plain JSON.
        for line in buf.split(b"\n"):
            line = line.strip()
            if line.startswith(b"data:"):
                line = line[5:].strip()
            if not line.startswith(b"{"):
                continue
            try:
                ev = json.loads(line)
            except ValueError:
                continue
            for u in (ev.get("usage"), (ev.get("message") or {}).get("usage")):
                if isinstance(u, dict):
                    usage.update({k: v for k, v in u.items() if v is not None})
            if ev.get("type") == "message_delta":
                rec["stop_reason"] = (ev.get("delta") or {}).get("stop_reason")
        rec["usage"] = usage
        rec["resp_bytes"] = len(buf)
        emit(rec)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
