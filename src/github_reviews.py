"""GitHub PR review integration via gh CLI.

All functions are synchronous (subprocess.run). The server wraps them
with asyncio.to_thread() where needed for non-blocking execution.
"""

import json
import logging
import subprocess
import time
from datetime import datetime
from typing import Optional

logger = logging.getLogger(__name__)


def post_pr_review_comment(
    repo: str,
    pr_number: int,
    steer: Optional[str] = None,
) -> dict:
    """Post @codex review comment on a PR.

    Args:
        repo: GitHub repo in owner/repo format
        pr_number: PR number
        steer: Optional focus instructions appended to the comment

    Returns:
        {"comment_id": int, "body": str, "posted_at": str (ISO)}

    Raises:
        RuntimeError: If gh CLI fails
    """
    if steer:
        body = f"@codex review for {steer}"
    else:
        body = "@codex review"

    result = subprocess.run(
        ["gh", "pr", "comment", str(pr_number), "--repo", repo, "--body", body],
        capture_output=True,
        text=True,
        timeout=30,
    )

    if result.returncode != 0:
        raise RuntimeError(f"gh pr comment failed: {result.stderr.strip()}")

    posted_at = datetime.now().isoformat()

    # gh pr comment prints the comment URL on success; parse comment ID from it
    # URL format: https://github.com/owner/repo/pull/N#issuecomment-NNNNN
    comment_id = 0
    output = result.stdout.strip()
    if "#issuecomment-" in output:
        try:
            comment_id = int(output.split("#issuecomment-")[-1])
        except (ValueError, IndexError):
            pass

    return {
        "comment_id": comment_id,
        "body": body,
        "posted_at": posted_at,
    }


def poll_for_codex_review(
    repo: str,
    pr_number: int,
    since: datetime,
    timeout: int = 600,
    poll_interval: int = 30,
) -> Optional[dict]:
    """Poll for a Codex review on a PR.

    Synchronous function using subprocess.run in a loop with time.sleep.
    Callable from sync CLI code or wrapped with asyncio.to_thread() on the server.

    Args:
        repo: GitHub repo in owner/repo format
        pr_number: PR number
        since: Only consider reviews submitted after this time
        timeout: Maximum seconds to poll (default 600)
        poll_interval: Seconds between polls (default 30)

    Returns:
        Review data dict or None on timeout
    """
    since_iso = since.isoformat()
    deadline = time.monotonic() + timeout

    owner, repo_name = repo.split("/", 1)

    while time.monotonic() < deadline:
        try:
            result = subprocess.run(
                [
                    "gh", "api",
                    f"repos/{owner}/{repo_name}/pulls/{pr_number}/reviews",
                    "--jq", ".",
                ],
                capture_output=True,
                text=True,
                timeout=30,
            )

            if result.returncode == 0 and result.stdout.strip():
                reviews = json.loads(result.stdout)
                for review in reviews:
                    user_login = review.get("user", {}).get("login", "")
                    submitted_at = review.get("submitted_at", "")

                    # Match codex[bot] reviews submitted after our comment
                    if "codex" in user_login.lower() and submitted_at > since_iso:
                        logger.info(
                            f"Found Codex review on PR #{pr_number}: "
                            f"user={user_login}, submitted_at={submitted_at}"
                        )
                        return review

        except subprocess.TimeoutExpired:
            logger.warning(f"gh api call timed out while polling PR #{pr_number}")
        except (json.JSONDecodeError, Exception) as e:
            logger.warning(f"Error polling for Codex review on PR #{pr_number}: {e}")

        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        time.sleep(min(poll_interval, remaining))

    logger.info(f"Timeout polling for Codex review on PR #{pr_number} after {timeout}s")
    return None


def fetch_latest_codex_review(repo: str, pr_number: int) -> Optional[dict]:
    """Fetch the most recent Codex review from a GitHub PR.

    Args:
        repo: GitHub repo in owner/repo format
        pr_number: PR number

    Returns:
        Review data dict or None if no Codex review found
    """
    owner, repo_name = repo.split("/", 1)
    try:
        result = subprocess.run(
            [
                "gh", "api",
                f"repos/{owner}/{repo_name}/pulls/{pr_number}/reviews",
                "--jq", ".",
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode == 0 and result.stdout.strip():
            reviews = json.loads(result.stdout)
            for review in reversed(reviews):
                if "codex" in review.get("user", {}).get("login", "").lower():
                    return review
    except (subprocess.TimeoutExpired, json.JSONDecodeError, Exception) as e:
        logger.warning(f"Failed to fetch Codex review for PR #{pr_number}: {e}")
    return None


def get_pr_repo_from_git(working_dir: str) -> Optional[str]:
    """Infer owner/repo from the git remote in working_dir.

    Args:
        working_dir: Directory to check

    Returns:
        owner/repo string or None if not a git repo or gh fails
    """
    try:
        result = subprocess.run(
            ["gh", "repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"],
            cwd=working_dir,
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip()
        return None
    except Exception as e:
        logger.debug(f"Failed to get repo from git in {working_dir}: {e}")
        return None
