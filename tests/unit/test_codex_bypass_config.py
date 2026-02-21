"""Tests for codex bypass flag config (sm#185).

Verifies that:
1. codex.args with bypass flag is loaded into codex_cli_args
2. app_server_args: [] prevents bypass flag from leaking into codex-app sessions
"""

import tempfile

from src.session_manager import SessionManager


class TestCodexBypassConfig:
    """Test codex bypass flag isolation between CLI and app-server."""

    def test_bypass_flag_in_codex_cli_args(self):
        """Verify bypass flag is passed to codex CLI sessions."""
        config = {
            "codex": {
                "command": "codex",
                "args": ["--dangerously-bypass-approvals-and-sandbox"],
                "app_server_args": [],
            }
        }
        with tempfile.TemporaryDirectory() as tmpdir:
            sm = SessionManager(log_dir=tmpdir, state_file=f"{tmpdir}/state.json", config=config)
            assert "--dangerously-bypass-approvals-and-sandbox" in sm.codex_cli_args

    def test_app_server_args_empty_prevents_leak(self):
        """Verify app_server_args: [] prevents bypass flag from leaking to codex app-server."""
        config = {
            "codex": {
                "command": "codex",
                "args": ["--dangerously-bypass-approvals-and-sandbox"],
                "app_server_args": [],
            }
        }
        with tempfile.TemporaryDirectory() as tmpdir:
            sm = SessionManager(log_dir=tmpdir, state_file=f"{tmpdir}/state.json", config=config)
            assert sm.codex_config.args == []

    def test_without_app_server_args_bypass_leaks(self):
        """Verify that WITHOUT app_server_args, bypass flag leaks to app-server (the bug)."""
        config = {
            "codex": {
                "command": "codex",
                "args": ["--dangerously-bypass-approvals-and-sandbox"],
                # No app_server_args key — fallback chain picks up args
            }
        }
        with tempfile.TemporaryDirectory() as tmpdir:
            sm = SessionManager(log_dir=tmpdir, state_file=f"{tmpdir}/state.json", config=config)
            # Without app_server_args, the fallback chain leaks bypass flag
            assert "--dangerously-bypass-approvals-and-sandbox" in sm.codex_config.args

    def test_separate_codex_app_server_section_overrides(self):
        """Verify codex_app_server section overrides codex section for app-server."""
        config = {
            "codex": {
                "command": "codex",
                "args": ["--dangerously-bypass-approvals-and-sandbox"],
            },
            "codex_app_server": {
                "command": "codex",
                "app_server_args": ["--custom-flag"],
            }
        }
        with tempfile.TemporaryDirectory() as tmpdir:
            sm = SessionManager(log_dir=tmpdir, state_file=f"{tmpdir}/state.json", config=config)
            assert sm.codex_cli_args == ["--dangerously-bypass-approvals-and-sandbox"]
            assert sm.codex_config.args == ["--custom-flag"]

    def test_codex_rollout_defaults_enabled(self):
        """All codex rollout feature flags default to enabled."""
        with tempfile.TemporaryDirectory() as tmpdir:
            sm = SessionManager(log_dir=tmpdir, state_file=f"{tmpdir}/state.json", config={})
            assert sm.is_codex_rollout_enabled("enable_durable_events") is True
            assert sm.is_codex_rollout_enabled("enable_structured_requests") is True
            assert sm.is_codex_rollout_enabled("enable_observability_projection") is True
            assert sm.is_codex_rollout_enabled("enable_codex_tui") is True

    def test_codex_rollout_override(self):
        """codex_rollout overrides are applied from config."""
        config = {
            "codex_rollout": {
                "enable_durable_events": False,
                "enable_structured_requests": False,
                "enable_observability_projection": True,
                "enable_codex_tui": False,
            }
        }
        with tempfile.TemporaryDirectory() as tmpdir:
            sm = SessionManager(log_dir=tmpdir, state_file=f"{tmpdir}/state.json", config=config)
            assert sm.is_codex_rollout_enabled("enable_durable_events") is False
            assert sm.is_codex_rollout_enabled("enable_structured_requests") is False
            assert sm.is_codex_rollout_enabled("enable_observability_projection") is True
            assert sm.is_codex_rollout_enabled("enable_codex_tui") is False

    def test_codex_rollout_string_bool_parsing(self):
        """String booleans in config are parsed as expected (no truthy-string bug)."""
        config = {
            "codex_rollout": {
                "enable_durable_events": "false",
                "enable_structured_requests": "0",
                "enable_observability_projection": "true",
                "enable_codex_tui": "on",
            }
        }
        with tempfile.TemporaryDirectory() as tmpdir:
            sm = SessionManager(log_dir=tmpdir, state_file=f"{tmpdir}/state.json", config=config)
            assert sm.is_codex_rollout_enabled("enable_durable_events") is False
            assert sm.is_codex_rollout_enabled("enable_structured_requests") is False
            assert sm.is_codex_rollout_enabled("enable_observability_projection") is True
            assert sm.is_codex_rollout_enabled("enable_codex_tui") is True
