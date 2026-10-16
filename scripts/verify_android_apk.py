#!/usr/bin/env python3
"""Refuse to publish an APK that silently disables the configured app features."""

import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


REQUIRED_FIELDS = (
    "SM_DEFAULT_SERVER_URL",
    "SM_GOOGLE_SERVER_CLIENT_ID",
    "SM_LINK_HOST",
    "SM_FIREBASE_PROJECT_ID",
    "SM_FIREBASE_SENDER_ID",
    "SM_FIREBASE_APP_ID",
    "SM_FIREBASE_API_KEY",
)


def missing_configuration(dex: str) -> list[str]:
    fields = dict(re.findall(
        r'^\.field public static final (SM_[A-Z_]+):Ljava/lang/String; = "(.*)"$',
        dex, re.MULTILINE,
    ))
    return [name for name in REQUIRED_FIELDS if not fields.get(name, "").strip()]


def find_apkanalyzer() -> str:
    on_path = shutil.which("apkanalyzer")
    if on_path:
        return on_path
    sdk_paths = [os.environ.get("ANDROID_HOME"), os.environ.get("ANDROID_SDK_ROOT")]
    local = Path(__file__).resolve().parents[1] / "android-app/local.properties"
    if local.is_file():
        sdk_paths.extend(line.split("=", 1)[1].strip() for line in local.read_text().splitlines()
                         if line.startswith("sdk.dir="))
    for sdk in sdk_paths:
        if sdk:
            candidate = Path(sdk) / "cmdline-tools/latest/bin/apkanalyzer"
            if candidate.is_file():
                return str(candidate)
    raise RuntimeError("apkanalyzer is required: set ANDROID_HOME or put it on PATH")


def manifest_version(apkanalyzer: str, apk: Path) -> tuple[str, str]:
    """The APK's own versionCode and versionName, which Android compares on install."""
    values = []
    for field in ("version-code", "version-name"):
        result = subprocess.run([apkanalyzer, "manifest", field, str(apk)],
                                capture_output=True, text=True, timeout=60)
        lines = result.stdout.strip().splitlines()
        if result.returncode or not lines:
            raise RuntimeError(f"cannot read the APK {field}")
        values.append(lines[-1].strip())
    return values[0], values[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("apk", type=Path)
    parser.add_argument("--print-version", action="store_true",
                        help="print only '<versionCode> <versionName>' from the APK manifest")
    args = parser.parse_args()
    if args.print_version:
        try:
            print(*manifest_version(find_apkanalyzer(), args.apk))
        except (RuntimeError, OSError, subprocess.TimeoutExpired) as error:
            print(f"APK verification failed: {error}", file=sys.stderr)
            return 1
        return 0
    try:
        result = subprocess.run(
            [find_apkanalyzer(), "dex", "code", "--class", "li.rajeshgo.sm.BuildConfig", str(args.apk)],
            capture_output=True, text=True, timeout=60,
        )
        if result.returncode:
            raise RuntimeError("cannot inspect APK BuildConfig; check the APK, Android SDK, and JAVA_HOME")
        missing = missing_configuration(result.stdout)
        if missing:
            raise RuntimeError("APK has missing configuration: " + ", ".join(missing)
                               + ". Refusing to deploy an app with disabled features. "
                               "Copy ~/projects/session-manager/android-app/local.defaults.properties "
                               "into your worktree's android-app/, rebuild, and deploy again.")
    except (RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(f"APK verification failed: {error}", file=sys.stderr)
        return 1
    # Never print the configuration values.
    print("APK configuration verified: server, sign-in, app links, and push notifications")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
