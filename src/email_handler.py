"""Email bridge helpers and legacy notification harness support."""

from __future__ import annotations

import asyncio
import html
import logging
import re
import sys
from dataclasses import dataclass
from email import policy
from email.parser import BytesParser
from pathlib import Path
from typing import Any, Optional

import httpx
import yaml

logger = logging.getLogger(__name__)

# Path to existing email automation
EMAIL_HARNESS_PATH = Path(__file__).parent.parent.parent.parent / "claude-email-automation"
DEFAULT_BRIDGE_CONFIG_PATH = Path(__file__).resolve().parents[1] / "config" / "email_send.yaml"
DEFAULT_EMAIL_WEBHOOK_PATH = "/api/email-inbound"
DEFAULT_EMAIL_WORKER_SECRET_HEADER = "x-email-worker-secret"
DEFAULT_EMAIL_SESSION_ID_HEADER = "x-email-session-id"
MAX_EMAIL_SUBJECT_LENGTH = 140
MARKDOWN_LINK_RE = re.compile(r"\[([^\]]+)\]\((https?://[^)\s]+)\)")
MARKDOWN_CODE_RE = re.compile(r"`([^`]+)`")
MARKDOWN_STRONG_RE = re.compile(r"\*\*([^*]+)\*\*")
MARKDOWN_EM_RE = re.compile(r"(?<!\*)\*([^*]+)\*(?!\*)")
HTML_TAG_RE = re.compile(r"<[^>]+>")
WHITESPACE_RE = re.compile(r"\s+")
ROUTING_FOOTER_RE = re.compile(r"(?im)^\s*>*\s*SM:\s+(.+?)\s+([a-z0-9]{6,})\s+([a-z0-9-]+)\s*$")
SESSION_ID_RE = re.compile(r"^[a-z0-9]{6,}$")


@dataclass(frozen=True)
class RegisteredEmailUser:
    """Resolved user entry from the gitignored email bridge config."""

    username: str
    email: str
    display_name: str
    aliases: tuple[str, ...]


class EmailHandler:
    """Handles legacy email notifications and Resend-based agent email bridging."""

    def __init__(
        self,
        email_config: str = "",
        imap_config: str = "",
        bridge_config: str = "",
    ):
        # Legacy harness config
        self.email_config = Path(email_config) if email_config else EMAIL_HARNESS_PATH / "email.yaml"
        self.imap_config = Path(imap_config) if imap_config else EMAIL_HARNESS_PATH / "imap.yaml"
        self.bridge_config = Path(bridge_config).expanduser() if bridge_config else DEFAULT_BRIDGE_CONFIG_PATH

        # Add harness path to sys.path for imports
        harness_str = str(EMAIL_HARNESS_PATH)
        if harness_str not in sys.path:
            sys.path.insert(0, harness_str)

        self._send_module = None
        self._wait_module = None
        self._bridge_cache: Optional[dict[str, Any]] = None
        self._bridge_cache_mtime_ns: Optional[int] = None

    def _load_modules(self):
        """Lazy load the legacy email harness modules."""
        if self._send_module is None:
            try:
                import send_completion_email as send_module
                import wait_for_response as wait_module

                self._send_module = send_module
                self._wait_module = wait_module
                logger.info("Loaded legacy email harness modules")
            except ImportError as exc:
                logger.error("Failed to import email harness: %s", exc)
                logger.error("Expected harness at: %s", EMAIL_HARNESS_PATH)
                raise

    def is_available(self) -> bool:
        """Check if the legacy email harness is available and configured."""
        if not EMAIL_HARNESS_PATH.exists():
            logger.warning("Email harness not found at %s", EMAIL_HARNESS_PATH)
            return False

        if not self.email_config.exists():
            logger.warning("Email config not found at %s", self.email_config)
            return False

        return True

    def _load_bridge_config(self) -> dict[str, Any]:
        """Load and cache the gitignored email bridge config."""
        if not self.bridge_config.exists():
            self._bridge_cache = {}
            self._bridge_cache_mtime_ns = None
            return {}

        stat = self.bridge_config.stat()
        if self._bridge_cache is not None and self._bridge_cache_mtime_ns == stat.st_mtime_ns:
            return self._bridge_cache

        try:
            data = yaml.safe_load(self.bridge_config.read_text(encoding="utf-8")) or {}
        except Exception as exc:
            logger.error("Failed to load email bridge config %s: %s", self.bridge_config, exc)
            data = {}

        if not isinstance(data, dict):
            logger.error("Email bridge config %s must contain a YAML mapping", self.bridge_config)
            data = {}

        self._bridge_cache = data
        self._bridge_cache_mtime_ns = stat.st_mtime_ns
        return data

    def bridge_is_available(self) -> bool:
        """Whether the Resend-based email bridge is configured."""
        resend = (self._load_bridge_config().get("resend") or {})
        return bool(str(resend.get("api_key") or "").strip() and str(resend.get("domain") or "").strip())

    def bridge_webhook_path(self) -> str:
        """Return the configured inbound email webhook path."""
        bridge = (self._load_bridge_config().get("email_bridge") or {})
        raw_path = str(bridge.get("webhook_path") or DEFAULT_EMAIL_WEBHOOK_PATH).strip() or DEFAULT_EMAIL_WEBHOOK_PATH
        return raw_path if raw_path.startswith("/") else f"/{raw_path}"

    def bridge_worker_secret_header(self) -> str:
        """Return the header name used for the inbound worker shared secret."""
        bridge = (self._load_bridge_config().get("email_bridge") or {})
        raw_header = str(bridge.get("worker_secret_header") or DEFAULT_EMAIL_WORKER_SECRET_HEADER).strip().lower()
        return raw_header or DEFAULT_EMAIL_WORKER_SECRET_HEADER

    def bridge_worker_secret(self) -> Optional[str]:
        """Return the configured inbound worker shared secret, if any."""
        bridge = (self._load_bridge_config().get("email_bridge") or {})
        secret = str(bridge.get("worker_secret") or "").strip()
        return secret or None

    def bridge_session_id_header(self) -> str:
        """Return the trusted inbound header name for explicit session-id routing."""
        bridge = (self._load_bridge_config().get("email_bridge") or {})
        raw_header = str(bridge.get("session_id_header") or DEFAULT_EMAIL_SESSION_ID_HEADER).strip().lower()
        return raw_header or DEFAULT_EMAIL_SESSION_ID_HEADER

    def authorized_senders(self) -> set[str]:
        """Return normalized allowlisted sender addresses for inbound replies."""
        bridge = (self._load_bridge_config().get("email_bridge") or {})
        allowlist = bridge.get("authorized_senders") or []
        if isinstance(allowlist, str):
            allowlist = [allowlist]
        return {
            str(address).strip().lower()
            for address in allowlist
            if str(address).strip()
        }

    def is_authorized_sender(self, address: str) -> bool:
        """Return True when the sender email is explicitly allowlisted."""
        normalized = str(address or "").strip().lower()
        allowlist = self.authorized_senders()
        return bool(normalized and normalized in allowlist)

    def normalize_explicit_session_id(self, value: str) -> Optional[str]:
        """Validate and normalize an explicit worker-supplied session id."""
        normalized = str(value or "").strip().lower()
        if not normalized or not SESSION_ID_RE.fullmatch(normalized):
            return None
        return normalized

    def lookup_user(self, identifier: str) -> Optional[RegisteredEmailUser]:
        """Resolve one registered user by username or alias."""
        needle = str(identifier or "").strip().lower()
        if not needle:
            return None

        users = self._load_bridge_config().get("users") or {}
        if not isinstance(users, dict):
            return None

        for username, raw_spec in users.items():
            resolved = self._normalize_user_spec(username, raw_spec)
            if resolved is None:
                continue
            if needle in resolved.aliases:
                return resolved
        return None

    def resolve_users(self, identifiers: list[str]) -> list[RegisteredEmailUser]:
        """Resolve a list of usernames/aliases into distinct registered users."""
        resolved: list[RegisteredEmailUser] = []
        seen_emails: set[str] = set()

        for identifier in identifiers:
            user = self.lookup_user(identifier)
            if user is None:
                raise LookupError(f"No registered email user found for '{identifier}'")
            email_key = user.email.lower()
            if email_key in seen_emails:
                continue
            seen_emails.add(email_key)
            resolved.append(user)

        if not resolved:
            raise LookupError("No registered email users were provided")
        return resolved

    def _normalize_user_spec(self, username: Any, raw_spec: Any) -> Optional[RegisteredEmailUser]:
        """Normalize one YAML user record into a consistent shape."""
        normalized_username = str(username or "").strip()
        if not normalized_username:
            return None

        if isinstance(raw_spec, str):
            email_address = raw_spec.strip()
            display_name = normalized_username
            aliases: list[str] = [normalized_username]
        elif isinstance(raw_spec, dict):
            email_address = str(raw_spec.get("email") or "").strip()
            display_name = str(raw_spec.get("name") or normalized_username).strip() or normalized_username
            aliases = [normalized_username]
            raw_aliases = raw_spec.get("aliases") or []
            if isinstance(raw_aliases, str):
                raw_aliases = [raw_aliases]
            aliases.extend(str(alias).strip() for alias in raw_aliases if str(alias).strip())
        else:
            return None

        if not email_address:
            return None

        normalized_aliases = tuple({alias.lower() for alias in aliases if alias})
        return RegisteredEmailUser(
            username=normalized_username,
            email=email_address,
            display_name=display_name,
            aliases=normalized_aliases,
        )

    def _bridge_reply_domain(self) -> str:
        resend = (self._load_bridge_config().get("resend") or {})
        reply_domain = str(resend.get("reply_domain") or resend.get("domain") or "").strip()
        if not reply_domain:
            raise RuntimeError("Email bridge is missing resend.domain")
        return reply_domain

    def _bridge_reply_address(self, sender_session_id: str) -> str:
        """Return the reply-routable mailbox address for agent email."""
        resend = (self._load_bridge_config().get("resend") or {})
        reply_address = str(resend.get("reply_address") or resend.get("from_address") or "").strip()
        if reply_address:
            return reply_address
        reply_domain = self._bridge_reply_domain()
        return f"{sender_session_id}@{reply_domain}"

    def _bridge_api_key(self) -> str:
        resend = (self._load_bridge_config().get("resend") or {})
        api_key = str(resend.get("api_key") or "").strip()
        if not api_key:
            raise RuntimeError("Email bridge is missing resend.api_key")
        return api_key

    def _bridge_api_base_url(self) -> str:
        resend = (self._load_bridge_config().get("resend") or {})
        return str(resend.get("api_base_url") or "https://api.resend.com").rstrip("/")

    def _default_subject(self, sender_name: str, body_text: str) -> str:
        """Generate a deterministic email subject from the first non-empty line."""
        subject = ""
        for raw_line in body_text.splitlines():
            line = WHITESPACE_RE.sub(" ", raw_line).strip().lstrip("#*- ")
            if line:
                subject = line
                break
        if not subject:
            subject = f"Message from {sender_name or 'Session Manager'}"
        return subject[:MAX_EMAIL_SUBJECT_LENGTH]

    def _strip_html(self, body_html: str) -> str:
        """Best-effort conversion from HTML into plain text."""
        text = re.sub(r"(?i)<br\s*/?>", "\n", body_html)
        text = re.sub(r"(?i)</p\s*>", "\n\n", text)
        text = re.sub(r"(?i)</li\s*>", "\n", text)
        text = HTML_TAG_RE.sub("", text)
        text = html.unescape(text)
        text = re.sub(r"\n{3,}", "\n\n", text)
        return text.strip()

    def _render_inline_markdown(self, text: str) -> str:
        """Render a narrow markdown subset for email HTML bodies."""

        def _render_link(match: re.Match[str]) -> str:
            label = html.escape(match.group(1))
            href = html.escape(match.group(2), quote=True)
            return f'<a href="{href}">{label}</a>'

        escaped = html.escape(text)
        escaped = MARKDOWN_LINK_RE.sub(_render_link, escaped)
        escaped = MARKDOWN_CODE_RE.sub(lambda m: f"<code>{html.escape(m.group(1))}</code>", escaped)
        escaped = MARKDOWN_STRONG_RE.sub(lambda m: f"<strong>{html.escape(m.group(1))}</strong>", escaped)
        escaped = MARKDOWN_EM_RE.sub(lambda m: f"<em>{html.escape(m.group(1))}</em>", escaped)
        return escaped

    def render_markdown_to_html(self, body_text: str) -> str:
        """Render basic markdown to HTML without extra dependencies."""
        lines = body_text.splitlines()
        parts: list[str] = []
        list_open = False
        code_open = False
        code_lines: list[str] = []
        paragraph: list[str] = []

        def flush_paragraph() -> None:
            if not paragraph:
                return
            parts.append(f"<p>{'<br/>'.join(paragraph)}</p>")
            paragraph.clear()

        def flush_list() -> None:
            nonlocal list_open
            if list_open:
                parts.append("</ul>")
                list_open = False

        def flush_code() -> None:
            nonlocal code_open
            if code_open:
                code_text = "\n".join(code_lines)
                parts.append(f"<pre><code>{html.escape(code_text)}</code></pre>")
                code_lines.clear()
                code_open = False

        for raw_line in lines:
            line = raw_line.rstrip("\n")
            stripped = line.strip()

            if stripped.startswith("```"):
                flush_paragraph()
                flush_list()
                if code_open:
                    flush_code()
                else:
                    code_open = True
                continue

            if code_open:
                code_lines.append(line)
                continue

            if not stripped:
                flush_paragraph()
                flush_list()
                continue

            heading_match = re.match(r"^(#{1,6})\s+(.*)$", stripped)
            if heading_match:
                flush_paragraph()
                flush_list()
                level = len(heading_match.group(1))
                parts.append(f"<h{level}>{self._render_inline_markdown(heading_match.group(2).strip())}</h{level}>")
                continue

            list_match = re.match(r"^[-*+]\s+(.*)$", stripped)
            if list_match:
                flush_paragraph()
                if not list_open:
                    parts.append("<ul>")
                    list_open = True
                parts.append(f"<li>{self._render_inline_markdown(list_match.group(1).strip())}</li>")
                continue

            flush_list()
            paragraph.append(self._render_inline_markdown(stripped))

        flush_paragraph()
        flush_list()
        flush_code()
        if not parts:
            return "<p></p>"
        return "\n".join(parts)

    def _plain_text_to_html(self, body_text: str) -> str:
        """Convert plain text to readable HTML paragraphs."""
        paragraphs = [chunk.strip() for chunk in re.split(r"\n\s*\n", body_text) if chunk.strip()]
        if not paragraphs:
            return "<p></p>"
        return "\n".join(
            f"<p>{html.escape(paragraph).replace(chr(10), '<br/>')}</p>"
            for paragraph in paragraphs
        )

    def build_routing_footer(self, *, sender_name: str, sender_session_id: str, sender_provider: str) -> str:
        """Build the compact routing footer embedded in outbound email bodies."""
        normalized_name = WHITESPACE_RE.sub(" ", str(sender_name or "").strip()) or "session"
        normalized_provider = WHITESPACE_RE.sub(" ", str(sender_provider or "").strip()) or "unknown"
        return f"SM: {normalized_name} {sender_session_id} {normalized_provider}"

    def append_routing_footer(self, *, body_text: str, body_html: str, footer_line: str) -> tuple[str, str]:
        """Append a compact routing footer to both text and HTML email bodies."""
        normalized_text = body_text.rstrip()
        normalized_html = body_html.rstrip()
        text_with_footer = f"{normalized_text}\n\n--\n{footer_line}" if normalized_text else f"--\n{footer_line}"
        html_footer = f"<hr/>\n<p>{html.escape(footer_line)}</p>"
        html_with_footer = f"{normalized_html}\n{html_footer}" if normalized_html else html_footer
        return text_with_footer, html_with_footer

    def extract_routed_session_id(self, body_text: str) -> Optional[str]:
        """Extract the routed session id from the last compact footer in an inbound email body."""
        normalized_body = str(body_text or "").replace("\r\n", "\n")
        matches = list(ROUTING_FOOTER_RE.finditer(normalized_body))
        if not matches:
            return None
        return matches[-1].group(2)

    def extract_reply_message_body(self, body_text: str) -> str:
        """Strip quoted history and the routing footer from an inbound email body."""
        normalized_body = str(body_text or "").replace("\r\n", "\n").strip()
        if not normalized_body:
            return ""

        lines = normalized_body.split("\n")
        body_lines: list[str] = []
        for line in lines:
            trimmed = line.strip()
            if (
                line.startswith(">")
                or re.match(r"^On .+wrote:$", trimmed, re.IGNORECASE)
                or re.match(r"^From:\s", line)
                or re.match(r"^Sent:\s", line)
                or re.match(r"^Subject:\s", line)
                or re.match(r"^To:\s", line)
            ):
                break
            body_lines.append(line)

        cleaned_lines = body_lines[:]
        while cleaned_lines and not cleaned_lines[-1].strip():
            cleaned_lines.pop()
        if cleaned_lines and ROUTING_FOOTER_RE.match(cleaned_lines[-1].strip()):
            cleaned_lines.pop()
            while cleaned_lines and not cleaned_lines[-1].strip():
                cleaned_lines.pop()
            if cleaned_lines and cleaned_lines[-1].strip() == "--":
                cleaned_lines.pop()
        return "\n".join(cleaned_lines).strip()

    def extract_text_from_raw_email(self, raw_email: str) -> str:
        """Parse a raw RFC822 email and return the best-effort plain-text body."""
        normalized = str(raw_email or "").strip()
        if not normalized:
            return ""

        try:
            message = BytesParser(policy=policy.default).parsebytes(normalized.encode("utf-8", errors="replace"))
        except Exception:
            return normalized

        if message.is_multipart():
            plain_parts: list[str] = []
            html_parts: list[str] = []
            for part in message.walk():
                if part.is_multipart():
                    continue
                if str(part.get_content_disposition() or "").lower() == "attachment":
                    continue
                content_type = str(part.get_content_type() or "").lower()
                try:
                    content = part.get_content()
                except Exception:
                    try:
                        payload = part.get_payload(decode=True) or b""
                        charset = part.get_content_charset() or "utf-8"
                        content = payload.decode(charset, errors="replace")
                    except Exception:
                        content = ""
                if not isinstance(content, str):
                    continue
                if content_type == "text/plain":
                    plain_parts.append(content)
                elif content_type == "text/html":
                    html_parts.append(content)
            if plain_parts:
                return "\n".join(part.strip() for part in plain_parts if part.strip()).strip()
            if html_parts:
                return self._strip_html("\n".join(part.strip() for part in html_parts if part.strip()))

        try:
            content = message.get_content()
        except Exception:
            return normalized
        if isinstance(content, str):
            if str(message.get_content_type() or "").lower() == "text/html":
                return self._strip_html(content)
            return content.strip()
        return normalized

    def extract_subject_from_raw_email(self, raw_email: str) -> Optional[str]:
        """Parse a raw RFC822 email and return a normalized Subject header."""
        normalized = str(raw_email or "").strip()
        if not normalized:
            return None

        try:
            message = BytesParser(policy=policy.default).parsebytes(normalized.encode("utf-8", errors="replace"))
        except Exception:
            return None

        subject = str(message.get("Subject") or "").strip()
        if not subject:
            return None
        return WHITESPACE_RE.sub(" ", subject).strip()

    async def send_agent_email(
        self,
        *,
        sender_session_id: str,
        sender_name: str,
        sender_provider: str,
        to_identifiers: list[str],
        cc_identifiers: Optional[list[str]] = None,
        subject: Optional[str] = None,
        body_text: Optional[str] = None,
        body_html: Optional[str] = None,
        body_markdown: bool = False,
        auto_subject: bool = False,
    ) -> dict[str, Any]:
        """Send a reply-routable email from one managed session to registered user(s)."""
        if not self.bridge_is_available():
            raise RuntimeError(f"Email bridge config is unavailable at {self.bridge_config}")

        normalized_sender_id = str(sender_session_id or "").strip()
        normalized_sender_name = str(sender_name or "").strip() or normalized_sender_id
        if not normalized_sender_id:
            raise ValueError("Managed sender session is required for agent email delivery")

        text_payload = (body_text or "").strip()
        html_payload = (body_html or "").strip()
        if not text_payload and not html_payload:
            raise ValueError("Email body is required")

        to_users = self.resolve_users(to_identifiers)
        cc_users = self.resolve_users(cc_identifiers or []) if cc_identifiers else []

        if body_markdown and text_payload and not html_payload:
            html_payload = self.render_markdown_to_html(text_payload)
        elif text_payload and not html_payload:
            html_payload = self._plain_text_to_html(text_payload)
        elif html_payload and not text_payload:
            text_payload = self._strip_html(html_payload)

        resolved_subject = str(subject or "").strip()
        if not resolved_subject:
            if not auto_subject:
                raise ValueError("Email subject is required")
            resolved_subject = self._default_subject(normalized_sender_name, text_payload)

        footer_line = self.build_routing_footer(
            sender_name=normalized_sender_name,
            sender_session_id=normalized_sender_id,
            sender_provider=sender_provider,
        )
        text_payload, html_payload = self.append_routing_footer(
            body_text=text_payload,
            body_html=html_payload,
            footer_line=footer_line,
        )

        from_address = self._bridge_reply_address(normalized_sender_id)
        from_header = f"{normalized_sender_name} <{from_address}>"
        payload = {
            "from": from_header,
            "to": [user.email for user in to_users],
            "subject": resolved_subject,
            "text": text_payload,
            "reply_to": from_address,
            "headers": {
                "X-SM-Session-ID": normalized_sender_id,
            },
        }
        if cc_users:
            payload["cc"] = [user.email for user in cc_users]
        if html_payload:
            payload["html"] = html_payload

        endpoint = f"{self._bridge_api_base_url()}/emails"
        headers = {
            "Authorization": f"Bearer {self._bridge_api_key()}",
            "Content-Type": "application/json",
        }

        try:
            async with httpx.AsyncClient(timeout=15.0) as client:
                response = await client.post(endpoint, headers=headers, json=payload)
        except httpx.HTTPError as exc:
            raise RuntimeError(f"Resend email send failed: {exc}") from exc

        if response.status_code >= 400:
            try:
                error_payload = response.json()
            except ValueError:
                error_payload = response.text
            raise RuntimeError(f"Resend email send failed ({response.status_code}): {error_payload}")

        try:
            response_payload = response.json()
        except ValueError:
            response_payload = {}

        return {
            "subject": resolved_subject,
            "to": [{"username": user.username, "email": user.email} for user in to_users],
            "cc": [{"username": user.username, "email": user.email} for user in cc_users],
            "message_id": response_payload.get("id"),
            "from": from_header,
            "reply_to": from_address,
        }

    async def send_notification(
        self,
        session_id: str,
        message: str,
        urgent: bool = False,
    ) -> bool:
        """
        Send a legacy notification email for a session.

        The maintainer email-bridge feature uses `send_agent_email()` instead. This
        method remains as a compatibility wrapper for existing notification flows.
        """
        if not self.is_available():
            logger.warning("Email not available, skipping notification")
            return False

        try:
            self._load_modules()
            config = self._send_module.load_email_config(str(self.email_config))
            success = self._send_module.send_completion_email(
                session_id=session_id,
                body_content=message,
                config=config,
                urgent=urgent,
            )
            if success:
                logger.info("Email sent for session %s", session_id)
            else:
                logger.error("Failed to send email for session %s", session_id)
            return success
        except Exception as exc:
            logger.error("Email send error: %s", exc)
            return False

    async def wait_for_response(
        self,
        session_id: str,
        timeout: int = 3600,
    ) -> Optional[str]:
        """Wait for a legacy email response with the given session ID."""
        if not self.is_available():
            logger.warning("Email not available")
            return None

        if not self.imap_config.exists():
            logger.warning("IMAP config not found at %s", self.imap_config)
            return None

        try:
            self._load_modules()
            config = self._wait_module.load_imap_config(str(self.imap_config))
            loop = asyncio.get_event_loop()
            body = await loop.run_in_executor(
                None,
                self._wait_module.wait_for_response,
                session_id,
                config,
                timeout,
            )
            if body:
                logger.info("Received email response for session %s", session_id)
            return body
        except Exception as exc:
            logger.error("Email wait error: %s", exc)
            return None
