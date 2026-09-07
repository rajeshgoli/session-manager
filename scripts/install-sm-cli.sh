#!/usr/bin/env bash
# Install the Rust `sm` CLI where cargo cannot reach it.
#
# `cargo clean` deletes the whole target directory, so any binary that only
# ever lives at target/release/sm disappears with it - together with every
# spawned session's ability to run `sm` at all. The server binary already
# survives this because it is installed to .local/bin/sm-server and launchd is
# registered against that copy (sm#1134); the CLI had no such copy. This
# script gives it one.
#
# .local/bin is not a build directory: nothing in the cargo invocation writes
# it, so a clean, a rebuild, or a `cargo clean` by anyone working in the repo
# leaves the installed CLI untouched.
#
# The install is an atomic rename from a staging file beside the destination,
# so a `sm` that is mid-exec is never replaced in place and a failed build
# never leaves a truncated binary on PATH.
#
# Usage: scripts/install-sm-cli.sh [--skip-build] [--source PATH]

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

SM_CLI_BINARY="${SM_CLI_BINARY:-$REPO_ROOT/.local/bin/sm}"
SM_TARGET_DIR="${SM_TARGET_DIR:-$REPO_ROOT/target}"
SM_CLI_CARGO_OUTPUT="${SM_CLI_CARGO_OUTPUT:-$SM_TARGET_DIR/release/sm}"

SKIP_BUILD=0
SOURCE_OVERRIDE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    --source) SOURCE_OVERRIDE="${2:?--source needs a path}"; shift 2 ;;
    -h|--help)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done

fail() { echo "install-sm-cli: $*" >&2; exit 1; }
step() { echo; echo "== $*"; }

canonical_path() {
  /usr/bin/python3 - "$1" <<'PY'
import os, sys
print(os.path.realpath(sys.argv[1]))
PY
}

# The whole point of the installed copy is that it is not cargo's output. If
# the two ever resolve to the same file the protection is gone and a clean
# takes the CLI with it, so refuse rather than pretend to install.
if [[ "$(canonical_path "$SM_CLI_BINARY")" == "$(canonical_path "$SM_CLI_CARGO_OUTPUT")" ]]; then
  fail "$SM_CLI_BINARY resolves to cargo's own output ($SM_CLI_CARGO_OUTPUT);
       an installed copy there would be deleted by the next \`cargo clean\`"
fi

if [[ -n "$SOURCE_OVERRIDE" ]]; then
  SOURCE="$SOURCE_OVERRIDE"
elif [[ "$SKIP_BUILD" -eq 1 ]]; then
  SOURCE="$SM_CLI_CARGO_OUTPUT"
else
  step "Building sm"
  cargo build --release --bin sm --target-dir "$SM_TARGET_DIR" \
    || fail "build failed; the installed CLI at $SM_CLI_BINARY was not touched"
  SOURCE="$SM_CLI_CARGO_OUTPUT"
fi

[[ -x "$SOURCE" ]] || fail "no executable to install at $SOURCE"

step "Installing $SOURCE -> $SM_CLI_BINARY"
mkdir -p "$(dirname "$SM_CLI_BINARY")" \
  || fail "could not create $(dirname "$SM_CLI_BINARY")"

STAGING="$SM_CLI_BINARY.staging.$$"
trap 'rm -f "$STAGING"' EXIT

cp -f "$SOURCE" "$STAGING" || fail "could not stage $SOURCE"
chmod 755 "$STAGING"

# Verify before publishing: a binary that cannot run is worse than a missing
# one, because PATH resolution finds it and every caller fails at use time.
"$STAGING" --version >/dev/null 2>&1 \
  || fail "staged binary does not run; the previous $SM_CLI_BINARY is untouched"

mv -f "$STAGING" "$SM_CLI_BINARY" || fail "could not install $SM_CLI_BINARY"
trap - EXIT

step "Done"
echo "$("$SM_CLI_BINARY" --version) installed at $SM_CLI_BINARY"
echo "Put $(dirname "$SM_CLI_BINARY") on PATH ahead of the venv so \`sm\` resolves here."
