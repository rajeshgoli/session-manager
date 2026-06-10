#!/usr/bin/env bash
set -euo pipefail

manifest="crates/sm-server/Cargo.toml"
test_target="read_only_http"

tests=(
  runtime_core_lifecycle_uses_tmux_backend_when_enabled
  runtime_core_lifecycle_uses_codex_fork_launch_config
  runtime_core_send_and_retire_use_persisted_tmux_socket
  runtime_core_task_complete_wakes_parent_runtime
  runtime_core_rejects_remote_node_create_before_local_tmux_launch
)

for test_name in "${tests[@]}"; do
  cargo test --manifest-path "$manifest" --test "$test_target" "$test_name" -- --exact
done
