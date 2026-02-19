"""Template-based dispatch for sm CLI.

Loads dispatch templates from YAML config, resolves variables,
and expands templates for sending to target agents.
"""

import os
import re
import sys
from pathlib import Path
from typing import Optional


class DispatchError(Exception):
    """Raised when dispatch template loading or expansion fails."""
    pass


def load_template(working_dir: str) -> dict:
    """Load dispatch template YAML by walking up from working_dir.

    Discovery order:
    1. Walk up from working_dir looking for .sm/dispatch_templates.yaml
    2. ~/.sm/dispatch_templates.yaml (global fallback)

    Args:
        working_dir: Directory to start searching from.

    Returns:
        Parsed YAML dict.

    Raises:
        DispatchError: If no template file found or YAML parse error.
    """
    import yaml

    # Phase 1: Walk up from working_dir
    current = Path(working_dir).resolve()
    while True:
        candidate = current / ".sm" / "dispatch_templates.yaml"
        if candidate.is_file():
            return _parse_yaml(candidate)
        parent = current.parent
        if parent == current:
            break  # filesystem root
        current = parent

    # Phase 2: Global fallback
    global_path = Path.home() / ".sm" / "dispatch_templates.yaml"
    if global_path.is_file():
        return _parse_yaml(global_path)

    raise DispatchError(
        "No dispatch template found. "
        "Expected .sm/dispatch_templates.yaml or ~/.sm/dispatch_templates.yaml"
    )


def _parse_yaml(path: Path) -> dict:
    """Parse a YAML file, wrapping errors in DispatchError."""
    import yaml

    try:
        with open(path) as f:
            data = yaml.safe_load(f)
    except yaml.YAMLError as e:
        raise DispatchError(f"Failed to parse dispatch template: {e}")

    if not isinstance(data, dict):
        raise DispatchError(f"Failed to parse dispatch template: expected mapping, got {type(data).__name__}")

    return data


def get_role_params(template_config: dict, role: str) -> tuple[list, list]:
    """Get required and optional parameter names for a role.

    Args:
        template_config: Parsed template YAML dict.
        role: Role name.

    Returns:
        Tuple of (required, optional) param name lists.

    Raises:
        DispatchError: If role not found.
    """
    roles = template_config.get("roles", {})
    if role not in roles:
        available = ", ".join(sorted(roles.keys()))
        raise DispatchError(
            f"Role '{role}' not found in template. Available: {available}"
        )

    role_config = roles[role]
    required = role_config.get("required", [])
    optional = role_config.get("optional", [])
    return required, optional


def expand_template(
    template_config: dict,
    role: str,
    params: dict,
    em_id: Optional[str],
    dry_run: bool = False,
) -> str:
    """Expand a role template with resolved variables.

    Args:
        template_config: Parsed template YAML dict.
        role: Role name.
        params: Dict of param_name -> value from CLI flags.
        em_id: Sender's session ID (from CLAUDE_SESSION_MANAGER_ID).
        dry_run: If True, allow missing em_id with placeholder.

    Returns:
        Expanded template string.

    Raises:
        DispatchError: On missing required params, unknown role, or unresolved placeholders.
    """
    roles = template_config.get("roles", {})
    if role not in roles:
        available = ", ".join(sorted(roles.keys()))
        raise DispatchError(
            f"Role '{role}' not found in template. Available: {available}"
        )

    role_config = roles[role]
    template_text = role_config.get("template", "")
    required = role_config.get("required", [])
    optional = role_config.get("optional", [])

    # Validate required params
    for param in required:
        if param not in params:
            raise DispatchError(
                f"Missing required parameter '--{param}' for role '{role}'"
            )

    # Build substitution map
    subs = {}

    # Resolve repo.* variables
    repo = template_config.get("repo", {})
    for key, value in repo.items():
        subs[f"repo.{key}"] = str(value)

    # Resolve em_id
    if em_id:
        subs["em_id"] = em_id
    elif dry_run:
        print("Warning: CLAUDE_SESSION_MANAGER_ID not set; {em_id} resolves to <unset>", file=sys.stderr)
        subs["em_id"] = "<unset>"
    # If not dry_run and no em_id, we'll catch unresolved {em_id} below

    # Add required params
    for param in required:
        subs[param] = params[param]

    # Handle optional params (except 'extra' which is appended)
    extra_value = params.get("extra")
    for param in optional:
        if param == "extra":
            continue
        if param in params:
            subs[param] = params[param]

    # Perform substitutions
    expanded = template_text

    # Replace all {key} placeholders with values from subs
    def replace_var(match: re.Match) -> str:
        var_name = match.group(1)
        if var_name in subs:
            return subs[var_name]
        return match.group(0)  # Leave unresolved for error check

    expanded = re.sub(r'\{([a-zA-Z_][a-zA-Z0-9_.]*)\}', replace_var, expanded)

    # Handle unresolved optional params: remove lines containing only unresolved optional vars
    for param in optional:
        if param == "extra":
            continue
        if param not in params:
            # Remove lines that contain only this unresolved variable (with optional whitespace)
            pattern = rf'^[^\S\n]*\{{{param}\}}[^\S\n]*\n'
            expanded = re.sub(pattern, '', expanded, flags=re.MULTILINE)
            # Also replace any remaining inline occurrences with empty string
            expanded = expanded.replace(f'{{{param}}}', '')

    # Append extra if provided
    if extra_value:
        expanded = expanded.rstrip('\n') + '\n' + extra_value

    # Check for unresolved placeholders
    unresolved = re.findall(r'\{([a-zA-Z_][a-zA-Z0-9_.]*)\}', expanded)
    if unresolved:
        raise DispatchError(
            f"Unresolved variable '{{{unresolved[0]}}}' in template"
        )

    return expanded.strip()


def parse_dispatch_args(argv: list[str]) -> tuple:
    """Parse dispatch-specific arguments from sys.argv[2:].

    Two-phase parsing:
    1. Parse known static args (agent_id, --role, --dry-run, delivery modes)
    2. Parse remaining dynamic args as --key value pairs

    Args:
        argv: sys.argv[2:] (everything after 'dispatch')

    Returns:
        Tuple of (agent_id, role, dry_run, delivery_mode, notify_on_stop, dynamic_params)

    Raises:
        SystemExit: On parse errors (via argparse).
    """
    import argparse

    # Phase 1: Parse known static args
    static_parser = argparse.ArgumentParser(
        prog="sm dispatch",
        description=(
            "Dispatch a template-expanded prompt to a target agent.\n"
            "Dynamic flags (e.g. --issue, --spec) are derived from the role template."
        ),
    )
    static_parser.add_argument("agent_id", help="Target agent ID or friendly name")
    static_parser.add_argument("--role", required=True, help="Role name from template")
    static_parser.add_argument("--dry-run", action="store_true", help="Print expanded template without sending")
    static_parser.add_argument("--urgent", action="store_true", help="Pass through to sm send --urgent")
    static_parser.add_argument("--important", action="store_true", help="Pass through to sm send --important")
    static_parser.add_argument("--steer", action="store_true", help="Pass through to sm send --steer")
    static_parser.add_argument("--no-notify-on-stop", action="store_true", help="Pass through to sm send --no-notify-on-stop")

    known, remaining = static_parser.parse_known_args(argv)

    # Determine delivery mode (precedence: urgent > important > steer > sequential)
    delivery_mode = "sequential"
    if known.urgent:
        delivery_mode = "urgent"
    elif known.important:
        delivery_mode = "important"
    elif known.steer:
        delivery_mode = "steer"

    notify_on_stop = not known.no_notify_on_stop

    # Phase 2: Parse remaining dynamic args as --key value pairs
    dynamic_params = {}
    i = 0
    while i < len(remaining):
        arg = remaining[i]
        if arg.startswith("--"):
            key = arg[2:]  # strip --
            if i + 1 < len(remaining) and not remaining[i + 1].startswith("--"):
                dynamic_params[key] = remaining[i + 1]
                i += 2
            else:
                print(f"Error: Flag '--{key}' requires a value", file=sys.stderr)
                sys.exit(1)
        else:
            print(f"Error: Unexpected argument: {arg}", file=sys.stderr)
            sys.exit(1)

    return (
        known.agent_id,
        known.role,
        known.dry_run,
        delivery_mode,
        notify_on_stop,
        dynamic_params,
    )
