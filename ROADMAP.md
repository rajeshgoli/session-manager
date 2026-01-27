# Claude Session Manager - Roadmap

## Recent Work (2026-01-27)

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

---

## Notes

This roadmap is not prioritized. Features are added as interesting ideas emerge during development and usage. Implementation depends on user need and development time availability.
