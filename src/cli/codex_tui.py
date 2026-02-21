"""Terminal UI for codex-app session progress and structured request handling."""

from __future__ import annotations

import json
import select
import shutil
import sys
import time
from collections import deque
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any, Optional, TextIO

from .client import SessionManagerClient


def normalize_mode(mode: str) -> Optional[str]:
    """Normalize mode aliases to one of chat/approval/input."""
    value = (mode or "").strip().lower()
    if value in {"chat"}:
        return "chat"
    if value in {"approval", "approve"}:
        return "approval"
    if value in {"input", "answers"}:
        return "input"
    return None


def parse_approval_decision(value: str) -> Optional[str]:
    """Parse approval decision text into API enum value."""
    normalized = (value or "").strip().lower()
    if normalized == "accept":
        return "accept"
    if normalized in {"acceptforsession", "accept-for-session", "accept_for_session"}:
        return "acceptForSession"
    if normalized == "decline":
        return "decline"
    if normalized == "cancel":
        return "cancel"
    return None


def parse_answers_json(value: str) -> Optional[dict]:
    """Parse JSON input answers. Returns None for invalid/non-object payloads."""
    try:
        parsed = json.loads(value)
    except Exception:
        return None
    return parsed if isinstance(parsed, dict) else None


def _format_ts(ts: Optional[str]) -> str:
    if not ts:
        return "--:--:--"
    try:
        dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
        return dt.astimezone().strftime("%H:%M:%S")
    except Exception:
        return "--:--:--"


def _age_s(ts: Optional[str]) -> str:
    if not ts:
        return "?"
    try:
        dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        now = datetime.now(timezone.utc)
        delta = max(0, int((now - dt.astimezone(timezone.utc)).total_seconds()))
        if delta < 60:
            return f"{delta}s"
        if delta < 3600:
            return f"{delta // 60}m"
        return f"{delta // 3600}h"
    except Exception:
        return "?"


def format_event_line(event: dict[str, Any], width: int = 120) -> str:
    """Render one codex event row for the timeline."""
    seq = event.get("seq")
    seq_text = str(seq) if isinstance(seq, int) else "-"
    event_type = event.get("event_type", "event")
    ts = _format_ts(event.get("timestamp"))
    turn_id = event.get("turn_id")
    turn_text = f" turn={turn_id[:8]}" if turn_id else ""

    payload = event.get("payload_preview")
    summary = ""
    if isinstance(payload, dict):
        for key in (
            "method",
            "status",
            "state",
            "decision",
            "error_code",
            "error_message",
            "delta_preview",
            "output_preview",
            "ledger_request_id",
        ):
            value = payload.get(key)
            if value:
                text = str(value).replace("\n", " ").strip()
                summary = f" {key}={text}"
                break
        if not summary and payload.get("truncated") and payload.get("original_chars"):
            summary = f" payload=truncated({payload['original_chars']})"

    line = f"{seq_text:>6} {ts} {event_type}{turn_text}{summary}"
    return line[: max(40, width - 2)]


@dataclass
class CodexTuiState:
    mode: str = "chat"
    next_seq: Optional[int] = None
    pending_requests: list[dict[str, Any]] = field(default_factory=list)
    selected_request_id: Optional[str] = None
    history_gap: bool = False
    gap_reason: Optional[str] = None
    status_message: str = ""
    events: deque[dict[str, Any]] = field(default_factory=lambda: deque(maxlen=200))


class CodexTui:
    """Simple terminal dashboard and input surface for codex-app sessions."""

    def __init__(
        self,
        client: SessionManagerClient,
        session_id: str,
        poll_interval: float = 1.0,
        event_limit: int = 100,
        stream_in: TextIO = sys.stdin,
        stream_out: TextIO = sys.stdout,
    ):
        self.client = client
        self.session_id = session_id
        self.poll_interval = max(0.2, poll_interval)
        self.event_limit = max(10, min(event_limit, 500))
        self.stream_in = stream_in
        self.stream_out = stream_out

        self.state = CodexTuiState()
        self.running = True
        self.session_name = session_id
        self.activity_state = "unknown"

    def bootstrap(self) -> int:
        """Validate target session and perform first sync."""
        session = self.client.get_session(self.session_id)
        if not session:
            self._status("Session not found or session manager unavailable")
            return 2
        if session.get("provider") != "codex-app":
            self._status("sm codex-tui supports only provider=codex-app sessions")
            return 1
        self.session_name = session.get("friendly_name") or session.get("name") or self.session_id
        self.activity_state = session.get("activity_state", "unknown")
        self.sync(initial=True)
        return 0

    def run(self) -> int:
        """Run interactive dashboard loop."""
        boot_rc = self.bootstrap()
        if boot_rc != 0:
            self._render_error_screen()
            return boot_rc

        try:
            while self.running:
                self._render()
                line = self._read_line_with_timeout(self.poll_interval)
                if line is None:
                    self.sync(initial=False)
                    continue
                line = line.strip()
                if line:
                    self.handle_line(line)
                self.sync(initial=False)
            return 0
        except KeyboardInterrupt:
            self._status("Interrupted")
            return 1

    def sync(self, initial: bool) -> None:
        """Refresh session state, pending requests, and events."""
        session = self.client.get_session(self.session_id)
        if not session:
            self._status("Session manager unavailable while refreshing")
            return
        self.session_name = session.get("friendly_name") or session.get("name") or self.session_id
        self.activity_state = session.get("activity_state", "unknown")

        pending_payload = self.client.get_codex_pending_requests(self.session_id)
        if pending_payload is None:
            self._status("Failed to fetch pending requests")
        else:
            self.state.pending_requests = pending_payload.get("requests", [])
            self._reconcile_selected_request()

        since_seq = None if self.state.next_seq is None else max(0, self.state.next_seq - 1)
        page = self.client.get_codex_events(self.session_id, since_seq=since_seq, limit=self.event_limit)
        if page is None:
            if initial:
                self._status("Failed to fetch codex events")
            return

        self.state.history_gap = bool(page.get("history_gap"))
        self.state.gap_reason = page.get("gap_reason")
        page_events = page.get("events", [])
        if self.state.next_seq is None:
            self.state.events.clear()
            for event in page_events:
                self.state.events.append(event)
        else:
            for event in page_events:
                self.state.events.append(event)
        self.state.next_seq = page.get("next_seq", self.state.next_seq)

    def handle_line(self, line: str) -> None:
        """Handle a command or mode-specific text submission."""
        if line.startswith("/"):
            self._handle_command(line)
            return

        if self.state.mode == "chat":
            self._send_chat(line)
        elif self.state.mode == "approval":
            self._send_approval(line)
        else:
            self._send_answers(line)

    def _handle_command(self, line: str) -> None:
        parts = line.split(" ", 1)
        cmd = parts[0].lower()
        arg = parts[1].strip() if len(parts) > 1 else ""

        if cmd in {"/quit", "/q", "/exit"}:
            self.running = False
            self._status("Exiting codex-tui")
            return
        if cmd in {"/help", "/h"}:
            self._status(
                "Commands: /mode chat|approval|input, /select <n|request_id>, /refresh, "
                "/send <text>, /approve <decision>, /answer <json>, /quit"
            )
            return
        if cmd == "/refresh":
            self.sync(initial=False)
            self._status("Refreshed")
            return
        if cmd == "/mode":
            normalized = normalize_mode(arg)
            if not normalized:
                self._status("Invalid mode. Use chat, approval, or input")
                return
            self.state.mode = normalized
            self._status(f"Mode set to {normalized}")
            return
        if cmd == "/select":
            if not arg:
                self._status("Usage: /select <index|request_id>")
                return
            self._select_request(arg)
            return
        if cmd == "/send":
            if not arg:
                self._status("Usage: /send <text>")
                return
            self._send_chat(arg)
            return
        if cmd == "/approve":
            if not arg:
                self._status("Usage: /approve accept|acceptForSession|decline|cancel")
                return
            self._send_approval(arg)
            return
        if cmd == "/answer":
            if not arg:
                self._status("Usage: /answer {\"key\":\"value\"}")
                return
            self._send_answers(arg)
            return

        self._status(f"Unknown command: {cmd}")

    def _send_chat(self, text: str) -> None:
        if self.state.pending_requests:
            self._status("Chat blocked: resolve pending structured request(s) first with /mode approval or /mode input")
            return
        result = self.client.send_input_with_result(
            self.session_id,
            text,
            sender_session_id=self.client.session_id,
            delivery_mode="sequential",
            from_sm_send=False,
        )
        if result.get("unavailable"):
            self._status("Session manager unavailable")
            return
        if result.get("ok"):
            self._status("Chat turn queued")
            return
        detail = result.get("detail")
        if isinstance(detail, dict) and detail.get("error_code") == "pending_structured_request":
            self._status("Chat blocked by pending_structured_request; switch to /mode approval or /mode input")
            return
        self._status(f"Chat send failed (HTTP {result.get('status_code', '?')})")

    def _send_approval(self, decision_text: str) -> None:
        request = self._selected_request()
        if not request:
            self._status("No pending request selected")
            return
        decision = parse_approval_decision(decision_text)
        if not decision:
            self._status("Invalid decision. Use accept|acceptForSession|decline|cancel")
            return

        result = self.client.respond_codex_request(
            self.session_id,
            request["request_id"],
            decision=decision,
        )
        if result.get("unavailable"):
            self._status("Session manager unavailable")
            return
        if result.get("ok"):
            self._status(f"Request {request['request_id']} resolved with {decision}")
            return
        self._status(f"Request response failed (HTTP {result.get('status_code', '?')})")

    def _send_answers(self, answers_text: str) -> None:
        request = self._selected_request()
        if not request:
            self._status("No pending request selected")
            return
        answers = parse_answers_json(answers_text)
        if answers is None:
            self._status("Invalid JSON object for answers")
            return

        result = self.client.respond_codex_request(
            self.session_id,
            request["request_id"],
            answers=answers,
        )
        if result.get("unavailable"):
            self._status("Session manager unavailable")
            return
        if result.get("ok"):
            self._status(f"Request {request['request_id']} answered")
            return
        self._status(f"Request response failed (HTTP {result.get('status_code', '?')})")

    def _selected_request(self) -> Optional[dict[str, Any]]:
        if not self.state.pending_requests:
            return None
        req_id = self.state.selected_request_id
        if req_id:
            for req in self.state.pending_requests:
                if req.get("request_id") == req_id:
                    return req
        return self.state.pending_requests[0]

    def _select_request(self, token: str) -> None:
        token = token.strip()
        if token.isdigit():
            idx = int(token)
            if idx < 1 or idx > len(self.state.pending_requests):
                self._status(f"Invalid request index: {idx}")
                return
            self.state.selected_request_id = self.state.pending_requests[idx - 1].get("request_id")
            self._status(f"Selected request #{idx}")
            return

        for req in self.state.pending_requests:
            if req.get("request_id") == token:
                self.state.selected_request_id = token
                self._status(f"Selected request {token}")
                return
        self._status(f"Request not found: {token}")

    def _reconcile_selected_request(self) -> None:
        if not self.state.pending_requests:
            self.state.selected_request_id = None
            if self.state.mode in {"approval", "input"}:
                self.state.mode = "chat"
            return
        ids = {req.get("request_id") for req in self.state.pending_requests}
        if self.state.selected_request_id not in ids:
            self.state.selected_request_id = self.state.pending_requests[0].get("request_id")

    def _status(self, message: str) -> None:
        self.state.status_message = message

    def _render_error_screen(self) -> None:
        self.stream_out.write(f"{self.state.status_message}\n")
        self.stream_out.flush()

    def _render(self) -> None:
        if self.stream_out.isatty():
            self.stream_out.write("\x1b[2J\x1b[H")

        width = shutil.get_terminal_size((120, 40)).columns
        pending = self.state.pending_requests
        selected = self.state.selected_request_id

        lines: list[str] = []
        lines.append(f"sm codex-tui | {self.session_name} ({self.session_id})")
        lines.append(
            f"state={self.activity_state} mode={self.state.mode} pending={len(pending)} "
            f"next_seq={self.state.next_seq if self.state.next_seq is not None else '-'}"
        )
        if self.state.history_gap:
            reason = self.state.gap_reason or "unknown"
            lines.append(f"history_gap=true reason={reason}")
        if self.state.status_message:
            lines.append(f"status: {self.state.status_message}")
        lines.append("")
        lines.append("Pending Requests")
        lines.append("-" * min(width, 80))
        if not pending:
            lines.append("(none)")
        else:
            for idx, req in enumerate(pending, start=1):
                marker = "*" if req.get("request_id") == selected else " "
                req_id = req.get("request_id", "?")
                req_type = req.get("request_type", "?")
                age = _age_s(req.get("requested_at"))
                lines.append(f"{marker} {idx:>2}. {req_type:<18} {req_id} ({age} ago)")
        lines.append("")
        lines.append("Event Timeline")
        lines.append("-" * min(width, 80))
        if not self.state.events:
            lines.append("(no events)")
        else:
            for event in list(self.state.events)[-12:]:
                lines.append(format_event_line(event, width=width))

        lines.append("")
        lines.append(
            "Commands: /mode chat|approval|input | /select <n|request_id> | /send <text> | "
            "/approve <decision> | /answer <json> | /refresh | /quit"
        )
        if self.state.mode == "approval":
            lines.append("Approval mode: Enter decision directly (accept|acceptForSession|decline|cancel).")
        elif self.state.mode == "input":
            lines.append("Input mode: Enter JSON object directly, e.g. {\"answer\":\"yes\"}.")
        else:
            lines.append("Chat mode: Enter plain text turn.")
            if pending:
                lines.append("Chat is disabled while pending structured requests exist.")

        lines.append("")
        lines.append("> ")
        self.stream_out.write("\n".join(lines))
        self.stream_out.flush()

    def _read_line_with_timeout(self, timeout_s: float) -> Optional[str]:
        supports_select = hasattr(self.stream_in, "fileno")
        if supports_select:
            try:
                self.stream_in.fileno()
            except Exception:
                supports_select = False

        if not supports_select:
            line = self.stream_in.readline()
            if line == "":
                self.running = False
                return None
            return line
        try:
            ready, _, _ = select.select([self.stream_in], [], [], timeout_s)
        except Exception:
            time.sleep(timeout_s)
            return None
        if not ready:
            return None
        line = self.stream_in.readline()
        if line == "":
            self.running = False
            return None
        return line.rstrip("\n")


def run_codex_tui(
    client: SessionManagerClient,
    session_id: str,
    poll_interval: float = 1.0,
    event_limit: int = 100,
) -> int:
    """Entry point for `sm codex-tui` command."""
    return CodexTui(
        client=client,
        session_id=session_id,
        poll_interval=poll_interval,
        event_limit=event_limit,
    ).run()
