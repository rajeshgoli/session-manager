# sm#285: sm watch — interactive agent dashboard

## Problem

When running agent swarms (EM + engineer + architect across repos), the only way to know what's happening is to tab through tmux panes. `sm all` gives a static snapshot but no interactivity, no role info, no reliable "is this agent actually working right now?" signal. The tab-switching tax grows linearly with agent count.

What's needed: a single pane that shows every agent, what role it's playing, whether it's truly active, and lets you jump into any session with Enter and back out with detach.

## Design

### Three pillars

This spec covers three things that must ship together — the dashboard is useless without reliable data, and the data is useless without a way to act on it.

1. **Role tracking** — know each agent's role (engineer, architect, scout, em, etc.)
2. **Reliable activity signal** — know if an agent is truly working vs. idle vs. stuck
3. **`sm watch` TUI** — interactive dashboard with attach/detach loop

---

## Pillar 1: Role tracking

### Data model

Add `role: Optional[str]` to Session. This is a short tag: `"engineer"`, `"architect"`, `"scout"`, `"em"`, `"reviewer"`, or `None` (untagged / user-initiated).

```python
# models.py — Session
role: Optional[str] = None  # Agent role tag (engineer, architect, em, etc.)
```

Persisted in `to_dict()` / `from_dict()`. Exposed in the session API response.

### How roles get set

**Priority order (highest wins):**

1. **`sm em`** — already sets `is_em = True`. Additionally set `role = "em"`. The `is_em` flag stays for backward compat (gates notify-on-stop logic).

2. **`sm dispatch <agent> <role>`** — dispatch already knows the role name. After dispatch completes, set `role` on the target session server-side. This is the cleanest path — dispatch already has the role string, just forward it.

3. **First-prompt heuristic (fallback)** — for sessions not dispatched via `sm dispatch` (e.g., user types directly into a tmux session after `sm clear`). On the server, when a session's first input after creation or clear contains `"As engineer"`, `"As architect"`, etc., extract the role.

   Detection logic (applied once per session lifecycle, reset on clear):
   ```python
   ROLE_KEYWORDS = ["engineer", "architect", "scout", "reviewer", "product", "director", "ux", "em"]

   def detect_role_from_prompt(text: str) -> Optional[str]:
       """Match 'As <role>' pattern at start of prompt."""
       lower = text[:200].lower()  # Only scan first 200 chars
       for kw in ROLE_KEYWORDS:
           if f"as {kw}" in lower:
               return kw
       return None
   ```

   Where to hook this: `session_manager.send_input()` — when a session has `role is None` and input is being sent, run detection. This catches both `sm send` and `sm dispatch` paths, but dispatch sets role explicitly so it wins.

4. **`sm role <role>`** — manual override CLI command for edge cases. Calls `PUT /sessions/{id}/role`. Simple, no magic.

### Reset on clear

`sm clear` must reset `role`, `completion_status`, and `agent_status_text` across all providers. The current clear paths are provider-specific and divergent:

- **Claude tmux:** CLI sends tmux keys + calls `POST /invalidate-cache` → `_invalidate_session_cache(arm_skip=True)`. SessionStart `context_reset` hook fires asynchronously later → resets `agent_status_text`/`at`.
- **Codex tmux:** CLI sends tmux keys + calls `POST /invalidate-cache` → `_invalidate_session_cache(arm_skip=True)`. No context_reset hook. Best-effort `clear_agent_status` call.
- **Codex-app:** CLI calls `POST /sessions/{id}/clear` → `SessionManager.clear_session()` + `_invalidate_session_cache(arm_skip=False)`.

**Fix: provider-independent reset in `_invalidate_session_cache()`.**

`_invalidate_session_cache()` (`server.py:264`) is the true common touchpoint — called by all three paths:
- tmux providers: via `POST /invalidate-cache` endpoint directly
- codex-app: via `POST /sessions/{id}/clear` endpoint which calls it after `clear_session()`

Add field resets there:

```python
def _invalidate_session_cache(app: FastAPI, session_id: str, arm_skip: bool = False) -> None:
    # ... existing cache invalidation ...

    # Provider-independent field reset on clear (#285)
    session = app.state.session_manager.get_session(session_id) if app.state.session_manager else None
    if session:
        session.role = None
        session.completion_status = None
        session.agent_status_text = None
        session.agent_status_at = None
        app.state.session_manager._save_state()
```

The Claude context_reset handler keeps its existing resets as belt-and-suspenders (it fires asynchronously after the hook, but the canonical reset already happened in `_invalidate_session_cache`).

The next input (dispatch or manual) re-tags.

### API

```
PUT /sessions/{id}/role   body: {"role": "engineer"}  — set role
DELETE /sessions/{id}/role                             — clear role
GET /sessions/{id}        — already returns session dict, now includes "role"
```

### Valid roles

Roles are **not hardcoded in the server**. Any string is accepted — except `"em"`. The dispatch templates YAML defines what roles exist for a project. The server stores whatever it's told. The dashboard displays whatever it finds. This keeps the server generic and roles configurable per-project.

**EM exclusion:** The `PUT /sessions/{id}/role` endpoint rejects `role="em"` with 400. EM setup has invariants beyond role tagging (single-EM enforcement, Telegram topic inheritance — `server.py:1049-1061`) that must go through the existing `sm em` / `PATCH /sessions/{id}` `is_em` path. `sm em` sets both `is_em = True` and `role = "em"` atomically through that path.

---

## Pillar 2: Reliable activity signal

### Problem

The current `status` field (`RUNNING` / `IDLE` / `STOPPED`) is coarse. `RUNNING` means "tmux session exists and we haven't timed out yet" — not "the agent is actively calling tools and producing output right now." An agent can sit at a permission prompt for 4 minutes and still show `RUNNING`.

### New field: `activity_state`

Add a computed field to the session API response (not persisted — derived from real-time signals):

```python
class ActivityState(str, Enum):
    WORKING = "working"              # Actively producing output / calling tools
    THINKING = "thinking"            # No recent output but hasn't timed out (< 30s)
    IDLE = "idle"                    # No activity for > idle_timeout
    WAITING_PERMISSION = "waiting_permission"  # Permission prompt detected
    WAITING_INPUT = "waiting_input"  # Waiting for user/agent input (post-completion, or sm send pending)
    STOPPED = "stopped"              # tmux session gone
```

### How activity_state is computed

Two signal sources feed the computation:

1. **Primary: message_queue `SessionDeliveryState`** — the Stop hook calls `mark_session_idle()` and PreToolUse calls `mark_session_active()` (`server.py:1757-1761`, `server.py:2586-2590`). This is the authoritative active/idle boundary because it fires on actual Claude turn boundaries, not log output heuristics.

2. **Supplementary: OutputMonitor `MonitorState`** — `is_output_flowing` distinguishes `working` (output being produced) from `thinking` (active turn, no output yet) within an active phase. `last_pattern` detects permission prompts.

The computation reads **only in-memory state** — no subprocess calls (no `tmux has-session`). Tmux existence is already validated by OutputMonitor's periodic 30s check, which updates `session.status` to STOPPED when tmux dies. We read that cached status here.

```python
def compute_activity_state(session: Session, queue_mgr, output_monitor) -> str:
    # Stopped: trust cached session.status (maintained by OutputMonitor 30s tmux check)
    if session.status == SessionStatus.STOPPED:
        return "stopped"

    # Provider-aware: codex-app has no tmux, no OutputMonitor
    if session.provider == "codex-app":
        return _compute_codex_app_activity(session, queue_mgr)

    # Primary signal: message_queue delivery state (from hooks)
    # Read delivery_states dict directly for tri-state:
    #   state exists + is_idle=True  → idle
    #   state exists + is_idle=False → active
    #   state is None                → no hook data (unregistered session, fallback to timestamp)
    # Note: queue_mgr.is_session_idle() returns bool (defaults missing to False),
    # which would misclassify unregistered sessions as active. Avoid it here.
    delivery_state = queue_mgr.delivery_states.get(session.id) if queue_mgr else None
    has_hook_data = delivery_state is not None
    is_idle = delivery_state.is_idle if has_hook_data else None

    # Supplementary: OutputMonitor for finer-grained state
    monitor_state = output_monitor.get_session_state(session.id) if output_monitor else None

    # Permission prompt takes priority (agent is blocked)
    if monitor_state and monitor_state.last_pattern == "permission":
        return "waiting_permission"

    # Completed task, waiting for next dispatch
    if session.completion_status is not None:
        return "waiting_input"

    # Hook says idle → idle
    if is_idle is True:
        return "idle"

    # Hook says active — refine with OutputMonitor
    if is_idle is False:
        if monitor_state and monitor_state.is_output_flowing:
            return "working"
        return "thinking"

    # Fallback: no hook data (unregistered session), use timestamp
    idle_seconds = (datetime.now() - session.last_activity).total_seconds()
    if idle_seconds < 30:
        return "thinking"

    return "idle"


def _compute_codex_app_activity(session: Session, queue_mgr) -> str:
    """Activity state for codex-app sessions (no tmux, no OutputMonitor)."""
    if session.completion_status is not None:
        return "waiting_input"

    # Same tri-state pattern: read delivery_states directly
    delivery_state = queue_mgr.delivery_states.get(session.id) if queue_mgr else None
    if delivery_state is not None:
        return "idle" if delivery_state.is_idle else "working"

    # Fallback: no hook data
    idle_seconds = (datetime.now() - session.last_activity).total_seconds()
    return "idle" if idle_seconds > 30 else "thinking"
```

### OutputMonitor changes

Add per-session state tracking that's queryable:

```python
@dataclass
class MonitorState:
    """Real-time activity state for a monitored session."""
    last_output_at: Optional[datetime] = None
    is_output_flowing: bool = False       # Output received in last 2 poll cycles
    last_pattern: Optional[str] = None    # "permission", "error", "completion", None
    output_bytes_last_10s: int = 0        # Rough throughput indicator
```

`OutputMonitor.get_session_state(session_id) -> Optional[MonitorState]` — returns current state, or None if not monitored.

The `is_output_flowing` flag is the key signal. Set to `True` when new output is detected in a poll cycle, set to `False` after 2 consecutive cycles with no output. This gives a ~2-second resolution for "is this agent actually doing something?"

### API

```
GET /sessions/{id}  — response now includes "activity_state": "working"|"thinking"|...
GET /sessions        — bulk: each session includes activity_state
```

Computed on every API call. Cost: reads in-memory `SessionDeliveryState.is_idle` (dict lookup) + in-memory `MonitorState` (dict lookup). No subprocess calls, no I/O. Not persisted.

---

## Pillar 3: `sm watch` TUI

### Interaction model

```
sm watch
  ↓ (j/k or arrows to navigate, Enter to attach)
tmux attach -t claude-xxx   ← blocks, user is now in the session
  ↓ (Ctrl-b d to detach from tmux)
sm watch refreshes           ← back to dashboard
```

### Display layout

```
╔══════════════════════════════════════════════════════════════════╗
║  sm watch                                    3 agents · 2 repos ║
╠══════════════════════════════════════════════════════════════════╣
║                                                                  ║
║  session-manager/                                                ║
║  ├─ em-main [a1b2c3d4] em          idle         2min             ║
║  │    "waiting on engineer + architect"                          ║
║  ├─▸eng-287 [e5f6a7b8] engineer    working ●    12s             ║
║  │    "implementing fix for #287"                                ║
║  └─ arch-review [c9d0e1f2] architect idle       5min             ║
║       "standing by for review"                                   ║
║                                                                  ║
║  another-repo/                                                   ║
║  └─▸debug [f3a4b5c6]              working ●    3s              ║
║       "investigating flaky test in test_auth.py"                 ║
║                                                                  ║
╠══════════════════════════════════════════════════════════════════╣
║  ↑↓/jk navigate · Enter attach · q quit · s send · k kill       ║
╚══════════════════════════════════════════════════════════════════╝
```

**Layout rules:**

- Sessions grouped by `working_dir` (collapsed to basename or relative path)
- Within each group, parent first, then children indented with tree lines (`├─`, `└─`)
- Unparented sessions shown as root entries in their group
- Selected row highlighted (inverted colors or `▸` marker)
- `●` indicator for `working` state, animated spinner for `thinking`
- Role tag shown after the ID bracket (dimmed if None)
- Status text (`agent_status_text`) shown indented below each agent
- Time column: seconds since last activity for working/thinking, minutes for idle

### Refresh

- Poll `/sessions` every 2 seconds
- Full redraw on each poll (simple, no diff)
- On attach return, immediate refresh

### Key bindings

| Key | Action |
|-----|--------|
| `j` / `↓` | Move selection down |
| `k` / `↑` | Move selection up |
| `Enter` | Attach to selected session's tmux (blocking). **codex-app sessions: disabled** — shows inline flash message "no terminal (use s to send)" since codex-app has no tmux session. |
| `q` | Quit sm watch |
| `s` | Prompt for message, `sm send` to selected session (works for all providers) |
| `K` | Kill selected session (with confirmation, children only) |
| `r` | Force refresh |
| `/` | Filter by name/role |

### Provider-specific behavior

| Provider | Enter (attach) | s (send) | Display |
|----------|---------------|----------|---------|
| `claude` | tmux attach | sm send | Full activity states |
| `codex` | tmux attach | sm send | Full activity states |
| `codex-app` | Disabled (flash msg) | sm send | Hook-based activity only (no OutputMonitor) |

### Implementation: curses vs. textual

**Use curses.** Rationale:
- Zero new dependencies (stdlib)
- The layout is simple — it's a list with tree structure, not a form-heavy app
- `textual` is 5MB+ and overkill for this
- curses handles the raw terminal mode needed for key capture alongside `tmux attach` subprocess handoff cleanly

### Attach flow (critical detail)

```python
def attach_to_session(tmux_session_name: str):
    """Hand terminal to tmux, return when user detaches."""
    # Suspend curses, restore terminal
    curses.endwin()

    # Blocking — user is now in the tmux session
    subprocess.run(["tmux", "attach-session", "-t", tmux_session_name])

    # User detached (Ctrl-b d) — reclaim terminal
    # curses re-initializes on next refresh cycle
```

After `curses.endwin()`, the terminal is restored to normal mode. `tmux attach` takes over. When the user detaches, control returns, and the curses TUI reinitializes. This is the standard pattern for curses apps that shell out.

### Send message flow

When user presses `s`:
1. Pause curses refresh
2. Show input prompt at bottom of screen (curses `getstr` or simple input line)
3. On Enter, call `sm send <selected_session_id> "<message>"` via API
4. Resume refresh

### CLI registration

```bash
sm watch              # Launch dashboard (all sessions)
sm watch --repo .     # Filter to current repo only
sm watch --role engineer  # Filter by role
```

Arguments:
- `--repo <path>` — filter sessions by working_dir (default: show all)
- `--role <role>` — filter by role tag
- `--interval <secs>` — refresh interval (default: 2)

---

## Prerequisites

**Prerequisite fix: wire `agent_status_text` into session API responses.**

`SessionResponse` already declares `agent_status_text` and `agent_status_at` fields (`server.py:93-94`), but the list and get endpoints don't populate them (`server.py:960-975`, `server.py:1007-1022`). The dashboard depends on this. Fix: add `agent_status_text=s.agent_status_text` and `agent_status_at=s.agent_status_at.isoformat() if s.agent_status_at else None` to both response construction sites. This is a small, standalone fix that should land first.

---

## Implementation approach

### Files to modify

**`src/models.py`**
- Add `role: Optional[str] = None` to Session
- Add to `to_dict()` / `from_dict()`
- Add `ActivityState` enum
- Add `MonitorState` dataclass

**`src/session_manager.py`**
- Add `set_role(session_id, role)` / `clear_role(session_id)` methods
- Add `compute_activity_state()` logic (reads `delivery_states` dict + OutputMonitor state, no subprocess calls)
- No changes for clear reset (handled in server.py `_invalidate_session_cache`)
- In `send_input()`: call role detection if `role is None`

**`src/output_monitor.py`**
- Add `MonitorState` per-session tracking
- Add `get_session_state(session_id)` method
- Track `is_output_flowing` flag (True if output in last 2 poll cycles)
- Track `last_pattern` for permission/error/completion

**`src/server.py` (FastAPI routes)**
- Add `PUT /sessions/{id}/role` endpoint (rejects `"em"` with 400)
- Add `DELETE /sessions/{id}/role` endpoint
- Include `activity_state` in session GET/list responses (computed via `compute_activity_state`)
- Include `role` in session GET/list responses
- Wire `agent_status_text`/`agent_status_at` into GET/list responses (prerequisite fix)
- Add role/completion_status/agent_status reset to `_invalidate_session_cache()` (canonical cross-provider clear reset)
- Keep `context_reset` handler resets as belt-and-suspenders
- In `sm em` handler: set `role = "em"` alongside `is_em = True`

**`src/cli/commands.py`**
- Add `cmd_watch()` — curses TUI main loop (delegates to watch_tui.py)
- Add `cmd_role()` — manual role set/clear
- Modify `cmd_dispatch()` — set role on target after dispatch

**`src/cli/main.py`**
- Register `watch` subcommand with args (`--repo`, `--role`, `--interval`)
- Register `role` subcommand

**`src/cli/client.py`**
- Add `set_role()`, `clear_role()` API client methods
- Sessions response now includes `activity_state` and `role` — no client change needed (already passes through dicts)

**`src/cli/watch_tui.py`** (new file)
- Curses-based TUI: layout, key handling, refresh loop, attach handoff
- Kept separate from commands.py to isolate curses concerns

### Files NOT modified

- `config.yaml` — no new config needed (refresh interval is a CLI arg)
- `dispatch_templates.yaml` — roles already defined there, no change
- `telegram_bot.py` — no change (future: could push dashboard state to Telegram, but out of scope)

---

## Test plan

### Unit tests

**`tests/test_role_tracking.py`**
1. `test_role_set_via_dispatch` — dispatch engineer sets role="engineer" on target
2. `test_role_set_via_sm_em` — sm em sets role="em"
3. `test_role_set_via_manual_command` — sm role engineer sets role
4. `test_role_detection_from_prompt` — "As engineer, ..." detected on send_input
5. `test_role_detection_case_insensitive` — "as Architect" → "architect"
6. `test_role_reset_on_clear_claude` — invalidate-cache resets role to None (claude tmux path)
6b. `test_role_reset_on_clear_codex` — invalidate-cache resets role to None (codex tmux path)
6c. `test_role_reset_on_clear_codex_app` — /clear endpoint resets role to None (codex-app path, also calls _invalidate_session_cache)
7. `test_role_persisted` — save/load round-trip preserves role
8. `test_role_any_string_accepted` — server accepts arbitrary role strings
9. `test_em_role_via_sm_em_sets_both` — sm em sets role="em" and is_em=True atomically
10. `test_role_endpoint_rejects_em` — PUT /role with "em" returns 400
11. `test_completion_status_reset_on_clear_claude` — invalidate-cache clears completion_status (claude tmux path)
12. `test_completion_status_reset_on_clear_codex` — invalidate-cache clears completion_status (codex tmux path)
13. `test_completion_status_reset_on_clear_codex_app` — /clear endpoint clears completion_status (codex-app path)

**`tests/test_activity_state.py`**
1. `test_activity_working_when_output_flowing` — is_idle=False + is_output_flowing=True → "working"
2. `test_activity_thinking_active_no_output` — is_idle=False + is_output_flowing=False → "thinking"
3. `test_activity_idle_from_hook` — is_idle=True → "idle"
4. `test_activity_waiting_permission` — permission pattern detected → "waiting_permission" (overrides active)
5. `test_activity_waiting_input_after_completion` — completion_status set → "waiting_input"
6. `test_activity_stopped_cached_status` — session.status=STOPPED → "stopped" (no tmux subprocess)
7. `test_activity_codex_app_working` — codex-app provider + is_idle=False → "working"
8. `test_activity_codex_app_idle` — codex-app provider + is_idle=True → "idle"
9. `test_activity_fallback_no_hook_data` — no delivery_state entry → falls back to timestamp (not misclassified as active)
10. `test_monitor_state_is_output_flowing_timing` — flag clears after 2 empty cycles

**`tests/test_watch_tui.py`**
1. `test_session_grouping_by_repo` — sessions grouped by working_dir
2. `test_tree_structure_parent_child` — parent shown first, children indented
3. `test_unparented_sessions_shown_as_root` — no parent → root level in group
4. `test_selection_navigation` — j/k moves selection up/down
5. `test_filter_by_role` — --role flag filters sessions
6. `test_filter_by_repo` — --repo flag filters sessions
7. `test_enter_disabled_for_codex_app` — Enter on codex-app row shows flash message, does not attempt tmux attach
8. `test_send_works_for_codex_app` — s key works for codex-app sessions

### Manual verification

```bash
# 1. Role tracking via dispatch (actual CLI syntax)
sm spawn claude "stand by" --name test-eng
sm dispatch test-eng --role engineer --issue 100 --spec docs/working/test.md
# Verify: curl localhost:8420/sessions/<id> shows "role": "engineer"

# 2. Role detection from prompt
sm spawn claude "stand by" --name test-arch
sm send test-arch "As architect, review this code"
# Verify: curl localhost:8420/sessions/<id> shows "role": "architect"

# 3. Role endpoint rejects "em"
curl -X PUT localhost:8420/sessions/<id>/role -d '{"role": "em"}'
# Verify: 400 response

# 4. Activity state
sm spawn claude "write a long file with 1000 lines" --name active-test
# While running: curl localhost:8420/sessions/<id> → "activity_state": "working"
# After Stop hook fires: → "idle" or "waiting_input"

# 5. Clear resets role + completion_status
sm role engineer  # (from within test-eng session, or via curl)
# Then: sm clear test-eng
# Verify: curl shows "role": null, "completion_status": null

# 6. Watch TUI
sm watch
# Verify: tree layout, grouping, roles shown
# Select agent, press Enter → attached to tmux
# Ctrl-b d → back to watch
# Press q → exit
```

---

## Ticket classification

**Epic.** Four tickets (one prerequisite + three pillars):

0. **Prerequisite: wire agent_status_text + fix cross-provider clear reset** — populate `agent_status_text`/`agent_status_at` in session API responses, add role/completion_status/agent_status reset to `_invalidate_session_cache()` (covers all providers). ~30 lines, single ticket.
1. **Role tracking** (model + API + dispatch integration + detection + EM exclusion) — ~200 lines, single ticket. Depends on 0.
2. **Activity state** (MonitorState + compute logic using message_queue + OutputMonitor + codex-app path) — ~200 lines, single ticket. Depends on 0.
3. **Watch TUI** (curses app + CLI registration) — ~400 lines, single ticket. Depends on 1 and 2.

Sub-tickets to be filed after spec approval.
