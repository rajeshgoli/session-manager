"""Unit tests for github_reviews module — #141."""

import json
import time
from datetime import datetime
from unittest.mock import patch, MagicMock

import pytest

from src.github_reviews import (
    post_pr_review_comment,
    poll_for_codex_review,
    get_pr_repo_from_git,
)


class TestPostPrReviewComment:
    """Tests for post_pr_review_comment()."""

    @patch("src.github_reviews.subprocess.run")
    def test_posts_plain_review(self, mock_run):
        """Posts @codex review without steer."""
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout="https://github.com/owner/repo/pull/42#issuecomment-12345\n",
        )

        result = post_pr_review_comment("owner/repo", 42)

        assert result["body"] == "@codex review"
        assert result["comment_id"] == 12345
        assert "posted_at" in result

        mock_run.assert_called_once()
        call_args = mock_run.call_args[0][0]
        assert "gh" in call_args
        assert "--body" in call_args
        body_idx = call_args.index("--body")
        assert call_args[body_idx + 1] == "@codex review"

    @patch("src.github_reviews.subprocess.run")
    def test_posts_steered_review(self, mock_run):
        """Posts @codex review for <steer> with steer text."""
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout="https://github.com/owner/repo/pull/42#issuecomment-99999\n",
        )

        result = post_pr_review_comment("owner/repo", 42, steer="security focus")

        assert result["body"] == "@codex review for security focus"
        assert result["comment_id"] == 99999

    @patch("src.github_reviews.subprocess.run")
    def test_raises_on_failure(self, mock_run):
        """Raises RuntimeError when gh CLI fails."""
        mock_run.return_value = MagicMock(
            returncode=1,
            stderr="not found",
        )

        with pytest.raises(RuntimeError, match="gh pr comment failed"):
            post_pr_review_comment("owner/repo", 42)

    @patch("src.github_reviews.subprocess.run")
    def test_handles_missing_comment_id(self, mock_run):
        """Returns comment_id=0 when URL doesn't contain issuecomment."""
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout="https://github.com/owner/repo/pull/42\n",
        )

        result = post_pr_review_comment("owner/repo", 42)
        assert result["comment_id"] == 0


class TestPollForCodexReview:
    """Tests for poll_for_codex_review()."""

    @patch("src.github_reviews.time.sleep")
    @patch("src.github_reviews.subprocess.run")
    def test_finds_codex_review(self, mock_run, mock_sleep):
        """Returns review when codex[bot] review is found after since."""
        since = datetime(2026, 2, 14, 10, 0, 0)
        review_data = [
            {
                "user": {"login": "codex[bot]"},
                "submitted_at": "2026-02-14T10:05:00Z",
                "state": "COMMENTED",
            }
        ]
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps(review_data),
        )

        result = poll_for_codex_review("owner/repo", 42, since, timeout=60)

        assert result is not None
        assert result["user"]["login"] == "codex[bot]"
        mock_sleep.assert_not_called()  # Found on first poll

    @patch("src.github_reviews.time.sleep")
    @patch("src.github_reviews.time.monotonic")
    @patch("src.github_reviews.subprocess.run")
    def test_returns_none_on_timeout(self, mock_run, mock_monotonic, mock_sleep):
        """Returns None when no review found within timeout."""
        since = datetime(2026, 2, 14, 10, 0, 0)
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout="[]",
        )
        # Simulate time passing: start, check, after sleep, check (past deadline)
        mock_monotonic.side_effect = [0, 0, 0, 31, 31]

        result = poll_for_codex_review("owner/repo", 42, since, timeout=30, poll_interval=30)

        assert result is None

    @patch("src.github_reviews.time.sleep")
    @patch("src.github_reviews.subprocess.run")
    def test_ignores_old_reviews(self, mock_run, mock_sleep):
        """Ignores reviews submitted before since timestamp."""
        since = datetime(2026, 2, 14, 10, 0, 0)
        review_data = [
            {
                "user": {"login": "codex[bot]"},
                "submitted_at": "2026-02-14T09:00:00Z",  # Before since
                "state": "COMMENTED",
            }
        ]
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps(review_data),
        )

        # Make it timeout after one iteration
        with patch("src.github_reviews.time.monotonic", side_effect=[0, 0, 0, 100]):
            result = poll_for_codex_review("owner/repo", 42, since, timeout=1)

        assert result is None


class TestGetPrRepoFromGit:
    """Tests for get_pr_repo_from_git()."""

    @patch("src.github_reviews.subprocess.run")
    def test_returns_repo(self, mock_run):
        """Returns owner/repo from gh repo view."""
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout="owner/repo\n",
        )

        result = get_pr_repo_from_git("/tmp/workspace")
        assert result == "owner/repo"

    @patch("src.github_reviews.subprocess.run")
    def test_returns_none_on_failure(self, mock_run):
        """Returns None when gh fails."""
        mock_run.return_value = MagicMock(
            returncode=1,
            stdout="",
        )

        result = get_pr_repo_from_git("/tmp/workspace")
        assert result is None

    @patch("src.github_reviews.subprocess.run")
    def test_returns_none_on_exception(self, mock_run):
        """Returns None when subprocess raises."""
        mock_run.side_effect = Exception("not a git repo")

        result = get_pr_repo_from_git("/tmp/workspace")
        assert result is None
