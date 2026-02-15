"""Parse review output from Codex TUI and GitHub PR reviews."""

import json
import logging
import re
import subprocess
from typing import Optional

from .models import ReviewFinding, ReviewResult
from .notifier import strip_ansi

logger = logging.getLogger(__name__)

# --- TUI output parsing ---
# TODO: Patterns based on spec schema -- verify against real Codex TUI output

# Match [P0]-[P3] headers: "[P0] Some finding title"
PRIORITY_HEADER_RE = re.compile(r'\[P(\d)\]\s+(.+)')

# Match overall confidence score line
CONFIDENCE_RE = re.compile(r'overall_confidence_score[:\s]+(\d+\.?\d*)', re.IGNORECASE)

# Match overall correctness line (e.g., "Correctness: mostly correct")
CORRECTNESS_RE = re.compile(r'(?:overall_)?correctness:\s*(.+)', re.IGNORECASE)


def parse_tui_output(raw_output: str) -> ReviewResult:
    """Parse Codex TUI review output into structured ReviewResult.

    Strips ANSI codes, then looks for [P0]-[P3] prefixed headers
    and body text between them.

    Args:
        raw_output: Raw terminal output (may contain ANSI codes)

    Returns:
        ReviewResult with parsed findings
    """
    clean = strip_ansi(raw_output)
    lines = clean.split('\n')

    findings: list[ReviewFinding] = []
    overall_confidence: Optional[float] = None
    overall_correctness: Optional[str] = None
    overall_explanation: Optional[str] = None

    current_title: Optional[str] = None
    current_priority: Optional[int] = None
    current_body_lines: list[str] = []

    def _flush_finding():
        nonlocal current_title, current_priority, current_body_lines
        if current_title is not None and current_priority is not None:
            body = '\n'.join(current_body_lines).strip()
            findings.append(ReviewFinding(
                title=current_title,
                body=body,
                priority=current_priority,
            ))
        current_title = None
        current_priority = None
        current_body_lines = []

    for line in lines:
        # Check for priority header
        m = PRIORITY_HEADER_RE.match(line.strip())
        if m:
            _flush_finding()
            current_priority = int(m.group(1))
            current_title = m.group(2).strip()
            continue

        # Check for overall confidence score
        cm = CONFIDENCE_RE.search(line)
        if cm:
            try:
                overall_confidence = float(cm.group(1))
            except ValueError:
                pass
            continue

        # Check for correctness (standalone verdict line)
        cr = CORRECTNESS_RE.search(line)
        if cr:
            overall_correctness = cr.group(1).strip()
            continue

        # Accumulate body lines for the current finding
        if current_title is not None:
            current_body_lines.append(line)

    _flush_finding()

    return ReviewResult(
        findings=findings,
        overall_correctness=overall_correctness,
        overall_confidence_score=overall_confidence,
        raw_output=raw_output,
        source="tui",
    )


# --- GitHub PR review parsing ---


def parse_github_review(
    repo: str,
    pr_number: int,
    review_data: dict,
) -> ReviewResult:
    """Parse a GitHub PR review (from gh API) into structured ReviewResult.

    Fetches inline review comments for the review and parses [P0]-[P3]
    badges from comment bodies.

    Args:
        repo: GitHub repo in owner/repo format
        pr_number: PR number
        review_data: Review object from gh api repos/{owner}/{repo}/pulls/{number}/reviews

    Returns:
        ReviewResult with parsed findings
    """
    review_id = review_data.get("id")
    review_body = review_data.get("body", "")
    owner, repo_name = repo.split("/", 1)

    findings: list[ReviewFinding] = []
    overall_correctness: Optional[str] = None
    overall_explanation: Optional[str] = None
    overall_confidence: Optional[float] = None

    # Parse overall verdict from review body
    if review_body:
        cr = CORRECTNESS_RE.search(review_body)
        if cr:
            overall_correctness = cr.group(1).strip()

        cm = CONFIDENCE_RE.search(review_body)
        if cm:
            try:
                overall_confidence = float(cm.group(1))
            except ValueError:
                pass

        # Use the full review body as explanation
        overall_explanation = review_body.strip()

    # Fetch inline review comments
    if review_id:
        comments = _fetch_review_comments(owner, repo_name, pr_number, review_id)
        for comment in comments:
            finding = _parse_review_comment(comment)
            if finding:
                findings.append(finding)

    return ReviewResult(
        findings=findings,
        overall_correctness=overall_correctness,
        overall_explanation=overall_explanation,
        overall_confidence_score=overall_confidence,
        raw_output=review_body,
        source="github_pr",
    )


def _fetch_review_comments(
    owner: str,
    repo_name: str,
    pr_number: int,
    review_id: int,
) -> list[dict]:
    """Fetch review comments from GitHub API.

    Args:
        owner: Repository owner
        repo_name: Repository name
        pr_number: PR number
        review_id: Review ID

    Returns:
        List of comment dicts
    """
    try:
        result = subprocess.run(
            [
                "gh", "api",
                f"repos/{owner}/{repo_name}/pulls/{pr_number}/reviews/{review_id}/comments",
                "--jq", ".",
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode == 0 and result.stdout.strip():
            return json.loads(result.stdout)
    except (subprocess.TimeoutExpired, json.JSONDecodeError, Exception) as e:
        logger.warning(f"Failed to fetch review comments: {e}")
    return []


def _parse_review_comment(comment: dict) -> Optional[ReviewFinding]:
    """Parse a single GitHub review comment into a ReviewFinding.

    Looks for [P0]-[P3] badges in the comment body.

    Args:
        comment: Comment dict from GitHub API

    Returns:
        ReviewFinding or None if no priority badge found
    """
    body = comment.get("body", "")
    path = comment.get("path")
    line = comment.get("line") or comment.get("original_line")
    start_line = comment.get("start_line") or comment.get("original_start_line")

    # Look for priority badge in the comment body
    m = PRIORITY_HEADER_RE.search(body)
    if m:
        priority = int(m.group(1))
        title = m.group(2).strip()
        # Body is everything after the priority header line
        header_end = m.end()
        remaining = body[header_end:].strip()
    else:
        # No priority badge -- treat as P2 (medium) by default
        priority = 2
        first_line = body.split('\n', 1)[0].strip()
        title = first_line[:120] if first_line else "Review comment"
        remaining = body.strip()

    return ReviewFinding(
        title=title,
        body=remaining,
        priority=priority,
        file_path=path,
        line_start=start_line or line,
        line_end=line,
    )
