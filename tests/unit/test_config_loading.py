"""Tests for config loading in various components.

Verifies that timeout and delay values are correctly loaded from config.yaml
and that fallback defaults work when config is not provided.
"""

import tempfile
from pathlib import Path
from unittest.mock import MagicMock, AsyncMock

import pytest

from src.main import load_config
from src.tmux_controller import TmuxController
from src.output_monitor import OutputMonitor
from src.message_queue import MessageQueueManager


class TestTmuxControllerConfig:
    """Test TmuxController loads config values correctly."""

    def test_default_values_without_config(self):
        """Verify default fallback values when no config provided."""
        with tempfile.TemporaryDirectory() as temp_dir:
            controller = TmuxController(log_dir=temp_dir)

            assert controller.shell_export_settle_seconds == 0.1
            assert controller.claude_init_seconds == 3
            assert controller.claude_init_no_prompt_seconds == 1
            assert controller.send_keys_timeout_seconds == 5
            assert controller.send_keys_settle_seconds == 0.3
            assert controller.socket_name is None
            assert controller.native_scrollback is False
            assert controller.history_limit == 100000

    def test_config_values_loaded(self):
        """Verify config values override defaults."""
        config = {
            "timeouts": {
                "tmux": {
                    "shell_export_settle_seconds": 0.5,
                    "claude_init_seconds": 5,
                    "claude_init_no_prompt_seconds": 2,
                    "send_keys_timeout_seconds": 10,
                    "send_keys_settle_seconds": 0.5,
                }
            }
        }

        with tempfile.TemporaryDirectory() as temp_dir:
            controller = TmuxController(log_dir=temp_dir, config=config)

            assert controller.shell_export_settle_seconds == 0.5
            assert controller.claude_init_seconds == 5
            assert controller.claude_init_no_prompt_seconds == 2
            assert controller.send_keys_timeout_seconds == 10
            assert controller.send_keys_settle_seconds == 0.5

    def test_tmux_config_values_loaded(self):
        """Verify tmux socket/native-scrollback/history values load from config."""
        config = {
            "tmux": {
                "socket_name": "session-manager-test",
                "native_scrollback": True,
                "history_limit": 12345,
            }
        }

        with tempfile.TemporaryDirectory() as temp_dir:
            controller = TmuxController(log_dir=temp_dir, config=config)

            assert controller.socket_name == "session-manager-test"
            assert controller.native_scrollback is True
            assert controller.history_limit == 12345
            assert controller.tmux_cmd("list-sessions") == [
                "tmux",
                "-L",
                "session-manager-test",
                "list-sessions",
            ]

    def test_partial_config_uses_defaults(self):
        """Verify missing config values fall back to defaults."""
        config = {
            "timeouts": {
                "tmux": {
                    "shell_export_settle_seconds": 0.2,
                    # Other values not specified
                }
            }
        }

        with tempfile.TemporaryDirectory() as temp_dir:
            controller = TmuxController(log_dir=temp_dir, config=config)

            assert controller.shell_export_settle_seconds == 0.2  # From config
            assert controller.claude_init_seconds == 3  # Default
            assert controller.send_keys_timeout_seconds == 5  # Default


class TestOutputMonitorConfig:
    """Test OutputMonitor loads config values correctly."""

    def test_default_values_without_config(self):
        """Verify default fallback values when no config provided."""
        monitor = OutputMonitor()

        assert monitor._idle_cooldown == 300
        assert monitor._permission_debounce == 30

    def test_config_values_loaded(self):
        """Verify config values override defaults."""
        config = {
            "timeouts": {
                "output_monitor": {
                    "idle_cooldown_seconds": 600,
                    "permission_debounce_seconds": 60,
                }
            }
        }

        monitor = OutputMonitor(config=config)

        assert monitor._idle_cooldown == 600
        assert monitor._permission_debounce == 60

    def test_partial_config_uses_defaults(self):
        """Verify missing config values fall back to defaults."""
        config = {
            "timeouts": {
                "output_monitor": {
                    "idle_cooldown_seconds": 180,
                    # permission_debounce_seconds not specified
                }
            }
        }

        monitor = OutputMonitor(config=config)

        assert monitor._idle_cooldown == 180  # From config
        assert monitor._permission_debounce == 30  # Default


class TestMessageQueueManagerConfig:
    """Test MessageQueueManager loads config values correctly."""

    @pytest.fixture
    def mock_session_manager(self):
        """Create a mock session manager."""
        mock = MagicMock()
        mock.tmux = MagicMock()
        mock.tmux.send_input_async = AsyncMock(return_value=True)
        return mock

    def test_default_values_without_config(self, mock_session_manager):
        """Verify default fallback values when no config provided."""
        with tempfile.TemporaryDirectory() as temp_dir:
            db_path = Path(temp_dir) / "test.db"
            manager = MessageQueueManager(
                mock_session_manager,
                db_path=str(db_path),
            )

            # sm_send defaults
            assert manager.input_poll_interval == 5
            assert manager.input_stale_timeout == 120
            assert manager.max_batch_size == 10
            assert manager.urgent_delay_ms == 500

            # timeout defaults
            assert manager.subprocess_timeout == 2
            assert manager.async_send_timeout == 5
            assert manager.initial_retry_delay == 1.0
            assert manager.max_retry_delay == 30
            assert manager.watch_poll_interval == 2

    def test_config_values_loaded(self, mock_session_manager):
        """Verify config values override defaults."""
        config = {
            "sm_send": {
                "input_poll_interval": 10,
                "input_stale_timeout": 240,
                "max_batch_size": 20,
                "urgent_delay_ms": 1000,
            },
            "timeouts": {
                "message_queue": {
                    "subprocess_timeout_seconds": 5,
                    "async_send_timeout_seconds": 10,
                    "initial_retry_delay_seconds": 2.0,
                    "max_retry_delay_seconds": 60,
                    "watch_poll_interval_seconds": 5,
                }
            }
        }

        with tempfile.TemporaryDirectory() as temp_dir:
            db_path = Path(temp_dir) / "test.db"
            manager = MessageQueueManager(
                mock_session_manager,
                db_path=str(db_path),
                config=config,
            )

            # sm_send values
            assert manager.input_poll_interval == 10
            assert manager.input_stale_timeout == 240
            assert manager.max_batch_size == 20
            assert manager.urgent_delay_ms == 1000

            # timeout values
            assert manager.subprocess_timeout == 5
            assert manager.async_send_timeout == 10
            assert manager.initial_retry_delay == 2.0
            assert manager.max_retry_delay == 60
            assert manager.watch_poll_interval == 5

    def test_partial_config_uses_defaults(self, mock_session_manager):
        """Verify missing config values fall back to defaults."""
        config = {
            "sm_send": {
                "input_poll_interval": 15,
                # Other sm_send values not specified
            },
            "timeouts": {
                "message_queue": {
                    "subprocess_timeout_seconds": 3,
                    # Other timeout values not specified
                }
            }
        }

        with tempfile.TemporaryDirectory() as temp_dir:
            db_path = Path(temp_dir) / "test.db"
            manager = MessageQueueManager(
                mock_session_manager,
                db_path=str(db_path),
                config=config,
            )

            # Mixed config and defaults
            assert manager.input_poll_interval == 15  # From config
            assert manager.input_stale_timeout == 120  # Default
            assert manager.subprocess_timeout == 3  # From config
            assert manager.async_send_timeout == 5  # Default


class TestLoadConfig:
    """Test config.yaml merges gitignored local auth env overrides."""

    def test_load_config_merges_local_google_auth_overrides(self, tmp_path: Path):
        config_path = tmp_path / "config.yaml"
        config_path.write_text("server:\n  host: 127.0.0.1\n")

        env_path = tmp_path / "values.env"
        env_path.write_text(
            "\n".join(
                [
                    "PUBLIC_HTTP_HOST=sm.rajeshgo.li",
                    "PUBLIC_SSH_HOST=ssh.sm.rajeshgo.li",
                    "SSH_USERNAME=rajesh",
                    "HTTP_ORIGIN_URL=http://127.0.0.1:8420",
                    "GOOGLE_ANDROID_CLIENT_ID=android-client-id",
                    "GOOGLE_WEB_CLIENT_ID=web-client-id",
                    "GOOGLE_WEB_CLIENT_SECRET=web-client-secret",
                    "ALLOWLIST_EMAIL=rajeshgoli@gmail.com",
                ]
            )
        )

        config = load_config(str(config_path), local_env_path=str(env_path))

        assert config["server"]["host"] == "127.0.0.1"
        assert config["external_access"]["public_http_host"] == "sm.rajeshgo.li"
        assert config["external_access"]["public_ssh_host"] == "ssh.sm.rajeshgo.li"
        assert config["external_access"]["ssh_username"] == "rajesh"

        google_auth = config["auth"]["google"]
        assert google_auth["enabled"] is True
        assert google_auth["android_client_id"] == "android-client-id"
        assert google_auth["client_id"] == "web-client-id"
        assert google_auth["client_secret"] == "web-client-secret"
        assert google_auth["allowlist_emails"] == ["rajeshgoli@gmail.com"]
        assert google_auth["redirect_uri"] == "https://sm.rajeshgo.li/auth/google/callback"
        assert google_auth["session_cookie_secret"]

    def test_partial_local_auth_env_does_not_clear_yaml_values(self, tmp_path: Path):
        config_path = tmp_path / "config.yaml"
        config_path.write_text(
            "\n".join(
                [
                    "auth:",
                    "  google:",
                    "    enabled: true",
                    "    public_host: existing.example.com",
                    "    client_id: yaml-client-id",
                    "    client_secret: yaml-client-secret",
                    "    redirect_uri: https://existing.example.com/auth/google/callback",
                    "    allowlist_emails:",
                    "      - existing@example.com",
                    "    session_cookie_secret: yaml-secret",
                ]
            )
        )

        env_path = tmp_path / "values.env"
        env_path.write_text("PUBLIC_HTTP_HOST=sm.rajeshgo.li\n")

        config = load_config(str(config_path), local_env_path=str(env_path))

        google_auth = config["auth"]["google"]
        assert google_auth["enabled"] is True
        assert google_auth["client_id"] == "yaml-client-id"
        assert google_auth["client_secret"] == "yaml-client-secret"
        assert google_auth["allowlist_emails"] == ["existing@example.com"]
        assert google_auth["session_cookie_secret"] == "yaml-secret"
        assert google_auth["public_host"] == "sm.rajeshgo.li"
        assert google_auth["redirect_uri"] == "https://sm.rajeshgo.li/auth/google/callback"
