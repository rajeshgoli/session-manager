"""Lock file management for multi-agent coordination fallback."""

import hashlib
import logging
import subprocess
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path
from typing import Optional

logger = logging.getLogger(__name__)

LOCK_FILE_NAME = ".claude/workspace.lock"
STALE_THRESHOLD_MINUTES = 30


def get_git_root(file_path: str) -> Optional[str]:
    """
    Find git repository root for a file path.

    Args:
        file_path: Path to a file (may not exist yet)

    Returns:
        Absolute path to git repo root, or None if not in a git repo
    """
    dir_path = Path(file_path).parent
    if not dir_path.exists():
        dir_path = dir_path.parent  # File might not exist yet

    try:
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            cwd=dir_path,
            capture_output=True,
            text=True,
            timeout=2
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except Exception:
        pass
    return None


def is_worktree(repo_root: str) -> bool:
    """
    Check if a path is a git worktree (not the main working tree).

    Args:
        repo_root: Path to check

    Returns:
        True if path is a worktree, False if main repo or not a git repo
    """
    try:
        # In a worktree, .git is a file pointing to the main repo
        # In main repo, .git is a directory
        git_path = Path(repo_root) / ".git"
        return git_path.is_file()
    except Exception:
        return False


def has_uncommitted_changes(repo_root: str) -> bool:
    """
    Check if a repository has uncommitted changes.

    Args:
        repo_root: Path to git repository root

    Returns:
        True if there are uncommitted changes, False otherwise
    """
    try:
        result = subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=repo_root,
            capture_output=True,
            text=True,
            timeout=5
        )
        return bool(result.stdout.strip())
    except Exception:
        return False


def get_worktree_status_hash(repo_root: str) -> Optional[str]:
    """
    Return a stable hash of the repo's working tree status.

    Args:
        repo_root: Path to git repository root

    Returns:
        Hex digest string when there are uncommitted changes, otherwise None.
    """
    try:
        result = subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=repo_root,
            capture_output=True,
            text=True,
            timeout=5
        )
        status = result.stdout.strip()
        if not status:
            return None
        return hashlib.sha256(status.encode("utf-8")).hexdigest()
    except Exception:
        return None


@dataclass
class LockInfo:
    """Information about a workspace lock."""
    session_id: str
    task: str
    branch: str
    started: datetime

    def is_stale(self) -> bool:
        """Check if lock is older than threshold."""
        age = datetime.now() - self.started
        return age > timedelta(minutes=STALE_THRESHOLD_MINUTES)


@dataclass
class LockResult:
    """Result of trying to acquire a lock."""
    acquired: bool
    locked_by_other: bool = False
    owner_session_id: Optional[str] = None


class LockManager:
    """Manages workspace lock files for agent coordination."""

    def __init__(self, working_dir: str = "."):
        """
        Initialize lock manager.

        Args:
            working_dir: Working directory (will find git root)
        """
        self.working_dir = Path(working_dir).resolve()
        self.repo_root = self._find_repo_root()
        self.lock_file = self.repo_root / LOCK_FILE_NAME if self.repo_root else None

    def _find_repo_root(self) -> Optional[Path]:
        """Find git repository root."""
        try:
            result = subprocess.run(
                ["git", "rev-parse", "--show-toplevel"],
                cwd=self.working_dir,
                capture_output=True,
                text=True,
                timeout=2,
            )
            if result.returncode == 0:
                return Path(result.stdout.strip())
            return None
        except Exception as e:
            logger.debug(f"Failed to find git root: {e}")
            return None

    def try_acquire(self, repo_root: str, session_id: str) -> LockResult:
        """
        Try to acquire lock on a specific repo root.

        Args:
            repo_root: Absolute path to git repository root
            session_id: Session ID attempting to acquire lock

        Returns:
            LockResult indicating success/failure and lock owner
        """
        lock_file = Path(repo_root) / LOCK_FILE_NAME

        # Check if lock exists
        existing = self._read_lock_file(lock_file)

        # If locked by another session (and not stale), return failure
        if existing and existing.session_id != session_id and not existing.is_stale():
            return LockResult(
                acquired=False,
                locked_by_other=True,
                owner_session_id=existing.session_id
            )

        # Acquire the lock
        try:
            lock_file.parent.mkdir(parents=True, exist_ok=True)
            branch = self._get_current_branch_for_path(repo_root)
            started = datetime.now().isoformat()

            with open(lock_file, "w") as f:
                f.write(f"session={session_id}\n")
                f.write(f"task=auto-acquired\n")
                f.write(f"branch={branch}\n")
                f.write(f"started={started}\n")

            logger.info(f"Lock acquired on {repo_root} by session {session_id}")
            return LockResult(acquired=True, locked_by_other=False)
        except Exception as e:
            logger.error(f"Failed to acquire lock on {repo_root}: {e}")
            return LockResult(acquired=False, locked_by_other=False)

    def _get_current_branch_for_path(self, repo_root: str) -> str:
        """Get current git branch for a specific repo path."""
        try:
            result = subprocess.run(
                ["git", "branch", "--show-current"],
                cwd=repo_root,
                capture_output=True,
                text=True,
                timeout=2,
            )
            if result.returncode == 0:
                return result.stdout.strip() or "unknown"
            return "unknown"
        except Exception:
            return "unknown"

    def _get_current_branch(self) -> str:
        """Get current git branch."""
        try:
            result = subprocess.run(
                ["git", "branch", "--show-current"],
                cwd=self.working_dir,
                capture_output=True,
                text=True,
                timeout=2,
            )
            if result.returncode == 0:
                return result.stdout.strip() or "unknown"
            return "unknown"
        except Exception:
            return "unknown"

    def acquire_lock(self, session_id: str, task: str) -> bool:
        """
        Acquire a workspace lock.

        Args:
            session_id: Session ID acquiring lock
            task: Task description

        Returns:
            True if lock acquired, False if lock exists
        """
        if not self.lock_file:
            logger.warning("Not in a git repository, cannot acquire lock")
            return False

        # Check if lock already exists
        existing_lock = self.check_lock()
        if existing_lock and not existing_lock.is_stale():
            logger.info(f"Lock already held by session {existing_lock.session_id}")
            return False

        # Create .claude directory if needed
        self.lock_file.parent.mkdir(parents=True, exist_ok=True)

        # Write lock file
        branch = self._get_current_branch()
        started = datetime.now().isoformat()

        try:
            with open(self.lock_file, "w") as f:
                f.write(f"session={session_id}\n")
                f.write(f"task={task}\n")
                f.write(f"branch={branch}\n")
                f.write(f"started={started}\n")
            logger.info(f"Lock acquired by session {session_id}")
            return True
        except Exception as e:
            logger.error(f"Failed to write lock file: {e}")
            return False

    def release_lock(self, repo_root: Optional[str] = None, session_id: Optional[str] = None) -> bool:
        """
        Release a workspace lock.

        Args:
            repo_root: Optional repo root path (if None, uses self.lock_file for backward compatibility)
            session_id: Optional session ID (only releases if it owns the lock)

        Returns:
            True if lock released or didn't exist
        """
        # Determine lock file to release
        if repo_root:
            lock_file = Path(repo_root) / LOCK_FILE_NAME
        else:
            lock_file = self.lock_file

        if not lock_file or not lock_file.exists():
            return True

        # If session_id provided, verify ownership
        if session_id:
            existing_lock = self._read_lock_file(lock_file)
            if existing_lock and existing_lock.session_id != session_id:
                logger.warning(
                    f"Lock on {repo_root or 'workspace'} held by {existing_lock.session_id}, "
                    f"not releasing for {session_id}"
                )
                return False

        try:
            lock_file.unlink()
            logger.info(f"Lock released on {repo_root or 'workspace'}")
            return True
        except Exception as e:
            logger.error(f"Failed to release lock on {repo_root or 'workspace'}: {e}")
            return False

    def _read_lock_file(self, lock_file: Path) -> Optional[LockInfo]:
        """
        Read lock info from a specific lock file.

        Args:
            lock_file: Path to lock file

        Returns:
            LockInfo if lock exists and is valid, None otherwise
        """
        if not lock_file.exists():
            return None

        try:
            with open(lock_file) as f:
                lines = f.readlines()

            lock_data = {}
            for line in lines:
                line = line.strip()
                if "=" in line:
                    key, value = line.split("=", 1)
                    lock_data[key] = value

            if not all(k in lock_data for k in ["session", "task", "branch", "started"]):
                logger.warning(f"Invalid lock file format: {lock_file}")
                return None

            return LockInfo(
                session_id=lock_data["session"],
                task=lock_data["task"],
                branch=lock_data["branch"],
                started=datetime.fromisoformat(lock_data["started"]),
            )
        except Exception as e:
            logger.error(f"Failed to read lock file {lock_file}: {e}")
            return None

    def check_lock(self) -> Optional[LockInfo]:
        """
        Check if a lock exists (for backward compatibility).

        Returns:
            LockInfo if lock exists, None otherwise
        """
        if not self.lock_file:
            return None
        return self._read_lock_file(self.lock_file)

    def is_locked(self) -> bool:
        """
        Check if workspace is locked (and not stale).

        Returns:
            True if locked by another active session
        """
        lock = self.check_lock()
        return lock is not None and not lock.is_stale()
