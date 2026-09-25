#!/usr/bin/env bash
set -euo pipefail

# Every Rust test must run through this launcher. AppState uses this root before
# opening durable stores, so default config can never resolve the live session
# registry, queue, usage databases, or reparent-apply lock.
# The isolation root must be absolute, and tests build fixture paths from the
# temp dir, so resolve a relative TMPDIR before anything derives from it.
case "${TMPDIR:-/tmp}" in
  /*) ;;
  # The ./ prefix keeps cd from searching (and echoing) a CDPATH match.
  *) TMPDIR="$(cd -- "./$TMPDIR" && pwd)"; export TMPDIR ;;
esac
test_root="$(mktemp -d "${TMPDIR:-/tmp}/sm-rust-test.XXXXXX")"
cleanup() {
  rm -rf -- "$test_root"
}
trap cleanup EXIT

# Jobs started by `sm queue run` inherit launchd's 256-descriptor soft limit,
# which the parallel HTTP suite can exhaust ("Too many open files"). Raise the
# soft limit toward the hard limit before running tests (sm#1432).
target_files=8192
hard_files="$(ulimit -Hn)"
if [ "$hard_files" != unlimited ] && [ "$hard_files" -lt "$target_files" ]; then
  target_files="$hard_files"
fi
soft_files="$(ulimit -Sn)"
if [ "$soft_files" != unlimited ] && [ "$soft_files" -lt "$target_files" ]; then
  ulimit -Sn "$target_files"
fi

SM_TEST_ISOLATION_ROOT="$test_root" cargo test -p sm-server "$@"
