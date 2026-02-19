"""Tests for sm dispatch — template-based dispatch with auto-expansion."""

import copy
import os
import sys
import textwrap
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from src.cli.dispatch import (
    DispatchError,
    expand_template,
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
