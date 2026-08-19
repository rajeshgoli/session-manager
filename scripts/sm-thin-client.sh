#!/bin/zsh
# sm — thin-client SSH shim to studio's session-manager (issue #1345).
#
# The laptop hosts no session-manager infra. This shim is command-agnostic: it
# forwards WHATEVER arguments you pass straight to studio's real `sm` CLI over
# ssh. It makes no assumptions about subcommands (studio's Rust CLI differs from
# the old Python one).
#
# Host selection is per-run: if studio.local:22 is reachable we're home and use
# `studio`; otherwise we're away and use `studio-away` (cloudflared access ssh).
#
# On sleep/wake the ssh transport drops (exit 255). We reconnect with bounded
# exponential backoff, re-probing home<->away each attempt. Any non-255 exit is
# the remote `sm` itself exiting (e.g. you pressed `q`) and is propagated as-is.
#
# Installed to ~/bin/sm (first on PATH), overriding any venv `sm`.

emulate -L zsh
set -o pipefail

# Full path to studio's sm, invoked directly to sidestep login-shell PATH issues.
STUDIO_SM='/Users/rajesh/projects/session-manager/venv/bin/sm'

# --- host selection -------------------------------------------------------
pick_host() {
  if nc -z -G 2 studio.local 22 >/dev/null 2>&1; then
    print -r -- studio
  else
    print -r -- studio-away
  fi
}

# --- build the remote command (single remote-shell layer) -----------------
# ssh hands our command to a NON-login remote shell whose PATH lacks Homebrew,
# so tools `sm` shells out to (notably `tmux` for attach) aren't found. Prepend
# Homebrew bins — `$PATH` stays literal locally and is expanded on studio.
# ssh joins our argv with spaces and the remote shell re-splits it, so quote
# each arg once with zsh ${(q)} to survive that single re-parse intact.
remote_cmd='PATH=/opt/homebrew/bin:/usr/local/bin:$PATH'" $STUDIO_SM"
for a in "$@"; do
  remote_cmd+=" ${(q)a}"
done

# Allocate a tty only when we have one locally, so interactive commands
# (attach/watch) get a pty while piped output (sm all | grep) stays clean.
ssh_tty=(-T)
[[ -t 0 && -t 1 ]] && ssh_tty=(-t)

# --- reconnect loop -------------------------------------------------------
attempt=0
max_attempts=8
backoff=1
backoff_cap=30

# Ctrl-C: when ssh holds the pty the signal goes to the remote; this trap is the
# fallback for when we're sleeping between retries.
trap 'exit 130' INT

while :; do
  host=$(pick_host)
  start=$SECONDS

  # ControlMaster: the first call opens a shared master that persists briefly,
  # so back-to-back `sm` calls (and follow-ups after `watch`) reuse it instead of
  # paying a fresh TCP+auth handshake each time. Per-host socket (%n) keeps
  # home/away separate. On sleep the master dies with the channel (exit 255) and
  # the loop below reopens it.
  ssh "${ssh_tty[@]}" \
    -o ControlMaster=auto \
    -o ControlPath="$HOME/.ssh/cm-%r@%n:%p" \
    -o ControlPersist=120 \
    -o ServerAliveInterval=5 \
    -o ServerAliveCountMax=3 \
    "$host" "$remote_cmd"
  rc=$?

  # Non-255 => remote sm exited on its own terms. Propagate and stop.
  if [[ $rc -ne 255 ]]; then
    exit $rc
  fi

  # Transport drop. If the connection had been stable (~30s+), a fresh sleep on a
  # long-lived watch/attach earns a fresh retry budget.
  if (( SECONDS - start >= 30 )); then
    attempt=0
    backoff=1
  fi

  attempt=$(( attempt + 1 ))
  if (( attempt > max_attempts )); then
    print -u2 -- "sm: lost connection to studio after ${max_attempts} attempts; giving up."
    exit 255
  fi

  print -u2 -- "sm: connection dropped (attempt ${attempt}/${max_attempts}); reconnecting in ${backoff}s..."
  sleep $backoff
  backoff=$(( backoff * 2 ))
  (( backoff > backoff_cap )) && backoff=$backoff_cap
done
