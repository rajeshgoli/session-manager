#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 3 ]]; then
  cat <<'USAGE'
Usage:
  scripts/codex_fork/release_artifacts.sh <codex_repo_path> <artifact_release> <artifact_ref> [github_repo]

Examples:
  scripts/codex_fork/release_artifacts.sh /tmp/codex-fork v0.1.0-sm 8f00aa11...
  scripts/codex_fork/release_artifacts.sh /tmp/codex-fork v0.1.0-sm 8f00aa11... rajeshgoli/codex-fork
USAGE
  exit 1
fi

CODEX_REPO_PATH="$1"
ARTIFACT_RELEASE="$2"
ARTIFACT_REF="$3"
GITHUB_REPO="${4:-}"

if [[ ! -d "$CODEX_REPO_PATH" ]]; then
  echo "error: codex repo path not found: $CODEX_REPO_PATH" >&2
  exit 1
fi

OUTPUT_DIR="$CODEX_REPO_PATH/dist/$ARTIFACT_RELEASE"
mkdir -p "$OUTPUT_DIR"

TARGETS=(
  "aarch64-apple-darwin"
  "x86_64-apple-darwin"
  "x86_64-unknown-linux-gnu"
)

pushd "$CODEX_REPO_PATH" >/dev/null
git checkout "$ARTIFACT_REF"

for target in "${TARGETS[@]}"; do
  echo "building codex for $target..."
  cargo build --release --target "$target"
  binary="target/$target/release/codex"
  if [[ ! -x "$binary" ]]; then
    echo "error: expected binary missing: $binary" >&2
    exit 1
  fi
  archive="$OUTPUT_DIR/codex-${ARTIFACT_RELEASE}-${target}.tar.gz"
  tar -C "$(dirname "$binary")" -czf "$archive" "$(basename "$binary")"
  echo "created $archive"
done

cat >"$OUTPUT_DIR/manifest.json" <<MANIFEST
{
  "artifact_release": "$ARTIFACT_RELEASE",
  "artifact_ref": "$ARTIFACT_REF",
  "targets": [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu"
  ]
}
MANIFEST

if [[ -n "$GITHUB_REPO" ]]; then
  echo "publishing assets to GitHub release $ARTIFACT_RELEASE in $GITHUB_REPO..."
  gh release create "$ARTIFACT_RELEASE" \
    "$OUTPUT_DIR"/codex-"$ARTIFACT_RELEASE"-*.tar.gz \
    "$OUTPUT_DIR"/manifest.json \
    --repo "$GITHUB_REPO" \
    --title "$ARTIFACT_RELEASE" \
    --notes "Codex fork artifact release pinned to $ARTIFACT_REF"
fi

popd >/dev/null
echo "done: artifacts in $OUTPUT_DIR"
