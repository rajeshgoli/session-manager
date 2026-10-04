#!/bin/zsh
# #1966 proof 4: the same conversation and local provider survive an opencode restart and a model restart.
# usage: prove_p4.sh <tag> <process|model|all> [model restart command]
#   process  kill opencode serve; sm send during the outage; relaunch on the same XDG data dir.
#   all      also kill the bridge and the pane (as after a reboot); relaunch everything.
#   model    stop the model; sm send; start the model again 25 s later.
#            The model stop/start commands come from $STOP_MODEL / $START_MODEL.
set -u
K=${0:A:h} TAG=$1 MODE=$2
P=${L1966_RUNS:-$HOME/.local/share/local-agents-proto/runs-1966}/proof R=${L1966_RUNS:-$HOME/.local/share/local-agents-proto/runs-1966}/a
MODEL_URL=${MODEL_URL:-http://127.0.0.1:18452/v1}
cfg() { python3 -c "import json;print(json.load(open('$R/bridge.json'))['$1'])"; }
SID=$(cfg oc_session)
count() { curl -s localhost:18451/session/$SID/message | python3 -c 'import json,sys;ms=json.load(sys.stdin);print(len(ms), sum(m["info"]["role"]=="user" for m in ms))'; }
python3 -c 'import time;print(time.time())' > $P/p4-$TAG-t0
echo "before: session $SID, messages/user turns $(count)" | tee $P/p4-$TAG.txt
python3 $K/smwatch.py $P/p4-$TAG-view.jsonl 120 & W=$!
case $MODE in
  process|all)
    tmux -L l1966 kill-session -t oc
    [[ $MODE == all ]] && { tmux -L l1966 kill-session -t bridge; tmux -L l1966-scratch kill-session -t l1966-a1966oc0; }
    sleep 2
    [[ $MODE == process ]] && $K/as_parent.sh send sm-1966-oc "RESTORE-PROBE $TAG sent while opencode is down." >> $P/p4-$TAG.txt 2>&1
    sleep 5
    zsh $K/run_oc.sh $MODEL_URL restart >> $P/p4-$TAG.txt 2>&1
    [[ $MODE == all ]] && { sleep 3; $K/as_parent.sh send sm-1966-oc "RESTORE-PROBE $TAG after full restart." >> $P/p4-$TAG.txt 2>&1; }
    ;;
  model)
    eval $STOP_MODEL
    sleep 2
    $K/as_parent.sh send sm-1966-oc "RESTORE-PROBE $TAG sent while the model is down." >> $P/p4-$TAG.txt 2>&1
    sleep 25
    eval $START_MODEL
    ;;
esac
sleep 30; kill $W 2>/dev/null
echo "after: session $(cfg oc_session), messages/user turns $(count)" | tee -a $P/p4-$TAG.txt
python3 - $R $P $TAG <<'EOF'
import json, sys, urllib.request
R, P, tag = sys.argv[1:]
cfg = json.load(open(f"{R}/bridge.json"))
t0 = float(open(f"{P}/p4-{tag}-t0").read())
ms = json.load(urllib.request.urlopen(f"{cfg['oc_url']}/session/{cfg['oc_session']}/message"))
for m in ms:
    i = m["info"]
    if i["time"]["created"] / 1000 < t0 - 1:
        continue
    err = (i.get("error") or {}).get("data", {}).get("message", "") if i["role"] == "assistant" else ""
    print(round(i["time"]["created"] / 1000 - t0, 1), i["role"], i.get("providerID", ""), i.get("modelID", ""),
          " ".join(p.get("text", "") for p in m["parts"] if p["type"] == "text").replace("\n", " | ")[:90], err[:80])
for l in open(f"{P}/p4-{tag}-view.jsonl"):
    r = json.loads(l); print("sm", round(r["t"] - t0, 1), r.get("status"), r.get("activity_state"))
for l in open(f"{R}/pane.jsonl"):
    r = json.loads(l)
    if r["t"] > t0 - 1: print("pane", round(r["t"] - t0, 1), r["line"][:90].replace("\n", " | "))
for l in open(f"{R}/bridge_events.jsonl"):
    r = json.loads(l)
    if r["t"] > t0 - 1 and r["kind"] in ("event-stream", "status"): print("bridge", round(r["t"] - t0, 1), {k: v for k, v in r.items() if k not in ("t",)})
EOF
