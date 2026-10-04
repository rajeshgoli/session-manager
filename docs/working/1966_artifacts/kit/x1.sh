#!/bin/zsh
# #1966 experiment 1: plugin loading under --pure, event types, duplicate prompt_async messageID.
# usage: x1.sh <fresh dir> <pure|nopure>
set -u
K=${0:A:h}; X=$1; MODE=$2
mkdir -p $X/cfg/opencode/plugins $X/data $X/cache $X/state $X/proj
cd $X/proj && git init -q && echo hi > a.txt && git add . && git -c user.name=t -c user.email=t@t commit -qm init; cd $X
echo '[{"tool":"bash","args":{"command":"echo hello-from-tool","description":"say hello"}}]' > script.json
cat > cfg/opencode/opencode.json <<'EOF'
{"$schema":"https://opencode.ai/config.json","autoupdate":false,"share":"disabled","snapshot":false,
 "provider":{"fake":{"npm":"@ai-sdk/openai-compatible","name":"fake","options":{"baseURL":"http://127.0.0.1:18472/v1","apiKey":"x"},"models":{"m":{"name":"m","limit":{"context":200000,"output":8000}}}}},
 "model":"fake/m","small_model":"fake/m","enabled_providers":["fake"],
 "permission":{"*":"allow","question":"deny","doom_loop":"allow","external_directory":"allow","webfetch":"deny","websearch":"deny","task":"deny"}}
EOF
cat > cfg/opencode/plugins/probe.js <<'EOF'
import { appendFileSync } from "fs";
export const Probe = async () => ({
  "tool.execute.before": async (input, output) => { appendFileSync(process.env.PROBE_LOG, JSON.stringify({hook:"before", input, args: output.args})+"\n"); },
  event: async ({event}) => { appendFileSync(process.env.PROBE_LOG, JSON.stringify({hook:"event", type: event.type})+"\n"); },
});
EOF
python3 $K/fake_oai.py 18472 script.json model.jsonl > fake.log 2>&1 & FP=$!
export XDG_CONFIG_HOME=$X/cfg XDG_DATA_HOME=$X/data XDG_CACHE_HOME=$X/cache XDG_STATE_HOME=$X/state OPENCODE_DISABLE_AUTOUPDATE=1 PROBE_LOG=$X/probe.log
FLAG=--pure; [[ $MODE == nopure ]] && FLAG=
(cd proj && opencode serve $FLAG --port 18473 > ../serve.log 2>&1) & OP=$!
sleep 5
curl -s -N localhost:18473/event > events.jsonl & EP=$!
SID=$(curl -s -XPOST localhost:18473/session -H 'content-type: application/json' -d '{}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])'); echo SID=$SID
B='{"messageID":"msg_0000000000001testbrief01","parts":[{"id":"prt_0000000000001testbriefp1","type":"text","text":"brief one"}]}'
curl -s -o /dev/null -w 'first %{http_code}\n' -XPOST localhost:18473/session/$SID/prompt_async -H 'content-type: application/json' -d "$B"
sleep 4
curl -s -o /dev/null -w 'repeat %{http_code}\n' -XPOST localhost:18473/session/$SID/prompt_async -H 'content-type: application/json' -d "$B"
sleep 4
kill $EP
curl -s localhost:18473/session/$SID/message > messages.json
python3 - <<'EOF'
import json
for m in json.load(open("messages.json")):
    i = m["info"]
    print(i["role"], i["id"], [(p["type"], (p.get("text") or p.get("tool") or "")[:40]) for p in m["parts"]], i.get("tokens"))
EOF
echo "probe hooks:"; cut -c1-160 probe.log | grep -v '"event"' ; grep -c '"event"' probe.log
echo "event types:"; grep -o '"type":"[a-z.]*"' events.jsonl | sort | uniq -c
kill $OP $FP 2>/dev/null; pkill -f "opencode serve.*18473"; true
