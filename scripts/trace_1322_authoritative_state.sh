#!/usr/bin/env bash
# Read-only #1322 trace: durable session/rotation records plus tmux/process liveness.
# Intentionally never reads terminal scrollback or sends input.
set -euo pipefail

session_id=${1:?usage: trace_1322_authoritative_state.sh SESSION_ID [STATE_FILE]}
state_file=${2:-"$HOME/.local/share/claude-sessions/sessions.json"}

jq --arg id "$session_id" '
  def matching($field): [.[$field][]? | select(.session_id == $id)];
  {
    session: ([.sessions[]? | select(.id == $id)] | .[0] |
      if . == null then null else {
        id, status, completion_status, stopped_at, provider, provider_resume_id,
        tmux_session, tmux_socket_name, last_activity
      } end),
    rotations: matching("session_credential_rotations") |
      map({id, status, requested_at, idle_proof_at, runtime_launch_id, updated_at, failure_reason}),
    launches: matching("session_runtime_launches") |
      map({id, operation_kind, status, created_at, updated_at, failure_reason})
  }
' "$state_file"

tmux_session=$(jq -r --arg id "$session_id" '[.sessions[]? | select(.id == $id)][0].tmux_session // empty' "$state_file")
tmux_socket=$(jq -r --arg id "$session_id" '[.sessions[]? | select(.id == $id)][0].tmux_socket_name // empty' "$state_file")
if [[ -z "$tmux_session" ]]; then
  exit 0
fi

tmux_args=()
if [[ -n "$tmux_socket" ]]; then
  tmux_args=(-L "$tmux_socket")
fi

if tmux "${tmux_args[@]}" has-session -t "$tmux_session" 2>/dev/null; then
  echo 'tmux_session_exists=true'
  tmux "${tmux_args[@]}" list-panes -t "$tmux_session" -F '#{pane_pid}' |
    while IFS= read -r pane_pid; do
      ps -p "$pane_pid" -o pid=,ppid=,stat=,command=
    done
else
  echo 'tmux_session_exists=false'
fi
