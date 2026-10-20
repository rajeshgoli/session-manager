#!/bin/bash
set -euo pipefail
OUT=${1:?Usage: capture.sh OUTPUT_DIR}
mkdir -p "$OUT"
SCRIPTS=$(cd "$(dirname "$0")" && pwd)
for ((i=1; i<=6; i++)); do
  # Resolve the listening process each time; never combine server generations.
  pid=$(lsof -tiTCP:8420 -sTCP:LISTEN | sort -u)
  [[ "$pid" =~ ^[0-9]+$ ]] || { echo 'Expected one live server PID' >&2; exit 1; }
  if [[ $i == 1 ]]; then first_pid=$pid; fi
  [[ "$pid" == "$first_pid" ]] || { echo 'Server restarted during capture' >&2; exit 1; }
  uptime > "$OUT/load$i.txt"
  sample "$pid" 10 -file "$OUT/s$i.txt"
  bash "$SCRIPTS/bench.sh" > "$OUT/bench$i.txt"
  [[ "$(lsof -tiTCP:8420 -sTCP:LISTEN | sort -u)" == "$first_pid" ]] || {
    echo 'Server restarted during capture' >&2; exit 1;
  }
  if ((i < 6)); then sleep 110; fi
done
python3 "$SCRIPTS/agg.py" "$OUT"/s*.txt
