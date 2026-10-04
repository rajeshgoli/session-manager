#!/bin/zsh
# #1966 proof 3: sm send, a reminder and a queue completion reach the opencode agent once each.
# usage: prove_p3.sh <tag> <busy|idle>
#   busy: the agent runs a 20 s command (PROBE-SLOW); all three arrive during it.
#   idle: the agent is idle; the three arrive a few seconds apart.
# Prints the opencode conversation since t0 and sm's view of the agent; evidence in runs/proof/p3-<tag>*.
set -u
K=${0:A:h} TAG=$1 MODE=$2
RU=${L1966_RUNS:-$HOME/.local/share/local-agents-proto/runs-1966} P=${L1966_RUNS:-$HOME/.local/share/local-agents-proto/runs-1966}/proof R=${L1966_RUNS:-$HOME/.local/share/local-agents-proto/runs-1966}/a
python3 $K/smwatch.py $P/p3-$TAG-view.jsonl 70 & W=$!
python3 -c 'import time;print(time.time())' > $P/p3-$TAG-t0
if [[ $MODE == busy ]]; then
  $K/as_parent.sh send sm-1966-oc "PROBE-SLOW $TAG run the slow command." > /dev/null 2>&1
  sleep 2
fi
$K/as_agent.sh remind 4 "REMINDER-PROBE $TAG check your progress" > $P/p3-$TAG-remind.txt 2>&1
$K/as_agent.sh queue run --type tests --label 1966-wake-$TAG --cwd /private/tmp/l1966-a -- sleep 2 > $P/p3-$TAG-queue.txt 2>&1
sleep 3
$K/as_parent.sh send sm-1966-oc "SEND-PROBE $TAG plain sm send." > $P/p3-$TAG-send.txt 2>&1
sleep 45; kill $W 2>/dev/null
python3 - $R $P $TAG <<'EOF'
import json, sys, urllib.request
R, P, tag = sys.argv[1:]
cfg = json.load(open(f"{R}/bridge.json"))
ms = json.load(urllib.request.urlopen(f"{cfg['oc_url']}/session/{cfg['oc_session']}/message"))
json.dump(ms, open(f"{P}/p3-{tag}-messages.json", "w"))
t0 = float(open(f"{P}/p3-{tag}-t0").read())
users = 0
for m in ms:
    i = m["info"]
    if i["time"]["created"] / 1000 < t0 - 1:
        continue
    at = round(i["time"]["created"] / 1000 - t0, 1)
    if i["role"] == "user":
        users += 1
        print(at, "USER", " ".join(p.get("text", "") for p in m["parts"] if p["type"] == "text").replace("\n", " | ")[:100])
    else:
        print(at, "  asst", [(p.get("tool"), (p.get("state") or {}).get("status"), round(((p.get("state") or {}).get("time") or {}).get("end", 0) / 1000 - t0, 1)) if p["type"] == "tool" else p.get("text", "")[:50] for p in m["parts"] if p["type"] in ("tool", "text")])
print("user turns since t0:", users)
for l in open(f"{P}/p3-{tag}-view.jsonl"):
    r = json.loads(l); print("sm", round(r["t"] - t0, 1), r.get("status"), r.get("activity_state"))
for l in open(f"{R}/pane.jsonl"):
    r = json.loads(l)
    if r["t"] > t0 - 1: print("pane", round(r["t"] - t0, 1), r["line"][:90].replace("\n", " | "))
EOF
