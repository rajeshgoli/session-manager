"""Data models for Claude Session Manager."""

from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from typing import Optional, List
import uuid


class SessionStatus(Enum):
    """Session lifecycle status."""
    STARTING = "starting"
    RUNNING = "running"
    WAITING_INPUT = "waiting_input"
    WAITING_PERMISSION = "waiting_permission"
    IDLE = "idle"
    STOPPED = "stopped"
    ERROR = "error"


class DeliveryMode(Enum):
    """Message delivery modes for sm send."""
    SEQUENTIAL = "sequential"
    IMPORTANT = "important"
    URGENT = "urgent"


class NotificationChannel(Enum):
    """Available notification channels."""
    TELEGRAM = "telegram"
    EMAIL = "email"


class SubagentStatus(Enum):
    """Subagent lifecycle status."""
    RUNNING = "running"
    COMPLETED = "completed"
    ERROR = "error"


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
    name: str = ""
    working_dir: str = ""
    tmux_session: str = ""
    log_file: str = ""
    status: SessionStatus = SessionStatus.STARTING
    created_at: datetime = field(default_factory=datetime.now)
    last_activity: datetime = field(default_factory=datetime.now)
    telegram_chat_id: Optional[int] = None
    telegram_root_msg_id: Optional[int] = None
    telegram_topic_id: Optional[int] = None  # Forum topic ID (message_thread_id)
    error_message: Optional[str] = None
    transcript_path: Optional[str] = None  # Claude's transcript file path
    friendly_name: Optional[str] = None  # User-friendly name
    current_task: Optional[str] = None  # What the session is currently working on
    git_remote_url: Optional[str] = None  # Git remote URL for repo matching
    subagents: List[Subagent] = field(default_factory=list)  # Subagents spawned by this session

    # Multi-agent coordination fields
    parent_session_id: Optional[str] = None  # Parent that spawned this session
    spawn_prompt: Optional[str] = None  # Initial prompt used to spawn
    completion_status: Optional[str] = None  # completed, error, abandoned, killed
    completion_message: Optional[str] = None  # Message when completed
    spawned_at: Optional[datetime] = None  # When this session was spawned
    completed_at: Optional[datetime] = None  # When this session completed
    tokens_used: int = 0  # Approximate token count
    tools_used: dict[str, int] = field(default_factory=dict)  # {"Read": 5, "Write": 3}
    last_tool_call: Optional[datetime] = None  # Last tool usage timestamp

    def __post_init__(self):
        if not self.name:
            self.name = f"claude-{self.id}"
        if not self.tmux_session:
            self.tmux_session = self.name

    def to_dict(self) -> dict:
        """Convert session to dictionary for JSON serialization."""
        return {
            "id": self.id,
            "name": self.name,
            "working_dir": self.working_dir,
            "tmux_session": self.tmux_session,
            "log_file": self.log_file,
            "status": self.status.value,
            "created_at": self.created_at.isoformat(),
            "last_activity": self.last_activity.isoformat(),
            "telegram_chat_id": self.telegram_chat_id,
            "telegram_root_msg_id": self.telegram_root_msg_id,
            "telegram_topic_id": self.telegram_topic_id,
            "error_message": self.error_message,
            "transcript_path": self.transcript_path,
            "friendly_name": self.friendly_name,
            "current_task": self.current_task,
            "git_remote_url": self.git_remote_url,
            "subagents": [s.to_dict() for s in self.subagents],
            # Multi-agent coordination fields
            "parent_session_id": self.parent_session_id,
            "spawn_prompt": self.spawn_prompt,
            "completion_status": self.completion_status,
            "completion_message": self.completion_message,
            "spawned_at": self.spawned_at.isoformat() if self.spawned_at else None,
            "completed_at": self.completed_at.isoformat() if self.completed_at else None,
            "tokens_used": self.tokens_used,
            "tools_used": self.tools_used,
            "last_tool_call": self.last_tool_call.isoformat() if self.last_tool_call else None,
        }

    @classmethod
    def from_dict(cls, data: dict) -> "Session":
        """Create session from dictionary."""
        subagents_data = data.get("subagents", [])
        subagents = [Subagent.from_dict(s) for s in subagents_data] if subagents_data else []

        return cls(
            id=data["id"],
            name=data["name"],
            working_dir=data["working_dir"],
            tmux_session=data["tmux_session"],
            log_file=data["log_file"],
            status=SessionStatus(data["status"]),
            created_at=datetime.fromisoformat(data["created_at"]),
            last_activity=datetime.fromisoformat(data["last_activity"]),
            telegram_chat_id=data.get("telegram_chat_id"),
            telegram_root_msg_id=data.get("telegram_root_msg_id"),
            telegram_topic_id=data.get("telegram_topic_id"),
            error_message=data.get("error_message"),
            transcript_path=data.get("transcript_path"),
            friendly_name=data.get("friendly_name"),
            current_task=data.get("current_task"),
            git_remote_url=data.get("git_remote_url"),
            subagents=subagents,
            # Multi-agent coordination fields
            parent_session_id=data.get("parent_session_id"),
            spawn_prompt=data.get("spawn_prompt"),
            completion_status=data.get("completion_status"),
            completion_message=data.get("completion_message"),
            spawned_at=datetime.fromisoformat(data["spawned_at"]) if data.get("spawned_at") else None,
            completed_at=datetime.fromisoformat(data["completed_at"]) if data.get("completed_at") else None,
            tokens_used=data.get("tokens_used", 0),
            tools_used=data.get("tools_used", {}),
            last_tool_call=datetime.fromisoformat(data["last_tool_call"]) if data.get("last_tool_call") else None,
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


@dataclass
class UserInput:
    """Input received from user via Telegram or Email."""
    session_id: str
    text: str
    source: NotificationChannel
    chat_id: Optional[int] = None
    message_id: Optional[int] = None
