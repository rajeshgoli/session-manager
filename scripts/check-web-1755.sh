#!/usr/bin/env bash
set -euo pipefail
node --test scripts/test-web-history-inbox.mjs
cargo fmt -p sm-server --check
cargo clippy -p sm-server --all-targets -- -D warnings
scripts/test-rust-isolated.sh --test history_http
scripts/test-rust-isolated.sh --lib web_shell
