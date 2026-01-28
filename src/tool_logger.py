"""Tool usage logging for security audit and analytics."""

import sqlite3
import json
import re
from datetime import datetime
from pathlib import Path
from typing import Optional
import logging

logger = logging.getLogger(__name__)

# Patterns for sensitive/destructive operations
DESTRUCTIVE_PATTERNS = [
    # Git operations
    (r"git\s+push.*(?:main|master)", "git_push_main"),
    (r"git\s+push\s+--force", "git_push_force"),
    (r"git\s+reset\s+--hard", "git_reset_hard"),
    (r"git\s+branch\s+-[dD]", "git_branch_delete"),

    # File operations
    (r"rm\s+-rf?\s+/", "rm_root"),
    (r"rm\s+-rf", "rm_recursive"),
    (r"chmod\s+777", "chmod_777"),
    (r"chown", "chown"),

    # Database operations
    (r"DROP\s+TABLE", "drop_table"),
    (r"DROP\s+DATABASE", "drop_database"),
    (r"DELETE\s+FROM.*WHERE\s+1\s*=\s*1", "delete_all"),
    (r"TRUNCATE", "truncate"),

    # Package management
    (r"npm\s+install\s+-g", "npm_global_install"),
    (r"pip\s+install", "pip_install"),
    (r"brew\s+install", "brew_install"),

    # System operations
    (r"sudo\s+", "sudo"),
    (r"systemctl\s+(?:stop|disable|restart)", "systemctl"),

    # Sensitive files
    (r"\.env", "env_file"),
    (r"credentials", "credentials_file"),
    (r"\.ssh", "ssh_file"),
    (r"id_rsa", "ssh_key"),
]

SENSITIVE_FILE_PATTERNS = [
    r"\.env",
    r"\.env\.\w+",
    r"credentials",
    r"secrets",
    r"\.ssh/",
    r"id_rsa",
    r"\.aws/",
    r"\.npmrc",
    r"\.pypirc",
]


class ToolLogger:
    """Logs tool usage to SQLite database."""

    def __init__(self, db_path: str = "~/.local/share/claude-sessions/tool_usage.db"):
        self.db_path = Path(db_path).expanduser()
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._init_db()

    def _init_db(self):
        """Initialize database schema."""
        conn = sqlite3.connect(self.db_path)
        cursor = conn.cursor()

        cursor.execute("""
            CREATE TABLE IF NOT EXISTS tool_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,

                -- Session info (ours)
                session_id TEXT,              -- Our CLAUDE_SESSION_MANAGER_ID
                session_name TEXT,
                parent_session_id TEXT,

                -- Session info (Claude's native)
                claude_session_id TEXT,       -- Claude Code's internal session ID
                tool_use_id TEXT,             -- For correlating PreToolUse/PostToolUse
                cwd TEXT,                     -- Working directory at time of call
                project_name TEXT,            -- Derived from cwd (last path component)
                agent_id TEXT,                -- Subagent ID if this is a subagent call

                -- Hook info
                hook_type TEXT NOT NULL,      -- PreToolUse or PostToolUse

                -- Tool info
                tool_name TEXT NOT NULL,
                tool_input TEXT,              -- JSON
                tool_response TEXT,           -- JSON (PostToolUse only)

                -- Derived fields
                is_destructive BOOLEAN DEFAULT 0,
                destructive_type TEXT,        -- e.g., "git_push_main", "rm_recursive"
                is_sensitive_file BOOLEAN DEFAULT 0,
                target_file TEXT,             -- For file operations
                bash_command TEXT,            -- For Bash tool
                exit_code INTEGER             -- For Bash PostToolUse
            )
        """)

        # Indexes for common queries
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_session ON tool_usage(session_id)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_tool ON tool_usage(tool_name)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_destructive ON tool_usage(is_destructive)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_timestamp ON tool_usage(timestamp)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_hook_type ON tool_usage(hook_type)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_tool_use_id ON tool_usage(tool_use_id)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_agent_id ON tool_usage(agent_id)")
        cursor.execute("CREATE INDEX IF NOT EXISTS idx_project_name ON tool_usage(project_name)")

        conn.commit()
        conn.close()

    def _detect_destructive(self, tool_name: str, tool_input: dict) -> tuple[bool, Optional[str]]:
        """Detect if operation is destructive."""
        text_to_check = ""

        if tool_name == "Bash":
            text_to_check = tool_input.get("command", "")
        elif tool_name in ("Write", "Edit", "Read"):
            text_to_check = tool_input.get("file_path", "")

        for pattern, dtype in DESTRUCTIVE_PATTERNS:
            if re.search(pattern, text_to_check, re.IGNORECASE):
                return True, dtype

        return False, None

    def _detect_sensitive_file(self, tool_name: str, tool_input: dict) -> tuple[bool, Optional[str]]:
        """Detect if operation involves sensitive files."""
        file_path = None

        if tool_name in ("Write", "Edit", "Read"):
            file_path = tool_input.get("file_path", "")
        elif tool_name == "Bash":
            # Try to extract file paths from command
            command = tool_input.get("command", "")
            file_path = command  # Check whole command

        if file_path:
            for pattern in SENSITIVE_FILE_PATTERNS:
                if re.search(pattern, file_path, re.IGNORECASE):
                    return True, file_path

        return False, None

    async def log(
        self,
        session_id: Optional[str],
        claude_session_id: Optional[str],
        session_name: Optional[str],
        parent_session_id: Optional[str],
        hook_type: str,
        tool_name: str,
        tool_input: dict,
        tool_response: Optional[dict] = None,
        tool_use_id: Optional[str] = None,
        cwd: Optional[str] = None,
        agent_id: Optional[str] = None,
    ):
        """Log a tool usage event."""
        try:
            # Detect destructive operations
            is_destructive, destructive_type = self._detect_destructive(tool_name, tool_input)

            # Detect sensitive file access
            is_sensitive, target_file = self._detect_sensitive_file(tool_name, tool_input)

            # Extract bash command
            bash_command = None
            if tool_name == "Bash":
                bash_command = tool_input.get("command")

            # Extract exit code (note: Claude uses camelCase "exitCode")
            exit_code = None
            if tool_response and tool_name == "Bash":
                exit_code = tool_response.get("exitCode")

            # Extract target file for file operations
            if tool_name in ("Write", "Edit", "Read") and not target_file:
                target_file = tool_input.get("file_path")

            # Derive project name from cwd (last path component)
            project_name = None
            if cwd:
                project_name = Path(cwd).name

            conn = sqlite3.connect(self.db_path)
            cursor = conn.cursor()

            cursor.execute("""
                INSERT INTO tool_usage (
                    session_id, claude_session_id, session_name, parent_session_id,
                    tool_use_id, cwd, project_name, agent_id,
                    hook_type, tool_name, tool_input, tool_response,
                    is_destructive, destructive_type, is_sensitive_file,
                    target_file, bash_command, exit_code
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """, (
                session_id, claude_session_id, session_name, parent_session_id,
                tool_use_id, cwd, project_name, agent_id,
                hook_type, tool_name,
                json.dumps(tool_input) if tool_input else None,
                json.dumps(tool_response) if tool_response else None,
                is_destructive, destructive_type, is_sensitive,
                target_file, bash_command, exit_code,
            ))

            conn.commit()
            conn.close()

            # Log warning for destructive operations
            if is_destructive:
                logger.warning(
                    f"Destructive operation detected: {destructive_type} "
                    f"by session {session_name or session_id}"
                )

        except Exception as e:
            logger.error(f"Failed to log tool usage: {e}")
