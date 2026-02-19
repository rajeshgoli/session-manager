# sm#185: Codex spawn bypass flag + action logging

## Problem

When `sm spawn codex` creates a codex session, it doesn't pass `--dangerously-bypass-approvals-and-sandbox`. Claude sessions get `--dangerously-skip-permissions` via `config.yaml` (`claude.args`), but the equivalent `codex.args` list is empty. The config plumbing exists — the missing piece is the config value itself. This means codex agents can't run `sm send` or other shell commands, breaking the review loop workflow.

## Root Cause Analysis

### Part 1: Bypass flag

The infrastructure already supports passing arbitrary args to codex. The code path is:

1. `config.yaml` `codex.args: []` (currently empty)
2. `src/session_manager.py:48-49` reads `self.codex_cli_args = codex_config.get("args", [])`
3. `src/session_manager.py:328-329` passes to `create_session_with_command(args=self.codex_cli_args)`
4. `src/tmux_controller.py:255-257` extends `cmd_parts` with `args`

The only missing piece is the config value. However, `codex.args` is also used as a fallback for codex-app sessions. At `src/session_manager.py:46`, if no `codex_app_server` section exists in config.yaml, `codex_app_config` falls back to `codex_config`. Then at line 55, `app_server_args` is resolved as `codex_app_config.get("app_server_args", codex_app_config.get("args", []))`. Since neither key exists in the fallback, it falls through to `codex.args`. These args are passed directly to the `codex app-server` CLI at `codex_app_server.py:77`. The `--dangerously-bypass-approvals-and-sandbox` flag is not a valid `app-server` subcommand option and would cause a CLI error.

**Fix:** Add `app_server_args: []` to the `codex` config section so the first `get()` resolves without falling through to `args`.

The codex CLI flag (`codex --help`) is:
```
--dangerously-bypass-approvals-and-sandbox
    Skip all confirmation prompts and execute commands without sandboxing.
    EXTREMELY DANGEROUS. Intended solely for running in environments
    that are externally sandboxed.
```

This is the exact equivalent of Claude's `--dangerously-skip-permissions`.

### Part 2: Action logging (research findings)

Claude Code tool logging works via a hook chain:
1. Claude Code fires `PreToolUse`/`PostToolUse` hook events
2. `hooks/log_tool_use.sh` receives JSON on stdin, injects `CLAUDE_SESSION_MANAGER_ID`
3. POSTs to `http://localhost:8420/hooks/tool-use`
4. `src/tool_logger.py` logs to `~/.local/share/claude-sessions/tool_usage.db`

**Codex has no equivalent hook system.** There are three possible logging paths:

#### Path A: CLI interactive sessions (provider=`codex`)

Codex CLI writes per-session rollout JSONL files to `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`. These contain:
- `session_meta` — session metadata (model, cwd, git info)
- `response_item` with `function_call` name + arguments and `function_call_output` results
- `event_msg` with agent reasoning, token counts
- `turn_context` with approval/sandbox policy

These files are written to disk during the session but are not exposed as a real-time event stream. To log tool usage from CLI codex sessions, sm would need to either:
- **Tail the rollout file** and parse JSONL events (fragile — file path depends on session UUID, no event bus)
- **Use `codex exec --json`** which outputs events as JSONL to stdout (non-interactive only — unsuitable for the current tmux-based interactive flow)

#### Path B: App-server sessions (provider=`codex-app`)

The codex app-server protocol (JSON-RPC over stdio) exposes rich notifications that already flow through `src/codex_app_server.py`:

| Notification | What it captures |
|---|---|
| `item/commandExecution/requestApproval` | Command to be executed (method, params) |
| `item/fileChange/requestApproval` | File change to be applied |
| `item/commandExecution/outputDelta` | Command execution output |
| `item/fileChange/outputDelta` | File change output |
| `item/started` / `item/completed` | Item lifecycle with type info |
| `rawResponseItem/completed` | Raw model response items |

The `_handle_notification()` method at `codex_app_server.py:326` currently only handles `turn/started`, `item/agentMessage/delta`, `turn/completed`, `item/started` (review mode), and `item/completed` (review mode). It silently drops all other notifications.

The `_handle_server_request()` method at `codex_app_server.py:384` handles approval requests (`item/commandExecution/requestApproval`, `item/fileChange/requestApproval`) by auto-responding with `self.config.approval_decision` (default: `"decline"` per `codex_app_server.py:22`), but does not log them.

#### Path C: Rollout file post-processing

After a codex CLI session completes, parse its rollout JSONL and backfill `tool_usage.db`. This is a batch approach — no real-time logging, but captures the same data.

## Proposed Solution

### Part 1: Config change

Add `--dangerously-bypass-approvals-and-sandbox` to `codex.args` and add explicit `app_server_args: []` to prevent the bypass flag from leaking into codex-app sessions:

```yaml
codex:
  command: "codex"
  args:
    - "--dangerously-bypass-approvals-and-sandbox"
  app_server_args: []   # Explicit empty list prevents fallback to args
  default_model: null
```

The `app_server_args: []` is required because `src/session_manager.py:55` resolves app-server args as `codex_app_config.get("app_server_args", codex_app_config.get("args", []))`. Without this key, when no `codex_app_server` section exists, the fallback chain would pass `codex.args` (including the bypass flag) to `codex app-server`, which doesn't accept that flag.

Also update `config.yaml.example` to document both options.

### Part 2: Action logging (two-phase approach)

**Phase 2a: App-server logging (low-hanging fruit)**

For `codex-app` sessions, intercept approval requests and item notifications in `codex_app_server.py` and log to `tool_usage.db` via the existing `ToolLogger`. Changes needed:
1. Pass a `ToolLogger` reference to `CodexAppServerSession`
2. In `_handle_server_request()`, log `commandExecution` and `fileChange` approval requests before responding
3. In `_handle_notification()`, log `item/started` and `item/completed` events for command/file items

**Phase 2b: CLI session rollout parsing (batch backfill)**

For `codex` CLI sessions:
1. When a codex CLI session stops, locate its rollout file in `~/.codex/sessions/`
2. Parse the JSONL for `function_call` / `function_call_output` response items
3. Backfill entries into `tool_usage.db` with the session's `CLAUDE_SESSION_MANAGER_ID`

**Trigger point limitation:** `ChildMonitor` only tracks sessions registered with `--wait` (`src/session_manager.py:528-534`). It is not a general session lifecycle hook. Codex CLI sessions spawned without `--wait` would not trigger rollout parsing. Options:
- Require `--wait` for all codex CLI spawns that need logging (operational constraint)
- Add a tmux exit hook or polling-based session lifecycle detector (code change)
- Parse rollouts on-demand via an `sm` CLI command (manual trigger)

Rollout file discovery: the rollout filename includes the codex session UUID (from `session_meta.payload.id`). The child monitor knows the tmux session name but not the codex session UUID. Options:
- Parse the latest rollout file in the date directory whose `session_meta.cwd` matches the session's working directory
- Store the codex session UUID when it's detected (e.g., by watching the first line of the session's log file)

### Part 2 alternative: Do nothing for now

The bypass flag (Part 1) is the immediate blocker. Action logging is a "nice to have" for permission profiling. If the team wants to defer Part 2, the config change alone unblocks the review loop workflow.

## Implementation Approach

### Part 1 (config change)
1. Edit `config.yaml`: add `--dangerously-bypass-approvals-and-sandbox` to `codex.args` and `app_server_args: []`
2. Edit `config.yaml.example`: add equivalent entries with comments
3. Verify: inspect the tmux session's command line to confirm the bypass flag is present in the spawned codex process

### Part 2a (app-server logging)
1. Add `tool_logger: Optional[ToolLogger]` param to `CodexAppServerSession.__init__()`
2. In `_handle_server_request()`: before responding, call `tool_logger.log()` with extracted command/file info
3. In `_handle_notification()`: add handler for `item/started`/`item/completed` with type=`commandExecution`/`fileChange`
4. Wire up: pass `tool_logger` from `session_manager.py` when creating `CodexAppServerSession`

### Part 2b (rollout parsing)
1. Add `parse_codex_rollout(path: Path) -> list[ToolEvent]` utility
2. Add trigger point — either via child monitor completion handler (requires `--wait`), on-demand CLI command, or tmux exit hook
3. Add rollout file discovery logic (match by cwd + timestamp proximity)

## Test Plan

### Part 1
- Spawn a codex CLI session via `sm spawn codex` and verify the tmux pane shows `codex --dangerously-bypass-approvals-and-sandbox` in the running command (deterministic check)
- From within the spawned session, run `sm send <parent> "test"` to verify shell commands execute without approval prompts (the original user pain point)
- Spawn a `codex-app` session and verify it starts without error (confirms `app_server_args: []` prevents the bypass flag from leaking)
- Verify crash recovery (`session_manager.py:1683`) — codex sessions currently don't have crash recovery, so no regression risk

### Part 2a
- Spawn a `codex-app` session, send a turn that triggers a command execution
- Query `tool_usage.db` and verify the command was logged with correct session metadata

### Part 2b
- Run a codex CLI session that executes several commands
- After session stops, verify rollout was parsed and entries appear in `tool_usage.db`

## Ticket Classification

**Part 1** is a single config change — no code needed. Can be done in under a minute.

**Part 2a** is a small code change (4 touchpoints). Single ticket.

**Part 2b** is a medium code change (new utility + integration). Single ticket, but could be deferred.

Recommendation: File Part 1 as its own ticket (or just merge the config change directly). File Part 2a and Part 2b as separate tickets if the team wants action logging.
