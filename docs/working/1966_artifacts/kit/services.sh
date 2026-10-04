#!/bin/zsh
# #1966: every service outside the wall in one queue job, so the proof takes one queue slot.
#   scratch sm :18450, judge service :8441 (#1954 judge_service.py, log and allow file in runs/),
#   and the model: fake (scripted stand-in on :18452) or mtplx (#1954 logging proxy :18456 -> MTPLX :8000).
# usage: sm queue run --type service --label 1966-services -- services.sh <checkout> <fake|mtplx> [script.json]
set -u
K=${0:A:h} RUNS=${L1966_RUNS:-$HOME/.local/share/local-agents-proto/runs-1966}
WT=$1 MODEL=$2 SCRIPT=${3:-$K/script.json}
mkdir -p $RUNS
[[ -f $RUNS/agents.json ]] || echo '{}' > $RUNS/agents.json
pids=()
zsh $K/scratch_sm.sh $WT > $RUNS/scratch_sm.log 2>&1 & pids+=$!
JARGS=(--port 8441 --proxy-port 8442 --log $RUNS/local_judge.jsonl --allow-file $RUNS/local_judge_allow.jsonl)
if [[ $MODEL == fake ]]; then
  python3 $K/fake_oai.py 18452 $SCRIPT $RUNS/fake_model.jsonl > $RUNS/fake_model.log 2>&1 & pids+=$!
  JARGS+=(--no-model)
else
  python3 ${K:h:h}/1954_artifacts/kit/proto_proxy.py $RUNS/model_requests.jsonl 18456 8000 > $RUNS/proxy.log 2>&1 & pids+=$!
fi
python3 ${K:h:h}/1954_artifacts/kit/judge_service.py $RUNS/agents.json $JARGS > $RUNS/judge.log 2>&1 & pids+=$!
trap 'kill $pids 2>/dev/null' EXIT INT TERM
wait $pids[1]
