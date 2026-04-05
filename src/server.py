"""FastAPI server for hooks and API endpoints."""

import asyncio
import json
import logging
import os
import secrets
import shutil
import subprocess
import tempfile
import time
import shlex
import base64
import hashlib
import hmac
import re
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional, Dict, Any, Literal
from urllib.parse import urlencode, urlparse

import httpx
from google.auth.transport.requests import Request as GoogleAuthRequest
from google.oauth2 import id_token as google_id_token
from fastapi import FastAPI, HTTPException, Body, Request, Query
from fastapi.responses import HTMLResponse, JSONResponse, RedirectResponse, FileResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel, Field
from starlette.middleware.base import BaseHTTPMiddleware
from starlette.middleware.sessions import SessionMiddleware

from .codex_provider_policy import (
    CODEX_APP_RETIRED_SESSION_ERROR,
    CODEX_APP_RETIRED_SESSION_REASON,
    REMOVED_CODEX_SERVER_ENTRYPOINT_MESSAGE,
    get_codex_app_policy,
)
from .models import (
    AdoptionProposal,
    Session,
    SessionStatus,
    NotificationChannel,
    Subagent,
    SubagentStatus,
    DeliveryResult,
)
from .cli.commands import validate_friendly_name
from .cli.dispatch import get_auto_remind_config
from .mobile_analytics import MobileAnalyticsBuilder

logger = logging.getLogger(__name__)

# Delay before retrying a stale transcript read in the Stop hook handler (#184).
TRANSCRIPT_RETRY_DELAY_SECONDS = 0.3
# Delay before retrying an empty transcript read in the Stop hook handler (#230).
EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS = 0.5
LOCAL_TRUSTED_CLIENTS = {"127.0.0.1", "localhost", "::1", "testclient"}
LOCAL_TRUSTED_HOSTS = {"127.0.0.1", "localhost", "::1", "testserver"}
GOOGLE_AUTH_SCOPES = "openid email profile"
DEVICE_TOKEN_MAX_AGE_SECONDS = 60 * 60 * 24 * 14
_EM_SPAWN_STOP_NOTIFY_DELAY_SECONDS = 8
APP_ARTIFACT_NAME_PATTERN = re.compile(r"^[a-z0-9][a-z0-9-]*$")
APP_ARTIFACT_HASH_PATTERN = re.compile(r"^[0-9a-f]{8}$")
APP_ARTIFACT_MAX_SIZE_BYTES = 100 * 1024 * 1024
DEFAULT_APP_ARTIFACTS_ROOT = Path(__file__).resolve().parents[1] / "data" / "apps"


def _is_valid_app_artifact_name(app_name: str) -> bool:
    return bool(APP_ARTIFACT_NAME_PATTERN.fullmatch(app_name))


def _is_valid_app_artifact_hash(artifact_hash: str) -> bool:
    return bool(APP_ARTIFACT_HASH_PATTERN.fullmatch(artifact_hash))


def _write_json_atomically(path: Path, payload: dict[str, Any]) -> None:
    """Write JSON metadata via temp file + replace to avoid partial writes."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp_path = tempfile.mkstemp(dir=str(path.parent), prefix=".tmp-meta-", suffix=".json")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(payload, handle)
        os.replace(temp_path, path)
    except Exception:
        try:
            os.unlink(temp_path)
        except FileNotFoundError:
            pass
        raise


def _copy_file_atomically(source_path: Path, destination_path: Path) -> None:
    """Copy file content via temp file + replace to publish immutable artifacts safely."""
    destination_path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp_path = tempfile.mkstemp(
        dir=str(destination_path.parent),
        prefix=".tmp-artifact-copy-",
        suffix=destination_path.suffix,
    )
    os.close(fd)
    try:
        shutil.copyfile(source_path, temp_path)
        os.replace(temp_path, destination_path)
    except Exception:
        try:
            os.unlink(temp_path)
        except FileNotFoundError:
            pass
        raise


class WatchStaticFiles(StaticFiles):
    """Static file handler that disables caching for HTML entrypoints only."""

    async def get_response(self, path: str, scope):
        response = await super().get_response(path, scope)
        content_type = response.headers.get("content-type", "")
        is_html_entry = content_type.startswith("text/html") or path in {"", ".", "index.html"}
        if response.status_code in {200, 304} and is_html_entry:
            response.headers["Cache-Control"] = "no-store, max-age=0, must-revalidate"
            response.headers["Pragma"] = "no-cache"
        return response


def _google_auth_config(config: Optional[dict]) -> dict:
    """Return normalized Google auth config block."""
    return ((config or {}).get("auth") or {}).get("google") or {}


def _google_auth_requested(config: Optional[dict]) -> bool:
    """Whether operators requested Google auth enforcement."""
    return bool(_google_auth_config(config).get("enabled"))


def _google_auth_ready(config: Optional[dict]) -> bool:
    """Whether external Google auth is fully configured."""
    auth = _google_auth_config(config)
    return bool(
        auth.get("enabled")
        and auth.get("client_id")
        and auth.get("client_secret")
        and auth.get("session_cookie_secret")
        and auth.get("allowlist_emails")
        and auth.get("public_host")
        and auth.get("redirect_uri")
    )


def _request_hostname(request: Request) -> str:
    """Best-effort hostname extraction without port noise."""
    host_value = request.headers.get("host") or request.url.hostname or ""
    return host_value.split(":", 1)[0].strip().lower()


def _is_local_bypass_request(request: Request, config: Optional[dict]) -> bool:
    """Allow only true loopback/test requests to keep working without external auth."""
    client_host = ((request.client.host if request.client else "") or "").strip().lower()
    if client_host not in LOCAL_TRUSTED_CLIENTS:
        return False
    hostname = _request_hostname(request)
    public_host = str(_google_auth_config(config).get("public_host") or "").strip().lower()
    if public_host and hostname == public_host:
        return False
    return hostname in LOCAL_TRUSTED_HOSTS


def _is_safe_next_path(next_path: Optional[str]) -> bool:
    """Allow only relative in-app redirect targets."""
    if not next_path:
        return False
    if not next_path.startswith("/"):
        return False
    parsed = urlparse(next_path)
    return not parsed.scheme and not parsed.netloc


def _google_login_redirect(next_path: Optional[str] = None) -> str:
    """Build the login redirect URL."""
    safe_next = next_path if _is_safe_next_path(next_path) else "/watch/"
    return f"/auth/google/login?{urlencode({'next': safe_next})}"


def _allowed_google_audiences(config: Optional[dict]) -> set[str]:
    auth = _google_auth_config(config)
    audiences = {
        str(auth.get("client_id") or "").strip(),
        str(auth.get("android_client_id") or "").strip(),
    }
    return {aud for aud in audiences if aud}


async def _exchange_google_code(client_id: str, client_secret: str, redirect_uri: str, code: str) -> dict:
    """Exchange an OAuth authorization code for Google tokens."""
    async with httpx.AsyncClient(timeout=10.0) as client:
        response = await client.post(
            "https://oauth2.googleapis.com/token",
            data={
                "code": code,
                "client_id": client_id,
                "client_secret": client_secret,
                "redirect_uri": redirect_uri,
                "grant_type": "authorization_code",
            },
        )
        response.raise_for_status()
        return response.json()


async def _fetch_google_userinfo(access_token: str) -> dict:
    """Fetch OpenID user info for an authenticated Google session."""
    async with httpx.AsyncClient(timeout=10.0) as client:
        response = await client.get(
            "https://openidconnect.googleapis.com/v1/userinfo",
            headers={"Authorization": f"Bearer {access_token}"},
        )
        response.raise_for_status()
        return response.json()


async def _verify_google_id_token(id_token: str) -> dict:
    """Verify a Google ID token locally with Google's official auth library."""
    return await asyncio.to_thread(
        google_id_token.verify_oauth2_token,
        id_token,
        GoogleAuthRequest(),
        None,
    )


def _urlsafe_b64encode(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def _urlsafe_b64decode(data: str) -> bytes:
    padding = "=" * (-len(data) % 4)
    return base64.urlsafe_b64decode((data + padding).encode("ascii"))


def _device_token_secret(config: Optional[dict]) -> Optional[bytes]:
    auth = _google_auth_config(config)
    secret = str(auth.get("session_cookie_secret") or "").strip()
    if not secret:
        return None
    return secret.encode("utf-8")


def _issue_device_access_token(config: Optional[dict], *, email: str, name: Optional[str]) -> Optional[dict[str, Any]]:
    secret = _device_token_secret(config)
    if not secret:
        return None

    now = int(time.time())
    payload = {
        "v": 1,
        "type": "device_access",
        "email": email,
        "name": name,
        "iat": now,
        "exp": now + DEVICE_TOKEN_MAX_AGE_SECONDS,
    }
    payload_b64 = _urlsafe_b64encode(json.dumps(payload, separators=(",", ":"), sort_keys=True).encode("utf-8"))
    signature = _urlsafe_b64encode(hmac.new(secret, payload_b64.encode("ascii"), hashlib.sha256).digest())
    return {
        "access_token": f"smat_{payload_b64}.{signature}",
        "expires_at": datetime.fromtimestamp(payload["exp"], tz=timezone.utc).isoformat(),
    }


def _verify_device_access_token(config: Optional[dict], token: str) -> Optional[dict[str, Any]]:
    secret = _device_token_secret(config)
    if not secret or not token.startswith("smat_"):
        return None

    raw = token[len("smat_"):]
    if "." not in raw:
        return None
    payload_b64, signature = raw.split(".", 1)
    expected = _urlsafe_b64encode(hmac.new(secret, payload_b64.encode("ascii"), hashlib.sha256).digest())
    if not hmac.compare_digest(signature, expected):
        return None

    try:
        payload = json.loads(_urlsafe_b64decode(payload_b64).decode("utf-8"))
    except Exception:
        return None

    if payload.get("type") != "device_access":
        return None
    if int(payload.get("exp", 0)) <= int(time.time()):
        return None
    email = str(payload.get("email") or "").strip().lower()
    if not email:
        return None
    return payload


def _request_bearer_token(request: Request) -> Optional[str]:
    auth_header = str(request.headers.get("authorization") or "").strip()
    if not auth_header.lower().startswith("bearer "):
        return None
    token = auth_header[7:].strip()
    return token or None


def _device_auth_from_request(request: Request, config: Optional[dict]) -> Optional[dict[str, Any]]:
    state_payload = getattr(request.state, "device_auth", None)
    if isinstance(state_payload, dict):
        return state_payload
    bearer_token = _request_bearer_token(request)
    if not bearer_token:
        return None
    return _verify_device_access_token(config, bearer_token)


class GoogleAuthMiddleware(BaseHTTPMiddleware):
    """Protect external routes with Google cookie auth while preserving local loopback access."""

    def __init__(self, app, config: Optional[dict] = None):
        super().__init__(app)
        self.config = config or {}
        self.requested = _google_auth_requested(self.config)
        self.ready = _google_auth_ready(self.config)
        self.exempt_paths = {
            "/",
            "/logged-out",
            "/health",
            "/health/detailed",
            "/auth/google/login",
            "/auth/google/callback",
            "/auth/device/google",
            "/auth/logout",
            "/auth/session",
            "/client/bootstrap",
        }

    async def dispatch(self, request: Request, call_next):
        if not self.requested:
            return await call_next(request)
        if _is_local_bypass_request(request, self.config):
            return await call_next(request)

        path = request.url.path
        if path in self.exempt_paths or path == "/apk" or path.startswith("/apps/"):
            return await call_next(request)

        if not self.ready:
            return JSONResponse(
                status_code=503,
                content={"detail": "Google auth is enabled but incomplete"},
            )

        bearer_token = _request_bearer_token(request)
        if bearer_token:
            payload = _verify_device_access_token(self.config, bearer_token)
            if payload is not None:
                request.state.device_auth = payload
                return await call_next(request)

        session_state = getattr(request, "session", {}) or {}
        if session_state.get("google_authenticated") is True:
            return await call_next(request)

        if path.startswith("/watch"):
            return RedirectResponse(url=_google_login_redirect(request.url.path), status_code=302)

        return JSONResponse(
            status_code=401,
            content={"detail": "Authentication required", "login_url": _google_login_redirect(path)},
        )


def _canonical_session_name(session_manager, session: Session, fallback: str) -> str:
    """Return canonical display identity for a session when the manager can resolve one."""
    getter = getattr(session_manager, "get_effective_session_name", None) if session_manager else None
    if callable(getter):
        display_name = getter(session)
        if isinstance(display_name, str) and display_name:
            return display_name
    return fallback


def _normalize_provider(provider: Optional[str]) -> str:
    """Normalize/validate provider string."""
    if not provider:
        return "claude"
    provider = provider.lower()
    if provider in ("codex-server", "codex-app-server"):
        raise HTTPException(status_code=400, detail=REMOVED_CODEX_SERVER_ENTRYPOINT_MESSAGE)
    if provider in ("codex-app", "codex_app"):
        return "codex-app"
    if provider in ("codex-fork", "codex_fork", "codexfork"):
        return "codex-fork"
    if provider in ("claude", "codex"):
        return provider
    raise HTTPException(status_code=400, detail=f"Invalid provider: {provider}")


class RequestTimingMiddleware(BaseHTTPMiddleware):
    """Log slow requests for debugging."""

    def __init__(self, app, config: Optional[dict] = None):
        super().__init__(app)
        self.config = config or {}

        # Load timing thresholds from config
        timeouts = self.config.get("timeouts", {})
        server_timeouts = timeouts.get("server", {})
        self.slow_threshold = server_timeouts.get("slow_request_threshold_seconds", 1.0)
        self.timing_threshold = server_timeouts.get("request_timing_threshold_seconds", 0.1)

    async def dispatch(self, request: Request, call_next):
        start = time.monotonic()
        response = await call_next(request)
        elapsed = time.monotonic() - start

        # Log slow requests
        if elapsed > self.slow_threshold:
            logger.warning(
                f"SLOW REQUEST: {request.method} {request.url.path} "
                f"took {elapsed:.2f}s"
            )
        elif elapsed > self.timing_threshold:
            logger.info(
                f"Request: {request.method} {request.url.path} "
                f"took {elapsed*1000:.0f}ms"
            )

        return response


class CreateSessionRequest(BaseModel):
    """Request to create a new session."""
    working_dir: str = "~"
    name: Optional[str] = None
    provider: Optional[str] = "claude"
    parent_session_id: Optional[str] = None


class AdoptionProposalResponse(BaseModel):
    """Response payload for a pending or resolved adoption proposal."""
    id: str
    proposer_session_id: str
    proposer_name: Optional[str] = None
    target_session_id: str
    created_at: str
    status: str
    decided_at: Optional[str] = None


class SessionResponse(BaseModel):
    """Response containing session info."""
    id: str
    name: str
    working_dir: str
    status: str
    created_at: str
    last_activity: str
    tmux_session: str
    provider: Optional[str] = "claude"
    friendly_name: Optional[str] = None
    telegram_chat_id: Optional[int] = None
    telegram_thread_id: Optional[int] = None
    current_task: Optional[str] = None
    git_remote_url: Optional[str] = None
    parent_session_id: Optional[str] = None
    last_handoff_path: Optional[str] = None  # Last executed handoff doc path (#203)
    agent_status_text: Optional[str] = None  # Self-reported agent status text (#188)
    agent_status_at: Optional[str] = None  # When agent_status_text was last set (#188)
    agent_task_completed_at: Optional[str] = None  # Last self-reported task completion timestamp
    is_em: bool = False  # EM role flag (#256)
    role: Optional[str] = None  # Role tag (#287)
    activity_state: str = "idle"  # Computed operational state (working/thinking/idle/etc)
    last_tool_call: Optional[str] = None
    last_tool_name: Optional[str] = None
    last_action_summary: Optional[str] = None
    last_action_at: Optional[str] = None
    tokens_used: int = 0
    context_monitor_enabled: bool = False
    pending_adoption_proposals: list[AdoptionProposalResponse] = Field(default_factory=list)
    aliases: list[str] = Field(default_factory=list)
    is_maintainer: bool = False


class AgentRegistrationResponse(BaseModel):
    """Response payload for one live agent registry entry."""
    role: str
    session_id: str
    friendly_name: Optional[str] = None
    provider: Optional[str] = None
    status: str
    activity_state: str = "idle"
    created_at: str


class EnsureMaintainerResponse(BaseModel):
    """Response payload for maintainer auto-bootstrap."""
    created: bool
    session: SessionResponse


class ClientBootstrapResponse(BaseModel):
    """Bootstrap config for generic mobile/native clients."""
    auth: Dict[str, Any]
    external_access: Dict[str, Any]
    session_open_defaults: Dict[str, Any]


class DeviceGoogleAuthRequest(BaseModel):
    """Request body for native Android Google ID token exchange."""
    id_token: str


class DeviceGoogleAuthResponse(BaseModel):
    """Response payload for native Android auth token exchange."""
    access_token: str
    token_type: str = "Bearer"
    expires_at: str
    email: str
    name: Optional[str] = None


class AppArtifactMetadataResponse(BaseModel):
    """Metadata describing the latest published Android client artifact."""
    artifact_hash: str
    size_bytes: int
    uploaded_at: str
    uploaded_by: Optional[str] = None
    version_code: Optional[int] = None
    version_name: Optional[str] = None


class AppArtifactDeployResponse(BaseModel):
    """Response for a successful app artifact upload."""
    ok: bool = True
    app: str
    size_bytes: int
    download_url: str
    artifact_hash: str


class SendInputRequest(BaseModel):
    """Request to send input to a session."""
    text: str
    sender_session_id: Optional[str] = None  # Optional sender identification
    delivery_mode: str = "sequential"  # sequential, important, urgent
    from_sm_send: bool = False  # True if called from sm send command
    timeout_seconds: Optional[int] = None  # Drop message if not delivered in time
    notify_on_delivery: bool = False  # Notify sender when delivered
    notify_after_seconds: Optional[int] = None  # Notify sender N seconds after delivery
    notify_on_stop: bool = False  # Notify sender when receiver's Stop hook fires
    remind_soft_threshold: Optional[int] = None  # Seconds for soft remind after delivery (#188)
    remind_hard_threshold: Optional[int] = None  # Seconds for hard remind after delivery (#188)
    remind_cancel_on_reply_session_id: Optional[str] = None  # Cancel remind when target replies to this session (#406)
    parent_session_id: Optional[str] = None  # EM session to wake periodically after delivery (#225-C)


class CodexRequestRespondRequest(BaseModel):
    """Structured response payload for codex request resolution."""
    decision: Optional[Literal["accept", "acceptForSession", "decline", "cancel"]] = None
    answers: Optional[Dict[str, Any]] = None


class PeriodicRemindRequest(BaseModel):
    """Request to register a periodic remind for a session (#188)."""
    soft_threshold: int
    hard_threshold: int
    cancel_on_reply_session_id: Optional[str] = None


class JobWatchCreateRequest(BaseModel):
    """Request to create a durable external job watch (#377)."""
    target_session_id: str
    label: Optional[str] = None
    pid: Optional[int] = Field(default=None, gt=0)
    file_path: Optional[str] = None
    progress_regex: Optional[str] = None
    done_regex: Optional[str] = None
    error_regex: Optional[str] = None
    exit_code_file: Optional[str] = None
    interval_seconds: int = Field(default=300, gt=0)
    tail_lines: int = Field(default=200, gt=0)
    tail_on_error: int = Field(default=10, gt=0)
    notify_on_change: bool = True


class JobWatchResponse(BaseModel):
    """Response payload for one durable external job watch."""
    id: str
    target_session_id: str
    target_name: Optional[str] = None
    label: str
    pid: Optional[int] = None
    file_path: Optional[str] = None
    progress_regex: Optional[str] = None
    done_regex: Optional[str] = None
    error_regex: Optional[str] = None
    exit_code_file: Optional[str] = None
    interval_seconds: int
    tail_lines: int
    tail_on_error: int
    notify_on_change: bool
    created_at: str
    last_polled_at: Optional[str] = None
    last_notified_at: Optional[str] = None
    last_progress_text: Optional[str] = None
    last_event: Optional[str] = None
    is_active: bool = True


class AgentStatusRequest(BaseModel):
    """Request from an agent to self-report its current status (#188). text=None clears status (#283)."""
    text: Optional[str] = None


class SetRoleRequest(BaseModel):
    """Request to set a session role tag."""
    role: str


class SetMaintainerRequest(BaseModel):
    """Request to register or clear the maintainer alias."""
    requester_session_id: str


class EnsureMaintainerRequest(BaseModel):
    """Request to ensure the maintainer service session exists."""
    requester_session_id: Optional[str] = None


class EnsureRoleRequest(BaseModel):
    """Request to ensure a generic service role session exists."""
    requester_session_id: Optional[str] = None


class RoleRegistrationRequest(BaseModel):
    """Request to register or clear a generic agent registry role."""
    requester_session_id: str
    role: str


class ClearSessionRequest(BaseModel):
    """Request to clear/reset a session context."""
    prompt: Optional[str] = None


class NotifyRequest(BaseModel):
    """Request to send a notification."""
    message: str
    channel: Optional[str] = None  # "telegram" or "email"
    urgent: bool = False


class HookPayload(BaseModel):
    """Payload from Claude Code hooks."""
    # Claude Code hook fields
    hook_type: Optional[str] = None  # "Stop", "Notification", etc.
    session_id: Optional[str] = None
    transcript: Optional[list] = None
    stop_hook_active: Optional[bool] = None
    # For backward compatibility
    event: Optional[str] = None
    data: Optional[dict] = None

    class Config:
        extra = "allow"  # Allow additional fields from Claude


class SubagentStartRequest(BaseModel):
    """Request to register subagent start."""
    agent_id: str
    agent_type: str
    transcript_path: Optional[str] = None


class SubagentStopRequest(BaseModel):
    """Request to register subagent stop."""
    summary: Optional[str] = None


class SubagentResponse(BaseModel):
    """Response containing subagent info."""
    agent_id: str
    agent_type: str
    parent_session_id: str
    started_at: str
    stopped_at: Optional[str] = None
    status: str
    summary: Optional[str] = None


class SpawnChildRequest(BaseModel):
    """Request to spawn a child agent session."""
    parent_session_id: str
    prompt: str
    name: Optional[str] = None
    wait: Optional[int] = None
    model: Optional[str] = None
    working_dir: Optional[str] = None
    provider: Optional[str] = None
    track_seconds: Optional[int] = Field(default=None, gt=0)


class KillSessionRequest(BaseModel):
    """Request to kill a session with ownership check."""
    requester_session_id: Optional[str] = None


class HandoffRequest(BaseModel):
    """Request to schedule a self-directed handoff."""
    requester_session_id: str
    file_path: str


class TaskCompleteRequest(BaseModel):
    """Request to mark a session's task as complete (self-directed)."""
    requester_session_id: str


class CreateAdoptionProposalRequest(BaseModel):
    """Request for an EM session to propose adopting another session."""
    requester_session_id: str


class StartReviewRequest(BaseModel):
    """Start a review on an existing session."""
    mode: str = "branch"
    base_branch: Optional[str] = None
    commit_sha: Optional[str] = None
    custom_prompt: Optional[str] = None
    steer: Optional[str] = None
    wait: Optional[int] = None
    watcher_session_id: Optional[str] = None


class SpawnReviewRequest(BaseModel):
    """Spawn a new session and start a review."""
    parent_session_id: str
    mode: str = "branch"
    base_branch: Optional[str] = None
    commit_sha: Optional[str] = None
    custom_prompt: Optional[str] = None
    steer: Optional[str] = None
    name: Optional[str] = None
    wait: Optional[int] = None
    model: Optional[str] = None
    working_dir: Optional[str] = None


class PRReviewRequest(BaseModel):
    """Trigger @codex review on a GitHub PR."""
    pr_number: int
    repo: Optional[str] = None
    steer: Optional[str] = None
    wait: Optional[int] = None
    caller_session_id: Optional[str] = None


# Health check response models
class HealthCheckResult(BaseModel):
    """Result of a single health check."""
    status: Literal["ok", "warning", "error"]
    message: Optional[str] = None
    details: Optional[Dict[str, Any]] = None


class HealthCheckResponse(BaseModel):
    """Response from detailed health check endpoint."""
    status: Literal["healthy", "degraded", "unhealthy"]
    checks: Dict[str, HealthCheckResult]
    resources: Dict[str, Any]
    timestamp: str


class ContextMonitorRequest(BaseModel):
    """Request to register/deregister context monitoring for a session (#206)."""
    enabled: bool
    notify_session_id: Optional[str] = None  # Session ID to notify; required when enabled=True
    requester_session_id: str  # Required — caller's session ID for ownership check


class ArmStopNotifyRequest(BaseModel):
    """Request to arm stop notification for a session without a queued message (sm#277)."""
    sender_session_id: str  # Session to notify when target stops
    requester_session_id: str  # Must be is_em=True (EM sessions only)
    delay_seconds: int = 0  # Optional delay before the first stop notification fires


def _invalidate_session_cache(app: FastAPI, session_id: str, arm_skip: bool = False) -> None:
    """Clear server-side caches for a session after a context reset.

    Prevents stale cached output and notification state from a previous
    task from leaking into stop-hook notifications for the next task (#167).

    When arm_skip=True (tmux CLI path), increments stop_notify_skip_count so
    the /clear Stop hook is absorbed without consuming stop_notify_sender_id (#174).
    """
    app.state.last_claude_output.pop(session_id, None)
    app.state.pending_stop_notifications.discard(session_id)

    # Canonical cross-provider reset for context clear workflows (#286).
    # arm_skip=True is used by the tmux pre-clear phase to arm the skip fence.
    # Defer field reset until finalize call (arm_skip=False) after clear succeeds.
    if not arm_skip:
        session = app.state.session_manager.get_session(session_id) if app.state.session_manager else None
        if session:
            session.role = None
            session.completion_status = None
            session.agent_status_text = None
            session.agent_status_at = None
            session.agent_task_completed_at = None
            app.state.session_manager._save_state()

    queue_mgr = (
        app.state.session_manager.message_queue_manager
        if app.state.session_manager
        else None
    )
    if queue_mgr:
        if arm_skip:
            # Arm 2 slots only when agent is explicitly known to be running:
            # - existing delivery state (not created by this call) with is_idle=False, AND
            # - session.status == RUNNING (set by mark_session_active on actual delivery)
            # Both conditions required. Either alone is unreliable:
            # - is_idle=False alone: prior clear-only path creates state with default False.
            # - session.status RUNNING alone: persisted status could be stale post-restart.
            # Missing delivery state (first dispatch, post-restart) → 1 slot (sm#263).
            existing_state = queue_mgr.delivery_states.get(session_id)
            session_obj = app.state.session_manager.get_session(session_id) if app.state.session_manager else None
            agent_explicitly_running = (
                existing_state is not None
                and not existing_state.is_idle
                and session_obj is not None
                and session_obj.status == SessionStatus.RUNNING
            )
            # 2 = prev-task Stop hook + /clear Stop hook (both expected when running)
            # 1 = /clear Stop hook only (agent idle → no in-flight prev-task hook)
            slots = 2 if agent_explicitly_running else 1
            state = queue_mgr._get_or_create_state(session_id)
            state.stop_notify_skip_count += slots
            state.skip_count_armed_at = datetime.now()  # sm#232
        else:
            state = queue_mgr.delivery_states.get(session_id)
        if state:
            state.stop_notify_sender_id = None
            state.stop_notify_sender_name = None
            state.last_outgoing_sm_send_target = None
            state.last_outgoing_sm_send_at = None
        # Cancel stale context-monitor notifications from this session (#241)
        queue_mgr.cancel_context_monitor_messages_from(session_id)


async def _handle_em_topic_inheritance(session, session_manager, telegram_bot):
    """
    Inherit the previous EM Telegram forum topic when a session is designated as EM.

    When sm em marks a session as the EM, that session already has a freshly-created
    Telegram topic. This function:
    1. Deletes the newly-created topic
    2. Reopens the previous EM topic (from session_manager.em_topic)
    3. Updates in-memory mappings so messages route to the inherited thread
    4. Posts a "EM session [id] continuing" message
    5. Clears telegram_thread_id from any old EM sessions referencing the same thread

    Implements Fix B from sm#271 spec.
    """
    def _set_session_topic(target_session, chat_id: int, thread_id: Optional[int]) -> None:
        if callable(getattr(type(session_manager), "update_telegram_thread", None)):
            session_manager.update_telegram_thread(target_session.id, chat_id, thread_id)
            return
        target_session.telegram_chat_id = chat_id
        target_session.telegram_thread_id = thread_id
        session_manager._save_state()

    def _mark_topic_deleted(chat_id: int, thread_id: int) -> None:
        if callable(getattr(type(session_manager), "mark_telegram_topic_deleted", None)):
            session_manager.mark_telegram_topic_deleted(chat_id, thread_id, session=session)

    em_topic = session_manager.em_topic
    new_chat_id = session.telegram_chat_id
    new_thread_id = session.telegram_thread_id

    if not em_topic or em_topic.get("chat_id") != new_chat_id:
        # No previous EM topic or different chat — keep the newly-created topic
        session_manager.em_topic = {"chat_id": new_chat_id, "thread_id": new_thread_id}
        session_manager._save_state()
        return

    old_thread_id = em_topic["thread_id"]

    # Step 1: Delete the newly-created topic
    delete_ok = await telegram_bot.delete_forum_topic(new_chat_id, new_thread_id)
    if not delete_ok:
        logger.warning(
            f"EM topic inheritance: failed to delete new topic {new_thread_id}. "
            "Keeping new topic as EM thread."
        )
        session_manager.em_topic = {"chat_id": new_chat_id, "thread_id": new_thread_id}
        session_manager._save_state()
        return

    _mark_topic_deleted(new_chat_id, new_thread_id)

    # Remove stale _topic_sessions entry for the deleted topic (applies to all paths below)
    telegram_bot._topic_sessions.pop((new_chat_id, new_thread_id), None)

    # Step 2: Reopen the old EM topic
    reopen_ok = await telegram_bot.reopen_forum_topic(new_chat_id, old_thread_id)
    if not reopen_ok:
        logger.warning(
            f"EM topic inheritance: failed to reopen old topic {old_thread_id}. "
            "Creating a new topic as EM thread."
        )
        topic_name = (
            f"{_canonical_session_name(session_manager, session, session.friendly_name or 'em')} [{session.id}]"
        )
        brand_new_id = await telegram_bot.create_forum_topic(new_chat_id, topic_name)
        if brand_new_id:
            _set_session_topic(session, new_chat_id, brand_new_id)
            telegram_bot.register_topic_session(new_chat_id, brand_new_id, session.id)
            telegram_bot._session_threads[session.id] = (new_chat_id, brand_new_id)
            session_manager.em_topic = {"chat_id": new_chat_id, "thread_id": brand_new_id}
        else:
            logger.warning(
                f"EM topic inheritance: create_forum_topic returned None after reopen failure. "
                "Session has no valid Telegram topic."
            )
            session.telegram_thread_id = None
        session_manager._save_state()
        return

    # Step 3: Update the session to use the inherited thread_id
    _set_session_topic(session, new_chat_id, old_thread_id)

    # Step 4: Update in-memory mappings for the inherited thread
    telegram_bot.register_topic_session(new_chat_id, old_thread_id, session.id)
    telegram_bot._session_threads[session.id] = (new_chat_id, old_thread_id)

    # Step 5: Clear old EM sessions' telegram_thread_id to prevent them from
    # closing the shared thread when cleanup_session fires (output_monitor.py:506)
    for sid, s in session_manager.sessions.items():
        if sid != session.id and s.telegram_thread_id == old_thread_id and s.telegram_chat_id == new_chat_id:
            _set_session_topic(s, new_chat_id, None)
            telegram_bot._session_threads.pop(sid, None)
            logger.info(f"Cleared old EM session {sid}'s telegram thread reference")

    # Step 6: Post continuation message (non-critical — failure is logged only)
    try:
        await telegram_bot.send_with_fallback(
            chat_id=new_chat_id,
            message=f"EM session [{session.id}] continuing",
            thread_id=old_thread_id,
            session_id=session.id,
        )
    except Exception as e:
        logger.warning(f"EM topic inheritance: failed to post continuation message: {e}")

    # Persist the inherited EM topic
    session_manager.em_topic = {"chat_id": new_chat_id, "thread_id": old_thread_id}
    session_manager._save_state()
    logger.info(
        f"EM session [{session.id}] inherited topic {old_thread_id} "
        f"(deleted new topic {new_thread_id})"
    )


def create_app(
    session_manager=None,
    notifier=None,
    output_monitor=None,
    child_monitor=None,
    config: Optional[dict] = None,
    lifespan=None,
) -> FastAPI:
    """
    Create the FastAPI application.

    Args:
        session_manager: SessionManager instance
        notifier: Notifier instance
        output_monitor: OutputMonitor instance
        child_monitor: ChildMonitor instance
        config: Configuration dictionary
        lifespan: Optional ASGI lifespan context manager

    Returns:
        Configured FastAPI app
    """
    app = FastAPI(
        title="Claude Session Manager",
        description="Manage Claude Code sessions with Telegram/Email notifications",
        version="0.1.0",
        lifespan=lifespan,
    )

    # Store config first so middleware can access it
    app.state.config = config or {}

    if _google_auth_requested(config):
        auth_config = _google_auth_config(config)
        app.add_middleware(GoogleAuthMiddleware, config=config)
        if _google_auth_ready(config):
            app.add_middleware(
                SessionMiddleware,
                secret_key=str(auth_config.get("session_cookie_secret")),
                session_cookie="sm_auth",
                same_site="lax",
                https_only=True,
                max_age=60 * 60 * 24 * 14,
            )

    # Add timing middleware for debugging (with config)
    app.add_middleware(RequestTimingMiddleware, config=config)

    # Store references to components
    app.state.session_manager = session_manager
    app.state.notifier = notifier
    app.state.output_monitor = output_monitor
    app.state.child_monitor = child_monitor
    app.state.infra_supervisor = None
    app.state.last_claude_output = {}  # Store last output per session from hooks
    app.state.pending_stop_notifications = set()  # Sessions where Stop hook had empty transcript
    if notifier is not None:
        setattr(notifier, "session_manager", session_manager)

    attach_infra_cache = {"expires_at": 0.0, "issue": None}

    # Wire _app back-reference so _execute_handoff can clear server-side caches (#196)
    if session_manager:
        session_manager._app = app
        policy_getter = getattr(session_manager, "get_codex_provider_policy", None)
        if callable(policy_getter):
            policy = policy_getter()
            if isinstance(policy, dict) and policy.get("phase") == "post_cutover":
                retire = getattr(session_manager, "retire_codex_app_sessions", None)
                if callable(retire):
                    retired_count = int(retire(reason=CODEX_APP_RETIRED_SESSION_REASON))
                    if retired_count:
                        logger.info("Retired %s codex-app session(s) for post-cutover policy", retired_count)

    def _fallback_activity_state(session: Session) -> str:
        if session.status == SessionStatus.STOPPED:
            return "stopped"
        if session.status == SessionStatus.RUNNING:
            return "working"
        return "idle"

    def _get_activity_state(session: Session) -> str:
        sm = app.state.session_manager
        if not sm:
            return _fallback_activity_state(session)
        getter = getattr(sm, "get_activity_state", None)
        if not callable(getter):
            return _fallback_activity_state(session)
        try:
            state = getter(session)
        except Exception:
            return _fallback_activity_state(session)
        if isinstance(state, str):
            return state
        return _fallback_activity_state(session)

    def _effective_session_name(session: Session) -> str:
        sm = app.state.session_manager
        if sm:
            getter = getattr(sm, "get_effective_session_name", None)
            if callable(getter):
                display_name = getter(session)
                if isinstance(display_name, str) and display_name:
                    return display_name
        return session.friendly_name or session.name or session.id

    def _track_hard_threshold_seconds(track_seconds: int) -> int:
        """Return the hard-threshold cadence for spawn/send --track."""
        return track_seconds * 2

    def _register_spawn_monitoring(
        child_session: Session,
        parent_session: Session,
        *,
        track_seconds: Optional[int],
    ) -> list[str]:
        """Best-effort monitoring registration for a newly spawned child session."""
        warnings: list[str] = []
        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            if track_seconds is not None or parent_session.is_em:
                warnings.append("message queue unavailable; monitoring not registered")
            return warnings

        child_id = child_session.id
        parent_id = parent_session.id
        child_working_dir = child_session.working_dir or parent_session.working_dir

        def _append_warning(message: str, exc: Exception) -> None:
            logger.warning("Spawn monitoring degraded for %s: %s (%s)", child_id, message, exc)
            warnings.append(message)

        if track_seconds is not None:
            try:
                queue_mgr.register_periodic_remind(
                    target_session_id=child_id,
                    soft_threshold=track_seconds,
                    hard_threshold=_track_hard_threshold_seconds(track_seconds),
                    cancel_on_reply_session_id=parent_id,
                    persistent_tracking=True,
                )
            except Exception as exc:
                _append_warning("failed to register spawn tracking", exc)

        if not parent_session.is_em:
            return warnings

        if track_seconds is None:
            try:
                soft_threshold, hard_threshold = get_auto_remind_config(child_working_dir)
            except Exception as exc:
                logger.warning(
                    "Failed to load auto-remind config for spawned child %s in %s: %s",
                    child_id,
                    child_working_dir,
                    exc,
                )
                warnings.append("failed to load EM auto-remind config")
            else:
                try:
                    queue_mgr.register_periodic_remind(
                        target_session_id=child_id,
                        soft_threshold=soft_threshold,
                        hard_threshold=hard_threshold,
                    )
                except Exception as exc:
                    _append_warning("failed to register EM auto-remind", exc)

        child_session.context_monitor_enabled = True
        child_session.context_monitor_notify = parent_id
        child_session._context_warning_sent = False
        child_session._context_critical_sent = False

        if getattr(child_session, "provider", "claude") != "codex-fork":
            try:
                queue_mgr.arm_stop_notify(
                    session_id=child_id,
                    sender_session_id=parent_id,
                    sender_name=_effective_session_name(parent_session),
                    delay_seconds=_EM_SPAWN_STOP_NOTIFY_DELAY_SECONDS,
                )
            except Exception as exc:
                _append_warning("failed to arm EM stop notification", exc)

        try:
            app.state.session_manager._save_state()
        except Exception as exc:
            _append_warning("failed to persist spawn monitoring state", exc)
        return warnings

    def _session_to_response(session: Session) -> SessionResponse:
        provider = getattr(session, "provider", "claude")
        last_action_summary: Optional[str] = None
        last_action_at: Optional[str] = None
        pending_adoption_proposals: list[AdoptionProposalResponse] = []
        if provider == "codex-app" and _codex_rollout_enabled("enable_observability_projection"):
            latest_action_getter = getattr(app.state.session_manager, "get_codex_latest_activity_action", None)
            if callable(latest_action_getter):
                action = latest_action_getter(session.id)
                if action:
                    last_action_summary = action.get("summary_text")
                    last_action_at = action.get("ended_at") or action.get("started_at")

        proposal_getter = getattr(app.state.session_manager, "list_adoption_proposals", None)
        if callable(proposal_getter):
            for proposal in proposal_getter(target_session_id=session.id, status=None):
                if proposal.status.value != "pending":
                    continue
                pending_adoption_proposals.append(_proposal_to_response(proposal))
        alias_getter = getattr(app.state.session_manager, "get_session_aliases", None)
        aliases = alias_getter(session.id) if callable(alias_getter) else []
        is_maintainer = "maintainer" in aliases

        return SessionResponse(
            id=session.id,
            name=session.name,
            working_dir=session.working_dir,
            status=session.status.value,
            created_at=session.created_at.isoformat(),
            last_activity=session.last_activity.isoformat(),
            tmux_session=session.tmux_session,
            provider=provider,
            friendly_name=_effective_session_name(session),
            telegram_chat_id=session.telegram_chat_id,
            telegram_thread_id=session.telegram_thread_id,
            current_task=session.current_task,
            git_remote_url=session.git_remote_url,
            parent_session_id=session.parent_session_id,
            last_handoff_path=session.last_handoff_path,
            agent_status_text=session.agent_status_text,
            agent_status_at=session.agent_status_at.isoformat() if session.agent_status_at else None,
            agent_task_completed_at=(
                session.agent_task_completed_at.isoformat()
                if session.agent_task_completed_at
                else None
            ),
            is_em=session.is_em,
            role=getattr(session, "role", None),
            activity_state=_get_activity_state(session),
            last_tool_call=session.last_tool_call.isoformat() if session.last_tool_call else None,
            last_tool_name=getattr(session, "last_tool_name", None),
            last_action_summary=last_action_summary,
            last_action_at=last_action_at,
            tokens_used=getattr(session, "tokens_used", 0),
            context_monitor_enabled=bool(getattr(session, "context_monitor_enabled", False)),
            pending_adoption_proposals=pending_adoption_proposals,
            aliases=aliases,
            is_maintainer=is_maintainer,
        )

    def _proposal_to_response(proposal: AdoptionProposal) -> AdoptionProposalResponse:
        proposer = app.state.session_manager.get_session(proposal.proposer_session_id)
        proposer_name = None
        if proposer is not None:
            proposer_name = _effective_session_name(proposer)
        return AdoptionProposalResponse(
            id=proposal.id,
            proposer_session_id=proposal.proposer_session_id,
            proposer_name=proposer_name,
            target_session_id=proposal.target_session_id,
            created_at=proposal.created_at.isoformat(),
            status=proposal.status.value,
            decided_at=proposal.decided_at.isoformat() if proposal.decided_at else None,
        )

    def _registration_to_response(registration) -> AgentRegistrationResponse:
        session = app.state.session_manager.get_session(registration.session_id)
        if session is None:
            raise HTTPException(status_code=404, detail="Registered session not found")
        return AgentRegistrationResponse(
            role=registration.role,
            session_id=registration.session_id,
            friendly_name=_effective_session_name(session),
            provider=getattr(session, "provider", "claude"),
            status=session.status.value,
            activity_state=_get_activity_state(session),
            created_at=registration.created_at.isoformat(),
        )

    def _job_watch_to_response(registration) -> JobWatchResponse:
        target_session = app.state.session_manager.get_session(registration.target_session_id)
        return JobWatchResponse(
            id=registration.id,
            target_session_id=registration.target_session_id,
            target_name=_effective_session_name(target_session) if target_session else registration.target_session_id,
            label=registration.label,
            pid=registration.pid,
            file_path=registration.file_path,
            progress_regex=registration.progress_regex,
            done_regex=registration.done_regex,
            error_regex=registration.error_regex,
            exit_code_file=registration.exit_code_file,
            interval_seconds=registration.interval_seconds,
            tail_lines=registration.tail_lines,
            tail_on_error=registration.tail_on_error,
            notify_on_change=registration.notify_on_change,
            created_at=registration.created_at.isoformat(),
            last_polled_at=registration.last_polled_at.isoformat() if registration.last_polled_at else None,
            last_notified_at=registration.last_notified_at.isoformat() if registration.last_notified_at else None,
            last_progress_text=registration.last_progress_text,
            last_event=registration.last_event,
            is_active=registration.is_active,
        )

    def _response_dict(model: BaseModel) -> dict:
        dumper = getattr(model, "model_dump", None)
        if callable(dumper):
            return dumper()
        return model.dict()

    def _external_access_config() -> dict:
        return (app.state.config or {}).get("external_access") or {}

    def _app_artifacts_root() -> Path:
        configured = ((app.state.config or {}).get("paths") or {}).get("app_artifacts_dir")
        if configured:
            return Path(configured).expanduser()
        return DEFAULT_APP_ARTIFACTS_ROOT

    def _app_artifact_dir(app_name: str) -> Path:
        return _app_artifacts_root() / app_name

    def _app_artifact_latest_path(app_name: str) -> Path:
        return _app_artifact_dir(app_name) / "latest.apk"

    def _app_artifact_hashed_path(app_name: str, artifact_hash: str) -> Path:
        return _app_artifact_dir(app_name) / f"{artifact_hash}.apk"

    def _app_artifact_meta_path(app_name: str) -> Path:
        return _app_artifact_dir(app_name) / "meta.json"

    def _read_app_artifact_metadata(app_name: str) -> dict[str, Any]:
        metadata_path = _app_artifact_meta_path(app_name)
        if not metadata_path.exists():
            raise HTTPException(status_code=404, detail="Artifact metadata not found")
        try:
            return json.loads(metadata_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            logger.error("Failed to read artifact metadata for %s: %s", app_name, exc)
            raise HTTPException(status_code=500, detail="Artifact metadata unreadable") from exc

    def _request_actor_email(request: Request) -> Optional[str]:
        device_auth = _device_auth_from_request(request, app.state.config)
        if isinstance(device_auth, dict):
            email = str(device_auth.get("email") or "").strip().lower()
            return email or None
        session_state = getattr(request, "session", {}) or {}
        email = str(session_state.get("google_email") or "").strip().lower()
        if email:
            return email
        if _is_local_bypass_request(request, app.state.config):
            return "local_bypass"
        return None

    def _infra_check(name: str) -> Optional[dict[str, Any]]:
        supervisor = getattr(app.state, "infra_supervisor", None)
        getter = getattr(supervisor, "get_check", None) if supervisor else None
        if not callable(getter):
            return None
        return getter(name)

    def _analytics_health_checks() -> list[dict[str, Any]]:
        supervisor = getattr(app.state, "infra_supervisor", None)
        snapshotter = getattr(supervisor, "snapshot", None) if supervisor else None
        if not callable(snapshotter):
            return []
        snapshot = snapshotter() or {}
        labels = {
            "android_sshd": "Android attach SSHD",
            "tmux_base": "tmux base",
            "ac_caffeinate": "AC caffeinate",
        }
        checks: list[dict[str, Any]] = []
        for key, payload in snapshot.items():
            checks.append(
                {
                    "key": key,
                    "label": labels.get(key, key.replace("_", " ")),
                    "status": payload.get("status"),
                    "message": payload.get("message"),
                }
            )
        return checks

    def _termux_attach_infra_issue() -> Optional[str]:
        now = time.time()
        if now < attach_infra_cache["expires_at"]:
            return attach_infra_cache["issue"]

        public_ssh_host = str((_external_access_config().get("public_ssh_host") or "")).strip()
        if not public_ssh_host:
            return None
        infra_status = _infra_check("android_sshd")
        if not infra_status:
            return None
        details = infra_status.get("details") or {}
        attach_ready = details.get("attach_ready")
        issue = None
        if attach_ready is not True and not (
            attach_ready is None and str(infra_status.get("status") or "").lower() in {"ok", "warning"}
        ):
            issue = str(infra_status.get("message") or "android attach sshd is unavailable")
        attach_infra_cache["issue"] = issue
        attach_infra_cache["expires_at"] = now + 5.0
        return issue

    def _attach_descriptor(session: Session) -> Optional[dict[str, Any]]:
        sm = app.state.session_manager
        getter = getattr(sm, "get_attach_descriptor", None) if sm else None
        if not callable(getter):
            return None
        return getter(session.id)

    def _termux_attach_metadata(session: Session, descriptor: Optional[dict[str, Any]]) -> dict[str, Any]:
        external_access = _external_access_config()
        public_ssh_host = str(external_access.get("public_ssh_host") or "").strip()
        ssh_username = str(external_access.get("ssh_username") or "").strip()
        ssh_proxy_command = str(external_access.get("ssh_proxy_command") or "").strip()
        tmux_session = ""
        if descriptor:
            tmux_session = str(descriptor.get("tmux_session") or "").strip()
        if not tmux_session:
            tmux_session = str(getattr(session, "tmux_session", "") or "").strip()

        if not descriptor:
            return {
                "supported": False,
                "reason": "attach descriptor unavailable",
                "transport": "termux-ssh-tmux",
            }
        if not descriptor.get("attach_supported", True):
            return {
                "supported": False,
                "reason": descriptor.get("message") or "attach not supported",
                "transport": "termux-ssh-tmux",
            }
        if not public_ssh_host or not ssh_username:
            return {
                "supported": False,
                "reason": "external ssh attach is not configured",
                "transport": "termux-ssh-tmux",
            }
        infra_issue = _termux_attach_infra_issue()
        if infra_issue:
            return {
                "supported": False,
                "reason": infra_issue,
                "transport": "termux-ssh-tmux",
            }
        if not tmux_session:
            return {
                "supported": False,
                "reason": "tmux target unavailable",
                "transport": "termux-ssh-tmux",
            }

        ssh_args = ["ssh"]
        if ssh_proxy_command:
            ssh_args.extend(["-o", f"ProxyCommand={ssh_proxy_command}"])
        remote_attach_script = (
            "PATH=/opt/homebrew/bin:/usr/local/bin:/opt/homebrew/sbin:/usr/local/sbin:/usr/bin:/bin:$PATH; "
            "export PATH; "
            "if command -v tmux >/dev/null 2>&1; then "
            "exec tmux attach-session -t \"$SM_TMUX_SESSION\"; "
            "elif [ -x /opt/homebrew/bin/tmux ]; then "
            "exec /opt/homebrew/bin/tmux attach-session -t \"$SM_TMUX_SESSION\"; "
            "elif [ -x /usr/local/bin/tmux ]; then "
            "exec /usr/local/bin/tmux attach-session -t \"$SM_TMUX_SESSION\"; "
            "else echo \"tmux not found on remote host\" >&2; exit 127; fi"
        )
        remote_command = (
            f"SM_TMUX_SESSION={shlex.quote(tmux_session)} "
            f"sh -lc {shlex.quote(remote_attach_script)}"
        )
        ssh_args.extend([
            "-t",
            f"{ssh_username}@{public_ssh_host}",
            remote_command,
        ])

        return {
            "supported": True,
            "transport": "termux-ssh-tmux",
            "ssh_host": public_ssh_host,
            "ssh_username": ssh_username,
            "ssh_proxy_command": ssh_proxy_command or None,
            "ssh_command": shlex.join(ssh_args),
            "tmux_session": tmux_session,
            "runtime_mode": descriptor.get("runtime_mode"),
            "termux_package": "com.termux",
        }

    def _mobile_primary_action(termux_attach: dict[str, Any], descriptor: Optional[dict[str, Any]]) -> dict[str, Any]:
        if termux_attach.get("supported"):
            return {
                "type": "termux_attach",
                "label": "Attach in Termux",
            }
        if descriptor and not descriptor.get("attach_supported", True):
            return {
                "type": "details",
                "label": "View details",
                "reason": descriptor.get("message") or "attach not supported",
            }
        return {
            "type": "details",
            "label": "View details",
        }

    def _mobile_session_payload(session: Session) -> dict[str, Any]:
        base = _response_dict(_session_to_response(session))
        descriptor = _attach_descriptor(session)
        termux_attach = _termux_attach_metadata(session, descriptor)
        base["attach_descriptor"] = descriptor
        base["termux_attach"] = termux_attach
        base["primary_action"] = _mobile_primary_action(termux_attach, descriptor)
        return base

    async def _sync_session_display_identity(session: Session) -> None:
        """Propagate the canonical display name to tmux and Telegram surfaces."""
        display_name = _effective_session_name(session)
        if getattr(session, "provider", "claude") != "codex-app":
            app.state.session_manager.tmux.set_status_bar(session.tmux_session, display_name)
        if session.telegram_thread_id and app.state.notifier:
            success = await app.state.notifier.rename_session_topic(session, display_name)
            if not success:
                logger.warning(f"Failed to rename Telegram topic for session {session.id}")

    def _configure_watch_frontend() -> None:
        """Serve the mobile dashboard if static assets exist in web/sm-watch/dist."""
        project_root = Path(__file__).resolve().parents[1]
        watch_dist = project_root / "web" / "sm-watch" / "dist"

        if not watch_dist.is_dir():
            detail = (
                "sm-watch frontend is not built. "
                "Build with: (cd web/sm-watch && npm install && npm run build)"
            )

            @app.get("/watch", include_in_schema=False)
            async def watch_frontend_not_available():
                return JSONResponse(status_code=503, content={"error": detail})

            @app.get("/watch/{_path:path}", include_in_schema=False)
            async def watch_frontend_not_available_path(_path: str):
                return JSONResponse(status_code=503, content={"error": detail})

            return

        @app.get("/watch", include_in_schema=False)
        async def watch_frontend_root():
            return RedirectResponse(url="/watch/")

        app.mount("/watch", WatchStaticFiles(directory=str(watch_dist), html=True), name="sm_watch")

    _configure_watch_frontend()

    def _codex_rollout_enabled(flag_name: str) -> bool:
        """Read codex rollout gate from SessionManager when available."""
        sm = app.state.session_manager
        if not sm:
            return True
        getter = getattr(sm, "is_codex_rollout_enabled", None)
        if not callable(getter):
            return True
        try:
            return bool(getter(flag_name))
        except Exception:
            return True

    def _codex_provider_policy() -> Dict[str, Any]:
        """Read codex provider mapping policy from SessionManager when available."""
        sm = app.state.session_manager
        if not sm:
            return get_codex_app_policy()
        getter = getattr(sm, "get_codex_provider_policy", None)
        if not callable(getter):
            return get_codex_app_policy()
        try:
            policy = getter()
        except Exception:
            return get_codex_app_policy()
        if isinstance(policy, dict):
            return policy
        return get_codex_app_policy()

    def _codex_app_create_rejection(provider: str) -> Optional[str]:
        """Return provider-mapping rejection text for codex-app creation paths."""
        if provider != "codex-app":
            return None
        policy = _codex_provider_policy()
        if policy.get("allow_create", True):
            return None
        return str(policy.get("rejection_error") or "provider=codex-app is not available")

    def _codex_app_mutation_rejection(session: Session) -> Optional[str]:
        """Return post-cutover retirement rejection for mutating codex-app actions."""
        if getattr(session, "provider", "claude") != "codex-app":
            return None
        policy = _codex_provider_policy()
        if policy.get("phase") != "post_cutover":
            return None
        return CODEX_APP_RETIRED_SESSION_ERROR

    @app.get("/")
    async def root(request: Request):
        """Health check endpoint locally, watch entrypoint externally."""
        if _google_auth_requested(app.state.config) and not _is_local_bypass_request(request, app.state.config):
            return RedirectResponse(url="/watch/", status_code=302)
        return {"status": "ok", "service": "session-manager"}

    @app.get("/auth/session")
    async def auth_session(request: Request):
        """Return current external auth session state."""
        if not _google_auth_requested(app.state.config):
            return {
                "enabled": False,
                "authenticated": True,
                "bypass": True,
                "email": None,
                "name": None,
            }
        if _is_local_bypass_request(request, app.state.config):
            return {
                "enabled": True,
                "authenticated": True,
                "bypass": True,
                "email": None,
                "name": None,
            }
        if not _google_auth_ready(app.state.config):
            return {
                "enabled": True,
                "authenticated": False,
                "bypass": False,
                "email": None,
                "name": None,
                "error": "misconfigured",
            }

        device_auth = _device_auth_from_request(request, app.state.config)
        if isinstance(device_auth, dict):
            return {
                "enabled": True,
                "authenticated": True,
                "bypass": False,
                "email": device_auth.get("email"),
                "name": device_auth.get("name"),
                "auth_type": "device_bearer",
            }

        session_state = getattr(request, "session", {}) or {}
        return {
            "enabled": True,
            "authenticated": session_state.get("google_authenticated") is True,
            "bypass": False,
            "email": session_state.get("google_email"),
            "name": session_state.get("google_name"),
            "auth_type": "browser_session" if session_state.get("google_authenticated") is True else None,
        }

    @app.get("/client/bootstrap", response_model=ClientBootstrapResponse)
    async def client_bootstrap():
        """Return runtime bootstrap config for native/mobile clients."""
        external_access = _external_access_config()
        google_auth = _google_auth_config(app.state.config)
        public_http_host = str(external_access.get("public_http_host") or "").strip()
        public_ssh_host = str(external_access.get("public_ssh_host") or "").strip()
        ssh_username = str(external_access.get("ssh_username") or "").strip()
        google_server_client_id = str(google_auth.get("client_id") or "").strip()
        termux_supported = bool(public_ssh_host and ssh_username and not _termux_attach_infra_issue())

        return ClientBootstrapResponse(
            auth={
                "mode": "browser_session_cookie",
                "session_endpoint": "/auth/session",
                "login_endpoint": "/auth/google/login",
                "logout_endpoint": "/auth/logout",
                "device_auth_endpoint": "/auth/device/google",
                "device_auth_token_type": "Bearer",
                "google_server_client_id": google_server_client_id or None,
            },
            external_access={
                "public_http_host": public_http_host or None,
                "public_ssh_host": public_ssh_host or None,
                "ssh_username": ssh_username or None,
                "termux_attach_supported": termux_supported,
            },
            session_open_defaults={
                "preferred_action": "termux_attach" if termux_supported else "details",
                "termux_package": "com.termux",
            },
        )

    @app.get("/client/analytics/summary")
    async def client_analytics_summary():
        """Return mobile-friendly analytics summary derived from live state and local telemetry."""
        builder = MobileAnalyticsBuilder(app.state.session_manager, app.state.config)
        payload = builder.build_summary()
        payload["health_checks"] = _analytics_health_checks()
        payload["attach_available"] = not bool(_termux_attach_infra_issue())
        return payload

    @app.post("/auth/device/google", response_model=DeviceGoogleAuthResponse)
    async def auth_device_google(request: DeviceGoogleAuthRequest):
        """Exchange a Google ID token for a native-client bearer token."""
        if not _google_auth_ready(app.state.config):
            raise HTTPException(status_code=503, detail="Google auth is not configured")

        try:
            tokeninfo = await _verify_google_id_token(request.id_token)
        except Exception as exc:
            logger.warning("Device Google auth verification failed: %s", exc)
            raise HTTPException(status_code=401, detail="Invalid Google ID token")

        audience = str(tokeninfo.get("aud") or "").strip()
        if audience not in _allowed_google_audiences(app.state.config):
            raise HTTPException(status_code=401, detail="Google ID token audience is not allowed")

        email = str(tokeninfo.get("email") or "").strip().lower()
        verified = str(tokeninfo.get("email_verified") or "").lower() == "true"
        allowlist = {str(item).strip().lower() for item in _google_auth_config(app.state.config).get("allowlist_emails", []) if item}
        if not verified or email not in allowlist:
            raise HTTPException(status_code=403, detail="Google account is not allowlisted")

        issued = _issue_device_access_token(
            app.state.config,
            email=email,
            name=str(tokeninfo.get("name") or email),
        )
        if issued is None:
            raise HTTPException(status_code=503, detail="Device auth signing is not configured")

        return DeviceGoogleAuthResponse(
            access_token=issued["access_token"],
            expires_at=issued["expires_at"],
            email=email,
            name=str(tokeninfo.get("name") or email),
        )

    @app.post("/deploy/{app_name}", response_model=AppArtifactDeployResponse)
    async def deploy_app_artifact(request: Request, app_name: str):
        """Upload the latest Android artifact for an app."""
        if not _is_valid_app_artifact_name(app_name):
            raise HTTPException(status_code=400, detail="Invalid app name")

        actor_email = _request_actor_email(request)
        if actor_email is None:
            raise HTTPException(status_code=401, detail="Authentication required")

        try:
            form = await request.form()
        except Exception as exc:
            raise HTTPException(status_code=400, detail="Expected multipart form upload") from exc

        upload = form.get("file")
        if upload is None or not hasattr(upload, "read"):
            raise HTTPException(status_code=400, detail="Missing multipart field 'file'")

        raw_version_code = str(form.get("version_code") or "").strip()
        version_name = str(form.get("version_name") or "").strip() or None
        version_code: Optional[int] = None
        if raw_version_code:
            try:
                version_code = int(raw_version_code)
            except ValueError as exc:
                raise HTTPException(status_code=400, detail="version_code must be an integer") from exc

        app_dir = _app_artifact_dir(app_name)
        app_dir.mkdir(parents=True, exist_ok=True)
        artifact_path = _app_artifact_latest_path(app_name)
        fd, temp_path = tempfile.mkstemp(dir=str(app_dir), prefix=".tmp-artifact-", suffix=".apk")
        size_bytes = 0
        sha256 = hashlib.sha256()

        try:
            with os.fdopen(fd, "wb") as handle:
                while True:
                    chunk = await upload.read(64 * 1024)
                    if not chunk:
                        break
                    size_bytes += len(chunk)
                    if size_bytes > APP_ARTIFACT_MAX_SIZE_BYTES:
                        raise HTTPException(status_code=413, detail="Artifact exceeds 100 MB limit")
                    sha256.update(chunk)
                    handle.write(chunk)

            if size_bytes <= 0:
                raise HTTPException(status_code=400, detail="Uploaded artifact is empty")

            os.replace(temp_path, artifact_path)
            artifact_hash = sha256.hexdigest()[:8]
            hashed_artifact_path = _app_artifact_hashed_path(app_name, artifact_hash)
            if not hashed_artifact_path.exists():
                _copy_file_atomically(artifact_path, hashed_artifact_path)

            metadata: dict[str, Any] = {
                "artifact_hash": artifact_hash,
                "uploaded_at": datetime.now(timezone.utc).isoformat(),
                "size_bytes": size_bytes,
                "uploaded_by": actor_email,
            }
            if version_code is not None:
                metadata["version_code"] = version_code
            if version_name is not None:
                metadata["version_name"] = version_name
            _write_json_atomically(_app_artifact_meta_path(app_name), metadata)

            return AppArtifactDeployResponse(
                app=app_name,
                size_bytes=size_bytes,
                download_url=f"/apps/{app_name}/latest.apk",
                artifact_hash=artifact_hash,
            )
        except HTTPException:
            try:
                os.unlink(temp_path)
            except FileNotFoundError:
                pass
            raise
        except Exception as exc:
            logger.error("Failed to store app artifact for %s: %s", app_name, exc)
            try:
                os.unlink(temp_path)
            except FileNotFoundError:
                pass
            raise HTTPException(status_code=500, detail="Failed to store artifact") from exc
        finally:
            close_upload = getattr(upload, "close", None)
            if callable(close_upload):
                maybe_coro = close_upload()
                if asyncio.iscoroutine(maybe_coro):
                    await maybe_coro

    @app.get("/apps/{app_name}/latest.apk")
    async def get_latest_app_artifact(app_name: str):
        """Redirect callers to the immutable APK artifact for the current app build."""
        if not _is_valid_app_artifact_name(app_name):
            raise HTTPException(status_code=404, detail="Artifact not found")
        metadata = _read_app_artifact_metadata(app_name)
        artifact_hash = str(metadata.get("artifact_hash") or "").strip().lower()
        if not _is_valid_app_artifact_hash(artifact_hash):
            raise HTTPException(status_code=404, detail="Artifact not found")
        return RedirectResponse(
            url=f"/apps/{app_name}/{artifact_hash}.apk",
            status_code=302,
            headers={"Cache-Control": "no-cache"},
        )

    @app.get("/apps/{app_name}/{artifact_hash}.apk")
    async def get_hashed_app_artifact(app_name: str, artifact_hash: str):
        """Download a content-addressed immutable app artifact."""
        if not _is_valid_app_artifact_name(app_name) or not _is_valid_app_artifact_hash(artifact_hash):
            raise HTTPException(status_code=404, detail="Artifact not found")
        artifact_path = _app_artifact_hashed_path(app_name, artifact_hash)
        if not artifact_path.exists():
            raise HTTPException(status_code=404, detail="Artifact not found")
        response = FileResponse(
            artifact_path,
            media_type="application/vnd.android.package-archive",
            filename=f"{app_name}.apk",
        )
        response.headers["Cache-Control"] = "public, max-age=31536000, immutable"
        return response

    @app.get("/apps/{app_name}/meta.json", response_model=AppArtifactMetadataResponse)
    async def get_app_artifact_metadata(app_name: str):
        """Return metadata for the latest published app artifact."""
        if not _is_valid_app_artifact_name(app_name):
            raise HTTPException(status_code=404, detail="Artifact metadata not found")
        metadata = _read_app_artifact_metadata(app_name)
        return AppArtifactMetadataResponse(**metadata)

    @app.get("/apk")
    async def get_legacy_apk_download():
        """Backward-compatible alias for the session-manager Android artifact."""
        return RedirectResponse(url="/apps/session-manager-android/latest.apk", status_code=302)

    @app.get("/auth/google/login")
    async def google_login(request: Request, next: Optional[str] = Query(default="/watch/")):
        """Start the Google OAuth redirect flow."""
        if not _google_auth_ready(app.state.config):
            raise HTTPException(status_code=503, detail="Google auth is not configured")

        auth_config = _google_auth_config(app.state.config)
        safe_next = next if _is_safe_next_path(next) else "/watch/"
        oauth_state = secrets.token_urlsafe(24)
        request.session["google_oauth_state"] = oauth_state
        request.session["google_post_auth_redirect"] = safe_next

        redirect_uri = str(auth_config["redirect_uri"])
        google_params = urlencode(
            {
                "client_id": str(auth_config["client_id"]),
                "redirect_uri": redirect_uri,
                "response_type": "code",
                "scope": GOOGLE_AUTH_SCOPES,
                "state": oauth_state,
                "access_type": "offline",
                "prompt": "select_account",
            }
        )
        return RedirectResponse(
            url=f"https://accounts.google.com/o/oauth2/v2/auth?{google_params}",
            status_code=302,
        )

    @app.get("/auth/google/callback")
    async def google_callback(
        request: Request,
        state: Optional[str] = None,
        code: Optional[str] = None,
        error: Optional[str] = None,
    ):
        """Finish the Google OAuth flow and establish a signed browser session."""
        if not _google_auth_ready(app.state.config):
            raise HTTPException(status_code=503, detail="Google auth is not configured")

        if error:
            return RedirectResponse(url="/watch/?auth_error=google_denied", status_code=302)
        if not code or not state:
            return RedirectResponse(url="/watch/?auth_error=missing_code", status_code=302)

        session_state = getattr(request, "session", {}) or {}
        expected_state = session_state.get("google_oauth_state")
        if not expected_state or state != expected_state:
            session_state.clear()
            return RedirectResponse(url="/watch/?auth_error=invalid_state", status_code=302)

        auth_config = _google_auth_config(app.state.config)
        try:
            token_payload = await _exchange_google_code(
                client_id=str(auth_config["client_id"]),
                client_secret=str(auth_config["client_secret"]),
                redirect_uri=str(auth_config["redirect_uri"]),
                code=code,
            )
            userinfo = await _fetch_google_userinfo(str(token_payload["access_token"]))
        except Exception as exc:
            logger.warning("Google OAuth callback failed: %s", exc)
            session_state.clear()
            return RedirectResponse(url="/watch/?auth_error=exchange_failed", status_code=302)

        email = str(userinfo.get("email") or "").strip().lower()
        verified = bool(userinfo.get("email_verified"))
        allowlist = {str(item).strip().lower() for item in auth_config.get("allowlist_emails", []) if item}
        if not verified or email not in allowlist:
            logger.warning("Rejected Google login for email=%s verified=%s", email or "<missing>", verified)
            session_state.clear()
            return RedirectResponse(url="/watch/?auth_error=unauthorized_email", status_code=302)

        next_path = session_state.get("google_post_auth_redirect")
        session_state.clear()
        session_state["google_authenticated"] = True
        session_state["google_email"] = email
        session_state["google_name"] = userinfo.get("name") or email
        session_state["google_picture"] = userinfo.get("picture")
        session_state["google_authenticated_at"] = datetime.now(timezone.utc).isoformat()
        return RedirectResponse(
            url=next_path if _is_safe_next_path(next_path) else "/watch/",
            status_code=302,
        )

    @app.get("/logged-out", include_in_schema=False)
    async def logged_out_landing():
        """Public post-logout landing page for external browser flows."""
        if _google_auth_ready(app.state.config):
            follow_up = '<p><a href="/auth/google/login?next=%2Fwatch%2F">Sign in again</a></p>'
        elif _google_auth_requested(app.state.config):
            follow_up = '<p>Google sign-in is not available on this deployment right now.</p>'
        else:
            follow_up = '<p><a href="/watch/">Return to watch</a></p>'

        return HTMLResponse(
            f"""
<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Signed Out</title>
    <style>
      body {{ font-family: -apple-system, BlinkMacSystemFont, sans-serif; background: #f6f8fb; color: #0f172a; margin: 0; }}
      .wrap {{ min-height: 100vh; display: grid; place-items: center; padding: 24px; }}
      .card {{ background: white; border: 1px solid #e2e8f0; border-radius: 18px; padding: 24px; max-width: 420px; box-shadow: 0 10px 30px rgba(15, 23, 42, 0.08); }}
      a {{ color: #0f172a; font-weight: 600; }}
      p {{ line-height: 1.5; color: #475569; }}
    </style>
  </head>
  <body>
    <div class="wrap">
      <div class="card">
        <h1>Signed out</h1>
        <p>Your session-manager browser session is closed.</p>
        {follow_up}
      </div>
    </div>
  </body>
</html>
            """.strip()
        )

    @app.get("/auth/logout")
    async def auth_logout(request: Request, next: Optional[str] = Query(default="/logged-out")):
        """Clear the current browser auth session."""
        session_state = request.scope.get("session")
        if isinstance(session_state, dict):
            session_state.clear()
        safe_next = next if _is_safe_next_path(next) else "/logged-out"
        response = RedirectResponse(url=safe_next, status_code=302)
        response.delete_cookie("sm_auth")
        return response

    @app.get("/health")
    async def health():
        """Health check endpoint."""
        return {"status": "healthy"}

    @app.get("/health/detailed", response_model=HealthCheckResponse)
    async def health_detailed():
        """
        Detailed health check endpoint for monitoring and debugging.

        Performs comprehensive checks on:
        - State file integrity
        - Session consistency (memory vs tmux)
        - Message queue health
        - Component status (telegram, monitors)
        - Resource usage
        """
        checks: Dict[str, HealthCheckResult] = {}
        resources: Dict[str, Any] = {}

        # Track overall status (starts healthy, degrades based on check results)
        overall_status: Literal["healthy", "degraded", "unhealthy"] = "healthy"

        def update_status(check_status: str):
            nonlocal overall_status
            if check_status == "error":
                overall_status = "unhealthy"
            elif check_status == "warning" and overall_status == "healthy":
                overall_status = "degraded"

        # 1. State File Integrity Check
        state_file_check = await _check_state_file(app)
        checks["state_file"] = state_file_check
        update_status(state_file_check.status)

        # 2. Session Consistency Check (memory vs tmux)
        session_check = await _check_session_consistency(app)
        checks["tmux_sessions"] = session_check
        update_status(session_check.status)

        # 3. Message Queue Health Check
        mq_check = await _check_message_queue(app)
        checks["message_queue"] = mq_check
        update_status(mq_check.status)

        # 4. Component Status Checks
        telegram_check = await _check_telegram(app)
        checks["telegram"] = telegram_check
        update_status(telegram_check.status)

        monitor_check = await _check_monitors(app)
        checks["monitors"] = monitor_check
        update_status(monitor_check.status)

        infra_check = await _check_infrastructure(app)
        checks["infrastructure"] = infra_check
        update_status(infra_check.status)

        # 6. Resource Usage
        resources = _get_resource_usage(app)

        return HealthCheckResponse(
            status=overall_status,
            checks=checks,
            resources=resources,
            timestamp=datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        )

    async def _check_state_file(app) -> HealthCheckResult:
        """Check state file integrity."""
        if not app.state.session_manager:
            return HealthCheckResult(
                status="error",
                message="Session manager not configured",
            )

        state_file = app.state.session_manager.state_file
        try:
            if not state_file.exists():
                return HealthCheckResult(
                    status="ok",
                    message="No state file (fresh start)",
                    details={"exists": False},
                )

            with open(state_file) as f:
                data = json.load(f)

            # Validate required structure
            if not isinstance(data, dict):
                return HealthCheckResult(
                    status="error",
                    message="State file is not a valid JSON object",
                )

            sessions = data.get("sessions", [])
            if not isinstance(sessions, list):
                return HealthCheckResult(
                    status="error",
                    message="State file sessions field is not a list",
                )

            return HealthCheckResult(
                status="ok",
                message="State file valid",
                details={
                    "exists": True,
                    "sessions_in_file": len(sessions),
                    "file_size_bytes": state_file.stat().st_size,
                },
            )

        except json.JSONDecodeError as e:
            return HealthCheckResult(
                status="error",
                message=f"State file contains invalid JSON: {e}",
            )
        except Exception as e:
            return HealthCheckResult(
                status="error",
                message=f"Failed to read state file: {e}",
            )

    async def _check_session_consistency(app) -> HealthCheckResult:
        """Check that sessions in memory match tmux sessions."""
        if not app.state.session_manager:
            return HealthCheckResult(
                status="error",
                message="Session manager not configured",
            )

        sm = app.state.session_manager
        memory_sessions = list(sm.sessions.values())

        # Get tmux sessions managed by us (those starting with "claude-" or "codex-")
        try:
            all_tmux_sessions = set(sm.tmux.list_sessions())
            our_tmux_sessions = {
                s for s in all_tmux_sessions
                if s.startswith("claude-") or s.startswith("codex-")
            }
        except Exception as e:
            return HealthCheckResult(
                status="error",
                message=f"Failed to list tmux sessions: {e}",
            )

        # Check for sessions in memory that don't exist in tmux (tmux providers only)
        missing_in_tmux = []
        for session in memory_sessions:
            if getattr(session, "provider", "claude") == "codex-app":
                continue
            if session.status not in (SessionStatus.STOPPED,) and session.tmux_session not in all_tmux_sessions:
                missing_in_tmux.append(session.id)

        # Check for orphaned tmux sessions (in tmux but not in memory)
        memory_tmux_names = {
            s.tmux_session for s in memory_sessions
            if getattr(s, "provider", "claude") != "codex-app"
        }
        orphaned_tmux = list(our_tmux_sessions - memory_tmux_names)

        # Check for duplicate session IDs (should never happen)
        session_ids = [s.id for s in memory_sessions]
        duplicates = [sid for sid in session_ids if session_ids.count(sid) > 1]

        if missing_in_tmux or duplicates:
            return HealthCheckResult(
                status="error",
                message="Session consistency issues found",
                details={
                    "sessions_in_memory": len(memory_sessions),
                    "our_tmux_sessions": len(our_tmux_sessions),
                    "missing_in_tmux": missing_in_tmux,
                    "orphaned_tmux": orphaned_tmux,
                    "duplicate_ids": list(set(duplicates)),
                },
            )

        if orphaned_tmux:
            return HealthCheckResult(
                status="warning",
                message=f"Found {len(orphaned_tmux)} orphaned tmux sessions",
                details={
                    "sessions_in_memory": len(memory_sessions),
                    "our_tmux_sessions": len(our_tmux_sessions),
                    "orphaned_tmux": orphaned_tmux,
                },
            )

        return HealthCheckResult(
            status="ok",
            message="Sessions consistent",
            details={
                "sessions_in_memory": len(memory_sessions),
                "our_tmux_sessions": len(our_tmux_sessions),
                "orphaned_tmux": 0,
            },
        )

    async def _check_message_queue(app) -> HealthCheckResult:
        """Check message queue health."""
        sm = app.state.session_manager
        if not sm or not sm.message_queue_manager:
            return HealthCheckResult(
                status="warning",
                message="Message queue not configured",
            )

        mq = sm.message_queue_manager

        try:
            # Check if database is accessible
            db_path = mq.db_path
            if not db_path.exists():
                return HealthCheckResult(
                    status="warning",
                    message="Message queue database does not exist yet",
                    details={"db_exists": False},
                )

            # Count pending and potentially stuck messages
            import sqlite3
            conn = sqlite3.connect(str(db_path))
            cursor = conn.cursor()

            # Count total pending
            cursor.execute("SELECT COUNT(*) FROM messages WHERE delivered_at IS NULL")
            pending_count = cursor.fetchone()[0]

            # Count stuck messages (queued > 1 hour ago and not delivered)
            cursor.execute("""
                SELECT COUNT(*) FROM messages
                WHERE delivered_at IS NULL
                AND datetime(queued_at) < datetime('now', '-1 hour')
            """)
            stuck_count = cursor.fetchone()[0]

            # Count expired messages still in queue
            cursor.execute("""
                SELECT COUNT(*) FROM messages
                WHERE delivered_at IS NULL
                AND timeout_at IS NOT NULL
                AND datetime(timeout_at) < datetime('now')
            """)
            expired_count = cursor.fetchone()[0]

            conn.close()

            if stuck_count > 0 or expired_count > 0:
                return HealthCheckResult(
                    status="warning",
                    message=f"Found {stuck_count} stuck and {expired_count} expired messages",
                    details={
                        "db_exists": True,
                        "pending": pending_count,
                        "stuck": stuck_count,
                        "expired": expired_count,
                    },
                )

            return HealthCheckResult(
                status="ok",
                message="Message queue healthy",
                details={
                    "db_exists": True,
                    "pending": pending_count,
                    "stuck": 0,
                    "expired": 0,
                },
            )

        except Exception as e:
            return HealthCheckResult(
                status="error",
                message=f"Failed to check message queue: {e}",
            )

    async def _check_telegram(app) -> HealthCheckResult:
        """Check Telegram bot status."""
        notifier = app.state.notifier
        if not notifier:
            return HealthCheckResult(
                status="ok",
                message="Notifier not configured",
                details={"configured": False},
            )

        telegram_bot = getattr(notifier, 'telegram', None)
        if not telegram_bot:
            return HealthCheckResult(
                status="ok",
                message="Telegram not configured",
                details={"configured": False},
            )

        # Check if bot is initialized and running
        bot = getattr(telegram_bot, 'bot', None)
        application = getattr(telegram_bot, 'application', None)

        if not bot or not application:
            return HealthCheckResult(
                status="warning",
                message="Telegram bot not fully initialized",
                details={
                    "configured": True,
                    "bot_initialized": bot is not None,
                    "application_initialized": application is not None,
                },
            )

        # Try to check if bot is running
        is_running = application.running if hasattr(application, 'running') else None

        return HealthCheckResult(
            status="ok",
            message="Telegram bot running",
            details={
                "configured": True,
                "bot_initialized": True,
                "application_running": is_running,
                "tracked_sessions": len(telegram_bot._session_threads),
                "tracked_topics": len(telegram_bot._topic_sessions),
            },
        )

    async def _check_monitors(app) -> HealthCheckResult:
        """Check output and child monitors status."""
        output_monitor = app.state.output_monitor
        child_monitor = app.state.child_monitor

        output_status = {
            "configured": output_monitor is not None,
            "active_tasks": 0,
        }

        if output_monitor:
            output_status["active_tasks"] = len(output_monitor._tasks)

        child_status = {
            "configured": child_monitor is not None,
            "running": False,
        }

        if child_monitor:
            child_status["running"] = getattr(child_monitor, '_running', False)

        # Determine overall status
        if not output_monitor:
            return HealthCheckResult(
                status="warning",
                message="Output monitor not configured",
                details={
                    "output_monitor": output_status,
                    "child_monitor": child_status,
                },
            )

        # Check if active sessions are being monitored
        sm = app.state.session_manager
        if sm:
            active_sessions = [
                s for s in sm.sessions.values()
                if s.status not in (SessionStatus.STOPPED,)
                and getattr(s, "provider", "claude") != "codex-app"
            ]
            monitored = len(output_monitor._tasks)
            if len(active_sessions) > monitored:
                return HealthCheckResult(
                    status="warning",
                    message=f"{len(active_sessions) - monitored} active sessions not being monitored",
                    details={
                        "output_monitor": output_status,
                        "child_monitor": child_status,
                        "active_sessions": len(active_sessions),
                        "monitored_sessions": monitored,
                    },
                )

        return HealthCheckResult(
            status="ok",
            message="Monitors running",
            details={
                "output_monitor": output_status,
                "child_monitor": child_status,
            },
        )

    async def _check_infrastructure(app) -> HealthCheckResult:
        """Check local sidecar infrastructure supervised by SM."""
        supervisor = getattr(app.state, "infra_supervisor", None)
        snapshot_getter = getattr(supervisor, "snapshot", None) if supervisor else None
        if not callable(snapshot_getter):
            return HealthCheckResult(
                status="ok",
                message="Infrastructure supervisor not configured",
                details={"checks": {}},
            )

        snapshot = snapshot_getter() or {}
        if not snapshot:
            return HealthCheckResult(
                status="warning",
                message="Infrastructure supervisor has no status yet",
                details={"checks": {}},
            )

        statuses = {str(item.get("status") or "").lower() for item in snapshot.values()}
        if "error" in statuses:
            status = "error"
            message = "One or more local sidecars are down"
        elif "warning" in statuses:
            status = "warning"
            message = "One or more local sidecars required recovery"
        else:
            status = "ok"
            message = "Local sidecar infrastructure healthy"

        return HealthCheckResult(
            status=status,
            message=message,
            details={"checks": snapshot},
        )

    def _get_resource_usage(app) -> Dict[str, Any]:
        """Get resource usage statistics."""
        resources = {
            "active_sessions": 0,
            "output_cache_size": 0,
            "scheduled_tasks": 0,
            "monitor_tasks": 0,
        }

        # Active sessions
        sm = app.state.session_manager
        if sm:
            resources["active_sessions"] = len([
                s for s in sm.sessions.values()
                if s.status not in (SessionStatus.STOPPED,)
            ])
            resources["total_sessions"] = len(sm.sessions)

        # Output cache size
        if hasattr(app.state, 'last_claude_output'):
            resources["output_cache_size"] = len(app.state.last_claude_output)

        # Scheduled tasks in message queue
        if sm and sm.message_queue_manager:
            mq = sm.message_queue_manager
            if hasattr(mq, '_scheduled_tasks'):
                resources["scheduled_tasks"] = len(mq._scheduled_tasks)

        # Monitor tasks
        if app.state.output_monitor:
            resources["monitor_tasks"] = len(app.state.output_monitor._tasks)

        return resources

    @app.post("/sessions", response_model=SessionResponse)
    async def create_session(request: CreateSessionRequest):
        """Create a new Claude Code session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        provider = _normalize_provider(request.provider)
        creation_rejection = _codex_app_create_rejection(provider)
        if creation_rejection:
            raise HTTPException(status_code=400, detail=creation_rejection)
        session = await app.state.session_manager.create_session(
            working_dir=request.working_dir,
            name=request.name,
            provider=provider,
            parent_session_id=request.parent_session_id,
        )

        if not session:
            raise HTTPException(status_code=500, detail="Failed to create session")

        # Start monitoring the session (tmux providers only)
        if app.state.output_monitor and getattr(session, "provider", "claude") != "codex-app":
            await app.state.output_monitor.start_monitoring(session)

        return _session_to_response(session)

    @app.post("/sessions/create")
    async def create_session_endpoint(
        working_dir: str,
        provider: str = "claude",
        parent_session_id: Optional[str] = None,
    ):
        """
        Create a new Claude Code session.

        Args:
            working_dir: Absolute path to working directory

        Returns:
            Session object dict
        """
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        # Create session using config settings
        provider = _normalize_provider(provider)
        creation_rejection = _codex_app_create_rejection(provider)
        if creation_rejection:
            raise HTTPException(status_code=400, detail=creation_rejection)
        session = await app.state.session_manager.create_session(
            working_dir=working_dir,
            telegram_chat_id=None,  # No Telegram association
            provider=provider,
            parent_session_id=parent_session_id,
        )

        if not session:
            raise HTTPException(status_code=500, detail="Failed to create session")

        # Start monitoring (tmux providers only)
        if app.state.output_monitor and getattr(session, "provider", "claude") != "codex-app":
            await app.state.output_monitor.start_monitoring(session)

        return session.to_dict()

    @app.get("/sessions")
    async def list_sessions(include_stopped: bool = Query(default=False)):
        """List all active sessions."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        sessions = app.state.session_manager.list_sessions(include_stopped=include_stopped)

        return {
            "sessions": [
                _session_to_response(s)
                for s in sessions
            ]
        }

    @app.get("/client/sessions")
    async def list_client_sessions():
        """List sessions with mobile-friendly attach metadata."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        sessions = app.state.session_manager.list_sessions()
        return {
            "sessions": [_mobile_session_payload(session) for session in sessions]
        }

    @app.get("/sessions/{session_id}/attach-descriptor")
    async def get_attach_descriptor(session_id: str):
        """Get provider-specific attach metadata for detached-runtime reattach."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")
        getter = getattr(app.state.session_manager, "get_attach_descriptor", None)
        if not callable(getter):
            raise HTTPException(status_code=503, detail="attach descriptor unavailable")
        descriptor = getter(session_id)
        if descriptor is None:
            raise HTTPException(status_code=404, detail="Session not found")
        return {"attach": descriptor}

    @app.get("/sessions/context-monitor")
    async def get_context_monitor_status():
        """List sessions with context monitoring enabled (#206)."""
        if not app.state.session_manager:
            return {"monitored": []}
        monitored = [
            {
                "session_id": s.id,
                "friendly_name": s.friendly_name,
                "notify_session_id": s.context_monitor_notify,
            }
            for s in app.state.session_manager.sessions.values()
            if s.context_monitor_enabled
        ]
        return {"monitored": monitored}

    @app.get("/sessions/{session_id}", response_model=SessionResponse)
    async def get_session(session_id: str):
        """Get session details."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)

        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        return _session_to_response(session)

    @app.get("/client/sessions/{session_id}")
    async def get_client_session(session_id: str):
        """Get one session with mobile-friendly attach metadata."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        return _mobile_session_payload(session)

    @app.get("/sessions/{session_id}/codex-events")
    async def get_codex_events(
        session_id: str,
        since_seq: Optional[int] = Query(default=None, ge=0),
        limit: int = Query(default=200, ge=1, le=500),
    ):
        """Get persisted codex lifecycle events for a codex-app session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")
        if getattr(session, "provider", "claude") != "codex-app":
            raise HTTPException(status_code=400, detail="codex-events supported only for provider=codex-app")
        if not _codex_rollout_enabled("enable_durable_events"):
            raise HTTPException(status_code=503, detail="codex durable events disabled by rollout flag")

        getter = getattr(app.state.session_manager, "get_codex_events", None)
        if not callable(getter):
            raise HTTPException(status_code=503, detail="Codex events store not configured")

        return getter(session_id=session_id, since_seq=since_seq, limit=limit)

    @app.get("/sessions/{session_id}/activity-actions")
    async def get_codex_activity_actions(
        session_id: str,
        limit: int = Query(default=20, ge=1, le=200),
    ):
        """Get provider-neutral projected activity actions for a codex-app session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")
        if getattr(session, "provider", "claude") != "codex-app":
            raise HTTPException(status_code=400, detail="activity actions supported only for provider=codex-app")
        if not _codex_rollout_enabled("enable_observability_projection"):
            raise HTTPException(status_code=503, detail="codex activity projection disabled by rollout flag")

        getter = getattr(app.state.session_manager, "get_codex_activity_actions", None)
        if not callable(getter):
            raise HTTPException(status_code=503, detail="Codex activity projection not configured")

        return {"actions": getter(session_id=session_id, limit=limit)}

    @app.get("/sessions/{session_id}/codex-pending-requests")
    async def list_codex_pending_requests(
        session_id: str,
        include_orphaned: bool = Query(default=False),
    ):
        """List pending structured codex requests for a codex-app session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")
        if getattr(session, "provider", "claude") != "codex-app":
            raise HTTPException(status_code=400, detail="codex requests supported only for provider=codex-app")
        if not _codex_rollout_enabled("enable_structured_requests"):
            raise HTTPException(status_code=503, detail="codex structured requests disabled by rollout flag")

        lister = getattr(app.state.session_manager, "list_codex_pending_requests", None)
        if not callable(lister):
            raise HTTPException(status_code=503, detail="Codex request ledger not configured")

        return {
            "requests": lister(session_id=session_id, include_orphaned=include_orphaned),
        }

    @app.post("/sessions/{session_id}/codex-requests/{request_id}/respond")
    async def respond_codex_request(
        session_id: str,
        request_id: str,
        request: CodexRequestRespondRequest,
    ):
        """Resolve one pending codex structured request by request id."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")
        if getattr(session, "provider", "claude") != "codex-app":
            raise HTTPException(status_code=400, detail="codex requests supported only for provider=codex-app")
        mutation_rejection = _codex_app_mutation_rejection(session)
        if mutation_rejection:
            raise HTTPException(
                status_code=410,
                detail={
                    "error_code": CODEX_APP_RETIRED_SESSION_REASON,
                    "message": mutation_rejection,
                },
            )
        if not _codex_rollout_enabled("enable_structured_requests"):
            raise HTTPException(status_code=503, detail="codex structured requests disabled by rollout flag")

        resolver = getattr(app.state.session_manager, "respond_codex_request", None)
        if not callable(resolver):
            raise HTTPException(status_code=503, detail="Codex request ledger not configured")

        response_payload: Optional[dict[str, Any]] = None
        if request.decision is not None and request.answers is not None:
            raise HTTPException(
                status_code=422,
                detail="response payload must include exactly one of decision or answers",
            )
        if request.decision is not None:
            response_payload = {"decision": request.decision}
        elif request.answers is not None:
            response_payload = {"answers": request.answers}
        else:
            raise HTTPException(
                status_code=422,
                detail="response payload must include exactly one of decision or answers",
            )

        result = await resolver(session_id=session_id, request_id=request_id, response_payload=response_payload)
        if not result.get("ok"):
            raise HTTPException(
                status_code=result.get("http_status", 400),
                detail={
                    "error_code": result.get("error_code", "request_resolution_failed"),
                    "message": result.get("error_message", "failed to resolve request"),
                },
            )

        return result

    @app.patch("/sessions/{session_id}", response_model=SessionResponse)
    async def update_session(
        session_id: str,
        friendly_name: Optional[str] = Body(None, embed=True),
        is_em: Optional[bool] = Body(None, embed=True),
    ):
        """Update session metadata (friendly_name and/or is_em role flag)."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        if friendly_name is not None:
            # Validate friendly name
            valid, error = validate_friendly_name(friendly_name)
            if not valid:
                raise HTTPException(status_code=400, detail=error)

            validator = getattr(app.state.session_manager, "validate_friendly_name_update", None)
            if callable(validator):
                identity_error = validator(session_id, friendly_name)
                if identity_error:
                    raise HTTPException(status_code=400, detail=identity_error)

            setter = getattr(app.state.session_manager, "set_session_friendly_name", None)
            updated = setter(session, friendly_name, explicit=True) if callable(setter) else False
            if updated is not True:
                session.friendly_name = friendly_name
                session.friendly_name_is_explicit = True

        if is_em is not None:
            session.is_em = is_em

            if is_em:
                session.role = "em"
                # Clear is_em from any other session (only one EM at a time)
                for sid, s in app.state.session_manager.sessions.items():
                    if sid != session_id and s.is_em:
                        s.is_em = False
                        if getattr(s, "role", None) == "em":
                            s.role = None

                # Handle EM topic inheritance (Fix B: sm#271)
                notifier = getattr(app.state, 'notifier', None)
                telegram_bot = getattr(notifier, 'telegram', None) if notifier else None
                if telegram_bot and session.telegram_thread_id and session.telegram_chat_id:
                    await _handle_em_topic_inheritance(
                        session, app.state.session_manager, telegram_bot
                    )
            elif getattr(session, "role", None) == "em":
                session.role = None

        if friendly_name is not None or is_em is not None:
            app.state.session_manager._save_state()

        if friendly_name is not None:
            await _sync_session_display_identity(session)

        return _session_to_response(session)

    @app.put("/sessions/{session_id}/role", response_model=SessionResponse)
    async def set_session_role(session_id: str, request: SetRoleRequest):
        """Set a session role tag."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        role = (request.role or "").strip()
        if not role:
            raise HTTPException(status_code=400, detail="role cannot be empty")
        if role.lower() == "em":
            raise HTTPException(status_code=400, detail='role "em" must be set via sm em')
        if session.is_em:
            raise HTTPException(
                status_code=400,
                detail="cannot override role for active EM session; use PATCH /sessions/{id} with is_em=false first",
            )

        setter = getattr(app.state.session_manager, "set_role", None)
        if callable(setter):
            setter(session_id, role)
        else:
            session.role = role
            app.state.session_manager._save_state()

        return _session_to_response(session)

    @app.delete("/sessions/{session_id}/role", response_model=SessionResponse)
    async def clear_session_role(session_id: str):
        """Clear a session role tag."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        clearer = getattr(app.state.session_manager, "clear_role", None)
        if callable(clearer):
            clearer(session_id)
        else:
            session.role = None
            app.state.session_manager._save_state()

        return _session_to_response(session)

    @app.put("/sessions/{session_id}/maintainer", response_model=SessionResponse)
    async def set_session_maintainer(session_id: str, request: SetMaintainerRequest):
        """Register the current session as the durable maintainer alias."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        if request.requester_session_id != session_id:
            raise HTTPException(status_code=400, detail="sm maintainer is self-directed only")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        setter = getattr(app.state.session_manager, "set_maintainer_session", None)
        if not callable(setter):
            raise HTTPException(status_code=503, detail="Maintainer registry unavailable")
        if not setter(session_id):
            raise HTTPException(status_code=400, detail="Failed to register maintainer")
        await _sync_session_display_identity(session)
        return _session_to_response(session)

    @app.delete("/sessions/{session_id}/maintainer", response_model=SessionResponse)
    async def clear_session_maintainer(session_id: str, request: SetMaintainerRequest):
        """Clear the durable maintainer alias owned by this session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        if request.requester_session_id != session_id:
            raise HTTPException(status_code=400, detail="sm maintainer --clear is self-directed only")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        clearer = getattr(app.state.session_manager, "clear_maintainer_session", None)
        if not callable(clearer) or not clearer(session_id):
            raise HTTPException(status_code=400, detail="Session is not the active maintainer")
        await _sync_session_display_identity(session)
        return _session_to_response(session)

    @app.post("/maintainer/ensure", response_model=EnsureMaintainerResponse)
    async def ensure_maintainer(request: EnsureMaintainerRequest):
        """Ensure the maintainer service session exists, bootstrapping it when absent."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        ensurer = getattr(app.state.session_manager, "ensure_maintainer_session", None)
        if not callable(ensurer):
            raise HTTPException(status_code=503, detail="Maintainer bootstrap unavailable")

        try:
            session, created = await ensurer()
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        except RuntimeError as exc:
            raise HTTPException(status_code=500, detail=str(exc)) from exc

        if created and app.state.output_monitor and getattr(session, "provider", "claude") != "codex-app":
            await app.state.output_monitor.start_monitoring(session)
        await _sync_session_display_identity(session)

        return EnsureMaintainerResponse(
            created=created,
            session=_session_to_response(session),
        )

    @app.post("/registry/{role}/ensure", response_model=EnsureMaintainerResponse)
    async def ensure_agent_registry_role(role: str, request: EnsureRoleRequest):
        """Ensure one configured auto-bootstrap registry role session exists."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        ensurer = getattr(app.state.session_manager, "ensure_role_session", None)
        if not callable(ensurer):
            raise HTTPException(status_code=503, detail="Role bootstrap unavailable")

        try:
            session, created = await ensurer(role)
        except ValueError as exc:
            detail = str(exc)
            status_code = 404 if "not configured for auto-bootstrap" in detail else 400
            raise HTTPException(status_code=status_code, detail=detail) from exc
        except RuntimeError as exc:
            raise HTTPException(status_code=500, detail=str(exc)) from exc

        if created and app.state.output_monitor and getattr(session, "provider", "claude") != "codex-app":
            await app.state.output_monitor.start_monitoring(session)
        await _sync_session_display_identity(session)

        return EnsureMaintainerResponse(
            created=created,
            session=_session_to_response(session),
        )

    @app.get("/registry")
    async def list_agent_registry():
        """List live agent registry roles."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        lister = getattr(app.state.session_manager, "list_agent_registrations", None)
        if not callable(lister):
            raise HTTPException(status_code=503, detail="Agent registry unavailable")

        registrations = [_registration_to_response(registration) for registration in lister()]
        return {"registrations": [_response_dict(registration) for registration in registrations]}

    @app.get("/registry/{role}", response_model=AgentRegistrationResponse)
    async def lookup_agent_registry(role: str):
        """Resolve one registry role to the owning live session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        getter = getattr(app.state.session_manager, "lookup_agent_registration", None)
        if not callable(getter):
            raise HTTPException(status_code=503, detail="Agent registry unavailable")

        registration = getter(role)
        if registration is None:
            raise HTTPException(status_code=404, detail="Role not registered")
        return _registration_to_response(registration)

    @app.post("/sessions/{session_id}/registry", response_model=AgentRegistrationResponse)
    async def register_agent_role(session_id: str, request: RoleRegistrationRequest):
        """Register the current session for one registry role."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        if request.requester_session_id != session_id:
            raise HTTPException(status_code=400, detail="sm register is self-directed only")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        registrar = getattr(app.state.session_manager, "register_agent_role", None)
        if not callable(registrar):
            raise HTTPException(status_code=503, detail="Agent registry unavailable")

        try:
            registration = registrar(session_id, request.role)
        except ValueError as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc
        await _sync_session_display_identity(session)
        return _registration_to_response(registration)

    @app.delete("/sessions/{session_id}/registry", response_model=AgentRegistrationResponse)
    async def unregister_agent_role(session_id: str, request: RoleRegistrationRequest):
        """Remove one registry role owned by the current session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        if request.requester_session_id != session_id:
            raise HTTPException(status_code=400, detail="sm unregister is self-directed only")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        getter = getattr(app.state.session_manager, "lookup_agent_registration", None)
        clearer = getattr(app.state.session_manager, "unregister_agent_role", None)
        if not callable(getter) or not callable(clearer):
            raise HTTPException(status_code=503, detail="Agent registry unavailable")

        registration = getter(request.role)
        if registration is None:
            raise HTTPException(status_code=404, detail="Role not registered")
        if registration.session_id != session_id:
            raise HTTPException(status_code=409, detail="Role is not owned by this session")
        response = _registration_to_response(registration)
        clearer(session_id, request.role)
        await _sync_session_display_identity(session)
        return response

    @app.post("/sessions/{session_id}/context-monitor")
    async def set_context_monitor(session_id: str, request: ContextMonitorRequest):
        """Enable or disable context monitoring for a session (#206)."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        if request.enabled and not request.notify_session_id:
            raise HTTPException(status_code=422, detail="notify_session_id required when enabling")

        # requester_session_id is a required field (str, not Optional).
        # Pydantic rejects requests missing it with 422 before this code runs.
        # Ownership check: requester must be self or the session's parent.
        is_self = (request.requester_session_id == session_id)
        is_parent = (session.parent_session_id == request.requester_session_id)
        if not is_self and not is_parent:
            raise HTTPException(
                status_code=403,
                detail="Cannot configure context monitor — not your session or child session",
            )

        # Validate notify target exists (prevents silent black-holing)
        if request.enabled and request.notify_session_id:
            notify_session = app.state.session_manager.get_session(request.notify_session_id)
            if not notify_session:
                raise HTTPException(
                    status_code=422,
                    detail=f"notify_session_id {request.notify_session_id!r} not found",
                )

        session.context_monitor_enabled = request.enabled
        session.context_monitor_notify = request.notify_session_id if request.enabled else None

        # Re-arm one-shot flags when enabling so warnings fire fresh in the new cycle.
        # If re-enabled after a period of being disabled (during which compaction may have
        # fired unobserved), stale flag state would suppress the first warning.
        if request.enabled:
            session._context_warning_sent = False
            session._context_critical_sent = False

        app.state.session_manager._save_state()
        return {"status": "ok", "enabled": session.context_monitor_enabled}

    @app.post("/sessions/{session_id}/notify-on-stop")
    async def arm_stop_notify(session_id: str, request: ArmStopNotifyRequest):
        """Arm stop notification for a session without a queued message (sm#277).

        Used by sm spawn when the parent is an EM: arms the spawned child to notify the EM
        on stop without requiring a message delivery through the queue.
        """
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        # Only EM sessions may arm stop notify (mirrors the is_em guard in send_input)
        requester = app.state.session_manager.get_session(request.requester_session_id)
        if not requester or not requester.is_em:
            raise HTTPException(
                status_code=403,
                detail="Only EM sessions (is_em=True) may arm stop notifications",
            )

        # Requester must be the parent of the target session
        if session.parent_session_id != request.requester_session_id:
            raise HTTPException(
                status_code=403,
                detail="Cannot arm stop notify — not the parent of target session",
            )

        # Validate sender session exists
        sender = app.state.session_manager.get_session(request.sender_session_id)
        if not sender:
            raise HTTPException(
                status_code=422,
                detail=f"sender_session_id {request.sender_session_id!r} not found",
            )

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            raise HTTPException(status_code=503, detail="Message queue not configured")

        if getattr(session, "provider", "claude") == "codex-fork":
            return {
                "status": "suppressed",
                "session_id": session_id,
                "sender_session_id": request.sender_session_id,
                "reason": "notify_on_stop disabled for codex-fork sessions",
            }

        sender_name = _effective_session_name(sender)
        queue_mgr.arm_stop_notify(
            session_id=session_id,
            sender_session_id=request.sender_session_id,
            sender_name=sender_name,
            delay_seconds=max(0, int(request.delay_seconds)),
        )

        return {"status": "ok", "session_id": session_id, "sender_session_id": request.sender_session_id}

    @app.put("/sessions/{session_id}/task")
    async def update_task(session_id: str, task: str = Body(..., embed=True)):
        """Register what the session is currently working on."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        session.current_task = task
        app.state.session_manager._save_state()

        return {"session_id": session_id, "task": task}

    @app.post("/sessions/{session_id}/input")
    async def send_input(session_id: str, request: SendInputRequest):
        """Send input to a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        mutation_rejection = _codex_app_mutation_rejection(session)
        if mutation_rejection:
            raise HTTPException(
                status_code=410,
                detail={
                    "error_code": CODEX_APP_RETIRED_SESSION_REASON,
                    "message": mutation_rejection,
                },
            )

        if getattr(session, "provider", "claude") == "codex-app":
            has_pending = getattr(app.state.session_manager, "has_pending_codex_requests", None)
            oldest_pending = getattr(app.state.session_manager, "oldest_pending_codex_request", None)
            structured_gate_enabled = _codex_rollout_enabled("enable_structured_requests")
            if structured_gate_enabled and callable(has_pending) and has_pending(session_id):
                summary = oldest_pending(session_id) if callable(oldest_pending) else None
                raise HTTPException(
                    status_code=409,
                    detail={
                        "error_code": "pending_structured_request",
                        "message": "structured codex request pending; resolve request before chat input",
                        "pending_request": summary,
                    },
                )

        result = await app.state.session_manager.send_input(
            session_id,
            request.text,
            sender_session_id=request.sender_session_id,
            delivery_mode=request.delivery_mode,
            from_sm_send=request.from_sm_send,
            timeout_seconds=request.timeout_seconds,
            notify_on_delivery=request.notify_on_delivery,
            notify_after_seconds=request.notify_after_seconds,
            notify_on_stop=request.notify_on_stop,
            remind_soft_threshold=request.remind_soft_threshold,
            remind_hard_threshold=request.remind_hard_threshold,
            remind_cancel_on_reply_session_id=request.remind_cancel_on_reply_session_id,
            parent_session_id=request.parent_session_id,
        )

        if result == DeliveryResult.FAILED:
            raise HTTPException(status_code=500, detail="Failed to send input")

        # Update activity in monitor
        if app.state.output_monitor:
            app.state.output_monitor.update_activity(session_id)

        # Return delivery result with queue info if queued
        response = {
            "status": result.value,  # "delivered", "queued", or "failed"
            "session_id": session_id,
            "delivery_mode": request.delivery_mode,
        }

        if result == DeliveryResult.QUEUED:
            queue_mgr = app.state.session_manager.message_queue_manager
            if queue_mgr:
                queue_len = queue_mgr.get_queue_length(session_id)
                response["queue_position"] = queue_len
                response["estimated_delivery"] = "deferred"

        return response

    @app.post("/sessions/{session_id}/key")
    async def send_key(session_id: str, key: str = Body(..., embed=True)):
        """Send a single key to a session (e.g., 'y', 'n')."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        success = app.state.session_manager.send_key(session_id, key)

        if not success:
            raise HTTPException(status_code=500, detail="Failed to send key")

        if app.state.output_monitor:
            app.state.output_monitor.update_activity(session_id)

        return {"status": "sent", "session_id": session_id, "key": key}

    @app.post("/sessions/{session_id}/clear")
    async def clear_session(session_id: str, request: ClearSessionRequest):
        """Clear/reset a session's context."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")
        mutation_rejection = _codex_app_mutation_rejection(session)
        if mutation_rejection:
            raise HTTPException(
                status_code=410,
                detail={
                    "error_code": CODEX_APP_RETIRED_SESSION_REASON,
                    "message": mutation_rejection,
                },
            )

        success = await app.state.session_manager.clear_session(session_id, request.prompt)
        if not success:
            raise HTTPException(status_code=500, detail="Failed to clear session")

        # Invalidate server-side caches so stale state from the previous task
        # doesn't leak into stop-hook notifications for the next task (#167)
        _invalidate_session_cache(app, session_id)

        # Send "Context cleared" marker to the Telegram thread (#200)
        notifier = app.state.notifier if hasattr(app.state, 'notifier') else None
        telegram_bot = getattr(notifier, 'telegram', None) if notifier else None
        if telegram_bot and session.telegram_chat_id and session.telegram_thread_id:
            cleared_msg = f"Context cleared [{session_id}] — ready for new task"
            chat_id = session.telegram_chat_id
            thread_id = session.telegram_thread_id
            # Try forum-topic delivery; fall back to reply-thread if it fails.
            await telegram_bot.send_with_fallback(
                chat_id=chat_id,
                message=cleared_msg,
                thread_id=thread_id,
                session_id=session_id,
            )

        # Cancel periodic remind and parent wake (context reset means task is over) (#188, #225-C)
        queue_mgr = app.state.session_manager.message_queue_manager
        if queue_mgr:
            queue_mgr.cancel_remind(session_id)
            queue_mgr.cancel_parent_wake(session_id)

        # Clear stale agent status from previous task (#283)
        session.agent_status_text = None
        session.agent_status_at = None
        session.agent_task_completed_at = None
        app.state.session_manager._save_state()

        return {"status": "cleared", "session_id": session_id}

    @app.post("/sessions/{session_id}/invalidate-cache")
    async def invalidate_session_cache(
        session_id: str,
        arm_skip: bool = Query(default=True),
    ):
        """Invalidate server-side caches for a session.

        Called by the CLI after tmux-level clear operations so that stale
        cached output and notification state from a previous task don't
        leak into the next task's stop-hook notifications (#167).
        """
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        _invalidate_session_cache(app, session_id, arm_skip=arm_skip)

        return {"status": "invalidated", "session_id": session_id}

    @app.delete("/sessions/{session_id}")
    async def kill_session(session_id: str):
        """Kill a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        # Cancel periodic remind and parent wake before killing (#188, #225-C)
        queue_mgr = app.state.session_manager.message_queue_manager
        if queue_mgr:
            queue_mgr.cancel_remind(session_id)
            queue_mgr.cancel_parent_wake(session_id)

        # Kill tmux session
        success = app.state.session_manager.kill_session(session_id)

        if not success:
            raise HTTPException(status_code=500, detail="Failed to kill session")

        # Perform full cleanup (Telegram, monitoring, state)
        if app.state.output_monitor:
            await app.state.output_monitor.cleanup_session(session, preserve_record=True)

        return {"status": "killed", "session_id": session_id}

    @app.post("/sessions/{session_id}/restore", response_model=SessionResponse)
    async def restore_session(session_id: str):
        """Restore a stopped session in place."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        success, restored_session, error = await app.state.session_manager.restore_session(session_id)
        if not success or not restored_session:
            detail = error or "Failed to restore session"
            status_code = 409 if detail == "Session is not stopped" else 400
            raise HTTPException(status_code=status_code, detail=detail)

        if app.state.output_monitor and getattr(restored_session, "provider", "claude") != "codex-app":
            await app.state.output_monitor.start_monitoring(restored_session)

        return _session_to_response(restored_session)

    @app.post("/sessions/{session_id}/open")
    async def open_terminal(session_id: str):
        """Open a session in Terminal.app."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        success = app.state.session_manager.open_terminal(session_id)

        if not success:
            raise HTTPException(status_code=500, detail="Failed to open Terminal")

        return {"status": "opened", "session_id": session_id}

    @app.get("/sessions/{session_id}/output")
    async def capture_output(session_id: str, lines: int = 50):
        """Capture recent output from a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        output = app.state.session_manager.capture_output(session_id, lines)

        return {"session_id": session_id, "output": output}

    @app.get("/sessions/{session_id}/tool-calls")
    async def get_tool_calls(
        session_id: str,
        limit: int = Query(default=10, ge=1, le=100),
    ):
        """Return recent PreToolUse tool calls for a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        provider = getattr(session, "provider", "claude")
        if provider == "codex-fork":
            observability = getattr(app.state.session_manager, "codex_observability_logger", None)
            if observability is None:
                return {"session_id": session_id, "tool_calls": []}

            tool_events = observability.list_recent_tool_events(session_id, limit=limit * 4)
            normalized_rows: list[dict[str, Any]] = []
            for row in reversed(tool_events):
                raw_payload = row.get("raw_payload_json")
                tool_name = None
                if isinstance(raw_payload, str) and raw_payload:
                    try:
                        parsed = json.loads(raw_payload)
                    except json.JSONDecodeError:
                        parsed = {}
                    tool_name = parsed.get("tool_name") if isinstance(parsed, dict) else None
                if not isinstance(tool_name, str) or not tool_name:
                    continue
                normalized_rows.append(
                    {
                        "timestamp": row.get("created_at"),
                        "tool_name": tool_name,
                        "hook_type": "CodexForkToolCall",
                    }
                )
                if len(normalized_rows) >= limit:
                    break
            normalized_rows.reverse()
            return {"session_id": session_id, "tool_calls": normalized_rows}

        tool_logger = getattr(app.state, "tool_logger", None)
        db_path = None
        if tool_logger is not None:
            db_path_value = getattr(tool_logger, "db_path", None)
            if db_path_value:
                db_path = Path(db_path_value).expanduser()
        if not db_path:
            configured = (app.state.config or {}).get("tool_logging", {}).get(
                "db_path",
                "~/.local/share/claude-sessions/tool_usage.db",
            )
            db_path = Path(configured).expanduser()

        if not db_path.exists():
            return {"session_id": session_id, "tool_calls": []}

        try:
            import sqlite3

            conn = sqlite3.connect(str(db_path))
            cursor = conn.cursor()
            cursor.execute(
                """
                SELECT timestamp, tool_name, hook_type
                FROM tool_usage
                WHERE session_id = ? AND hook_type = 'PreToolUse'
                ORDER BY id DESC
                LIMIT ?
                """,
                (session_id, limit),
            )
            rows = cursor.fetchall()
            conn.close()
        except Exception as exc:
            logger.warning("Failed to read tool-calls for %s: %s", session_id, exc)
            return {"session_id": session_id, "tool_calls": []}

        return {
            "session_id": session_id,
            "tool_calls": [
                {
                    "timestamp": ts,
                    "tool_name": tool,
                    "hook_type": hook_type,
                }
                for ts, tool, hook_type in rows
            ],
        }

    @app.get("/sessions/{session_id}/last-message")
    async def get_last_message(session_id: str):
        """Get the last Claude message from hooks (structured output)."""
        output = app.state.last_claude_output.get(session_id)
        if not output:
            # Try "latest" as fallback
            output = app.state.last_claude_output.get("latest")
        return {"session_id": session_id, "message": output}

    @app.get("/sessions/{session_id}/summary")
    async def get_summary(session_id: str, lines: int = 100):
        """
        Generate AI-powered summary of session activity.

        Args:
            session_id: Session to summarize
            lines: Number of lines of tmux output to analyze (default 100)

        Returns:
            JSON with summary text
        """
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        try:
            # Get tmux output
            tmux_output = app.state.session_manager.capture_output(session_id, lines)

            if not tmux_output:
                raise HTTPException(status_code=404, detail="No output available for session")

            # Strip ANSI codes
            from .notifier import strip_ansi
            clean_output = strip_ansi(tmux_output)

            # Prepare prompt for Claude Haiku
            prompt = f"""You are analyzing terminal output from a Claude Code session. Based on the output below, write a 2-3 line summary of what the session is currently working on. Focus on:
- What task/problem they're solving
- Current status (working, waiting, error, etc)
- Key details (files, commands, progress)

Output from session:
{clean_output}

Provide ONLY the summary, no preamble or questions."""

            logger.info(f"Generating summary for session {session_id}, input length: {len(prompt)} chars")

            # Get timeout from config
            config = app.state.config or {}
            timeouts = config.get("timeouts", {})
            server_timeouts = timeouts.get("server", {})
            summary_timeout = server_timeouts.get("summary_generation_timeout_seconds", 60)

            # Use asyncio.create_subprocess_exec for non-blocking execution
            import asyncio
            proc = await asyncio.create_subprocess_exec(
                '/opt/homebrew/bin/claude', '--model', 'haiku', '--print',
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE
            )

            try:
                stdout, stderr = await asyncio.wait_for(
                    proc.communicate(input=prompt.encode('utf-8')),
                    timeout=summary_timeout
                )
                result_stdout = stdout.decode('utf-8')
                result_stderr = stderr.decode('utf-8')
                returncode = proc.returncode
            except asyncio.TimeoutError:
                proc.kill()
                await proc.wait()
                raise HTTPException(status_code=504, detail=f"Summary generation timed out ({summary_timeout}s)")

            logger.info(f"Summary generated, return code: {returncode}, output length: {len(result_stdout)}")

            if returncode != 0:
                logger.error(f"Claude command failed: {result_stderr}")
                raise HTTPException(status_code=500, detail=f"Error generating summary: {result_stderr[:200]}")

            summary = result_stdout.strip()

            if not summary:
                raise HTTPException(status_code=500, detail="Summary was empty")

            return {"session_id": session_id, "summary": summary}

        except HTTPException:
            raise
        except Exception as e:
            logger.error(f"Error generating summary for session {session_id}: {e}", exc_info=True)
            raise HTTPException(status_code=500, detail=str(e))

    @app.post("/sessions/{session_id}/subagents", response_model=SubagentResponse)
    async def register_subagent_start(session_id: str, request: SubagentStartRequest):
        """Register a new subagent spawned by this session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        # Create new subagent
        from datetime import datetime
        subagent = Subagent(
            agent_id=request.agent_id,
            agent_type=request.agent_type,
            parent_session_id=session_id,
            transcript_path=request.transcript_path,
            started_at=datetime.now(),
            status=SubagentStatus.RUNNING,
        )

        # Add to session
        session.subagents.append(subagent)
        app.state.session_manager._save_state()

        logger.info(f"Registered subagent {request.agent_id} ({request.agent_type}) for session {session_id}")

        return SubagentResponse(
            agent_id=subagent.agent_id,
            agent_type=subagent.agent_type,
            parent_session_id=subagent.parent_session_id,
            started_at=subagent.started_at.isoformat(),
            stopped_at=None,
            status=subagent.status.value,
            summary=None,
        )

    @app.post("/sessions/{session_id}/subagents/{agent_id}/stop")
    async def register_subagent_stop(session_id: str, agent_id: str, request: SubagentStopRequest):
        """Register subagent completion."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        # Find the subagent
        subagent = None
        for sa in session.subagents:
            if sa.agent_id == agent_id:
                subagent = sa
                break

        if not subagent:
            raise HTTPException(status_code=404, detail=f"Subagent {agent_id} not found")

        # Update subagent
        from datetime import datetime
        subagent.stopped_at = datetime.now()
        subagent.status = SubagentStatus.COMPLETED
        if request.summary:
            subagent.summary = request.summary

        app.state.session_manager._save_state()

        logger.info(f"Stopped subagent {agent_id} for session {session_id}")

        return {
            "session_id": session_id,
            "agent_id": agent_id,
            "status": "stopped",
            "summary": subagent.summary,
        }

    @app.get("/sessions/{session_id}/subagents")
    async def list_subagents(session_id: str):
        """List all subagents for a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        return {
            "session_id": session_id,
            "subagents": [
                SubagentResponse(
                    agent_id=sa.agent_id,
                    agent_type=sa.agent_type,
                    parent_session_id=sa.parent_session_id,
                    started_at=sa.started_at.isoformat(),
                    stopped_at=sa.stopped_at.isoformat() if sa.stopped_at else None,
                    status=sa.status.value,
                    summary=sa.summary,
                )
                for sa in session.subagents
            ],
        }

    @app.post("/notify")
    async def send_notification(request: NotifyRequest):
        """
        Send a notification (for use by Claude Code hooks).

        This endpoint allows Claude to request notifications to be sent
        to the user via Telegram or Email.
        """
        if not app.state.notifier:
            raise HTTPException(status_code=503, detail="Notifier not configured")

        channel = None
        if request.channel:
            try:
                channel = NotificationChannel(request.channel)
            except ValueError:
                raise HTTPException(status_code=400, detail=f"Invalid channel: {request.channel}")

        # Use email handler directly for explicit email requests
        if channel == NotificationChannel.EMAIL:
            success = await app.state.notifier.request_email_notification(
                session_id="api",
                message=request.message,
                urgent=request.urgent,
            )
        else:
            # For Telegram, we need a chat ID - this should come from an active session
            # For now, just log it
            logger.info(f"Notification requested: {request.message}")
            success = True

        return {"status": "sent" if success else "failed"}

    @app.post("/hooks/claude")
    async def claude_hook(payload: dict = Body(...)):
        """
        Webhook endpoint for Claude Code hooks.

        Receives structured data from Claude Code Stop/Notification hooks.
        """
        import json
        from pathlib import Path

        hook_event = payload.get("hook_event_name", "unknown")
        logger.info(f"Hook received: {hook_event}")
        logger.debug(f"Hook payload keys: {list(payload.keys())}")

        transcript_path = payload.get("transcript_path")
        claude_session_id = payload.get("session_id")
        # This will be set by the environment variable we pass when launching Claude
        session_manager_id = payload.get("session_manager_id") or payload.get("CLAUDE_SESSION_MANAGER_ID")

        # Read transcript metadata in a thread pool to avoid blocking the event loop.
        last_message = None
        native_title = None
        native_title_mtime_ns = None
        if transcript_path:
            import asyncio

            def read_transcript():
                """
                Read transcript file synchronously (runs in thread pool).

                Returns:
                    Tuple of (
                        success: bool,
                        message: str | None,
                        native_title: str | None,
                        transcript_mtime_ns: int | None,
                    )
                """
                try:
                    transcript_file = Path(transcript_path)
                    if not transcript_file.exists():
                        logger.warning(f"Transcript file does not exist: {transcript_path}")
                        return (False, None, None, None)
                    transcript_stat = transcript_file.stat()
                    # JSONL file - read last lines and find last assistant message
                    lines = transcript_file.read_text().strip().split('\n')
                    latest_native_title = None
                    latest_assistant_message = None
                    for line in reversed(lines):
                        try:
                            entry = json.loads(line)
                            if latest_native_title is None:
                                if entry.get("type") == "custom-title":
                                    candidate = str(entry.get("customTitle") or "").strip()
                                    if candidate:
                                        latest_native_title = candidate
                                elif entry.get("type") == "agent-name":
                                    candidate = str(entry.get("agentName") or "").strip()
                                    if candidate:
                                        latest_native_title = candidate
                            if latest_assistant_message is None and entry.get("type") == "assistant":
                                # Extract text from message content
                                message = entry.get("message", {})
                                content = message.get("content", [])
                                texts = []
                                for item in content:
                                    if isinstance(item, dict) and item.get("type") == "text":
                                        texts.append(item.get("text", ""))
                                full_text = "\n".join(texts).strip()
                                if full_text:
                                    latest_assistant_message = full_text
                                else:
                                    # Newest assistant message exists but has no
                                    # visible text (whitespace-only / not flushed).
                                    # Stop here — do NOT fall back to older entries
                                    # which would surface a stale message.
                                    latest_assistant_message = None
                            if latest_native_title is not None and (
                                latest_assistant_message is not None or entry.get("type") == "assistant"
                            ):
                                break
                        except json.JSONDecodeError as e:
                            logger.debug(f"Skipping malformed JSON line in transcript: {e}")
                            continue
                    return (True, latest_assistant_message, latest_native_title, transcript_stat.st_mtime_ns)
                except Exception as e:
                    logger.error(f"CRITICAL: Error reading transcript {transcript_path}: {e}")
                    logger.error(f"Claude output will not be available for this hook event")
                    return (False, None, None, None)

            try:
                success, last_message, native_title, native_title_mtime_ns = await asyncio.to_thread(read_transcript)
                if not success:
                    logger.warning(f"Failed to read transcript for hook event: {hook_event}")
            except Exception as e:
                logger.error(f"CRITICAL: Error reading transcript in thread: {e}")
                last_message = None
                native_title = None
                native_title_mtime_ns = None

            # Fix #230: Bounded retry for empty transcript reads on Stop hooks.
            # The Stop hook can fire before Claude flushes the current response to
            # the transcript JSONL, returning None. Retry once after 500ms before
            # deferring to the idle_prompt hook.
            # Note: this retry is inside the if-transcript_path block and executes
            # before Stop-hook side effects (queue idle, lock cleanup). The 500ms
            # delay applies only in the empty-transcript edge case.
            if hook_event == "Stop" and not last_message:
                logger.info(
                    f"Empty transcript for {session_manager_id or 'unknown'}, "
                    f"retrying after {EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS}s"
                )
                await asyncio.sleep(EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS)
                try:
                    success, last_message, native_title, native_title_mtime_ns = await asyncio.to_thread(read_transcript)
                    if not success:
                        logger.warning(f"Empty transcript retry: failed for {session_manager_id or 'unknown'}")
                        last_message = None
                        native_title = None
                        native_title_mtime_ns = None
                except Exception as e:
                    logger.error(f"Empty transcript retry: error for {session_manager_id or 'unknown'}: {e}")
                    last_message = None
                    native_title = None
                    native_title_mtime_ns = None

            # Fix #184: Bounded retry for stale transcript reads on Stop hooks.
            # The Stop hook can fire before Claude writes the current response to
            # the transcript JSONL, causing read_transcript() to return the
            # previous response. Detect staleness by comparing against the stored
            # output and retry once after 300ms.
            if hook_event == "Stop" and session_manager_id and last_message:
                stored_output = app.state.last_claude_output.get(session_manager_id)
                if stored_output and last_message == stored_output:
                    logger.info(f"Transcript appears stale for {session_manager_id}, retrying after {TRANSCRIPT_RETRY_DELAY_SECONDS}s")
                    await asyncio.sleep(TRANSCRIPT_RETRY_DELAY_SECONDS)
                    try:
                        success, last_message, native_title, native_title_mtime_ns = await asyncio.to_thread(read_transcript)
                        if not success:
                            logger.warning(f"Retry: Failed to read transcript for {session_manager_id}")
                            last_message = None
                            native_title = None
                            native_title_mtime_ns = None
                    except Exception as e:
                        logger.error(f"Retry: Error reading transcript for {session_manager_id}: {e}")
                        last_message = None
                        native_title = None
                        native_title_mtime_ns = None

        if session_manager_id and app.state.session_manager:
            target_session = app.state.session_manager.get_session(session_manager_id)
            if target_session:
                state_changed = False
                title_changed = False

                if transcript_path and target_session.transcript_path != transcript_path:
                    target_session.transcript_path = transcript_path
                    app.state.session_manager._sync_session_resume_id(target_session)
                    state_changed = True

                if target_session.provider == "claude" and native_title_mtime_ns is not None:
                    previous_native_title = target_session.native_title
                    if target_session.native_title_source_mtime_ns != native_title_mtime_ns:
                        target_session.native_title = native_title
                        target_session.native_title_source_mtime_ns = native_title_mtime_ns
                        if previous_native_title != native_title:
                            target_session.native_title_updated_at_ns = native_title_mtime_ns
                            title_changed = True
                            state_changed = True

                if state_changed:
                    app.state.session_manager._save_state()

                if title_changed:
                    await _sync_session_display_identity(target_session)

        # Store last message
        if last_message:
            app.state.last_claude_output["latest"] = last_message
            # If we have the session manager ID, store under that immediately
            if session_manager_id:
                app.state.last_claude_output[session_manager_id] = last_message
                logger.info(f"Stored Claude output for session {session_manager_id}: {last_message[:100]}...")
            else:
                app.state.last_claude_output[claude_session_id or "default"] = last_message
                logger.warning(f"No session_manager_id in hook - stored as {claude_session_id or 'default'}")

        # Handle Stop hook - Claude finished responding
        # Mark session as idle for message queue delivery
        if hook_event == "Stop" and session_manager_id:
            queue_mgr = app.state.session_manager.message_queue_manager if app.state.session_manager else None
            if queue_mgr:
                queue_mgr.mark_session_idle(session_manager_id, last_output=last_message, from_stop_hook=True)
                # Skip _restore_user_input if a handoff was triggered.
                # mark_session_idle sets is_idle=False synchronously when a handoff is pending,
                # so is_idle==False here reliably signals handoff in progress (#196).
                import asyncio
                state = queue_mgr.delivery_states.get(session_manager_id)
                handoff_in_progress = state and not state.is_idle
                if not handoff_in_progress:
                    asyncio.create_task(queue_mgr._restore_user_input_after_response(session_manager_id))

                # Keep session.status in sync with delivery_state.is_idle.
                # Only set IDLE if message_queue also considers session idle (sm#232):
                # when skip_count absorbed the Stop hook, state.is_idle was NOT set True,
                # so session.status correctly remains RUNNING.
                if app.state.session_manager:
                    target_session = app.state.session_manager.get_session(session_manager_id)
                    if target_session and target_session.status != SessionStatus.STOPPED:
                        if not state or state.is_idle:
                            app.state.session_manager.update_session_status(
                                session_manager_id, SessionStatus.IDLE
                            )
            else:
                # No queue_mgr — no delivery state; always update to IDLE
                if app.state.session_manager:
                    target_session = app.state.session_manager.get_session(session_manager_id)
                    if target_session and target_session.status != SessionStatus.STOPPED:
                        app.state.session_manager.update_session_status(
                            session_manager_id, SessionStatus.IDLE
                        )

            # Auto-release locks and check for cleanup
            if app.state.session_manager:
                session = app.state.session_manager.get_session(session_manager_id)
                if session:
                    # Import lock manager functions
                    from .lock_manager import LockManager, is_worktree, get_worktree_status_hash

                    # Release all locks (silent)
                    for repo_root in session.touched_repos:
                        lock_mgr = LockManager(working_dir=repo_root)
                        lock_mgr.release_lock(repo_root, session_manager_id)
                        logger.info(f"Released lock on {repo_root} for session {session_manager_id}")

                    worktree_cleanup_config = (app.state.config or {}).get("worktree_cleanup", {})
                    notify_dirty = bool(worktree_cleanup_config.get("notify_dirty", True))

                    # Check for worktrees with uncommitted changes
                    cleanup_needed = []
                    prompt_state_changed = False
                    for repo_root in session.touched_repos:
                        if not is_worktree(repo_root):
                            continue

                        status_hash = get_worktree_status_hash(repo_root)
                        if status_hash is None:
                            # Clean worktree: clear any prior prompt state
                            if repo_root in session.cleanup_prompted:
                                del session.cleanup_prompted[repo_root]
                                prompt_state_changed = True
                            continue

                        if session.cleanup_prompted.get(repo_root) == status_hash:
                            continue

                        cleanup_needed.append((repo_root, status_hash))

                    # Inject cleanup prompt if needed (configurable noise control).
                    if cleanup_needed and notify_dirty:
                        dirty_worktree_paths = sorted(repo_root for repo_root, _ in cleanup_needed)
                        paths_str = "\n".join(f"- {path}" for path in dirty_worktree_paths)
                        cleanup_prompt = (
                            "[sm info] Uncommitted changes in worktree(s):\n"
                            f"{paths_str}\n"
                            "If work has completed, consider checking in."
                        )

                        # Send cleanup prompt to session
                        await app.state.session_manager.send_input(
                            session_manager_id,
                            cleanup_prompt,
                            delivery_mode="important"
                        )
                        for repo_root, status_hash in cleanup_needed:
                            session.cleanup_prompted[repo_root] = status_hash
                        app.state.session_manager._save_state()
                        logger.info(f"Sent cleanup prompt for {len(cleanup_needed)} worktree(s)")
                    elif cleanup_needed and not notify_dirty:
                        # Track hashes even when muted so we don't repeatedly evaluate/signal.
                        for repo_root, status_hash in cleanup_needed:
                            session.cleanup_prompted[repo_root] = status_hash
                        app.state.session_manager._save_state()
                    elif prompt_state_changed:
                        app.state.session_manager._save_state()

        if hook_event == "Stop" and not last_message and session_manager_id:
            # Transcript was empty/whitespace-only at Stop time (race condition:
            # file not flushed yet). Track this so we can send a deferred
            # notification when the idle_prompt Notification hook arrives with
            # the real content ~60s later.
            app.state.pending_stop_notifications.add(session_manager_id)
            logger.info(f"Stop hook for {session_manager_id} had empty transcript, deferring notification")

        if hook_event == "Stop" and last_message:
            # Send immediate notification to Telegram
            app.state.pending_stop_notifications.discard(session_manager_id)
            if app.state.notifier and app.state.session_manager:
                # Try to find the session this hook belongs to
                target_session = None

                # First try: match by session_manager_id (set when session manager launches Claude)
                # This is the most reliable - set via CLAUDE_SESSION_MANAGER_ID env var
                if session_manager_id:
                    target_session = app.state.session_manager.get_session(session_manager_id)
                    if target_session:
                        logger.info(f"Matched hook to session {session_manager_id} via environment variable")

                # If we have a reliable match, don't try other methods
                if target_session:
                    pass  # Use this session
                else:
                    # Second try: match by transcript path - but only if it hasn't been set yet
                    # (to avoid matching wrong session if transcript paths get reused)
                    if transcript_path:
                        sessions = app.state.session_manager.list_sessions()
                        for session in sessions:
                            # Only match if session has this transcript path already recorded
                            if session.transcript_path == transcript_path:
                                target_session = session
                                logger.info(f"Matched hook to session {session.id} via existing transcript path")
                                break

                    # Third try: match by claude_session_id (if provided)
                    if not target_session and claude_session_id:
                        target_session = app.state.session_manager.get_session(claude_session_id)
                        if target_session:
                            logger.info(f"Matched hook to session {claude_session_id} via claude_session_id")

                # If we found a matching session, send the notification
                if target_session and target_session.telegram_chat_id:
                    # Store last output under our session ID for /status
                    app.state.last_claude_output[target_session.id] = last_message

                    # Update session's last activity timestamp
                    from datetime import datetime
                    target_session.last_activity = datetime.now()
                    app.state.session_manager._save_state()

                    from .models import NotificationEvent
                    event = NotificationEvent(
                        session_id=target_session.id,
                        event_type="response",
                        message="Claude responded",
                        context=last_message,
                        urgent=False,
                    )
                    await app.state.notifier.notify(event, target_session)
                    # Mark response sent (starts idle cooldown)
                    if app.state.output_monitor:
                        app.state.output_monitor.mark_response_sent(target_session.id)

                    # If session has a review_config, emit review_complete notification
                    if target_session.review_config:
                        try:
                            from .review_parser import parse_tui_output, parse_github_review
                            review_config = target_session.review_config

                            review_result = None
                            if review_config.mode == "pr" and review_config.pr_repo and review_config.pr_number:
                                # PR mode: fetch from GitHub (async)
                                import asyncio
                                from .github_reviews import fetch_latest_codex_review
                                codex_review = await asyncio.to_thread(
                                    fetch_latest_codex_review,
                                    review_config.pr_repo,
                                    review_config.pr_number,
                                )
                                if codex_review:
                                    review_result = await asyncio.to_thread(
                                        parse_github_review,
                                        review_config.pr_repo,
                                        review_config.pr_number,
                                        codex_review,
                                    )
                            else:
                                # TUI mode: parse from last message
                                review_result = parse_tui_output(last_message)

                            if review_result and review_result.findings:
                                review_event = NotificationEvent(
                                    session_id=target_session.id,
                                    event_type="review_complete",
                                    message="Review complete",
                                    context="",
                                    urgent=False,
                                )
                                review_event.review_result = review_result
                                await app.state.notifier.notify(review_event, target_session)
                        except Exception as e:
                            logger.warning(f"Failed to emit review_complete notification: {e}")
                else:
                    # Couldn't find matching session
                    logger.warning(
                        f"Stop hook: Could not find matching session for "
                        f"claude_session_id={claude_session_id}, "
                        f"transcript_path={transcript_path}"
                    )

        # Handle Notification hook (permission prompts, idle, etc.)
        elif hook_event == "Notification":
            notification_type = payload.get("notification_type")
            message = payload.get("message", "")
            logger.info(f"Claude notification: {notification_type} - {message}")

            # idle_prompt: normally skip, but send deferred response if the
            # preceding Stop hook had an empty transcript (race condition).
            if notification_type == "idle_prompt":
                sid = session_manager_id
                if sid and sid in app.state.pending_stop_notifications and last_message:
                    app.state.pending_stop_notifications.discard(sid)
                    logger.info(f"Sending deferred response notification for {sid} (idle_prompt had content)")
                    if app.state.notifier and app.state.session_manager:
                        target_session = app.state.session_manager.get_session(sid)
                        if target_session and target_session.telegram_chat_id:
                            # Persist transcript path (same as immediate Stop path)
                            if transcript_path and not target_session.transcript_path:
                                target_session.transcript_path = transcript_path
                                app.state.session_manager._sync_session_resume_id(target_session)
                            app.state.last_claude_output[target_session.id] = last_message
                            from datetime import datetime
                            target_session.last_activity = datetime.now()
                            app.state.session_manager._save_state()
                            from .models import NotificationEvent
                            event = NotificationEvent(
                                session_id=target_session.id,
                                event_type="response",
                                message="Claude responded",
                                context=last_message,
                                urgent=False,
                            )
                            await app.state.notifier.notify(event, target_session)
                            if app.state.output_monitor:
                                app.state.output_monitor.mark_response_sent(target_session.id)
                else:
                    # Only clear pending state if the session is NOT awaiting
                    # a deferred notification (i.e. it was never pending, or
                    # was pending but last_message is empty — keep it pending
                    # so a later hook can still deliver the content).
                    if sid and sid not in app.state.pending_stop_notifications:
                        logger.debug(f"Skipping idle_prompt notification for {sid} (filtered out)")
                    elif sid and sid in app.state.pending_stop_notifications:
                        logger.info(f"idle_prompt for {sid} had empty transcript, keeping deferred state")
                    else:
                        logger.debug(f"Skipping idle_prompt notification (filtered out)")
                return {"status": "received", "hook_event": hook_event}

            # Send notification to Telegram for permission prompts and errors
            if app.state.notifier and app.state.session_manager:
                # Try to find the session
                target_session = None

                if session_manager_id:
                    target_session = app.state.session_manager.get_session(session_manager_id)
                    if target_session:
                        logger.info(f"Found session {session_manager_id} for notification")

                if not target_session and claude_session_id:
                    target_session = app.state.session_manager.get_session(claude_session_id)
                    if target_session:
                        logger.info(f"Found session {claude_session_id} for notification")

                # Send notification if we found a session
                if target_session and target_session.telegram_chat_id:
                    from .models import NotificationEvent
                    event = NotificationEvent(
                        session_id=target_session.id,
                        event_type=notification_type,  # "permission_prompt", "idle_prompt", etc.
                        message=message,
                        context=last_message or "",
                        urgent=notification_type in ["permission_prompt", "error"],
                    )
                    await app.state.notifier.notify(event, target_session)
                else:
                    logger.warning(
                        f"Notification hook: Could not find matching session for "
                        f"session_manager_id={session_manager_id}, "
                        f"claude_session_id={claude_session_id}"
                    )

        return {"status": "received", "hook_event": hook_event}

    @app.post("/sessions/spawn")
    async def spawn_child_session(request: SpawnChildRequest):
        """Spawn a child agent session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        # Get parent session
        parent_session = app.state.session_manager.get_session(request.parent_session_id)
        if not parent_session:
            return {"error": "Parent session not found"}

        # Spawn child session
        provider = _normalize_provider(request.provider) if request.provider else None
        selected_provider = provider or getattr(parent_session, "provider", "claude")
        creation_rejection = _codex_app_create_rejection(selected_provider)
        if creation_rejection:
            raise HTTPException(status_code=400, detail=creation_rejection)
        child_session = await app.state.session_manager.spawn_child_session(
            parent_session_id=request.parent_session_id,
            prompt=request.prompt,
            name=request.name,
            wait=request.wait,
            model=request.model,
            working_dir=request.working_dir or parent_session.working_dir,
            provider=provider,
            defer_telegram_topic=True,
        )

        if not child_session:
            return {"error": "Failed to spawn child session"}

        spawn_warnings = _register_spawn_monitoring(
            child_session,
            parent_session,
            track_seconds=request.track_seconds,
        )

        # Start monitoring the child session (tmux providers only)
        if app.state.output_monitor and getattr(child_session, "provider", "claude") != "codex-app":
            await app.state.output_monitor.start_monitoring(child_session)

        # Note: --wait monitoring is already registered by session_manager.spawn_child_session()

        response = {
            "session_id": child_session.id,
            "name": child_session.name,
            "friendly_name": _effective_session_name(child_session),
            "working_dir": child_session.working_dir,
            "parent_session_id": child_session.parent_session_id,
            "tmux_session": child_session.tmux_session,
            "provider": getattr(child_session, "provider", "claude"),
            "created_at": child_session.created_at.isoformat(),
        }
        if spawn_warnings:
            response["warnings"] = spawn_warnings
        return response

    @app.get("/sessions/{session_id}/review-results")
    async def get_review_results(session_id: str):
        """Get parsed review results for a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        if not session.review_config:
            raise HTTPException(status_code=404, detail="No review configured for this session")

        from .review_parser import parse_github_review, parse_tui_output
        from .github_reviews import fetch_latest_codex_review

        review_config = session.review_config

        if review_config.mode == "pr" and review_config.pr_repo and review_config.pr_number:
            # GitHub PR mode: fetch review from GitHub API
            import asyncio
            repo = review_config.pr_repo
            pr_number = review_config.pr_number

            try:
                codex_review = await asyncio.to_thread(
                    fetch_latest_codex_review, repo, pr_number
                )
                if not codex_review:
                    raise HTTPException(status_code=404, detail="No Codex review found on PR")

                review_result = await asyncio.to_thread(
                    parse_github_review, repo, pr_number, codex_review
                )
                return review_result.to_dict()

            except HTTPException:
                raise
            except Exception as e:
                raise HTTPException(status_code=500, detail=f"Failed to fetch review: {e}")
        else:
            # TUI mode: capture tmux pane output
            output = app.state.session_manager.capture_output(session_id, lines=500)
            if not output:
                raise HTTPException(status_code=404, detail="No output available from session")

            review_result = parse_tui_output(output)
            return review_result.to_dict()

    @app.post("/sessions/{session_id}/review")
    async def start_review(session_id: str, request: StartReviewRequest):
        """Start a Codex review on an existing session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        result = await app.state.session_manager.start_review(
            session_id=session_id,
            mode=request.mode,
            base_branch=request.base_branch,
            commit_sha=request.commit_sha,
            custom_prompt=request.custom_prompt,
            steer_text=request.steer,
            wait=request.wait,
            watcher_session_id=request.watcher_session_id,
        )

        if result.get("error"):
            return result  # Return 200 with error payload (matches spawn flow pattern)

        return result

    @app.post("/sessions/review")
    async def spawn_review(request: SpawnReviewRequest):
        """Spawn a new Codex session and start a review."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        parent = app.state.session_manager.get_session(request.parent_session_id)
        if not parent:
            raise HTTPException(status_code=404, detail="Parent session not found")

        session = await app.state.session_manager.spawn_review_session(
            parent_session_id=request.parent_session_id,
            mode=request.mode,
            base_branch=request.base_branch,
            commit_sha=request.commit_sha,
            custom_prompt=request.custom_prompt,
            steer_text=request.steer,
            name=request.name,
            wait=request.wait,
            model=request.model,
            working_dir=request.working_dir,
        )

        if not session:
            return {"error": "Failed to spawn review session"}

        # Start monitoring
        if app.state.output_monitor:
            await app.state.output_monitor.start_monitoring(session)

        return {
            "session_id": session.id,
            "name": session.name,
            "friendly_name": _effective_session_name(session),
            "review_mode": request.mode,
            "base_branch": request.base_branch,
            "status": "started",
        }

    @app.post("/reviews/pr")
    async def start_pr_review(request: PRReviewRequest):
        """Trigger @codex review on a GitHub PR."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        result = await app.state.session_manager.start_pr_review(
            pr_number=request.pr_number,
            repo=request.repo,
            steer=request.steer,
            wait=request.wait,
            caller_session_id=request.caller_session_id,
        )

        if result.get("error"):
            return result  # Return 200 with error payload (matches spawn flow pattern)

        return result

    @app.get("/sessions/{parent_session_id}/children")
    async def list_children_sessions(
        parent_session_id: str,
        recursive: bool = False,
        status: Optional[str] = None,
    ):
        """List child sessions of a parent."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        # Get all sessions and filter by parent_session_id
        all_sessions = app.state.session_manager.list_sessions(include_stopped=True)
        children = [s for s in all_sessions if s.parent_session_id == parent_session_id]

        # Filter by status if specified
        if status and status != "all":
            if status == "running":
                children = [s for s in children if s.status == SessionStatus.RUNNING]
            elif status == "completed":
                from src.models import CompletionStatus
                children = [s for s in children if s.completion_status == CompletionStatus.COMPLETED]
            elif status == "error":
                from src.models import CompletionStatus
                children = [s for s in children if s.completion_status == CompletionStatus.ERROR]

        # Handle recursive
        if recursive:
            all_descendants = []
            for child in children:
                all_descendants.append(child)
                # Get grandchildren
                grandchildren = [s for s in all_sessions if s.parent_session_id == child.id]
                all_descendants.extend(grandchildren)
            children = all_descendants

        latest_action_getter = getattr(app.state.session_manager, "get_codex_latest_activity_action", None)
        codex_projection_enabled = _codex_rollout_enabled("enable_observability_projection")
        children_payload = []
        for s in children:
            provider = getattr(s, "provider", "claude")
            activity_projection = None
            if provider == "codex-app" and codex_projection_enabled and callable(latest_action_getter):
                activity_projection = latest_action_getter(s.id)
            children_payload.append(
                {
                    "id": s.id,
                    "name": s.name,
                    "friendly_name": _effective_session_name(s),
                    "status": s.status.value,
                    "activity_state": _get_activity_state(s),
                    "completion_status": s.completion_status.value if s.completion_status else None,
                    "completion_message": s.completion_message,
                    "last_activity": s.last_activity.isoformat(),
                    "spawned_at": s.spawned_at.isoformat() if s.spawned_at else None,
                    "completed_at": s.completed_at.isoformat() if s.completed_at else None,
                    # sm remind: self-reported status (#188)
                    "agent_status_text": s.agent_status_text,
                    "agent_status_at": s.agent_status_at.isoformat() if s.agent_status_at else None,
                    "provider": provider,
                    "activity_projection": activity_projection,
                }
            )

        return {
            "parent_session_id": parent_session_id,
            "children": children_payload,
        }

    @app.get("/admin/rollout-flags")
    async def get_rollout_flags():
        """Expose codex rollout feature gates for CLI and operator checks."""
        policy = _codex_provider_policy()
        return {
            "codex_rollout": {
                "enable_durable_events": _codex_rollout_enabled("enable_durable_events"),
                "enable_structured_requests": _codex_rollout_enabled("enable_structured_requests"),
                "enable_observability_projection": _codex_rollout_enabled("enable_observability_projection"),
                "enable_codex_tui": _codex_rollout_enabled("enable_codex_tui"),
                "provider_mapping_phase": policy.get("phase"),
                "codex_app_allow_create": policy.get("allow_create"),
                "codex_app_warning": policy.get("warning"),
                "codex_app_rejection_error": policy.get("rejection_error"),
            }
        }

    @app.get("/admin/codex-fork-runtime")
    async def get_codex_fork_runtime():
        """Expose codex-fork artifact pinning/runtime metadata for operators."""
        sm = app.state.session_manager
        if not sm:
            raise HTTPException(status_code=503, detail="Session manager not configured")
        getter = getattr(sm, "get_codex_fork_runtime_info", None)
        if not callable(getter):
            raise HTTPException(status_code=503, detail="codex-fork runtime metadata unavailable")
        payload = getter()
        if not isinstance(payload, dict):
            raise HTTPException(status_code=500, detail="invalid codex-fork runtime metadata")
        return {"codex_fork": payload}

    @app.get("/admin/codex-launch-gates")
    async def get_codex_launch_gates():
        """Expose codex launch/cutover gate status for operators."""
        sm = app.state.session_manager
        if not sm:
            raise HTTPException(status_code=503, detail="Session manager not configured")
        getter = getattr(sm, "get_codex_launch_gates", None)
        if not callable(getter):
            raise HTTPException(status_code=503, detail="codex launch gates unavailable")
        payload = getter()
        if not isinstance(payload, dict):
            raise HTTPException(status_code=500, detail="invalid codex launch gates payload")
        return {"codex_launch_gates": payload}

    @app.post("/sessions/{target_session_id}/kill")
    async def kill_session_with_check(target_session_id: str, request: KillSessionRequest):
        """Kill a session with parent-child ownership check."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        # Get target session
        target_session = app.state.session_manager.get_session(target_session_id)
        if not target_session:
            return {"error": f"Session {target_session_id} not found"}

        # Check ownership if requester provided
        if request.requester_session_id:
            # Requester must be the parent
            if target_session.parent_session_id != request.requester_session_id:
                return {"error": f"Cannot kill session {target_session_id} - not your child session"}

        # Cancel periodic remind and parent wake before killing (#188, #225-C)
        queue_mgr = app.state.session_manager.message_queue_manager
        if queue_mgr:
            queue_mgr.cancel_remind(target_session_id)
            queue_mgr.cancel_parent_wake(target_session_id)

        # Kill the session
        success = app.state.session_manager.kill_session(target_session_id)

        if not success:
            return {"error": "Failed to kill session"}

        # Perform full cleanup (Telegram, monitoring, state)
        if app.state.output_monitor:
            try:
                await app.state.output_monitor.cleanup_session(target_session, preserve_record=True)
            except Exception:
                logger.exception(f"cleanup_session failed for {target_session_id}")
                return {"error": "Failed to finalize session cleanup"}

        return {"status": "killed", "session_id": target_session_id}

    @app.post("/sessions/{session_id}/handoff")
    async def schedule_handoff(session_id: str, request: HandoffRequest):
        """Schedule a self-directed context rotation via handoff doc (#196)."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        # 1. Verify self-auth: requester must be the session itself
        if request.requester_session_id != session_id:
            return {"error": "sm handoff is self-directed only — requester must equal target session"}

        # 2. Verify session exists
        session = app.state.session_manager.get_session(session_id)
        if not session:
            return {"error": f"Session {session_id} not found"}

        # 3. Reject codex-app sessions (no tmux, different clear mechanism)
        if session.provider == "codex-app":
            return {"error": "sm handoff is not supported for codex-app sessions"}

        # 4. Store pending handoff on delivery state
        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            return {"error": "Message queue manager not available"}

        state = queue_mgr._get_or_create_state(session_id)
        state.pending_handoff_path = request.file_path
        logger.info(f"Handoff scheduled for {session_id}: {request.file_path}")
        return {"status": "scheduled"}

    @app.post("/sessions/{target_session_id}/adoption-proposals")
    async def create_adoption_proposal(target_session_id: str, request: CreateAdoptionProposalRequest):
        """Create an explicit adoption proposal for user approval in sm watch."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        creator = getattr(app.state.session_manager, "create_adoption_proposal", None)
        if not callable(creator):
            raise HTTPException(status_code=503, detail="Adoption proposals unavailable")

        try:
            proposal = creator(request.requester_session_id, target_session_id)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc

        return {
            "status": "pending",
            "proposal": _response_dict(_proposal_to_response(proposal)),
        }

    @app.post("/adoption-proposals/{proposal_id}/accept")
    async def accept_adoption_proposal(proposal_id: str):
        """Accept an adoption proposal from the operator watch UI."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        decider = getattr(app.state.session_manager, "decide_adoption_proposal", None)
        if not callable(decider):
            raise HTTPException(status_code=503, detail="Adoption proposals unavailable")

        try:
            proposal = decider(proposal_id, accepted=True)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc

        return {
            "status": "accepted",
            "proposal": _response_dict(_proposal_to_response(proposal)),
        }

    @app.post("/adoption-proposals/{proposal_id}/reject")
    async def reject_adoption_proposal(proposal_id: str):
        """Reject an adoption proposal from the operator watch UI."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        decider = getattr(app.state.session_manager, "decide_adoption_proposal", None)
        if not callable(decider):
            raise HTTPException(status_code=503, detail="Adoption proposals unavailable")

        try:
            proposal = decider(proposal_id, accepted=False)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc

        return {
            "status": "rejected",
            "proposal": _response_dict(_proposal_to_response(proposal)),
        }

    @app.post("/sessions/{session_id}/task-complete")
    async def task_complete(session_id: str, request: TaskCompleteRequest):
        """Mark a session's task as complete: cancel remind + parent-wake, notify EM (#269)."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        # 1. Verify self-auth: requester must be the session itself
        if request.requester_session_id != session_id:
            return {"error": "sm task-complete is self-directed only — requester must equal target session"}

        # 2. Verify session exists
        session = app.state.session_manager.get_session(session_id)
        if not session:
            return {"error": f"Session {session_id} not found"}

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            return {"error": "Message queue manager not available"}

        # 3. Resolve EM before cancelling parent-wake (cancel deactivates the DB row)
        def _get_em_for_session(qm, sm, sid: str):
            rows = qm._execute_query(
                "SELECT parent_session_id FROM parent_wake_registrations "
                "WHERE child_session_id = ? AND is_active = 1 LIMIT 1",
                (sid,)
            )
            if rows:
                return rows[0][0]
            s = sm.get_session(sid)
            return s.parent_session_id if s else None

        em_id = _get_em_for_session(queue_mgr, app.state.session_manager, session_id)

        # 4. Cancel remind and parent-wake
        queue_mgr.cancel_remind(session_id)
        queue_mgr.cancel_parent_wake(session_id)

        # Persist self-reported completion metadata for watch surfaces.
        session.agent_task_completed_at = datetime.now()
        app.state.session_manager._save_state()

        # 5. Notify EM if found
        em_notified = False
        if em_id:
            friendly = _effective_session_name(session)
            queue_mgr.queue_message(
                target_session_id=em_id,
                text=f"[sm task-complete] agent {session_id}({friendly}) completed its task. Clear context with: sm clear {session_id}",
                delivery_mode="important",
            )
            em_notified = True
            logger.info(f"task-complete: notified EM {em_id} about {session_id}")
        else:
            logger.warning(f"task-complete: no EM found for {session_id}, skipping notification")

        return {
            "status": "completed",
            "session_id": session_id,
            "em_notified": em_notified,
            "agent_task_completed_at": session.agent_task_completed_at.isoformat(),
        }

    @app.post("/sessions/{session_id}/turn-complete")
    async def turn_complete(session_id: str, request: TaskCompleteRequest):
        """Mark a session's current turn as complete and suppress periodic remind."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        if request.requester_session_id != session_id:
            return {"error": "sm turn-complete is self-directed only — requester must equal target session"}

        session = app.state.session_manager.get_session(session_id)
        if not session:
            return {"error": f"Session {session_id} not found"}

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            return {"error": "Message queue manager not available"}

        queue_mgr.cancel_remind(session_id)
        logger.info("turn-complete: cancelled periodic remind for %s", session_id)

        return {
            "status": "turn_completed",
            "session_id": session_id,
        }

    @app.get("/sessions/{session_id}/send-queue")
    async def get_send_queue(session_id: str):
        """Get pending messages for a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            return {"session_id": session_id, "is_idle": False, "pending_count": 0, "pending_messages": []}

        return queue_mgr.get_queue_status(session_id)

    @app.post("/scheduler/remind")
    async def schedule_reminder(
        session_id: str,
        message: str,
        delay_seconds: int = Query(..., gt=0),
        recurring_interval_seconds: Optional[int] = Query(default=None, gt=0),
    ):
        """Schedule a self-reminder for a session."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            raise HTTPException(status_code=503, detail="Message queue not configured")

        reminder_id = await queue_mgr.schedule_reminder(
            session_id,
            delay_seconds,
            message,
            recurring_interval_seconds=recurring_interval_seconds,
        )

        return {
            "status": "scheduled",
            "reminder_id": reminder_id,
            "session_id": session_id,
            "fires_in_seconds": delay_seconds,
            "mode": "recurring" if recurring_interval_seconds is not None else "one-shot",
            "recurring_interval_seconds": recurring_interval_seconds,
        }

    @app.delete("/scheduler/remind/{reminder_id}")
    async def cancel_scheduled_reminder(reminder_id: str):
        """Cancel a scheduled reminder by reminder ID."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            raise HTTPException(status_code=503, detail="Message queue not configured")

        reminder = queue_mgr.cancel_scheduled_reminder(reminder_id)
        if reminder is None:
            raise HTTPException(status_code=404, detail="Reminder not found")

        return {
            "status": "cancelled",
            "reminder_id": reminder_id,
            "session_id": reminder["target_session_id"],
            "recurring_interval_seconds": reminder["recurring_interval_seconds"],
            "fired": reminder["fired"],
        }

    @app.post("/sessions/{session_id}/remind")
    async def register_remind(session_id: str, request: PeriodicRemindRequest):
        """Register (or replace) a periodic remind for a session (#188)."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            raise HTTPException(status_code=503, detail="Message queue not configured")

        reg_id = queue_mgr.register_periodic_remind(
            target_session_id=session_id,
            soft_threshold=request.soft_threshold,
            hard_threshold=request.hard_threshold,
            cancel_on_reply_session_id=request.cancel_on_reply_session_id,
        )
        return {
            "status": "registered",
            "registration_id": reg_id,
            "session_id": session_id,
            "soft_threshold": request.soft_threshold,
            "hard_threshold": request.hard_threshold,
            "cancel_on_reply_session_id": request.cancel_on_reply_session_id,
        }

    @app.delete("/sessions/{session_id}/remind")
    async def cancel_remind(session_id: str):
        """Cancel periodic remind for a session (#188)."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        queue_mgr = app.state.session_manager.message_queue_manager
        if queue_mgr:
            queue_mgr.cancel_remind(session_id)

        return {"status": "cancelled", "session_id": session_id}

    @app.post("/job-watches", response_model=JobWatchResponse)
    async def create_job_watch(request: JobWatchCreateRequest):
        """Register a durable external job watch that wakes a target session (#377)."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        target_session = app.state.session_manager.get_session(request.target_session_id)
        if not target_session:
            raise HTTPException(status_code=404, detail="Target session not found")

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            raise HTTPException(status_code=503, detail="Message queue not configured")

        try:
            registration = queue_mgr.register_job_watch(
                target_session_id=request.target_session_id,
                label=request.label,
                pid=request.pid,
                file_path=request.file_path,
                progress_regex=request.progress_regex,
                done_regex=request.done_regex,
                error_regex=request.error_regex,
                exit_code_file=request.exit_code_file,
                interval_seconds=request.interval_seconds,
                tail_lines=request.tail_lines,
                tail_on_error=request.tail_on_error,
                notify_on_change=request.notify_on_change,
            )
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc

        return _job_watch_to_response(registration)

    @app.get("/job-watches")
    async def list_job_watches(
        target_session_id: Optional[str] = Query(None),
        include_inactive: bool = Query(False),
    ):
        """List durable external job watches (#377)."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            raise HTTPException(status_code=503, detail="Message queue not configured")

        registrations = queue_mgr.list_job_watches(
            target_session_id=target_session_id,
            include_inactive=include_inactive,
        )
        return {"watches": [_response_dict(_job_watch_to_response(reg)) for reg in registrations]}

    @app.delete("/job-watches/{watch_id}", response_model=JobWatchResponse)
    async def cancel_job_watch(watch_id: str):
        """Cancel one durable external job watch by ID (#377)."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            raise HTTPException(status_code=503, detail="Message queue not configured")

        registration = queue_mgr.cancel_job_watch(watch_id)
        if registration is None:
            raise HTTPException(status_code=404, detail="Job watch not found")
        return _job_watch_to_response(registration)

    @app.post("/sessions/{session_id}/agent-status")
    async def set_agent_status(session_id: str, request: AgentStatusRequest):
        """Agent self-reports current status, resets remind timer (#188)."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        session = app.state.session_manager.get_session(session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Session not found")

        # Update session fields; text=None clears the status (#283)
        session.agent_status_text = request.text
        session.agent_status_at = datetime.now() if request.text is not None else None
        app.state.session_manager._save_state()

        # Reset remind timer only when setting a non-null status — a clear call
        # must not disturb an active remind registration on the new task (#283).
        queue_mgr = app.state.session_manager.message_queue_manager
        if request.text is not None and queue_mgr:
            queue_mgr.reset_remind(session_id)

        return {
            "status": "updated",
            "session_id": session_id,
            "agent_status_text": request.text,
        }

    @app.post("/sessions/{target_session_id}/watch")
    async def watch_session(
        target_session_id: str,
        watcher_session_id: str,
        timeout_seconds: int,
    ):
        """Watch a session and notify when it goes idle or timeout."""
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        # Verify both sessions exist
        target_session = app.state.session_manager.get_session(target_session_id)
        if not target_session:
            raise HTTPException(status_code=404, detail="Target session not found")

        watcher_session = app.state.session_manager.get_session(watcher_session_id)
        if not watcher_session:
            raise HTTPException(status_code=404, detail="Watcher session not found")

        queue_mgr = app.state.session_manager.message_queue_manager
        if not queue_mgr:
            raise HTTPException(status_code=503, detail="Message queue not configured")

        watch_id = await queue_mgr.watch_session(target_session_id, watcher_session_id, timeout_seconds)

        target_name = _effective_session_name(target_session)

        return {
            "status": "watching",
            "watch_id": watch_id,
            "target_session_id": target_session_id,
            "target_name": target_name,
            "watcher_session_id": watcher_session_id,
            "timeout_seconds": timeout_seconds,
        }

    @app.post("/hooks/tool-use")
    async def hook_tool_use(request: Request):
        """
        Receive tool usage events from Claude Code hooks.
        """
        import asyncio

        start = time.monotonic()
        data = await request.json()
        parse_time = time.monotonic() - start

        # Our session ID (injected by hook script)
        session_manager_id = data.get("session_manager_id")

        # Claude Code's native fields
        claude_session_id = data.get("session_id")  # Claude's internal ID
        hook_type = data.get("hook_event_name")  # PreToolUse, PostToolUse, SubagentStart, SubagentStop
        tool_name = data.get("tool_name") or hook_type  # Fall back to hook_type for non-tool events
        tool_input = data.get("tool_input", {})
        tool_response = data.get("tool_response")  # Only for PostToolUse
        tool_use_id = data.get("tool_use_id")  # For Pre/Post correlation
        cwd = data.get("cwd")  # Working directory

        # Subagent context (if present)
        agent_id = data.get("agent_id")  # From SubagentStart context

        # Get session info if available
        session = None
        if session_manager_id and app.state.session_manager:
            session = app.state.session_manager.get_session(session_manager_id)

        # Clear stale is_idle on PreToolUse — signals turn has started (sm#183)
        if hook_type == "PreToolUse" and session_manager_id:
            queue_mgr = app.state.session_manager.message_queue_manager if app.state.session_manager else None
            if queue_mgr:
                queue_mgr.mark_session_active(session_manager_id)
            if session is not None:
                session.last_tool_call = datetime.now()
                session.last_tool_name = tool_name

        # Auto-acquire lock on file write (PreToolUse for Edit/Write/NotebookEdit)
        if hook_type == "PreToolUse" and tool_name in ("Edit", "Write", "NotebookEdit"):
            file_path = tool_input.get("file_path", "")

            if file_path:
                # Resolve to absolute path
                if file_path.startswith("/"):
                    abs_path = file_path
                else:
                    abs_path = str(Path(cwd) / file_path) if cwd else file_path

                # Import lock manager functions
                from .lock_manager import get_git_root, LockManager

                # Find git repo root for this file
                repo_root = get_git_root(abs_path)

                if repo_root:
                    # Try to acquire lock
                    lock_mgr = LockManager(working_dir=repo_root)
                    lock_result = lock_mgr.try_acquire(repo_root, session_manager_id)

                    if lock_result.locked_by_other:
                        # Get the other session's friendly name
                        other_session = None
                        if app.state.session_manager:
                            other_session = app.state.session_manager.get_session(lock_result.owner_session_id)

                        other_name = (
                            _effective_session_name(other_session)
                            if other_session is not None
                            else lock_result.owner_session_id
                        )

                        return {
                            "status": "error",
                            "error": f"⚠️  {repo_root} is locked by session [{other_name}].\n\n"
                                     f"Work in a separate worktree:\n"
                                     f"  git worktree add ../my-feature feature-branch\n"
                                     f"  Then edit ../my-feature/{Path(abs_path).relative_to(repo_root)}"
                        }

                    # Lock acquired - track this repo
                    if session:
                        session.touched_repos.add(repo_root)
                        app.state.session_manager._save_state()

        # Track worktree creation (PreToolUse for Bash)
        if hook_type == "PreToolUse" and tool_name == "Bash" and session:
            command = tool_input.get("command", "")

            # Detect worktree creation
            if "git worktree add" in command:
                import re
                # Parse: git worktree add <path> [<branch>]
                match = re.search(r'git\s+worktree\s+add\s+([^\s]+)', command)
                if match:
                    worktree_path = match.group(1)
                    # Resolve to absolute path
                    abs_worktree = str((Path(cwd) / worktree_path).resolve()) if cwd else worktree_path
                    session.worktrees.append(abs_worktree)
                    app.state.session_manager._save_state()
                    logger.info(f"Tracked worktree creation: {abs_worktree}")

        # Log to database (fire and forget - don't block response)
        if hasattr(app.state, 'tool_logger') and app.state.tool_logger:
            # Create task but don't await - let it run in background
            asyncio.create_task(app.state.tool_logger.log(
                session_id=session_manager_id,
                claude_session_id=claude_session_id,
                session_name=_effective_session_name(session) if session else None,
                parent_session_id=session.parent_session_id if session else None,
                hook_type=hook_type,
                tool_name=tool_name,
                tool_input=tool_input,
                tool_response=tool_response,
                tool_use_id=tool_use_id,
                cwd=cwd,
                agent_id=agent_id,
            ))

        elapsed = time.monotonic() - start
        # Get hook timing threshold from config
        config = app.state.config or {}
        hook_threshold = config.get("timeouts", {}).get("server", {}).get("hook_timing_threshold_seconds", 0.05)
        if elapsed > hook_threshold:
            logger.debug(f"hook_tool_use: parse={parse_time*1000:.1f}ms total={elapsed*1000:.1f}ms tool={tool_name}")

        return {"status": "logged"}

    @app.post("/hooks/context-usage")
    async def hook_context_usage(request: Request):
        """
        Receive context usage events from Claude Code status line and hook scripts (#203).

        Three event types:
        - compaction event (from PreCompact hook): resets one-shot flags, notifies parent
        - context_reset event (from SessionStart clear hook): resets one-shot flags
        - context usage update (from status line script): stores tokens_used, sends
          warning at warning_percentage and critical at critical_percentage (one-shot per cycle)
        """
        data = await request.json()
        session_id = data.get("session_id")

        session = app.state.session_manager.get_session(session_id) if session_id and app.state.session_manager else None
        if not session:
            return {"status": "unknown_session"}

        queue_mgr = app.state.session_manager.message_queue_manager

        # Handle compaction event (from PreCompact hook)
        # Compaction = context loss — always process, bypasses registration gate (#210).
        # Warning/critical/usage events remain opt-in via context_monitor_enabled (#206).
        if data.get("event") == "compaction":
            trigger = data.get("trigger", "unknown")
            logger.warning(
                f"Compaction fired for {_effective_session_name(session)} (trigger={trigger})"
            )
            # Set compaction flag — suppress remind delivery until compaction_complete (#249).
            session._is_compacting = True
            # Reset one-shot flags — PreCompact fires before context is refreshed,
            # so this is the reliable reset point for the next accumulation cycle.
            # Cannot rely on used_pct < warning_pct because post-compaction context
            # may land above the warning threshold (documented range: 55K–110K tokens,
            # warning at 50% = 100K — overlap is possible).
            session._context_warning_sent = False
            session._context_critical_sent = False
            # Notify via context_monitor_notify; fall back to parent_session_id so
            # unregistered children still alert their parent on context loss (#210).
            notify_target = session.context_monitor_notify or session.parent_session_id
            if notify_target and queue_mgr:
                msg = (
                    f"[sm context] Compaction fired for {_effective_session_name(session)}. "
                    "Context was compacted — agent is still running."
                )
                queue_mgr.queue_message(
                    target_session_id=notify_target,
                    text=msg,
                    delivery_mode="sequential",
                    sender_session_id=session_id,
                    message_category="context_monitor",
                )
            return {"status": "compaction_logged"}

        # Handle compaction_complete event (from post_compact_recovery.sh SessionStart hook)
        # Clears the compacting flag and resets the remind timer so the agent gets a fresh
        # soft-threshold window exactly when it wakes (#249).
        if data.get("event") == "compaction_complete":
            session._is_compacting = False
            if queue_mgr:
                queue_mgr.reset_remind(session_id, force_tracked=True)
            logger.info(
                f"Compaction complete for {_effective_session_name(session)}, remind timer reset"
            )
            return {"status": "compaction_complete_logged"}

        # Handle manual /clear event (from SessionStart clear hook)
        # Must be before the registration gate — unregistered sessions still receive
        # compaction notifications and need cancellation on context reset (#241).
        if data.get("event") == "context_reset":
            # Re-arm one-shot flags so warnings fire correctly in the new cycle.
            # Covers: TUI /clear and sm clear CLI (both trigger SessionStart source=clear).
            session._context_warning_sent = False
            session._context_critical_sent = False
            # Clear stale status from previous task (#283)
            session.agent_status_text = None
            session.agent_status_at = None
            session.agent_task_completed_at = None
            app.state.session_manager._save_state()
            if queue_mgr:
                queue_mgr.cancel_context_monitor_messages_from(session_id)
            return {"status": "flags_reset"}

        # Gate: skip unregistered sessions for usage/warning/critical events (#206)
        if not session.context_monitor_enabled:
            return {"status": "not_registered"}

        # Handle context usage update (from status line script)
        used_pct = data.get("used_percentage")
        if used_pct is None:
            # used_percentage is null before first API call — ignore
            return {"status": "ok", "used_percentage": None}

        session.tokens_used = data.get("total_input_tokens", 0)

        config = (app.state.config or {}).get("context_monitor", {})
        warning_pct = config.get("warning_percentage", 50)
        critical_pct = config.get("critical_percentage", 65)

        if used_pct >= critical_pct:
            if not session._context_critical_sent:
                session._context_critical_sent = True
                if queue_mgr:
                    is_self_alert = (session.context_monitor_notify == session.id)
                    if is_self_alert:
                        msg = (
                            f"[sm context] Context at {used_pct}% — critically high. "
                            "Write your handoff doc NOW and run `sm handoff <path>`. "
                            "Compaction is imminent."
                        )
                    else:
                        child_label = _effective_session_name(session)
                        msg = (
                            f"[sm context] Child {child_label} ({session.id}) context at {used_pct}% — critically high. "
                            "Compaction is imminent."
                        )
                    queue_mgr.queue_message(
                        target_session_id=session.context_monitor_notify,
                        text=msg,
                        delivery_mode="urgent",
                        sender_session_id=session_id,
                        message_category="context_monitor",
                    )
        elif used_pct >= warning_pct:
            if not session._context_warning_sent:
                session._context_warning_sent = True
                if queue_mgr:
                    is_self_alert = (session.context_monitor_notify == session.id)
                    if is_self_alert:
                        total = data.get("total_input_tokens", 0)
                        msg = (
                            f"[sm context] Context at {used_pct}% ({total:,} tokens). "
                            "Consider writing a handoff doc and running `sm handoff <path>`."
                        )
                    else:
                        child_label = _effective_session_name(session)
                        msg = f"[sm context] Child {child_label} ({session.id}) context at {used_pct}%."
                    queue_mgr.queue_message(
                        target_session_id=session.context_monitor_notify,
                        text=msg,
                        delivery_mode="sequential",
                        sender_session_id=session_id,
                        message_category="context_monitor",
                    )

        return {"status": "ok", "used_percentage": used_pct}

    @app.post("/admin/cleanup-idle-topics")
    async def cleanup_idle_topics(request: Request):
        """
        Close Telegram forum topics for idle/completed sessions (Fix C: sm#271).

        Mode 1 — automated (no body or empty body):
          Iterates sessions where completion_status == COMPLETED.
          Calls close_session_topic() for each.
          Returns {closed: N, skipped: M}.

        Mode 2 — explicit (body with session_ids list):
          Closes topics for the exact session IDs listed.
          Rejects sessions that are is_em=True or status=running.
          Returns {closed: N, rejected: [{id, reason}]}.
        """
        if not app.state.session_manager:
            raise HTTPException(status_code=503, detail="Session manager not configured")

        output_monitor = app.state.output_monitor
        if not output_monitor:
            raise HTTPException(status_code=503, detail="Output monitor not configured")

        from .models import CompletionStatus, SessionStatus as SStatus

        body = {}
        try:
            body = await request.json()
        except Exception:
            pass  # Empty body is valid for Mode 1

        session_ids = body.get("session_ids") if isinstance(body, dict) else None

        if session_ids is not None:
            # Mode 2: explicit session IDs
            closed = 0
            rejected = []
            for sid in session_ids:
                session = app.state.session_manager.get_session(sid)
                if not session:
                    rejected.append({"id": sid, "reason": "not found"})
                    continue
                if session.is_em:
                    rejected.append({"id": sid, "reason": "is_em=True (safety guard)"})
                    continue
                if session.status == SStatus.RUNNING:
                    rejected.append({"id": sid, "reason": "status=running"})
                    continue
                await output_monitor.close_session_topic(session, message="Manually closed")
                closed += 1
            return {"closed": closed, "rejected": rejected}

        else:
            # Mode 1: automated — only COMPLETED sessions
            closed = 0
            skipped = 0
            for session in list(app.state.session_manager.sessions.values()):
                if session.completion_status == CompletionStatus.COMPLETED:
                    if session.telegram_thread_id and session.telegram_chat_id:
                        await output_monitor.close_session_topic(session, message="Completed")
                        closed += 1
                    else:
                        skipped += 1
                else:
                    skipped += 1
            return {"closed": closed, "skipped": skipped}

    return app
