#!/usr/bin/env python3
"""#1966: the tmux pane sm types into for an opencode agent. Stands in for an HTTP delivery arm.

sm (unchanged) delivers to a session by typing into its tmux pane and pressing Enter, and gates
background wakes (reminders, queue completions) on the pane showing an empty Claude composer: a
last line that is exactly ">" with the cursor right after it. This pane draws that composer only
while opencode reports the session idle, so sm's readiness gate becomes "opencode is idle". Text
typed into it is buffered; Enter (CR) submits the buffer once through oc_bridge.submit with a fresh
key, so each sm delivery is exactly one opencode user turn. LF stays in the buffer as a newline.
Escape aborts the current turn, except right after C-b: sm's urgent delivery sends C-b (Claude:
move the running command to the background), Escape, then the text. opencode cannot background a
running tool, so C-b + Escape aborts nothing; the text is queued and opencode reads it at the next
step boundary, which keeps the running command alive as Claude does.

usage: oc_pane.py <bridge.json>
"""
import json, os, sys, termios, threading, time, tty, uuid
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import oc_bridge as ob

CFG = json.load(open(sys.argv[1]))
LOG = os.path.join(CFG["state_dir"], "pane.jsonl")
lock = threading.Lock()
state = {"status": "unknown", "buf": "", "lines": [], "inflight": 0}


def note(line, **kw):
    with open(LOG, "a") as f:
        f.write(json.dumps({"t": time.time(), "line": line, **kw}) + "\n")
    with lock:
        # A log line must never look like a composer row to sm's pane parser.
        state["lines"] = (state["lines"] + [line.lstrip(">❯ ")[:150]])[-30:]
    draw()


def draw():
    with lock:
        rows, cols = os.get_terminal_size()
        out = ["\x1b[H\x1b[2J", f"opencode {CFG['oc_session']}  (sm {CFG['sm_id']})\r\n"]
        body = state["lines"][-(rows - 4):]
        out += [l.replace("\n", " ")[:cols - 1] + "\r\n" for l in body]
        ready = state["status"] == "idle" and state["inflight"] == 0
        if ready or state["buf"]:
            lines = state["buf"].split("\n")
            out.append(">" + (" " + lines[0] if lines[0] else ""))
            for l in lines[1:]:
                out.append("\r\n  " + l)
        else:
            out.append(f"· opencode {state['status']}")
        sys.stdout.write("".join(out))
        sys.stdout.flush()


def poll_status():
    last = None
    while True:
        try:
            _, st = ob.http("GET", CFG["oc_url"] + "/session/status", timeout=3)
            cur = ((st or {}).get(CFG["oc_session"]) or {"type": "idle"})["type"]
        except OSError:
            cur = "unreachable"
        if cur != last:
            with lock:
                state["status"] = cur
            note(f"status {cur}")
            last = cur
        time.sleep(0.25)


def send(text, key=None):
    key = key or "pane-" + uuid.uuid4().hex[:12]
    with lock:
        state["inflight"] += 1
    try:
        mid, result = ob.submit(CFG, key, text, "pane")
        note(f"delivered {mid} ({len(text)} chars, {result}): {text[:60]}", key=key, messageID=mid)
    except BaseException as e:
        note(f"DELIVERY FAILED {key}: {e}", key=key)
    finally:
        with lock:
            state["inflight"] -= 1
        draw()


def main():
    fd = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    tty.setraw(fd)
    threading.Thread(target=poll_status, daemon=True).start()
    for rec in ob.replay_outbox(CFG):  # typed before a pane restart, never accepted
        note(f"replaying {rec['key']} from the outbox")
        threading.Thread(target=send, args=(rec["text"], rec["key"]), daemon=True).start()
    paste, ctrl_b_at = False, 0.0
    try:
        while True:
            data = os.read(fd, 4096).decode(errors="replace")
            i = 0
            while i < len(data):
                if data.startswith("\x1b[200~", i):
                    paste, i = True, i + 6
                    continue
                if data.startswith("\x1b[201~", i):
                    paste, i = False, i + 6
                    continue
                ch = data[i]
                i += 1
                if ch == "\r" and not paste:
                    with lock:
                        text, state["buf"] = state["buf"], ""
                    if text.strip():
                        threading.Thread(target=send, args=(text,), daemon=True).start()
                elif ch == "\x1b":
                    if time.time() - ctrl_b_at < 2:
                        note("C-b + escape (urgent delivery): not aborting; the text is queued")
                    else:
                        ob.http("POST", f"{CFG['oc_url']}/session/{CFG['oc_session']}/abort", {})
                        note("escape: aborted the current turn")
                elif ch in ("\x7f", "\x08"):
                    with lock:
                        state["buf"] = state["buf"][:-1]
                elif ch == "\x03":
                    raise KeyboardInterrupt
                elif ch == "\x02":
                    ctrl_b_at = time.time()  # sm's urgent path: C-b, then Escape
                else:
                    with lock:
                        state["buf"] += "\n" if ch in ("\n", "\r") else ch
            draw()
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)


if __name__ == "__main__":
    main()
