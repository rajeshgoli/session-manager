#!/bin/zsh
# #1966: how opencode's retry behaves through a 12-minute model outage. Samples /session/status every 5 s.
K=${0:A:h}; P=${L1966_RUNS:-$HOME/.local/share/local-agents-proto/runs-1966}/proof; SID=$(python3 -c "import json;print(json.load(open('${L1966_RUNS:-$HOME/.local/share/local-agents-proto/runs-1966}/a/bridge.json'))['oc_session'])")
pkill -f 'fake_oai.py 18452'; sleep 1
$K/as_parent.sh send sm-1966-oc "LONG-OUTAGE probe sent while the model is down." > /dev/null 2>&1
for i in {1..144}; do
  print -r -- "$(date +%s) $(curl -s localhost:18451/session/status | python3 -c "import json,sys;print(json.dumps(json.load(sys.stdin).get('$SID')))")" >> $P/p4-long-status.txt
  sleep 5
done
python3 $K/fake_oai.py 18452 $K/script.json ${L1966_RUNS:-$HOME/.local/share/local-agents-proto/runs-1966}/fake_model.jsonl >> ${L1966_RUNS:-$HOME/.local/share/local-agents-proto/runs-1966}/fake_model.log 2>&1 &
sleep 60
print -r -- "$(date +%s) after restart $(curl -s localhost:18451/session/status | python3 -c "import json,sys;print(json.dumps(json.load(sys.stdin).get('$SID')))")" >> $P/p4-long-status.txt
