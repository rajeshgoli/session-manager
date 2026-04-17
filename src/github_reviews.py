"""GitHub PR review integration via gh CLI.

All functions are synchronous (`subprocess.run`). Async callers should wrap them
with `asyncio.to_thread()` to keep the event loop responsive.
"""

from __future__ import annotations

import json
import logging
import subprocess
import time
from datetime import datetime, timezone
from typing import Any, Optional

logger = logging.getLogger(__name__)


def _parse_github_datetime(value: Optional[str]) -> Optional[datetime]:
    """Parse one GitHub ISO timestamp into an aware UTC datetime."""
    if not value:
        return None
    normalized = value.strip()
    if normalized.endswith("Z"):
        normalized = normalized[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def _coerce_utc(dt: datetime) -> datetime:
    """Normalize one datetime for safe GitHub timestamp comparisons."""
    if dt.tzinfo is None:
        return dt.replace(tzinfo=timezone.utc)
    return dt.astimezone(timezone.utc)


def _github_since_value(dt: datetime) -> str:
    """Render one UTC timestamp for GitHub REST `since=` parameters."""
    return _coerce_utc(dt).strftime("%Y-%m-%dT%H:%M:%SZ")


def _split_repo(repo: str) -> tuple[str, str]:
    """Split owner/repo into its path components."""
    try:
        return repo.split("/", 1)
    except ValueError as exc:
        raise RuntimeError(f"Invalid repo {repo!r}; expected owner/repo") from exc


def _gh_api_json(repo: str, endpoint: str, *, paginate: bool = False) -> Any:
    """Call `gh api` for one repo-scoped endpoint and decode JSON."""
    owner, repo_name = _split_repo(repo)
    command = [
        "gh",
        "api",
        f"repos/{owner}/{repo_name}/{endpoint.lstrip('/')}",
        "-H",
        "Accept: application/vnd.github+json",
    ]
    if paginate:
        command.extend(["--paginate", "--slurp"])

    result = subprocess.run(
        command,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode != 0:
        raise RuntimeError(f"gh api failed: {result.stderr.strip()}")
    payload = result.stdout.strip()
    if not payload:
        return None
    decoded = json.loads(payload)
    if paginate and isinstance(decoded, list):
        flattened: list[Any] = []
        for page in decoded:
            if isinstance(page, list):
                flattened.extend(page)
            else:
                flattened.append(page)
        return flattened
    return decoded


def is_codex_actor(payload: Optional[dict]) -> bool:
    """Return True when a GitHub review/comment/reaction actor looks like Codex."""
    if not isinstance(payload, dict):
        return False

    user_login = str(payload.get("user", {}).get("login", "")).lower()
    if "codex" in user_login:
        return True

    app = payload.get("performed_via_github_app") or {}
    for field_name in ("slug", "name"):
        field_value = str(app.get(field_name, "")).lower()
        if "codex" in field_value:
            return True

    return False


def post_pr_review_comment(
    repo: str,
    pr_number: int,
    steer: Optional[str] = None,
) -> dict:
    """Post `@codex review` on a PR and return the created issue comment metadata."""
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

    posted_at = datetime.now(timezone.utc).isoformat()
    comment_id = 0
    comment_url = result.stdout.strip() or None
    if comment_url and "#issuecomment-" in comment_url:
        try:
            comment_id = int(comment_url.split("#issuecomment-")[-1])
        except (ValueError, IndexError):
            pass

    return {
        "comment_id": comment_id,
        "comment_url": comment_url,
        "body": body,
        "posted_at": posted_at,
    }


def validate_open_pr(repo: str, pr_number: int) -> dict:
    """Validate that one PR exists and is open."""
    result = subprocess.run(
        [
            "gh",
            "pr",
            "view",
            str(pr_number),
            "--repo",
            repo,
            "--json",
            "number,state,title,url",
        ],
        capture_output=True,
        text=True,
        timeout=10,
    )
    if result.returncode != 0:
        raise RuntimeError(f"PR #{pr_number} not found in {repo}: {result.stderr.strip()}")

    payload = json.loads(result.stdout or "{}")
    state = payload.get("state")
    if state != "OPEN":
        raise RuntimeError(f"PR #{pr_number} is {state or 'unknown'}, not OPEN")
    return payload


def fetch_issue_comment(repo: str, comment_id: int) -> Optional[dict]:
    """Fetch one GitHub issue comment by id."""
    if comment_id <= 0:
        return None
    payload = _gh_api_json(repo, f"issues/comments/{comment_id}")
    return payload if isinstance(payload, dict) else None


def fetch_issue_comment_reactions(repo: str, comment_id: int) -> list[dict]:
    """Fetch explicit reactions for one issue comment."""
    if comment_id <= 0:
        return []
    payload = _gh_api_json(repo, f"issues/comments/{comment_id}/reactions", paginate=True)
    return payload if isinstance(payload, list) else []


def fetch_pr_issue_comments(
    repo: str,
    pr_number: int,
    *,
    since: Optional[datetime] = None,
) -> list[dict]:
    """Fetch issue comments for one PR, optionally filtered by update time."""
    endpoint = f"issues/{pr_number}/comments"
    if since is not None:
        endpoint = f"{endpoint}?since={_github_since_value(since)}"
    payload = _gh_api_json(repo, endpoint, paginate=True)
    return payload if isinstance(payload, list) else []


def fetch_pr_reviews(repo: str, pr_number: int) -> list[dict]:
    """Fetch all PR reviews for one PR."""
    payload = _gh_api_json(repo, f"pulls/{pr_number}/reviews", paginate=True)
    return payload if isinstance(payload, list) else []


def detect_codex_pickup(repo: str, comment_id: int) -> bool:
    """Return True when Codex reacted with eyes on the request comment."""
    for reaction in fetch_issue_comment_reactions(repo, comment_id):
        if reaction.get("content") == "eyes" and is_codex_actor(reaction):
            return True
    return False


def get_codex_request_reaction_state(repo: str, comment_id: int) -> dict[str, bool]:
    """Return pickup/clean-pass reaction state for one Codex review request comment."""
    picked_up = False
    clean_pass = False
    for reaction in fetch_issue_comment_reactions(repo, comment_id):
        if not is_codex_actor(reaction):
            continue
        if reaction.get("content") == "eyes":
            picked_up = True
        elif reaction.get("content") == "+1":
            clean_pass = True
    return {"picked_up": picked_up, "clean_pass": clean_pass}


def find_fresh_codex_review_or_comment(
    repo: str,
    pr_number: int,
    since: datetime,
) -> Optional[dict]:
    """Find the first fresh Codex review or issue comment newer than `since`."""
    since_utc = _coerce_utc(since)
    candidates: list[dict[str, Any]] = []

    latest_review = fetch_latest_codex_review(repo, pr_number)
    if latest_review:
        submitted_at = _parse_github_datetime(latest_review.get("submitted_at"))
        if submitted_at and submitted_at > since_utc:
            candidates.append(
                {
                    "source": "review",
                    "created_at": submitted_at,
                    "id": latest_review.get("id"),
                    "url": latest_review.get("html_url") or latest_review.get("pull_request_url"),
                    "state": latest_review.get("state"),
                    "body": latest_review.get("body"),
                    "actor": latest_review.get("user", {}).get("login"),
                }
            )

    for comment in fetch_pr_issue_comments(repo, pr_number, since=since_utc):
        created_at = _parse_github_datetime(comment.get("created_at"))
        if created_at and created_at > since_utc and is_codex_actor(comment):
            candidates.append(
                {
                    "source": "comment",
                    "created_at": created_at,
                    "id": comment.get("id"),
                    "url": comment.get("html_url"),
                    "state": None,
                    "body": comment.get("body"),
                    "actor": comment.get("user", {}).get("login"),
                }
            )

    if not candidates:
        return None

    candidates.sort(key=lambda item: item["created_at"])
    winner = candidates[0]
    winner["created_at"] = winner["created_at"].isoformat()
    return winner


def poll_for_codex_review(
    repo: str,
    pr_number: int,
    since: datetime,
    timeout: int = 600,
    poll_interval: int = 30,
) -> Optional[dict]:
    """Poll for a fresh Codex PR review (not issue comment) on one PR."""
    since_utc = _coerce_utc(since)
    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        try:
            review = fetch_latest_codex_review(repo, pr_number)
            if review:
                user_login = review.get("user", {}).get("login", "")
                submitted_at = _parse_github_datetime(review.get("submitted_at"))
                if submitted_at and "codex" in user_login.lower() and submitted_at > since_utc:
                    logger.info(
                        "Found Codex review on PR #%s: user=%s submitted_at=%s",
                        pr_number,
                        user_login,
                        review.get("submitted_at"),
                    )
                    return review
        except RuntimeError as exc:
            logger.warning("Error polling for Codex review on PR #%s: %s", pr_number, exc)
        except Exception as exc:  # pragma: no cover - defensive guard
            logger.warning("Unexpected error polling for Codex review on PR #%s: %s", pr_number, exc)

        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        time.sleep(min(poll_interval, remaining))

    logger.info("Timeout polling for Codex review on PR #%s after %ss", pr_number, timeout)
    return None


def fetch_latest_codex_review(repo: str, pr_number: int) -> Optional[dict]:
    """Fetch the most recent Codex-authored PR review on one PR."""
    try:
        result = subprocess.run(
            [
                "gh",
                "pr",
                "view",
                str(pr_number),
                "--repo",
                repo,
                "--json",
                "url,latestReviews",
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode != 0:
            raise RuntimeError(f"gh pr view failed: {result.stderr.strip()}")
        payload = json.loads(result.stdout or "{}")
        pr_url = payload.get("url")
        latest_reviews = payload.get("latestReviews") or []
        freshest_match = None
        freshest_submitted_at = None
        for review in latest_reviews:
            author = (review.get("author") or {}).get("login", "")
            if "codex" not in author.lower():
                continue
            submitted_at = _parse_github_datetime(review.get("submittedAt"))
            if submitted_at is None:
                continue
            if freshest_submitted_at is None or submitted_at > freshest_submitted_at:
                freshest_submitted_at = submitted_at
                freshest_match = {
                    "id": review.get("id"),
                    "user": {"login": author},
                    "submitted_at": review.get("submittedAt"),
                    "state": review.get("state"),
                    "body": review.get("body"),
                    "html_url": review.get("url") or pr_url,
                    "pull_request_url": pr_url,
                }
        if freshest_match:
            return freshest_match
    except Exception as exc:
        logger.warning("Failed to fetch Codex review for PR #%s: %s", pr_number, exc)
    return None


def get_pr_repo_from_git(working_dir: str) -> Optional[str]:
    """Infer owner/repo from the current git checkout in one working directory."""
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
    except Exception as exc:
        logger.debug("Failed to get repo from git in %s: %s", working_dir, exc)
        return None
