# sm#234: sm dispatch should auto-clear before sending (add --no-clear flag)

## Problem

`sm dispatch` sends a template-expanded message but does NOT clear the agent first. The EM persona doc says "sm dispatch handles clear, send, and wait in one command" — but it only does `send`.

When dispatching a new role to an agent that just finished a different task, stale context causes:
- 3+ minute churn as the agent processes old context before starting on new instructions
- Risk of the agent conflating old task context with the new task
- Agent starts at 50%+ context before even beginning new work

## Root Cause

`cmd_dispatch` in `src/cli/commands.py` calls `expand_template` then `cmd_send` — no clearing step exists. The EM persona doc's phrase "handles clear, send, and wait" is aspirational documentation of desired behavior, not current behavior.

## Proposed Solution

Make `sm dispatch` clear the target session before sending by default, with `--no-clear` to opt out when doing minor follow-up dispatches on the same task.

```bash
# Default: clear then send (new role dispatch)
sm dispatch cc4d95a7 --role architect --urgent --pr 231 --repo ...

# Opt-out: skip clear (follow-up on same task, agent retains context)
sm dispatch cc4d95a7 --role architect --no-clear --urgent --pr 231 --repo ...
```

This matches EM best practice: "clear before every NEW dispatch." The opt-out covers the re-dispatch pattern in em.md:
> Re-dispatch template: Do NOT clear — agent retains context from prior review/work.

### Why `--no-clear` (clear by default) over `--clear` (opt-in)

- Clear before new-role dispatch is the rule, not the exception. EM persona explicitly states "Always: sm clear before every dispatch"
- Most dispatch calls in practice are new-role dispatches. Opt-in `--clear` means EM agents forget it and the stale-context problem recurs
- The only safe case for skipping clear is documented in em.md as the "Re-dispatch template" — a narrow pattern for minor follow-ups. This is the exception that warrants an explicit flag

## Breaking Change

**This is a behavior change for existing callers.** Any `sm dispatch` invocation that targets a non-child session (or targets a session where the caller is not the parent) will now fail by default with:

```
Error: Not authorized. You can only clear your child sessions.
```

**Migration:** Any dispatch that legitimately skips clearing (follow-ups, cross-session dispatches) must add `--no-clear`:

```bash
# Before (current behavior — no clear)
sm dispatch <agent-id> --role architect --urgent --pr 231 --spec ...

# After (same behavior explicitly)
sm dispatch <agent-id> --role architect --no-clear --urgent --pr 231 --spec ...
```

**Output change:** When clear runs (default), `sm dispatch` output now includes `cmd_clear` output before the send confirmation:

```
Cleared engineer-1465 (cc4d95a7)
Input sent to engineer-1465 (cc4d95a7) (interrupted)
```

Any automation, snapshot tests, or scripts that parse `sm dispatch` stdout should be updated to handle the additional clear output line.

## Implementation Approach

### `src/cli/dispatch.py` — `parse_dispatch_args`

Add `--no-clear` flag to the static parser:

```python
static_parser.add_argument("--no-clear", action="store_true",
    help="Skip clearing target session before dispatch (use for follow-up dispatches on same task)")
```

Return the `no_clear` value in the tuple from `parse_dispatch_args`. Update the docstring return shape from 6 fields to 7:

```python
return (
    known.agent_id,
    known.role,
    known.dry_run,
    known.no_clear,    # <-- added (field 4 of 7)
    delivery_mode,
    notify_on_stop,
    dynamic_params,
)
```

### `src/cli/main.py` — `_handle_dispatch`

Unpack `no_clear` from `parse_dispatch_args` and pass it to `cmd_dispatch`:

```python
agent_id, role, dry_run, no_clear, delivery_mode, notify_on_stop, dynamic_params = \
    parse_dispatch_args(sys.argv[2:])

return commands.cmd_dispatch(
    client, agent_id, role, dynamic_params, em_id,
    dry_run=dry_run, no_clear=no_clear,
    delivery_mode=delivery_mode, notify_on_stop=notify_on_stop,
)
```

### `src/cli/commands.py` — `cmd_dispatch`

Add `no_clear: bool = False` parameter. Before calling `cmd_send`, conditionally call `cmd_clear`:

```python
def cmd_dispatch(
    client, agent_id, role, params, em_id,
    dry_run=False, no_clear=False,
    delivery_mode="sequential", notify_on_stop=True,
) -> int:
    # ... existing template load/expand logic ...

    if dry_run:
        print(expanded)
        return 0

    # Clear target before dispatch unless opted out
    if not no_clear:
        rc = cmd_clear(client, em_id, agent_id)  # requester = self (em_id)
        if rc != 0:
            return rc  # propagate error (session not found, not authorized, etc.)

    return cmd_send(client, agent_id, expanded, delivery_mode, notify_on_stop=notify_on_stop)
```

**Authorization note:** `cmd_clear` requires the requester to be the parent of the target session. Since EM agents only dispatch to their own children, this naturally holds. If `em_id` is the parent, clear succeeds. If `em_id` is not the parent, clear returns exit code 1 and `cmd_dispatch` propagates it — the caller gets a clear error message rather than silently skipping the clear.

**Dry-run behavior:** `--dry-run` skips the clear step (prints template only, no side effects). This is unchanged from current behavior.

**`--no-clear` + `--dry-run`:** Allowed. `--dry-run` already implies no side effects, so `--no-clear` is redundant but harmless.

### Error handling

| Condition | Behavior |
|-----------|----------|
| Clear fails (not authorized) | Propagates exit 1 with `cmd_clear`'s error message |
| Clear fails (session not found) | Propagates exit 1 with session-not-found error |
| Clear fails (server unavailable) | Propagates exit 2 |
| `--no-clear` + clear skipped | Proceeds directly to send |

## Test Plan

### Unit Tests (`tests/unit/test_dispatch.py`)

1. **`parse_dispatch_args` — `--no-clear` parsed** — `--no-clear` flag sets `no_clear=True` in return tuple
2. **`parse_dispatch_args` — default no-clear is False** — omitting `--no-clear` returns `no_clear=False`

### Integration Tests (`tests/integration/` or `tests/unit/test_dispatch_cmd.py`)

3. **Default behavior clears before send** — mock `cmd_clear` and `cmd_send`; verify `cmd_clear` called before `cmd_send` when `no_clear=False`
4. **`--no-clear` skips clear** — verify `cmd_clear` not called when `no_clear=True`
5. **`--dry-run` skips clear** — verify `cmd_clear` not called in dry-run mode regardless of `no_clear`
6. **Clear failure aborts send** — mock `cmd_clear` returning exit 1; verify `cmd_send` never called
7. **Existing dispatch tests pass** — no regressions on template expansion, param validation, delivery mode passthrough

### Manual verification

```bash
# New-role dispatch: verify clear fires first (check session output cleared)
sm dispatch <child-id> --role engineer --urgent --issue 123 --spec docs/working/123.md

# Follow-up dispatch: verify clear skipped
sm dispatch <child-id> --role engineer --no-clear --urgent --issue 123 --spec docs/working/123.md
```

## Ticket Classification

**Single ticket.** Changes are contained to three files (`dispatch.py`, `main.py`, `commands.py`), ~30 lines total plus tests. One agent can complete without compacting context.
