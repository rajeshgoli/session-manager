# Spec: `sm new` and `sm attach` Commands

**Status:** Draft
**Author:** Session Manager
**Created:** 2026-01-27

## Getting Started (For Fresh Agent)

This spec assumes familiarity with the codebase. Before implementing:

1. **Read these files to understand the architecture:**
   - `src/cli/commands.py` - See existing command implementations (especially `cmd_spawn`, `cmd_send`)
   - `src/cli/client.py` - See how API client methods work (especially `spawn_child`, `send_input`)
   - `src/cli/formatting.py` - See helper functions for output formatting
   - `src/server.py` - See existing API endpoints (especially `/sessions/spawn`)
   - `src/session_manager.py` - Understand `create_session()` method

2. **Key existing patterns to follow:**
   - Commands use `resolve_session_id()` to handle both IDs and friendly names
   - Client methods return `None` when session manager is unavailable
   - Exit codes: 0=success, 1=error, 2=unavailable
   - Use `format_session_line()` for consistent session display

3. **Reference implementations:**
   - `cmd_spawn()` - Similar: creates sessions, uses config, spawns child
   - `cmd_send()` - Similar: resolves session IDs, handles unavailable state
   - `/sessions/spawn` endpoint - Similar: creates sessions, starts monitoring

4. **Project structure:**
   ```
   src/
   ├── cli/
   │   ├── main.py       # Argparse setup and command routing
   │   ├── commands.py   # Command implementations (add cmd_new, cmd_attach here)
   │   ├── client.py     # API client (add create_session method here)
   │   └── formatting.py # Display helpers (update format_session_line here)
   ├── server.py         # FastAPI endpoints (add /sessions/create here)
   ├── session_manager.py # Session lifecycle management
   └── tmux_controller.py # Tmux operations
   ```

5. **How to test:**
   - Start session manager: `python -m src.main`
   - Run CLI commands: `sm new`, `sm attach`
   - Config file: `config.yaml` (has `claude.command`, `claude.args`, `claude.default_model`)

6. **Missing imports:**
   - All necessary imports are shown in code snippets
   - Follow existing patterns in each file for import organization
   - Key imports: `subprocess`, `Path`, `httpx`, `Optional`, `sys`

---

## Overview

Add native commands for creating and attaching to Claude Code sessions via the sm CLI.

## Motivation

Currently:
- Users need to manually create sessions via Telegram `/new` or manually start tmux sessions
- The `attach-session` script exists as a separate bash script
- No way to create and immediately attach to a session from CLI

Goals:
- Streamline session creation workflow from CLI
- Replace the separate bash script with native sm command
- Integrate with existing `sm all` listing for seamless session discovery

## Design

### Command: `sm new`

Create a new Claude Code session and attach to it.

**Usage:**
```bash
sm new [working_dir]
```

**Arguments:**
- `working_dir` (optional): Directory to start Claude in
  - If omitted, uses current working directory (`$PWD`)
  - Supports shell expansion (`~`, relative paths)

**Behavior:**
1. Resolve working directory:
   - If provided: expand and resolve to absolute path
   - If omitted: use current working directory
2. Call session manager API to create new session
   - POST `/sessions/create` with `working_dir`
   - Session manager creates tmux session with Claude Code running
3. Wait for session creation to complete
4. Automatically attach to the tmux session: `tmux attach -t <session_name>`

**Exit codes:**
- 0: Successfully created and attached
- 1: Failed to create session
- 2: Session manager unavailable

**Example:**
```bash
# Create session in current directory
sm new

# Create session in specific directory
sm new ~/projects/my-app

# Create session with relative path
sm new ../other-project
```

**Notes:**
- After attach, user is inside the Claude Code session
- Detach with Ctrl+B, D (tmux detach)
- Session persists after detach (can re-attach later)
- Uses `claude` section from config.yaml for command, args, and default model
- Automatically sets `ENABLE_TOOL_SEARCH=false` (workaround for Claude Code bug)

---

### Command: `sm attach`

Attach to an existing Claude Code session.

**Usage:**
```bash
sm attach [session_id_or_name]
```

**Arguments:**
- `session_id_or_name` (optional): Session ID or friendly name
  - If omitted, shows interactive menu

**Behavior:**

#### Case 1: Specific session provided
```bash
sm attach abc123de
sm attach office-automate
```

1. Resolve identifier (ID or friendly name) using `resolve_session_id()`
2. Get session details from API
3. Extract tmux session name
4. Attach: `tmux attach -t <tmux_session>`

**Exit codes:**
- 0: Successfully attached
- 1: Session not found
- 2: Session manager unavailable

#### Case 2: No argument (interactive menu)
```bash
sm attach
```

1. Call `sm all` to get all sessions
2. Filter to only running/idle sessions (skip stopped/error)
3. If no sessions: print "No sessions available" and exit 1
4. If one session: attach directly (no menu)
5. If multiple sessions: show interactive menu:
   ```
   Available sessions:
   1. office-automate [abc123de] - idle - 2m ago
   2. architect1 [def456gh] - running - 5m ago
   3. sessionmgr [hij789kl] - running - 10m ago

   Select session (number or name): _
   ```
6. Accept input:
   - Number (1-N): select by menu index
   - Session ID or friendly name: resolve and attach
   - Ctrl+C: cancel
7. Attach to selected session

**Menu format:**
```
<index>. <friendly_name or name> [<session_id>] - <status> - <last_activity>
```

**Exit codes:**
- 0: Successfully attached
- 1: No sessions available or invalid selection
- 2: Session manager unavailable

**Example interactions:**
```bash
# Interactive menu
$ sm attach
Available sessions:
1. office-automate [fc7d7dbc] - idle - 2m ago
2. sessionmgr [a4af4272] - running - 5m ago

Select session (number or name): 1
# Attaches to office-automate

# Direct attach by ID
$ sm attach fc7d7dbc
# Attaches immediately

# Direct attach by friendly name
$ sm attach office-automate
# Attaches immediately
```

**Notes:**
- Integrates with `sm all` output format
- Shows friendly names when available
- Falls back to session name if no friendly name
- After attach, user is inside the Claude Code session
- Detach with Ctrl+B, D (tmux detach)

---

## API Requirements

### Existing endpoints used:
- `GET /sessions` - List all sessions (for menu)
- `GET /sessions/{id}` - Get session details (for direct attach)

### New endpoint needed:
- `POST /sessions/create` - Create new session
  - Request body: `{"working_dir": "/path/to/dir"}`
  - Response: Session object with `id`, `name`, `tmux_session`, etc.
  - Uses `claude` config section for command, args, and default model
  - Same behavior as Telegram `/new` but without Telegram association

---

## Implementation Guide

### Phase 1: Server-side API endpoint

**File:** `src/server.py`

Add new endpoint:

```python
@router.post("/sessions/create")
async def create_session_endpoint(
    request: Request,
    working_dir: str,
) -> dict:
    """
    Create a new Claude Code session.

    Args:
        working_dir: Absolute path to working directory

    Returns:
        Session object dict
    """
    session_manager = request.app.state.session_manager
    output_monitor = request.app.state.output_monitor

    # Create session using config settings
    session = session_manager.create_session(
        working_dir=working_dir,
        telegram_chat_id=None,  # No Telegram association
    )

    if not session:
        raise HTTPException(status_code=500, detail="Failed to create session")

    # Start monitoring (same as Telegram /new does)
    await output_monitor.start_monitoring(session)

    return session.to_dict()
```

**Note:** Uses existing `session_manager.create_session()` which already:
- Reads `claude` config for command/args
- Creates tmux session with Claude running
- Uses `tmux_controller.create_session()` (which sets ENABLE_TOOL_SEARCH=false)

---

### Phase 2: CLI client method

**File:** `src/cli/client.py`

Add method to `SessionManagerClient` class:

```python
def create_session(self, working_dir: str) -> Optional[dict]:
    """
    Create a new Claude Code session.

    Args:
        working_dir: Working directory path

    Returns:
        Session dict or None if unavailable
    """
    try:
        response = self.session.post(
            f"{self.base_url}/sessions/create",
            params={"working_dir": working_dir},
            timeout=10.0,
        )
        response.raise_for_status()
        return response.json()
    except httpx.ConnectError:
        return None
    except httpx.HTTPStatusError as e:
        logger.error(f"HTTP error creating session: {e}")
        return None
    except Exception as e:
        logger.error(f"Failed to create session: {e}")
        return None
```

---

### Phase 3: CLI command implementations

**File:** `src/cli/commands.py`

Add two new command functions:

```python
def cmd_new(client: SessionManagerClient, working_dir: Optional[str] = None) -> int:
    """
    Create a new Claude Code session and attach to it.

    Args:
        client: API client
        working_dir: Working directory (optional, defaults to $PWD)

    Exit codes:
        0: Successfully created and attached
        1: Failed to create session
        2: Session manager unavailable
    """
    import os
    import subprocess
    from pathlib import Path

    # Resolve working directory
    if working_dir is None:
        working_dir = os.getcwd()

    # Expand and resolve path
    try:
        path = Path(working_dir).expanduser().resolve()
        if not path.exists():
            print(f"Error: Directory does not exist: {working_dir}", file=sys.stderr)
            return 1
        if not path.is_dir():
            print(f"Error: Not a directory: {working_dir}", file=sys.stderr)
            return 1
        working_dir = str(path)
    except Exception as e:
        print(f"Error: Invalid path: {e}", file=sys.stderr)
        return 1

    # Create session via API
    print(f"Creating session in {working_dir}...")
    session = client.create_session(working_dir)

    if session is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    # Extract tmux session name
    tmux_session = session.get("tmux_session")
    session_id = session.get("id")

    if not tmux_session:
        print("Error: Failed to get tmux session name", file=sys.stderr)
        return 1

    print(f"Session created: {session_id}")
    print(f"Attaching to {tmux_session}...")

    # Wait briefly for Claude to initialize
    import time
    time.sleep(1)

    # Attach to tmux session (blocks until detach)
    try:
        subprocess.run(["tmux", "attach", "-t", tmux_session], check=True)
        return 0
    except subprocess.CalledProcessError:
        print(f"Error: Failed to attach to tmux session {tmux_session}", file=sys.stderr)
        return 1
    except FileNotFoundError:
        print("Error: tmux not found. Is tmux installed?", file=sys.stderr)
        return 1


def cmd_attach(client: SessionManagerClient, identifier: Optional[str] = None) -> int:
    """
    Attach to an existing Claude Code session.

    Args:
        client: API client
        identifier: Session ID or friendly name (optional, shows menu if omitted)

    Exit codes:
        0: Successfully attached
        1: No sessions available or invalid selection
        2: Session manager unavailable
    """
    import subprocess

    # Case 1: Direct attach with identifier
    if identifier:
        # Resolve identifier to session
        session_id, session = resolve_session_id(client, identifier)

        if session_id is None:
            sessions = client.list_sessions()
            if sessions is None:
                print("Error: Session manager unavailable", file=sys.stderr)
                return 2
            else:
                print(f"Error: Session '{identifier}' not found", file=sys.stderr)
                return 1

        # Extract tmux session name
        tmux_session = session.get("tmux_session")
        if not tmux_session:
            print("Error: Session has no tmux session", file=sys.stderr)
            return 1

        # Attach
        try:
            subprocess.run(["tmux", "attach", "-t", tmux_session], check=True)
            return 0
        except subprocess.CalledProcessError:
            print(f"Error: Failed to attach to tmux session {tmux_session}", file=sys.stderr)
            return 1
        except FileNotFoundError:
            print("Error: tmux not found. Is tmux installed?", file=sys.stderr)
            return 1

    # Case 2: Interactive menu
    sessions = client.list_sessions()

    if sessions is None:
        print("Error: Session manager unavailable", file=sys.stderr)
        return 2

    # Filter to running/idle sessions only
    active_sessions = [
        s for s in sessions
        if s.get("status") not in ["stopped", "error"]
    ]

    if not active_sessions:
        print("No sessions available")
        return 1

    # Single session - attach directly
    if len(active_sessions) == 1:
        session = active_sessions[0]
        tmux_session = session.get("tmux_session")

        try:
            subprocess.run(["tmux", "attach", "-t", tmux_session], check=True)
            return 0
        except subprocess.CalledProcessError:
            print(f"Error: Failed to attach to tmux session {tmux_session}", file=sys.stderr)
            return 1

    # Multiple sessions - show menu
    print("Available sessions:")
    for i, session in enumerate(active_sessions, start=1):
        print(format_session_line(session, index=i))

    print()

    # Get user selection
    try:
        selection = input("Select session (number or name): ").strip()
    except KeyboardInterrupt:
        print("\nCancelled")
        return 1
    except EOFError:
        print("\nCancelled")
        return 1

    if not selection:
        print("No selection made")
        return 1

    # Try as number first
    if selection.isdigit():
        index = int(selection)
        if index < 1 or index > len(active_sessions):
            print(f"Error: Invalid selection. Must be between 1 and {len(active_sessions)}", file=sys.stderr)
            return 1
        session = active_sessions[index - 1]
    else:
        # Try as session ID or friendly name
        session_id, session = resolve_session_id(client, selection)
        if session_id is None:
            print(f"Error: Session '{selection}' not found", file=sys.stderr)
            return 1

    # Attach to selected session
    tmux_session = session.get("tmux_session")
    if not tmux_session:
        print("Error: Session has no tmux session", file=sys.stderr)
        return 1

    try:
        subprocess.run(["tmux", "attach", "-t", tmux_session], check=True)
        return 0
    except subprocess.CalledProcessError:
        print(f"Error: Failed to attach to tmux session {tmux_session}", file=sys.stderr)
        return 1
    except FileNotFoundError:
        print("Error: tmux not found. Is tmux installed?", file=sys.stderr)
        return 1
```

---

### Phase 4: Update formatting helper

**File:** `src/cli/formatting.py`

Update `format_session_line()` to accept optional `index` parameter:

```python
def format_session_line(session: dict, show_working_dir: bool = False, index: Optional[int] = None) -> str:
    """
    Format a session as a single line.

    Args:
        session: Session dict
        show_working_dir: Include working directory
        index: Optional menu index number

    Returns:
        Formatted string
    """
    session_id = session.get("id", "unknown")
    friendly_name = session.get("friendly_name")
    name = session.get("name", session_id)
    status = session.get("status", "unknown")
    last_activity = session.get("last_activity")

    # Display name: prefer friendly_name, fall back to name
    display_name = friendly_name or name

    # Format last activity
    if last_activity:
        time_str = format_relative_time(last_activity)
    else:
        time_str = "unknown"

    # Build line
    parts = []

    if index is not None:
        parts.append(f"{index}.")

    parts.append(display_name)
    parts.append(f"[{session_id}]")
    parts.append(f"- {status}")
    parts.append(f"- {time_str}")

    if show_working_dir:
        working_dir = session.get("working_dir", "")
        if working_dir:
            parts.append(f"({working_dir})")

    return " ".join(parts)
```

---

### Phase 5: Wire up argparse

**File:** `src/cli/main.py`

Add two new subcommands to the argparse setup:

```python
# Add after existing command parsers

# sm new
parser_new = subparsers.add_parser(
    "new",
    help="Create a new Claude Code session and attach to it"
)
parser_new.add_argument(
    "working_dir",
    nargs="?",
    help="Working directory (defaults to current directory)"
)

# sm attach
parser_attach = subparsers.add_parser(
    "attach",
    help="Attach to an existing session"
)
parser_attach.add_argument(
    "session",
    nargs="?",
    help="Session ID or friendly name (shows menu if omitted)"
)
```

Then in the command dispatch section:

```python
# Add to command routing
if args.command == "new":
    return cmd_new(client, args.working_dir)
elif args.command == "attach":
    return cmd_attach(client, args.session)
# ... existing commands
```

---

### Phase 6: Testing checklist

1. **sm new:**
   - `sm new` - creates in current directory
   - `sm new ~/Desktop/test` - creates in specific directory
   - `sm new ../relative` - handles relative paths
   - `sm new ~/nonexistent` - errors gracefully
   - After creation, tmux attaches successfully
   - Session appears in `sm all`
   - Session uses `--dangerously-skip-permissions` from config
   - Session has `ENABLE_TOOL_SEARCH=false` set

2. **sm attach with argument:**
   - `sm attach <session_id>` - attaches by ID
   - `sm attach <friendly_name>` - attaches by name
   - `sm attach nonexistent` - errors gracefully
   - Can detach with Ctrl+B D and re-attach

3. **sm attach without argument:**
   - Shows menu when multiple sessions
   - Numeric selection (1, 2, 3) works
   - Name selection works
   - Ctrl+C cancels gracefully
   - No sessions shows appropriate message
   - Single session attaches directly (no menu)

4. **Error cases:**
   - Session manager not running - shows "unavailable" error
   - tmux not installed - shows helpful error
   - Session killed between list and attach - shows error

---

## Implementation Notes

1. **Path expansion:**
   - Use `Path(working_dir).expanduser().resolve()` to handle `~` and relative paths
   - Validate directory exists before API call
   - Use `is_dir()` to ensure it's actually a directory

2. **Interactive menu:**
   - Use Python's `input()` for selection
   - Handle both `KeyboardInterrupt` (Ctrl+C) and `EOFError` (Ctrl+D)
   - Try numeric selection first with `isdigit()` check
   - Fall back to name resolution for text input
   - Validate numeric range before indexing

3. **Tmux integration:**
   - Use `subprocess.run(["tmux", "attach", "-t", session_name], check=True)`
   - No need to capture output - tmux takes over the terminal
   - Handle `CalledProcessError` if attach fails
   - Handle `FileNotFoundError` if tmux not installed
   - Add 1 second sleep after creation for Claude initialization

4. **Error handling:**
   - Check if session manager is running (client returns None)
   - Validate session exists before attaching
   - Show friendly error messages with context
   - Use appropriate exit codes (0=success, 1=error, 2=unavailable)

5. **Integration with existing code:**
   - Use `resolve_session_id()` helper for name resolution
   - Use `SessionManagerClient.list_sessions()` for menu
   - Reuse `format_session_line()` for consistent formatting (add `index` param)
   - Use `format_relative_time()` for timestamps

---

## Future Enhancements (out of scope)

- `sm new --name <friendly_name>` - Set friendly name during creation
- `sm attach --read-only` - Attach in read-only mode (tmux attach -r)
- Bash/Zsh completion for `sm attach` (autocomplete session names)
- `sm new --model <opus|sonnet|haiku>` - Override config default model

---

## Testing

1. Create session in current dir: `sm new`
2. Create session with explicit path: `sm new ~/Desktop/test-project`
3. Attach to specific session: `sm attach <session_id>`
4. Attach to session by name: `sm attach <friendly_name>`
5. Interactive menu: `sm attach` (no args)
6. Error cases:
   - Session manager not running
   - Invalid session ID/name
   - Working directory doesn't exist
   - No sessions available for menu

---

## Deprecation

Once `sm attach` is implemented, the standalone `attach-session` bash script can be deprecated. Users should be migrated to `sm attach`.

Consider:
- Adding deprecation warning to `attach-session` script
- Updating documentation to reference `sm attach`
- Keep `attach-session` for backward compatibility short-term
