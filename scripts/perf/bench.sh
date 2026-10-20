#!/bin/bash
set -euo pipefail
N=${1:-10}
ID=${SM_BENCH_ID:?Set SM_BENCH_ID to a live session ID}
[[ "$N" =~ ^[1-9][0-9]*$ ]] || { echo 'runs must be positive' >&2; exit 1; }
uptime
for p in /health /sessions /client/bootstrap /client/queue /client/board /client/follows \
         /client/notices /client/sessions/"$ID" /client/usage/meters '/queue-jobs?limit=20'; do
  ts=$(for ((i=0; i<N; i++)); do
    curl --fail -sS -m 60 -o /dev/null -w '%{time_total}\n' "http://127.0.0.1:8420$p"
  done | sort -n)
  echo "$ts" | awk -v p="$p" '{a[NR]=$1} END{printf "%-28s p50 %6.3f  p90 %6.3f  max %6.3f\n", p, a[int(NR/2)+1], a[int(NR*0.9) < 1 ? 1 : int(NR*0.9)], a[NR]}'
done
uptime
