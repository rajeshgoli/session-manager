#!/usr/bin/env bash
set -euo pipefail

APP_NAME="${APP_NAME:-session-manager-android}"
SERVER_URL="${SERVER_URL:-http://127.0.0.1:8420}"
APK_PATH="${1:-android-app/app/build/outputs/apk/debug/app-debug.apk}"
VERSION_CODE="${VERSION_CODE:-}"
VERSION_NAME="${VERSION_NAME:-}"
RELEASE_NOTES="${RELEASE_NOTES:-}"
if [[ -n "${RELEASE_NOTES_FILE:-}" ]]; then
  RELEASE_NOTES="$(cat -- "$RELEASE_NOTES_FILE")"
fi
if [[ -z "${RELEASE_NOTES//[[:space:]]/}" ]]; then
  echo "Set RELEASE_NOTES (or RELEASE_NOTES_FILE) to a short summary of what changed and what to try." >&2
  exit 1
fi

if [[ ! -f "$APK_PATH" ]]; then
  echo "APK not found: $APK_PATH" >&2
  exit 1
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
python3 "$SCRIPT_DIR/verify_android_apk.py" "$APK_PATH"

# Publish the version Android will compare on install, read from the APK itself.
# The server refuses a version code that is not higher than the published one.
read -r APK_VERSION_CODE APK_VERSION_NAME < <(python3 "$SCRIPT_DIR/verify_android_apk.py" --print-version "$APK_PATH")
if [[ -n "$VERSION_CODE" && "$VERSION_CODE" != "$APK_VERSION_CODE" ]]; then
  echo "VERSION_CODE=$VERSION_CODE but the APK has versionCode $APK_VERSION_CODE. Rebuild with SM_VERSION_CODE=$VERSION_CODE." >&2
  exit 1
fi
if [[ -n "$VERSION_NAME" && "$VERSION_NAME" != "$APK_VERSION_NAME" ]]; then
  echo "VERSION_NAME=$VERSION_NAME but the APK has versionName $APK_VERSION_NAME. Rebuild with SM_VERSION_NAME=$VERSION_NAME." >&2
  exit 1
fi
VERSION_CODE="$APK_VERSION_CODE"
VERSION_NAME="$APK_VERSION_NAME"
echo "APK version: $VERSION_NAME ($VERSION_CODE)"

curl_args=(
  --fail-with-body
  --show-error
  --silent
  -X POST
  "$SERVER_URL/deploy/$APP_NAME"
  -F "file=@${APK_PATH};type=application/vnd.android.package-archive"
  --form-string "release_notes=$RELEASE_NOTES"
)

# The auto PM's weekly app-update cap counts deploys by the agent's session.
if [[ -n "${CLAUDE_SESSION_MANAGER_ID:-}" ]]; then
  curl_args+=(-H "x-sm-session-id: $CLAUDE_SESSION_MANAGER_ID")
fi

curl_args+=(-F "version_code=$VERSION_CODE" --form-string "version_name=$VERSION_NAME")

curl "${curl_args[@]}"
printf '\n'
