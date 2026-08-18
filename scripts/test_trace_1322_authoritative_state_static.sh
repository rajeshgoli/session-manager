#!/usr/bin/env bash
# Static guard for #1322's read-only trace.  It intentionally executes no
# trace or production command; the isolated test gate may run it later.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
trace="$script_dir/trace_1322_authoritative_state.sh"

bash -n "$trace"
! rg -F 'tmux -L "$tmux_socket"' "$trace"
rg -F 'if [[ -n "$tmux_socket" ]]; then' "$trace"
rg -F 'tmux "${tmux_args[@]}" has-session' "$trace"
rg -F 'tmux "${tmux_args[@]}" list-panes' "$trace"
