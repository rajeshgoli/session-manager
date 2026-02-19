# sm#187: sm dispatch — Template-Based Dispatch with Auto-Expansion

## Problem

Every EM dispatch includes 150-200 chars of boilerplate: repo path, spec path, test command, PR target, "report back to me via sm send", role-specific instructions. This is repeated for every dispatch across every session. It wastes EM tokens and is error-prone (e.g., forgetting to include persona path, wrong PR target).

## Root Cause

`sm send` is a raw text transport — it has no concept of structured dispatch patterns. The EM agent must manually compose the full prompt text for every dispatch, including repo-specific constants and role-specific boilerplate that never change.

## Proposed Solution

Add `sm dispatch` — a template-based wrapper around `sm send` that resolves role templates from YAML config, auto-fills repo and session variables, accepts required/optional CLI parameters, and sends the expanded prompt to the target agent.

## Design

### Template File Location

Discovery order (first found wins):
1. Walk up from CWD looking for `.sm/dispatch_templates.yaml` — checks CWD, then parent, grandparent, etc., stopping at filesystem root. This matches git's `.git/` discovery convention and ensures dispatch works correctly from repo subdirectories.
2. `~/.sm/dispatch_templates.yaml` (global fallback)

If neither is found, `sm dispatch` prints an error and exits with code 1.

### Template File Format

```yaml
repo:
  path: /Users/rajesh/Desktop/fractal-market-simulator
  pr_target: dev
  test_command: "source venv/bin/activate && PYTHONPATH=. python -m pytest tests/ -v"

roles:
  engineer:
    template: |
      As engineer, implement GitHub issue #{issue} in {repo.path}.
      Read the spec at {spec}.
      Work on a feature branch off {repo.pr_target}, create a PR to {repo.pr_target} when done.
      Run tests when done: {repo.test_command}
      Report the PR number back to me ({em_id}) via sm send.
    required: [issue, spec]
    optional: [extra]

  architect:
    template: |
      As architect, review PR #{pr} in {repo.path}.
      Read the spec at {spec} for context.
      Report all feedback as blocking.
      Do NOT write code.
      sm send verdict to me ({em_id}).
    required: [pr, spec]
    optional: [extra]

  scout:
    template: |
      As scout, investigate GitHub issue #{issue} in {repo.path}.
      Read personas/scout.md from ~/.agent-os/personas/.
      Write spec to {spec}.
      When done, send spec to codex reviewer ({reviewer_id}) via sm send.
      When converged, sm send completion to me ({em_id}).
    required: [issue, spec, reviewer_id]
    optional: [extra]

  reviewer:
    template: |
      You are a spec reviewer. Working directory: {repo.path}.
      Review protocol is in ~/.agent-os/personas/em.md.
      You will receive a spec from scout agent ({scout_id}) via sm send.
      Classify feedback by severity. Send review to spec owner ({scout_id}) via sm send.
      Stand by.
    required: [scout_id]
    optional: [extra]
```

### CLI Interface

```
sm dispatch <agent-id> --role <role> [--<param> <value>]... [--urgent|--important] [--dry-run]
```

**Positional arguments:**
| Arg | Description |
|-----|-------------|
| `agent-id` | Target session ID or friendly name (same resolution as `sm send`) |

**Required flags:**
| Flag | Description |
|------|-------------|
| `--role <name>` | Role name matching a key under `roles:` in the template YAML |

**Dynamic flags** — derived from the role's `required` and `optional` lists:
| Flag | Description |
|------|-------------|
| `--<param> <value>` | Each entry in `required` and `optional` becomes a CLI flag. E.g., `required: [issue, spec]` means `--issue` and `--spec` are accepted. |

**Static flags:**
| Flag | Description |
|------|-------------|
| `--urgent` | Pass through to `sm send --urgent` |
| `--important` | Pass through to `sm send --important` |
| `--steer` | Pass through to `sm send --steer` (mid-turn steering for Codex) |
| `--dry-run` | Print expanded template to stdout instead of sending |
| `--no-notify-on-stop` | Pass through to `sm send --no-notify-on-stop` |

**Delivery mode precedence** (matches `sm send`): `urgent > important > steer > sequential` (default). Flags are not mutually exclusive; highest-precedence flag wins.

### Auto-Filled Variables

These are resolved at dispatch time without CLI input:

| Variable | Source | Notes |
|----------|--------|-------|
| `{repo.path}` | `repo.path` from template YAML | — |
| `{repo.pr_target}` | `repo.pr_target` from template YAML | — |
| `{repo.test_command}` | `repo.test_command` from template YAML | — |
| `{em_id}` | `CLAUDE_SESSION_MANAGER_ID` env var | Sender's own session ID |

### Variable Resolution

1. Load template YAML from discovery path
2. Look up `roles.<role>` — error if not found
3. Collect all `{repo.*}` references from template, resolve from `repo:` section
4. Set `{em_id}` from `CLAUDE_SESSION_MANAGER_ID`
5. For each entry in `required`: require corresponding `--<param>` flag; error if missing
6. For each entry in `optional`: use `--<param>` value if provided, otherwise remove the line containing only that variable (or replace with empty string if inline)
7. If `{extra}` is provided, append it as a new line at the end of the expanded template
8. Validate no unresolved `{...}` placeholders remain — error if any found

### Delivery Flow

```
sm dispatch <id> --role engineer --issue 1668 --spec docs/working/1668.md
```

Expands to:

```
As engineer, implement GitHub issue #1668 in /Users/rajesh/Desktop/fractal-market-simulator.
Read the spec at docs/working/1668.md.
Work on a feature branch off dev, create a PR to dev when done.
Run tests when done: source venv/bin/activate && PYTHONPATH=. python -m pytest tests/ -v
Report the PR number back to me (c3bbc6b9) via sm send.
```

Then calls `cmd_send(client, <id>, expanded_text, delivery_mode)` — reusing the existing send infrastructure completely.

### Error Handling

| Condition | Behavior |
|-----------|----------|
| No template file found | `Error: No dispatch template found. Expected .sm/dispatch_templates.yaml or ~/.sm/dispatch_templates.yaml` (exit 1) |
| Unknown role | `Error: Role 'foo' not found in template. Available: engineer, architect, scout, reviewer` (exit 1) |
| Missing required param | `Error: Missing required parameter '--issue' for role 'engineer'` (exit 1) |
| Unresolved placeholder | `Error: Unresolved variable '{reviewer_id}' in template` (exit 1) |
| YAML parse error | `Error: Failed to parse dispatch template: <yaml error>` (exit 1) |
| `em_id` not available (send mode) | `Error: CLAUDE_SESSION_MANAGER_ID not set. Use --dry-run to test templates outside managed sessions.` (exit 1) |
| `em_id` not available (dry-run) | Warning printed to stderr; `{em_id}` resolves to `<unset>` placeholder. Allows template testing outside managed sessions without producing silently broken prompts. |

## Implementation Approach

### New Files

1. **`src/cli/dispatch.py`** — Template loading, variable resolution, expansion logic:
   - `load_template(working_dir: str) -> dict` — walks up from `working_dir` looking for `.sm/dispatch_templates.yaml`, falls back to `~/.sm/`, raises if neither found
   - `expand_template(template_config: dict, role: str, params: dict, em_id: Optional[str]) -> str` — resolves all variables
   - `get_role_params(template_config: dict, role: str) -> tuple[list, list]` — returns (required, optional) param names

### Modified Files

2. **`src/cli/main.py`** — Add `dispatch` subparser and dispatch block:
   - Register `dispatch` subparser with positional `agent_id`, `--role`, `--dry-run`, `--steer`, delivery mode flags
   - Dynamic param handling is scoped to dispatch only (see Argument Parsing Strategy below)
   - Dispatch to `cmd_dispatch` in commands.py

3. **`src/cli/commands.py`** — Add `cmd_dispatch` handler:
   - Calls `load_template()` and `expand_template()` from dispatch.py
   - On `--dry-run`: print and exit 0
   - Otherwise: call `cmd_send()` with the expanded text and delivery mode flags

4. **`no_session_needed` list** in `main.py` — Add `"dispatch"` only when `--dry-run` is present. Without `--dry-run`, `dispatch` requires `CLAUDE_SESSION_MANAGER_ID` for `{em_id}` resolution.

### Argument Parsing Strategy

The challenge: dynamic flags from `required`/`optional` lists aren't known at argparse definition time.

**Critical constraint:** The top-level `parser.parse_args()` call (main.py:216) MUST remain unchanged for all other commands. Changing it to `parse_known_args()` would silently swallow typos/unknown flags on every command.

**Approach: Pre-intercept for dispatch only.**

1. Before calling `parser.parse_args()`, check if `sys.argv[1] == "dispatch"` (or `sys.argv` has no args).
2. If NOT dispatch: proceed through `parser.parse_args()` as today — zero behavioral change for all existing commands.
3. If dispatch: extract `sys.argv[2:]` and parse in two phases:
   - Phase 1: Build a dedicated `argparse.ArgumentParser` for dispatch's known args (`agent_id`, `--role`, `--dry-run`, `--urgent`, `--important`, `--steer`, `--no-notify-on-stop`). Call `parse_known_args()` on this dedicated parser only.
   - Phase 2: Load template YAML, look up the role, get required/optional param names. Parse the remaining args as `--key value` pairs (simple loop: if arg starts with `--`, strip prefix, next arg is value).
   - Validate: all `required` params present, no unknown params.

This keeps dynamic parsing completely isolated to dispatch — existing commands retain strict validation.

### `extra` Handling

The `extra` parameter (commonly in `optional`) is special:
- If provided via `--extra "some instructions"`, it's appended as a final line to the expanded template
- Unlike other optional params which replace inline `{param}`, `extra` is always appended (never inline-replaced)
- If not provided, nothing is appended

## Test Plan

### Unit Tests (`tests/unit/test_dispatch.py`)

1. **Template discovery** — walks up from CWD to find `.sm/dispatch_templates.yaml`, falls back to `~/.sm/`, errors when neither exists; finds template from subdirectory of repo root
2. **Variable expansion** — all `{repo.*}` and `{em_id}` vars resolve correctly
3. **Required param validation** — missing required param → error
4. **Optional param handling** — optional params resolve when provided, lines removed when absent
5. **Extra handling** — `--extra` text appended as final line
6. **Unknown role** — clear error with available roles listed
7. **Unresolved placeholder detection** — catches `{foo}` left in expanded text
8. **YAML parse error** — graceful error message
9. **`em_id` required for send** — errors when `CLAUDE_SESSION_MANAGER_ID` not set (without `--dry-run`); resolves to `<unset>` with warning in `--dry-run` mode

### Integration Tests

10. **`--dry-run`** — prints expanded template, does not call send
11. **Full dispatch** — calls `cmd_send` with correct expanded text and delivery mode
12. **Delivery mode passthrough** — `--urgent` / `--important` / `--steer` forwarded to send with correct precedence
13. **Existing commands unaffected** — verify `sm send --typo` still errors (no silent swallowing from parse_known_args leak)

## Ticket Classification

**Single ticket.** The implementation is:
- ~150 lines for `dispatch.py` (template loading + expansion)
- ~40 lines for the subparser in `main.py`
- ~30 lines for `cmd_dispatch` in `commands.py`
- ~100 lines for unit tests

One agent can complete this without compacting context.
