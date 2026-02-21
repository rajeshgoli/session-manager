"""Provider-neutral projection of codex-app observability rows for CLI surfaces."""

from __future__ import annotations

from datetime import datetime, timedelta
from typing import Any, Optional

from .codex_observability_logger import CodexObservabilityLogger


class CodexActivityProjection:
    """Projects codex observability events into provider-neutral action summaries."""

    def __init__(self, observability_logger: CodexObservabilityLogger):
        self.observability_logger = observability_logger

    def recent_actions(self, session_id: str, limit: int = 20) -> list[dict[str, Any]]:
        rows = self.observability_logger.list_recent_tool_events(session_id=session_id, limit=limit)
        return [self._project_row(row) for row in rows]

    def latest_action(self, session_id: str) -> Optional[dict[str, Any]]:
        actions = self.recent_actions(session_id=session_id, limit=1)
        if not actions:
            return None
        return actions[-1]

    def _project_row(self, row: dict[str, Any]) -> dict[str, Any]:
        event_type = str(row.get("event_type", "unknown"))
        item_type = row.get("item_type")
        created_at = row.get("created_at")
        latency_ms = row.get("latency_ms")
        started_at = self._derive_started_at(created_at, latency_ms)
        ended_at = created_at if self._is_terminal_event(event_type) else None

        return {
            "source_provider": "codex-app",
            "action_kind": self._action_kind(event_type=event_type, item_type=item_type),
            "summary_text": self._summary_text(row),
            "status": self._status(event_type, row.get("final_status")),
            "started_at": started_at,
            "ended_at": ended_at,
            "session_id": row.get("session_id"),
            "turn_id": row.get("turn_id"),
            "item_id": row.get("item_id"),
        }

    def _derive_started_at(self, created_at: Optional[str], latency_ms: Optional[int]) -> Optional[str]:
        if not created_at:
            return None
        if not latency_ms:
            return created_at
        try:
            ended = datetime.fromisoformat(created_at)
            started = ended - timedelta(milliseconds=int(latency_ms))
            return started.isoformat()
        except Exception:
            return created_at

    def _action_kind(self, *, event_type: str, item_type: Optional[str]) -> str:
        if event_type in ("request_approval", "approval_decision"):
            return "approval"
        if event_type in ("request_user_input", "user_input_submitted"):
            return "user_input"
        if item_type == "commandExecution":
            return "command"
        if item_type == "fileChange":
            return "file_change"
        return "tool"

    def _status(self, event_type: str, final_status: Optional[str]) -> str:
        terminal = {"completed", "failed", "interrupted", "cancelled", "timeout"}
        if event_type in ("request_approval", "request_user_input"):
            return "pending"
        if event_type in terminal:
            return str(final_status or event_type)
        if event_type in ("approval_decision", "user_input_submitted"):
            return "completed"
        return "running"

    def _summary_text(self, row: dict[str, Any]) -> str:
        event_type = str(row.get("event_type", "unknown"))
        item_type = row.get("item_type") or "tool"
        command = row.get("command")
        file_path = row.get("file_path")
        decision = row.get("approval_decision")
        error_message = row.get("error_message")

        if event_type == "request_approval":
            return f"Approval requested ({item_type})"
        if event_type == "approval_decision":
            if decision:
                return f"Approval decision: {decision}"
            return "Approval decision submitted"
        if event_type == "request_user_input":
            return "User input requested"
        if event_type == "user_input_submitted":
            return "User input submitted"
        if event_type == "started":
            if command:
                return f"Started: {str(command)[:80]}"
            if file_path:
                return f"Started file change: {file_path}"
            return f"Started {item_type}"
        if event_type == "output_delta":
            if command:
                return f"Output update: {str(command)[:60]}"
            if file_path:
                return f"File update: {file_path}"
            return "Output update"
        if event_type in ("completed", "failed", "interrupted", "cancelled", "timeout"):
            target = str(command or file_path or item_type)
            if error_message and event_type == "failed":
                return f"Failed {target}: {str(error_message)[:120]}"
            return f"{event_type.capitalize()} {target}"
        return f"{event_type} ({item_type})"

    def _is_terminal_event(self, event_type: str) -> bool:
        return event_type in {
            "completed",
            "failed",
            "interrupted",
            "cancelled",
            "timeout",
            "approval_decision",
            "user_input_submitted",
        }
