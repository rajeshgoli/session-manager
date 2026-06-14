"""Run read-only Cloudflare Access/mobile cutover smoke checks.

The script is an evidence collector, not a Cloudflare configurator. Operators
provide the hostnames and Access assertions from the deployed policy; the script
records which route-class checks passed, skipped, or blocked.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import json
import os
import time
import urllib.error
import urllib.request
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


DEFAULT_BASE_URL = "http://127.0.0.1:8421"
DEFAULT_APP_NAME = "session-manager-android"


def build_smoke_report(
    *,
    base_url: str = DEFAULT_BASE_URL,
    mobile_host: str | None = None,
    browser_host: str | None = None,
    mobile_access_jwt: str | None = None,
    browser_access_jwt: str | None = None,
    public_edge_secret: str | None = None,
    bearer_token: str | None = None,
    cookie: str | None = None,
    session_id: str | None = None,
    app_name: str = DEFAULT_APP_NAME,
    timeout_seconds: float = 5.0,
    urlopen=None,
) -> dict[str, Any]:
    if timeout_seconds <= 0:
        raise ValueError("--timeout must be positive")
    base_url = base_url.rstrip("/")
    checks = [
        _request_check(
            check_id="mobile.bootstrap_requires_access",
            description="mobile host denies bootstrap without Access assertion",
            base_url=base_url,
            method="GET",
            path="/client/bootstrap",
            host=mobile_host,
            public_edge_secret=public_edge_secret,
            expected_status=403,
            expected_detail="Cloudflare Access mobile app assertion is required",
            timeout_seconds=timeout_seconds,
            urlopen=urlopen,
            require_public_edge_secret=True,
        ),
        _request_check(
            check_id="mobile.bootstrap_with_access",
            description="mobile host accepts enrolled Access assertion for bootstrap",
            base_url=base_url,
            method="GET",
            path="/client/bootstrap",
            host=mobile_host,
            access_jwt=mobile_access_jwt,
            public_edge_secret=public_edge_secret,
            expected_status=200,
            expected_json_type=dict,
            expected_json_keys=("auth",),
            timeout_seconds=timeout_seconds,
            urlopen=urlopen,
            require_public_edge_secret=True,
        ),
        _request_check(
            check_id="mobile.bootstrap_requires_public_edge",
            description="mobile host denies bootstrap without public-edge proof",
            base_url=base_url,
            method="GET",
            path="/client/bootstrap",
            host=mobile_host,
            access_jwt=mobile_access_jwt,
            public_edge_secret=public_edge_secret,
            expected_status=403,
            expected_detail="Public edge assertion is required",
            timeout_seconds=timeout_seconds,
            urlopen=urlopen,
            skip_without_access=True,
            require_public_edge_secret=True,
            send_public_edge_proof=False,
        ),
        _request_check(
            check_id="mobile.sessions_require_sm_auth",
            description="mobile host reaches SM auth boundary after Access proof",
            base_url=base_url,
            method="GET",
            path="/client/sessions",
            host=mobile_host,
            access_jwt=mobile_access_jwt,
            public_edge_secret=public_edge_secret,
            expected_status=401,
            expected_detail="Authentication required",
            timeout_seconds=timeout_seconds,
            urlopen=urlopen,
            skip_without_access=True,
            require_public_edge_secret=True,
        ),
        _request_check(
            check_id="mobile.sessions_with_sm_auth",
            description="mobile host returns native session list with Access and SM auth",
            base_url=base_url,
            method="GET",
            path="/client/sessions",
            host=mobile_host,
            access_jwt=mobile_access_jwt,
            public_edge_secret=public_edge_secret,
            bearer_token=bearer_token,
            cookie=cookie,
            expected_status=200,
            expected_json_type=dict,
            expected_json_keys=("sessions",),
            timeout_seconds=timeout_seconds,
            urlopen=urlopen,
            skip_without_any_auth=True,
            skip_without_access=True,
            require_public_edge_secret=True,
        ),
        _request_check(
            check_id="mobile.app_artifact_metadata",
            description="mobile host serves app artifact metadata with Access proof",
            base_url=base_url,
            method="GET",
            path=f"/apps/{app_name}/meta.json",
            host=mobile_host,
            access_jwt=mobile_access_jwt,
            public_edge_secret=public_edge_secret,
            bearer_token=bearer_token,
            cookie=cookie,
            expected_status=200,
            expected_json_type=dict,
            expected_json_keys=("artifact_hash",),
            timeout_seconds=timeout_seconds,
            urlopen=urlopen,
            skip_without_access=True,
            skip_without_any_auth=True,
            require_public_edge_secret=True,
        ),
        _browser_edge_only_check(
            check_id="browser.auth_session_requires_access",
            description="browser host denies auth-session without browser Access assertion",
            host=browser_host,
        ),
        _browser_edge_only_check(
            check_id="browser.auth_session_with_access",
            description="browser host reaches SM auth-session after browser Access proof",
            host=browser_host,
            access_jwt=browser_access_jwt,
        ),
    ]
    if session_id:
        checks.append(
            _request_check(
                check_id="mobile.session_detail_with_sm_auth",
                description="mobile host returns native session detail with Access and SM auth",
                base_url=base_url,
                method="GET",
                path=f"/client/sessions/{session_id}",
                host=mobile_host,
                access_jwt=mobile_access_jwt,
                public_edge_secret=public_edge_secret,
                bearer_token=bearer_token,
                cookie=cookie,
                expected_status=200,
                expected_json_type=dict,
                expected_json_keys=("id",),
                timeout_seconds=timeout_seconds,
                urlopen=urlopen,
                skip_without_any_auth=True,
                skip_without_access=True,
                require_public_edge_secret=True,
            )
        )

    blockers = []
    for check in checks:
        if check["status"] == "blocked":
            blockers.append(
                {
                    "check_id": check["id"],
                    "kind": check["blocker_kind"],
                    "detail": check["detail"],
                }
            )
        elif check["status"] == "skipped" and check.get("required"):
            blockers.append(
                {
                    "check_id": check["id"],
                    "kind": "required_check_skipped",
                    "detail": check["detail"],
                }
            )
    return {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "inputs": {
            "base_url": base_url,
            "mobile_host": mobile_host,
            "browser_host": browser_host,
            "mobile_access_jwt_supplied": bool(mobile_access_jwt),
            "browser_access_jwt_supplied": bool(browser_access_jwt),
            "public_edge_secret_supplied": bool(public_edge_secret),
            "sm_auth_supplied": bool(bearer_token or cookie),
            "session_id_supplied": bool(session_id),
            "app_name": app_name,
        },
        "status": "blocked" if blockers else "passed",
        "summary": {
            "passed": sum(1 for check in checks if check["status"] == "passed"),
            "blocked": len(blockers),
            "skipped": sum(1 for check in checks if check["status"] == "skipped"),
        },
        "checks": checks,
        "blockers": blockers,
    }


def _request_check(
    *,
    check_id: str,
    description: str,
    base_url: str,
    method: str,
    path: str,
    host: str | None,
    expected_status: int,
    timeout_seconds: float,
    urlopen,
    access_jwt: str | None = None,
    public_edge_secret: str | None = None,
    bearer_token: str | None = None,
    cookie: str | None = None,
    expected_detail: str | None = None,
    expected_json_type: type | None = None,
    expected_json_keys: tuple[str, ...] = (),
    skip_without_access: bool = False,
    skip_without_any_auth: bool = False,
    require_public_edge_secret: bool = False,
    send_public_edge_proof: bool = True,
    required: bool = True,
) -> dict[str, Any]:
    if not host:
        return _skipped(check_id, description, "host was not supplied", required=required)
    if require_public_edge_secret and not public_edge_secret:
        return _skipped(
            check_id,
            description,
            "public edge secret was not supplied",
            required=required,
        )
    if access_jwt is None and "with_access" in check_id:
        return _skipped(
            check_id,
            description,
            "Access assertion was not supplied",
            required=required,
        )
    if access_jwt is None and skip_without_access:
        return _skipped(
            check_id,
            description,
            "Access assertion was not supplied",
            required=required,
        )
    if skip_without_any_auth and not (bearer_token or cookie):
        return _skipped(
            check_id,
            description,
            "SM bearer token or cookie was not supplied",
            required=required,
        )

    headers = {"Host": host}
    if access_jwt:
        headers["Cf-Access-Jwt-Assertion"] = access_jwt
    if bearer_token:
        headers["Authorization"] = f"Bearer {bearer_token}"
    if cookie:
        headers["Cookie"] = cookie
    if public_edge_secret and send_public_edge_proof:
        headers.update(_public_edge_headers(public_edge_secret, method, path))

    request = urllib.request.Request(
        base_url + path,
        method=method,
        headers=headers,
    )
    request_urlopen = urlopen or _urlopen_no_redirect
    try:
        with request_urlopen(request, timeout=timeout_seconds) as response:
            status = response.getcode()
            body = response.read()
            content_type = response.headers.get("content-type")
    except urllib.error.HTTPError as exc:
        status = exc.code
        body = exc.read()
        content_type = exc.headers.get("content-type")
    except Exception as exc:  # noqa: BLE001 - evidence report should capture probe failures.
        return {
            "id": check_id,
            "description": description,
            "status": "blocked",
            "blocker_kind": "request_failed",
            "detail": f"{type(exc).__name__}: {exc}",
            "method": method,
            "path": path,
            "expected_status": expected_status,
            "actual_status": None,
            "response": None,
        }

    actual_json = _parse_json_body(body)
    include_response_body = not (bearer_token or cookie)
    response = _response_summary(
        body,
        content_type,
        parsed_json=actual_json,
        include_json_body=include_response_body,
    )
    if status != expected_status:
        return _blocked(
            check_id,
            description,
            "unexpected_status",
            f"expected HTTP {expected_status}, got HTTP {status}",
            method=method,
            path=path,
            expected_status=expected_status,
            actual_status=status,
            response=response,
            )
    if expected_detail is not None:
        actual_detail = actual_json.get("detail") if isinstance(actual_json, dict) else None
        if actual_detail != expected_detail:
            return _blocked(
                check_id,
                description,
                "unexpected_detail",
                f"expected detail {expected_detail!r}, got {actual_detail!r}",
                method=method,
                path=path,
                expected_status=expected_status,
                actual_status=status,
                response=response,
            )
    if expected_json_type is not None:
        if not isinstance(actual_json, expected_json_type):
            return _blocked(
                check_id,
                description,
                "unexpected_json",
                f"expected JSON {expected_json_type.__name__}, got non-matching response",
                method=method,
                path=path,
                expected_status=expected_status,
                actual_status=status,
                response=response,
            )
        if isinstance(actual_json, dict):
            missing_keys = [key for key in expected_json_keys if key not in actual_json]
            if missing_keys:
                return _blocked(
                    check_id,
                    description,
                    "unexpected_json",
                    f"missing expected JSON keys: {', '.join(missing_keys)}",
                    method=method,
                    path=path,
                    expected_status=expected_status,
                    actual_status=status,
                    response=response,
                )
    return {
        "id": check_id,
        "description": description,
        "status": "passed",
        "method": method,
        "path": path,
        "expected_status": expected_status,
        "actual_status": status,
        "response": response,
    }


def _browser_edge_only_check(
    *,
    check_id: str,
    description: str,
    host: str | None,
    access_jwt: str | None = None,
) -> dict[str, Any]:
    if not host:
        return _skipped(check_id, description, "host was not supplied")
    if access_jwt is None and "with_access" in check_id:
        return _skipped(check_id, description, "Access assertion was not supplied")
    return _skipped(
        check_id,
        description,
        "browser Cloudflare Access is enforced at the edge, not by the Rust origin",
    )


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: ANN001
        return None


_NO_REDIRECT_OPENER = urllib.request.build_opener(_NoRedirectHandler)


def _urlopen_no_redirect(request: urllib.request.Request, *, timeout: float):
    return _NO_REDIRECT_OPENER.open(request, timeout=timeout)


def _public_edge_headers(secret: str, method: str, path: str) -> dict[str, str]:
    timestamp = str(time.time())
    nonce = uuid.uuid4().hex
    message = "\n".join(
        [
            "SM-PUBLIC-EDGE-V1",
            method.upper(),
            path,
            timestamp,
            nonce,
        ]
    )
    signature = hmac.new(secret.encode(), message.encode(), hashlib.sha256).digest()
    return {
        "X-SM-Edge-Timestamp": timestamp,
        "X-SM-Edge-Nonce": nonce,
        "X-SM-Edge-Signature": base64.b64encode(signature).decode("ascii"),
    }


def _parse_json_body(body: bytes) -> Any:
    try:
        return json.loads(body.decode("utf-8"))
    except Exception:  # noqa: BLE001 - body may be non-JSON.
        return None


def _response_summary(
    body: bytes,
    content_type: str | None,
    *,
    parsed_json: Any,
    include_json_body: bool,
) -> dict[str, Any]:
    body_sha256 = hashlib.sha256(body).hexdigest()
    summary: dict[str, Any] = {
        "content_type": content_type,
        "body_sha256": body_sha256,
        "body_bytes": len(body),
    }
    if parsed_json is not None:
        if include_json_body:
            summary["json"] = parsed_json
        else:
            summary["json_redacted"] = True
            summary["json_type"] = type(parsed_json).__name__
            if isinstance(parsed_json, dict):
                summary["json_keys"] = sorted(str(key) for key in parsed_json.keys())
            elif isinstance(parsed_json, list):
                summary["json_length"] = len(parsed_json)
    elif include_json_body:
        summary["text_preview"] = body[:200].decode("utf-8", errors="replace")
    else:
        summary["body_redacted"] = True
    return summary


def _skipped(
    check_id: str,
    description: str,
    detail: str,
    *,
    required: bool = False,
) -> dict[str, Any]:
    return {
        "id": check_id,
        "description": description,
        "status": "skipped",
        "detail": detail,
        "required": required,
    }


def _blocked(
    check_id: str,
    description: str,
    kind: str,
    detail: str,
    *,
    method: str,
    path: str,
    expected_status: int,
    actual_status: int | None,
    response: dict[str, Any] | None,
) -> dict[str, Any]:
    return {
        "id": check_id,
        "description": description,
        "status": "blocked",
        "blocker_kind": kind,
        "detail": detail,
        "method": method,
        "path": path,
        "expected_status": expected_status,
        "actual_status": actual_status,
        "response": response,
    }


def render_text_report(report: dict[str, Any]) -> str:
    lines = [
        "Cloudflare Access mobile smoke report",
        f"status: {report['status']}",
        f"passed: {report['summary']['passed']}",
        f"blocked: {report['summary']['blocked']}",
        f"skipped: {report['summary']['skipped']}",
    ]
    if report["blockers"]:
        lines.extend(["", "Blockers:"])
        for blocker in report["blockers"]:
            lines.append(f"  {blocker['check_id']}: {blocker['kind']} - {blocker['detail']}")
    return "\n".join(lines)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default=DEFAULT_BASE_URL)
    parser.add_argument("--mobile-host", default=None)
    parser.add_argument("--browser-host", default=None)
    _add_secret_source_args(
        parser,
        "mobile-access-jwt",
        "Cloudflare Access JWT for the mobile app host",
    )
    _add_secret_source_args(
        parser,
        "browser-access-jwt",
        "Cloudflare Access JWT for the browser host",
    )
    _add_secret_source_args(
        parser,
        "public-edge-secret",
        "SM public-edge HMAC secret",
    )
    _add_secret_source_args(parser, "bearer-token", "SM device bearer token")
    _add_secret_source_args(parser, "cookie", "SM browser cookie header")
    parser.add_argument("--session-id", default=None)
    parser.add_argument("--app-name", default=DEFAULT_APP_NAME)
    parser.add_argument("--timeout", type=float, default=5.0)
    parser.add_argument("--output", type=Path, default=None)
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--fail-on-blockers", action="store_true")
    return parser


def _add_secret_source_args(parser: argparse.ArgumentParser, name: str, help_text: str) -> None:
    parser.add_argument(
        f"--{name}-env",
        default=None,
        metavar="ENV",
        help=f"read {help_text} from ENV",
    )
    parser.add_argument(
        f"--{name}-file",
        default=None,
        metavar="PATH",
        help=f"read {help_text} from PATH; trailing newline is stripped",
    )


def _resolve_secret(
    *,
    parser: argparse.ArgumentParser,
    label: str,
    env_name: str | None,
    file_path: str | None,
) -> str | None:
    if env_name and file_path:
        parser.error(f"{label}: pass only one of --{label}-env or --{label}-file")
    if env_name:
        value = os.environ.get(env_name)
        if value is None:
            parser.error(f"{label}: environment variable {env_name!r} is not set")
        return value
    if file_path:
        return Path(file_path).read_text(encoding="utf-8").rstrip("\n")
    return None


def main(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    report = build_smoke_report(
        base_url=args.base_url,
        mobile_host=args.mobile_host,
        browser_host=args.browser_host,
        mobile_access_jwt=_resolve_secret(
            parser=parser,
            label="mobile-access-jwt",
            env_name=args.mobile_access_jwt_env,
            file_path=args.mobile_access_jwt_file,
        ),
        browser_access_jwt=_resolve_secret(
            parser=parser,
            label="browser-access-jwt",
            env_name=args.browser_access_jwt_env,
            file_path=args.browser_access_jwt_file,
        ),
        public_edge_secret=_resolve_secret(
            parser=parser,
            label="public-edge-secret",
            env_name=args.public_edge_secret_env,
            file_path=args.public_edge_secret_file,
        ),
        bearer_token=_resolve_secret(
            parser=parser,
            label="bearer-token",
            env_name=args.bearer_token_env,
            file_path=args.bearer_token_file,
        ),
        cookie=_resolve_secret(
            parser=parser,
            label="cookie",
            env_name=args.cookie_env,
            file_path=args.cookie_file,
        ),
        session_id=args.session_id,
        app_name=args.app_name,
        timeout_seconds=args.timeout,
    )
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render_text_report(report))
    if args.fail_on_blockers and report["blockers"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
