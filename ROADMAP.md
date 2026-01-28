# Claude Session Manager - Roadmap

## Recent Work (2026-01-28)

### Session 6: Tool Usage Logging for Security Audit

**What was completed:**

1. **Spec: Tool Usage Logging** (Issue #26)
   - Comprehensive spec at `specs/tool-usage-logging.md`
   - Architect review with 4 blocking items addressed
   - Hook payload format verified from official Claude Code docs
   - Database schema with destructive operation detection

2. **Phase 1 Implementation** (Issue #26)
   - `hooks/log_tool_use.sh` - Fire-and-forget hook script
     - Timeout protection (5s process + 3s curl)
     - Fallback to local file on API failure
     - Injects CLAUDE_SESSION_MANAGER_ID
   - `src/tool_logger.py` - ToolLogger class (230 lines)
     - SQLite at `~/.local/share/claude-sessions/tool_usage.db`
     - tool_use_id for Pre/Post correlation
     - project_name derived from cwd
     - 20+ destructive patterns (git push --force, rm -rf, DROP TABLE, etc.)
     - Sensitive file detection (.env, credentials, .ssh)
     - 8 indexes for efficient queries
   - `POST /hooks/tool-use` endpoint in server.py
   - Wired up in main.py

3. **Hook Configuration**
   - Added PreToolUse/PostToolUse hooks to `~/.claude/settings.json`
   - Also logs SubagentStart/SubagentStop events

**Tests Passed:**
- Database initialization
- Destructive pattern detection (6/6)
- Sensitive file detection (4/4)
- Async logging with Pre/Post correlation
- Server endpoint registered

**Files Changed:** 4 files, +311 lines

**Pending:** Phase 2 (sm tools CLI), Phase 3 (config options)

---

## Previous Work (2026-01-27)

### Session 5: CLI Enhancements & Agent Workflow Improvements

**What was completed:**

1. **sm new & sm attach Commands** (Issue: specs/sm-new-and-attach.md)
   - `sm new [working_dir]` - Create session and auto-attach (uses config for claude args)
   - `sm attach [session]` - Interactive menu or direct attach by ID/name
   - Uses `claude` config section for command, args, default model
   - Automatically sets `ENABLE_TOOL_SEARCH=false` (Claude Code bug workaround)

2. **sm clear Command** (Issue #21)
   - `sm clear <session> [prompt]` - Reset child agent context for task reuse
   - Sends ESC to interrupt + /clear to reset context
   - Optional new prompt after clearing
   - Parent-child ownership check

3. **sm output Command** (Issue #23)
   - `sm output <session> [--lines N]` - View recent tmux output
   - Resolves by ID or friendly name
   - Uses existing tmux_controller.capture_pane()
   - Default 30 lines, configurable

4. **sm name Enhancement** (Issue #24)
   - `sm name <name>` - Rename self (existing)
   - `sm name <session> <name>` - Rename child session (new)
   - Parent-child ownership check

5. **/follow Command for Telegram** (Issue #22)
   - `/follow <session>` - Associate existing session with Telegram topic
   - Creates forum topic for sessions created via sm spawn/new
   - Enables Telegram notifications for CLI-created sessions

6. **Bug Fixes**
   - #18: tmux session naming - Always use `claude-{session_id}` not friendly name
   - #19: sm spawn prompt submission - Increased init wait to 3s
   - #20: sm status excludes idle sessions - Added "idle" to status filter
   - sm attach excluding "error" status - Now shows all non-stopped sessions

7. **Workaround: Claude Code ToolSearch Bug**
   - Added `export ENABLE_TOOL_SEARCH=false` to all new sessions
   - References upstream issues #20329, #20468, #20982
   - Prevents infinite recursion/stack overflow in Claude Code

**Agent Workflow Demonstrated:**
- Spawned child agents to implement features
- Used `sm clear` to reuse agents for multiple issues
- Used `sm output` to monitor agent progress
- Used `sm name` to rename agents mid-session

**Files Changed:** 11 files, +600 lines

**Issues Filed:** #18, #19, #20, #21, #22, #23, #24

---

### Session 4: Multi-Agent Coordination Phase 1 MVP

**What was completed:**

1. **Session Model Extensions** (src/models.py)
   - Added parent-child relationship tracking (`parent_session_id`)
   - Added spawn metadata: `spawn_prompt`, `completion_status`, `completion_message`
   - Added lifecycle timestamps: `spawned_at`, `completed_at`
   - Added progress tracking: `tokens_used`, `tools_used`, `last_tool_call`

2. **sm spawn Command**
   - Create child Claude Code sessions from parent agents
   - Flags: `--name`, `--wait N`, `--model`, `--working-dir`, `--json`
   - Non-blocking: returns immediately, monitors in background
   - Model override support (opus/sonnet/haiku)
   - API endpoint: `POST /sessions/spawn`
   - SessionManager.spawn_child_session() implementation
   - TmuxController.create_session_with_command() for custom Claude invocation

3. **sm children Command**
   - List all child sessions of a parent
   - Flags: `--recursive` (include grandchildren), `--status` (filter), `--json`
   - Shows completion status and messages
   - API endpoint: `GET /sessions/{parent_session_id}/children`

4. **sm kill Command Enhancement**
   - **CRITICAL SECURITY**: Parent-child ownership check
   - Only allows killing own children (prevents agent interference)
   - API endpoint: `POST /sessions/{target_session_id}/kill`
   - Prevents "virtual bloodbath" scenario

5. **Background Monitoring for --wait** (src/child_monitor.py)
   - ChildMonitor service monitors child sessions
   - Detects: idle timeout (N seconds), completion patterns, session exit
   - Extracts completion summary from transcript/tmux output
   - Sends notification to parent's Claude input when complete
   - Non-blocking: parent doesn't burn tokens waiting

6. **Delivery Modes for sm send** (src/message_queue.py)
   - `--sequential` (default): Queues message, waits for idle (30s threshold)
   - `--important`: Sends immediately
   - `--urgent`: Interrupts immediately
   - MessageQueueManager monitors queues, delivers when idle
   - Updated SendInputRequest with delivery_mode parameter

**Implementation Details:**
- Spec: `specs/multi-agent-coordination.md` (Phase 1)
- 11 files changed: 8 modified, 2 new (child_monitor.py, message_queue.py)
- 1,211 lines added
- All basic tests pass
- Integration testing completed with multi-session environment

**Git Status:**
- Initial implementation: `0932fcd` - Implement Multi-Agent Coordination Phase 1 MVP
- Bug fix: `c9d99d7` - Fix --working-dir argument handling in sm spawn
- Branch: `main`
- Ready for Phase 2 (auto-detection, transcript parsing, advanced monitoring)

**Known Issues:**
- ✅ Issue #15 (sm send intermittent failures): Resolved - Python bytecode caching with editable install

**Testing:**
- ✅ All imports successful
- ✅ Session model has new fields
- ✅ CLI commands registered (spawn, children, kill)
- ✅ Multi-session validation (scout-correlation testing)
- ✅ Delivery modes working correctly

---

### Session 3: sm CLI Demo & Timeout Fix

**What was completed:**

1. **sm CLI Demo**
   - Demonstrated all 10 commands with live session manager
   - Verified: name, me, task, who, alone, status, lock, unlock, what, others
   - Confirmed exit codes work correctly (0=success, 1=error, 2=unavailable)
   - Verified lock file fallback system
   - Tested friendly name updates (propagate to tmux status bar)

2. **Timeout Fix** (src/cli/client.py)
   - **Problem:** `sm what` command timed out (2s timeout, but summary takes 60s)
   - **Solution:** Made `_request()` timeout configurable, use 65s for `get_summary()`
   - **Result:** `sm what` now works and returns AI-generated summaries
   - Commit: `fedccb7`

**Testing:**
- ✅ All commands work with live session manager
- ✅ `sm what a4af4272` returns summaries successfully
- ✅ Lock file system works correctly
- ✅ Exit codes conform to specification

**Git Status:**
- Latest commit: `fedccb7` - Fix timeout issue in sm what command
- Branch: `main`
- Working tree: clean
- Ready for next agent

---

### Session 2: sm CLI Implementation

**What was completed:**

1. **sm CLI Tool** (src/cli/)
   - Full implementation of multi-agent coordination CLI
   - 10 commands: name, me, who, what, others, alone, task, lock, unlock, status
   - HTTP client with 2s timeout for API calls
   - Pretty output formatting with relative times
   - Proper exit codes (0=success, 1=error, 2=unavailable)
   - Installed as `sm` system command via pyproject.toml

2. **Lock Manager** (src/lock_manager.py)
   - File-based coordination fallback when session manager unavailable
   - Lock file: `.claude/workspace.lock` in repo root
   - Stale lock detection (>30 minutes)
   - Operations: acquire, release, check

3. **Model & API Enhancements**
   - Added `current_task` and `git_remote_url` fields to Session model
   - Added `PATCH /sessions/{id}` endpoint to update friendly_name
   - Added `PUT /sessions/{id}/task` endpoint to register current task
   - Updated SessionResponse to include new fields

4. **Session Manager**
   - Git remote URL detection from working_dir
   - Automatic population of git_remote_url when creating sessions
   - Support for matching sessions across git worktrees

**Implementation Details:**
- 8 tasks completed: model changes, API endpoints, git detection, lock manager, CLI framework, commands, entry point, testing
- 965 lines added across 10 files (final squash merge count)
- All Python files compile successfully
- Package installs and works correctly
- Lock/unlock commands tested and working

**PR Status:**
- Branch: `feature/sm-cli` (deleted after merge)
- PR: https://github.com/rajeshgoli/claude-sessions/pull/1
- Status: ✅ **MERGED** (squash merge)
- Final Commit: `bdb97f3`
- Architect Review: Addressed feedback on redundant API calls and import placement

**Files Changed:**
- `src/models.py` - Added current_task, git_remote_url fields
- `src/server.py` - New endpoints + SessionResponse updates
- `src/session_manager.py` - Git detection method
- `src/lock_manager.py` - New file
- `src/cli/__init__.py` - New file
- `src/cli/client.py` - New file (3-tuple return for connection error handling)
- `src/cli/commands.py` - New file
- `src/cli/formatting.py` - New file
- `src/cli/main.py` - New file
- `pyproject.toml` - Added sm entry point

**Next Steps:**
- Test sm CLI with actual session manager running
- Consider adding shell completion for sm commands
- Document sm CLI usage for multi-agent workflows

---

### Session 1: Notifications & Summaries

**What was completed in this session:**

1. **Idle Notification Filtering** (server.py)
   - Added filter to skip `idle_prompt` notifications from Claude Code hooks
   - User was getting duplicate messages (Stop hook + idle_prompt notification)
   - Now only Stop hooks send notifications, idle_prompt hooks are logged but not forwarded
   - Location: `server.py:432-435`

2. **Summary API Endpoint** (server.py)
   - Added `GET /sessions/{session_id}/summary?lines=100` endpoint
   - Generates AI-powered summaries using Claude Haiku
   - Uses async subprocess execution (60s timeout)
   - Mirrors functionality of Telegram `/summary` command
   - Returns JSON: `{"session_id": "...", "summary": "..."}`
   - Location: `server.py:266-356`

3. **Documentation**
   - Created ROADMAP.md with future feature ideas
   - Created CODEBASE_OVERVIEW.md (earlier in session history)

**Current System State:**
- All sessions using `CLAUDE_SESSION_MANAGER_ID` env var for reliable identification
- Notification filtering via config.yaml (only permission_prompts enabled by default)
- Tmux status bars show friendly names when set via `/name`
- Session activity tracking works correctly with last_activity field
- No known bugs or issues

**Environment Variables Used:**
- `CLAUDE_SESSION_MANAGER_ID`: Set in tmux session, passed to Claude Code hooks for session identification

**Key Files Modified Recently:**
- `src/server.py`: Added summary endpoint, idle_prompt filtering
- `src/telegram_bot.py`: Session identification fixes, `/summary` command (earlier)
- `src/output_monitor.py`: Activity tracking fixes (earlier)
- `src/tmux_controller.py`: Status bar updates (earlier)
- `src/notifier.py`: Message truncation removal (earlier)
- `src/main.py`: Handler wiring (earlier)

**Testing Status:**
- Summary API endpoint: Code complete, not yet tested via HTTP
- Idle prompt filtering: Code complete, awaiting user confirmation
- All previous features: Tested and working

**Next Steps (if requested):**
- Test the new `/sessions/{id}/summary` endpoint via curl/HTTP
- Monitor if idle_prompt filtering resolves user's duplicate notification issue
- Consider implementing "User Input Detection from Tmux" if user requests it (see Backlog below)

**Git Status:**
- Latest commit: `e3a48f3` - Add /sessions/{id}/summary API endpoint
- Previous commit: `fb6e68a` - Add ROADMAP.md and filter idle_prompt notifications
- Branch: `main`
- Remote: `https://github.com/rajeshgoli/claude-sessions.git`

---

## Backlog (Not Prioritized)

### User Input Detection from Tmux
**Status:** Research Complete
**Complexity:** Medium
**Description:** Detect when user types messages directly in tmux terminal and forward them to Telegram.

**Technical Details:**
- Monitor tmux log files for lines containing `❯` prompt
- Strip ANSI escape codes and extract user input text
- Send notifications to Telegram when new input detected
- Avoid duplicates using state tracking
- Filter out Claude's thinking messages ("Tomfoolering…", etc.)

**Implementation Notes:**
- Add pattern detector in OutputMonitor (similar to permission/error detectors)
- Use regex pattern: `❯\s+(.+)` to capture input
- Estimated 50-100 lines of code
- Would provide bidirectional visibility: see both Claude responses AND user prompts in Telegram

**Use Case:** When switching between devices/locations, user can see full conversation history in Telegram including what they asked Claude, not just Claude's responses.

---

## Potential Future Enhancements

### Session Grouping/Tagging
- Add tags to sessions (e.g., "backend", "frontend", "research")
- Filter `/list` by tag
- Useful for managing many concurrent sessions

### Session Snapshots
- Save full transcript at specific points
- Resume from snapshot later
- Useful for experimentation with rollback capability

### Cost Tracking
- Track API usage per session
- Show cumulative costs in `/status`
- Budget alerts

### Multi-User Support
- Allow multiple Telegram users to share sessions
- Permission levels (view-only, interact, admin)
- Useful for team collaboration

### Session Templates
- Pre-configured session types (e.g., "Python Dev", "System Admin")
- Auto-load specific working directories and initial prompts
- Quick session creation with `/new template:python-dev`

### Web Dashboard
- Alternative to Telegram for session management
- Visual session browser
- Real-time output streaming
- Mobile-friendly interface

### Session Recording/Replay
- Record full session for later playback
- Export to video or text format
- Training/documentation purposes

### Smart Notifications
- ML-based notification filtering (detect truly important events)
- Quiet hours configuration
- Priority levels for different notification types

### Integration Hooks
- Webhook support for external services
- Slack/Discord integration in addition to Telegram
- Custom notification handlers

---

## Completed Features

- ✅ Basic session management (create, list, kill)
- ✅ Telegram bot interface with commands
- ✅ Real-time notifications via hooks
- ✅ Forum topic organization
- ✅ Friendly session naming
- ✅ Tmux status bar updates
- ✅ AI-powered session summaries (Telegram `/summary` + HTTP API)
- ✅ Session activity tracking
- ✅ Notification filtering (config-based + idle_prompt hook filtering)
- ✅ Message retrieval (`/message` command + `/last-message` API)
- ✅ Terminal attachment
- ✅ Session interrupt (Escape key)
- ✅ REST API for all core operations (sessions, input, output, summary)
- ✅ **sm CLI tool** for multi-agent coordination (PR #1)
  - 10 commands: name, me, who, what, others, alone, task, lock, unlock, status
  - Lock file fallback when session manager unavailable
  - Git worktree support via remote URL matching
  - Exit codes for scripting (0=success, 1=error, 2=unavailable)
- ✅ **Multi-Agent Coordination Phase 1** (Session 4)
  - sm spawn: Create child agent sessions with model override
  - sm children: List child sessions with recursive/status filters
  - sm kill: Terminate children with ownership check (security)
  - Background monitoring: --wait flag for completion notifications
  - Delivery modes: --sequential/--important/--urgent for sm send
  - Parent-child session hierarchy with full lifecycle tracking
  - MessageQueueManager for sequential delivery mode
  - ChildMonitor for automatic completion detection
- ✅ **CLI Enhancements** (Session 5)
  - sm new: Create session and auto-attach to tmux
  - sm attach: Interactive menu or direct attach by ID/name
  - sm clear: Reset child agent context for task reuse
  - sm output: View recent tmux output from any session
  - sm name: Rename child sessions (not just self)
  - /follow: Associate Telegram topics with existing sessions
  - ToolSearch bug workaround (ENABLE_TOOL_SEARCH=false)
- ✅ **Tool Usage Logging Phase 1** (Session 6)
  - Hook script with timeout protection + fallback file
  - ToolLogger class with SQLite database
  - Destructive operation detection (20+ patterns)
  - Sensitive file detection
  - POST /hooks/tool-use API endpoint
  - PreToolUse/PostToolUse/SubagentStart/SubagentStop hooks configured

---

## Notes

This roadmap is not prioritized. Features are added as interesting ideas emerge during development and usage. Implementation depends on user need and development time availability.
