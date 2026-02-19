"""Tests for sm dispatch — template-based dispatch with auto-expansion."""

import copy
import os
import sys
import textwrap
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from src.cli.dispatch import (
    DEFAULT_DISPATCH_HARD_THRESHOLD,
    DEFAULT_DISPATCH_SOFT_THRESHOLD,
    DispatchError,
    expand_template,
    get_auto_remind_config,
    get_role_params,
    load_template,
    parse_dispatch_args,
)
from src.cli.commands import cmd_dispatch


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

SAMPLE_CONFIG = {
    "repo": {
        "path": "/Users/test/project",
        "pr_target": "dev",
        "test_command": "pytest tests/ -v",
    },
    "roles": {
        "engineer": {
            "template": (
                "As engineer, implement #{issue} in {repo.path}.\n"
                "Read spec at {spec}.\n"
                "PR target: {repo.pr_target}.\n"
                "Test: {repo.test_command}\n"
                "Report back to ({em_id}).\n"
            ),
            "required": ["issue", "spec"],
            "optional": ["extra"],
        },
        "scout": {
            "template": (
                "As scout, investigate #{issue} in {repo.path}.\n"
                "Write spec to {spec}.\n"
                "Send to reviewer ({reviewer_id}).\n"
                "Report to ({em_id}).\n"
            ),
            "required": ["issue", "spec", "reviewer_id"],
            "optional": ["extra"],
        },
        "reviewer": {
            "template": (
                "You are a reviewer. Repo: {repo.path}.\n"
                "Scout: ({scout_id}).\n"
                "{notes}\n"
            ),
            "required": ["scout_id"],
            "optional": ["notes", "extra"],
        },
    },
}


@pytest.fixture
def sample_config():
    return copy.deepcopy(SAMPLE_CONFIG)


@pytest.fixture
def template_dir(tmp_path):
    """Create a .sm/dispatch_templates.yaml in a temp directory."""
    import yaml

    sm_dir = tmp_path / ".sm"
    sm_dir.mkdir()
    yaml_path = sm_dir / "dispatch_templates.yaml"
    yaml_path.write_text(yaml.dump(SAMPLE_CONFIG))
    return tmp_path


@pytest.fixture
def nested_dir(template_dir):
    """Create a nested subdirectory under the template dir."""
    nested = template_dir / "src" / "lib"
    nested.mkdir(parents=True)
    return nested


@pytest.fixture
def global_template(tmp_path):
    """Create a ~/.sm/dispatch_templates.yaml in a temp home dir."""
    import yaml

    home = tmp_path / "home"
    sm_dir = home / ".sm"
    sm_dir.mkdir(parents=True)
    yaml_path = sm_dir / "dispatch_templates.yaml"
    yaml_path.write_text(yaml.dump(SAMPLE_CONFIG))
    return home


# ---------------------------------------------------------------------------
# 1. Template discovery
# ---------------------------------------------------------------------------

class TestTemplateDiscovery:
    def test_finds_template_in_cwd(self, template_dir):
        """Template found when .sm/dispatch_templates.yaml is in working dir."""
        config = load_template(str(template_dir))
        assert "roles" in config
        assert "engineer" in config["roles"]

    def test_finds_template_from_subdirectory(self, nested_dir, template_dir):
        """Template found when walking up from a subdirectory."""
        config = load_template(str(nested_dir))
        assert "roles" in config
        assert config["repo"]["path"] == "/Users/test/project"

    def test_falls_back_to_global(self, global_template, tmp_path):
        """Falls back to ~/.sm/ when no local template found."""
        empty_dir = tmp_path / "empty"
        empty_dir.mkdir()
        with patch.object(Path, "home", return_value=global_template):
            config = load_template(str(empty_dir))
        assert "roles" in config

    def test_errors_when_no_template(self, tmp_path):
        """Raises DispatchError when no template file found anywhere."""
        empty_dir = tmp_path / "empty"
        empty_dir.mkdir()
        with patch.object(Path, "home", return_value=tmp_path / "nohome"):
            with pytest.raises(DispatchError, match="No dispatch template found"):
                load_template(str(empty_dir))


# ---------------------------------------------------------------------------
# 2. Variable expansion
# ---------------------------------------------------------------------------

class TestVariableExpansion:
    def test_all_variables_resolve(self, sample_config):
        """All repo.*, em_id, and required params resolve correctly."""
        result = expand_template(
            sample_config, "engineer",
            {"issue": "42", "spec": "docs/42.md"},
            em_id="abc123",
        )
        assert "#42" in result
        assert "/Users/test/project" in result
        assert "docs/42.md" in result
        assert "dev" in result
        assert "pytest tests/ -v" in result
        assert "(abc123)" in result

    def test_repo_dot_vars_resolve(self, sample_config):
        """repo.path, repo.pr_target, repo.test_command all expand."""
        result = expand_template(
            sample_config, "engineer",
            {"issue": "1", "spec": "s.md"},
            em_id="x",
        )
        assert "/Users/test/project" in result
        assert "dev" in result
        assert "pytest tests/ -v" in result


# ---------------------------------------------------------------------------
# 3. Required param validation
# ---------------------------------------------------------------------------

class TestRequiredParams:
    def test_missing_required_param_errors(self, sample_config):
        """Missing required param produces clear error message."""
        with pytest.raises(DispatchError, match="Missing required parameter '--spec'"):
            expand_template(
                sample_config, "engineer",
                {"issue": "42"},  # missing 'spec'
                em_id="abc",
            )

    def test_all_required_present_succeeds(self, sample_config):
        """No error when all required params provided."""
        result = expand_template(
            sample_config, "engineer",
            {"issue": "42", "spec": "s.md"},
            em_id="abc",
        )
        assert "#42" in result


# ---------------------------------------------------------------------------
# 4. Optional param handling
# ---------------------------------------------------------------------------

class TestOptionalParams:
    def test_optional_resolves_when_provided(self, sample_config):
        """Optional param replaces placeholder when provided."""
        result = expand_template(
            sample_config, "reviewer",
            {"scout_id": "s1", "notes": "Be thorough"},
            em_id="abc",
        )
        assert "Be thorough" in result

    def test_optional_line_removed_when_absent(self, sample_config):
        """Lines containing only an unresolved optional var are removed."""
        result = expand_template(
            sample_config, "reviewer",
            {"scout_id": "s1"},
            em_id="abc",
        )
        # {notes} line should be removed, not present as literal
        assert "{notes}" not in result
        # But the rest of the template is intact
        assert "Scout: (s1)" in result


# ---------------------------------------------------------------------------
# 5. Extra handling
# ---------------------------------------------------------------------------

class TestExtraHandling:
    def test_extra_appended_as_final_line(self, sample_config):
        """--extra text is appended as the last line."""
        result = expand_template(
            sample_config, "engineer",
            {"issue": "42", "spec": "s.md", "extra": "Note: use debug logging"},
            em_id="abc",
        )
        lines = result.strip().split('\n')
        assert lines[-1] == "Note: use debug logging"

    def test_no_extra_nothing_appended(self, sample_config):
        """Without --extra, no extra line is appended."""
        result = expand_template(
            sample_config, "engineer",
            {"issue": "42", "spec": "s.md"},
            em_id="abc",
        )
        assert "Note:" not in result


# ---------------------------------------------------------------------------
# 6. Unknown role
# ---------------------------------------------------------------------------

class TestUnknownRole:
    def test_unknown_role_lists_available(self, sample_config):
        """Unknown role error lists available roles."""
        with pytest.raises(DispatchError, match="Role 'foo' not found") as exc_info:
            expand_template(sample_config, "foo", {}, em_id="abc")
        assert "engineer" in str(exc_info.value)
        assert "scout" in str(exc_info.value)

    def test_get_role_params_unknown_role(self, sample_config):
        """get_role_params raises for unknown role."""
        with pytest.raises(DispatchError, match="Role 'nonexistent' not found"):
            get_role_params(sample_config, "nonexistent")


# ---------------------------------------------------------------------------
# 7. Unresolved placeholder detection
# ---------------------------------------------------------------------------

class TestUnresolvedPlaceholders:
    def test_unresolved_var_detected(self):
        """Catches {foo} left in expanded text."""
        config = {
            "repo": {"path": "/p"},
            "roles": {
                "test": {
                    "template": "Do {action} in {repo.path} with {unknown_var}.\n",
                    "required": ["action"],
                    "optional": [],
                },
            },
        }
        with pytest.raises(DispatchError, match="Unresolved variable '{unknown_var}'"):
            expand_template(config, "test", {"action": "thing"}, em_id="x")


# ---------------------------------------------------------------------------
# 8. YAML parse error
# ---------------------------------------------------------------------------

class TestYAMLParseError:
    def test_invalid_yaml_graceful_error(self, tmp_path):
        """Graceful error message on YAML parse failure."""
        sm_dir = tmp_path / ".sm"
        sm_dir.mkdir()
        yaml_path = sm_dir / "dispatch_templates.yaml"
        yaml_path.write_text("{{invalid yaml: [}")
        with pytest.raises(DispatchError, match="Failed to parse dispatch template"):
            load_template(str(tmp_path))


# ---------------------------------------------------------------------------
# 9. em_id handling
# ---------------------------------------------------------------------------

class TestEmIdHandling:
    def test_em_id_required_for_send(self, sample_config):
        """Unresolved {em_id} when not in dry-run mode raises error."""
        with pytest.raises(DispatchError, match="Unresolved variable '{em_id}'"):
            expand_template(
                sample_config, "engineer",
                {"issue": "42", "spec": "s.md"},
                em_id=None,
                dry_run=False,
            )

    def test_em_id_placeholder_in_dry_run(self, sample_config, capsys):
        """In dry-run mode, missing em_id resolves to <unset> with warning."""
        result = expand_template(
            sample_config, "engineer",
            {"issue": "42", "spec": "s.md"},
            em_id=None,
            dry_run=True,
        )
        assert "(<unset>)" in result
        captured = capsys.readouterr()
        assert "Warning" in captured.err


# ---------------------------------------------------------------------------
# 10. --dry-run integration
# ---------------------------------------------------------------------------

class TestDryRun:
    def test_dry_run_prints_and_exits_zero(self, sample_config, capsys):
        """--dry-run prints expanded template, returns 0, does not call send."""
        mock_client = MagicMock()

        with patch("src.cli.commands.os.getcwd", return_value="/tmp"):
            with patch("src.cli.dispatch.load_template", return_value=sample_config):
                exit_code = cmd_dispatch(
                    mock_client, "agent1", "engineer",
                    {"issue": "42", "spec": "s.md"},
                    em_id="abc",
                    dry_run=True,
                )

        assert exit_code == 0
        captured = capsys.readouterr()
        assert "#42" in captured.out
        # cmd_send should NOT have been called
        mock_client.send_input.assert_not_called()


# ---------------------------------------------------------------------------
# 11. Full dispatch (calls cmd_send)
# ---------------------------------------------------------------------------

class TestFullDispatch:
    def test_dispatch_calls_send(self, sample_config):
        """Full dispatch calls cmd_send with expanded text and delivery mode."""
        mock_client = MagicMock()
        # resolve_session_id will be called inside cmd_send
        mock_client.get_session.return_value = {
            "id": "agent1",
            "friendly_name": "eng",
            "status": "running",
        }
        mock_client.session_id = "abc"
        mock_client.send_input.return_value = (True, False)

        with patch("src.cli.commands.os.getcwd", return_value="/tmp"):
            with patch("src.cli.dispatch.load_template", return_value=sample_config):
                exit_code = cmd_dispatch(
                    mock_client, "agent1", "engineer",
                    {"issue": "42", "spec": "s.md"},
                    em_id="abc",
                    delivery_mode="urgent",
                    notify_on_stop=False,
                )

        assert exit_code == 0
        # Verify send_input was called
        call_args = mock_client.send_input.call_args
        assert call_args is not None
        sent_text = call_args[0][1]
        assert "#42" in sent_text
        assert call_args[1]["delivery_mode"] == "urgent"


# ---------------------------------------------------------------------------
# 12. Delivery mode passthrough
# ---------------------------------------------------------------------------

class TestDeliveryModePassthrough:
    def test_urgent_passthrough(self):
        """--urgent flag produces delivery_mode='urgent'."""
        _, _, _, mode, _, _ = parse_dispatch_args(
            ["agent1", "--role", "engineer", "--urgent", "--issue", "1", "--spec", "s"]
        )
        assert mode == "urgent"

    def test_important_passthrough(self):
        """--important flag produces delivery_mode='important'."""
        _, _, _, mode, _, _ = parse_dispatch_args(
            ["agent1", "--role", "engineer", "--important", "--issue", "1", "--spec", "s"]
        )
        assert mode == "important"

    def test_steer_passthrough(self):
        """--steer flag produces delivery_mode='steer'."""
        _, _, _, mode, _, _ = parse_dispatch_args(
            ["agent1", "--role", "engineer", "--steer", "--issue", "1", "--spec", "s"]
        )
        assert mode == "steer"

    def test_default_sequential(self):
        """No delivery flag defaults to sequential."""
        _, _, _, mode, _, _ = parse_dispatch_args(
            ["agent1", "--role", "engineer", "--issue", "1", "--spec", "s"]
        )
        assert mode == "sequential"

    def test_precedence_urgent_over_important(self):
        """--urgent takes precedence over --important."""
        _, _, _, mode, _, _ = parse_dispatch_args(
            ["agent1", "--role", "engineer", "--urgent", "--important", "--issue", "1", "--spec", "s"]
        )
        assert mode == "urgent"


# ---------------------------------------------------------------------------
# 13. Existing commands unaffected
# ---------------------------------------------------------------------------

class TestExistingCommandsUnaffected:
    def test_send_with_typo_still_errors(self):
        """Verify sm send --typo still produces an error (no silent swallowing)."""
        import subprocess
        # This tests the actual CLI binary behavior
        # We can't easily test this in-process, so we verify that our
        # pre-intercept only triggers for sys.argv[1] == "dispatch"
        # and doesn't affect parse_args() for other commands.
        from src.cli.main import main

        # Simulate `sm send --typo target text` — should fail with argparse error
        with patch("sys.argv", ["sm", "send", "--typo", "target", "text"]):
            with pytest.raises(SystemExit) as exc_info:
                main()
            # argparse exits with code 2 for unrecognized arguments
            assert exc_info.value.code == 2

    def test_send_remind_flag_rejected(self):
        """sm send --remind is no longer accepted (sm#225-B).

        --remind was removed from sm send; sm dispatch arms it automatically.
        Passing --remind should now produce an argparse error (exit code 2).
        """
        from src.cli.main import main

        with patch("sys.argv", ["sm", "send", "--remind", "180", "target", "message"]):
            with pytest.raises(SystemExit) as exc_info:
                main()
            assert exc_info.value.code == 2


# ---------------------------------------------------------------------------
# parse_dispatch_args tests
# ---------------------------------------------------------------------------

class TestParseDispatchArgs:
    def test_parses_basic_args(self):
        """Parses agent_id, role, and dynamic params."""
        agent_id, role, dry_run, mode, notify, params = parse_dispatch_args(
            ["my-agent", "--role", "engineer", "--issue", "42", "--spec", "s.md"]
        )
        assert agent_id == "my-agent"
        assert role == "engineer"
        assert dry_run is False
        assert params == {"issue": "42", "spec": "s.md"}

    def test_parses_dry_run(self):
        """--dry-run flag is captured."""
        _, _, dry_run, _, _, _ = parse_dispatch_args(
            ["agent1", "--role", "engineer", "--dry-run", "--issue", "1", "--spec", "s"]
        )
        assert dry_run is True

    def test_no_notify_on_stop(self):
        """--no-notify-on-stop sets notify_on_stop to False."""
        _, _, _, _, notify, _ = parse_dispatch_args(
            ["agent1", "--role", "engineer", "--no-notify-on-stop", "--issue", "1", "--spec", "s"]
        )
        assert notify is False


# ---------------------------------------------------------------------------
# 14. Auto-remind integration (sm#225-A)
# ---------------------------------------------------------------------------

class TestAutoRemindConfig:
    def test_defaults_when_no_config_file(self, tmp_path):
        """Falls back to module-level defaults when no config.yaml exists."""
        empty_dir = tmp_path / "no_config"
        empty_dir.mkdir()
        soft, hard = get_auto_remind_config(str(empty_dir))
        assert soft == DEFAULT_DISPATCH_SOFT_THRESHOLD  # 210
        assert hard == DEFAULT_DISPATCH_HARD_THRESHOLD  # 420

    def test_reads_thresholds_from_config_yaml(self, tmp_path):
        """Custom thresholds from config.yaml dispatch.auto_remind are returned."""
        import yaml
        config = {
            "dispatch": {
                "auto_remind": {
                    "soft_threshold_seconds": 300,
                    "hard_threshold_seconds": 600,
                }
            }
        }
        (tmp_path / "config.yaml").write_text(yaml.dump(config))
        soft, hard = get_auto_remind_config(str(tmp_path))
        assert soft == 300
        assert hard == 600

    def test_partial_config_falls_back_for_missing_keys(self, tmp_path):
        """Partial dispatch.auto_remind uses defaults for absent keys."""
        import yaml
        config = {
            "dispatch": {
                "auto_remind": {
                    "soft_threshold_seconds": 120,
                    # hard_threshold_seconds absent → use default
                }
            }
        }
        (tmp_path / "config.yaml").write_text(yaml.dump(config))
        soft, hard = get_auto_remind_config(str(tmp_path))
        assert soft == 120
        assert hard == DEFAULT_DISPATCH_HARD_THRESHOLD

    def test_walks_up_to_find_config(self, tmp_path):
        """Config.yaml in a parent directory is discovered by walking up."""
        import yaml
        config = {
            "dispatch": {
                "auto_remind": {
                    "soft_threshold_seconds": 180,
                    "hard_threshold_seconds": 360,
                }
            }
        }
        (tmp_path / "config.yaml").write_text(yaml.dump(config))
        subdir = tmp_path / "src" / "cli"
        subdir.mkdir(parents=True)
        soft, hard = get_auto_remind_config(str(subdir))
        assert soft == 180
        assert hard == 360

    def test_missing_dispatch_section_uses_defaults(self, tmp_path):
        """config.yaml without dispatch section uses defaults."""
        import yaml
        config = {"remind": {"soft_threshold_seconds": 180}}
        (tmp_path / "config.yaml").write_text(yaml.dump(config))
        soft, hard = get_auto_remind_config(str(tmp_path))
        assert soft == DEFAULT_DISPATCH_SOFT_THRESHOLD
        assert hard == DEFAULT_DISPATCH_HARD_THRESHOLD

    def test_invalid_yaml_uses_defaults(self, tmp_path):
        """Malformed config.yaml uses defaults without crashing."""
        (tmp_path / "config.yaml").write_text("{{bad: yaml: [}")
        soft, hard = get_auto_remind_config(str(tmp_path))
        assert soft == DEFAULT_DISPATCH_SOFT_THRESHOLD
        assert hard == DEFAULT_DISPATCH_HARD_THRESHOLD


class TestAutoRemindDispatch:
    """Tests that cmd_dispatch passes auto-remind thresholds to cmd_send."""

    def _make_client(self):
        mock_client = MagicMock()
        mock_client.get_session.return_value = {
            "id": "agent1",
            "friendly_name": "eng",
            "status": "running",
        }
        mock_client.session_id = "em-abc"
        mock_client.send_input.return_value = (True, False)
        return mock_client

    def test_dispatch_passes_default_remind_thresholds(self, sample_config):
        """cmd_dispatch passes default soft/hard thresholds to send_input."""
        mock_client = self._make_client()

        with patch("src.cli.commands.os.getcwd", return_value="/tmp"), \
             patch("src.cli.dispatch.load_template", return_value=sample_config), \
             patch("src.cli.dispatch.get_auto_remind_config",
                   return_value=(DEFAULT_DISPATCH_SOFT_THRESHOLD,
                                 DEFAULT_DISPATCH_HARD_THRESHOLD)):
            exit_code = cmd_dispatch(
                mock_client, "agent1", "engineer",
                {"issue": "42", "spec": "s.md"},
                em_id="em-abc",
            )

        assert exit_code == 0
        call_kwargs = mock_client.send_input.call_args[1]
        assert call_kwargs["remind_soft_threshold"] == DEFAULT_DISPATCH_SOFT_THRESHOLD
        assert call_kwargs["remind_hard_threshold"] == DEFAULT_DISPATCH_HARD_THRESHOLD

    def test_dispatch_passes_custom_remind_thresholds_from_config(self, sample_config):
        """cmd_dispatch uses thresholds from config when available."""
        mock_client = self._make_client()

        with patch("src.cli.commands.os.getcwd", return_value="/tmp"), \
             patch("src.cli.dispatch.load_template", return_value=sample_config), \
             patch("src.cli.dispatch.get_auto_remind_config",
                   return_value=(300, 600)):
            exit_code = cmd_dispatch(
                mock_client, "agent1", "engineer",
                {"issue": "42", "spec": "s.md"},
                em_id="em-abc",
            )

        assert exit_code == 0
        call_kwargs = mock_client.send_input.call_args[1]
        assert call_kwargs["remind_soft_threshold"] == 300
        assert call_kwargs["remind_hard_threshold"] == 600

    def test_dispatch_always_arms_remind_no_flag_needed(self, sample_config):
        """Remind is always armed on dispatch without any explicit flag."""
        mock_client = self._make_client()

        with patch("src.cli.commands.os.getcwd", return_value="/tmp"), \
             patch("src.cli.dispatch.load_template", return_value=sample_config), \
             patch("src.cli.dispatch.get_auto_remind_config",
                   return_value=(210, 420)):
            cmd_dispatch(
                mock_client, "agent1", "engineer",
                {"issue": "42", "spec": "s.md"},
                em_id="em-abc",
            )

        call_kwargs = mock_client.send_input.call_args[1]
        # Remind must be armed regardless of calling flags
        assert call_kwargs["remind_soft_threshold"] is not None
        assert call_kwargs["remind_hard_threshold"] is not None

    def test_dry_run_does_not_call_send(self, sample_config, capsys):
        """--dry-run mode prints template and does not call send_input."""
        mock_client = self._make_client()

        with patch("src.cli.commands.os.getcwd", return_value="/tmp"), \
             patch("src.cli.dispatch.load_template", return_value=sample_config):
            exit_code = cmd_dispatch(
                mock_client, "agent1", "engineer",
                {"issue": "42", "spec": "s.md"},
                em_id="em-abc",
                dry_run=True,
            )

        assert exit_code == 0
        mock_client.send_input.assert_not_called()
        captured = capsys.readouterr()
        assert "#42" in captured.out


class TestCmdSendRemindParams:
    """Tests that cmd_send correctly wires remind_soft/hard_threshold."""

    def _make_client(self):
        mock_client = MagicMock()
        mock_client.get_session.return_value = {
            "id": "sess1",
            "friendly_name": "eng",
            "status": "idle",
        }
        mock_client.session_id = "sender1"
        mock_client.send_input.return_value = (True, False)
        return mock_client

    def test_explicit_thresholds_passed_through(self):
        """Explicit remind_soft_threshold and remind_hard_threshold are forwarded."""
        from src.cli.commands import cmd_send
        mock_client = self._make_client()

        cmd_send(
            mock_client, "sess1", "hello",
            remind_soft_threshold=210,
            remind_hard_threshold=420,
        )

        call_kwargs = mock_client.send_input.call_args[1]
        assert call_kwargs["remind_soft_threshold"] == 210
        assert call_kwargs["remind_hard_threshold"] == 420

    def test_no_remind_params_sends_none(self):
        """Without any remind params, thresholds are None."""
        from src.cli.commands import cmd_send
        mock_client = self._make_client()

        cmd_send(mock_client, "sess1", "hello")

        call_kwargs = mock_client.send_input.call_args[1]
        assert call_kwargs["remind_soft_threshold"] is None
        assert call_kwargs["remind_hard_threshold"] is None


# ---------------------------------------------------------------------------
# sm setup tests (sm#225-D)
# ---------------------------------------------------------------------------

class TestCmdSetup:
    """Tests for cmd_setup and sm setup CLI."""

    def _default_template_path(self):
        from pathlib import Path
        return Path(__file__).parent.parent.parent / "src" / "cli" / "default_dispatch_templates.yaml"

    def test_creates_file_when_not_present(self, tmp_path):
        """sm setup installs default templates when dest doesn't exist."""
        from src.cli.commands import cmd_setup
        dest = tmp_path / ".sm" / "dispatch_templates.yaml"
        with patch("pathlib.Path.home", return_value=tmp_path):
            result = cmd_setup()
        assert result == 0
        assert dest.exists()

    def test_file_content_has_expected_roles(self, tmp_path):
        """Installed file contains engineer, architect, scout, reviewer roles."""
        import yaml
        from src.cli.commands import cmd_setup
        with patch("pathlib.Path.home", return_value=tmp_path):
            cmd_setup()
        dest = tmp_path / ".sm" / "dispatch_templates.yaml"
        data = yaml.safe_load(dest.read_text())
        assert "roles" in data
        roles = data["roles"]
        assert "engineer" in roles
        assert "architect" in roles
        assert "scout" in roles
        assert "reviewer" in roles

    def test_does_not_overwrite_existing(self, tmp_path, capsys):
        """sm setup prints a message and exits 0 if file already exists."""
        from src.cli.commands import cmd_setup
        dest = tmp_path / ".sm" / "dispatch_templates.yaml"
        dest.parent.mkdir(parents=True)
        dest.write_text("existing: content\n")

        with patch("pathlib.Path.home", return_value=tmp_path):
            result = cmd_setup(overwrite=False)

        assert result == 0
        assert dest.read_text() == "existing: content\n"
        captured = capsys.readouterr()
        assert "already installed" in captured.out or "overwrite" in captured.out.lower()

    def test_overwrite_flag_replaces_existing(self, tmp_path):
        """sm setup --overwrite replaces existing file."""
        from src.cli.commands import cmd_setup
        dest = tmp_path / ".sm" / "dispatch_templates.yaml"
        dest.parent.mkdir(parents=True)
        dest.write_text("old: content\n")

        with patch("pathlib.Path.home", return_value=tmp_path):
            result = cmd_setup(overwrite=True)

        assert result == 0
        content = dest.read_text()
        assert "old: content" not in content
        assert "roles:" in content

    def test_creates_parent_directory(self, tmp_path):
        """cmd_setup creates ~/.sm/ if it doesn't exist."""
        from src.cli.commands import cmd_setup
        sm_dir = tmp_path / ".sm"
        assert not sm_dir.exists()
        with patch("pathlib.Path.home", return_value=tmp_path):
            result = cmd_setup()
        assert result == 0
        assert sm_dir.is_dir()

    def test_cli_setup_no_overwrite(self, tmp_path):
        """sm setup (CLI) succeeds without --overwrite."""
        from src.cli.main import main
        with patch("pathlib.Path.home", return_value=tmp_path):
            with patch("sys.argv", ["sm", "setup"]):
                with pytest.raises(SystemExit) as exc_info:
                    main()
        assert exc_info.value.code == 0

    def test_cli_setup_with_overwrite_flag(self, tmp_path):
        """sm setup --overwrite is accepted by CLI."""
        from src.cli.main import main
        dest = tmp_path / ".sm" / "dispatch_templates.yaml"
        dest.parent.mkdir(parents=True)
        dest.write_text("old\n")
        with patch("pathlib.Path.home", return_value=tmp_path):
            with patch("sys.argv", ["sm", "setup", "--overwrite"]):
                with pytest.raises(SystemExit) as exc_info:
                    main()
        assert exc_info.value.code == 0
        assert "old" not in dest.read_text()


class TestDefaultTemplateExpansion:
    """Verify default templates expand correctly with sm dispatch."""

    def _load_default_templates(self):
        from pathlib import Path
        import yaml
        path = Path(__file__).parent.parent.parent / "src" / "cli" / "default_dispatch_templates.yaml"
        return yaml.safe_load(path.read_text())

    def test_engineer_role_expands(self):
        """Default engineer template expands with required params."""
        config = self._load_default_templates()
        expanded = expand_template(
            config, "engineer",
            {"issue": "123", "spec": "docs/123.md"},
            em_id="abc123",
        )
        assert "123" in expanded
        assert "docs/123.md" in expanded
        assert "abc123" in expanded

    def test_architect_role_expands(self):
        """Default architect template expands with required params."""
        config = self._load_default_templates()
        expanded = expand_template(
            config, "architect",
            {"pr": "42", "spec": "docs/42.md"},
            em_id="abc123",
        )
        assert "42" in expanded
        assert "docs/42.md" in expanded
        assert "abc123" in expanded

    def test_scout_role_expands(self):
        """Default scout template expands with required params."""
        config = self._load_default_templates()
        expanded = expand_template(
            config, "scout",
            {"issue": "99", "spec": "docs/99.md", "reviewer_id": "rev456"},
            em_id="abc123",
        )
        assert "99" in expanded
        assert "rev456" in expanded

    def test_reviewer_role_expands(self):
        """Default reviewer template expands with required params."""
        config = self._load_default_templates()
        expanded = expand_template(
            config, "reviewer",
            {"scout_id": "scout789"},
            em_id="abc123",
        )
        assert "scout789" in expanded

    def test_engineer_missing_required_raises(self):
        """Missing required param raises DispatchError."""
        config = self._load_default_templates()
        with pytest.raises(DispatchError, match="Missing required parameter"):
            expand_template(config, "engineer", {"issue": "1"}, em_id="x")
