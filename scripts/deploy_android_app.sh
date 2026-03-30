#!/usr/bin/env bash
set -euo pipefail

APP_NAME="${APP_NAME:-session-manager-android}"
SERVER_URL="${SERVER_URL:-http://127.0.0.1:8420}"
APK_PATH="${1:-android-app/app/build/outputs/apk/debug/app-debug.apk}"
VERSION_CODE="${VERSION_CODE:-}"
VERSION_NAME="${VERSION_NAME:-}"

if [[ ! -f "$APK_PATH" ]]; then
  echo "APK not found: $APK_PATH" >&2
  exit 1
fi

curl_args=(
  --fail
  --show-error
  --silent
  -X POST
  "$SERVER_URL/deploy/$APP_NAME"
  -F "file=@${APK_PATH};type=application/vnd.android.package-archive"
)

if [[ -n "$VERSION_CODE" ]]; then
  curl_args+=(-F "version_code=$VERSION_CODE")
fi

if [[ -n "$VERSION_NAME" ]]; then
  curl_args+=(-F "version_name=$VERSION_NAME")
fi

curl "${curl_args[@]}"
printf '\n'
