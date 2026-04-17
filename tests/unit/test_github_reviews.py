"""Unit tests for github_reviews module — #141."""

import json
import time
from datetime import datetime
from unittest.mock import patch, MagicMock

import pytest

from src.github_reviews import (
    detect_codex_pickup,
    find_fresh_codex_review_or_comment,
    fetch_issue_comment,
    fetch_issue_comment_reactions,
    fetch_latest_codex_review,
    fetch_pr_issue_comments,
    fetch_pr_reviews,
    get_codex_request_reaction_state,
    is_codex_actor,
    post_pr_review_comment,
    poll_for_codex_review,
    validate_open_pr,
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
        assert result["comment_url"] == "https://github.com/owner/repo/pull/42"


class TestPollForCodexReview:
    """Tests for poll_for_codex_review()."""

    @patch("src.github_reviews.time.sleep")
    @patch("src.github_reviews.fetch_latest_codex_review")
    def test_finds_codex_review(self, mock_fetch_latest, mock_sleep):
        """Returns review when codex[bot] review is found after since."""
        since = datetime(2026, 2, 14, 10, 0, 0)
        mock_fetch_latest.return_value = {
            "user": {"login": "codex[bot]"},
            "submitted_at": "2026-02-14T10:05:00Z",
            "state": "COMMENTED",
        }

        result = poll_for_codex_review("owner/repo", 42, since, timeout=60)

        assert result is not None
        assert result["user"]["login"] == "codex[bot]"
        mock_sleep.assert_not_called()  # Found on first poll

    @patch("src.github_reviews.time.sleep")
    @patch("src.github_reviews.time.monotonic")
    @patch("src.github_reviews.fetch_latest_codex_review")
    def test_returns_none_on_timeout(self, mock_fetch_latest, mock_monotonic, mock_sleep):
        """Returns None when no review found within timeout."""
        since = datetime(2026, 2, 14, 10, 0, 0)
        mock_fetch_latest.return_value = None
        # Simulate time passing: start, check, after sleep, check (past deadline)
        mock_monotonic.side_effect = [0, 0, 0, 31, 31]

        result = poll_for_codex_review("owner/repo", 42, since, timeout=30, poll_interval=30)

        assert result is None

    @patch("src.github_reviews.time.sleep")
    @patch("src.github_reviews.fetch_latest_codex_review")
    def test_ignores_old_reviews(self, mock_fetch_latest, mock_sleep):
        """Ignores reviews submitted before since timestamp."""
        since = datetime(2026, 2, 14, 10, 0, 0)
        mock_fetch_latest.return_value = {
            "user": {"login": "codex[bot]"},
            "submitted_at": "2026-02-14T09:00:00Z",  # Before since
            "state": "COMMENTED",
        }

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


class TestCodexHelpers:
    """Tests for newer GitHub review helper primitives (#618)."""

    @patch("src.github_reviews.subprocess.run")
    def test_validate_open_pr_returns_payload(self, mock_run):
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps({"number": 42, "state": "OPEN", "url": "https://example/pr/42"}),
        )

        result = validate_open_pr("owner/repo", 42)

        assert result["state"] == "OPEN"

    @patch("src.github_reviews.subprocess.run")
    def test_validate_open_pr_rejects_closed_pr(self, mock_run):
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps({"number": 42, "state": "MERGED"}),
        )

        with pytest.raises(RuntimeError, match="not OPEN"):
            validate_open_pr("owner/repo", 42)

    @patch("src.github_reviews.subprocess.run")
    def test_fetch_helpers_decode_json(self, mock_run):
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps([{"id": 1}]),
        )

        assert fetch_pr_reviews("owner/repo", 42) == [{"id": 1}]
        assert fetch_pr_issue_comments("owner/repo", 42) == [{"id": 1}]
        assert fetch_issue_comment_reactions("owner/repo", 99) == [{"id": 1}]

    @patch("src.github_reviews.subprocess.run")
    def test_fetch_issue_comments_supports_since(self, mock_run):
        mock_run.return_value = MagicMock(returncode=0, stdout=json.dumps([{"id": 1}]))

        fetch_pr_issue_comments("owner/repo", 42, since=datetime(2026, 4, 16, 18, 27, 57))

        command = mock_run.call_args[0][0]
        assert any("since=2026-04-16T18:27:57Z" in token for token in command)

    @patch("src.github_reviews.subprocess.run")
    def test_fetch_helpers_flatten_paginated_results(self, mock_run):
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps([[{"id": 1}], [{"id": 2}]]),
        )

        assert fetch_pr_reviews("owner/repo", 42) == [{"id": 1}, {"id": 2}]

    @patch("src.github_reviews.subprocess.run")
    def test_fetch_issue_comment_returns_dict(self, mock_run):
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps({"id": 123, "html_url": "https://example/comment/123"}),
        )

        result = fetch_issue_comment("owner/repo", 123)
        assert result["id"] == 123

    def test_is_codex_actor_matches_bot_login_or_app(self):
        assert is_codex_actor({"user": {"login": "chatgpt-codex-connector[bot]"}})
        assert is_codex_actor({"performed_via_github_app": {"slug": "chatgpt-codex-connector"}})
        assert not is_codex_actor({"user": {"login": "octocat"}})

    @patch("src.github_reviews.fetch_issue_comment_reactions")
    def test_detect_codex_pickup_requires_codex_eyes(self, mock_reactions):
        mock_reactions.return_value = [
            {"content": "eyes", "user": {"login": "chatgpt-codex-connector[bot]"}},
        ]
        assert detect_codex_pickup("owner/repo", 12345) is True

        mock_reactions.return_value = [
            {"content": "eyes", "user": {"login": "octocat"}},
            {"content": "+1", "user": {"login": "chatgpt-codex-connector[bot]"}},
        ]
        assert detect_codex_pickup("owner/repo", 12345) is False

    @patch("src.github_reviews.fetch_issue_comment_reactions")
    def test_get_codex_request_reaction_state_detects_clean_pass(self, mock_reactions):
        mock_reactions.return_value = [
            {"content": "eyes", "user": {"login": "chatgpt-codex-connector[bot]"}},
            {"content": "+1", "user": {"login": "chatgpt-codex-connector[bot]"}},
        ]

        result = get_codex_request_reaction_state("owner/repo", 12345)

        assert result == {"picked_up": True, "clean_pass": True}

    @patch("src.github_reviews.fetch_pr_issue_comments")
    @patch("src.github_reviews.fetch_latest_codex_review")
    def test_find_fresh_codex_review_or_comment_prefers_newer_than_since(self, mock_latest_review, mock_comments):
        since = datetime(2026, 4, 16, 18, 27, 57)
        mock_latest_review.return_value = {
            "id": 100,
            "user": {"login": "codex[bot]"},
            "submitted_at": "2026-04-16T18:20:00Z",
            "html_url": "https://example/review/100",
            "state": "COMMENTED",
        }
        mock_comments.return_value = [
            {
                "id": 200,
                "user": {"login": "chatgpt-codex-connector[bot]"},
                "performed_via_github_app": {"slug": "chatgpt-codex-connector"},
                "created_at": "2026-04-16T18:32:59Z",
                "html_url": "https://example/comment/200",
                "body": "Codex Review: clean",
            }
        ]

        result = find_fresh_codex_review_or_comment("owner/repo", 42, since)

        assert result is not None
        assert result["source"] == "comment"
        assert result["id"] == 200

    @patch("src.github_reviews.subprocess.run")
    def test_fetch_latest_codex_review_uses_pr_view_latest_reviews(self, mock_run):
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps(
                {
                    "url": "https://github.com/owner/repo/pull/42",
                    "latestReviews": [
                        {
                            "id": "R_kw123",
                            "author": {"login": "chatgpt-codex-connector[bot]"},
                            "submittedAt": "2026-04-16T18:32:59Z",
                            "state": "COMMENTED",
                            "body": "clean",
                        }
                    ],
                }
            ),
        )

        result = fetch_latest_codex_review("owner/repo", 42)

        assert result is not None
        assert result["user"]["login"] == "chatgpt-codex-connector[bot]"
        assert result["html_url"] == "https://github.com/owner/repo/pull/42"

    @patch("src.github_reviews.subprocess.run")
    def test_returns_none_on_exception(self, mock_run):
        """Returns None when subprocess raises."""
        mock_run.side_effect = Exception("not a git repo")

        result = get_pr_repo_from_git("/tmp/workspace")
        assert result is None
