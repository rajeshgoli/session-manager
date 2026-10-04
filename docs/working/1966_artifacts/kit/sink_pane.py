#!/usr/bin/env python3
"""#1966: the scratch parent sm-1966's pane. Always shows an empty Claude composer (">" with the
cursor after it) so sm delivers to it, and appends each submitted message to <log.jsonl>.
usage: sink_pane.py <log.jsonl>"""
import json, os, sys, termios, time, tty

LOG = sys.argv[1]
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
tty.setraw(fd)
buf, n = "", 0
try:
    while True:
        sys.stdout.write(f"\x1b[H\x1b[2Jscratch parent sm-1966: {n} messages received\r\n>")
        sys.stdout.flush()
        for ch in os.read(fd, 4096).decode(errors="replace"):
            if ch == "\r":
                if buf.strip():
                    with open(LOG, "a") as f:
                        f.write(json.dumps({"t": time.time(), "text": buf}) + "\n")
                    n += 1
                buf = ""
            elif ch == "\x03":
                raise KeyboardInterrupt
            elif ch not in ("\x1b", "\x02"):
                buf += ch
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
