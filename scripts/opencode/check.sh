#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
node --test scripts/opencode/sm_judge.test.mjs
scripts/test-rust-isolated.sh opencode -- --test-threads=1
cargo clippy -p sm-server --all-targets -- -D warnings
cargo fmt -p sm-server --check
