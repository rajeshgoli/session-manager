#!/usr/bin/env bash
set -euo pipefail

# Every Rust test must run through this launcher. AppState uses this root before
# opening durable stores, so default config can never resolve the live session
# registry, queue, usage databases, or reparent-apply lock.
test_root="$(mktemp -d "${TMPDIR:-/tmp}/sm-rust-test.XXXXXX")"
cleanup() {
  rm -rf -- "$test_root"
}
trap cleanup EXIT

SM_TEST_ISOLATION_ROOT="$test_root" cargo test -p sm-server "$@"
