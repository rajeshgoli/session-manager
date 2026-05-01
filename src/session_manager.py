"""Session registry and lifecycle management."""

import asyncio
import contextlib
import json
import logging
import os
import re
import shlex
import shutil
import socket
import subprocess
import textwrap
import threading
import time
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Optional, Callable, Awaitable, Any

from .models import (
    AgentRegistration,
    ActivityState,
    AdoptionProposal,
    AdoptionProposalStatus,
    CompletionStatus,
    DeliveryResult,
    NotificationEvent,
    ReviewConfig,
    Session,
    SessionStatus,
    TelegramTopicRecord,
)
from .tmux_controller import TmuxController
from .codex_app_server import CodexAppServerSession, CodexAppServerConfig, CodexAppServerError
from .codex_activity_projection import CodexActivityProjection
from .codex_event_store import CodexEventStore
from .codex_observability_logger import CodexObservabilityLogger
from .codex_provider_policy import (
    CODEX_APP_RETIRED_SESSION_REASON,
    get_codex_app_policy,
    normalize_provider_mapping_phase,
)
from .codex_request_ledger import CodexRequestLedger
from .github_reviews import post_pr_review_comment, poll_for_codex_review, get_pr_repo_from_git
from .queue_runner import QueueRunner

logger = logging.getLogger(__name__)

DEFAULT_LOG_DIR = "/tmp/claude-sessions"
DEFAULT_SESSION_STATE_FILE = "~/.local/share/claude-sessions/sessions.json"
LEGACY_TMP_SESSION_STATE_FILE = "/tmp/claude-sessions/sessions.json"
CODEX_FORK_DISABLE_STARTUP_UPDATE_ARGS = ["-c", "check_for_update_on_startup=false"]

DEFAULT_MAINTAINER_BOOTSTRAP_PROMPT = textwrap.dedent(
    """
    As engineer, act as the Session Manager maintainer service agent for this repository.

    Role:
    - You own the incoming maintainer queue for Session Manager.
    - Agents will report bugs and maintenance requests via `sm send maintainer "..."`.
    - Keep the `maintainer` registry role for this session.

    Workflow:
    - Investigate against real behavior first; do not speculate from code alone.
    - File or update a GitHub ticket when needed.
    - Implement the fix with focused changes and tests.
    - Restart Session Manager with `launchctl`.
    - Open a PR, comment `@codex review`, poll about every 3 minutes, address feedback, and if review is clean or silent after reasonable polling, merge and delete the branch.
    - Restart Session Manager again after merge.
    - Keep working until the reported issue is resolved end-to-end.

    Communication:
    - Do not send acknowledgements unless the reporter asks for one.
    - Use concise status updates only when needed for blockers or explicit follow-up.

    Repository:
    - Work in {working_dir}.
    """
).strip()

DEFAULT_MAINTAINER_TASK_COMPLETE_TTL_SECONDS = 600

DEFAULT_SERVICE_ROLE_BOOTSTRAP_PROMPT = textwrap.dedent(
    """
    Act as the {role} service agent for this repository.

    Role:
    - Keep the `{role}` registry role for this session.
    - Process requests delivered via `sm send {role} "..."`.

    Communication:
    - Work through the incoming queue for this role until the request is resolved.

    Repository:
    - Work in {working_dir}.
    """
).strip()

ROLE_KEYWORDS = (
    "engineer",
    "architect",
    "scout",
    "reviewer",
    "product",
    "director",
    "ux",
)

SECRET_FIELD_PATTERN = re.compile(
    r"(token|secret|password|passwd|authorization|cookie|api[_-]?key|access[_-]?key|session[_-]?key)",
    re.IGNORECASE,
)
BEARER_TOKEN_PATTERN = re.compile(r"(?i)\bbearer\s+[a-z0-9._~+/=-]{6,}")
INLINE_SECRET_PATTERN = re.compile(
    r"(?i)\b(token|secret|password|api[_-]?key|access[_-]?key)\b\s*[:=]\s*([^\s,;]+)"
)


def _coerce_rollout_flag(value: Any, default: bool = True) -> bool:
    """Parse rollout config values robustly (supports bools and common string forms)."""
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    if isinstance(value, (int, float)):
        return value != 0
    if isinstance(value, str):
        normalized = value.strip().lower()
        if normalized in {"true", "1", "yes", "on"}:
            return True
        if normalized in {"false", "0", "no", "off"}:
            return False
    return default


class SessionManager:
    """Manages the lifecycle of Claude Code sessions."""

    def __init__(
        self,
        log_dir: str = DEFAULT_LOG_DIR,
        state_file: str = DEFAULT_SESSION_STATE_FILE,
        config: Optional[dict] = None,
    ):
        self.log_dir = Path(log_dir).expanduser()
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self.state_file = Path(state_file).expanduser()
        self.state_file.parent.mkdir(parents=True, exist_ok=True)
        self.default_state_file = Path(DEFAULT_SESSION_STATE_FILE).expanduser()
        self.legacy_state_file = Path(LEGACY_TMP_SESSION_STATE_FILE)
        self.config = config or {}
        self.process_generation = uuid.uuid4().hex[:12]
        self._state_save_lock = threading.Lock()
        mq_timeouts = self.config.get("timeouts", {}).get("message_queue", {})
        self.input_delivery_wait_seconds = float(
            mq_timeouts.get("input_delivery_wait_seconds", 1.0)
        )

        self.tmux = TmuxController(log_dir=log_dir, config=self.config)
        self.sessions: dict[str, Session] = {}
        self._event_handlers: list[Callable[[NotificationEvent], Awaitable[None]]] = []
        self.codex_sessions: dict[str, CodexAppServerSession] = {}
        self.codex_turns_in_flight: set[str] = set()
        self.codex_active_turn_ids: dict[str, str] = {}
        self.codex_last_delta_at: dict[str, datetime] = {}
        self.codex_wait_states: dict[str, tuple[str, datetime]] = {}
        self._codex_item_started_at: dict[tuple[str, str], datetime] = {}
        self.codex_working_delta_window_seconds = float(
            self.config.get("codex_events", {}).get("working_delta_window_seconds", 2.5)
        )
        self.hook_output_store: Optional[dict] = None
        self.output_monitor = None  # Set by main app for activity projection (#288)

        # Telegram topic auto-sync
        self.orphaned_topics: list[tuple[int, int]] = []  # (chat_id, thread_id) from dead sessions
        telegram_config = self.config.get("telegram", {})
        self.default_forum_chat_id: Optional[int] = telegram_config.get("default_forum_chat_id")
        topic_registry_config = telegram_config.get("topic_registry", {})
        self.telegram_topic_registry_path = Path(
            topic_registry_config.get("path", "~/.local/share/claude-sessions/telegram_topics.json")
        ).expanduser()
        self.telegram_topic_registry_path.parent.mkdir(parents=True, exist_ok=True)
        self.telegram_topic_registry: dict[tuple[int, int], TelegramTopicRecord] = {}
        self._topic_creator: Optional[Callable[..., Awaitable[Optional[int]]]] = None
        self._pending_telegram_topic_tasks: set[asyncio.Task[Any]] = set()
        self._pending_telegram_topic_tasks_by_session: dict[str, asyncio.Task[Any]] = {}
        self._telegram_topic_ensure_locks: dict[str, asyncio.Lock] = {}
        self.adoption_proposals: dict[str, AdoptionProposal] = {}
        self.agent_registrations: dict[str, AgentRegistration] = {}
        self.agent_role_last_session_ids: dict[str, str] = {}
        self._maintainer_bootstrap_lock = asyncio.Lock()
        self._service_role_bootstrap_locks: dict[str, asyncio.Lock] = {}

        maintainer_config = self.config.get("maintainer_agent", {})
        raw_service_roles = self.config.get("service_roles", {})
        self._maintainer_service_role_explicit = isinstance(raw_service_roles, dict) and "maintainer" in raw_service_roles
        configured_working_dir = maintainer_config.get("working_dir")
        default_working_dir = str(Path(__file__).resolve().parents[1])
        self.maintainer_working_dir = str(configured_working_dir or default_working_dir)
        self.maintainer_friendly_name = str(
            maintainer_config.get("friendly_name", "sm-maintainer")
        ).strip() or "sm-maintainer"
        preferred_providers = maintainer_config.get("preferred_providers", ["codex-fork", "codex", "claude"])
        if isinstance(preferred_providers, list):
            normalized_providers = [str(provider).strip() for provider in preferred_providers if str(provider).strip()]
        else:
            normalized_providers = ["codex-fork", "codex", "claude"]
        self.maintainer_preferred_providers = normalized_providers or ["codex-fork", "codex", "claude"]
        raw_bootstrap_prompt = maintainer_config.get(
            "bootstrap_prompt",
            DEFAULT_MAINTAINER_BOOTSTRAP_PROMPT,
        )
        self.maintainer_bootstrap_prompt_template = str(raw_bootstrap_prompt).strip() or DEFAULT_MAINTAINER_BOOTSTRAP_PROMPT
        self.maintainer_bootstrap_prompt_file = str(
            maintainer_config.get("bootstrap_prompt_file", maintainer_config.get("boot_prompt_file", ""))
        ).strip()
        raw_maintainer_task_complete_ttl = maintainer_config.get("task_complete_ttl_seconds")
        self.maintainer_task_complete_ttl_seconds = DEFAULT_MAINTAINER_TASK_COMPLETE_TTL_SECONDS
        if raw_maintainer_task_complete_ttl not in (None, ""):
            try:
                normalized_maintainer_ttl = int(raw_maintainer_task_complete_ttl)
            except (TypeError, ValueError):
                normalized_maintainer_ttl = DEFAULT_MAINTAINER_TASK_COMPLETE_TTL_SECONDS
            if normalized_maintainer_ttl > 0:
                self.maintainer_task_complete_ttl_seconds = normalized_maintainer_ttl
        self.service_role_bootstrap_specs = self._build_service_role_bootstrap_specs(
            default_working_dir=default_working_dir,
            maintainer_config=maintainer_config,
        )
        self._maintainer_bootstrap_lock = self._service_role_bootstrap_locks.setdefault(
            "maintainer",
            asyncio.Lock(),
        )
        service_role_maintenance_config = self.config.get("service_role_maintenance", {})
        self.service_role_maintenance_poll_interval_seconds = float(
            service_role_maintenance_config.get("poll_interval_seconds", 60.0)
        )
        self._service_role_maintenance_task: Optional[asyncio.Task[Any]] = None

        codex_config = self.config.get("codex", {})
        codex_app_config = self.config.get("codex_app_server", codex_config)
        codex_rollout = self.config.get("codex_rollout", {})
        self.codex_rollout_flags = {
            "enable_durable_events": _coerce_rollout_flag(
                codex_rollout.get("enable_durable_events"), default=True
            ),
            "enable_structured_requests": _coerce_rollout_flag(
                codex_rollout.get("enable_structured_requests"), default=True
            ),
            "enable_observability_projection": _coerce_rollout_flag(
                codex_rollout.get("enable_observability_projection"), default=True
            ),
            "enable_codex_tui": _coerce_rollout_flag(
                codex_rollout.get("enable_codex_tui"), default=True
            ),
        }
        self.codex_provider_mapping_phase = normalize_provider_mapping_phase(
            codex_rollout.get("provider_mapping_phase")
        )

        self.codex_cli_command = codex_config.get("command", "codex")
        self.codex_cli_args = codex_config.get("args", [])
        self.codex_default_model = codex_config.get("default_model")
        self.codex_session_index_path = Path(
            codex_config.get("session_index_path", "~/.codex/session_index.jsonl")
        ).expanduser()
        codex_fork_config = self.config.get("codex_fork", codex_config)
        self.codex_fork_command = codex_fork_config.get("command", self.codex_cli_command)
        self.codex_fork_args = self._with_codex_fork_managed_args(
            codex_fork_config.get("args", self.codex_cli_args)
        )
        self.codex_fork_default_model = codex_fork_config.get("default_model", self.codex_default_model)
        self.codex_fork_event_schema_version = int(codex_fork_config.get("event_schema_version", 2))
        raw_artifact_ref = codex_fork_config.get("artifact_ref", "local-unpinned")
        artifact_ref = str(raw_artifact_ref).strip() if raw_artifact_ref is not None else ""
        if not artifact_ref or artifact_ref.lower() in {"none", "null"}:
            artifact_ref = "local-unpinned"
        self.codex_fork_artifact_ref = artifact_ref
        self.codex_fork_artifact_release = str(codex_fork_config.get("artifact_release", "local"))
        artifact_platforms = codex_fork_config.get(
            "artifact_platforms",
            ["darwin-arm64", "darwin-x86_64", "linux-x86_64"],
        )
        if isinstance(artifact_platforms, list):
            self.codex_fork_artifact_platforms = [str(item) for item in artifact_platforms if str(item).strip()]
        else:
            self.codex_fork_artifact_platforms = ["darwin-arm64", "darwin-x86_64", "linux-x86_64"]
        self.codex_fork_rollback_provider = str(codex_fork_config.get("rollback_provider", "codex"))
        self.codex_fork_rollback_command = str(codex_fork_config.get("rollback_command", "sm codex-legacy"))
        self.codex_fork_event_poll_interval_seconds = float(
            codex_fork_config.get("event_poll_interval_seconds", 0.5)
        )
        self.codex_fork_control_timeout_seconds = float(
            codex_fork_config.get("control_timeout_seconds", 5.0)
        )
        self.codex_fork_control_tmux_fallback_enabled = _coerce_rollout_flag(
            codex_fork_config.get("control_tmux_fallback_enabled"),
            default=True,
        )
        self.codex_fork_tool_input_max_chars = max(
            200, int(codex_fork_config.get("tool_input_max_chars", 2000))
        )
        self.codex_fork_output_preview_max_chars = max(
            100, int(codex_fork_config.get("tool_output_preview_max_chars", 1200))
        )
        self.codex_fork_tool_payload_max_items = max(
            10, int(codex_fork_config.get("tool_payload_max_items", 100))
        )
        self.codex_fork_event_monitors: dict[str, asyncio.Task] = {}
        self.codex_fork_event_offsets: dict[str, int] = {}
        self.codex_fork_event_buffers: dict[str, str] = {}
        self.codex_fork_lifecycle: dict[str, dict[str, Any]] = {}
        self.codex_fork_turns_in_flight: set[str] = set()
        self.codex_fork_wait_resume_state: dict[str, str] = {}
        self.codex_fork_wait_kind: dict[str, str] = {}
        self.codex_fork_last_seq: dict[str, int] = {}
        self.codex_fork_session_epoch: dict[str, Any] = {}
        self.codex_fork_control_epoch: dict[str, str] = {}
        self.codex_fork_control_degraded: dict[str, str] = {}
        self.codex_fork_runtime_owner: dict[str, str] = {}
        codex_fork_runtime_maintenance_config = self.config.get("codex_fork_runtime_maintenance", {})
        self.codex_fork_runtime_maintenance_poll_interval_seconds = float(
            codex_fork_runtime_maintenance_config.get("poll_interval_seconds", 300.0)
        )
        self._codex_fork_runtime_maintenance_task: Optional[asyncio.Task[Any]] = None

        # App-server config (can be overridden by codex_app_server section)
        self.codex_config = CodexAppServerConfig(
            command=codex_app_config.get("command", self.codex_cli_command),
            args=codex_app_config.get("app_server_args", codex_app_config.get("args", [])),
            default_model=codex_app_config.get("default_model", self.codex_default_model),
            approval_policy=codex_app_config.get("approval_policy", "never"),
            sandbox=codex_app_config.get("sandbox", "workspace-write"),
            approval_decision=codex_app_config.get("approval_decision", "decline"),
            request_timeout_seconds=codex_app_config.get("request_timeout_seconds", 60),
            client_name=codex_app_config.get("client_name", "session-manager"),
            client_title=codex_app_config.get("client_title", "Claude Session Manager"),
            client_version=codex_app_config.get("client_version", "0.1.0"),
        )

        codex_events_config = self.config.get("codex_events", {})
        default_events_db = str(self.state_file.with_name("codex_events.db"))
        self.codex_event_store = CodexEventStore(
            db_path=codex_events_config.get("db_path", default_events_db),
            ring_size=codex_events_config.get("ring_size", 1000),
            retention_max_events_per_session=codex_events_config.get("retention_max_events_per_session", 5000),
            retention_max_age_days=codex_events_config.get("retention_max_age_days", 14),
            prune_every_writes=codex_events_config.get("prune_every_writes", 200),
            payload_preview_chars=codex_events_config.get("payload_preview_chars", 1500),
        )

        default_requests_db = str(self.state_file.with_name("codex_requests.db"))
        self.codex_request_ledger = CodexRequestLedger(
            db_path=self.config.get("codex_requests", {}).get("db_path", default_requests_db),
            process_generation=self.process_generation,
        )
        codex_observability_config = self.config.get("codex_observability", {})
        retention_max_age_days = codex_observability_config.get("retention_max_age_days")
        if retention_max_age_days is None:
            retention_max_age_days = 14
        retention_codex_fork_max_age_days = codex_observability_config.get(
            "retention_codex_fork_max_age_days"
        )
        if retention_codex_fork_max_age_days is None:
            if "retention_max_age_days" in codex_observability_config:
                retention_codex_fork_max_age_days = retention_max_age_days
            else:
                retention_codex_fork_max_age_days = 30
        default_observability_db = str(self.state_file.with_name("codex_observability.db"))
        self.codex_observability_logger = CodexObservabilityLogger(
            db_path=codex_observability_config.get("db_path", default_observability_db),
            retention_max_age_days=retention_max_age_days,
            retention_codex_fork_max_age_days=retention_codex_fork_max_age_days,
            retention_tool_events_per_session=codex_observability_config.get(
                "retention_tool_events_per_session", 20000
            ),
            retention_turn_events_per_session=codex_observability_config.get(
                "retention_turn_events_per_session", 5000
            ),
            payload_max_chars=codex_observability_config.get("payload_max_chars", 4000),
            prune_interval_seconds=codex_observability_config.get("prune_interval_seconds", 3600),
        )
        self.codex_activity_projection = CodexActivityProjection(self.codex_observability_logger)

        # Message queue manager (set by main app)
        self.message_queue_manager = None
        queue_runner_config = dict(self.config)
        if "queue_runner" not in queue_runner_config and self.state_file != self.default_state_file:
            queue_runner_config["queue_runner"] = {
                "state_dir": str(self.state_file.parent / "queue-runner")
            }
        self.queue_runner = QueueRunner(self, config=queue_runner_config)

        # Child monitor (set by main app)
        self.child_monitor = None

        # EM topic continuity (Fix B: sm#271): persisted across handoffs
        # Format: {"chat_id": int, "thread_id": int} or None
        self.em_topic: Optional[dict] = None
        self.maintainer_session_id: Optional[str] = None

        self._load_telegram_topic_registry()
        self._migrate_legacy_state_file_if_needed()

        # Load existing sessions from state file
        self._load_state()
        if self.sync_codex_native_titles_from_index(persist=False):
            self._save_state()

    def _tmux_socket_name(self) -> Optional[str]:
        """Return configured tmux socket name, treating partial mocks as legacy default-server."""
        socket_name = getattr(self.tmux, "socket_name", None)
        return socket_name if isinstance(socket_name, str) and socket_name else None

    def _tmux_history_limit(self) -> Optional[int]:
        """Return configured tmux history limit when available."""
        history_limit = getattr(self.tmux, "history_limit", None)
        return history_limit if isinstance(history_limit, int) else None

    def _tmux_session_history_limit(self, tmux_session: str) -> Optional[int]:
        """Return a session's current tmux history limit when the controller can provide it."""
        getter = getattr(self.tmux, "get_history_limit", None)
        if not callable(getter):
            return None
        try:
            history_limit = getter(tmux_session)
        except Exception:
            return None
        return history_limit if isinstance(history_limit, int) else None

    def _migrate_legacy_state_file_if_needed(self) -> None:
        """Copy a pre-durable temp-backed session registry into the configured path once."""
        if self.state_file != self.default_state_file:
            return
        if self.state_file == self.legacy_state_file:
            return
        if self.state_file.exists() or not self.legacy_state_file.exists():
            return

        try:
            self.state_file.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(self.legacy_state_file, self.state_file)
            logger.info(
                "Migrated session state from legacy path %s to %s",
                self.legacy_state_file,
                self.state_file,
            )
        except Exception as exc:
            logger.warning(
                "Failed to migrate legacy state file from %s to %s: %s",
                self.legacy_state_file,
                self.state_file,
                exc,
            )

    def _load_telegram_topic_registry(self) -> bool:
        """Load the durable Telegram topic registry from disk."""
        if not self.telegram_topic_registry_path.exists():
            return True

        try:
            with open(self.telegram_topic_registry_path) as f:
                data = json.load(f)

            self.telegram_topic_registry = {}
            for item in data.get("topics", []):
                record = TelegramTopicRecord.from_dict(item)
                self.telegram_topic_registry[(record.chat_id, record.thread_id)] = record
            return True
        except Exception as e:
            logger.error(
                "CRITICAL: Failed to load Telegram topic registry from %s: %s",
                self.telegram_topic_registry_path,
                e,
            )
            return False

    def _save_telegram_topic_registry(self) -> bool:
        """Persist the durable Telegram topic registry using atomic file replacement."""
        try:
            data = {
                "topics": [
                    record.to_dict()
                    for _, record in sorted(
                        self.telegram_topic_registry.items(),
                        key=lambda item: (item[0][0], item[0][1]),
                    )
                ]
            }
            path = self.telegram_topic_registry_path
            temp_file = path.with_suffix(".tmp")

            with open(temp_file, "w") as f:
                json.dump(data, f, indent=2)

            temp_file.rename(path)
            return True
        except Exception as e:
            logger.error(
                "CRITICAL: Failed to save Telegram topic registry to %s: %s",
                self.telegram_topic_registry_path,
                e,
            )
            try:
                temp_file = self.telegram_topic_registry_path.with_suffix(".tmp")
                if temp_file.exists():
                    temp_file.unlink()
            except Exception:
                pass
            return False

    def _upsert_telegram_topic_record(
        self,
        session: Session,
        chat_id: int,
        thread_id: int,
        *,
        persist: bool = True,
        revive_deleted: bool = False,
    ) -> None:
        """Create or refresh a durable Telegram topic registry entry."""
        if not chat_id or not thread_id:
            return

        key = (chat_id, thread_id)
        existing = self.telegram_topic_registry.get(key)
        if existing is not None and existing.deleted_at is not None and not revive_deleted:
            return
        now = datetime.now()
        created_at = existing.created_at if existing else session.created_at
        record = TelegramTopicRecord(
            session_id=session.id,
            chat_id=chat_id,
            thread_id=thread_id,
            tmux_session=session.tmux_session or (existing.tmux_session if existing else None),
            provider=session.provider,
            created_at=created_at,
            last_seen_at=now,
            deleted_at=None,
            is_em_topic=session.is_em or (existing.is_em_topic if existing else False),
        )
        changed = existing != record
        self.telegram_topic_registry[key] = record
        if changed and persist:
            self._save_telegram_topic_registry()

    def mark_telegram_topic_deleted(
        self,
        chat_id: int,
        thread_id: int,
        *,
        session: Optional[Session] = None,
        persist: bool = True,
    ) -> None:
        """Mark a durable topic registry entry as deleted."""
        key = (chat_id, thread_id)
        record = self.telegram_topic_registry.get(key)
        if record is None and session is not None:
            self._upsert_telegram_topic_record(session, chat_id, thread_id, persist=False)
            record = self.telegram_topic_registry.get(key)
        if record is None or record.deleted_at is not None:
            return

        record.deleted_at = datetime.now()
        if session is not None:
            record.session_id = session.id
            record.tmux_session = session.tmux_session
            record.provider = session.provider
            record.is_em_topic = record.is_em_topic or session.is_em
        record.last_seen_at = datetime.now()
        if persist:
            self._save_telegram_topic_registry()

    def get_active_telegram_topic_record(
        self,
        session_id: str,
        chat_id: Optional[int] = None,
    ) -> Optional[TelegramTopicRecord]:
        """Return the most recently seen active topic record for a session."""
        matches = [
            record
            for record in self.telegram_topic_registry.values()
            if record.session_id == session_id
            and record.deleted_at is None
            and (chat_id is None or record.chat_id == chat_id)
        ]
        if not matches:
            return None
        return max(matches, key=lambda record: record.last_seen_at)

    def list_adoption_proposals(
        self,
        *,
        target_session_id: Optional[str] = None,
        status: Optional[AdoptionProposalStatus] = None,
    ) -> list[AdoptionProposal]:
        """Return adoption proposals filtered by target and/or status."""
        proposals = list(self.adoption_proposals.values())
        if target_session_id is not None:
            proposals = [proposal for proposal in proposals if proposal.target_session_id == target_session_id]
        if status is not None:
            proposals = [proposal for proposal in proposals if proposal.status == status]
        return sorted(proposals, key=lambda proposal: (proposal.created_at, proposal.id))

    def create_adoption_proposal(
        self,
        proposer_session_id: str,
        target_session_id: str,
    ) -> AdoptionProposal:
        """Create or return a pending adoption proposal for an EM session."""
        proposer = self.sessions.get(proposer_session_id)
        if proposer is None:
            raise ValueError(f"Proposer session {proposer_session_id} not found")
        if not proposer.is_em:
            raise ValueError("Only EM sessions may propose adoption")
        if proposer.status == SessionStatus.STOPPED:
            raise ValueError("Stopped sessions cannot propose adoption")

        target = self.sessions.get(target_session_id)
        if target is None:
            raise ValueError(f"Target session {target_session_id} not found")
        if target.status == SessionStatus.STOPPED:
            raise ValueError("Stopped sessions cannot be adopted")
        if proposer_session_id == target_session_id:
            raise ValueError("A session cannot adopt itself")
        if target.parent_session_id == proposer_session_id:
            raise ValueError(f"Session {target_session_id} is already managed by {proposer_session_id}")

        pending = self.list_adoption_proposals(
            target_session_id=target_session_id,
            status=AdoptionProposalStatus.PENDING,
        )
        if pending:
            existing = pending[0]
            if existing.proposer_session_id == proposer_session_id:
                return existing
            raise ValueError(
                f"Session {target_session_id} already has a pending adoption proposal "
                f"from {existing.proposer_session_id}"
            )

        proposal = AdoptionProposal(
            proposer_session_id=proposer_session_id,
            target_session_id=target_session_id,
        )
        self.adoption_proposals[proposal.id] = proposal
        self._save_state()
        return proposal

    def decide_adoption_proposal(
        self,
        proposal_id: str,
        *,
        accepted: bool,
    ) -> AdoptionProposal:
        """Accept or reject a pending adoption proposal."""
        proposal = self.adoption_proposals.get(proposal_id)
        if proposal is None:
            raise ValueError(f"Adoption proposal {proposal_id} not found")
        if proposal.status != AdoptionProposalStatus.PENDING:
            raise ValueError(f"Adoption proposal {proposal_id} is already {proposal.status.value}")

        target = self.sessions.get(proposal.target_session_id)
        proposer = self.sessions.get(proposal.proposer_session_id)

        if accepted:
            if proposer is None:
                raise ValueError(f"Proposer session {proposal.proposer_session_id} no longer exists")
            if not proposer.is_em:
                raise ValueError(f"Proposer session {proposal.proposer_session_id} is no longer an EM")
            if proposer.status == SessionStatus.STOPPED:
                raise ValueError(f"Proposer session {proposal.proposer_session_id} is stopped")
            if target is None:
                raise ValueError(f"Target session {proposal.target_session_id} no longer exists")
            if target.status == SessionStatus.STOPPED:
                raise ValueError(f"Target session {proposal.target_session_id} is stopped")
            target.parent_session_id = proposal.proposer_session_id

        proposal.status = (
            AdoptionProposalStatus.ACCEPTED if accepted else AdoptionProposalStatus.REJECTED
        )
        proposal.decided_at = datetime.now()

        if accepted:
            for other in self.adoption_proposals.values():
                if other.id == proposal.id:
                    continue
                if (
                    other.target_session_id == proposal.target_session_id
                    and other.status == AdoptionProposalStatus.PENDING
                ):
                    other.status = AdoptionProposalStatus.REJECTED
                    other.decided_at = proposal.decided_at

        self._save_state()
        return proposal

    def _load_state(self) -> bool:
        """
        Load session state from disk.

        Returns:
            True if state loaded successfully (or no state file exists),
            False if an error occurred during loading.
        """
        state_path = self.state_file
        if (
            not state_path.exists()
            and self.state_file == self.default_state_file
            and self.legacy_state_file.exists()
        ):
            state_path = self.legacy_state_file
            logger.warning(
                "Configured state file %s missing; falling back to legacy state path %s for startup load",
                self.state_file,
                self.legacy_state_file,
            )

        if state_path.exists():
            try:
                with open(state_path) as f:
                    data = json.load(f)
            except Exception as e:
                if (
                    state_path == self.state_file
                    and self.state_file == self.default_state_file
                    and self.legacy_state_file.exists()
                ):
                    logger.warning(
                        "Failed to read configured state file %s (%s); falling back to legacy state path %s",
                        self.state_file,
                        e,
                        self.legacy_state_file,
                    )
                    try:
                        with open(self.legacy_state_file) as f:
                            data = json.load(f)
                        state_path = self.legacy_state_file
                    except Exception as legacy_exc:
                        logger.error(f"CRITICAL: Failed to load state from {self.legacy_state_file}: {legacy_exc}")
                        logger.error(f"Session state may be lost! Please check {self.legacy_state_file}")
                        return False
                else:
                    logger.error(f"CRITICAL: Failed to load state from {state_path}: {e}")
                    logger.error(f"Session state may be lost! Please check {state_path}")
                    return False

            try:
                self._hydrate_state_from_data(data)
                return True
            except Exception as e:
                logger.error(f"CRITICAL: Failed to load state from {state_path}: {e}")
                logger.error(f"Session state may be lost! Please check {state_path}")
                return False
        return True  # No state file is not an error

    def _hydrate_state_from_data(self, data: dict) -> None:
        """Apply parsed state payload to the in-memory session manager."""
        legacy_codex_sessions: list[dict] = []
        cleaned_sessions: list[dict] = []
        retired_codex_app_sessions = False
        registry_backfilled = False
        for session_data in data.get("sessions", []):
            raw_provider = session_data.get("provider")
            raw_tmux_session = session_data.get("tmux_session")
            raw_log_file = session_data.get("log_file")
            raw_codex_thread_id = session_data.get("codex_thread_id")
            is_legacy_codex_app = (
                raw_provider == "codex"
                and (
                    raw_codex_thread_id is not None
                    or (not raw_tmux_session and not raw_log_file)
                )
            )
            if is_legacy_codex_app:
                legacy_codex_sessions.append(session_data)
                name = session_data.get("name") or session_data.get("id", "unknown")
                logger.warning(
                    f"Dropping legacy codex app session from state: {name}"
                )
                continue
            cleaned_sessions.append(session_data)
            session = Session.from_dict(session_data)
            if session.telegram_chat_id and session.telegram_thread_id:
                key = (session.telegram_chat_id, session.telegram_thread_id)
                if key not in self.telegram_topic_registry:
                    registry_backfilled = True
                self._upsert_telegram_topic_record(
                    session,
                    session.telegram_chat_id,
                    session.telegram_thread_id,
                    persist=False,
                )
            # Codex app-server sessions are restored without tmux
            if session.provider == "codex-app":
                if (
                    self.codex_provider_mapping_phase == "post_cutover"
                    and not (
                        session.status == SessionStatus.STOPPED
                        and session.error_message == CODEX_APP_RETIRED_SESSION_REASON
                    )
                ):
                    self._retire_codex_app_session_state(
                        session,
                        reason=CODEX_APP_RETIRED_SESSION_REASON,
                        cleanup_queue=False,
                    )
                    retired_codex_app_sessions = True
                self.sessions[session.id] = session
                logger.info(f"Restored codex app session: {session.name}")
                continue

            if session.status == SessionStatus.STOPPED:
                if session.provider == "codex-fork" and self._codex_fork_runtime_reachable(session):
                    session.status = SessionStatus.IDLE
                    session.completion_status = None
                    session.completion_message = None
                    logger.info(
                        "Healed stopped codex-fork session %s because detached runtime is still reachable",
                        session.name,
                    )
                    if session.telegram_chat_id and session.telegram_thread_id:
                        self._upsert_telegram_topic_record(
                            session,
                            session.telegram_chat_id,
                            session.telegram_thread_id,
                            persist=True,
                            revive_deleted=True,
                        )
                    self.sessions[session.id] = session
                    self.codex_fork_runtime_owner[session.id] = session.parent_session_id or session.id
                    continue
                self.sessions[session.id] = session
                logger.info(
                    "Restored stopped session record without live tmux runtime check: %s",
                    session.name,
                )
                continue

            # Verify tmux session still exists (Claude/Codex CLI)
            if self.tmux.session_exists(session.tmux_session):
                if session.telegram_chat_id and session.telegram_thread_id:
                    self._upsert_telegram_topic_record(
                        session,
                        session.telegram_chat_id,
                        session.telegram_thread_id,
                        persist=True,
                        revive_deleted=True,
                    )
                self.sessions[session.id] = session
                if session.provider == "codex-fork":
                    self.codex_fork_runtime_owner[session.id] = session.parent_session_id or session.id
                logger.info(f"Restored session: {session.name}")
            else:
                logger.warning(f"Session {session.name} no longer exists in tmux")
                # Collect orphaned Telegram forum topics for cleanup at startup.
                # Only collect if chat_id matches the known forum group —
                # in non-forum chats, telegram_thread_id is a reply message_id,
                # not a forum topic, so delete_forum_topic would fail.
                if (
                    session.telegram_chat_id
                    and session.telegram_thread_id
                    and self.default_forum_chat_id
                    and session.telegram_chat_id == self.default_forum_chat_id
                ):
                    self.orphaned_topics.append(
                        (session.telegram_chat_id, session.telegram_thread_id)
                    )
                    logger.info(
                        f"Collected orphaned topic: chat={session.telegram_chat_id}, "
                        f"thread={session.telegram_thread_id} from dead session {session.name}"
                    )
        if legacy_codex_sessions:
            preserved_state = {key: value for key, value in data.items() if key != "sessions"}
            self._rewrite_state_raw(cleaned_sessions, extra_state=preserved_state)
        if registry_backfilled:
            self._save_telegram_topic_registry()

        # Load EM topic continuity field (backward compat: missing = None)
        self.em_topic = data.get("em_topic")
        self.maintainer_session_id = data.get("maintainer_session_id")
        self.agent_registrations = {}
        raw_last_session_ids = data.get("agent_role_last_session_ids", {})
        self.agent_role_last_session_ids = (
            {
                self.normalize_agent_role(str(role)): str(session_id)
                for role, session_id in raw_last_session_ids.items()
                if self.normalize_agent_role(str(role)) and str(session_id).strip()
            }
            if isinstance(raw_last_session_ids, dict)
            else {}
        )
        for registration_data in data.get("agent_registrations", []):
            registration = AgentRegistration.from_dict(registration_data)
            self.agent_registrations[registration.role] = registration
            self.agent_role_last_session_ids[registration.role] = registration.session_id
        if self.maintainer_session_id and "maintainer" not in self.agent_registrations:
            self.agent_registrations["maintainer"] = AgentRegistration(
                role="maintainer",
                session_id=self.maintainer_session_id,
            )
            self.agent_role_last_session_ids["maintainer"] = self.maintainer_session_id
        self.adoption_proposals = {}
        for proposal_data in data.get("adoption_proposals", []):
            proposal = AdoptionProposal.from_dict(proposal_data)
            self.adoption_proposals[proposal.id] = proposal
        registry_changed = self._prune_agent_registrations(persist=False)
        if retired_codex_app_sessions or registry_changed:
            self._save_state()

    def _rewrite_state_raw(self, sessions_data: list[dict], extra_state: Optional[dict] = None) -> bool:
        """Rewrite state file with provided session data (used for one-time cleanup)."""
        try:
            data = {"sessions": sessions_data}
            if extra_state:
                data.update(extra_state)
            state_path = Path(self.state_file)
            temp_file = state_path.with_suffix(".tmp")

            with open(temp_file, "w") as f:
                json.dump(data, f, indent=2)

            temp_file.rename(state_path)
            logger.info("State file rewritten to drop legacy codex app sessions.")
            return True
        except Exception as e:
            logger.error(f"CRITICAL: Failed to rewrite state file {self.state_file}: {e}")
            return False

    def _build_state_snapshot(self) -> dict:
        return {
            "sessions": [s.to_dict() for s in list(self.sessions.values())],
            "em_topic": self.em_topic,
            "maintainer_session_id": self.maintainer_session_id,
            "agent_registrations": [
                registration.to_dict()
                for registration in sorted(
                    list(self.agent_registrations.values()),
                    key=lambda registration: (registration.role, registration.created_at),
                )
            ],
            "agent_role_last_session_ids": {
                role: self.agent_role_last_session_ids[role]
                for role in sorted(list(self.agent_role_last_session_ids))
                if self.agent_role_last_session_ids.get(role)
            },
            "adoption_proposals": [
                proposal.to_dict()
                for proposal in sorted(
                    list(self.adoption_proposals.values()),
                    key=lambda proposal: (proposal.created_at, proposal.id),
                )
            ],
        }

    def _write_state_snapshot(self, data: dict) -> bool:
        temp_file: Optional[Path] = None
        with self._state_save_lock:
            try:
                state_path = Path(self.state_file)
                temp_file = state_path.with_name(
                    f"{state_path.name}.tmp.{os.getpid()}.{threading.get_ident()}"
                )

                with open(temp_file, "w") as f:
                    json.dump(data, f, indent=2)

                # Atomic replace (POSIX guarantees atomicity).
                temp_file.replace(state_path)
                return True

            except Exception as e:
                logger.error(f"CRITICAL: Failed to save state to {self.state_file}: {e}")
                logger.error(f"Session state NOT persisted! Data may be lost on restart.")
                try:
                    if temp_file is not None and temp_file.exists():
                        temp_file.unlink()
                except Exception:
                    pass
                return False

    def _save_state(self) -> bool:
        """
        Save session state to disk using atomic file operations.

        Uses temp file + rename to ensure atomic writes and prevent race conditions
        when multiple async tasks call this method concurrently.

        Returns:
            True if state saved successfully, False if an error occurred.
        """
        return self._write_state_snapshot(self._build_state_snapshot())

    async def _save_state_async(self) -> bool:
        """Snapshot mutable manager state on the event loop, then write it off-loop."""
        data = self._build_state_snapshot()
        return await asyncio.to_thread(self._write_state_snapshot, data)

    def add_event_handler(self, handler: Callable[[NotificationEvent], Awaitable[None]]):
        """Register a handler for session events."""
        self._event_handlers.append(handler)

    async def _emit_event(self, event: NotificationEvent):
        """Emit an event to all registered handlers."""
        for handler in self._event_handlers:
            try:
                await handler(event)
            except Exception as e:
                logger.error(f"Event handler error: {e}")

    async def _get_git_remote_url_async(self, working_dir: str) -> Optional[str]:
        """
        Get the git remote URL for a working directory (async, non-blocking).

        Args:
            working_dir: Directory to check

        Returns:
            Git remote URL or None if not a git repo
        """
        try:
            working_path = Path(working_dir).expanduser().resolve()
            proc = await asyncio.create_subprocess_exec(
                "git", "config", "--get", "remote.origin.url",
                cwd=working_path,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=2)
            if proc.returncode == 0:
                return stdout.decode().strip()
            return None
        except Exception as e:
            logger.debug(f"Failed to get git remote for {working_dir}: {e}")
            return None

    def _get_git_remote_url(self, working_dir: str) -> Optional[str]:
        """
        Get the git remote URL for a working directory (sync wrapper).

        DEPRECATED: Use _get_git_remote_url_async() in async contexts.
        This sync version is kept for backward compatibility but should not be used.

        Args:
            working_dir: Directory to check

        Returns:
            Git remote URL or None if not a git repo
        """
        try:
            working_path = Path(working_dir).expanduser().resolve()
            result = subprocess.run(
                ["git", "config", "--get", "remote.origin.url"],
                cwd=working_path,
                capture_output=True,
                text=True,
                timeout=2,
            )
            if result.returncode == 0:
                return result.stdout.strip()
            return None
        except Exception as e:
            logger.debug(f"Failed to get git remote for {working_dir}: {e}")
            return None

    def _codex_fork_event_stream_path(self, session: Session) -> Path:
        """Return event-stream JSONL path for one codex-fork session."""
        return self.log_dir / f"{session.id}.codex-fork.events.jsonl"

    def _codex_fork_control_socket_path(self, session: Session) -> Path:
        """Return control-socket path for one codex-fork session."""
        return self.log_dir / f"{session.id}.codex-fork.control.sock"

    @staticmethod
    def _resolve_cli_command(
        command: str,
        *,
        working_dir: str,
    ) -> tuple[Optional[str], Optional[str]]:
        """Resolve one CLI command path the same way tmux launch preflight does."""
        raw_command = str(command or "").strip()
        if not raw_command:
            return None, "Launch command is empty"

        working_path = Path(working_dir).expanduser().resolve()
        if raw_command.startswith("~") or "/" in raw_command:
            candidate = Path(raw_command).expanduser()
            if not candidate.is_absolute():
                candidate = (working_path / candidate).resolve()
            else:
                candidate = candidate.resolve()
            if not candidate.exists():
                return None, f"Launch command does not exist: {candidate}"
            if not candidate.is_file():
                return None, f"Launch command is not a file: {candidate}"
            if not os.access(candidate, os.X_OK):
                return None, f"Launch command is not executable: {candidate}"
            return str(candidate), None

        if shutil.which(raw_command) is None:
            return None, f"Launch command not found on PATH: {raw_command}"
        return raw_command, None

    @staticmethod
    def _with_codex_fork_managed_args(args: list[str]) -> list[str]:
        """Return codex-fork args with SM-owned startup-update prompts disabled."""
        managed_args = [str(arg) for arg in (args or [])]
        if not any("check_for_update_on_startup=false" in arg.replace(" ", "") for arg in managed_args):
            managed_args.extend(CODEX_FORK_DISABLE_STARTUP_UPDATE_ARGS)
        return managed_args

    def _build_codex_fork_launch_spec(
        self,
        session: Session,
        *,
        resume_id: Optional[str] = None,
    ) -> tuple[str, list[str], str, Optional[str]]:
        """Return launch command/args, falling back to codex when codex-fork is unavailable."""
        _, fork_error = self._resolve_cli_command(
            self.codex_fork_command,
            working_dir=session.working_dir,
        )
        if fork_error:
            args = list(self.codex_cli_args)
            if resume_id:
                args = ["resume", resume_id, *args]
            return (
                self.codex_cli_command,
                args,
                "codex",
                f"Configured codex-fork runtime unavailable; falling back to codex: {fork_error}",
            )

        args = list(self.codex_fork_args)
        if resume_id:
            args = ["resume", resume_id, *args]
        event_stream_path = self._codex_fork_event_stream_path(session)
        control_socket_path = self._codex_fork_control_socket_path(session)
        event_stream_path.parent.mkdir(parents=True, exist_ok=True)
        if event_stream_path.exists():
            event_stream_path.unlink()
        if control_socket_path.exists():
            control_socket_path.unlink()
        args.extend(
            [
                "--event-stream",
                str(event_stream_path),
                "--event-schema-version",
                str(self.codex_fork_event_schema_version),
                "--control-socket",
                str(control_socket_path),
            ]
        )
        return self.codex_fork_command, args, "codex-fork", None

    def get_provider_create_rejection(
        self,
        provider: str,
        *,
        working_dir: str,
    ) -> Optional[str]:
        """Return a user-facing rejection reason when a fresh provider create cannot proceed."""
        if provider != "codex-fork":
            return None

        _, fork_error = self._resolve_cli_command(
            self.codex_fork_command,
            working_dir=working_dir,
        )
        if fork_error:
            return f"Configured codex-fork runtime unavailable: {fork_error}"
        return None

    @staticmethod
    def _codex_fork_session_id_from_artifact_name(name: str) -> Optional[str]:
        """Extract the owning session id from one codex-fork runtime artifact filename."""
        if name.endswith(".codex-fork.events.jsonl"):
            return name[: -len(".codex-fork.events.jsonl")] or None
        if name.endswith(".codex-fork.control.sock"):
            return name[: -len(".codex-fork.control.sock")] or None
        return None

    def _iter_codex_fork_runtime_artifacts(self) -> list[Path]:
        """List codex-fork runtime artifacts currently present in the log directory."""
        return sorted(
            [
                *self.log_dir.glob("*.codex-fork.events.jsonl"),
                *self.log_dir.glob("*.codex-fork.control.sock"),
            ]
        )

    @staticmethod
    def _normalize_codex_fork_event_type(event_type: Any) -> str:
        """Normalize codex-fork event types to a stable reducer vocabulary."""
        if not event_type:
            return ""
        raw = str(event_type).strip()
        if not raw:
            return ""
        snake = re.sub(r"(?<!^)(?=[A-Z])", "_", raw).replace("-", "_").lower()
        aliases = {
            "task_started": "turn_started",
            "task_complete": "turn_complete",
            "turn_completed": "turn_complete",
            "exec_approval_request": "approval_request",
            "patch_approval_request": "approval_request",
            "request_approval": "approval_request",
            "request_user_input": "user_input_request",
            "approval_decision": "approval_resolved",
            "approval_submitted": "approval_resolved",
            "user_input_submitted": "user_input_resolved",
            "user_input_response": "user_input_resolved",
            "runtime_error": "error",
            "fatal_error": "error",
        }
        return aliases.get(snake, snake)

    @staticmethod
    def _is_codex_native_title_provider(provider: str) -> bool:
        """Return True when provider-native thread titles are safe display identities."""
        return provider in {"codex", "codex-app", "codex-fork"}

    def _codex_fork_runtime_reachable(self, session: Session) -> bool:
        """Return True when a codex-fork detached runtime still answers on its control socket."""
        if getattr(session, "provider", "") != "codex-fork":
            return False
        socket_path = self._codex_fork_control_socket_path(session)
        if not socket_path.exists():
            return False
        if not session.tmux_session or not self.tmux.session_exists(session.tmux_session):
            return False

        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(min(0.25, max(0.05, self.codex_fork_control_timeout_seconds)))
        try:
            client.connect(str(socket_path))
            return True
        except OSError:
            return False
        finally:
            with contextlib.suppress(Exception):
                client.close()

    def _set_codex_fork_lifecycle_state(
        self,
        session_id: str,
        state: str,
        cause_event_type: str,
        seq: Optional[int] = None,
        session_epoch: Optional[Any] = None,
    ) -> dict[str, Any]:
        """Persist current lifecycle state and append transition-audit event."""
        previous = self.codex_fork_lifecycle.get(session_id, {})
        previous_state = previous.get("state")
        now = datetime.now()
        snapshot = {
            "state": state,
            "cause_event_type": cause_event_type,
            "seq": seq,
            "session_epoch": session_epoch,
            "updated_at": now.isoformat(),
        }
        self.codex_fork_lifecycle[session_id] = snapshot

        if previous_state != state:
            self.codex_event_store.append_event(
                session_id=session_id,
                event_type="lifecycle_transition",
                payload={
                    "from_state": previous_state,
                    "to_state": state,
                    "cause_event_type": cause_event_type,
                    "seq": seq,
                    "session_epoch": session_epoch,
                },
            )

        session = self.sessions.get(session_id)
        if session:
            status_before = session.status
            if state == "idle":
                session.status = SessionStatus.IDLE
            elif state in ("shutdown", "error"):
                session.status = SessionStatus.STOPPED
                session.stopped_at = now
            else:
                session.status = SessionStatus.RUNNING
                session.stopped_at = None
            session.last_activity = now

            if self.message_queue_manager:
                # Reassert active on every active event to repair stale idle flags
                # from recovery/interrupt paths. Idle marking stays transition-gated
                # so idle replays do not consume stop-notify state (issue #341).
                if state not in ("idle", "shutdown", "error"):
                    self.message_queue_manager.mark_session_active(session_id)
                elif previous_state != state:
                    self.message_queue_manager.mark_session_idle(
                        session_id,
                        completion_transition=True,
                    )

            if session.status != status_before:
                self._save_state()

        return snapshot

    def _reduce_codex_fork_lifecycle(
        self,
        session_id: str,
        event_type: str,
        payload: Optional[dict[str, Any]] = None,
        seq: Optional[int] = None,
        session_epoch: Optional[Any] = None,
        event_timestamp_ns: Optional[int] = None,
    ) -> Optional[dict[str, Any]]:
        """Apply one codex-fork lifecycle event to deterministic reducer state."""
        normalized = self._normalize_codex_fork_event_type(event_type)
        if not normalized:
            return None

        previous_epoch = self.codex_fork_session_epoch.get(session_id)
        if session_epoch is not None and previous_epoch is not None and session_epoch != previous_epoch:
            self.codex_fork_turns_in_flight.discard(session_id)
            self.codex_fork_wait_resume_state.pop(session_id, None)
            self.codex_fork_wait_kind.pop(session_id, None)
            self.codex_fork_last_seq.pop(session_id, None)
        if session_epoch is not None:
            self.codex_fork_session_epoch[session_id] = session_epoch

        if seq is not None:
            last_seq = self.codex_fork_last_seq.get(session_id)
            if last_seq is not None and seq <= last_seq:
                return self.codex_fork_lifecycle.get(session_id)
            self.codex_fork_last_seq[session_id] = seq

        if normalized == "thread_name_updated":
            session = self.sessions.get(session_id)
            if session:
                thread_id = self._normalize_codex_thread_id(payload.get("thread_id") if payload else None)
                if thread_id is None:
                    thread_id = self._normalize_codex_thread_id(payload.get("session_id") if payload else None)
                self._sync_codex_native_title(
                    session,
                    thread_name=(payload or {}).get("thread_name") or (payload or {}).get("name"),
                    updated_at_ns=event_timestamp_ns,
                    thread_id=thread_id,
                )
            return self.codex_fork_lifecycle.get(session_id)

        current_state = self.codex_fork_lifecycle.get(session_id, {}).get("state", "idle")
        next_state = current_state

        if normalized == "turn_started":
            self.codex_fork_turns_in_flight.add(session_id)
            next_state = "running"
        elif normalized == "turn_complete":
            self.codex_fork_turns_in_flight.discard(session_id)
            self.codex_fork_wait_resume_state.pop(session_id, None)
            self.codex_fork_wait_kind.pop(session_id, None)
            next_state = "idle"
        elif normalized == "turn_aborted":
            reason = str(payload.get("reason", "")).strip().lower() if payload else ""
            if reason == "interrupted":
                # Codex-fork emits interrupted aborts during prompt injection / runtime restarts.
                # They are not task completion and must not fire stop-notify side effects (#393).
                if current_state in {"waiting_on_approval", "waiting_on_user_input"}:
                    next_state = current_state
                elif session_id not in self.codex_fork_turns_in_flight:
                    next_state = current_state
                else:
                    next_state = "running"
            else:
                self.codex_fork_turns_in_flight.discard(session_id)
                self.codex_fork_wait_resume_state.pop(session_id, None)
                self.codex_fork_wait_kind.pop(session_id, None)
                next_state = "idle"
        elif normalized == "approval_request":
            resume_state = "running" if session_id in self.codex_fork_turns_in_flight else "idle"
            self.codex_fork_wait_resume_state[session_id] = resume_state
            self.codex_fork_wait_kind[session_id] = "approval"
            next_state = "waiting_on_approval"
        elif normalized == "user_input_request":
            resume_state = "running" if session_id in self.codex_fork_turns_in_flight else "idle"
            self.codex_fork_wait_resume_state[session_id] = resume_state
            self.codex_fork_wait_kind[session_id] = "user_input"
            next_state = "waiting_on_user_input"
        elif normalized in {"approval_resolved", "user_input_resolved"}:
            self.codex_fork_wait_resume_state.pop(session_id, None)
            self.codex_fork_wait_kind.pop(session_id, None)
            next_state = "running" if session_id in self.codex_fork_turns_in_flight else "idle"
        elif normalized == "stream_error":
            # Reconnect churn is noisy but non-terminal; preserve blocked wait states.
            if current_state in {"waiting_on_approval", "waiting_on_user_input"}:
                next_state = current_state
            elif session_id in self.codex_fork_turns_in_flight:
                next_state = "running"
        elif normalized == "turn_delta":
            if session_id in self.codex_fork_turns_in_flight:
                next_state = "running"
        elif current_state not in {"waiting_on_approval", "waiting_on_user_input"} and (
            normalized == "turn_diff"
            or normalized in {"item_started", "item_completed", "agent_message", "exec_command_end"}
            or normalized.endswith("_begin")
            or normalized.endswith("_delta")
        ):
            # After restart we may resume monitoring mid-turn without re-seeing the original
            # turn_started event. Fresh deltas/tool events must still reassert active work.
            next_state = "running"
        elif normalized == "shutdown_complete":
            # Detached runtime shutdown is emitted after normal completions and interrupted
            # turn restarts. It is transport churn, not a terminal SM session stop (#393).
            next_state = current_state
        elif normalized == "error":
            self.codex_fork_turns_in_flight.discard(session_id)
            self.codex_fork_wait_resume_state.pop(session_id, None)
            self.codex_fork_wait_kind.pop(session_id, None)
            next_state = "error"

        return self._set_codex_fork_lifecycle_state(
            session_id=session_id,
            state=next_state,
            cause_event_type=normalized,
            seq=seq,
            session_epoch=session_epoch,
        )

    def _sanitize_codex_fork_text(self, value: Any, max_chars: int) -> Any:
        text = str(value)
        if any((ord(ch) < 32 and ch not in "\n\r\t") or ord(ch) == 127 for ch in text):
            return {
                "redacted": True,
                "reason": "binary_payload_omitted",
                "original_chars": len(text),
            }
        text = BEARER_TOKEN_PATTERN.sub("Bearer [REDACTED]", text)
        text = INLINE_SECRET_PATTERN.sub(lambda match: f"{match.group(1)}=[REDACTED]", text)
        if len(text) <= max_chars:
            return text
        return {
            "truncated": True,
            "preview": text[:max_chars],
            "original_chars": len(text),
        }

    def _sanitize_codex_fork_value(self, value: Any, *, max_chars: int, depth: int = 0) -> Any:
        if depth >= 8:
            return {"truncated": True, "reason": "max_depth"}
        if isinstance(value, dict):
            items = list(value.items())
            sanitized: dict[str, Any] = {}
            for key, nested_value in items[: self.codex_fork_tool_payload_max_items]:
                key_str = str(key)
                if SECRET_FIELD_PATTERN.search(key_str):
                    sanitized[key_str] = "[REDACTED]"
                else:
                    sanitized[key_str] = self._sanitize_codex_fork_value(
                        nested_value, max_chars=max_chars, depth=depth + 1
                    )
            if len(items) > self.codex_fork_tool_payload_max_items:
                sanitized["_truncated_items"] = len(items) - self.codex_fork_tool_payload_max_items
            return sanitized
        if isinstance(value, (list, tuple)):
            bounded = [
                self._sanitize_codex_fork_value(item, max_chars=max_chars, depth=depth + 1)
                for item in list(value)[: self.codex_fork_tool_payload_max_items]
            ]
            if len(value) > self.codex_fork_tool_payload_max_items:
                bounded.append({"truncated_items": len(value) - self.codex_fork_tool_payload_max_items})
            return bounded
        if isinstance(value, bytes):
            return {
                "redacted": True,
                "reason": "binary_payload_omitted",
                "original_bytes": len(value),
            }
        if isinstance(value, str):
            return self._sanitize_codex_fork_text(value, max_chars=max_chars)
        if value is None or isinstance(value, (bool, int, float)):
            return value
        return self._sanitize_codex_fork_text(value, max_chars=max_chars)

    def _sanitize_codex_fork_tool_payload(self, payload: dict[str, Any]) -> dict[str, Any]:
        sanitized: dict[str, Any] = {}
        for key in ("turn_id", "call_id", "tool_name", "tool_kind", "failure_behavior"):
            if key in payload:
                sanitized[key] = self._sanitize_codex_fork_text(
                    payload.get(key), max_chars=200
                )

        for key in ("executed", "success", "mutating"):
            if key in payload:
                value = payload.get(key)
                if isinstance(value, bool):
                    sanitized[key] = value
                elif value is None:
                    sanitized[key] = None
                else:
                    sanitized[key] = bool(value)

        if "duration_ms" in payload:
            try:
                sanitized["duration_ms"] = int(payload.get("duration_ms"))
            except (TypeError, ValueError):
                sanitized["duration_ms"] = None

        for key in ("sandbox", "sandbox_metadata", "hook_error"):
            if key in payload:
                sanitized[key] = self._sanitize_codex_fork_value(
                    payload.get(key), max_chars=self.codex_fork_output_preview_max_chars
                )

        if "tool_input" in payload:
            sanitized["tool_input"] = self._sanitize_codex_fork_tool_input_value(
                payload.get("tool_input")
            )
        if "output_preview" in payload:
            sanitized["output_preview"] = self._sanitize_codex_fork_text(
                payload.get("output_preview"),
                max_chars=self.codex_fork_output_preview_max_chars,
            )
        if "error_message" in payload:
            sanitized["error_message"] = self._sanitize_codex_fork_text(
                payload.get("error_message"),
                max_chars=self.codex_fork_output_preview_max_chars,
            )
        return sanitized

    def _sanitize_codex_fork_tool_input_value(self, value: Any) -> Any:
        """Sanitize tool input, parsing JSON-encoded argument strings when possible."""
        if isinstance(value, str):
            stripped = value.strip()
            if stripped.startswith("{") or stripped.startswith("["):
                try:
                    value = json.loads(stripped)
                except json.JSONDecodeError:
                    pass
        return self._sanitize_codex_fork_value(
            value, max_chars=self.codex_fork_tool_input_max_chars
        )

    @staticmethod
    def _parse_codex_fork_timestamp(value: Any) -> Optional[datetime]:
        if isinstance(value, datetime):
            if value.tzinfo is None:
                return value.replace(tzinfo=timezone.utc)
            return value.astimezone(timezone.utc)
        if not isinstance(value, str):
            return None
        raw = value.strip()
        if not raw:
            return None
        if raw.endswith("Z"):
            raw = raw[:-1] + "+00:00"
        try:
            parsed = datetime.fromisoformat(raw)
        except ValueError:
            return None
        if parsed.tzinfo is None:
            return parsed.replace(tzinfo=timezone.utc)
        return parsed.astimezone(timezone.utc)

    @staticmethod
    def _datetime_to_epoch_ns(value: Optional[datetime]) -> Optional[int]:
        """Normalize one datetime to epoch nanoseconds."""
        if value is None:
            return None
        if value.tzinfo is None:
            value = value.replace(tzinfo=timezone.utc)
        return int(value.astimezone(timezone.utc).timestamp() * 1_000_000_000)

    @classmethod
    def _timestamp_to_epoch_ns(cls, value: Any) -> Optional[int]:
        """Parse provider timestamps into epoch nanoseconds, preserving RFC3339 ns precision."""
        if isinstance(value, datetime):
            return cls._datetime_to_epoch_ns(value)
        if not isinstance(value, str):
            return None
        raw = value.strip()
        if not raw:
            return None

        match = re.fullmatch(
            r"(?P<base>\d{4}-\d{2}-\d{2}[T ][0-9:.]+?)(?:\.(?P<fraction>\d{1,9}))?(?P<offset>Z|[+-]\d{2}:\d{2})?",
            raw,
        )
        if match and match.group("fraction"):
            base = match.group("base")
            offset = match.group("offset") or "+00:00"
            if offset == "Z":
                offset = "+00:00"
            try:
                parsed_base = datetime.fromisoformat(f"{base}{offset}")
            except ValueError:
                return cls._datetime_to_epoch_ns(cls._parse_codex_fork_timestamp(raw))
            if parsed_base.tzinfo is None:
                parsed_base = parsed_base.replace(tzinfo=timezone.utc)
            fraction_ns = int(match.group("fraction").ljust(9, "0")[:9])
            base_seconds = int(parsed_base.astimezone(timezone.utc).timestamp())
            return (base_seconds * 1_000_000_000) + fraction_ns

        return cls._datetime_to_epoch_ns(cls._parse_codex_fork_timestamp(raw))

    @staticmethod
    def _normalize_provider_native_title(value: Any) -> Optional[str]:
        """Normalize provider-native title text for display/cache use."""
        if not isinstance(value, str):
            return None
        title = re.sub(r"[\r\n\t]+", " ", value).strip()
        return title or None

    @staticmethod
    def _normalize_codex_thread_id(value: Any) -> Optional[str]:
        """Normalize Codex thread IDs while rejecting bridge sentinel values."""
        if not isinstance(value, str):
            return None
        thread_id = value.strip()
        if not thread_id or thread_id.lower() in {"unknown", "none", "null"}:
            return None
        return thread_id

    def _read_codex_session_index(self) -> dict[str, tuple[str, Optional[int]]]:
        """Read Codex's local thread index keyed by provider resume/thread id."""
        index_path = self.codex_session_index_path
        if not index_path.exists():
            return {}

        indexed: dict[str, tuple[str, Optional[int]]] = {}
        try:
            with index_path.open("r", encoding="utf-8", errors="ignore") as handle:
                for line in handle:
                    try:
                        record = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if not isinstance(record, dict):
                        continue
                    thread_id = str(record.get("id") or "").strip()
                    thread_name = self._normalize_provider_native_title(record.get("thread_name"))
                    if not thread_id or not thread_name:
                        continue
                    updated_ns = self._timestamp_to_epoch_ns(record.get("updated_at"))
                    previous = indexed.get(thread_id)
                    previous_ns = previous[1] if previous else None
                    if previous is None or (updated_ns or 0) >= (previous_ns or 0):
                        indexed[thread_id] = (thread_name, updated_ns)
        except OSError as exc:
            logger.debug("Failed reading Codex session index %s: %s", index_path, exc)
        return indexed

    def _sync_codex_native_title(
        self,
        session: Session,
        *,
        thread_name: Any,
        updated_at_ns: Optional[int] = None,
        thread_id: Any = None,
        persist: bool = True,
    ) -> bool:
        """Cache one Codex provider-native thread title on the Session record."""
        if not self._is_codex_native_title_provider(session.provider):
            return False

        native_title = self._normalize_provider_native_title(thread_name)
        if not native_title:
            return False

        incoming_updated_at_ns = updated_at_ns
        current_updated_at_ns = int(session.native_title_updated_at_ns or 0)
        if incoming_updated_at_ns is not None and current_updated_at_ns and incoming_updated_at_ns < current_updated_at_ns:
            return False

        changed = False
        normalized_thread_id = self._normalize_codex_thread_id(thread_id)
        if normalized_thread_id:
            if session.provider == "codex-app":
                if session.codex_thread_id != normalized_thread_id:
                    session.codex_thread_id = normalized_thread_id
                    changed = True
            elif session.provider_resume_id != normalized_thread_id:
                session.provider_resume_id = normalized_thread_id
                changed = True

        if session.native_title != native_title:
            session.native_title = native_title
            changed = True
        if incoming_updated_at_ns is not None:
            if session.native_title_updated_at_ns != incoming_updated_at_ns:
                session.native_title_updated_at_ns = incoming_updated_at_ns
                changed = True
        elif session.native_title_updated_at_ns is None:
            session.native_title_updated_at_ns = 0
            changed = True
        if session.native_title_source_mtime_ns is not None:
            session.native_title_source_mtime_ns = None
            changed = True

        if changed and persist:
            self._save_state()
        return changed

    def sync_codex_native_titles_from_index(self, *, persist: bool = True) -> bool:
        """Backfill Codex provider-native titles from Codex's local session index."""
        indexed_titles = self._read_codex_session_index()
        if not indexed_titles:
            return False

        changed = False
        for session in self.sessions.values():
            if not self._is_codex_native_title_provider(session.provider):
                continue
            thread_id = session.codex_thread_id if session.provider == "codex-app" else session.provider_resume_id
            if not thread_id:
                continue
            indexed = indexed_titles.get(thread_id)
            if not indexed:
                continue
            thread_name, updated_at_ns = indexed
            changed = (
                self._sync_codex_native_title(
                    session,
                    thread_name=thread_name,
                    updated_at_ns=updated_at_ns,
                    thread_id=thread_id,
                    persist=False,
                )
                or changed
            )

        if changed and persist:
            self._save_state()
        return changed

    def _ingest_codex_fork_tool_use_event(
        self,
        session_id: str,
        event: dict[str, Any],
        sanitized_payload: dict[str, Any],
    ) -> None:
        schema_version = event.get("schema_version")
        if isinstance(schema_version, str) and schema_version.isdigit():
            schema_version = int(schema_version)
        if not isinstance(schema_version, int):
            schema_version = None

        executed = sanitized_payload.get("executed")
        success = sanitized_payload.get("success")
        if executed is False:
            final_status = "skipped"
        elif success is True:
            final_status = "completed"
        elif success is False:
            final_status = "failed"
        else:
            final_status = None

        created_at = self._parse_codex_fork_timestamp(event.get("ts"))
        error_message = sanitized_payload.get("error_message")
        if isinstance(error_message, (dict, list)):
            error_message = json.dumps(error_message, separators=(",", ":"), default=str)
        elif error_message is not None and not isinstance(error_message, str):
            error_message = str(error_message)
        tool_name = sanitized_payload.get("tool_name")
        if isinstance(tool_name, str):
            session = self.sessions.get(session_id)
            if session is not None:
                session.last_tool_name = tool_name
                if created_at:
                    session.last_tool_call = created_at.astimezone().replace(tzinfo=None)
                else:
                    session.last_tool_call = datetime.now()

        self._safe_log_codex_tool_event(
            session_id=session_id,
            thread_id=str(event.get("session_id")) if event.get("session_id") else None,
            turn_id=sanitized_payload.get("turn_id"),
            item_id=sanitized_payload.get("call_id"),
            event_type="after_tool_use",
            item_type=sanitized_payload.get("tool_kind"),
            phase="post",
            latency_ms=sanitized_payload.get("duration_ms"),
            final_status=final_status,
            error_message=error_message,
            raw_payload=sanitized_payload,
            created_at=created_at,
            provider="codex-fork",
            schema_version=schema_version,
        )

    def _ingest_codex_fork_raw_response_item(
        self,
        session_id: str,
        event: dict[str, Any],
        payload: dict[str, Any],
    ) -> None:
        """Project codex-fork raw response items into observability rows."""
        item = payload.get("item") if isinstance(payload.get("item"), dict) else {}
        item_type = item.get("type")
        if not isinstance(item_type, str):
            return

        schema_version = event.get("schema_version")
        if isinstance(schema_version, str) and schema_version.isdigit():
            schema_version = int(schema_version)
        if not isinstance(schema_version, int):
            schema_version = None

        created_at = self._parse_codex_fork_timestamp(event.get("ts"))
        turn_id = payload.get("turn_id") or item.get("turn_id")
        thread_id = str(event.get("session_id")) if event.get("session_id") else None

        if item_type == "function_call":
            tool_name = item.get("name")
            if not isinstance(tool_name, str) or not tool_name:
                return
            raw_payload = {
                "tool_name": tool_name,
                "tool_input": self._sanitize_codex_fork_tool_input_value(item.get("arguments")),
            }
            self._safe_log_codex_tool_event(
                session_id=session_id,
                thread_id=thread_id,
                turn_id=turn_id,
                item_id=item.get("call_id"),
                event_type="submitted",
                item_type="tool",
                phase="pre",
                raw_payload=raw_payload,
                created_at=created_at,
                provider="codex-fork",
                schema_version=schema_version,
            )
            session = self.sessions.get(session_id)
            if session is not None:
                session.last_tool_name = tool_name
                if created_at:
                    session.last_tool_call = created_at.astimezone().replace(tzinfo=None)
                else:
                    session.last_tool_call = datetime.now()
            return

        if item_type != "message":
            return

        role = item.get("role")
        if role != "assistant":
            return

        content = item.get("content")
        if not isinstance(content, list):
            return

        text_parts: list[str] = []
        for part in content:
            if not isinstance(part, dict):
                continue
            if part.get("type") != "output_text":
                continue
            text = part.get("text")
            if isinstance(text, str) and text.strip():
                text_parts.append(text)

        if not text_parts:
            return

        joined_text = "\n\n".join(text_parts)
        self._safe_log_codex_turn_event(
            session_id=session_id,
            thread_id=thread_id,
            turn_id=turn_id,
            event_type="raw_response_item",
            status="streaming",
            output_preview=joined_text[:400],
            raw_payload={
                "role": role,
                "content_types": [part.get("type") for part in content if isinstance(part, dict)],
            },
            created_at=created_at,
            provider="codex-fork",
            schema_version=schema_version,
        )

    async def _handle_codex_fork_turn_complete(
        self,
        session_id: str,
        last_message: str,
        event: dict[str, Any],
    ) -> None:
        """Persist and notify on codex-fork turn completion output."""
        if not last_message:
            return

        if self.hook_output_store is not None:
            self.hook_output_store["latest"] = last_message
            self.hook_output_store[session_id] = last_message

        session = self.sessions.get(session_id)
        if not session:
            return

        session.last_activity = datetime.now()
        session.status = SessionStatus.IDLE
        self._save_state()

        if self.message_queue_manager:
            self.message_queue_manager.mark_session_idle(
                session_id,
                completion_transition=True,
            )

        if not getattr(self, "notifier", None):
            return
        if not session.telegram_chat_id:
            return

        event_obj = NotificationEvent(
            session_id=session.id,
            event_type="response",
            message="Codex responded",
            context=last_message,
            urgent=False,
        )
        await self.notifier.notify(event_obj, session)

    def ingest_codex_fork_event(self, session_id: str, event: dict[str, Any]) -> Optional[dict[str, Any]]:
        """Ingest one codex-fork bridge event record."""
        event_type = event.get("event_type") or event.get("type")
        if not event_type:
            return None
        payload = event.get("payload") if isinstance(event.get("payload"), dict) else {}
        provider_session_id = event.get("session_id")
        if not provider_session_id and payload:
            provider_session_id = payload.get("session_id")
        if isinstance(provider_session_id, str) and provider_session_id.strip() and provider_session_id != "unknown":
            session = self.sessions.get(session_id)
            if session and session.provider in ("codex", "codex-fork") and session.provider_resume_id != provider_session_id:
                session.provider_resume_id = provider_session_id
                self._save_state()
        seq_raw = event.get("seq")
        seq = int(seq_raw) if isinstance(seq_raw, int) or (isinstance(seq_raw, str) and seq_raw.isdigit()) else None
        session_epoch = event.get("session_epoch")
        normalized = self._normalize_codex_fork_event_type(event_type)
        payload_for_store = payload
        if normalized == "after_tool_use":
            payload_for_store = self._sanitize_codex_fork_tool_payload(payload)
            self._ingest_codex_fork_tool_use_event(
                session_id=session_id,
                event=event,
                sanitized_payload=payload_for_store,
            )
        elif normalized == "raw_response_item":
            self._ingest_codex_fork_raw_response_item(
                session_id=session_id,
                event=event,
                payload=payload,
            )
        elif normalized in {"turn_started", "turn_complete", "turn_aborted"}:
            payload_message = payload.get("last_agent_message")
            payload_preview = payload_message if isinstance(payload_message, str) else ""
            created_at = self._parse_codex_fork_timestamp(event.get("ts"))
            self._safe_log_codex_turn_event(
                session_id=session_id,
                thread_id=str(event.get("session_id")) if event.get("session_id") else None,
                turn_id=payload.get("turn_id") or event.get("turn_id"),
                event_type=normalized,
                status="completed" if normalized == "turn_complete" else None,
                output_preview=payload_preview[:400] if payload_preview else None,
                raw_payload=payload,
                created_at=created_at,
                provider="codex-fork",
                schema_version=event.get("schema_version") if isinstance(event.get("schema_version"), int) else None,
            )
        payload_for_reducer = payload_for_store
        if normalized == "thread_name_updated" and isinstance(payload_for_store, dict):
            event_thread_id = self._normalize_codex_thread_id(event.get("session_id"))
            if event_thread_id and not self._normalize_codex_thread_id(payload_for_store.get("thread_id")):
                payload_for_reducer = {**payload_for_store, "session_id": event_thread_id}

        turn_id = payload_for_store.get("turn_id") or event.get("turn_id")
        self.codex_event_store.append_event(
            session_id=session_id,
            event_type=f"codex_fork_{normalized}",
            turn_id=turn_id,
            payload={
                "schema_version": event.get("schema_version"),
                "seq": seq,
                "session_epoch": session_epoch,
                "payload": payload_for_store,
            },
        )
        return self._reduce_codex_fork_lifecycle(
            session_id=session_id,
            event_type=normalized,
            payload=payload_for_reducer,
            seq=seq,
            session_epoch=session_epoch,
            event_timestamp_ns=self._timestamp_to_epoch_ns(event.get("ts")),
        )

    def _sync_session_resume_id(self, session: Session) -> bool:
        """Synchronize one session's provider-native resume identifier."""
        resume_id: Optional[str] = None
        if session.provider == "claude" and session.transcript_path:
            resume_id = Path(session.transcript_path).expanduser().stem
        elif session.provider == "codex":
            resume_id = session.provider_resume_id or self._discover_codex_cli_resume_id(session)
        elif session.provider == "codex-app" and session.codex_thread_id:
            resume_id = session.codex_thread_id
        elif session.provider == "codex-fork" and session.provider_resume_id:
            resume_id = session.provider_resume_id

        if resume_id == session.provider_resume_id:
            return False

        session.provider_resume_id = resume_id
        return True

    def _read_codex_cli_session_metadata(self, session_file: Path) -> dict[str, Any]:
        """Read one Codex CLI session file and return minimal binding metadata."""
        try:
            with session_file.open("r", encoding="utf-8", errors="ignore") as handle:
                first_line = handle.readline()
        except OSError:
            return {"id": None, "cwd": None, "started_at": None}

        if not first_line:
            return {"id": None, "cwd": None, "started_at": None}

        try:
            record = json.loads(first_line)
        except json.JSONDecodeError:
            return {"id": None, "cwd": None, "started_at": None}

        if record.get("type") != "session_meta":
            return {"id": None, "cwd": None, "started_at": None}

        payload = record.get("payload") if isinstance(record.get("payload"), dict) else {}
        session_id = payload.get("id")
        cwd = payload.get("cwd")
        started_at = self._parse_claude_timestamp(payload.get("timestamp") or record.get("timestamp"))
        return {
            "id": session_id if isinstance(session_id, str) and session_id.strip() else None,
            "cwd": cwd if isinstance(cwd, str) and cwd.strip() else None,
            "started_at": started_at,
        }

    def _discover_codex_cli_resume_id(self, session: Session) -> Optional[str]:
        """Bind a missing legacy Codex CLI resume id using Codex's session metadata."""
        if session.provider != "codex" or not session.working_dir:
            return session.provider_resume_id

        sessions_root = Path.home() / ".codex" / "sessions"
        if not sessions_root.is_dir():
            return session.provider_resume_id

        resolved_working_dir = str(Path(session.working_dir).expanduser().resolve())
        claimed_ids = {
            other.provider_resume_id
            for other in self.sessions.values()
            if other.id != session.id and other.provider_resume_id
        }

        target_time_ns = max(
            self._session_time_ns(session, "last_activity"),
            self._session_time_ns(session, "created_at"),
        )

        candidate_files: list[Path] = []
        base_time = session.created_at.astimezone() if session.created_at.tzinfo else session.created_at
        for day_offset in (-1, 0, 1):
            day = base_time.date() + timedelta(days=day_offset)
            day_dir = sessions_root / f"{day.year:04d}" / f"{day.month:02d}" / f"{day.day:02d}"
            if day_dir.is_dir():
                candidate_files.extend(day_dir.glob("rollout-*.jsonl"))

        if not candidate_files:
            return session.provider_resume_id

        candidates: list[tuple[int, int, str]] = []
        for session_file in candidate_files:
            metadata = self._read_codex_cli_session_metadata(session_file)
            candidate_id = metadata.get("id")
            candidate_cwd = metadata.get("cwd")
            if not candidate_id or candidate_id in claimed_ids or not candidate_cwd:
                continue
            try:
                resolved_candidate_cwd = str(Path(candidate_cwd).expanduser().resolve())
            except OSError:
                continue
            if resolved_candidate_cwd != resolved_working_dir:
                continue

            started_at = metadata.get("started_at")
            started_ns = int(started_at.timestamp() * 1_000_000_000) if isinstance(started_at, datetime) else 0
            distance = abs(target_time_ns - started_ns) if started_ns else 10**30
            candidates.append((distance, -started_ns, candidate_id))

        if not candidates:
            return session.provider_resume_id

        candidates.sort()
        return candidates[0][2]

    def _get_codex_resume_id_from_events(self, session_id: str) -> Optional[str]:
        """Recover a codex resume id from persisted lifecycle events."""
        try:
            events = self.codex_event_store.get_events(session_id=session_id, limit=200).get("events", [])
        except Exception:
            return None

        for event in reversed(events):
            if event.get("event_type") != "codex_fork_session_configured":
                continue
            payload_preview = event.get("payload_preview") or {}
            payload = payload_preview.get("payload") if isinstance(payload_preview, dict) else None
            candidate = payload.get("session_id") if isinstance(payload, dict) else None
            if isinstance(candidate, str) and candidate.strip():
                return candidate
        return None

    def get_session_resume_id(self, session: Session) -> Optional[str]:
        """Return the provider-native identifier needed to resume a stopped session."""
        if self._sync_session_resume_id(session):
            self._save_state()
        if session.provider_resume_id:
            return session.provider_resume_id
        if session.provider == "codex-fork":
            recovered = self._get_codex_resume_id_from_events(session.id)
            if recovered:
                session.provider_resume_id = recovered
                self._save_state()
                return recovered
        return None

    def get_codex_fork_lifecycle_state(self, session_id: str) -> Optional[dict[str, Any]]:
        """Return current codex-fork lifecycle reducer snapshot."""
        state = self.codex_fork_lifecycle.get(session_id)
        if state is None:
            return None
        return dict(state)

    def _start_codex_fork_event_monitor(self, session: Session, from_eof: bool = False):
        """Start background JSONL event monitor for one codex-fork session."""
        if session.id in self.codex_fork_event_monitors:
            return
        if session.provider != "codex-fork":
            return
        if from_eof:
            stream_path = self._codex_fork_event_stream_path(session)
            if stream_path.exists():
                self.codex_fork_event_offsets[session.id] = stream_path.stat().st_size
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            return
        task = loop.create_task(self._monitor_codex_fork_event_stream(session.id))
        self.codex_fork_event_monitors[session.id] = task

    async def _stop_codex_fork_event_monitor(self, session_id: str):
        """Stop one codex-fork event monitor task."""
        task = self.codex_fork_event_monitors.pop(session_id, None)
        if task:
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass

    async def _monitor_codex_fork_event_stream(self, session_id: str):
        """Tail codex-fork event-stream file and feed reducer."""
        buffer = self.codex_fork_event_buffers.get(session_id, "")
        try:
            while True:
                session = self.sessions.get(session_id)
                if not session or session.provider != "codex-fork":
                    return

                stream_path = self._codex_fork_event_stream_path(session)
                offset = self.codex_fork_event_offsets.get(session_id, 0)
                if stream_path.exists():
                    with open(stream_path, "r", encoding="utf-8", errors="ignore") as handle:
                        handle.seek(offset)
                        chunk = handle.read()
                        offset = handle.tell()
                    self.codex_fork_event_offsets[session_id] = offset

                    if chunk:
                        buffer = buffer + chunk
                        lines = buffer.splitlines()
                        if buffer and not buffer.endswith("\n"):
                            buffer = lines.pop() if lines else buffer
                        else:
                            buffer = ""
                        self.codex_fork_event_buffers[session_id] = buffer

                        for line in lines:
                            raw = line.strip()
                            if not raw:
                                continue
                            try:
                                event = json.loads(raw)
                            except json.JSONDecodeError:
                                logger.debug("Skipping non-JSON codex-fork event line for %s", session_id)
                                continue
                            self.ingest_codex_fork_event(session_id, event)
                            normalized = self._normalize_codex_fork_event_type(
                                event.get("event_type") or event.get("type")
                            )
                            if normalized == "turn_complete":
                                payload = event.get("payload") if isinstance(event.get("payload"), dict) else {}
                                last_message = payload.get("last_agent_message")
                                if isinstance(last_message, str) and last_message.strip():
                                    await self._handle_codex_fork_turn_complete(
                                        session_id=session_id,
                                        last_message=last_message,
                                        event=event,
                                    )

                await asyncio.sleep(self.codex_fork_event_poll_interval_seconds)
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            logger.warning("Codex-fork event monitor failed for %s: %s", session_id, exc)
            self._set_codex_fork_lifecycle_state(
                session_id=session_id,
                state="error",
                cause_event_type="event_stream_monitor_error",
            )
        finally:
            self.codex_fork_event_monitors.pop(session_id, None)
            self.codex_fork_event_buffers.pop(session_id, None)

    async def _create_session_common(
        self,
        working_dir: str,
        name: Optional[str] = None,
        friendly_name: Optional[str] = None,
        telegram_chat_id: Optional[int] = None,
        parent_session_id: Optional[str] = None,
        spawn_prompt: Optional[str] = None,
        model: Optional[str] = None,
        initial_prompt: Optional[str] = None,
        provider: str = "claude",
        defer_telegram_topic: bool = False,
    ) -> Optional[Session]:
        """
        Common session creation logic (private method).

        Args:
            working_dir: Directory to run Claude in
            name: Optional session name (generated if not provided)
            friendly_name: Optional user-friendly name
            telegram_chat_id: Telegram chat to associate with session
            parent_session_id: Parent session ID (for child sessions)
            spawn_prompt: Initial prompt used to spawn (for child sessions)
            model: Model override (opus, sonnet, haiku)
            initial_prompt: Initial prompt to send after creation

        Returns:
            Created Session or None on failure
        """
        provider_rejection = self.get_provider_create_rejection(provider, working_dir=working_dir)
        if provider_rejection:
            logger.error("Rejecting %s session create in %s: %s", provider, working_dir, provider_rejection)
            return None

        # Create session object with common fields
        session = Session(
            working_dir=working_dir,
            telegram_chat_id=telegram_chat_id,
            parent_session_id=parent_session_id,
            spawn_prompt=spawn_prompt,
            spawned_at=datetime.now() if parent_session_id else None,
            provider=provider,
            model=model,
        )

        if friendly_name:
            self.set_session_friendly_name(session, friendly_name, explicit=True)

        # Set name if provided, otherwise __post_init__ generates claude-{id}
        if name:
            session.name = name

        # Detect git remote URL for repo matching (async to avoid blocking)
        session.git_remote_url = await self._get_git_remote_url_async(working_dir)

        # Set up log file path and tmux session for CLI providers
        if provider in ("claude", "codex", "codex-fork"):
            session.log_file = str(self.log_dir / f"{session.name}.log")

            if provider == "claude":
                # Get Claude config
                claude_config = self.config.get("claude", {})
                command = claude_config.get("command", "claude")
                args = claude_config.get("args", [])
                default_model = claude_config.get("default_model", "sonnet")
            elif provider == "codex":
                # Codex CLI config
                command = self.codex_cli_command
                args = self.codex_cli_args
                default_model = self.codex_default_model
            else:
                # Codex-fork config with codex fallback when the fork binary is unavailable.
                command, args, effective_provider, fallback_reason = self._build_codex_fork_launch_spec(session)
                default_model = (
                    self.codex_fork_default_model
                    if effective_provider == "codex-fork"
                    else self.codex_default_model
                )
                if effective_provider != session.provider:
                    logger.warning(
                        "Falling back from codex-fork to codex for %s: %s",
                        session.id,
                        fallback_reason,
                    )
                    session.provider = effective_provider
                    provider = effective_provider

            # Select model (override or default)
            selected_model = model or default_model

            # Create the tmux session with config args
            # NOTE: session.tmux_session is auto-set by __post_init__ to {provider}-{id}
            created = await asyncio.to_thread(
                self.tmux.create_session_with_command,
                session.tmux_session,
                working_dir,
                session.log_file,
                session_id=session.id,
                command=command,
                args=args,
                model=selected_model if model else None,  # Only pass if explicitly set
                initial_prompt=initial_prompt,
            )
            if not created:
                tmux_error = getattr(self.tmux, "last_error_message", None)
                if tmux_error:
                    logger.error("Failed to create tmux session for %s: %s", session.name, tmux_error)
                else:
                    logger.error(f"Failed to create tmux session for {session.name}")
                return None
            session.tmux_socket_name = self._tmux_socket_name()
        elif provider == "codex-app":
            try:
                codex_session = CodexAppServerSession(
                    session_id=session.id,
                    working_dir=working_dir,
                    config=self.codex_config,
                    on_turn_complete=self._handle_codex_turn_complete,
                    on_turn_started=self._handle_codex_turn_started,
                    on_turn_delta=self._handle_codex_turn_delta,
                    on_review_complete=self._handle_codex_review_complete,
                    on_server_request=self._handle_codex_server_request,
                    on_item_notification=self._handle_codex_item_notification,
                    on_stream_error=self._handle_codex_stream_error,
                )
                thread_id = await codex_session.start(thread_id=session.codex_thread_id, model=model)
                session.codex_thread_id = thread_id
                self._sync_session_resume_id(session)
                if initial_prompt:
                    try:
                        await codex_session.send_user_turn(initial_prompt, model=model)
                        session.last_activity = datetime.now()
                    except Exception:
                        await codex_session.close()
                        raise
                self.codex_sessions[session.id] = codex_session
            except CodexAppServerError as e:
                logger.error(f"Failed to start Codex app-server session for {session.name}: {e}")
                return None
            except Exception as e:
                logger.error(f"Unexpected error starting Codex app session: {e}")
                return None
        else:
            logger.error(f"Unknown session provider: {provider}")
            return None

        # Mark as running and save
        if provider == "codex-app" and not initial_prompt:
            session.status = SessionStatus.IDLE
        else:
            session.status = SessionStatus.RUNNING
        self.sessions[session.id] = session
        self._sync_session_resume_id(session)
        self._save_state()

        if provider == "codex-fork":
            self.codex_fork_runtime_owner[session.id] = parent_session_id or session.id
            self._set_codex_fork_lifecycle_state(
                session_id=session.id,
                state="running" if initial_prompt else "idle",
                cause_event_type="session_created",
            )
            self._start_codex_fork_event_monitor(session)

        # Auto-create Telegram topic for this session. Spawn paths can defer this
        # non-critical work so the HTTP response returns before Telegram latency.
        if defer_telegram_topic:
            self._schedule_telegram_topic_ensure(session, telegram_chat_id)
        else:
            await self._ensure_telegram_topic(session, telegram_chat_id)

        if provider == "codex-app" and not initial_prompt and self.message_queue_manager:
            self.message_queue_manager.mark_session_idle(session.id)

        if provider in ("claude", "codex", "codex-fork") and friendly_name:
            try:
                if self._is_safe_provider_native_rename_name(friendly_name):
                    await self.queue_provider_native_rename(session, friendly_name)
                else:
                    logger.warning(
                        "Skipping provider-native rename for session %s: unsafe friendly name",
                        session.id,
                    )
            except Exception as exc:
                logger.warning(
                    "Failed to queue provider-native rename for spawned session %s: %s",
                    session.id,
                    exc,
                )

        # Log creation
        if parent_session_id:
            logger.info(f"Spawned child session {session.name} (id={session.id}, parent={parent_session_id})")
        else:
            logger.info(f"Created session {session.name} (id={session.id})")

        return session

    def _schedule_telegram_topic_ensure(
        self,
        session: "Session",
        explicit_chat_id: Optional[int] = None,
    ) -> None:
        """Ensure Telegram topic creation runs in the background for a session."""
        existing_task = self._pending_telegram_topic_tasks_by_session.get(session.id)
        if existing_task is not None and not existing_task.done():
            return

        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            return

        async def _runner() -> None:
            try:
                await self._ensure_telegram_topic(session, explicit_chat_id)
            except Exception as exc:
                logger.warning(
                    "Deferred Telegram topic creation failed for session %s: %s",
                    session.id,
                    exc,
                )

        task = loop.create_task(_runner())
        self._pending_telegram_topic_tasks_by_session[session.id] = task
        self._pending_telegram_topic_tasks.add(task)
        task.add_done_callback(self._pending_telegram_topic_tasks.discard)
        task.add_done_callback(
            lambda completed_task, session_id=session.id: self._clear_pending_telegram_topic_task(
                session_id,
                completed_task,
            )
        )

    def _clear_pending_telegram_topic_task(
        self,
        session_id: str,
        completed_task: asyncio.Task[Any],
    ) -> None:
        """Drop the in-flight mapping only if it still points at this task."""
        current_task = self._pending_telegram_topic_tasks_by_session.get(session_id)
        if current_task is completed_task:
            self._pending_telegram_topic_tasks_by_session.pop(session_id, None)

    def set_topic_creator(self, creator: Callable[..., Awaitable[Optional[int]]]):
        """Set the callback used to create Telegram forum topics.

        Signature: async (session_id, chat_id, topic_name) -> Optional[int]
        Returns the topic/thread ID on success, None on failure.
        """
        self._topic_creator = creator

    async def _ensure_telegram_topic(self, session: "Session", explicit_chat_id: Optional[int] = None):
        """Ensure a session has a Telegram forum topic, creating one if needed.

        Args:
            session: The session to ensure a topic for
            explicit_chat_id: Chat ID passed by the caller (e.g. from Telegram /new)
        """
        lock = self._telegram_topic_ensure_locks.setdefault(session.id, asyncio.Lock())
        async with lock:
            changed = False

            # 1. Ensure chat_id is set (explicit > existing > default)
            if not session.telegram_chat_id:
                chat_id = explicit_chat_id or self.default_forum_chat_id
                if chat_id:
                    session.telegram_chat_id = chat_id
                    changed = True

            # 2. Create topic if chat_id is set but thread_id is missing
            if session.telegram_chat_id and not session.telegram_thread_id and self._topic_creator:
                display_name = self.get_effective_session_name(session) or "session"
                topic_name = f"{display_name} [{session.id}]"
                try:
                    thread_id = await self._topic_creator(
                        session.id, session.telegram_chat_id, topic_name
                    )
                    if thread_id:
                        session.telegram_thread_id = thread_id
                        self._upsert_telegram_topic_record(
                            session,
                            session.telegram_chat_id,
                            thread_id,
                        )
                        self._save_state()  # Persist IMMEDIATELY — minimize race window
                        changed = False     # Already saved; prevent redundant outer save
                        logger.info(
                            f"Auto-created topic for session {session.id}: "
                            f"chat={session.telegram_chat_id}, thread={thread_id}"
                        )
                except Exception as e:
                    logger.warning(f"Failed to auto-create topic for session {session.id}: {e}")

            if changed:
                self._save_state()

    async def create_session(
        self,
        working_dir: str,
        name: Optional[str] = None,
        telegram_chat_id: Optional[int] = None,
        provider: str = "claude",
        parent_session_id: Optional[str] = None,
    ) -> Optional[Session]:
        """
        Create a new Claude Code session (async, non-blocking).

        Args:
            working_dir: Directory to run Claude in
            name: Optional session name (generated if not provided)
            telegram_chat_id: Telegram chat to associate with session
            parent_session_id: Optional parent owner for direct creates from managed sessions

        Returns:
            Created Session or None on failure
        """
        return await self._create_session_common(
            working_dir=working_dir,
            name=name,
            telegram_chat_id=telegram_chat_id,
            provider=provider,
            parent_session_id=parent_session_id,
        )

    async def spawn_child_session(
        self,
        parent_session_id: str,
        prompt: str,
        name: Optional[str] = None,
        wait: Optional[int] = None,
        model: Optional[str] = None,
        working_dir: Optional[str] = None,
        provider: Optional[str] = None,
        defer_telegram_topic: bool = False,
    ) -> Optional[Session]:
        """
        Spawn a child agent session.

        Args:
            parent_session_id: Parent session ID
            prompt: Initial prompt for the child agent
            name: Friendly name for the child session
            wait: Monitor child and notify when complete or idle for N seconds
            model: Model override (opus, sonnet, haiku)
            working_dir: Working directory (defaults to parent's directory)

        Returns:
            Created child Session or None on failure
        """
        # Get parent session
        parent_session = self.sessions.get(parent_session_id)
        if not parent_session:
            logger.error(f"Parent session not found: {parent_session_id}")
            return None

        # Determine working directory
        child_working_dir = working_dir or parent_session.working_dir

        # Generate session name if not provided
        # Use friendly_name parameter, auto-generate session.name if needed
        # Take first 6 chars of parent ID for brevity (session IDs are 8-char UUIDs)
        session_name = f"child-{parent_session_id[:6]}" if not name else None

        # Select provider (default to parent)
        selected_provider = provider or parent_session.provider or "claude"

        # Create session using common logic
        session = await self._create_session_common(
            working_dir=child_working_dir,
            name=session_name,
            friendly_name=name,
            parent_session_id=parent_session_id,
            spawn_prompt=prompt,
            model=model,
            initial_prompt=prompt,
            provider=selected_provider,
            defer_telegram_topic=defer_telegram_topic,
        )

        if not session:
            return None

        # Register background monitoring if wait is specified
        if wait and self.child_monitor:
            self.child_monitor.register_child(
                child_session_id=session.id,
                parent_session_id=parent_session_id,
                wait_seconds=wait,
            )

        return session

    def get_session(self, session_id: str) -> Optional[Session]:
        """Get a session by ID."""
        return self.sessions.get(session_id)

    def get_session_by_name(self, name: str) -> Optional[Session]:
        """Get a session by name."""
        for session in self.sessions.values():
            if session.name == name:
                return session
        return None

    def get_session_by_telegram_chat(self, chat_id: int) -> list[Session]:
        """Get all sessions associated with a Telegram chat."""
        return [s for s in self.sessions.values() if s.telegram_chat_id == chat_id]

    def get_session_by_telegram_thread(self, chat_id: int, message_id: int) -> Optional[Session]:
        """Get session by Telegram thread (thread ID)."""
        for session in self.sessions.values():
            if session.telegram_chat_id == chat_id and session.telegram_thread_id == message_id:
                return session
        return None

    def set_role(self, session_id: str, role: str) -> bool:
        """Set the role tag for a session."""
        session = self.sessions.get(session_id)
        if not session:
            return False
        session.role = role
        self._save_state()
        return True

    def clear_role(self, session_id: str) -> bool:
        """Clear the role tag for a session."""
        session = self.sessions.get(session_id)
        if not session:
            return False
        session.role = None
        self._save_state()
        return True

    @staticmethod
    def normalize_agent_role(role: str) -> str:
        """Canonicalize registry roles for stable lookup and CLI use."""
        raw = (role or "").strip().lower()
        if not raw:
            return ""
        normalized = re.sub(r"[^a-z0-9]+", "-", raw).strip("-")
        return re.sub(r"-{2,}", "-", normalized)

    def _get_agent_registration_map(self) -> dict[str, AgentRegistration]:
        """Return the registry map, tolerating partially constructed test instances."""
        registrations = getattr(self, "agent_registrations", None)
        if registrations is None:
            registrations = {}
            self.agent_registrations = registrations
        return registrations

    def _synchronize_maintainer_alias(self) -> None:
        """Keep the legacy maintainer compatibility field in sync with the registry."""
        registration = self._get_agent_registration_map().get("maintainer")
        self.maintainer_session_id = registration.session_id if registration else None

    def _get_live_registered_session(self, session_id: str) -> Optional[Session]:
        """Return the owning session when it is still live for registry purposes."""
        session = self.sessions.get(session_id)
        if not session or session.status == SessionStatus.STOPPED:
            return None
        return session

    def _prune_agent_registrations(self, persist: bool = True) -> bool:
        """Drop registrations whose owning sessions no longer exist or are no longer live."""
        registration_map = self._get_agent_registration_map()
        removed = False
        for role, registration in list(registration_map.items()):
            if self._get_live_registered_session(registration.session_id):
                continue
            self.agent_role_last_session_ids[role] = registration.session_id
            registration_map.pop(role, None)
            removed = True
        if removed:
            self._synchronize_maintainer_alias()
            if persist:
                self._save_state()
        return removed

    def _reparent_live_children(self, old_parent_session_id: Optional[str], new_parent_session_id: str) -> int:
        """Move live child sessions from one dead/cleared owner to a new owner."""
        if not old_parent_session_id or old_parent_session_id == new_parent_session_id:
            return 0

        reparented = 0
        for session in self.sessions.values():
            if session.parent_session_id != old_parent_session_id:
                continue
            if session.status == SessionStatus.STOPPED:
                continue
            session.parent_session_id = new_parent_session_id
            reparented += 1

        if reparented:
            logger.info(
                "Reparented %s live child sessions from %s to %s during role takeover",
                reparented,
                old_parent_session_id,
                new_parent_session_id,
            )

        return reparented

    def register_agent_role(self, session_id: str, role: str) -> AgentRegistration:
        """Register one live session as the current owner for a registry role."""
        normalized_role = self.normalize_agent_role(role)
        if not normalized_role:
            raise ValueError("Role cannot be empty")

        session = self.sessions.get(session_id)
        if not session:
            raise ValueError("Session not found")
        if session.status == SessionStatus.STOPPED:
            raise ValueError("Stopped sessions cannot register roles")

        registration_map = self._get_agent_registration_map()
        prior_holder_session_id: Optional[str] = None
        preexisting = registration_map.get(normalized_role)
        if preexisting and preexisting.session_id != session_id:
            live_owner = self._get_live_registered_session(preexisting.session_id)
            if live_owner:
                raise ValueError(
                    f'Role "{normalized_role}" is already registered to {preexisting.session_id}'
                )
            prior_holder_session_id = preexisting.session_id

        self._prune_agent_registrations(persist=False)
        existing = registration_map.get(normalized_role)
        if existing and existing.session_id != session_id:
            live_owner = self._get_live_registered_session(existing.session_id)
            if live_owner:
                raise ValueError(
                    f'Role "{normalized_role}" is already registered to {existing.session_id}'
                )
        if prior_holder_session_id is None:
            historical_holder = self.agent_role_last_session_ids.get(normalized_role)
            if historical_holder and historical_holder != session_id:
                historical_session = self.sessions.get(historical_holder)
                if historical_session is None or historical_session.status == SessionStatus.STOPPED:
                    prior_holder_session_id = historical_holder

        registration = existing or AgentRegistration(role=normalized_role, session_id=session_id)
        registration.session_id = session_id
        registration_map[normalized_role] = registration
        self.agent_role_last_session_ids[normalized_role] = session_id
        self._reparent_live_children(prior_holder_session_id, session_id)
        self._synchronize_maintainer_alias()
        self._save_state()
        return registration

    def unregister_agent_role(self, session_id: str, role: str) -> bool:
        """Clear one registry role when owned by the given session."""
        normalized_role = self.normalize_agent_role(role)
        if not normalized_role:
            return False
        registration_map = self._get_agent_registration_map()
        registration = registration_map.get(normalized_role)
        if not registration or registration.session_id != session_id:
            return False
        self.agent_role_last_session_ids[normalized_role] = registration.session_id
        registration_map.pop(normalized_role, None)
        self._synchronize_maintainer_alias()
        self._save_state()
        return True

    def unregister_session_roles(self, session_id: str, persist: bool = True) -> list[str]:
        """Remove all registry roles owned by one session."""
        registration_map = self._get_agent_registration_map()
        removed_roles = [
            role
            for role, registration in registration_map.items()
            if registration.session_id == session_id
        ]
        if not removed_roles:
            return []
        for role in removed_roles:
            registration = registration_map.get(role)
            if registration:
                self.agent_role_last_session_ids[role] = registration.session_id
            registration_map.pop(role, None)
        self._synchronize_maintainer_alias()
        if persist:
            self._save_state()
        return sorted(removed_roles)

    def lookup_agent_registration(self, role: str) -> Optional[AgentRegistration]:
        """Resolve one registry role to its current live registration."""
        normalized_role = self.normalize_agent_role(role)
        if not normalized_role:
            return None
        registration_map = self._get_agent_registration_map()
        self._prune_agent_registrations(persist=True)
        registration = registration_map.get(normalized_role)
        if not registration:
            return None
        if not self._get_live_registered_session(registration.session_id):
            registration_map.pop(normalized_role, None)
            self._synchronize_maintainer_alias()
            self._save_state()
            return None
        return registration

    def list_agent_registrations(self) -> list[AgentRegistration]:
        """List all live registry roles."""
        registration_map = self._get_agent_registration_map()
        self._prune_agent_registrations(persist=True)
        registrations = list(registration_map.values())
        registrations.sort(key=lambda registration: registration.role)
        return registrations

    def _maintainer_bootstrap_prompt(self, working_dir: str) -> str:
        """Render the maintainer bootstrap prompt for a new service session."""
        return self.maintainer_bootstrap_prompt_template.replace("{working_dir}", working_dir)

    @staticmethod
    def _normalize_provider_list(raw_value: Any, fallback: list[str]) -> list[str]:
        """Normalize one preferred-provider config value to a non-empty list."""
        if isinstance(raw_value, list):
            normalized = [str(provider).strip() for provider in raw_value if str(provider).strip()]
            return normalized or fallback
        if raw_value is None:
            return fallback
        provider = str(raw_value).strip()
        return [provider] if provider else fallback

    def _normalize_service_role_bootstrap_spec(
        self,
        role: str,
        raw_spec: Any,
        *,
        default_working_dir: str,
        default_friendly_name: Optional[str] = None,
        default_preferred_providers: Optional[list[str]] = None,
        default_bootstrap_prompt: Optional[str] = None,
        default_auto_bootstrap: bool = False,
    ) -> Optional[dict[str, Any]]:
        """Normalize one service-role bootstrap config block."""
        if not isinstance(raw_spec, dict):
            return None

        normalized_role = self.normalize_agent_role(role)
        if not normalized_role:
            return None

        preferred_providers = self._normalize_provider_list(
            raw_spec.get("preferred_providers", raw_spec.get("provider")),
            default_preferred_providers or ["codex-fork", "claude"],
        )
        working_dir = str(raw_spec.get("working_dir", default_working_dir)).strip() or default_working_dir
        friendly_name = str(raw_spec.get("friendly_name", default_friendly_name or normalized_role)).strip() or (
            default_friendly_name or normalized_role
        )
        bootstrap_prompt = str(raw_spec.get("bootstrap_prompt", default_bootstrap_prompt or "")).strip()
        bootstrap_prompt_file = str(
            raw_spec.get("bootstrap_prompt_file", raw_spec.get("boot_prompt_file", ""))
        ).strip()
        raw_task_complete_ttl = raw_spec.get("task_complete_ttl_seconds")
        task_complete_ttl_seconds: Optional[int] = None
        if raw_task_complete_ttl is not None and raw_task_complete_ttl != "":
            try:
                normalized_ttl = int(raw_task_complete_ttl)
            except (TypeError, ValueError):
                normalized_ttl = 0
            if normalized_ttl > 0:
                task_complete_ttl_seconds = normalized_ttl

        return {
            "role": normalized_role,
            "auto_bootstrap": bool(raw_spec.get("auto_bootstrap", default_auto_bootstrap)),
            "working_dir": working_dir,
            "friendly_name": friendly_name,
            "preferred_providers": preferred_providers,
            "bootstrap_prompt": bootstrap_prompt,
            "bootstrap_prompt_file": bootstrap_prompt_file,
            "task_complete_ttl_seconds": task_complete_ttl_seconds,
        }

    def _build_service_role_bootstrap_specs(
        self,
        *,
        default_working_dir: str,
        maintainer_config: dict[str, Any],
    ) -> dict[str, dict[str, Any]]:
        """Build normalized service-role bootstrap specs from config."""
        specs: dict[str, dict[str, Any]] = {}
        raw_roles = self.config.get("service_roles", {})
        if isinstance(raw_roles, dict):
            for role, raw_spec in raw_roles.items():
                normalized = self._normalize_service_role_bootstrap_spec(
                    str(role),
                    raw_spec,
                    default_working_dir=default_working_dir,
                )
                if normalized:
                    specs[normalized["role"]] = normalized

        if "maintainer" not in specs:
            maintainer_spec = self._normalize_service_role_bootstrap_spec(
                "maintainer",
                maintainer_config,
                default_working_dir=default_working_dir,
                default_friendly_name=self.maintainer_friendly_name,
                default_preferred_providers=self.maintainer_preferred_providers,
                default_bootstrap_prompt=self.maintainer_bootstrap_prompt_template,
                default_auto_bootstrap=True,
            )
            if maintainer_spec:
                if maintainer_spec.get("task_complete_ttl_seconds") is None:
                    maintainer_spec["task_complete_ttl_seconds"] = self.maintainer_task_complete_ttl_seconds
                specs["maintainer"] = maintainer_spec

        return specs

    def get_service_role_bootstrap_spec(self, role: str) -> Optional[dict[str, Any]]:
        """Return one configured service-role bootstrap spec."""
        normalized_role = self.normalize_agent_role(role)
        if not normalized_role:
            return None
        spec = self.service_role_bootstrap_specs.get(normalized_role)
        if not spec:
            return None
        return dict(spec)

    def _get_service_role_bootstrap_lock(self, role: str) -> asyncio.Lock:
        """Return a stable bootstrap lock for one service role."""
        normalized_role = self.normalize_agent_role(role)
        return self._service_role_bootstrap_locks.setdefault(normalized_role, asyncio.Lock())

    def _resolve_service_role_working_dir(self, spec: dict[str, Any]) -> str:
        """Resolve one service-role working directory."""
        candidate = Path(str(spec["working_dir"])).expanduser().resolve()
        role = spec["role"]
        if not candidate.exists():
            raise ValueError(f'Service role "{role}" working directory does not exist: {candidate}')
        if not candidate.is_dir():
            raise ValueError(f'Service role "{role}" working directory is not a directory: {candidate}')
        return str(candidate)

    def _render_service_role_bootstrap_prompt(self, spec: dict[str, Any], working_dir: str) -> str:
        """Render one bootstrap prompt from inline text or a prompt file."""
        role = spec["role"]
        prompt_template = str(spec.get("bootstrap_prompt") or "").strip()
        prompt_file = str(spec.get("bootstrap_prompt_file") or "").strip()
        if prompt_file:
            prompt_path = Path(prompt_file).expanduser()
            if not prompt_path.is_absolute():
                prompt_path = Path(working_dir) / prompt_path
            prompt_path = prompt_path.resolve()
            if not prompt_path.exists():
                raise ValueError(f'Service role "{role}" bootstrap prompt file does not exist: {prompt_path}')
            if not prompt_path.is_file():
                raise ValueError(f'Service role "{role}" bootstrap prompt path is not a file: {prompt_path}')
            prompt_template = prompt_path.read_text().strip()
        if not prompt_template:
            prompt_template = DEFAULT_SERVICE_ROLE_BOOTSTRAP_PROMPT
        return (
            prompt_template
            .replace("{working_dir}", working_dir)
            .replace("{role}", role)
        )

    def _provider_entrypoint_available(self, provider: str) -> bool:
        """Best-effort preflight for tmux-backed providers used during maintainer bootstrap."""
        if provider == "codex-fork":
            command = self.codex_fork_command
        elif provider == "codex":
            command = self.codex_cli_command
        elif provider == "claude":
            command = self.config.get("claude", {}).get("command", "claude")
        else:
            return True

        if not command:
            return False

        try:
            entrypoint = shlex.split(str(command))[0]
        except ValueError:
            entrypoint = str(command).strip()
        if not entrypoint:
            return False

        if "/" in entrypoint or entrypoint.startswith("~"):
            candidate = Path(entrypoint).expanduser()
            return candidate.exists() and os.access(candidate, os.X_OK)

        return shutil.which(entrypoint) is not None

    def _resolve_maintainer_working_dir(self) -> str:
        """Resolve the maintainer service working directory."""
        candidate = Path(self.maintainer_working_dir).expanduser().resolve()
        if not candidate.exists():
            raise ValueError(f"Maintainer working directory does not exist: {candidate}")
        if not candidate.is_dir():
            raise ValueError(f"Maintainer working directory is not a directory: {candidate}")
        return str(candidate)

    def _refresh_maintainer_service_role_spec(self) -> None:
        """Keep legacy maintainer bootstrap attributes mirrored into the generic role spec."""
        if self._maintainer_service_role_explicit:
            return
        self.service_role_bootstrap_specs["maintainer"] = {
            "role": "maintainer",
            "auto_bootstrap": True,
            "working_dir": self.maintainer_working_dir,
            "friendly_name": self.maintainer_friendly_name,
            "preferred_providers": list(self.maintainer_preferred_providers),
            "bootstrap_prompt": self.maintainer_bootstrap_prompt_template,
            "bootstrap_prompt_file": self.maintainer_bootstrap_prompt_file,
            "task_complete_ttl_seconds": (
                self.service_role_bootstrap_specs.get("maintainer", {}).get("task_complete_ttl_seconds")
                or self.maintainer_task_complete_ttl_seconds
            ),
        }

    async def ensure_maintainer_session(self) -> tuple[Session, bool]:
        """Return the live maintainer session, spawning it if needed."""
        self._refresh_maintainer_service_role_spec()
        return await self.ensure_role_session("maintainer")

    def get_service_role_session(self, role: str) -> Optional[Session]:
        """Return the active live session for one service role, if registered."""
        registration = self.lookup_agent_registration(role)
        if not registration:
            return None
        return self._get_live_registered_session(registration.session_id)

    async def ensure_role_session(self, role: str) -> tuple[Session, bool]:
        """Return the live session for one auto-bootstrap service role, spawning it if needed."""
        normalized_role = self.normalize_agent_role(role)
        if not normalized_role:
            raise ValueError("Role cannot be empty")

        spec = self.get_service_role_bootstrap_spec(normalized_role)
        if not spec or not spec.get("auto_bootstrap"):
            raise ValueError(f'Role "{normalized_role}" is not configured for auto-bootstrap')

        existing = self.get_service_role_session(normalized_role)
        if existing:
            return existing, False

        async with self._get_service_role_bootstrap_lock(normalized_role):
            existing = self.get_service_role_session(normalized_role)
            if existing:
                return existing, False

            working_dir = self._resolve_service_role_working_dir(spec)
            bootstrap_prompt = self._render_service_role_bootstrap_prompt(spec, working_dir)
            last_error: Optional[str] = None

            for provider in spec["preferred_providers"]:
                if not self._provider_entrypoint_available(provider):
                    last_error = f"{provider} entrypoint unavailable"
                    logger.warning(
                        "Skipping %s bootstrap provider %s: entrypoint unavailable",
                        normalized_role,
                        provider,
                    )
                    continue

                session = await self._create_session_common(
                    working_dir=working_dir,
                    friendly_name=spec["friendly_name"],
                    initial_prompt=bootstrap_prompt,
                    provider=provider,
                )
                if not session:
                    last_error = f"{provider} session creation failed"
                    logger.warning(
                        "%s bootstrap failed for provider %s during session creation",
                        normalized_role,
                        provider,
                    )
                    continue

                session.role = normalized_role
                session.auto_bootstrapped_role = normalized_role
                self._save_state()

                try:
                    self.register_agent_role(session.id, normalized_role)
                except ValueError:
                    adopted = self.get_service_role_session(normalized_role)
                    if adopted and adopted.id != session.id:
                        self.kill_session(session.id)
                        return adopted, False
                    self.kill_session(session.id)
                    raise

                logger.info(
                    "Bootstrapped service role %s as session %s using provider %s",
                    normalized_role,
                    session.id,
                    provider,
                )
                return session, True

            detail = f'Failed to bootstrap role "{normalized_role}"'
            if last_error:
                detail = f"{detail}: {last_error}"
            raise RuntimeError(detail)

    def set_maintainer_session(self, session_id: str) -> bool:
        """Register one session as the current maintainer alias."""
        try:
            self.register_agent_role(session_id, "maintainer")
            return True
        except ValueError:
            return False

    def clear_maintainer_session(self, session_id: str) -> bool:
        """Clear the maintainer alias if owned by the given session."""
        return self.unregister_agent_role(session_id, "maintainer")

    def get_maintainer_session(self) -> Optional[Session]:
        """Return the active maintainer session if one is registered."""
        registration = self.lookup_agent_registration("maintainer")
        if not registration:
            return None
        return self._get_live_registered_session(registration.session_id)

    def get_session_aliases(self, session_id: str) -> list[str]:
        """Return durable aliases that should resolve to this session."""
        registration_map = self._get_agent_registration_map()
        self._prune_agent_registrations(persist=True)
        aliases = [
            role
            for role, registration in registration_map.items()
            if registration.session_id == session_id
        ]
        return sorted(aliases)

    def get_primary_session_alias(self, session_id: str) -> Optional[str]:
        """Return the canonical registry alias for one session, if any."""
        aliases = self.get_session_aliases(session_id)
        return aliases[0] if aliases else None

    def _claude_transcript_root(self) -> Path:
        """Return Claude's transcript project root."""
        config = getattr(self, "config", {}) or {}
        claude_config = config.get("claude", {})
        configured_root = claude_config.get("transcript_root", "~/.claude/projects")
        return Path(str(configured_root)).expanduser()

    @staticmethod
    def _claude_project_dir_name(working_dir: str) -> str:
        """Map one working directory to Claude's per-project transcript directory."""
        resolved = str(Path(working_dir).expanduser().resolve())
        return resolved.replace(os.sep, "-")

    @staticmethod
    def _parse_claude_timestamp(raw_timestamp: Any) -> Optional[datetime]:
        """Parse Claude transcript timestamps into UTC datetimes."""
        if not raw_timestamp:
            return None
        try:
            parsed = datetime.fromisoformat(str(raw_timestamp).replace("Z", "+00:00"))
        except ValueError:
            return None
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=timezone.utc)
        return parsed.astimezone(timezone.utc)

    @staticmethod
    def _session_time_ns(session: Session, attr: str) -> int:
        """Normalize one session timestamp attribute to epoch nanoseconds."""
        value = getattr(session, attr, None)
        if not isinstance(value, datetime):
            return 0
        if value.tzinfo is None:
            value = value.astimezone()
        return int(value.timestamp() * 1_000_000_000)

    def _read_claude_transcript_metadata(self, transcript_path: str) -> dict[str, Any]:
        """Read one Claude transcript and return title plus binding metadata."""
        transcript_file = Path(transcript_path).expanduser()
        if not transcript_file.exists():
            return {
                "title": None,
                "mtime_ns": None,
                "cwd": None,
                "started_at": None,
            }

        stat = transcript_file.stat()
        latest_custom_title: Optional[str] = None
        latest_agent_name: Optional[str] = None
        first_user_cwd: Optional[str] = None
        first_user_timestamp: Optional[datetime] = None

        with transcript_file.open() as handle:
            for line in handle:
                try:
                    entry = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(entry, dict):
                    continue
                if entry.get("type") == "user" and first_user_cwd is None:
                    candidate_cwd = str(entry.get("cwd") or "").strip()
                    if candidate_cwd:
                        first_user_cwd = str(Path(candidate_cwd).expanduser().resolve())
                    first_user_timestamp = self._parse_claude_timestamp(entry.get("timestamp"))
                if entry.get("type") == "custom-title":
                    candidate = str(entry.get("customTitle") or "").strip()
                    if candidate:
                        latest_custom_title = candidate
                elif entry.get("type") == "agent-name":
                    candidate = str(entry.get("agentName") or "").strip()
                    if candidate:
                        latest_agent_name = candidate

        return {
            "title": latest_custom_title or latest_agent_name,
            "mtime_ns": stat.st_mtime_ns,
            "cwd": first_user_cwd,
            "started_at": first_user_timestamp,
        }

    def _extract_claude_live_title(self, session: Session) -> Optional[str]:
        """Read Claude's current native title from tmux when available."""
        if session.provider != "claude" or not session.tmux_session:
            return None

        tmux_controller = getattr(self, "tmux", None)
        if tmux_controller is None or not hasattr(tmux_controller, "get_pane_title"):
            return None

        raw_title = tmux_controller.get_pane_title(session.tmux_session)
        if not isinstance(raw_title, str) or not raw_title:
            return None

        title = raw_title.strip()
        first_token, _, remainder = title.partition(" ")
        if remainder and not any(char.isalnum() for char in first_token):
            title = remainder.strip()
        title = title.strip()
        if not title or title == "Claude Code":
            return None
        hostname_candidates = {
            socket.gethostname().strip(),
            socket.gethostname().split(".", 1)[0].strip(),
        }
        if title in hostname_candidates:
            return None
        if title.endswith(".local") and " " not in title:
            return None
        return title

    def _discover_claude_transcript_path(
        self,
        session: Session,
        *,
        expected_title: Optional[str] = None,
    ) -> Optional[str]:
        """Bind a missing Claude transcript path using cwd plus recent activity."""
        if session.provider != "claude" or session.transcript_path or not session.working_dir:
            return session.transcript_path

        project_dir = self._claude_transcript_root() / self._claude_project_dir_name(session.working_dir)
        if not project_dir.is_dir():
            return None

        resolved_working_dir = str(Path(session.working_dir).expanduser().resolve())
        claimed_paths: set[Path] = set()
        for other_session in self.sessions.values():
            if other_session.id == session.id or not other_session.transcript_path:
                continue
            try:
                claimed_paths.add(Path(other_session.transcript_path).expanduser().resolve())
            except OSError:
                continue

        target_time_ns = max(
            self._session_time_ns(session, "last_activity"),
            self._session_time_ns(session, "created_at"),
        )
        candidates: list[tuple[int, int, int, str]] = []

        for transcript_file in project_dir.glob("*.jsonl"):
            try:
                resolved_transcript = transcript_file.expanduser().resolve()
            except OSError:
                continue
            if resolved_transcript in claimed_paths:
                continue
            try:
                metadata = self._read_claude_transcript_metadata(str(resolved_transcript))
            except OSError:
                continue
            transcript_cwd = metadata.get("cwd")
            if transcript_cwd and transcript_cwd != resolved_working_dir:
                continue
            transcript_title = metadata.get("title")
            if expected_title and transcript_title != expected_title:
                continue
            transcript_mtime_ns = int(metadata.get("mtime_ns") or 0)
            transcript_start_ns = 0
            started_at = metadata.get("started_at")
            if isinstance(started_at, datetime):
                transcript_start_ns = int(started_at.timestamp() * 1_000_000_000)
            distance = abs(target_time_ns - (transcript_mtime_ns or transcript_start_ns))
            start_distance = abs(target_time_ns - transcript_start_ns) if transcript_start_ns else distance
            candidates.append((distance, start_distance, -transcript_mtime_ns, str(resolved_transcript)))

        if not candidates:
            return None

        candidates.sort()
        return candidates[0][3]

    def _extract_claude_native_title(self, transcript_path: str) -> tuple[Optional[str], Optional[int]]:
        """Read Claude transcript metadata and return the latest native title plus file mtime."""
        metadata = self._read_claude_transcript_metadata(transcript_path)
        return metadata.get("title"), metadata.get("mtime_ns")

    def sync_claude_native_title(self, session_or_id: Session | str | None, persist: bool = True) -> Optional[str]:
        """Synchronize one Claude session's native title from transcript metadata."""
        if session_or_id is None:
            return None
        if isinstance(session_or_id, Session):
            session = session_or_id
        else:
            session = self.sessions.get(session_or_id)
            if session is None:
                return None

        if session.provider != "claude":
            return session.native_title

        live_title = self._extract_claude_live_title(session)
        state_changed = False
        if not session.transcript_path:
            discovered_transcript_path = self._discover_claude_transcript_path(
                session,
                expected_title=live_title,
            )
            if discovered_transcript_path and session.transcript_path != discovered_transcript_path:
                session.transcript_path = discovered_transcript_path
                state_changed = True

        if not session.transcript_path:
            if live_title and live_title != session.native_title:
                session.native_title = live_title
                session.native_title_updated_at_ns = time.time_ns()
                state_changed = True
            if state_changed and persist:
                self._save_state()
            return session.native_title

        transcript_file = Path(session.transcript_path).expanduser()
        if not transcript_file.exists():
            if live_title and live_title != session.native_title:
                session.native_title = live_title
                session.native_title_updated_at_ns = time.time_ns()
                state_changed = True
            if state_changed and persist:
                self._save_state()
            return session.native_title

        try:
            current_mtime_ns = transcript_file.stat().st_mtime_ns
        except OSError:
            if live_title and live_title != session.native_title:
                session.native_title = live_title
                session.native_title_updated_at_ns = time.time_ns()
                state_changed = True
            if state_changed and persist:
                self._save_state()
            return session.native_title

        if session.native_title_source_mtime_ns == current_mtime_ns:
            if live_title and live_title != session.native_title:
                session.native_title = live_title
                session.native_title_updated_at_ns = time.time_ns()
                state_changed = True
            if state_changed and persist:
                self._save_state()
            return session.native_title

        try:
            native_title, synced_mtime_ns = self._extract_claude_native_title(session.transcript_path)
        except OSError as exc:
            logger.debug("Failed reading Claude transcript title for %s: %s", session.id, exc)
            if live_title and live_title != session.native_title:
                session.native_title = live_title
                session.native_title_updated_at_ns = time.time_ns()
                state_changed = True
            if state_changed and persist:
                self._save_state()
            return session.native_title

        effective_native_title = live_title or native_title
        title_changed = effective_native_title != session.native_title
        session.native_title = effective_native_title
        session.native_title_source_mtime_ns = synced_mtime_ns
        if title_changed:
            if live_title and live_title != native_title:
                session.native_title_updated_at_ns = time.time_ns()
            else:
                session.native_title_updated_at_ns = synced_mtime_ns
        if (state_changed or title_changed) and persist:
            self._save_state()
        return session.native_title

    def set_session_friendly_name(
        self,
        session_or_id: Session | str | None,
        friendly_name: str,
        *,
        explicit: bool = True,
        updated_at_ns: Optional[int] = None,
    ) -> bool:
        """Record one Session Manager friendly-name update with a comparable timestamp."""
        if session_or_id is None:
            return False
        if isinstance(session_or_id, Session):
            session = session_or_id
        else:
            session = self.sessions.get(session_or_id)
        if session is None:
            return False

        session.friendly_name = friendly_name
        session.friendly_name_is_explicit = explicit
        session.friendly_name_updated_at_ns = updated_at_ns or time.time_ns()
        return True

    async def queue_provider_native_rename(
        self,
        session_or_id: Session | str | None,
        friendly_name: str,
    ) -> bool:
        """Best-effort enqueue of a provider-native `/rename` for one explicit SM name."""
        if session_or_id is None:
            return False
        if isinstance(session_or_id, Session):
            session = session_or_id
        else:
            session = self.sessions.get(session_or_id)
        if session is None:
            return False
        if session.provider not in ("claude", "codex", "codex-fork") or session.status == SessionStatus.STOPPED:
            return False
        if not session.tmux_session:
            return False
        if not self._is_safe_provider_native_rename_name(friendly_name):
            return False
        if self.message_queue_manager:
            self.message_queue_manager.cancel_queued_messages_for_target(
                session.id,
                "native_rename",
            )
        if (session.native_title or "").strip() == friendly_name:
            logger.debug(
                "Skipping provider-native rename for %s; native title is already %r",
                session.id,
                friendly_name,
            )
            return True

        rename_command = f"/rename {friendly_name}"
        if not self.message_queue_manager:
            return await self._deliver_provider_native_rename(session, friendly_name)

        self.message_queue_manager.queue_message(
            target_session_id=session.id,
            text=rename_command,
            delivery_mode="sequential",
            message_category="native_rename",
        )
        return True

    async def queue_claude_native_rename(
        self,
        session_or_id: Session | str | None,
        friendly_name: str,
    ) -> bool:
        """Backward-compatible wrapper for provider-native `/rename` queueing."""
        return await self.queue_provider_native_rename(session_or_id, friendly_name)

    def extract_provider_native_rename_name(self, text: str) -> Optional[str]:
        """Extract the safe target name from a queued provider-native rename command."""
        match = re.fullmatch(r"/rename\s+([a-zA-Z0-9_-]{1,32})", (text or "").strip())
        if not match:
            return None
        friendly_name = match.group(1)
        if not self._is_safe_provider_native_rename_name(friendly_name):
            return None
        return friendly_name

    async def _deliver_provider_native_rename(self, session: Session, friendly_name: str) -> bool:
        """Deliver a provider-native rename using the provider's real TUI contract."""
        if not self._is_safe_provider_native_rename_name(friendly_name):
            return False
        if session.provider == "codex-fork":
            success, reason = await self._rename_codex_fork_thread_via_control(session, friendly_name)
            if success:
                self._clear_codex_fork_control_degraded(session)
                return True
            if not self.codex_fork_control_tmux_fallback_enabled:
                logger.warning(
                    "Codex-fork control rename failed for %s and tmux fallback is disabled: %s",
                    session.id,
                    reason,
                )
                self._set_codex_fork_control_degraded(session, reason)
                return False
            logger.warning(
                "Codex-fork control rename failed for %s, falling back to tmux dialog: %s",
                session.id,
                reason,
            )
            return await self.tmux.rename_codex_thread_async(session.tmux_session, friendly_name)
        if session.provider == "codex":
            return await self.tmux.rename_codex_thread_async(session.tmux_session, friendly_name)
        if session.provider == "claude":
            return await self._deliver_direct(session, f"/rename {friendly_name}")
        return False

    async def _rename_codex_fork_thread_via_control(
        self,
        session: Session,
        friendly_name: str,
    ) -> tuple[bool, str]:
        """Rename a codex-fork thread through its control socket without touching the TUI prompt."""
        if session.provider != "codex-fork":
            return False, "session is not codex-fork"
        if not self._is_safe_provider_native_rename_name(friendly_name):
            return False, "unsafe thread name"
        return await self._send_codex_fork_control_command(
            session=session,
            command="set_thread_name",
            payload={"name": friendly_name},
        )

    @staticmethod
    def _is_safe_provider_native_rename_name(friendly_name: str) -> bool:
        """Return True when a friendly name is safe to embed in `/rename <name>`."""
        return (
            bool(friendly_name)
            and len(friendly_name) <= 32
            and re.fullmatch(r"[a-zA-Z0-9_-]+", friendly_name) is not None
        )

    _is_safe_claude_native_rename_name = _is_safe_provider_native_rename_name

    @staticmethod
    def _session_label_sort_key(session: Session) -> tuple[int, int]:
        """Return comparable timestamps for SM-managed and provider-native labels."""
        friendly_name_updated_at_ns = int(session.friendly_name_updated_at_ns or 0)
        native_title_updated_at_ns = int(session.native_title_updated_at_ns or session.native_title_source_mtime_ns or 0)
        return friendly_name_updated_at_ns, native_title_updated_at_ns

    def get_effective_session_name(self, session_or_id: Session | str | None) -> Optional[str]:
        """Return canonical display identity for one session."""
        if session_or_id is None:
            return None
        if isinstance(session_or_id, Session):
            session = session_or_id
        else:
            session = self.sessions.get(session_or_id)
            if session is None:
                return None

        primary_alias = self.get_primary_session_alias(session.id)
        if primary_alias:
            return primary_alias
        native_title = None
        if session.provider == "claude":
            native_title = self.sync_claude_native_title(session)
        elif self._is_codex_native_title_provider(session.provider):
            native_title = session.native_title
        friendly_name_updated_at_ns, native_title_updated_at_ns = self._session_label_sort_key(session)
        if session.friendly_name and native_title:
            if friendly_name_updated_at_ns >= native_title_updated_at_ns:
                return session.friendly_name
            return native_title
        if session.friendly_name and session.friendly_name_is_explicit:
            return session.friendly_name
        if native_title:
            return native_title
        if session.friendly_name:
            return session.friendly_name
        return session.name or session.id

    def validate_friendly_name_update(self, session_id: str, friendly_name: str) -> Optional[str]:
        """Return an error when a friendly name conflicts with canonical registry identity."""
        normalized_name = self.normalize_agent_role(friendly_name)
        primary_alias = self.get_primary_session_alias(session_id)
        if primary_alias and friendly_name != primary_alias:
            return f'Session identity is controlled by registry role "{primary_alias}"'

        reserved_aliases = {"maintainer"}
        registration_map = self._get_agent_registration_map()
        self._prune_agent_registrations(persist=True)
        reserved_aliases.update(registration_map.keys())

        if normalized_name in reserved_aliases and normalized_name != primary_alias:
            return f'Name "{friendly_name}" is reserved for registry identity "{normalized_name}"'

        return None

    def list_sessions(self, include_stopped: bool = False) -> list[Session]:
        """List all sessions."""
        sessions = list(self.sessions.values())
        if not include_stopped:
            sessions = [s for s in sessions if s.status != SessionStatus.STOPPED]
        return sessions

    @staticmethod
    def detect_role_from_prompt(text: str) -> Optional[str]:
        """Best-effort role detection from initial prompt text."""
        if not text:
            return None
        snippet = text[:200].lower()
        for keyword in ROLE_KEYWORDS:
            if re.search(rf"\bas\s+{re.escape(keyword)}\b", snippet):
                return keyword
        return None

    def update_session_status(self, session_id: str, status: SessionStatus, error_message: Optional[str] = None):
        """Update a session's status."""
        session = self.sessions.get(session_id)
        if session:
            session.status = status
            session.last_activity = datetime.now()
            if error_message:
                session.error_message = error_message
            self._save_state()

    def update_telegram_thread(self, session_id: str, chat_id: int, message_id: Optional[int]):
        """Associate a Telegram thread with a session."""
        session = self.sessions.get(session_id)
        if session:
            session.telegram_chat_id = chat_id
            session.telegram_thread_id = message_id
            if message_id:
                self._upsert_telegram_topic_record(
                    session,
                    chat_id,
                    message_id,
                    revive_deleted=True,
                )
            self._save_state()

    async def send_input(
        self,
        session_id: str,
        text: str,
        sender_session_id: Optional[str] = None,
        delivery_mode: str = "sequential",
        from_sm_send: bool = False,
        timeout_seconds: Optional[int] = None,
        notify_on_delivery: bool = False,
        notify_after_seconds: Optional[int] = None,
        notify_on_stop: bool = False,
        bypass_queue: bool = False,
        remind_soft_threshold: Optional[int] = None,
        remind_hard_threshold: Optional[int] = None,
        remind_cancel_on_reply_session_id: Optional[str] = None,
        parent_session_id: Optional[str] = None,
    ) -> DeliveryResult:
        """
        Send input to a session with optional sender metadata and delivery mode.

        Args:
            session_id: Target session ID
            text: Text to send
            sender_session_id: Optional ID of sending session (for metadata)
            delivery_mode: Delivery mode (sequential, important, urgent)
            from_sm_send: True if called from sm send command (triggers notification)
            timeout_seconds: Drop message if not delivered in this time
            notify_on_delivery: Notify sender when delivered
            notify_after_seconds: Notify sender N seconds after delivery
            notify_on_stop: Notify sender when receiver's Stop hook fires
            bypass_queue: If True, send directly to tmux (for permission responses)
            remind_soft_threshold: Seconds after delivery before soft remind fires (#188)
            remind_hard_threshold: Seconds after delivery before hard remind fires (#188)
            remind_cancel_on_reply_session_id: Cancel remind when target replies to this session (#406)

        Returns:
            DeliveryResult indicating whether message was DELIVERED, QUEUED, or FAILED
        """
        session = self.sessions.get(session_id)
        if not session:
            logger.error(f"Session not found: {session_id}")
            return DeliveryResult.FAILED
        if session.status == SessionStatus.STOPPED:
            logger.error(f"Cannot send input to stopped session {session_id}")
            return DeliveryResult.FAILED

        if session.role is None:
            detected_role = self.detect_role_from_prompt(text)
            if detected_role:
                session.role = detected_role
                self._save_state()

        should_clear_completed_state = session.agent_task_completed_at is not None and sender_session_id != session_id

        def _clear_completed_state() -> None:
            if should_clear_completed_state and session.agent_task_completed_at is not None:
                session.agent_task_completed_at = None
                self._save_state()

        # For permission responses, bypass queue and send directly
        if bypass_queue:
            logger.info(f"Bypassing queue for direct send to {session_id}: {text}")
            success = await self._deliver_direct(session, text)
            if success:
                session.last_activity = datetime.now()
                _clear_completed_state()
            return DeliveryResult.DELIVERED if success else DeliveryResult.FAILED

        # Format message with sender metadata if provided
        sender_name = None
        if sender_session_id:
            sender_session = self.sessions.get(sender_session_id)
            if sender_session:
                sender_name = self.get_effective_session_name(sender_session) or sender_session_id
                formatted_text = f"[Input from: {sender_name} ({sender_session_id[:8]}) via sm send]\n{text}"
            else:
                # Sender session not found, send without metadata
                formatted_text = text
        else:
            formatted_text = text

        # Self-sends must never arm stop-notify: they are used for deferred wakeups
        # and should not immediately wake the same agent on its next stop hook.
        if notify_on_stop and sender_session_id == session_id:
            notify_on_stop = False

        # Directional notify-on-stop (#256): only EM→agent sends should enroll recipient.
        # Fail-closed: unknown sender treated as non-EM.
        if notify_on_stop and sender_session_id:
            if not sender_session or not sender_session.is_em:
                notify_on_stop = False

        # Codex-fork turn boundaries are not trustworthy as parent completion signals.
        # Require explicit sm send / task-complete instead of arming stop-notify (#400).
        if notify_on_stop and session.provider == "codex-fork":
            notify_on_stop = False

        # Send Telegram notification if from sm send
        # Note: notifier will be set by server when calling send_input
        if from_sm_send and sender_session_id and hasattr(self, 'notifier'):
            asyncio.create_task(self._notify_sm_send(
                sender_session_id=sender_session_id,
                recipient_session_id=session_id,
                text=text,
                delivery_mode=delivery_mode,
                notifier=self.notifier,
            ))

        def _record_outgoing_sm_send_target() -> None:
            if from_sm_send and sender_session_id and self.message_queue_manager:
                sender_state = self.message_queue_manager._get_or_create_state(sender_session_id)
                sender_state.last_outgoing_sm_send_target = session_id
                sender_state.last_outgoing_sm_send_at = datetime.now()

        # Handle steer delivery mode — direct Enter-based injection, bypasses queue
        if delivery_mode == "steer":
            if session.provider not in ("codex", "codex-fork"):
                logger.error(f"Steer delivery only supported for Codex CLI sessions, not {session.provider}")
                return DeliveryResult.FAILED
            success = await self.tmux.send_steer_text(session.tmux_session, text)
            if success:
                _clear_completed_state()
            return DeliveryResult.DELIVERED if success else DeliveryResult.FAILED

        # Handle delivery modes using the message queue manager
        if self.message_queue_manager:
            # For sequential mode, queue first, then report whether the immediate
            # delivery attempt actually injected the message.
            if delivery_mode == "sequential":
                if session.provider == "claude" and session.status == SessionStatus.STOPPED:
                    return DeliveryResult.FAILED
                msg = self.message_queue_manager.queue_message(
                    target_session_id=session_id,
                    text=formatted_text,
                    sender_session_id=sender_session_id,
                    sender_name=sender_name,
                    delivery_mode=delivery_mode,
                    from_sm_send=from_sm_send,
                    timeout_seconds=timeout_seconds,
                    notify_on_delivery=notify_on_delivery,
                    notify_after_seconds=notify_after_seconds,
                    notify_on_stop=notify_on_stop,
                    remind_soft_threshold=remind_soft_threshold,
                    remind_hard_threshold=remind_hard_threshold,
                    remind_cancel_on_reply_session_id=remind_cancel_on_reply_session_id,
                    parent_session_id=parent_session_id,
                    trigger_delivery=False,
                )
                # Record outgoing sm send for deferred stop notification suppression (#182)
                # Placed after queue_message to ensure message was persisted first.
                _record_outgoing_sm_send_target()
                delivered = await self._deliver_queued_message_with_deadline(
                    session_id=session_id,
                    message_id=msg.id,
                    delivery_mode=delivery_mode,
                )
                _clear_completed_state()
                return DeliveryResult.DELIVERED if delivered else DeliveryResult.QUEUED

            # For important, queue first, then report the real immediate-delivery outcome.
            if delivery_mode == "important":
                if session.provider == "claude" and session.status == SessionStatus.STOPPED:
                    return DeliveryResult.FAILED
                msg = self.message_queue_manager.queue_message(
                    target_session_id=session_id,
                    text=formatted_text,
                    sender_session_id=sender_session_id,
                    sender_name=sender_name,
                    delivery_mode=delivery_mode,
                    from_sm_send=from_sm_send,
                    timeout_seconds=timeout_seconds,
                    notify_on_delivery=notify_on_delivery,
                    notify_after_seconds=notify_after_seconds,
                    notify_on_stop=notify_on_stop,
                    remind_soft_threshold=remind_soft_threshold,
                    remind_hard_threshold=remind_hard_threshold,
                    remind_cancel_on_reply_session_id=remind_cancel_on_reply_session_id,
                    parent_session_id=parent_session_id,
                    trigger_delivery=False,
                )
                # Record outgoing sm send for deferred stop notification suppression (#182)
                _record_outgoing_sm_send_target()
                delivered = await self._deliver_queued_message_with_deadline(
                    session_id=session_id,
                    message_id=msg.id,
                    delivery_mode=delivery_mode,
                )
                _clear_completed_state()
                return DeliveryResult.DELIVERED if delivered else DeliveryResult.QUEUED

            # Urgent always delivers (sends Escape first).
            if delivery_mode == "urgent":
                self.message_queue_manager.queue_message(
                    target_session_id=session_id,
                    text=formatted_text,
                    sender_session_id=sender_session_id,
                    sender_name=sender_name,
                    delivery_mode=delivery_mode,
                    from_sm_send=from_sm_send,
                    timeout_seconds=timeout_seconds,
                    notify_on_delivery=notify_on_delivery,
                    notify_after_seconds=notify_after_seconds,
                    notify_on_stop=notify_on_stop,
                    remind_soft_threshold=remind_soft_threshold,
                    remind_hard_threshold=remind_hard_threshold,
                    remind_cancel_on_reply_session_id=remind_cancel_on_reply_session_id,
                    parent_session_id=parent_session_id,
                )
                _record_outgoing_sm_send_target()
                _clear_completed_state()
                return DeliveryResult.DELIVERED

        # Fallback: send immediately (no queue manager or unknown mode)
        success = await self._deliver_direct(session, formatted_text)
        if success:
            session.last_activity = datetime.now()
            session.status = SessionStatus.RUNNING
            _clear_completed_state()
            self._save_state()

        return DeliveryResult.DELIVERED if success else DeliveryResult.FAILED

    async def _deliver_queued_message_with_deadline(
        self,
        session_id: str,
        message_id: str,
        delivery_mode: str,
    ) -> bool:
        """Attempt immediate queued delivery without letting control APIs wait on slow IO."""
        if not self.message_queue_manager:
            return False

        delivery_coro = self.message_queue_manager.deliver_queued_message_now(
            session_id=session_id,
            message_id=message_id,
            delivery_mode=delivery_mode,
        )
        task = asyncio.ensure_future(delivery_coro)
        wait_seconds = max(0.0, getattr(self, "input_delivery_wait_seconds", 1.0))
        if wait_seconds > 0:
            done, _ = await asyncio.wait({task}, timeout=wait_seconds)
            if task in done:
                return bool(task.result())

        def _log_delivery_failure(completed: asyncio.Task[Any]) -> None:
            try:
                completed.result()
            except asyncio.CancelledError:
                logger.warning(
                    "Background queued delivery was cancelled for %s message %s",
                    session_id,
                    message_id,
                )
            except Exception:
                logger.exception(
                    "Background queued delivery failed for %s message %s",
                    session_id,
                    message_id,
                )

        task.add_done_callback(_log_delivery_failure)
        logger.info(
            "Queued delivery for %s message %s exceeded %.2fs control wait; continuing in background",
            session_id,
            message_id,
            wait_seconds,
        )
        return False

    async def _notify_sm_send(
        self,
        sender_session_id: str,
        recipient_session_id: str,
        text: str,
        delivery_mode: str,
        notifier=None,
    ):
        """
        Send Telegram notification about sm send message.

        Args:
            sender_session_id: Sender session ID
            recipient_session_id: Recipient session ID
            text: Message text
            delivery_mode: Delivery mode (sequential, important, urgent)
            notifier: Notifier instance (passed from server)
        """
        recipient_session = self.sessions.get(recipient_session_id)
        sender_session = self.sessions.get(sender_session_id)

        if not recipient_session or not sender_session:
            return

        # Only notify if recipient has Telegram configured
        if not recipient_session.telegram_chat_id:
            return

        # Need notifier to send Telegram messages
        if not notifier:
            logger.warning(f"No notifier available for sm_send notification")
            return

        # Get sender friendly name
        sender_name = self.get_effective_session_name(sender_session) or sender_session_id

        # Format delivery mode with icon
        mode_icons = {
            "sequential": "📨",
            "important": "❗",
            "urgent": "⚡",
        }
        icon = mode_icons.get(delivery_mode, "📨")

        # Format notification message
        notification_text = f"{icon} **From [{sender_name}]** ({delivery_mode}): {text}"

        # Send notification via notifier
        from .models import NotificationEvent
        event = NotificationEvent(
            session_id=recipient_session_id,
            event_type="sm_send",
            message=notification_text,
            context="",
            urgent=False,
        )

        await notifier.notify(event, recipient_session)

    def set_hook_output_store(self, store: dict):
        """Attach hook output store (used to cache last responses)."""
        self.hook_output_store = store

    async def start_background_tasks(self):
        """Start periodic maintenance tasks owned by SessionManager."""
        await self.codex_observability_logger.start_periodic_prune()
        await self.queue_runner.start()
        await self.maintain_codex_fork_runtime_artifacts()
        for session in self.sessions.values():
            if session.provider == "codex-fork" and session.status != SessionStatus.STOPPED:
                self._start_codex_fork_event_monitor(session, from_eof=True)
        if self._service_role_maintenance_task is None:
            self._service_role_maintenance_task = asyncio.create_task(
                self._run_service_role_maintenance_loop()
            )
        if self._codex_fork_runtime_maintenance_task is None:
            self._codex_fork_runtime_maintenance_task = asyncio.create_task(
                self._run_codex_fork_runtime_maintenance_loop()
            )

    async def stop_background_tasks(self):
        """Stop periodic maintenance tasks owned by SessionManager."""
        await self.codex_observability_logger.stop_periodic_prune()
        await self.queue_runner.stop()
        if self._service_role_maintenance_task is not None:
            self._service_role_maintenance_task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._service_role_maintenance_task
            self._service_role_maintenance_task = None
        if self._codex_fork_runtime_maintenance_task is not None:
            self._codex_fork_runtime_maintenance_task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._codex_fork_runtime_maintenance_task
            self._codex_fork_runtime_maintenance_task = None
        for session_id in list(self.codex_fork_event_monitors.keys()):
            await self._stop_codex_fork_event_monitor(session_id)
        pending_topic_tasks = list(self._pending_telegram_topic_tasks)
        for task in pending_topic_tasks:
            task.cancel()
        for task in pending_topic_tasks:
            with contextlib.suppress(asyncio.CancelledError):
                await task

    def reap_completed_auto_bootstrapped_service_sessions(
        self,
        now: Optional[datetime] = None,
    ) -> list[str]:
        """Kill completed auto-bootstrapped service sessions that exceeded their task-complete TTL."""
        current_time = now or datetime.now()
        killed_session_ids: list[str] = []

        for session in list(self.sessions.values()):
            if session.status == SessionStatus.STOPPED:
                continue
            if not session.auto_bootstrapped_role:
                continue
            if session.role != session.auto_bootstrapped_role:
                continue
            if session.agent_task_completed_at is None:
                continue

            spec = self.get_service_role_bootstrap_spec(session.auto_bootstrapped_role)
            if not spec or not spec.get("auto_bootstrap"):
                continue
            ttl_seconds = spec.get("task_complete_ttl_seconds")
            if not isinstance(ttl_seconds, int) or ttl_seconds <= 0:
                continue

            completed_age_seconds = (current_time - session.agent_task_completed_at).total_seconds()
            if completed_age_seconds < ttl_seconds:
                continue

            if self.kill_session(session.id):
                killed_session_ids.append(session.id)
                logger.info(
                    "Auto-retired completed auto-bootstrapped service session %s for role %s after %.0fs",
                    session.id,
                    session.role,
                    completed_age_seconds,
                )

        return killed_session_ids

    async def _run_service_role_maintenance_loop(self) -> None:
        """Periodically retire completed auto-bootstrapped service sessions."""
        try:
            while True:
                try:
                    self.reap_completed_auto_bootstrapped_service_sessions()
                except Exception:
                    logger.exception("Service role maintenance pass failed")
                await asyncio.sleep(self.service_role_maintenance_poll_interval_seconds)
        except asyncio.CancelledError:
            raise

    def prune_codex_fork_runtime_artifacts(self) -> list[str]:
        """Remove codex-fork event/control artifacts that no longer belong to a live runtime."""
        removed: list[str] = []
        for artifact_path in self._iter_codex_fork_runtime_artifacts():
            session_id = self._codex_fork_session_id_from_artifact_name(artifact_path.name)
            if not session_id:
                continue
            session = self.sessions.get(session_id)
            should_remove = False
            if session is None:
                should_remove = True
            elif session.provider != "codex-fork":
                should_remove = True
            elif session.status == SessionStatus.STOPPED:
                should_remove = True
            elif not session.tmux_session or not self.tmux.session_exists(session.tmux_session):
                should_remove = True

            if not should_remove:
                continue

            with contextlib.suppress(FileNotFoundError):
                artifact_path.unlink()
                removed.append(artifact_path.name)

        return removed

    async def _restart_codex_fork_runtime(
        self,
        session: Session,
        *,
        reason: str,
    ) -> tuple[bool, str]:
        """Recreate one codex-fork runtime in place to restore missing bridge artifacts."""
        if session.provider != "codex-fork":
            return False, "session is not codex-fork"

        resume_id = self.get_session_resume_id(session)
        if not resume_id:
            return False, "no Codex resume id is available for this session"

        await self._stop_codex_fork_event_monitor(session.id)
        self.codex_fork_event_offsets.pop(session.id, None)
        self.codex_fork_event_buffers.pop(session.id, None)
        self.codex_fork_turns_in_flight.discard(session.id)
        self.codex_fork_wait_resume_state.pop(session.id, None)
        self.codex_fork_wait_kind.pop(session.id, None)
        self.codex_fork_last_seq.pop(session.id, None)
        self.codex_fork_session_epoch.pop(session.id, None)
        self.codex_fork_control_epoch.pop(session.id, None)
        self.codex_fork_control_degraded.pop(session.id, None)

        if session.tmux_session and self.tmux.session_exists(session.tmux_session):
            self.tmux.kill_session(session.tmux_session)

        command, args, effective_provider, fallback_reason = self._build_codex_fork_launch_spec(
            session,
            resume_id=resume_id,
        )
        if effective_provider != session.provider:
            logger.warning(
                "Falling back from codex-fork to codex while recreating %s: %s",
                session.id,
                fallback_reason,
            )
            session.provider = effective_provider

        if not self.tmux.create_session_with_command(
            session.tmux_session,
            session.working_dir,
            session.log_file,
            session_id=session.id,
            command=command,
            args=args,
        ):
            tmux_error = getattr(self.tmux, "last_error_message", None)
            session.error_message = f"codex_fork_runtime_artifacts_missing: {reason}"
            if tmux_error:
                session.error_message = f"{session.error_message} ({tmux_error})"
            self._save_state()
            return False, tmux_error or "failed to recreate Codex session runtime"

        session.error_message = None
        session.status = SessionStatus.RUNNING
        session.tmux_socket_name = self._tmux_socket_name()
        session.last_activity = datetime.now()
        if session.provider == "codex-fork":
            self.codex_fork_runtime_owner[session.id] = session.parent_session_id or session.id
            self._set_codex_fork_lifecycle_state(
                session_id=session.id,
                state="running",
                cause_event_type="runtime_artifacts_restored",
            )
            self._start_codex_fork_event_monitor(session)
        else:
            self.codex_fork_runtime_owner.pop(session.id, None)
        self._save_state()
        logger.info("Recreated codex-fork runtime artifacts for %s after %s", session.id, reason)
        return True, ""

    async def maintain_codex_fork_runtime_artifacts(self) -> dict[str, list[str]]:
        """Prune dead codex-fork artifacts and recreate missing live bridge artifacts when possible."""
        removed = self.prune_codex_fork_runtime_artifacts()
        healed: list[str] = []
        degraded: list[str] = []

        for session in list(self.sessions.values()):
            if session.provider != "codex-fork" or session.status == SessionStatus.STOPPED:
                continue
            if not session.tmux_session or not self.tmux.session_exists(session.tmux_session):
                continue

            missing: list[str] = []
            event_stream_path = self._codex_fork_event_stream_path(session)
            control_socket_path = self._codex_fork_control_socket_path(session)
            if not event_stream_path.exists():
                missing.append("event_stream")
            if not control_socket_path.exists():
                missing.append("control_socket")

            if not missing:
                if session.id not in self.codex_fork_event_monitors:
                    self._start_codex_fork_event_monitor(session, from_eof=True)
                if session.error_message and session.error_message.startswith("codex_fork_runtime_artifacts_missing:"):
                    session.error_message = None
                    self._save_state()
                continue

            reason = ", ".join(missing)
            if missing == ["control_socket"]:
                self._set_codex_fork_control_degraded(
                    session,
                    f"control socket missing: {control_socket_path}",
                )
                degraded.append(session.id)
                continue

            ok, error = await self._restart_codex_fork_runtime(session, reason=reason)
            if ok:
                healed.append(session.id)
            else:
                if not session.error_message or not session.error_message.startswith(
                    "codex_fork_runtime_artifacts_missing:"
                ):
                    session.error_message = (
                        f"codex_fork_runtime_artifacts_missing: {reason}"
                        + (f" ({error})" if error else "")
                    )
                    self._save_state()
                degraded.append(session.id)

        return {"removed": removed, "healed": healed, "degraded": degraded}

    async def _run_codex_fork_runtime_maintenance_loop(self) -> None:
        """Periodically heal missing codex-fork bridge artifacts and prune dead ones."""
        try:
            while True:
                try:
                    await self.maintain_codex_fork_runtime_artifacts()
                except Exception:
                    logger.exception("Codex-fork runtime maintenance pass failed")
                await asyncio.sleep(self.codex_fork_runtime_maintenance_poll_interval_seconds)
        except asyncio.CancelledError:
            raise

    async def _ensure_codex_session(self, session: Session, model: Optional[str] = None) -> Optional[CodexAppServerSession]:
        """Ensure a Codex app-server session is running for this session."""
        existing = self.codex_sessions.get(session.id)
        if existing:
            return existing

        try:
            codex_session = CodexAppServerSession(
                session_id=session.id,
                working_dir=session.working_dir,
                config=self.codex_config,
                on_turn_complete=self._handle_codex_turn_complete,
                on_turn_started=self._handle_codex_turn_started,
                on_turn_delta=self._handle_codex_turn_delta,
                on_review_complete=self._handle_codex_review_complete,
                on_server_request=self._handle_codex_server_request,
                on_item_notification=self._handle_codex_item_notification,
                on_stream_error=self._handle_codex_stream_error,
            )
            thread_id = await codex_session.start(thread_id=session.codex_thread_id, model=model)
            session.codex_thread_id = thread_id
            self.codex_sessions[session.id] = codex_session
            self._save_state()
            return codex_session
        except Exception as e:
            logger.error(f"Failed to ensure Codex session for {session.id}: {e}")
            return None

    async def _codex_fork_control_roundtrip(self, socket_path: Path, request: dict[str, Any]) -> dict[str, Any]:
        timeout = max(0.5, self.codex_fork_control_timeout_seconds)
        reader, writer = await asyncio.wait_for(
            asyncio.open_unix_connection(str(socket_path)),
            timeout=timeout,
        )
        try:
            writer.write((json.dumps(request, separators=(",", ":")) + "\n").encode("utf-8"))
            await asyncio.wait_for(writer.drain(), timeout=timeout)
            raw_response = await asyncio.wait_for(reader.readline(), timeout=timeout)
            if not raw_response:
                raise RuntimeError("control socket closed without response")
            try:
                return json.loads(raw_response.decode("utf-8"))
            except json.JSONDecodeError as exc:
                raise RuntimeError(f"control socket returned invalid JSON: {exc}") from exc
        finally:
            writer.close()
            with contextlib.suppress(Exception):
                await writer.wait_closed()

    async def _refresh_codex_fork_control_epoch(self, session: Session, socket_path: Path) -> tuple[bool, str]:
        request = {
            "request_id": uuid.uuid4().hex,
            "command": "get_epoch",
        }
        try:
            response = await self._codex_fork_control_roundtrip(socket_path, request)
        except Exception as exc:
            return False, f"failed to read control epoch: {exc}"
        if not response.get("ok"):
            error = response.get("error") if isinstance(response.get("error"), dict) else {}
            code = error.get("code", "unknown_error")
            message = error.get("message", "failed to fetch epoch")
            return False, f"{code}: {message}"
        result = response.get("result") if isinstance(response.get("result"), dict) else {}
        epoch = result.get("epoch")
        if not isinstance(epoch, str) or not epoch:
            fallback_epoch = response.get("epoch")
            if isinstance(fallback_epoch, str) and fallback_epoch:
                epoch = fallback_epoch
        if not isinstance(epoch, str) or not epoch:
            return False, "control epoch missing from response"
        self.codex_fork_control_epoch[session.id] = epoch
        return True, epoch

    async def _send_codex_fork_control_command(
        self,
        session: Session,
        command: str,
        payload: dict[str, Any],
    ) -> tuple[bool, str]:
        socket_path = self._codex_fork_control_socket_path(session)
        if not socket_path.exists():
            return False, f"control socket not found: {socket_path}"

        epoch = self.codex_fork_control_epoch.get(session.id)
        if not epoch:
            ok, epoch_or_error = await self._refresh_codex_fork_control_epoch(session, socket_path)
            if not ok:
                return False, epoch_or_error
            epoch = epoch_or_error

        async def send_with_epoch(expected_epoch: str) -> dict[str, Any]:
            request = {
                "request_id": uuid.uuid4().hex,
                "expected_epoch": expected_epoch,
                "command": command,
                **payload,
            }
            return await self._codex_fork_control_roundtrip(socket_path, request)

        try:
            response = await send_with_epoch(epoch)
        except Exception as exc:
            return False, f"control command failed: {exc}"

        if not response.get("ok"):
            error = response.get("error") if isinstance(response.get("error"), dict) else {}
            error_code = error.get("code", "unknown_error")
            if error_code == "stale_epoch":
                ok, epoch_or_error = await self._refresh_codex_fork_control_epoch(session, socket_path)
                if not ok:
                    return False, epoch_or_error
                try:
                    response = await send_with_epoch(epoch_or_error)
                except Exception as exc:
                    return False, f"control command failed after epoch refresh: {exc}"
            if not response.get("ok"):
                error = response.get("error") if isinstance(response.get("error"), dict) else {}
                code = error.get("code", "unknown_error")
                message = error.get("message", "control command failed")
                return False, f"{code}: {message}"

        response_epoch = response.get("epoch")
        if isinstance(response_epoch, str) and response_epoch:
            self.codex_fork_control_epoch[session.id] = response_epoch
        return True, ""

    def _set_codex_fork_control_degraded(self, session: Session, reason: str) -> None:
        normalized_reason = reason.strip() or "unknown_control_error"
        previous = self.codex_fork_control_degraded.get(session.id)
        self.codex_fork_control_degraded[session.id] = normalized_reason
        session.error_message = f"codex_fork_control_degraded: {normalized_reason}"
        if previous != normalized_reason:
            self.codex_event_store.append_event(
                session_id=session.id,
                event_type="codex_fork_control_degraded",
                payload={"reason": normalized_reason},
            )
        self._save_state()

    def _clear_codex_fork_control_degraded(self, session: Session) -> None:
        had_runtime_degraded = self.codex_fork_control_degraded.pop(session.id, None) is not None
        had_persisted_degraded_error = bool(
            session.error_message and session.error_message.startswith("codex_fork_control_degraded:")
        )
        if had_persisted_degraded_error:
            session.error_message = None
        if not had_runtime_degraded and not had_persisted_degraded_error:
            return
        self.codex_event_store.append_event(
            session_id=session.id,
            event_type="codex_fork_control_restored",
            payload={},
        )
        self._save_state()

    def _format_tmux_runtime_missing_message(
        self,
        session: Session,
        diagnostics: Optional[dict[str, object]] = None,
    ) -> str:
        tmux_name = session.tmux_session or session.name or session.id
        if diagnostics and diagnostics.get("pane_dead"):
            status = diagnostics.get("pane_dead_status")
            command = diagnostics.get("pane_current_command")
            details = []
            if status is not None:
                details.append(f"exit_status={status}")
            if command:
                details.append(f"command={command}")
            suffix = "; ".join(details) if details else "dead pane detected"
            return f"Tmux session {tmux_name} exited before delivery ({suffix})"
        return f"Tmux session {tmux_name} disappeared before delivery"

    async def _mark_tmux_runtime_missing_if_absent(self, session: Session) -> bool:
        """Mark a tmux-backed session stopped when delivery proves the runtime is gone."""
        if session.provider == "codex-app" or not session.tmux_session:
            return False
        if self.tmux.session_exists(session.tmux_session):
            return False

        diagnostics = None
        get_exit_diagnostics = getattr(self.tmux, "get_session_exit_diagnostics", None)
        if callable(get_exit_diagnostics):
            diagnostics = get_exit_diagnostics(session.tmux_session)

        session.status = SessionStatus.STOPPED
        session.error_message = self._format_tmux_runtime_missing_message(session, diagnostics)
        logger.warning(
            "Marked session %s stopped after missing tmux runtime during delivery: %s; diagnostics=%s",
            session.id,
            session.error_message,
            diagnostics,
        )
        cleanup_session = (
            getattr(self.output_monitor, "cleanup_session", None)
            if self.output_monitor
            else None
        )
        if callable(cleanup_session):
            await cleanup_session(session, preserve_record=True)
        else:
            self._save_state()
        return True

    async def _deliver_direct(self, session: Session, text: str, model: Optional[str] = None) -> bool:
        """Deliver a message directly to a session (no queue)."""
        if session.provider == "codex-app":
            codex_session = await self._ensure_codex_session(session, model=model)
            if not codex_session:
                return False
            try:
                await codex_session.send_user_turn(text, model=model)
                session.status = SessionStatus.RUNNING
                session.last_activity = datetime.now()
                self._save_state()
                if self.message_queue_manager:
                    self.message_queue_manager.mark_session_active(session.id)
                return True
            except Exception as e:
                logger.error(f"Codex app send failed for {session.id}: {e}")
                return False

        if session.provider == "codex-fork":
            success, reason = await self._send_codex_fork_control_command(
                session=session,
                command="submit_message",
                payload={"message": text},
            )
            if success:
                self._clear_codex_fork_control_degraded(session)
                session.status = SessionStatus.RUNNING
                session.last_activity = datetime.now()
                self._save_state()
                if self.message_queue_manager:
                    self.message_queue_manager.mark_session_active(session.id)
                return True

            logger.warning("Codex-fork control send failed for %s: %s", session.id, reason)
            self._set_codex_fork_control_degraded(session, reason)
            if not self.codex_fork_control_tmux_fallback_enabled:
                await self._mark_tmux_runtime_missing_if_absent(session)
                return False

            logger.warning("Falling back to tmux input path for codex-fork session %s", session.id)
            fallback_success = await self.tmux.send_input_async(
                session.tmux_session,
                text,
                verify_codex_submit=True,
            )
            if fallback_success and self.message_queue_manager:
                self.message_queue_manager.mark_session_active(session.id)
            elif not fallback_success:
                await self._mark_tmux_runtime_missing_if_absent(session)
            return fallback_success

        success = await self.tmux.send_input_async(
            session.tmux_session,
            text,
            verify_claude_submit=(session.provider == "claude"),
            verify_codex_submit=(session.provider == "codex"),
        )
        if success and self.message_queue_manager:
            self.message_queue_manager.mark_session_active(session.id)
        elif not success:
            await self._mark_tmux_runtime_missing_if_absent(session)
        return success

    async def _interrupt_codex(self, session: Session) -> bool:
        """Interrupt a Codex turn if one is in progress."""
        codex_session = await self._ensure_codex_session(session)
        if not codex_session:
            return False
        return await codex_session.interrupt_turn()

    async def _deliver_urgent(self, session: Session, text: str) -> bool:
        """Deliver an urgent message (interrupt if possible)."""
        if session.provider == "codex-app":
            codex_session = await self._ensure_codex_session(session)
            if not codex_session:
                return False
            await codex_session.interrupt_turn()
            return await self._deliver_direct(session, text)

        # Claude (tmux) urgent delivery handled in message queue directly
        return await self._deliver_direct(session, text)

    async def _handle_codex_turn_complete(self, session_id: str, text: str, status: str):
        """Handle Codex app-server turn completion."""
        session = self.sessions.get(session_id)
        if not session:
            return

        self.codex_turns_in_flight.discard(session_id)
        turn_id = self.codex_active_turn_ids.pop(session_id, None)
        self.codex_wait_states.pop(session_id, None)
        self.codex_last_delta_at.pop(session_id, None)

        self.codex_event_store.append_event(
            session_id=session_id,
            event_type="turn_completed",
            turn_id=turn_id,
            payload={
                "status": status,
                "output_preview": text[:400] if text else "",
                "output_chars": len(text or ""),
            },
        )
        self._safe_log_codex_turn_event(
            session_id=session_id,
            thread_id=self._thread_id_for_session(session_id),
            turn_id=turn_id,
            event_type="turn_completed",
            status=status,
            output_preview=text[:400] if text else "",
            raw_payload={"status": status, "output_chars": len(text or "")},
        )

        # Store last output (for /status, /last-message)
        if text and self.hook_output_store is not None:
            self.hook_output_store["latest"] = text
            self.hook_output_store[session_id] = text

        # Update session status and activity
        session.last_activity = datetime.now()
        session.status = SessionStatus.IDLE  # Session stopped, waiting for input
        self._save_state()

        # Mark idle for message queue delivery
        if self.message_queue_manager:
            self.message_queue_manager.mark_session_idle(
                session_id,
                completion_transition=True,
            )

        # Send notification similar to Claude Stop hook
        if text and hasattr(self, "notifier") and self.notifier:
            if session.telegram_chat_id:
                event = NotificationEvent(
                    session_id=session.id,
                    event_type="response",
                    message="Codex responded",
                    context=text,
                    urgent=False,
                )
                await self.notifier.notify(event, session)

                # If session has a review_config, emit review_complete
                if session.review_config:
                    try:
                        review_config = session.review_config
                        review_result = None
                        if review_config.mode == "pr" and review_config.pr_repo and review_config.pr_number:
                            from .github_reviews import fetch_latest_codex_review
                            from .review_parser import parse_github_review
                            codex_review = fetch_latest_codex_review(
                                review_config.pr_repo, review_config.pr_number
                            )
                            if codex_review:
                                review_result = parse_github_review(
                                    review_config.pr_repo,
                                    review_config.pr_number,
                                    codex_review,
                                )
                        else:
                            from .review_parser import parse_tui_output
                            review_result = parse_tui_output(text)
                        if review_result and review_result.findings:
                            review_event = NotificationEvent(
                                session_id=session.id,
                                event_type="review_complete",
                                message="Review complete",
                                context="",
                                urgent=False,
                            )
                            review_event.review_result = review_result
                            await self.notifier.notify(review_event, session)
                    except Exception as e:
                        logger.warning(f"Failed to emit review_complete: {e}")

    async def _handle_codex_turn_started(self, session_id: str, turn_id: str):
        """Mark Codex turn as active and update activity timestamps."""
        self.codex_turns_in_flight.add(session_id)
        self.codex_active_turn_ids[session_id] = turn_id
        self.codex_wait_states.pop(session_id, None)
        self.codex_last_delta_at.pop(session_id, None)
        self.codex_event_store.append_event(
            session_id=session_id,
            event_type="turn_started",
            turn_id=turn_id,
            payload={},
        )
        self._safe_log_codex_turn_event(
            session_id=session_id,
            thread_id=self._thread_id_for_session(session_id),
            turn_id=turn_id,
            event_type="turn_started",
            status="running",
            raw_payload={},
        )
        session = self.sessions.get(session_id)
        if not session:
            return
        session.status = SessionStatus.RUNNING
        session.last_activity = datetime.now()
        # Save on turn start (lower frequency)
        self._save_state()
        if self.message_queue_manager:
            self.message_queue_manager.mark_session_active(session_id)

    async def _handle_codex_turn_delta(self, session_id: str, turn_id: str, delta: str):
        """Update activity on Codex streaming deltas."""
        self.codex_last_delta_at[session_id] = datetime.now()
        self.codex_wait_states.pop(session_id, None)
        self.codex_event_store.append_event(
            session_id=session_id,
            event_type="turn_delta",
            turn_id=turn_id,
            payload={
                "delta_preview": delta[:240],
                "delta_chars": len(delta),
            },
        )
        self._safe_log_codex_turn_event(
            session_id=session_id,
            thread_id=self._thread_id_for_session(session_id),
            turn_id=turn_id,
            event_type="turn_delta",
            status="running",
            delta_chars=len(delta),
            output_preview=delta[:240],
            raw_payload={"delta_chars": len(delta)},
        )
        session = self.sessions.get(session_id)
        if not session:
            return
        session.last_activity = datetime.now()
        if self.message_queue_manager:
            self.message_queue_manager.mark_session_active(session_id)

    async def _handle_codex_review_complete(self, session_id: str, review_text: str):
        """Handle Codex app-server review completion (exitedReviewMode)."""
        session = self.sessions.get(session_id)
        if not session:
            return

        session.last_activity = datetime.now()
        session.status = SessionStatus.IDLE
        self._save_state()

        if self.message_queue_manager:
            self.message_queue_manager.mark_session_idle(
                session_id,
                completion_transition=True,
            )

        # Store review output
        if review_text and self.hook_output_store is not None:
            self.hook_output_store["latest"] = review_text
            self.hook_output_store[session_id] = review_text

        self.codex_event_store.append_event(
            session_id=session_id,
            event_type="review_completed",
            turn_id=None,
            payload={
                "output_preview": review_text[:400] if review_text else "",
                "output_chars": len(review_text or ""),
            },
        )

        # Emit review_complete notification
        if review_text and session.review_config and hasattr(self, "notifier") and self.notifier:
            try:
                from .review_parser import parse_app_server_output
                review_result = parse_app_server_output(review_text)
                if review_result and review_result.findings:
                    review_event = NotificationEvent(
                        session_id=session.id,
                        event_type="review_complete",
                        message="Review complete",
                        context="",
                        urgent=False,
                    )
                    review_event.review_result = review_result
                    await self.notifier.notify(review_event, session)
            except Exception as e:
                logger.warning(f"Failed to emit review_complete for codex-app: {e}")

    def is_codex_turn_active(self, session_id: str) -> bool:
        """Check if a Codex turn is currently in flight."""
        return session_id in self.codex_turns_in_flight

    async def _handle_codex_server_request(
        self,
        session_id: str,
        request_id: int,
        method: str,
        params: dict,
    ) -> Optional[dict]:
        """Track codex-app server requests as lifecycle events for observability/activity state."""
        now = datetime.now()
        state_name = None
        event_type = "server_request"
        request_type = "server_request"
        policy_payload: Optional[dict] = None
        if method in ("item/commandExecution/requestApproval", "item/fileChange/requestApproval"):
            state_name = "waiting_permission"
            event_type = "request_approval"
            request_type = "request_approval"
            policy_payload = {"decision": "decline"}
        elif method == "item/tool/requestUserInput":
            state_name = "waiting_input"
            event_type = "request_user_input"
            request_type = "request_user_input"
            policy_payload = {"answers": {}}

        if state_name:
            self.codex_wait_states[session_id] = (state_name, now)

        codex_session = self.codex_sessions.get(session_id)
        thread_id = codex_session.thread_id if codex_session else None
        item = params.get("item", {}) if isinstance(params.get("item"), dict) else {}
        turn_id = params.get("turnId")
        item_id = item.get("id")

        if policy_payload is not None:
            pending = await self.codex_request_ledger.register_request(
                session_id=session_id,
                rpc_request_id=request_id,
                request_method=method,
                request_payload=params,
                thread_id=thread_id,
                turn_id=turn_id,
                item_id=item_id,
                request_type=request_type,
                timeout_seconds=self.codex_config.request_timeout_seconds,
                policy_payload=policy_payload,
            )
            request_ledger_id = pending["request_id"]
            item_type = "commandExecution" if "commandExecution" in method else "fileChange"
            if method == "item/tool/requestUserInput":
                item_type = "tool"
            self._safe_log_codex_tool_event(
                session_id=session_id,
                thread_id=thread_id,
                turn_id=turn_id,
                item_id=item_id,
                request_id=request_ledger_id,
                event_type=event_type,
                item_type=item_type,
                phase="pre",
                raw_payload=params,
            )
        else:
            request_ledger_id = None

        self.codex_event_store.append_event(
            session_id=session_id,
            event_type=event_type,
            turn_id=params.get("turnId"),
            payload={
                "request_id": request_id,
                "ledger_request_id": request_ledger_id,
                "method": method,
            },
        )

        if request_ledger_id:
            resolved = await self.codex_request_ledger.wait_for_resolution(request_ledger_id)
            self.codex_wait_states.pop(session_id, None)
            return resolved

        return None

    async def _handle_codex_item_notification(self, session_id: str, method: str, params: dict[str, Any]):
        """Ingest codex item lifecycle notifications into observability storage."""
        item = params.get("item", {}) if isinstance(params.get("item"), dict) else {}
        item_id = item.get("id")
        turn_id = params.get("turnId")
        thread_id = self._thread_id_for_session(session_id)
        now = datetime.now()

        if method == "item/started":
            item_type = item.get("type")
            if item_type in ("commandExecution", "fileChange", "tool"):
                if item_id:
                    self._codex_item_started_at[(session_id, item_id)] = now
                self._safe_log_codex_tool_event(
                    session_id=session_id,
                    thread_id=thread_id,
                    turn_id=turn_id,
                    item_id=item_id,
                    event_type="started",
                    item_type=item_type,
                    phase="running",
                    command=item.get("command"),
                    cwd=item.get("cwd"),
                    file_path=item.get("filePath") or item.get("path"),
                    diff_summary=item.get("diffSummary") or item.get("summary"),
                    raw_payload=params,
                )
            return

        if method in ("item/commandExecution/outputDelta", "item/fileChange/outputDelta"):
            item_type = "commandExecution" if "commandExecution" in method else "fileChange"
            delta = params.get("delta")
            delta_summary = str(delta)[:240] if delta is not None else None
            self._safe_log_codex_tool_event(
                session_id=session_id,
                thread_id=thread_id,
                turn_id=turn_id,
                item_id=item_id,
                event_type="output_delta",
                item_type=item_type,
                phase="running",
                diff_summary=delta_summary,
                raw_payload=params,
            )
            return

        if method == "item/completed":
            item_type = item.get("type")
            if item_type not in ("commandExecution", "fileChange", "tool"):
                return
            status = str(item.get("status", "completed")).lower()
            event_type_map = {
                "completed": "completed",
                "failed": "failed",
                "interrupted": "interrupted",
                "cancelled": "cancelled",
                "timeout": "timeout",
            }
            event_type = event_type_map.get(status)
            if event_type is None:
                if "interrupt" in status:
                    event_type = "interrupted"
                elif "cancel" in status:
                    event_type = "cancelled"
                elif "fail" in status:
                    event_type = "failed"
                elif "timeout" in status:
                    event_type = "timeout"
                else:
                    event_type = "completed"
            started_at = self._codex_item_started_at.pop((session_id, item_id), None) if item_id else None
            latency_ms = None
            if started_at is not None:
                latency_ms = int((now - started_at).total_seconds() * 1000)
            self._safe_log_codex_tool_event(
                session_id=session_id,
                thread_id=thread_id,
                turn_id=turn_id,
                item_id=item_id,
                event_type=event_type,
                item_type=item_type,
                phase="post",
                command=item.get("command"),
                cwd=item.get("cwd"),
                exit_code=item.get("exitCode"),
                file_path=item.get("filePath") or item.get("path"),
                diff_summary=item.get("diffSummary") or item.get("summary"),
                latency_ms=latency_ms,
                final_status=status,
                error_code=item.get("errorCode"),
                error_message=item.get("errorMessage"),
                raw_payload=params,
            )

    async def _handle_codex_stream_error(self, session_id: str, error_code: str, error_message: str):
        """Emit synthetic terminal observability events when app-server stream closes unexpectedly."""
        turn_id = self.codex_active_turn_ids.get(session_id)
        thread_id = self._thread_id_for_session(session_id)
        self._safe_log_codex_tool_event(
            session_id=session_id,
            thread_id=thread_id,
            turn_id=turn_id,
            event_type="failed",
            item_type="tool",
            phase="post",
            final_status="failed",
            error_code=error_code,
            error_message=error_message,
            raw_payload={"error_code": error_code, "error_message": error_message},
        )
        self._safe_log_codex_turn_event(
            session_id=session_id,
            thread_id=thread_id,
            turn_id=turn_id,
            event_type="turn_stream_error",
            status="failed",
            error_code=error_code,
            error_message=error_message,
            raw_payload={"error_code": error_code, "error_message": error_message},
        )

    def _thread_id_for_session(self, session_id: str) -> Optional[str]:
        codex_session = self.codex_sessions.get(session_id)
        if codex_session and codex_session.thread_id:
            return codex_session.thread_id
        session = self.sessions.get(session_id)
        return session.codex_thread_id if session else None

    def _safe_log_codex_tool_event(self, **kwargs: Any):
        try:
            self.codex_observability_logger.log_tool_event(**kwargs)
        except Exception as exc:
            logger.warning("Failed to log codex tool event for %s: %s", kwargs.get("session_id"), exc)

    def _safe_log_codex_turn_event(self, **kwargs: Any):
        try:
            self.codex_observability_logger.log_turn_event(**kwargs)
        except Exception as exc:
            logger.warning("Failed to log codex turn event for %s: %s", kwargs.get("session_id"), exc)

    def is_codex_rollout_enabled(self, flag_name: str) -> bool:
        """Read codex rollout feature gate (defaults to True for unknown flags)."""
        return bool(self.codex_rollout_flags.get(flag_name, True))

    def get_codex_provider_policy(self) -> dict[str, Any]:
        """Expose codex provider mapping policy for API/operator surfaces."""
        return get_codex_app_policy(self.codex_provider_mapping_phase)

    def get_codex_fork_runtime_info(self) -> dict[str, Any]:
        """Expose codex-fork artifact pinning/runtime metadata for operators."""
        return {
            "command": self.codex_fork_command,
            "args": list(self.codex_fork_args),
            "event_schema_version": self.codex_fork_event_schema_version,
            "artifact_ref": self.codex_fork_artifact_ref,
            "artifact_release": self.codex_fork_artifact_release,
            "artifact_platforms": list(self.codex_fork_artifact_platforms),
            "rollback_provider": self.codex_fork_rollback_provider,
            "rollback_command": self.codex_fork_rollback_command,
            "is_pinned": self.codex_fork_artifact_ref != "local-unpinned",
        }

    def get_codex_launch_gates(self) -> dict[str, Any]:
        """Compute codex-fork launch gate status for operator cutover checks."""
        runtime = self.get_codex_fork_runtime_info()
        policy = self.get_codex_provider_policy()
        provider_counts = {
            "claude": 0,
            "codex": 0,
            "codex-fork": 0,
            "codex-app": 0,
            "other": 0,
        }
        for session in self.sessions.values():
            if getattr(session, "status", SessionStatus.RUNNING) == SessionStatus.STOPPED:
                continue
            provider = getattr(session, "provider", "claude")
            if provider in provider_counts:
                provider_counts[provider] += 1
            else:
                provider_counts["other"] += 1

        rollout_flags = {
            "enable_durable_events": self.is_codex_rollout_enabled("enable_durable_events"),
            "enable_structured_requests": self.is_codex_rollout_enabled("enable_structured_requests"),
            "enable_observability_projection": self.is_codex_rollout_enabled("enable_observability_projection"),
            "enable_codex_tui": self.is_codex_rollout_enabled("enable_codex_tui"),
        }
        rollout_ready = (
            rollout_flags["enable_durable_events"]
            and rollout_flags["enable_structured_requests"]
            and rollout_flags["enable_observability_projection"]
        )

        gates = {
            "a0_event_schema_contract": {
                "ok": runtime["event_schema_version"] >= 2,
                "details": f"event_schema_version={runtime['event_schema_version']}",
            },
            "launch_rollout_flags": {
                "ok": rollout_ready,
                "details": rollout_flags,
            },
            "launch_artifact_pin": {
                "ok": bool(runtime["is_pinned"]),
                "details": f"artifact_ref={runtime['artifact_ref']}",
            },
            "launch_codex_app_drain": {
                "ok": provider_counts["codex-app"] == 0,
                "details": f"active_codex_app_sessions={provider_counts['codex-app']}",
            },
            "launch_provider_mapping_phase": {
                "ok": policy.get("phase") in {"migration_window", "post_cutover"},
                "details": f"phase={policy.get('phase')}",
            },
        }
        return {
            "gates": gates,
            "rollout_flags": rollout_flags,
            "provider_counts": provider_counts,
            "codex_fork_runtime": runtime,
            "codex_provider_policy": policy,
        }

    def _retire_codex_app_session_state(
        self,
        session: Session,
        reason: str = CODEX_APP_RETIRED_SESSION_REASON,
        cleanup_queue: bool = True,
    ) -> dict[str, Any]:
        """Transition a codex-app session to retired state with cleanup semantics."""
        session_id = session.id
        codex_session = self.codex_sessions.pop(session_id, None)
        if codex_session:
            try:
                loop = asyncio.get_running_loop()
                loop.create_task(codex_session.close())
            except RuntimeError:
                asyncio.run(codex_session.close())

        self.codex_request_ledger.orphan_pending_for_session(session_id, error_code=reason)

        queue_messages_cleared = 0
        if cleanup_queue and self.message_queue_manager:
            retire_queue = getattr(self.message_queue_manager, "retire_session_queue", None)
            if callable(retire_queue):
                queue_messages_cleared = int(retire_queue(session_id, reason))

        for key in [k for k in self._codex_item_started_at if k[0] == session_id]:
            self._codex_item_started_at.pop(key, None)
        self.codex_turns_in_flight.discard(session_id)
        self.codex_active_turn_ids.pop(session_id, None)
        self.codex_last_delta_at.pop(session_id, None)
        self.codex_wait_states.pop(session_id, None)

        now = datetime.now()
        session.status = SessionStatus.STOPPED
        session.completion_status = CompletionStatus.KILLED
        session.completion_message = reason
        session.error_message = reason
        session.last_activity = now
        session.stopped_at = now
        with contextlib.suppress(Exception):
            self.codex_event_store.append_event(
                session_id=session_id,
                event_type="codex_app_retired",
                payload={
                    "reason": reason,
                    "queue_messages_cleared": queue_messages_cleared,
                },
            )
        return {"queue_messages_cleared": queue_messages_cleared}

    def retire_codex_app_sessions(self, reason: str = CODEX_APP_RETIRED_SESSION_REASON) -> int:
        """Retire all known codex-app sessions deterministically."""
        retired = 0
        for session in self.sessions.values():
            if session.provider != "codex-app":
                continue
            is_already_retired = (
                session.status == SessionStatus.STOPPED
                and session.error_message == reason
            )
            if is_already_retired:
                if self.message_queue_manager:
                    retire_queue = getattr(self.message_queue_manager, "retire_session_queue", None)
                    if callable(retire_queue):
                        retire_queue(session.id, reason)
                continue
            self._retire_codex_app_session_state(session, reason=reason, cleanup_queue=True)
            retired += 1
        if retired:
            self._save_state()
        return retired

    def get_activity_state(self, session_or_id: Session | str) -> str:
        """Get computed activity state for API consumers."""
        session: Optional[Session]
        if isinstance(session_or_id, Session):
            session = session_or_id
        else:
            session = self.sessions.get(session_or_id)
            if not session:
                return ActivityState.STOPPED.value

        if session.provider == "codex-fork":
            return self._compute_codex_fork_activity(session)

        if session.status == SessionStatus.STOPPED:
            return ActivityState.STOPPED.value

        if session.provider == "codex-app":
            return self._compute_codex_app_activity(session)

        queue_mgr = self.message_queue_manager
        delivery_state = queue_mgr.delivery_states.get(session.id) if queue_mgr else None
        is_idle = delivery_state.is_idle if delivery_state is not None else None

        monitor_state = None
        if self.output_monitor:
            getter = getattr(self.output_monitor, "get_session_state", None)
            if callable(getter):
                monitor_state = getter(session.id)

        if monitor_state and monitor_state.last_pattern == "permission":
            return ActivityState.WAITING_PERMISSION.value

        if session.completion_status == CompletionStatus.KILLED:
            return ActivityState.STOPPED.value

        if session.completion_status is not None:
            return ActivityState.WAITING_INPUT.value

        if is_idle is True:
            return ActivityState.IDLE.value

        if is_idle is False:
            if monitor_state and monitor_state.is_output_flowing:
                return ActivityState.WORKING.value
            return ActivityState.THINKING.value

        if session.status == SessionStatus.IDLE:
            if monitor_state and monitor_state.is_output_flowing:
                return ActivityState.WORKING.value
            if session.provider == "codex":
                idle_seconds = (datetime.now() - session.last_activity).total_seconds()
                if idle_seconds < 30:
                    return ActivityState.THINKING.value
            return ActivityState.IDLE.value

        idle_seconds = (datetime.now() - session.last_activity).total_seconds()
        if idle_seconds < 30:
            return ActivityState.THINKING.value
        return ActivityState.IDLE.value

    def _compute_codex_fork_activity(self, session: Session) -> str:
        """Compute activity state for codex-fork sessions from reducer state."""
        lifecycle = self.codex_fork_lifecycle.get(session.id)
        if not lifecycle:
            if session.status == SessionStatus.STOPPED:
                return ActivityState.STOPPED.value
            if session.status == SessionStatus.IDLE:
                return ActivityState.IDLE.value
            return ActivityState.THINKING.value

        state_name = lifecycle.get("state")
        if state_name == "running":
            return ActivityState.WORKING.value
        if state_name == "idle":
            return ActivityState.IDLE.value
        if state_name == "waiting_on_approval":
            return ActivityState.WAITING_PERMISSION.value
        if state_name == "waiting_on_user_input":
            return ActivityState.WAITING_INPUT.value
        if state_name in ("shutdown", "error"):
            return ActivityState.STOPPED.value
        return ActivityState.THINKING.value

    @staticmethod
    def _codex_fork_pane_text_indicates_working(pane_text: str) -> bool:
        """Return True when codex-fork TUI text shows active work not modeled by events."""
        if not pane_text:
            return False
        pane_tail = "\n".join(
            line
            for line in pane_text.splitlines()[-5:]
            if line.strip()
        )
        markers = (
            "Working (",
            "Waiting for background terminal",
            "background terminal running",
            "background terminals running",
        )
        return any(marker in pane_tail for marker in markers)

    def _codex_fork_pane_indicates_working(self, session: Session) -> bool:
        """Use the live codex-fork pane as a bounded correction for idle reducer gaps."""
        if not session.tmux_session:
            return False
        try:
            pane_text = self.tmux.capture_pane(session.tmux_session, lines=8)
        except Exception:
            logger.debug("Failed to capture codex-fork pane for activity projection", exc_info=True)
            return False
        return self._codex_fork_pane_text_indicates_working(pane_text or "")

    def _compute_codex_app_activity(self, session: Session) -> str:
        """Compute activity state for codex-app sessions (no tmux/output monitor)."""
        if session.completion_status == CompletionStatus.KILLED:
            return ActivityState.STOPPED.value

        if session.completion_status is not None:
            return ActivityState.WAITING_INPUT.value

        queue_mgr = self.message_queue_manager
        delivery_state = queue_mgr.delivery_states.get(session.id) if queue_mgr else None
        if delivery_state is not None:
            return ActivityState.IDLE.value if delivery_state.is_idle else ActivityState.WORKING.value

        idle_seconds = (datetime.now() - session.last_activity).total_seconds()
        if idle_seconds > 30:
            return ActivityState.IDLE.value
        return ActivityState.THINKING.value

    def get_attach_descriptor(self, session_id: str) -> Optional[dict[str, Any]]:
        """Return provider-specific attach metadata for detached-runtime reattach."""
        session = self.sessions.get(session_id)
        if not session:
            return None

        provider = getattr(session, "provider", "claude")
        if session.status == SessionStatus.STOPPED:
            return {
                "session_id": session.id,
                "provider": provider,
                "attach_supported": False,
                "runtime_mode": "stopped",
                "message": "Session is stopped and no live runtime is available for attach.",
            }
        if provider == "codex-fork":
            lifecycle = self.get_codex_fork_lifecycle_state(session_id) or {}
            return {
                "session_id": session.id,
                "provider": provider,
                "attach_supported": True,
                "attach_transport": "tmux",
                "tmux_session": session.tmux_session,
                "tmux_socket_name": session.tmux_socket_name,
                "tmux_history_limit": self._tmux_session_history_limit(session.tmux_session),
                "tmux_configured_history_limit": self._tmux_history_limit(),
                "runtime_mode": "detached_runtime",
                "runtime_id": f"codex-fork:{session.id}",
                "runtime_owner": self.codex_fork_runtime_owner.get(session.id),
                "lifecycle_state": lifecycle.get("state", "idle"),
                "lifecycle_cause": lifecycle.get("cause_event_type"),
                "control_socket_path": str(self._codex_fork_control_socket_path(session)),
                "event_stream_path": str(self._codex_fork_event_stream_path(session)),
            }

        if provider == "codex-app":
            return {
                "session_id": session.id,
                "provider": provider,
                "attach_supported": False,
                "runtime_mode": "headless",
                "message": "provider=codex-app is headless; use watch/status APIs instead of attach.",
            }

        return {
            "session_id": session.id,
            "provider": provider,
            "attach_supported": True,
            "attach_transport": "tmux",
            "tmux_session": session.tmux_session,
            "tmux_socket_name": session.tmux_socket_name,
            "tmux_history_limit": self._tmux_session_history_limit(session.tmux_session),
            "tmux_configured_history_limit": self._tmux_history_limit(),
            "runtime_mode": "tmux",
        }

    def get_codex_events(self, session_id: str, since_seq: Optional[int] = None, limit: int = 200) -> dict:
        """Read persisted codex event timeline for one session."""
        return self.codex_event_store.get_events(session_id=session_id, since_seq=since_seq, limit=limit)

    def get_codex_activity_actions(self, session_id: str, limit: int = 20) -> list[dict[str, Any]]:
        """Return provider-neutral projected codex-app actions for CLI surfaces."""
        return self.codex_activity_projection.recent_actions(session_id=session_id, limit=limit)

    def get_codex_latest_activity_action(self, session_id: str) -> Optional[dict[str, Any]]:
        """Return latest provider-neutral projected codex-app action summary."""
        return self.codex_activity_projection.latest_action(session_id=session_id)

    def list_codex_pending_requests(self, session_id: str, include_orphaned: bool = False) -> list[dict]:
        """List pending structured requests for a codex-app session."""
        return self.codex_request_ledger.list_requests(session_id=session_id, include_orphaned=include_orphaned)

    async def respond_codex_request(self, session_id: str, request_id: str, response_payload: dict) -> dict:
        """Resolve one structured request for a codex-app session."""
        request = self.codex_request_ledger.get_request(request_id)
        if not request or request.get("session_id") != session_id:
            return {
                "ok": False,
                "http_status": 404,
                "error_code": "request_not_found",
                "error_message": "request id not found for session",
            }
        result = await self.codex_request_ledger.resolve_request(
            request_id=request_id,
            response_payload=response_payload,
            resolution_source="api",
        )
        if result.get("ok"):
            event_type = "approval_decision" if "decision" in response_payload else "user_input_submitted"
            request_method = request.get("request_method", "")
            if "commandExecution" in request_method:
                item_type = "commandExecution"
            elif "fileChange" in request_method:
                item_type = "fileChange"
            else:
                item_type = "tool"
            self._safe_log_codex_tool_event(
                session_id=session_id,
                thread_id=request.get("thread_id"),
                turn_id=request.get("turn_id"),
                item_id=request.get("item_id"),
                request_id=request_id,
                event_type=event_type,
                item_type=item_type,
                phase="post",
                approval_decision=response_payload.get("decision"),
                raw_payload=response_payload,
            )
        return result

    def has_pending_codex_requests(self, session_id: str) -> bool:
        """Return True when unresolved structured codex requests block chat input."""
        return self.codex_request_ledger.has_pending_requests(session_id=session_id)

    def oldest_pending_codex_request(self, session_id: str) -> Optional[dict]:
        """Return oldest pending request summary for explicit input-gate error payloads."""
        return self.codex_request_ledger.oldest_pending_summary(session_id=session_id)

    async def clear_session(self, session_id: str, new_prompt: Optional[str] = None) -> bool:
        """Clear/reset a session's context (Claude: /clear, Codex: /new, Codex app: new thread)."""
        session = self.sessions.get(session_id)
        if not session:
            return False

        if session.provider == "codex-app":
            codex_session = await self._ensure_codex_session(session)
            if not codex_session:
                return False
            try:
                self.codex_request_ledger.orphan_pending_for_session(session_id, error_code="thread_reset")
                for key in [k for k in self._codex_item_started_at if k[0] == session_id]:
                    self._codex_item_started_at.pop(key, None)
                self.codex_turns_in_flight.discard(session_id)
                self.codex_active_turn_ids.pop(session_id, None)
                self.codex_last_delta_at.pop(session_id, None)
                self.codex_wait_states.pop(session_id, None)
                await codex_session.start_new_thread()
                session.codex_thread_id = codex_session.thread_id
                session.status = SessionStatus.IDLE
                session.last_activity = datetime.now()
                self._save_state()
                if new_prompt:
                    await codex_session.send_user_turn(new_prompt)
                    session.status = SessionStatus.RUNNING
                    session.last_activity = datetime.now()
                    self._save_state()
                    if self.message_queue_manager:
                        self.message_queue_manager.mark_session_active(session_id)
                elif self.message_queue_manager:
                    self.message_queue_manager.mark_session_idle(session_id)
                return True
            except Exception as e:
                logger.error(f"Failed to clear Codex app session {session_id}: {e}")
                return False

        if session.provider in ("codex", "codex-fork"):
            return await self._clear_tmux_session(session, new_prompt, clear_command="/new")

        return await self._clear_tmux_session(session, new_prompt, clear_command="/clear")

    async def _clear_tmux_session(
        self,
        session: Session,
        new_prompt: Optional[str],
        clear_command: str,
    ) -> bool:
        """Send a clear command to a tmux session (async)."""
        tmux_session = session.tmux_session
        if not tmux_session:
            return False

        from src.models import CompletionStatus
        try:
            # If session is completed, wake it up first
            if session.completion_status == CompletionStatus.COMPLETED:
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux.tmux_cmd_for_session(tmux_session, "send-keys", "-t", tmux_session, "Enter"),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await proc.communicate()
                await asyncio.sleep(1.5)

            # Interrupt any ongoing stream
            proc = await asyncio.create_subprocess_exec(
                *self.tmux.tmux_cmd_for_session(tmux_session, "send-keys", "-t", tmux_session, "Escape"),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await proc.communicate()
            await asyncio.sleep(0.5)

            # Send clear command
            proc = await asyncio.create_subprocess_exec(
                *self.tmux.tmux_cmd_for_session(tmux_session, "send-keys", "-t", tmux_session, clear_command),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await proc.communicate()
            await asyncio.sleep(1.0)

            proc = await asyncio.create_subprocess_exec(
                *self.tmux.tmux_cmd_for_session(tmux_session, "send-keys", "-t", tmux_session, "Enter"),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await proc.communicate()
            await asyncio.sleep(2.0)

            # Send new prompt if provided
            if new_prompt:
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux.tmux_cmd_for_session(tmux_session, "send-keys", "-t", tmux_session, new_prompt),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await proc.communicate()
                await asyncio.sleep(1.0)
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux.tmux_cmd_for_session(tmux_session, "send-keys", "-t", tmux_session, "Enter"),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await proc.communicate()
            return True
        except Exception as e:
            logger.error(f"Failed to clear tmux session {session.id}: {e}")
            return False

    def send_key(self, session_id: str, key: str) -> bool:
        """Send a key to a session (e.g., 'y', 'n')."""
        session = self.sessions.get(session_id)
        if not session:
            logger.error(f"Session not found: {session_id}")
            return False

        if session.provider == "codex-app":
            # Only support interrupt for Codex app sessions
            if key == "Escape":
                try:
                    loop = asyncio.get_running_loop()
                    loop.create_task(self._interrupt_codex(session))
                    return True
                except RuntimeError:
                    try:
                        asyncio.run(self._interrupt_codex(session))
                        return True
                    except Exception:
                        return False
                except Exception:
                    return False
            return False

        success = self.tmux.send_key(session.tmux_session, key)
        if success:
            session.last_activity = datetime.now()
            session.status = SessionStatus.RUNNING
            self._save_state()

        return success

    def kill_session(self, session_id: str) -> bool:
        """Kill a session."""
        session = self.sessions.get(session_id)
        if not session:
            logger.error(f"Session not found: {session_id}")
            return False

        if session.provider == "codex-app":
            codex_session = self.codex_sessions.pop(session_id, None)
            if codex_session:
                try:
                    loop = asyncio.get_running_loop()
                    loop.create_task(codex_session.close())
                except RuntimeError:
                    asyncio.run(codex_session.close())
            self.codex_request_ledger.orphan_pending_for_session(session_id)
            for key in [k for k in self._codex_item_started_at if k[0] == session_id]:
                self._codex_item_started_at.pop(key, None)
            self.codex_turns_in_flight.discard(session_id)
            self.codex_active_turn_ids.pop(session_id, None)
            self.codex_last_delta_at.pop(session_id, None)
            self.codex_wait_states.pop(session_id, None)
        else:
            if session.provider == "codex-fork":
                monitor_task = self.codex_fork_event_monitors.pop(session_id, None)
                if monitor_task:
                    monitor_task.cancel()
                self.codex_fork_event_offsets.pop(session_id, None)
                self.codex_fork_event_buffers.pop(session_id, None)
                self.codex_fork_turns_in_flight.discard(session_id)
                self.codex_fork_wait_resume_state.pop(session_id, None)
                self.codex_fork_wait_kind.pop(session_id, None)
                self.codex_fork_last_seq.pop(session_id, None)
                self.codex_fork_session_epoch.pop(session_id, None)
                self.codex_fork_control_epoch.pop(session_id, None)
                self.codex_fork_control_degraded.pop(session_id, None)
                self.codex_fork_runtime_owner.pop(session_id, None)
                event_stream_path = self._codex_fork_event_stream_path(session)
                control_socket_path = self._codex_fork_control_socket_path(session)
                if event_stream_path.exists():
                    event_stream_path.unlink()
                if control_socket_path.exists():
                    control_socket_path.unlink()
                self._set_codex_fork_lifecycle_state(
                    session_id=session_id,
                    state="shutdown",
                    cause_event_type="session_killed",
                )
            self.tmux.kill_session(session.tmux_session)
        now = datetime.now()
        session.status = SessionStatus.STOPPED
        session.completion_status = CompletionStatus.KILLED
        session.completion_message = "Terminated via sm kill"
        session.completed_at = now
        session.stopped_at = now
        self.unregister_session_roles(session_id, persist=False)
        self._save_state()

        logger.info(f"Killed session {session.name}")
        return True

    async def restore_session(self, session_id: str) -> tuple[bool, Optional[Session], Optional[str]]:
        """Restore a stopped session in place using provider-native resume metadata."""
        session = self.sessions.get(session_id)
        if not session:
            return False, None, "Session not found"
        tmux_runtime_missing = (
            session.provider != "codex-app"
            and not self.tmux.session_exists(session.tmux_session)
        )
        if session.status != SessionStatus.STOPPED and not tmux_runtime_missing:
            return False, session, "Session is not stopped"

        resume_id = self.get_session_resume_id(session)
        if session.provider != "codex-app" and not session.log_file:
            session.log_file = str(self.log_dir / f"{session.name}.log")
        if session.provider != "codex-app" and self.tmux.session_exists(session.tmux_session):
            self.tmux.kill_session(session.tmux_session)

        if session.provider == "claude":
            if not resume_id:
                return False, session, "No Claude resume id is available for this session"
            claude_config = self.config.get("claude", {})
            command = claude_config.get("command", "claude")
            args = list(claude_config.get("args", []))
            args.extend(["--resume", resume_id])
            if not self.tmux.create_session_with_command(
                session.tmux_session,
                session.working_dir,
                session.log_file,
                session_id=session.id,
                command=command,
                args=args,
                model=session.model,
            ):
                tmux_error = getattr(self.tmux, "last_error_message", None)
                if tmux_error:
                    session.error_message = tmux_error
                    self._save_state()
                return False, session, tmux_error or "Failed to restore Claude session runtime"
            session.tmux_socket_name = self._tmux_socket_name()
        elif session.provider in ("codex", "codex-fork"):
            if not resume_id:
                return False, session, "No Codex resume id is available for this session"
            if session.provider == "codex":
                command = self.codex_cli_command
                args = ["resume", resume_id, *self.codex_cli_args]
            else:
                command, args, effective_provider, fallback_reason = self._build_codex_fork_launch_spec(
                    session,
                    resume_id=resume_id,
                )
                if effective_provider != session.provider:
                    logger.warning(
                        "Falling back from codex-fork to codex while restoring %s: %s",
                        session.id,
                        fallback_reason,
                    )
                    session.provider = effective_provider
            if not self.tmux.create_session_with_command(
                session.tmux_session,
                session.working_dir,
                session.log_file,
                session_id=session.id,
                command=command,
                args=args,
            ):
                tmux_error = getattr(self.tmux, "last_error_message", None)
                if tmux_error:
                    session.error_message = tmux_error
                    self._save_state()
                return False, session, tmux_error or "Failed to restore Codex session runtime"
            session.tmux_socket_name = self._tmux_socket_name()
        elif session.provider == "codex-app":
            thread_id = session.codex_thread_id or resume_id
            if not thread_id:
                return False, session, "No Codex app thread id is available for this session"
            try:
                codex_session = CodexAppServerSession(
                    session_id=session.id,
                    working_dir=session.working_dir,
                    config=self.codex_config,
                    on_turn_complete=self._handle_codex_turn_complete,
                    on_turn_started=self._handle_codex_turn_started,
                    on_turn_delta=self._handle_codex_turn_delta,
                    on_review_complete=self._handle_codex_review_complete,
                    on_server_request=self._handle_codex_server_request,
                    on_item_notification=self._handle_codex_item_notification,
                    on_stream_error=self._handle_codex_stream_error,
                )
                session.codex_thread_id = await codex_session.start(thread_id=thread_id)
                self.codex_sessions[session.id] = codex_session
                self._sync_session_resume_id(session)
            except CodexAppServerError as exc:
                return False, session, f"Failed to restore Codex app session: {exc}"
            except Exception as exc:
                logger.error("Unexpected Codex app restore error for %s: %s", session.id, exc)
                return False, session, "Failed to restore Codex app session"
        else:
            return False, session, f"Restore not supported for provider={session.provider}"

        session.error_message = None
        session.completion_status = None
        session.completion_message = None
        session.completed_at = None
        session.agent_task_completed_at = None
        session.stopped_at = None
        session.status = SessionStatus.IDLE if session.provider == "codex-app" else SessionStatus.RUNNING
        session.last_activity = datetime.now()
        if session.provider == "codex-fork":
            self.codex_fork_runtime_owner[session.id] = session.parent_session_id or session.id
            self.codex_fork_event_offsets.pop(session.id, None)
            self.codex_fork_event_buffers.pop(session.id, None)
            self._set_codex_fork_lifecycle_state(
                session_id=session.id,
                state="running",
                cause_event_type="session_restored",
            )
            self._start_codex_fork_event_monitor(session)
        if self.message_queue_manager and session.provider == "codex-app":
            self.message_queue_manager.mark_session_idle(session.id)
        self._save_state()
        if session.telegram_chat_id:
            # Topic creation can make slow Telegram API calls; keep restore on the
            # runtime path and let topic repair complete in the background.
            self._schedule_telegram_topic_ensure(session, session.telegram_chat_id)
        logger.info("Restored session %s (%s)", session.id, session.provider)
        return True, session, None

    def open_terminal(self, session_id: str) -> bool:
        """Open a session in Terminal.app."""
        session = self.sessions.get(session_id)
        if not session:
            logger.error(f"Session not found: {session_id}")
            return False

        if session.provider == "codex-app":
            logger.warning("Terminal open not supported for Codex app sessions")
            return False

        return self.tmux.open_in_terminal(session.tmux_session)

    def capture_output(self, session_id: str, lines: int = 50) -> Optional[str]:
        """Capture recent output from a session."""
        session = self.sessions.get(session_id)
        if not session:
            return None

        if session.provider == "codex-app":
            if self.hook_output_store:
                output = self.hook_output_store.get(session_id)
                if output is None:
                    return None
                if lines <= 0:
                    return ""
                chunks = output.splitlines()
                tail = chunks[-lines:] if chunks else []
                if not tail:
                    return ""
                # Preserve trailing newline semantics from tmux capture where possible.
                suffix = "\n" if output.endswith("\n") else ""
                return "\n".join(tail) + suffix
            return None

        return self.tmux.capture_pane(session.tmux_session, lines)

    # cleanup_dead_sessions() removed - OutputMonitor now handles detection and cleanup automatically

    async def start_review(
        self,
        session_id: str,
        mode: str,
        base_branch: Optional[str] = None,
        commit_sha: Optional[str] = None,
        custom_prompt: Optional[str] = None,
        steer_text: Optional[str] = None,
        wait: Optional[int] = None,
        watcher_session_id: Optional[str] = None,
    ) -> dict:
        """
        Start a Codex /review on an existing session.

        Args:
            session_id: Target session ID (must be a Codex CLI/codex-fork/codex-app session)
            mode: Review mode (branch, uncommitted, commit, custom)
            base_branch: Target branch for branch mode
            commit_sha: Target SHA for commit mode
            custom_prompt: Custom review text for custom mode
            steer_text: Instructions to inject after review starts
            wait: Seconds to watch for completion
            watcher_session_id: Session to notify when review completes

        Returns:
            Status dict with review info
        """
        session = self.sessions.get(session_id)
        if not session:
            return {"error": f"Session {session_id} not found"}

        if session.provider not in ("codex", "codex-fork", "codex-app"):
            return {"error": "Review requires a Codex session (provider=codex, codex-fork, or codex-app)"}

        # Validate session is idle before sending /review
        if self.message_queue_manager:
            state = self.message_queue_manager.delivery_states.get(session_id)
            if state and not state.is_idle:
                return {"error": "Session is busy. Wait for current work to complete or use sm clear first."}

        # Store ReviewConfig on session
        review_config = ReviewConfig(
            mode=mode,
            base_branch=base_branch,
            commit_sha=commit_sha,
            custom_prompt=custom_prompt,
            steer_text=steer_text,
            steer_delivered=False,
        )
        session.review_config = review_config

        # Reset idle baseline for ChildMonitor
        session.last_tool_call = datetime.now()
        self._save_state()

        # --- codex-app path: use review/start RPC ---
        if session.provider == "codex-app":
            codex_session = await self._ensure_codex_session(session)
            if not codex_session:
                if self.message_queue_manager:
                    self.message_queue_manager.mark_session_idle(session_id)
                return {"error": "Failed to connect to Codex app-server"}

            try:
                # Mark active just before dispatch (after all validation)
                if self.message_queue_manager:
                    self.message_queue_manager.mark_session_active(session_id)
                await codex_session.review_start(
                    mode=mode,
                    base_branch=base_branch,
                    commit_sha=commit_sha,
                    custom_prompt=custom_prompt,
                )
                session.status = SessionStatus.RUNNING
                session.last_activity = datetime.now()
                self._save_state()
            except CodexAppServerError as e:
                if self.message_queue_manager:
                    self.message_queue_manager.mark_session_idle(session_id)
                return {"error": f"review/start RPC failed: {e}"}

            # Register watch if requested
            if wait and watcher_session_id and self.message_queue_manager:
                await self.message_queue_manager.watch_session(
                    session_id, watcher_session_id, wait
                )

            return {
                "session_id": session_id,
                "review_mode": mode,
                "base_branch": base_branch,
                "commit_sha": commit_sha,
                "status": "started",
                "steer_queued": False,  # steer not applicable for app-server
            }

        # --- codex CLI path: tmux key sequence ---
        # Validate working dir is a git repo
        working_path = Path(session.working_dir).expanduser().resolve()
        try:
            result = subprocess.run(
                ["git", "rev-parse", "--git-dir"],
                cwd=working_path,
                capture_output=True,
                text=True,
                timeout=2,
            )
            if result.returncode != 0:
                return {"error": f"Working directory is not a git repo: {session.working_dir}"}
        except Exception as e:
            return {"error": f"Failed to check git repo: {e}"}

        # For branch mode, find branch position
        branch_position = None
        if mode == "branch" and base_branch:
            try:
                result = subprocess.run(
                    ["git", "branch", "--list"],
                    cwd=working_path,
                    capture_output=True,
                    text=True,
                    timeout=2,
                )
                if result.returncode != 0:
                    return {"error": "Failed to list git branches"}

                branches = []
                for line in result.stdout.strip().split("\n"):
                    # Strip leading whitespace and * marker for current branch
                    branch = line.strip().lstrip("* ").strip()
                    if branch:
                        branches.append(branch)

                if base_branch not in branches:
                    return {"error": f"Branch '{base_branch}' not found. Available: {', '.join(branches)}"}

                branch_position = branches.index(base_branch)
                logger.info(f"Branch '{base_branch}' at position {branch_position} in list: {branches}")
            except subprocess.TimeoutExpired:
                return {"error": "Timeout listing git branches"}

        # Get review timing config
        codex_config = self.config.get("codex", {})
        review_timing = codex_config.get("review", {})

        # Mark active just before dispatch (after all validation)
        if self.message_queue_manager:
            self.message_queue_manager.mark_session_active(session_id)

        # Send the review key sequence
        success = await self.tmux.send_review_sequence(
            session_name=session.tmux_session,
            mode=mode,
            base_branch=base_branch,
            commit_sha=commit_sha,
            custom_prompt=custom_prompt,
            branch_position=branch_position,
            config=review_timing,
        )

        if not success:
            # Roll back active state to avoid wedged session
            if self.message_queue_manager:
                self.message_queue_manager.mark_session_idle(session_id)
            return {"error": "Failed to send review sequence to tmux"}

        # Schedule steer injection if requested
        if steer_text:
            steer_delay = review_timing.get("steer_delay_seconds", 5.0)

            async def _inject_steer():
                await asyncio.sleep(steer_delay)
                steer_success = await self.tmux.send_steer_text(session.tmux_session, steer_text)
                if steer_success:
                    session.review_config.steer_delivered = True
                    self._save_state()
                    logger.info(f"Steer text injected for session {session_id}")
                else:
                    logger.error(f"Failed to inject steer text for session {session_id}")

            asyncio.create_task(_inject_steer())

        # Register watch if requested
        if wait and watcher_session_id and self.message_queue_manager:
            await self.message_queue_manager.watch_session(
                session_id, watcher_session_id, wait
            )

        return {
            "session_id": session_id,
            "review_mode": mode,
            "base_branch": base_branch,
            "commit_sha": commit_sha,
            "status": "started",
            "steer_queued": steer_text is not None,
        }

    async def spawn_review_session(
        self,
        parent_session_id: str,
        mode: str,
        base_branch: Optional[str] = None,
        commit_sha: Optional[str] = None,
        custom_prompt: Optional[str] = None,
        steer_text: Optional[str] = None,
        name: Optional[str] = None,
        wait: Optional[int] = None,
        model: Optional[str] = None,
        working_dir: Optional[str] = None,
    ) -> Optional[Session]:
        """
        Spawn a new Codex session and immediately start a review.

        Args:
            parent_session_id: Parent session ID
            mode: Review mode (branch, uncommitted, commit, custom)
            base_branch: Target branch for branch mode
            commit_sha: Target SHA for commit mode
            custom_prompt: Custom review text for custom mode
            steer_text: Instructions to inject after review starts
            name: Friendly name for the new session
            wait: Seconds to watch for completion
            model: Model override
            working_dir: Working directory override

        Returns:
            Created Session or None on failure
        """
        parent = self.sessions.get(parent_session_id)
        if not parent:
            logger.error(f"Parent session not found: {parent_session_id}")
            return None

        child_working_dir = working_dir or parent.working_dir

        # Spawn a Codex session with no initial prompt
        session = await self._create_session_common(
            working_dir=child_working_dir,
            name=f"child-{parent_session_id[:6]}" if not name else None,
            friendly_name=name,
            parent_session_id=parent_session_id,
            spawn_prompt=f"review:{mode}",
            model=model,
            initial_prompt=None,  # No prompt — we send /review instead
            provider="codex",
        )

        if not session:
            return None

        # Wait for Codex CLI to initialize
        tmux_timeouts = self.config.get("timeouts", {}).get("tmux", {})
        init_seconds = tmux_timeouts.get("claude_init_seconds", 3)
        await asyncio.sleep(init_seconds)

        # Start the review (wait/watcher handled by ChildMonitor below, not watch_session)
        result = await self.start_review(
            session_id=session.id,
            mode=mode,
            base_branch=base_branch,
            commit_sha=commit_sha,
            custom_prompt=custom_prompt,
            steer_text=steer_text,
            wait=None,
            watcher_session_id=None,
        )

        if result.get("error"):
            logger.error(f"Failed to start review on spawned session {session.id}: {result['error']}")
            # Clean up the leaked session to avoid orphans
            self.kill_session(session.id)
            return None

        # Register with ChildMonitor if wait specified
        if wait and self.child_monitor:
            self.child_monitor.register_child(
                child_session_id=session.id,
                parent_session_id=parent_session_id,
                wait_seconds=wait,
            )

        return session

    async def start_pr_review(
        self,
        pr_number: int,
        repo: Optional[str] = None,
        steer: Optional[str] = None,
        wait: Optional[int] = None,
        caller_session_id: Optional[str] = None,
    ) -> dict:
        """
        Trigger @codex review on a GitHub PR.

        No tmux session needed — posts a GitHub comment and optionally
        polls for the review to appear.

        Args:
            pr_number: GitHub PR number
            repo: GitHub repo (owner/repo). Inferred from working dir if None.
            steer: Focus instructions appended to the @codex review comment
            wait: Seconds to poll for Codex review completion
            caller_session_id: Session to store ReviewConfig on and notify

        Returns:
            Status dict with repo, pr_number, posted_at, comment_id, status
        """
        # 1. Resolve repo
        if not repo:
            # Try to infer from caller session's working dir, or cwd
            working_dir = None
            if caller_session_id:
                caller = self.sessions.get(caller_session_id)
                if caller:
                    working_dir = caller.working_dir
            if working_dir:
                repo = await asyncio.to_thread(get_pr_repo_from_git, working_dir)
            if not repo:
                return {"error": "Could not determine repo. Provide --repo or run from a git directory."}

        # 2. Validate PR exists
        try:
            result = await asyncio.to_thread(
                subprocess.run,
                ["gh", "pr", "view", str(pr_number), "--repo", repo, "--json", "state"],
                capture_output=True,
                text=True,
                timeout=10,
            )
            if result.returncode != 0:
                return {"error": f"PR #{pr_number} not found in {repo}: {result.stderr.strip()}"}
            pr_data = json.loads(result.stdout)
            if pr_data.get("state") != "OPEN":
                return {"error": f"PR #{pr_number} is {pr_data.get('state', 'unknown')}, not OPEN"}
        except Exception as e:
            return {"error": f"Failed to validate PR: {e}"}

        # 3. Store ReviewConfig on caller session (if provided)
        review_config = ReviewConfig(
            mode="pr",
            pr_number=pr_number,
            pr_repo=repo,
            steer_text=steer,
        )
        if caller_session_id:
            caller = self.sessions.get(caller_session_id)
            if caller:
                caller.review_config = review_config
                self._save_state()

        # 4. Post @codex review comment
        try:
            comment_result = await asyncio.to_thread(
                post_pr_review_comment, repo, pr_number, steer
            )
        except RuntimeError as e:
            return {"error": str(e)}

        # Store comment_id on ReviewConfig
        if caller_session_id:
            caller = self.sessions.get(caller_session_id)
            if caller and caller.review_config:
                caller.review_config.pr_comment_id = comment_result.get("comment_id")
                self._save_state()

        posted_at = comment_result["posted_at"]

        # 5. Start background poll if wait AND caller_session_id
        server_polling = False
        if wait and caller_session_id:
            server_polling = True

            async def _poll_and_notify():
                since = datetime.fromisoformat(posted_at)
                review = await asyncio.to_thread(
                    poll_for_codex_review, repo, pr_number, since, wait
                )
                if review:
                    msg = f"Review --pr {pr_number} ({repo}) completed: Codex posted review on PR #{pr_number}"
                else:
                    msg = f"Review --pr {pr_number} ({repo}) timed out after {wait}s"
                # Notify caller
                await self.send_input(
                    caller_session_id,
                    msg,
                    delivery_mode="important",
                )

            asyncio.create_task(_poll_and_notify())

        return {
            "repo": repo,
            "pr_number": pr_number,
            "posted_at": posted_at,
            "comment_id": comment_result.get("comment_id", 0),
            "comment_body": comment_result.get("body", ""),
            "status": "posted",
            "server_polling": server_polling,
        }

    async def recover_session(self, session: Session, graceful: bool = False) -> bool:
        """
        Recover a session from Claude Code harness crash.

        This handles JavaScript stack overflow crashes in the TUI harness.
        The agent (Anthropic backend) is unaffected - only the local harness crashed.

        Recovery flow (graceful=False, harness is dead):
        1. Pause message queue (prevent sm send going to bash)
        2. Send Ctrl-C twice to kill the crashed harness
        3. Parse resume UUID from Claude's exit output in the terminal
        4. Reset terminal with stty sane
        5. Resume Claude with --resume <uuid>
        6. Unpause message queue

        Recovery flow (graceful=True, harness survived):
        1. Pause message queue
        2. Send /exit + Enter to cleanly shut down the harness
        3. Parse resume UUID from Claude's exit output
        4. Resume Claude with --resume <uuid>
        5. Unpause message queue

        Args:
            session: Session to recover
            graceful: If True, use /exit instead of Ctrl-C (harness is still alive)

        Returns:
            True if recovery successful, False otherwise
        """
        if session.provider != "claude":
            logger.warning(f"Crash recovery only supported for Claude sessions, not {session.provider}")
            return False

        logger.info(f"Starting crash recovery for session {session.id}")

        # 1. Pause message queue
        if self.message_queue_manager:
            self.message_queue_manager.pause_session(session.id)

        try:
            # 2. Shut down the harness
            if graceful:
                # Harness survived the crash — use /exit for a clean shutdown
                logger.debug(f"Sending /exit to session {session.id} (graceful)")
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux.tmux_cmd_for_session(session.tmux_session, "send-keys", "-t", session.tmux_session, "Escape"),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=5)
                await asyncio.sleep(0.3)
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux.tmux_cmd_for_session(session.tmux_session, "send-keys", "-t", session.tmux_session, "/exit", "Enter"),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=5)
                await asyncio.sleep(3.0)
            else:
                # Harness is dead — Ctrl-C to force kill
                logger.debug(f"Sending C-c twice to session {session.id}")
                for _ in range(2):
                    proc = await asyncio.create_subprocess_exec(
                        *self.tmux.tmux_cmd_for_session(session.tmux_session, "send-keys", "-t", session.tmux_session, "C-c"),
                        stdout=asyncio.subprocess.PIPE,
                        stderr=asyncio.subprocess.PIPE,
                    )
                    await asyncio.wait_for(proc.communicate(), timeout=5)
                    await asyncio.sleep(0.5)

                # Wait for Claude to print exit message (crash dump is large)
                await asyncio.sleep(3.0)

            # 4. Parse resume ID from Claude's exit output
            #    Claude prints: "To resume this conversation, run:\n  claude --resume <uuid>"
            resume_uuid = None
            proc = await asyncio.create_subprocess_exec(
                *self.tmux.tmux_cmd_for_session(session.tmux_session, "capture-pane", "-p", "-t", session.tmux_session, "-S", "-200"),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=5)
            if proc.returncode == 0:
                import re
                # Match Claude's specific exit block:
                #   "To resume this conversation, run:\n  claude --resume <uuid>"
                match = re.search(
                    r'To resume this conversation.*?--resume\s+([0-9a-f-]{36})',
                    stdout.decode(),
                    re.DOTALL,
                )
                if match:
                    resume_uuid = match.group(1)
                    logger.info(f"Parsed resume UUID from terminal output: {resume_uuid}")

            if not resume_uuid:
                # Fallback to stored transcript_path
                if session.transcript_path:
                    resume_uuid = Path(session.transcript_path).stem
                    logger.warning(f"Could not parse resume UUID from output, falling back to transcript_path: {resume_uuid}")
                else:
                    logger.error(f"Cannot recover session {session.id}: no resume UUID found")
                    return False

            # 5. Reset terminal with stty sane (only needed for forceful Ctrl-C recovery)
            if not graceful:
                logger.debug(f"Sending stty sane to session {session.id}")
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux.tmux_cmd_for_session(session.tmux_session, "send-keys", "-t", session.tmux_session, "stty sane", "Enter"),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=5)
                await asyncio.sleep(0.5)

            # 6. Unset CLAUDECODE to prevent nested-session detection
            #    (Claude Code exports this; it persists in the shell after the process dies)
            proc = await asyncio.create_subprocess_exec(
                *self.tmux.tmux_cmd_for_session(
                    session.tmux_session,
                    "send-keys", "-t", session.tmux_session,
                    "unset CLAUDECODE", "Enter",
                ),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=5)
            await asyncio.sleep(0.3)

            tmux_timeouts = self.config.get("timeouts", {}).get("tmux", {})
            shell_fd_limit = int(tmux_timeouts.get("shell_fd_limit", 65536))
            if shell_fd_limit > 0:
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux.tmux_cmd_for_session(
                        session.tmux_session,
                        "send-keys", "-t", session.tmux_session,
                        f"ulimit -n {shell_fd_limit}", "Enter",
                    ),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=5)
                await asyncio.sleep(0.3)

            color_env = {
                "TERM_PROGRAM": os.environ.get("TERM_PROGRAM"),
                "TERM_PROGRAM_VERSION": os.environ.get("TERM_PROGRAM_VERSION"),
                "COLORTERM": os.environ.get("COLORTERM"),
                "CLICOLOR": os.environ.get("CLICOLOR"),
                "CLICOLOR_FORCE": os.environ.get("CLICOLOR_FORCE"),
                "FORCE_COLOR": os.environ.get("FORCE_COLOR"),
            }
            color_cmds = ["unset NO_COLOR"]
            for name, value in color_env.items():
                if value:
                    color_cmds.append(f"export {name}={shlex.quote(value)}")
                else:
                    color_cmds.append(f"unset {name}")

            for color_cmd in color_cmds:
                proc = await asyncio.create_subprocess_exec(
                    *self.tmux.tmux_cmd_for_session(
                        session.tmux_session,
                        "send-keys", "-t", session.tmux_session,
                        color_cmd, "Enter",
                    ),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                await asyncio.wait_for(proc.communicate(), timeout=5)
                await asyncio.sleep(0.1)

            # 7. Build resume command with config args
            claude_config = self.config.get("claude", {})
            command = claude_config.get("command", "claude")
            args = claude_config.get("args", [])

            # Build full command: claude [args] --resume <uuid>
            resume_cmd = f"{command}"
            if args:
                resume_cmd += " " + " ".join(args)
            resume_cmd += f" --resume {resume_uuid}"

            logger.debug(f"Sending resume command to session {session.id}: {resume_cmd}")
            proc = await asyncio.create_subprocess_exec(
                *self.tmux.tmux_cmd_for_session(session.tmux_session, "send-keys", "-t", session.tmux_session, resume_cmd, "Enter"),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=5)

            # Wait for Claude to start
            await asyncio.sleep(3.0)

            # Update session state
            session.recovery_count += 1
            session.last_activity = datetime.now()
            session.status = SessionStatus.IDLE  # Claude starts idle after resume
            self._save_state()

            logger.info(
                f"Crash recovery complete for session {session.id} "
                f"(recovery count: {session.recovery_count})"
            )
            return True

        except asyncio.TimeoutError:
            logger.error(f"Timeout during crash recovery for session {session.id}")
            return False
        except Exception as e:
            logger.error(f"Crash recovery failed for session {session.id}: {e}")
            return False
        finally:
            # 6. Always unpause message queue (even on failure)
            if self.message_queue_manager:
                self.message_queue_manager.unpause_session(session.id)
