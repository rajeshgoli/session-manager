#!/bin/zsh
# #1954 queue job: serve Flash-Next on MTPLX with K.6's settings until the stop file appears, then
# stop MTPLX cleanly (never a bare kill: that loses its on-disk conversation cache).
# usage: sm queue run --type background --timeout 12h --label 1954-mtplx -- mtplx_job.sh
# Stop:  touch /private/tmp/l1954-mtplx.stop  (the queue's cancel kills after 10 s: too short),
#        or run `mtplx stop --port 8000 --grace-seconds 90` from outside; this shell reaps the server.
X=~/.local/share/mtplx-venv/bin/mtplx
STOP=/private/tmp/l1954-mtplx.stop
rm -f $STOP
if pgrep -f 'mtplx-venv/.*mtplx' >/dev/null; then echo "refusing: an MTPLX process is already running"; exit 2; fi
reclaimable=$(vm_stat | awk '/Pages free/{f=$3} /Pages inactive/{i=$3} /Pages speculative/{s=$3} /Pages purgeable/{p=$3} END {printf "%d", (f+i+s+p)*16384/1e9}')
echo "reclaimable memory: ${reclaimable} GB (need 160)"
if (( reclaimable < 160 )); then echo "refusing: reclaimable memory ${reclaimable} GB < 160 GB"; exit 3; fi
export MTPLX_SESSION_BANK_MAX_BYTES=16G
$X serve --model Youssofal/Qwen3.8-Flash-Next-MTPLX-Optimized-Speed --profile turbo \
  --host 127.0.0.1 --port 8000 --context-window 200000 --max-active-requests 2 \
  --batching-preset agent --yes &
SERVER=$!
stop_server() {
  echo "$(date +%T) stopping MTPLX ($1)"
  # Stop runs in the background while this shell waits on (and so reaps) the server child.
  $X stop --port 8000 --grace-seconds 90 >/dev/null 2>&1 &
  wait $SERVER; wait; sleep 2
  pgrep -f 'mtplx-venv/.*mtplx' >/dev/null && { echo "killing leftover MTPLX"; pkill -9 -f 'mtplx-venv/.*mtplx'; }
  pgrep -f 'mtplx-venv/.*mtplx' >/dev/null && echo "MTPLX STILL RUNNING" || echo "$(date +%T) no MTPLX process remains"
}
trap 'stop_server signal; exit 143' TERM INT
while kill -0 $SERVER 2>/dev/null; do
  if [[ -e $STOP ]]; then stop_server "stop file"; rm -f $STOP; exit 0; fi
  sleep 5
done
wait $SERVER; echo "$(date +%T) MTPLX exited: $?"
