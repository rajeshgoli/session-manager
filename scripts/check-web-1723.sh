#!/usr/bin/env bash
set -euo pipefail
node --test scripts/test-web-queue.mjs
node --check crates/sm-server/src/web/queue.js
node --check crates/sm-server/src/web/analytics.js
cargo fmt -p sm-server
cargo fmt -p sm-server --check
cargo clippy -p sm-server --all-targets -- -D warnings
scripts/test-rust-isolated.sh --lib utilization::
scripts/test-rust-isolated.sh --lib owner_web_guard
scripts/test-rust-isolated.sh --lib web_shell
