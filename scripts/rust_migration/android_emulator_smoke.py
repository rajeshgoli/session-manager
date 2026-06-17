"""Run a managed Android emulator smoke for the native SM app.

The smoke uses a debug-only app activity. It does not drive production UI, and
it does not require a plugged-in phone. The host starts an enrollment listener,
the emulator enrolls through the same repository path used by the deep link, and
then the app performs real HTTPS reads with its saved Cloudflare client cert.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import json
import os
import socket
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import yaml


DEFAULT_AVD = "sm_mtls_api35"
DEFAULT_SERVER_URL = "https://sm-app.rajeshgo.li"
DEFAULT_REPORT_FILE = "android-smoke-report.json"
DEFAULT_CONFIG = Path("config.yaml")
DEFAULT_APK = Path("android-app/app/build/outputs/apk/debug/app-debug.apk")
DEFAULT_APP_ID = "li.rajeshgo.sm"
DEFAULT_ACTIVITY = "li.rajeshgo.sm/.debug.AndroidSmokeActivity"
DEFAULT_OUTPUT_DIR = Path(".local/rust-mvp-rehearsals")
DEFAULT_SMOKE_TIMEOUT_SECONDS = 240
DEFAULT_BOOT_TIMEOUT_SECONDS = 180
DEVICE_TOKEN_MAX_AGE_SECONDS = 60 * 60 * 24 * 14


def build_device_access_token(
    *,
    session_cookie_secret: str,
    email: str,
    name: str,
    now: int | None = None,
    expires_in_seconds: int = 60 * 60,
) -> dict[str, Any]:
    secret = session_cookie_secret.strip()
    if not secret:
        raise ValueError("session_cookie_secret is required")
    if expires_in_seconds <= 0 or expires_in_seconds > DEVICE_TOKEN_MAX_AGE_SECONDS:
        raise ValueError("expires_in_seconds must be between 1 and 1209600")
    issued_at = int(time.time() if now is None else now)
    expires_at_unix = issued_at + expires_in_seconds
    payload = {
        "v": 1,
        "type": "device_access",
        "email": email.strip().lower(),
        "name": name.strip() or email.strip().lower(),
        "iat": issued_at,
        "exp": expires_at_unix,
    }
    payload_b64 = _urlsafe_no_pad(
        json.dumps(payload, separators=(",", ":"), sort_keys=True).encode("utf-8")
    )
    signature = _urlsafe_no_pad(
        hmac.new(secret.encode("utf-8"), payload_b64.encode("ascii"), hashlib.sha256).digest()
    )
    expires_at = datetime.fromtimestamp(expires_at_unix, timezone.utc).isoformat()
    return {
        "access_token": f"smat_{payload_b64}.{signature}",
        "expires_at": expires_at,
        "email": payload["email"],
        "name": payload["name"],
    }


def load_mobile_smoke_identity(
    config_path: Path,
    *,
    user_id: str | None = None,
    email: str | None = None,
    name: str | None = None,
) -> dict[str, str]:
    config = _load_runtime_config(config_path)
    mobile_terminal = _mapping(config.get("mobile_terminal"))
    allowed_users = _mapping(mobile_terminal.get("allowed_users"))
    resolved_user_id = (user_id or "").strip()
    if not resolved_user_id:
        interactive_users = sorted(
            key
            for key, raw in allowed_users.items()
            if _mapping(raw).get("interactive_shell_access") is True
        )
        if len(interactive_users) != 1:
            raise ValueError("pass --user-id when config has zero or multiple interactive mobile users")
        resolved_user_id = interactive_users[0]
    user_config = _mapping(allowed_users.get(resolved_user_id))
    resolved_email = (email or str(user_config.get("email") or resolved_user_id)).strip().lower()
    if not resolved_email:
        raise ValueError("mobile smoke email could not be resolved")
    google_auth = _mapping(_mapping(config.get("auth")).get("google"))
    secret = str(google_auth.get("session_cookie_secret") or "").strip()
    if not secret:
        raise ValueError("auth.google.session_cookie_secret is required to mint a device token")
    return {
        "user_id": resolved_user_id,
        "email": resolved_email,
        "name": (name or resolved_email).strip() or resolved_email,
        "session_cookie_secret": secret,
    }


def run_android_emulator_smoke(args: argparse.Namespace) -> dict[str, Any]:
    output = Path(args.output) if args.output else _default_output_path()
    output.parent.mkdir(parents=True, exist_ok=True)
    report: dict[str, Any] = {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "inputs": {
            "server_url_host": _host_from_url(args.server_url),
            "config": str(args.config),
            "avd": args.avd,
            "serial": args.serial,
            "app_id": args.app_id,
            "include_attach_ticket": args.include_attach_ticket,
        },
        "host_steps": [],
        "android_report": None,
        "summary": {"status": "blocked", "passed": 0, "skipped": 0, "blocked": 0},
        "artifacts": {"output": str(output)},
    }
    emulator_process: subprocess.Popen[str] | None = None
    enrollment_process: subprocess.Popen[str] | None = None
    reversed_port: int | None = None
    try:
        identity = load_mobile_smoke_identity(
            Path(args.config),
            user_id=args.user_id,
            email=args.email,
            name=args.name,
        )
        report["inputs"]["user_id"] = identity["user_id"]
        report["inputs"]["email"] = identity["email"]
        _host_step(report, "resolve_mobile_identity", "passed")

        if args.build_apk:
            _run_checked(["./gradlew", "assembleDebug"], cwd=Path("android-app"))
            _host_step(report, "build_debug_apk", "passed")
        else:
            _host_step(report, "build_debug_apk", "skipped", "disabled by --no-build-apk")

        apk_path = Path(args.apk)
        if not apk_path.is_file():
            raise RuntimeError(f"debug APK not found: {apk_path}")

        serial = args.serial or _first_booted_device(args.adb)
        if not serial:
            emulator_process = _start_emulator(args)
            serial = _wait_for_booted_device(args.adb, args.boot_timeout_seconds)
            _host_step(report, "start_emulator", "passed", f"serial={serial}")
        else:
            _host_step(report, "start_emulator", "skipped", f"using existing serial={serial}")
        report["inputs"]["serial"] = serial

        _run_checked([args.adb, "-s", serial, "install", "-r", str(apk_path)])
        _host_step(report, "install_debug_apk", "passed")
        _run_checked([args.adb, "-s", serial, "shell", "pm", "clear", args.app_id])
        _host_step(report, "clear_app_data", "passed")

        token = build_device_access_token(
            session_cookie_secret=identity["session_cookie_secret"],
            email=identity["email"],
            name=identity["name"],
            expires_in_seconds=args.token_expires_minutes * 60,
        )
        _host_step(report, "mint_short_lived_device_bearer", "passed")

        enrollment_port = _free_tcp_port()
        _run_checked([args.adb, "-s", serial, "reverse", f"tcp:{enrollment_port}", f"tcp:{enrollment_port}"])
        reversed_port = enrollment_port
        _host_step(report, "configure_adb_reverse_pairing_port", "passed")
        enrollment_process = _start_enrollment_listener(args, identity["user_id"], enrollment_port)
        enrollment_url = _read_enrollment_url(enrollment_process, args.smoke_timeout_seconds)
        _host_step(report, "start_enrollment_listener", "passed")

        _remove_android_report(args.adb, serial, args.app_id, args.report_file)
        _start_android_smoke_activity(
            args,
            serial,
            enrollment_url=enrollment_url,
            token=token,
        )
        _host_step(report, "start_debug_smoke_activity", "passed")

        android_report = _poll_android_report(
            args.adb,
            serial,
            args.app_id,
            args.report_file,
            args.smoke_timeout_seconds,
        )
        report["android_report"] = android_report
        _host_step(report, "collect_android_report", "passed")

        if enrollment_process.poll() is None:
            try:
                enrollment_process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass

        report["summary"] = _summarize(report)
    except Exception as error:
        _host_step(report, "android_emulator_smoke", "blocked", str(error))
        report["summary"] = _summarize(report)
        report["error"] = {"class": error.__class__.__name__, "detail": str(error)}
    finally:
        if reversed_port is not None:
            subprocess.run(
                [args.adb, "-s", report["inputs"].get("serial") or "", "reverse", "--remove", f"tcp:{reversed_port}"],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        if enrollment_process and enrollment_process.poll() is None:
            enrollment_process.terminate()
            try:
                enrollment_process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                enrollment_process.kill()
        if emulator_process and args.stop_started_emulator:
            subprocess.run([args.adb, "emu", "kill"], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return report


def _start_emulator(args: argparse.Namespace) -> subprocess.Popen[str]:
    return subprocess.Popen(
        [
            args.emulator,
            "-avd",
            args.avd,
            "-no-window",
            "-no-audio",
            "-no-boot-anim",
            "-gpu",
            "swiftshader_indirect",
            "-no-snapshot-save",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        text=True,
    )


def _first_booted_device(adb: str) -> str | None:
    result = subprocess.run([adb, "devices"], check=True, capture_output=True, text=True)
    for line in result.stdout.splitlines()[1:]:
        parts = line.split()
        if len(parts) >= 2 and parts[1] == "device":
            serial = parts[0]
            booted = subprocess.run(
                [adb, "-s", serial, "shell", "getprop", "sys.boot_completed"],
                check=False,
                capture_output=True,
                text=True,
            )
            if booted.stdout.strip() == "1":
                return serial
    return None


def _wait_for_booted_device(adb: str, timeout_seconds: int) -> str:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        serial = _first_booted_device(adb)
        if serial:
            return serial
        time.sleep(2)
    raise TimeoutError("Android emulator did not boot in time")


def _start_enrollment_listener(
    args: argparse.Namespace,
    user_id: str,
    port: int,
) -> subprocess.Popen[str]:
    sm_binary = _resolve_sm_binary(Path(args.sm_binary))
    command = [
        str(sm_binary),
        "enroll-device",
        "--config",
        str(args.config),
        "--user-id",
        user_id,
        "--expires-in-minutes",
        str(args.enrollment_expires_minutes),
        "--listen",
        f"127.0.0.1:{port}",
        "--url-base",
        f"http://127.0.0.1:{port}",
        "--no-qr",
    ]
    return subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )


def _read_enrollment_url(process: subprocess.Popen[str], timeout_seconds: int) -> str:
    assert process.stdout is not None
    deadline = time.monotonic() + timeout_seconds
    lines: list[str] = []
    while time.monotonic() < deadline:
        line = process.stdout.readline()
        if not line:
            if process.poll() is not None:
                raise RuntimeError("enroll-device exited before printing an enrollment URL: " + "".join(lines)[-500:])
            time.sleep(0.1)
            continue
        lines.append(line)
        marker = "Enrollment URL:"
        if marker in line:
            url = line.split(marker, 1)[1].strip()
            if url:
                return url
    raise TimeoutError("timed out waiting for enroll-device enrollment URL")


def _start_android_smoke_activity(
    args: argparse.Namespace,
    serial: str,
    *,
    enrollment_url: str,
    token: dict[str, Any],
) -> None:
    command = [
        args.adb,
        "-s",
        serial,
        "shell",
        "am",
        "start",
        "-W",
        "-n",
        args.activity,
        "--es",
        "server_url",
        args.server_url,
        "--es",
        "enrollment_url",
        enrollment_url,
        "--es",
        "access_token",
        token["access_token"],
        "--es",
        "user_email",
        token["email"],
        "--es",
        "user_name",
        token["name"],
        "--es",
        "expires_at",
        token["expires_at"],
        "--es",
        "report_file",
        args.report_file,
        "--ez",
        "include_attach_ticket",
        "true" if args.include_attach_ticket else "false",
    ]
    _run_checked(command)


def _poll_android_report(
    adb: str,
    serial: str,
    app_id: str,
    report_file: str,
    timeout_seconds: int,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout_seconds
    last_error = ""
    while time.monotonic() < deadline:
        result = subprocess.run(
            [adb, "-s", serial, "exec-out", "run-as", app_id, "cat", f"files/{report_file}"],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode == 0 and result.stdout.strip():
            try:
                return json.loads(result.stdout)
            except json.JSONDecodeError as error:
                last_error = f"invalid JSON report while polling: {error}"
                time.sleep(1)
                continue
        last_error = (result.stderr or result.stdout).strip()
        time.sleep(1)
    raise TimeoutError(f"timed out waiting for Android smoke report: {last_error}")


def _remove_android_report(adb: str, serial: str, app_id: str, report_file: str) -> None:
    subprocess.run(
        [adb, "-s", serial, "shell", "run-as", app_id, "rm", "-f", f"files/{report_file}"],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        text=True,
    )


def _run_checked(command: list[str], *, cwd: Path | None = None) -> None:
    subprocess.run(command, cwd=cwd, check=True)


def _resolve_sm_binary(path: Path) -> Path:
    if path.is_file():
        return path
    fallback = Path("target/debug/sm")
    if fallback.is_file():
        return fallback
    subprocess.run(["cargo", "build", "-p", "sm-server", "--bin", "sm"], check=True)
    if fallback.is_file():
        return fallback
    raise FileNotFoundError("Rust sm binary was not built")


def _free_tcp_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _load_runtime_config(config_path: Path) -> dict[str, Any]:
    # src.main.load_config applies the same default local-env overlay as the
    # live Python/Rust migration config path. Keep the import lazy so unit tests
    # can exercise helper functions without importing the server stack.
    try:
        from src.main import load_config
    except Exception:
        with config_path.open("r", encoding="utf-8") as handle:
            return yaml.safe_load(handle) or {}
    return load_config(str(config_path))


def _host_step(report: dict[str, Any], step_id: str, status: str, detail: str | None = None) -> None:
    row = {
        "id": step_id,
        "status": status,
        "at": datetime.now(timezone.utc).isoformat(),
    }
    if detail:
        row["detail"] = detail[:500]
    report["host_steps"].append(row)


def _summarize(report: dict[str, Any]) -> dict[str, Any]:
    passed = skipped = blocked = 0
    for row in report.get("host_steps", []):
        status = row.get("status")
        if status == "passed":
            passed += 1
        elif status == "skipped":
            skipped += 1
        elif status == "blocked":
            blocked += 1
    android_report = report.get("android_report")
    if isinstance(android_report, dict):
        summary = android_report.get("summary") or {}
        passed += int(summary.get("passed") or 0)
        skipped += int(summary.get("skipped") or 0)
        blocked += int(summary.get("blocked") or 0)
    return {
        "status": "passed" if blocked == 0 else "blocked",
        "passed": passed,
        "skipped": skipped,
        "blocked": blocked,
    }


def _mapping(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


def _urlsafe_no_pad(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).decode("ascii").rstrip("=")


def _default_output_path() -> Path:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return DEFAULT_OUTPUT_DIR / f"android-emulator-smoke-{stamp}.json"


def _host_from_url(url: str) -> str:
    from urllib.parse import urlparse

    return urlparse(url).hostname or ""


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--server-url", default=DEFAULT_SERVER_URL)
    parser.add_argument("--user-id")
    parser.add_argument("--email")
    parser.add_argument("--name")
    parser.add_argument("--adb", default=os.environ.get("ADB", "adb"))
    parser.add_argument(
        "--emulator",
        default=os.environ.get("ANDROID_EMULATOR", "/opt/homebrew/share/android-commandlinetools/emulator/emulator"),
    )
    parser.add_argument("--avd", default=DEFAULT_AVD)
    parser.add_argument("--serial")
    parser.add_argument("--apk", type=Path, default=DEFAULT_APK)
    parser.add_argument("--sm-binary", default="target/release/sm")
    parser.add_argument("--app-id", default=DEFAULT_APP_ID)
    parser.add_argument("--activity", default=DEFAULT_ACTIVITY)
    parser.add_argument("--report-file", default=DEFAULT_REPORT_FILE)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--boot-timeout-seconds", type=int, default=DEFAULT_BOOT_TIMEOUT_SECONDS)
    parser.add_argument("--smoke-timeout-seconds", type=int, default=DEFAULT_SMOKE_TIMEOUT_SECONDS)
    parser.add_argument("--enrollment-expires-minutes", type=int, default=15)
    parser.add_argument("--token-expires-minutes", type=int, default=60)
    parser.add_argument("--no-build-apk", dest="build_apk", action="store_false")
    parser.set_defaults(build_apk=True)
    parser.add_argument("--no-attach-ticket", dest="include_attach_ticket", action="store_false")
    parser.set_defaults(include_attach_ticket=True)
    parser.add_argument("--keep-started-emulator", dest="stop_started_emulator", action="store_false")
    parser.set_defaults(stop_started_emulator=True)
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--fail-on-blockers", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    report = run_android_emulator_smoke(args)
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(
            "Android emulator smoke: "
            f"{report['summary']['status']} "
            f"(passed={report['summary']['passed']} "
            f"skipped={report['summary']['skipped']} "
            f"blocked={report['summary']['blocked']})"
        )
        print(f"Report: {report['artifacts']['output']}")
    if args.fail_on_blockers and report["summary"]["blocked"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
