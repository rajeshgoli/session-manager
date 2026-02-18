"""Data models for Claude Session Manager."""

from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from typing import Optional, List
import uuid


class SessionStatus(Enum):
    """Session lifecycle status."""
    RUNNING = "running"  # Actively working
    IDLE = "idle"        # Waiting for input
    STOPPED = "stopped"  # Terminated


class DeliveryMode(Enum):
    """Message delivery modes for sm send."""
    SEQUENTIAL = "sequential"
    IMPORTANT = "important"
    URGENT = "urgent"
    STEER = "steer"


class DeliveryResult(Enum):
    """Result of message delivery attempt."""
    DELIVERED = "delivered"  # Message was delivered immediately (session idle)
    QUEUED = "queued"        # Message was queued (session busy)
    FAILED = "failed"        # Message delivery failed


class NotificationChannel(Enum):
    """Available notification channels."""
    TELEGRAM = "telegram"
    EMAIL = "email"


class SubagentStatus(Enum):
    """Subagent lifecycle status."""
    RUNNING = "running"
    COMPLETED = "completed"
    ERROR = "error"


class CompletionStatus(Enum):
    """Session completion status (for child sessions)."""
    COMPLETED = "completed"
    ERROR = "error"
    ABANDONED = "abandoned"
    KILLED = "killed"


@dataclass
class ReviewConfig:
    """Configuration for a Codex review session."""
    mode: str  # "branch", "uncommitted", "commit", "custom"
    base_branch: Optional[str] = None
    commit_sha: Optional[str] = None
    custom_prompt: Optional[str] = None
    steer_text: Optional[str] = None
    steer_delivered: bool = False
    pr_number: Optional[int] = None
    pr_repo: Optional[str] = None
    pr_comment_id: Optional[int] = None

    def to_dict(self) -> dict:
        return {
            "mode": self.mode,
            "base_branch": self.base_branch,
            "commit_sha": self.commit_sha,
            "custom_prompt": self.custom_prompt,
            "steer_text": self.steer_text,
            "steer_delivered": self.steer_delivered,
            "pr_number": self.pr_number,
            "pr_repo": self.pr_repo,
            "pr_comment_id": self.pr_comment_id,
        }

    @classmethod
    def from_dict(cls, data: dict) -> "ReviewConfig":
        return cls(**{k: v for k, v in data.items() if k in cls.__dataclass_fields__})


@dataclass
class ReviewFinding:
    """A single finding from a code review."""
    title: str
    body: str
    priority: int  # 0-3 (P0 = critical, P3 = nitpick)
    confidence_score: Optional[float] = None
    file_path: Optional[str] = None
    line_start: Optional[int] = None
    line_end: Optional[int] = None

    def to_dict(self) -> dict:
        return {
            "title": self.title,
            "body": self.body,
            "priority": self.priority,
            "confidence_score": self.confidence_score,
            "file_path": self.file_path,
            "line_start": self.line_start,
            "line_end": self.line_end,
        }


@dataclass
class ReviewResult:
    """Structured result from a code review."""
    findings: List["ReviewFinding"]
    overall_correctness: Optional[str] = None
    overall_explanation: Optional[str] = None
    overall_confidence_score: Optional[float] = None
    raw_output: Optional[str] = None
    source: str = "tui"  # "tui" or "github_pr"

    def to_dict(self) -> dict:
        return {
            "findings": [f.to_dict() for f in self.findings],
            "overall_correctness": self.overall_correctness,
            "overall_explanation": self.overall_explanation,
            "overall_confidence_score": self.overall_confidence_score,
            "raw_output": self.raw_output,
            "source": self.source,
        }


@dataclass
class Subagent:
    """Represents a subagent spawned by a Claude session."""
    agent_id: str
    agent_type: str  # engineer, architect, explorer, etc.
    parent_session_id: str
    transcript_path: Optional[str] = None
    started_at: datetime = field(default_factory=datetime.now)
    stopped_at: Optional[datetime] = None
    status: SubagentStatus = SubagentStatus.RUNNING
    summary: Optional[str] = None  # Cached summary of work done

    def to_dict(self) -> dict:
        """Convert subagent to dictionary for JSON serialization."""
        return {
            "agent_id": self.agent_id,
            "agent_type": self.agent_type,
            "parent_session_id": self.parent_session_id,
            "transcript_path": self.transcript_path,
            "started_at": self.started_at.isoformat(),
            "stopped_at": self.stopped_at.isoformat() if self.stopped_at else None,
            "status": self.status.value,
            "summary": self.summary,
        }

    @classmethod
    def from_dict(cls, data: dict) -> "Subagent":
        """Create subagent from dictionary."""
        return cls(
            agent_id=data["agent_id"],
            agent_type=data["agent_type"],
            parent_session_id=data["parent_session_id"],
            transcript_path=data.get("transcript_path"),
            started_at=datetime.fromisoformat(data["started_at"]),
            stopped_at=datetime.fromisoformat(data["stopped_at"]) if data.get("stopped_at") else None,
            status=SubagentStatus(data["status"]),
            summary=data.get("summary"),
        )


@dataclass
class Session:
    """Represents a Claude Code session in tmux."""
    id: str = field(default_factory=lambda: uuid.uuid4().hex[:8])
    name: str = ""  # Internal identifier (auto-generated: claude-{id})
    working_dir: str = ""
    tmux_session: str = ""
    provider: str = "claude"  # "claude", "codex" (tmux), or "codex-app" (app-server)
    log_file: str = ""
    status: SessionStatus = SessionStatus.RUNNING
    created_at: datetime = field(default_factory=datetime.now)
    last_activity: datetime = field(default_factory=datetime.now)
    telegram_chat_id: Optional[int] = None
    telegram_thread_id: Optional[int] = None  # Thread/topic ID for threading (message_thread_id)
    error_message: Optional[str] = None
    transcript_path: Optional[str] = None  # Claude's transcript file path
    friendly_name: Optional[str] = None  # User-provided label for display
    current_task: Optional[str] = None  # What the session is currently working on
    git_remote_url: Optional[str] = None  # Git remote URL for repo matching
    subagents: List[Subagent] = field(default_factory=list)  # Subagents spawned by this session
    codex_thread_id: Optional[str] = None  # Codex app-server thread ID (if provider=codex-app)
    review_config: Optional[ReviewConfig] = None  # Active review configuration

    # Multi-agent coordination fields
    parent_session_id: Optional[str] = None  # Parent that spawned this session
    spawn_prompt: Optional[str] = None  # Initial prompt used to spawn
    completion_status: Optional[CompletionStatus] = None  # Completion state for child sessions
    completion_message: Optional[str] = None  # Message when completed
    spawned_at: Optional[datetime] = None  # When this session was spawned
    completed_at: Optional[datetime] = None  # When this session completed
    tokens_used: int = 0  # Approximate token count
    tools_used: dict[str, int] = field(default_factory=dict)  # {"Read": 5, "Write": 3}
    last_tool_call: Optional[datetime] = None  # Last tool usage timestamp

    # Lock management fields
    touched_repos: set[str] = field(default_factory=set)  # Repo roots this session has written to
    worktrees: list[str] = field(default_factory=list)  # Worktree paths created by this session
    cleanup_prompted: dict[str, str] = field(default_factory=dict)  # worktree -> last status hash prompted

    # Crash recovery fields
    recovery_count: int = 0  # Number of times this session has been auto-recovered

    def __post_init__(self):
        if not self.name:
            if self.provider == "claude":
                prefix = "claude"
            elif self.provider == "codex":
                prefix = "codex"
            else:
                prefix = "codex-app"
            self.name = f"{prefix}-{self.id}"
        if not self.tmux_session and self.provider in ("claude", "codex"):
            # IMPORTANT: tmux_session should ALWAYS be {provider}-{id}, not name
            self.tmux_session = f"{self.provider}-{self.id}"

    def to_dict(self) -> dict:
        """Convert session to dictionary for JSON serialization."""
        return {
            "id": self.id,
            "name": self.name,
            "working_dir": self.working_dir,
            "tmux_session": self.tmux_session,
            "provider": self.provider,
            "log_file": self.log_file,
            "status": self.status.value,
            "created_at": self.created_at.isoformat(),
            "last_activity": self.last_activity.isoformat(),
            "telegram_chat_id": self.telegram_chat_id,
            "telegram_thread_id": self.telegram_thread_id,
            "error_message": self.error_message,
            "transcript_path": self.transcript_path,
            "friendly_name": self.friendly_name,
            "current_task": self.current_task,
            "git_remote_url": self.git_remote_url,
            "subagents": [s.to_dict() for s in self.subagents],
            "codex_thread_id": self.codex_thread_id,
            "review_config": self.review_config.to_dict() if self.review_config else None,
            # Multi-agent coordination fields
            "parent_session_id": self.parent_session_id,
            "spawn_prompt": self.spawn_prompt,
            "completion_status": self.completion_status.value if self.completion_status else None,
            "completion_message": self.completion_message,
            "spawned_at": self.spawned_at.isoformat() if self.spawned_at else None,
            "completed_at": self.completed_at.isoformat() if self.completed_at else None,
            "tokens_used": self.tokens_used,
            "tools_used": self.tools_used,
            "last_tool_call": self.last_tool_call.isoformat() if self.last_tool_call else None,
            # Lock management fields
            "touched_repos": list(self.touched_repos),
            "worktrees": self.worktrees,
            "cleanup_prompted": self.cleanup_prompted,
            # Crash recovery fields
            "recovery_count": self.recovery_count,
        }

    @classmethod
    def from_dict(cls, data: dict) -> "Session":
        """Create session from dictionary."""
        subagents_data = data.get("subagents", [])
        subagents = [Subagent.from_dict(s) for s in subagents_data] if subagents_data else []

        # Backward compatibility: consolidate telegram_root_msg_id and telegram_topic_id
        telegram_thread_id = data.get("telegram_thread_id")
        if telegram_thread_id is None:
            # Fallback to old field names (prefer telegram_topic_id as it's more specific)
            telegram_thread_id = data.get("telegram_topic_id") or data.get("telegram_root_msg_id")

        # Backward compatibility: convert completion_status string to enum
        completion_status = data.get("completion_status")
        if completion_status is not None and isinstance(completion_status, str):
            completion_status = CompletionStatus(completion_status)

        # Backward compatibility: map removed status values to current ones
        raw_status = data["status"]
        status_mapping = {
            "starting": "running",
            "waiting_input": "idle",
            "waiting_permission": "idle",
            "error": "idle",  # Error state no longer exists, treat as idle
        }
        mapped_status = status_mapping.get(raw_status, raw_status)

        return cls(
            id=data["id"],
            name=data["name"],
            working_dir=data["working_dir"],
            tmux_session=data["tmux_session"],
            provider=data.get("provider", "claude"),
            log_file=data["log_file"],
            status=SessionStatus(mapped_status),
            created_at=datetime.fromisoformat(data["created_at"]),
            last_activity=datetime.fromisoformat(data["last_activity"]),
            telegram_chat_id=data.get("telegram_chat_id"),
            telegram_thread_id=telegram_thread_id,
            error_message=data.get("error_message"),
            transcript_path=data.get("transcript_path"),
            friendly_name=data.get("friendly_name"),
            current_task=data.get("current_task"),
            git_remote_url=data.get("git_remote_url"),
            subagents=subagents,
            codex_thread_id=data.get("codex_thread_id"),
            review_config=ReviewConfig.from_dict(data["review_config"]) if data.get("review_config") else None,
            # Multi-agent coordination fields
            parent_session_id=data.get("parent_session_id"),
            spawn_prompt=data.get("spawn_prompt"),
            completion_status=completion_status,
            completion_message=data.get("completion_message"),
            spawned_at=datetime.fromisoformat(data["spawned_at"]) if data.get("spawned_at") else None,
            completed_at=datetime.fromisoformat(data["completed_at"]) if data.get("completed_at") else None,
            tokens_used=data.get("tokens_used", 0),
            tools_used=data.get("tools_used", {}),
            last_tool_call=datetime.fromisoformat(data["last_tool_call"]) if data.get("last_tool_call") else None,
            # Lock management fields
            touched_repos=set(data.get("touched_repos", [])),
            worktrees=data.get("worktrees", []),
            cleanup_prompted=data.get("cleanup_prompted", {}),
            # Crash recovery fields
            recovery_count=data.get("recovery_count", 0),
        )


@dataclass
class NotificationEvent:
    """An event that should trigger a notification."""
    session_id: str
    event_type: str  # "permission_prompt", "idle", "error", "complete"
    message: str
    context: str = ""  # Recent output for context
    urgent: bool = False
    channel: Optional[NotificationChannel] = None  # None = use default
    review_result: Optional["ReviewResult"] = None  # Structured review data


@dataclass
class UserInput:
    """Input received from user via Telegram or Email."""
    session_id: str
    text: str
    source: NotificationChannel
    chat_id: Optional[int] = None
    message_id: Optional[int] = None
    is_permission_response: bool = False  # If True, bypass queue and send directly
    delivery_mode: str = "sequential"  # sequential, important, urgent


@dataclass
class QueuedMessage:
    """A message queued for delivery to a session."""
    id: str = field(default_factory=lambda: uuid.uuid4().hex)
    target_session_id: str = ""
    sender_session_id: Optional[str] = None
    sender_name: Optional[str] = None
    text: str = ""
    delivery_mode: str = "sequential"  # sequential, important, urgent
    queued_at: datetime = field(default_factory=datetime.now)
    timeout_at: Optional[datetime] = None  # None = no timeout
    notify_on_delivery: bool = False
    notify_after_seconds: Optional[int] = None  # None = no post-delivery notification
    notify_on_stop: bool = False  # Notify sender when receiver's Stop hook fires
    delivered_at: Optional[datetime] = None  # None = pending

    def to_dict(self) -> dict:
        """Convert to dictionary for JSON serialization."""
        return {
            "id": self.id,
            "target_session_id": self.target_session_id,
            "sender_session_id": self.sender_session_id,
            "sender_name": self.sender_name,
            "text": self.text,
            "delivery_mode": self.delivery_mode,
            "queued_at": self.queued_at.isoformat(),
            "timeout_at": self.timeout_at.isoformat() if self.timeout_at else None,
            "notify_on_delivery": self.notify_on_delivery,
            "notify_after_seconds": self.notify_after_seconds,
            "notify_on_stop": self.notify_on_stop,
            "delivered_at": self.delivered_at.isoformat() if self.delivered_at else None,
        }


@dataclass
class SessionDeliveryState:
    """Tracks delivery state for a session."""
    session_id: str
    is_idle: bool = False  # Set True by Stop hook, False on input injection
    last_idle_at: Optional[datetime] = None
    saved_user_input: Optional[str] = None  # Saved input during delivery
    pending_user_input: Optional[str] = None  # Currently detected input
    pending_input_first_seen: Optional[datetime] = None  # When we first saw the pending input
    stop_notify_sender_id: Optional[str] = None  # Sender to notify on Stop hook
    stop_notify_sender_name: Optional[str] = None  # Sender name for notification
    stop_notify_skip_count: int = 0  # Absorb /clear Stop hooks before firing notification
