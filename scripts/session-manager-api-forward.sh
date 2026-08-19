#!/bin/zsh
# session-manager-api-forward — self-healing local API forward (issue #1345, C1).
#
# Keeps a laptop-local 127.0.0.1:8420 -> studio 127.0.0.1:8420 tunnel alive so
# HTTP clients (DeskBar, `curl 127.0.0.1:8420/health`, SM_API_URL) reach studio's
# session-manager without any laptop-side server. studio's bind stays
# localhost-only; nothing is exposed on the network.
#
# Home -> `studio`; away -> `studio-away` (cloudflared access ssh), so the -L
# forward works in both locations. Reuses the shim's bounded-backoff reconnect on
# sleep/wake. Runs under launchd (KeepAlive), so it never permanently gives up.

emulate -L zsh
set -o pipefail

LOCAL_PORT=8420
REMOTE_PORT=8420

pick_host() {
  if nc -z -G 2 studio.local 22 >/dev/null 2>&1; then
    print -r -- studio
  else
    print -r -- studio-away
  fi
}

attempt=0
backoff=1
backoff_cap=30

trap 'exit 0' INT TERM

while :; do
  host=$(pick_host)
  start=$SECONDS

  # BatchMode + accept-new so an unattended daemon never hangs on a prompt.
  ssh -N \
    -o BatchMode=yes \
    -o ConnectTimeout=10 \
    -o ControlMaster=no \
    -o ControlPath=none \
    -o StrictHostKeyChecking=accept-new \
    -o ExitOnForwardFailure=yes \
    -o ServerAliveInterval=5 \
    -o ServerAliveCountMax=3 \
    -L "127.0.0.1:${LOCAL_PORT}:127.0.0.1:${REMOTE_PORT}" \
    "$host"
  rc=$?

  # If the forward held for a while, reset the backoff budget so a sleep/wake
  # drop reconnects promptly instead of inheriting a long backoff.
  if (( SECONDS - start >= 30 )); then
    attempt=0
    backoff=1
  fi

  attempt=$(( attempt + 1 ))
  print -u2 -- "api-forward: ssh exited ($rc) via ${host}; reconnecting in ${backoff}s (attempt ${attempt})..."
  sleep $backoff
  backoff=$(( backoff * 2 ))
  (( backoff > backoff_cap )) && backoff=$backoff_cap
done
