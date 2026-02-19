# sm#233: sm em — one-shot EM pre-flight command

## Problem

EM pre-flight setup requires 3–5 manual steps that are easy to forget, especially after a handoff where the new EM session has no memory of previous setup:

```bash
sm name em-session9          # 1. Set identity
sm context-monitor enable    # 2. Register self for context warnings
# 3. For each existing child: sm context-monitor enable <child-id>
# 4. For each existing child: register periodic remind
```

Missing `sm context-monitor enable` means no context warnings. Missing child registration means children compact silently. Missing remind setup means EM may not notice when children stall. These failures accumulate over sessions.

## Root Cause

Pre-flight is documented in `em.md` but not enforced or automated. Post-handoff EM sessions start without memory of the previous EM's setup, and the persona doc is not re-read on every session start. The result is consistent omission of monitoring setup.

## Proposed Solution

Add `sm em [name]` — an explicit opt-in command that bundles all EM-specific setup and prints a summary of what was registered.

```bash
sm em session9
# equivalent to:
# sm name em-session9
# sm context-monitor enable                       (self-monitoring)
# sm context-monitor enable <child1-id>           (for each existing child)
# sm context-monitor enable <child2-id>
# POST /sessions/<child1-id>/remind soft=180 hard=300  (for each existing child)
# POST /sessions/<child2-id>/remind soft=180 hard=300
# Prints summary of all steps performed
```

Any agent that runs `sm em` is declaring itself EM. No automatic detection — explicit opt-in.

## Design

### CLI Interface

```
sm em [name]
```

| Argument | Description |
|----------|-------------|
| `name` (optional) | Suffix for friendly name. If provided, sets name to `em-<name>`. If omitted, sets name to `em` (no suffix). |

**Examples:**

```bash
sm em session9     # sets name to em-session9
sm em              # sets name to em
```

### Steps Performed

1. **Validate name** — calls `validate_friendly_name(friendly_name)` (same validation as `cmd_name`); exit 1 if invalid
2. **Set name** — calls `update_friendly_name(session_id, f"em-{name}" if name else "em")`
3. **Enable self context-monitoring** — calls `set_context_monitor(session_id, enabled=True, requester=session_id, notify_session_id=session_id)`
4. **List existing children** — calls `client.list_children(session_id)`; uses `child["id"]` key (matches existing commands.py convention)
5. **Register each child for context-monitoring** — for each child, calls `set_context_monitor(child["id"], enabled=True, requester=session_id, notify_session_id=session_id)`
6. **Register periodic remind for each child** — for each child, calls `client.register_remind(child["id"], soft_threshold=180, hard_threshold=300)` (fixed thresholds — see Remind Threshold Policy below)

### Output

```
EM pre-flight complete:
  Name set: em-session9 (a1b2c3d4)
  Context monitoring: enabled (notifications → self)
  Children processed: 2 (2 succeeded, 0 failed)
    scout-1465 (b2c3d4e5) → context monitoring enabled, remind registered (soft=180s, hard=300s)
    engineer-1465 (c3d4e5f6) → context monitoring enabled, remind registered (soft=180s, hard=300s)
```

If no children exist:
```
EM pre-flight complete:
  Name set: em-session9 (a1b2c3d4)
  Context monitoring: enabled (notifications → self)
  No existing children found.
```

If some child registrations fail:
```
  Children processed: 2 (1 succeeded, 1 failed)
    scout-1465 (b2c3d4e5) → context monitoring enabled, remind registered (soft=180s, hard=300s)
    engineer-1465 (c3d4e5f6) → Warning: context monitoring failed; remind registration failed
```

### Remind Threshold Policy

`sm em` uses fixed thresholds: `soft=180s, hard=300s`. These match the server's config defaults (`remind.soft_threshold_seconds=180`, `remind.hard_gap_seconds=120` → hard = 180+120 = 300). The client cannot read server-side config, so these values are baked into `cmd_em` as intentional fixed policy. If an operator changes the server's remind config, the new values won't automatically be reflected in `sm em` — they'd need to run `sm send <child> ... --remind N` explicitly. This is an acceptable tradeoff for the common case.

### Error Handling

**Strict exit (stop immediately):**
- No `CLAUDE_SESSION_MANAGER_ID` → exit 2 before any API calls
- Invalid name → exit 1 before any API calls
- Server unavailable during name set → exit 2
- Server unavailable during self context-monitor step → exit 2
- Server unavailable during `list_children` (returns `None`) → exit 2

**Best-effort (warn and continue):**
- Name set fails (non-unavailable API error) → print warning, continue
- Self context-monitor fails (non-unavailable) → print warning, continue
- Any child-level failure (context-monitor or remind) → print warning for that child, count as failed, continue with remaining children

Child-level remind failures are always treated as warning/continue — `client.register_remind()` returns `Optional[dict]` with no distinction between server unavailability and API failure. Since both outcomes result in the same action (warn + continue), no `(success, unavailable)` split is needed for this call.

### Client Fix Required: `list_children()` error masking

**Current behavior (bug):** `client.list_children()` returns `{"children": []}` on non-success API responses (line 280, `client.py`). Only `unavailable=True` returns `None`. This means a 500 or 404 from the server appears as "no children" instead of surfacing the error.

**Required fix:** Change `list_children()` to return `None` on both unavailable and non-success responses, consistent with `list_sessions()`:

```python
# src/cli/client.py — list_children()
data, success, unavailable = self._request("GET", path)
if unavailable or not success:   # <-- was: if unavailable
    return None
return data
```

`cmd_em` already handles `None` as exit 2 (server unavailable). This fix ensures API errors on the children endpoint are surfaced rather than silently treated as empty.

This is a small, focused client fix included in this ticket's scope.

## Implementation Approach

### `src/cli/main.py`

Add `em` subparser:

```python
em_parser = subparsers.add_parser("em", help="EM pre-flight: set name, enable context monitoring, register children")
em_parser.add_argument("name", nargs="?", default=None, help="Name suffix (sets friendly name to em-<name>)")
```

In the command dispatch block, add:

```python
elif args.command == "em":
    sys.exit(commands.cmd_em(client, session_id, args.name))
```

`em` requires `session_id` (must be in a managed session). Do NOT add to `no_session_needed`.

### `src/cli/commands.py`

Add `cmd_em`:

```python
def cmd_em(
    client: SessionManagerClient,
    session_id: Optional[str],
    name_suffix: Optional[str],
) -> int:
    """
    EM pre-flight: set name, enable context monitoring for self and all children,
    register periodic remind for all children.

    Args:
        client: API client
        session_id: Caller's session ID (must be set)
        name_suffix: Optional suffix for friendly name (sets to em-<suffix>)

    Exit codes:
        0: Success (even if some child steps partially failed)
        1: Invalid name
        2: Session manager unavailable or no session ID
    """
    if not session_id:
        print("Error: sm em requires a managed session (CLAUDE_SESSION_MANAGER_ID not set)", file=sys.stderr)
        return 2

    results = []
    child_success = 0
    child_fail = 0

    # Step 1: Validate and set name
    friendly_name = f"em-{name_suffix}" if name_suffix else "em"
    valid, error = validate_friendly_name(friendly_name)
    if not valid:
        print(f"Error: {error}", file=sys.stderr)
        return 1

    success, unavailable = client.update_friendly_name(session_id, friendly_name)
    if unavailable:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2
    if success:
        results.append(f"  Name set: {friendly_name} ({session_id})")
    else:
        results.append(f"  Warning: Failed to set name to {friendly_name}")

    # Step 2: Enable self context-monitoring
    data, success, unavailable = client.set_context_monitor(
        session_id, enabled=True, requester_session_id=session_id,
        notify_session_id=session_id,
    )
    if unavailable:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2
    if success:
        results.append(f"  Context monitoring: enabled (notifications → self)")
    else:
        results.append(f"  Warning: Failed to enable self context monitoring")

    # Step 3: List and register children
    children_data = client.list_children(session_id)
    if children_data is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    children = children_data.get("children", [])

    if not children:
        results.append(f"  No existing children found.")
    else:
        child_lines = []
        for child in children:
            child_id = child["id"]
            child_name = child.get("friendly_name") or child.get("name") or child_id
            line_parts = []
            ok = True

            _, cm_ok, _ = client.set_context_monitor(
                child_id, enabled=True, requester_session_id=session_id,
                notify_session_id=session_id,
            )
            if cm_ok:
                line_parts.append("context monitoring enabled")
            else:
                line_parts.append("Warning: context monitoring failed")
                ok = False

            remind_result = client.register_remind(child_id, soft_threshold=180, hard_threshold=300)
            if remind_result is not None:
                line_parts.append("remind registered (soft=180s, hard=300s)")
            else:
                line_parts.append("remind registration failed")
                ok = False

            if ok:
                child_success += 1
            else:
                child_fail += 1
            child_lines.append(f"    {child_name} ({child_id}) → {', '.join(line_parts)}")

        results.append(f"  Children processed: {len(children)} ({child_success} succeeded, {child_fail} failed)")
        results.extend(child_lines)

    print("EM pre-flight complete:")
    for line in results:
        print(line)
    return 0
```

### `.agent-os/personas/em.md` — In-scope doc update

The issue requests updating `em.md` to make `sm em` the mandatory first step. The file exists in-repo at `.agent-os/personas/em.md` — this update is in scope for the same ticket. The engineer should update the **Pre-Flight** section to replace the manual steps with:

```bash
sm em <session-name>   # Set name, enable monitoring, register children — all in one
```

and remove the individual `sm name`, `sm context-monitor enable`, and child registration instructions.

## Test Plan

### Unit Tests (`tests/unit/test_em_cmd.py`)

1. **Name set with suffix** — `sm em session9` sets friendly name to `em-session9`
2. **Name set without suffix** — `sm em` (no arg) sets friendly name to `em`
3. **Name validation** — name_suffix containing invalid chars triggers validate_friendly_name error, exit 1 before any API calls
4. **Self context-monitoring enabled** — verify `set_context_monitor` called with `session_id` as both target and notify target
5. **Children auto-registered (context monitor)** — mock two children via `list_children`; verify `set_context_monitor` called once per child using `child["id"]`
6. **Children auto-registered (remind)** — verify `register_remind(child_id, 180, 300)` called once per child
7. **No children** — `list_children` returns `{"children": []}`; output includes "No existing children found"
8. **Partial child failure** — one child's context monitor fails (returns non-success); continues with others, reports `(1 succeeded, 1 failed)` count
9. **Child remind failure treated as warning** — `register_remind` returns `None`; treated as warning/continue, not exit 2
10. **No session ID** — exit 2 with error message
11. **Server unavailable on name set** — exit 2
12. **Server unavailable on children list** (`list_children` returns `None`) — exit 2
13. **Output format** — verify printed summary matches spec

### Unit Tests (`tests/unit/test_client.py` or similar — list_children fix)

14. **list_children returns None on non-success** — when `_request` returns `(data, False, False)` (API error, not unavailable), `list_children` returns `None`
15. **list_children returns None on unavailable** — when `_request` returns `(None, False, True)`, `list_children` returns `None`
16. **list_children returns data on success** — when `_request` returns `({"children": [...]}, True, False)`, returns the data dict

### Manual verification

```bash
# With no children
sm em session9
# Expected: name set, self context-monitoring enabled, "No existing children found"

# With existing children
sm children  # note child IDs
sm em session9
# Expected: all children registered for context monitoring + remind
sm context-monitor status  # verify all show up
```

## Ticket Classification

**Single ticket.** Changes: `src/cli/client.py` (~2 lines, list_children fix), `main.py` (~10 lines), `commands.py` (~65 lines), `.agent-os/personas/em.md` (Pre-Flight section update), plus tests. One agent can complete without compacting context.
