#!/usr/bin/env bash
# Builds the `sm` the backup mini runs (#2264): an Intel Mac mini that may be on
# macOS 10.13. sm-server runs this from a checkout of its own commit when the
# mini asks for its sm and the published one is another commit
# (managed_artifacts/backup/cli_build.rs); it serves the published pair at
# GET /backup/cli and /backup/cli/binary, and the mini updates itself from it.
#
#   build-backup-cli.sh stage --target-dir DIR --expect-version V --dir CLI_DIR
#       Build, verify that `sm --version` prints V, and write
#       CLI_DIR/sm-x86_64-apple-darwin{,.json}.staging.
#   build-backup-cli.sh publish --dir CLI_DIR
#       Rename both staged files into place, binary first.
#
# Run from the repository root (cargo reads .cargo/config.toml from the cwd).
# SM_BUILD_SHA names the commit; it is what puts the commit into --version.
set -euo pipefail

TRIPLE=x86_64-apple-darwin
MIN_OS=10.13
NAME="sm-$TRIPLE"
SYMBOLS="$(cd "$(dirname "$0")" && pwd)/backup-cli-symbols.txt"

fail() { echo "ERROR: $1" >&2; exit 1; }

mode="${1:-}"
[[ $# -gt 0 ]] && shift
target_dir="" expected="" dir=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target-dir) target_dir="$2"; shift 2 ;;
    --expect-version) expected="$2"; shift 2 ;;
    --dir) dir="$2"; shift 2 ;;
    *) fail "unknown argument: $1" ;;
  esac
done
[[ -n "$dir" ]] || fail "--dir is required"

if [[ "$mode" == publish ]]; then
  [[ -f "$dir/$NAME.staging" && -f "$dir/$NAME.json.staging" ]] \
    || fail "nothing staged in $dir"
  # Binary first: a reader that sees the new description finds the new bytes.
  # Between the renames sm answers 503 for the binary (size mismatch), and the
  # mini's SHA-256 check rejects any other pairing.
  mv -f "$dir/$NAME.staging" "$dir/$NAME"
  mv -f "$dir/$NAME.json.staging" "$dir/$NAME.json"
  echo "published $(cat "$dir/$NAME.json")"
  exit 0
fi
[[ "$mode" == stage ]] || fail "usage: build-backup-cli.sh stage|publish ..."
[[ -n "$target_dir" && -n "$expected" ]] || fail "stage needs --target-dir and --expect-version"

rustup target list --installed 2>/dev/null | grep -qx "$TRIPLE" \
  || fail "the $TRIPLE Rust target is not installed; run: rustup target add $TRIPLE"

MACOSX_DEPLOYMENT_TARGET="$MIN_OS" cargo build --release -p sm-server --bin sm \
  --target "$TRIPLE" --target-dir "$target_dir" \
  || fail "the $TRIPLE build of sm failed"
binary="$target_dir/$TRIPLE/release/sm"
[[ -x "$binary" ]] || fail "the $TRIPLE build produced no executable at $binary"
[[ "$(lipo -archs "$binary")" == x86_64 ]] || fail "$binary is not an x86_64 executable"

# The minimum macOS the loader enforces: LC_VERSION_MIN_MACOSX (version) for
# deployment targets below 10.14, LC_BUILD_VERSION (minos) above.
minos="$(otool -l "$binary" | awk '
  /LC_VERSION_MIN_MACOSX/ { want = "version" } /LC_BUILD_VERSION/ { want = "minos" }
  want != "" && $1 == want { print $2; exit }')"
[[ -n "$minos" ]] || fail "could not read the minimum macOS version of $binary"
if ! awk -v have="$minos" -v max="$MIN_OS" 'BEGIN {
    split(have, h, "."); split(max, m, ".")
    exit !(h[1] + 0 < m[1] + 0 || (h[1] + 0 == m[1] + 0 && h[2] + 0 <= m[2] + 0)) }'; then
  fail "$binary requires macOS $minos; the backup mini needs $MIN_OS or lower"
fi

# A symbol missing on 10.13 fails only when it is first called, so --version on
# the mini cannot catch it. Every import was checked against the macOS SDK
# headers' availability annotations when it was added to $SYMBOLS.
imports="$(nm -um "$binary" | sed -n 's/.*external \([^ ]*\) (from \([^)]*\)).*/\1 \2/p' | sort -u)"
new="$(comm -23 <(printf '%s\n' "$imports") <(grep -v '^#' "$SYMBOLS" | sort -u))"
if [[ -n "$new" ]]; then
  fail "the backup mini's sm imports symbols nobody has checked against macOS $MIN_OS:
$new
       For each, find its declaration in \$(xcrun --show-sdk-path) and check its
       availability annotation (API_AVAILABLE / __OSX_AVAILABLE) is $MIN_OS or
       lower. Then add it to $SYMBOLS. If it is newer, the mini cannot run this
       build; remove the dependency that introduced it."
fi

arch -x86_64 /usr/bin/true 2>/dev/null \
  || fail "Rosetta is needed to check the $TRIPLE build's version; run: softwareupdate --install-rosetta"
intel_version="$(arch -x86_64 "$binary" --version)"
[[ "$intel_version" == "$expected" ]] \
  || fail "the $TRIPLE build reports '$intel_version'; sm-server expects '$expected'"
[[ "$intel_version" =~ ^sm\ [^\"\\]+\ \([0-9a-f]{12}\)$ ]] \
  || fail "sm --version must name the commit; got '$intel_version'"

mkdir -p "$dir"
cp "$binary" "$dir/$NAME.staging"
chmod 0755 "$dir/$NAME.staging"
sha256="$(shasum -a 256 "$dir/$NAME.staging" | awk '{print $1}')"
bytes="$(stat -f %z "$dir/$NAME.staging")"
printf '{"version":"%s","sha256":"%s","bytes":%s}\n' "$intel_version" "$sha256" "$bytes" \
  > "$dir/$NAME.json.staging"
echo "staged $intel_version for $TRIPLE (macOS $minos, $bytes bytes)"
