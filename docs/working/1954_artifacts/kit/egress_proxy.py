#!/usr/bin/env python3
"""#1954 egress proxy (memo K.2): HTTPS CONNECT on loopback to github.com and api.github.com only.

Everything else gets 403. One JSON line per connection: time, agent (from the proxy URL's user
part, e.g. HTTPS_PROXY=http://proto-1855@127.0.0.1:8432), target, allowed, bytes each way.
usage: egress_proxy.py <log.jsonl> [port]
"""
import base64, json, select, socket, sys, threading, time

LOG, PORT = sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 8432
ALLOWED = {("github.com", 443), ("api.github.com", 443)}
lock = threading.Lock()


def emit(rec):
    with lock, open(LOG, "a") as f:
        f.write(json.dumps(rec) + "\n")


def handle(c):
    t0, rec = time.time(), {"t": time.time()}
    try:
        head = b""
        while b"\r\n\r\n" not in head and len(head) < 16384:
            chunk = c.recv(4096)
            if not chunk:
                return
            head += chunk
        lines = head.split(b"\r\n")
        method, target = lines[0].split(b" ")[:2]
        agent = "unknown"
        for l in lines[1:]:
            if l.lower().startswith(b"proxy-authorization: basic "):
                try:
                    agent = base64.b64decode(l.split(b" ", 2)[2]).decode().split(":")[0]
                except Exception:
                    pass
        rec.update(agent=agent, method=method.decode(), target=target.decode()[:200])
        if method != b"CONNECT":
            rec["allowed"] = False
            c.sendall(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            return
        host, _, port = target.decode().rpartition(":")
        if (host.lower(), int(port or 0)) not in ALLOWED:
            rec["allowed"] = False
            c.sendall(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            return
        rec["allowed"] = True
        u = socket.create_connection((host, int(port)), timeout=30)
        c.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
        up = down = 0
        socks = [c, u]
        while True:
            r, _, _ = select.select(socks, [], [], 300)
            if not r:
                break
            for s in r:
                data = s.recv(65536)
                if not data:
                    raise EOFError
                (u if s is c else c).sendall(data)
                if s is c: up += len(data)
                else: down += len(data)
    except EOFError:
        pass
    except Exception as e:
        rec["error"] = f"{type(e).__name__}: {e}"[:200]
    finally:
        rec["secs"] = round(time.time() - t0, 2)
        rec.update(up=locals().get("up"), down=locals().get("down"))
        emit(rec)
        c.close()


s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", PORT)); s.listen(64)
print(f"egress proxy on 127.0.0.1:{PORT}", flush=True)
while True:
    conn, _ = s.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
