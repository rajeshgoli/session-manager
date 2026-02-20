# #137 — Native Codex /review Support for Code Reviews

**Status:** Draft v12
**Author:** Claude (Opus 4.6)
**Created:** 2026-02-14
**Updated:** 2026-02-14
**Ticket:** [#137](https://github.com/rajeshgoli/claude-sessions/issues/137)

---

## 1. Problem Statement

### Current Workflow

When the EM (Engineering Manager) agent wants a code review, it spawns a child session with a free-form prompt:

```bash
sm spawn codex "Review PR #42 on branch feat/login against main. \
  Check error handling, test coverage, security. \
  Read persona doc at docs/review-persona.md and apply checklist." \
  --name reviewer --wait 900
```

This child session is a full agent that burns regular tokens to:
1. Figure out what changed (manually running `git diff` or reading files)
2. Interpret the review criteria from the prompt
3. Produce unstructured text output

### Why This Is Suboptimal

| Issue | Impact |
|-------|--------|
| **Token waste** | Full agent session uses regular token budget for what should be a specialized review operation |
| **Unstructured output** | Free-form text vs. structured findings with priority levels, confidence scores, and precise code locations |
| **No diff awareness** | Agent must figure out what changed; `/review` natively computes diffs between branches |
| **Inconsistent quality** | Each review agent reinvents the review approach; `/review` has a battle-tested prompt with consistent output schema |

### Why This Matters

The session manager already supports Codex as a provider (`codex` for tmux CLI, `codex-app` for app-server). Adding native `/review` support means the EM can trigger reviews that:
- Use Codex's built-in review prompt (tuned for high-quality code review)
- Return structured JSON with P0-P3 prioritized findings
- Run inside visible tmux sessions the user can attach to and observe
- Produce consistent, parseable output that can be forwarded to Telegram, summarized, or acted upon programmatically

### Design Constraint: Two Review Paths

This spec supports two distinct review paths:

1. **Local TUI reviews** (`--base`, `--uncommitted`, `--commit`, `--custom`) — Run inside visible tmux sessions via the `/review` slash command. The user can `sm attach` and observe. Uses the local message quota.

2. **GitHub PR reviews** (`--pr`) — Trigger `@codex review` on a GitHub PR via comment. No tmux session needed. Uses the **separate weekly Code Reviews quota**, preserving local message budget for agent coding work. Review output appears as a standard GitHub PR review.

The non-interactive `codex review --base main` subprocess is explicitly **not** used for either path.

---

## 2. What We Found in the Docs

### Codex `/review` Interactive Slash Command

The `/review` command inside the interactive Codex CLI is a **read-only, specialized code reviewer**. It never modifies the working tree.

**Activation:** Type `/review` in the interactive CLI. A menu appears with four modes:

1. **Review against a base branch** — Select a local branch (e.g., `main`). Codex finds the merge base, computes the diff, and reviews your work against it.
2. **Review uncommitted changes** — Reviews staged, unstaged, and untracked files in the working tree.
3. **Review a specific commit** — Select a commit SHA from a list. Reviews that exact changeset.
4. **Custom review instructions** — Free-form prompt like `/review Focus on security vulnerabilities`.

**Under the hood:**
- Codex computes a `git diff` (unified format, 5 lines context) between comparison points
- The diff + file metadata is assembled into a review prompt
- Sent to the configured model with a strict JSON output schema
- Returns structured findings without modifying files

### Review Output Schema

```json
{
  "findings": [
    {
      "title": "[P1] Un-padding slices along wrong tensor dimensions",
      "body": "Markdown explanation with file/line/function citations...",
      "confidence_score": 0.85,
      "priority": 1,
      "code_location": {
        "absolute_file_path": "/path/to/file.py",
        "line_range": {"start": 42, "end": 48}
      }
    }
  ],
  "overall_correctness": "patch is correct",
  "overall_explanation": "1-3 sentence justification",
  "overall_confidence_score": 0.92
}
```

**Priority levels:** P0 (blocking), P1 (urgent), P2 (normal), P3 (low priority).

### Steering (Interactive TUI Only)

Codex supports mid-turn steering via the Enter key in the interactive CLI:
- While a turn is running, press **Enter** to inject new instructions into the **current turn**
- Press **Tab** to queue for the **next turn**
- In review context: can refine focus mid-review (e.g., "actually focus on the database queries")

This is an interactive TUI feature — it does not exist in the non-interactive `codex review` subcommand. This is another reason the TUI path is preferred.

### Known Limitation: `--base`/`--commit` + `[PROMPT]` Are Mutually Exclusive

In Codex CLI 0.101.0, the non-interactive `codex review --base main "Focus on security"` produces an error: `--base cannot be used with [PROMPT]`. The same applies to `--commit` + prompt. This limitation also affects how the interactive `/review` works — the four menu modes are distinct, you cannot combine branch-mode with custom instructions in a single invocation.

To add custom focus to a branch review, steering must be used after the review starts.

### Review Model Configuration

```toml
# ~/.codex/config.toml
review_model = "gpt-5.2-codex"
```

Review model can be configured separately from the session model.

### Quota — Two Separate Buckets

| Bucket | What Counts | Limit (Plus) | Limit (Pro) |
|--------|-------------|-------------|-------------|
| **Local messages** (5-hr rolling) | Local CLI `/review`, regular prompts | 45-225 | 300-1500 |
| **Code Reviews/week** | GitHub-integrated `@codex review` on PRs | 10-25/wk | 100-250/wk |

- Local CLI `/review` counts toward the **local message** quota — same bucket as regular agent work
- GitHub-integrated reviews (`@codex review` on PRs) use a **completely separate weekly quota**
- This means GitHub reviews are essentially free relative to the agent's coding budget — they don't consume local messages or cloud tasks

**This is the primary motivation for the `--pr` mode** added in this spec: by routing reviews through `@codex review` on GitHub, the EM can get code reviews without spending any of the local message budget that agents need for actual coding work.

### `@codex review` on GitHub PRs

Posting `@codex review` as a comment on a GitHub PR triggers a review from the Codex GitHub integration:

1. Codex reacts with 👀 acknowledging the request
2. Codex reads the PR diff and any `AGENTS.md` files in the repo
3. Codex posts a standard GitHub code review (P0/P1 findings flagged)
4. Custom focus can be added inline: `@codex review for security regressions`

**Key detail — `AGENTS.md` as review checklist:** Codex searches the repo for `AGENTS.md` files and follows any "Review guidelines" section it finds. It applies the guidance from the closest `AGENTS.md` to each changed file, so subdirectory-level overrides work.

This is directly relevant because in the target repo (`rajeshgoli/fractal-market-simulator`), `AGENTS.md` is a symlink to `CLAUDE.md`. Any review guidelines placed in `CLAUDE.md` will be automatically picked up by Codex GitHub reviews.

---

## 3. Design

### 3.1 Core Concept: Reuse Existing Sessions

Reviews are sent to **existing** Codex tmux sessions via the `/review` slash command. This means:

- No new session needed if a Codex session already exists in the right working directory
- The EM can `sm clear <session>` then `sm review <session> ...` to repurpose a child
- Or `sm review` can spawn a fresh Codex session if none is available
- In all cases, the review runs in a visible tmux session the user can `sm attach` to

```
EM Agent                         Session Manager                   Codex (tmux)
   │                                    │                              │
   ├─ sm review <session> --base main   │                              │
   │   --steer "read persona doc"       │                              │
   │                                    │                              │
   │                          ┌─────────┴──────────┐                   │
   │                          │ 1. resolve session  │                   │
   │                          │ 2. send "/review"   │──── /review ────>│
   │                          │ 3. navigate menu    │──── ↓ Enter ────>│
   │                          │    (select mode)    │                   │
   │                          │ 4. select branch    │──── ↓↓ Enter ──>│
   │                          └─────────┬──────────┘                   │
   │                                    │                              │
   │  (user can: sm attach <session>)   │ (review visible in TUI)     │
   │                                    │                              │
   │                                    │ 5. steer if requested        │
   │                                    │    Enter + text + Enter ────>│
   │                                    │                              │
   │                                    │ 6. review completes          │
   │                                    │    (Stop hook fires)         │
   │                                    │                              │
   │<─── completion notification ───────│                              │
```

### 3.2 The `sm review` Command

```bash
# === Local TUI modes (use local message quota) ===

# Review against a base branch (on an existing session)
sm review <session> --base <branch> [options]

# Review uncommitted changes
sm review <session> --uncommitted [options]

# Review a specific commit
sm review <session> --commit <sha> [options]

# Custom review instructions (free-form, no branch mode)
sm review <session> --custom "Focus on security" [options]

# Spawn a new session and immediately start review
sm review --new --base main [options]

# === GitHub PR mode (uses separate Code Reviews/week quota) ===

# Trigger @codex review on a GitHub PR
sm review --pr <number> [options]
```

**Options:**
```
--name <name>          Friendly name (only when --new)
--wait <seconds>       Monitor and notify when review completes (default: 600 in managed session, None standalone)
--model <model>        Model override for the spawned session (only when --new)
--working-dir <dir>    Override working directory (only when --new)
--steer <text>         Additional instructions to inject mid-review via Enter key (TUI modes) or append to @codex review comment (PR mode)
--repo <owner/repo>    GitHub repo for --pr mode (default: inferred from git remote in working dir)
```

**Examples:**
```bash
# Reuse an existing reviewer session for a branch review
sm review reviewer --base main --wait 600

# Same, but also steer the review with custom focus
sm review reviewer --base main --steer "Focus on auth security."

# Review uncommitted changes on an existing session
sm review reviewer --uncommitted

# Spawn fresh session and start review
sm review --new --base main --name pr42-review --wait 600

# Custom free-form review
sm review reviewer --custom "Check for SQL injection vulnerabilities in the auth module"

# === GitHub PR mode (separate quota) ===

# Trigger Codex GitHub review on PR #42
sm review --pr 42 --wait 600

# With custom focus (appended to the @codex review comment)
sm review --pr 42 --steer "Focus on auth security and SQL injection"

# Explicit repo (when not in the repo directory)
sm review --pr 42 --repo rajeshgoli/fractal-market-simulator --wait 600
```

### 3.3 How It Works: Tmux Interaction Sequence

The `/review` slash command presents an interactive menu navigated with arrow keys. The session manager automates this via tmux `send-keys`.

**Branch review (`sm review <session> --base main`):**

```
Step  Delay    Key Sequence              What It Does
────  ─────  ─ ────────────────────────  ─────────────────────────────
1     0s       /review                   Send /review slash command
2     0.3s     Enter                     Submit the command
3     1s       (wait for menu)           Menu appears with 4 review modes
4     0s       Enter                     Select "Review against a base branch" (1st item)
5     1s       (wait for branch list)    Branch picker appears
6     0s       ↓ × N                     Navigate to target branch
7     0.3s     Enter                     Confirm branch selection
8     —        (review runs)             Codex computes diff and reviews
```

**Uncommitted changes (`--uncommitted`):**

```
Step  Delay    Key Sequence              What It Does
────  ─────  ─ ────────────────────────  ─────────────────────────────
1-3   (same as above — send /review, wait for menu)
4     0s       ↓                         Move to "Review uncommitted changes" (2nd item)
5     0.3s     Enter                     Select it
6     —        (review runs)
```

**Specific commit (`--commit <sha>`):**

```
Step  Delay    Key Sequence              What It Does
────  ─────  ─ ────────────────────────  ─────────────────────────────
1-3   (same — send /review, wait for menu)
4     0s       ↓↓                        Move to "Review a specific commit" (3rd item)
5     0.3s     Enter                     Select it
6     1s       (wait for commit list)    Commit picker appears
7     0s       (navigate to commit)      Navigate to target SHA
8     0.3s     Enter                     Confirm
9     —        (review runs)
```

**Custom review (`--custom "..."`):**

```
Step  Delay    Key Sequence              What It Does
────  ─────  ─ ────────────────────────  ─────────────────────────────
1     0s       /review <custom text>     Send /review with custom prompt directly
2     0.3s     Enter                     Submit — bypasses menu, runs immediately
```

### 3.4 Branch Navigation Strategy

For branch mode, we need to select the target branch from a picker list. Approach:

1. Before sending `/review`, run `git branch --list` in the working directory to get the sorted branch list
2. Find the position of the target branch in that list
3. Send that many `↓` keys after the branch picker appears

If the branch isn't found in the list, fail before sending `/review` with a clear error.

### 3.5 Steering Mechanism

After a review starts, the EM may want to inject additional focus instructions. Since branch mode and custom prompt are mutually exclusive, steering is the **only way** to add custom instructions to a branch/commit review.

**Via `--steer` flag (at review start):**

When `--steer` is provided:
1. Wait for review output to begin (configurable delay: `steer_delay_seconds`)
2. Send `Enter` to open the steer input field
3. Send the steer text
4. Send `Enter` to submit

**Via `sm send --steer` (during review):**

The EM agent can steer an active review mid-turn:
```bash
sm send reviewer "Also check for SQL injection" --steer
```

The `--steer` flag bypasses normal message queue delivery and instead calls `send_steer_text()` directly on the target session's tmux pane: Enter (open steer field) → text → Enter (submit). This injects instructions into the **current** Codex turn without waiting for idle.

This is required because the EM is an agent — it cannot manually click the steer button in the Codex TUI. The `--steer` flag automates the full steer sequence.

**`--steer` vs other delivery modes:**

| Mode | Behavior | How | Use case |
|------|----------|-----|----------|
| `--sequential` (default) | Queued; delivered when idle (note: Codex sessions force immediate delivery) | Normal prompt injection | Post-review follow-up |
| `--important` | Queued with priority; delivered when idle (note: Codex sessions force immediate delivery) | Normal prompt injection | Notifications |
| `--urgent` | Immediate; sends Escape first to interrupt | Interrupts current turn | Emergency |
| `--steer` | Immediate; sends Enter first to open steer field | Injects into current turn via Codex steer | Mid-review focus change |

### 3.6 Review Session Model

Add `review_config` to the Session model to track review metadata:

```python
@dataclass
class ReviewConfig:
    """Configuration for a Codex review session."""
    mode: str  # "branch", "uncommitted", "commit", "custom", "pr"
    base_branch: Optional[str] = None    # For branch mode
    commit_sha: Optional[str] = None     # For commit mode
    custom_prompt: Optional[str] = None  # For custom mode
    steer_text: Optional[str] = None     # Instructions to inject after review starts (TUI) or append to comment (PR)
    steer_delivered: bool = False         # Whether steer text was injected
    pr_number: Optional[int] = None      # For PR mode
    pr_repo: Optional[str] = None        # For PR mode (owner/repo)
    pr_comment_id: Optional[int] = None  # GitHub comment ID (for tracking)
```

Note: no `fix_branch` field. The review always runs against the current HEAD. The user must be on the fix branch before starting the review. No automatic checkout.

For `--pr` mode, `ReviewConfig` is stored on the **caller's session** (not a child session) since no tmux session is created. The `pr_comment_id` is set after the comment is posted, enabling status checks.

### 3.7 API Endpoint

**POST `/sessions/{session_id}/review`**

Starts a review on an existing session.

```json
{
  "mode": "branch",
  "base_branch": "main",
  "steer": "Focus on auth security. Apply checklist."
}
```

**Response:**
```json
{
  "session_id": "def456",
  "review_mode": "branch",
  "base_branch": "main",
  "status": "started",
  "steer_queued": true
}
```

**POST `/sessions/review`** (with `--new` flag)

Spawns a new Codex session and starts a review.

```json
{
  "parent_session_id": "abc123",
  "mode": "branch",
  "base_branch": "main",
  "name": "pr42-review",
  "wait": 600,
  "working_dir": "/path/to/repo",
  "steer": "Focus on auth security."
}
```

**POST `/reviews/pr`** (GitHub PR mode)

Triggers a `@codex review` on a GitHub PR. No session required.

```json
{
  "pr_number": 42,
  "repo": "rajeshgoli/fractal-market-simulator",
  "steer": "Focus on auth security",
  "wait": 600,
  "caller_session_id": "abc123"
}
```

**Response:**
```json
{
  "pr_number": 42,
  "repo": "rajeshgoli/fractal-market-simulator",
  "comment_id": 12345678,
  "comment_body": "@codex review for Focus on auth security",
  "posted_at": "2026-02-14T10:30:00Z",
  "status": "posted",
  "server_polling": true
}
```

### 3.8 Completion Detection & `--wait`

The existing `OutputMonitor` (via tmux pipe-pane log files) detects review completion the same way it detects any Codex session going idle — via the Stop hook firing when Codex finishes the review turn and returns to the prompt.

**`--wait` has two distinct paths depending on invocation mode:**

**Existing-session reviews (`sm review <session> --base main --wait 600`):**

Uses the existing `watch_session()` infrastructure in `MessageQueueManager` (`src/message_queue.py:1068`). This polls the session's `delivery_state.is_idle` flag (set by the Stop hook) and notifies the caller when the review session goes idle.

- **Critical:** Before registering the watch, `start_review()` must call `message_queue_manager.mark_session_active(session_id)` (line 288). Otherwise, if the session was idle before `/review` was sent, `watch_session()` resolves immediately on the first poll (`_watch_for_idle` at line 1112-1115 checks `is_idle` with no grace period). Uses the public API to safely create the delivery state entry if it doesn't exist yet.
- Requires the caller to have a session context (`CLAUDE_SESSION_MANAGER_ID` set) so there's a session to notify
- If `--wait` is None (standalone user who didn't request it), no watch is registered — no warning needed
- Does **not** use `ChildMonitor` — no parent-child relationship needed

**Spawn-and-review (`sm review --new --base main --wait 600`):**

Uses `ChildMonitor.register_child()` (`src/child_monitor.py:44`) since a parent-child relationship exists.

- **Idle baseline fix:** When `start_review()` is called, set `session.last_tool_call = datetime.now()`. This is semantically imprecise (no tool call actually happened) but it correctly ensures the idle-time calculation at `child_monitor.py:101-102` uses the review start time as baseline, rather than falling through to `spawned_at` (line 110).
- Without this fix, `last_tool_call=None` causes fallback to `spawned_at/created_at`, which would declare the review idle immediately if the session was spawned minutes ago.

**Completion notification format:**

```
Review reviewer (def456) completed: review finished on branch main
```

### 3.9 Configuration

```yaml
# config.yaml additions (under existing codex section)
codex:
  review:
    default_wait: 600                # Default --wait seconds for reviews
    menu_settle_seconds: 1.0         # Wait for /review menu to appear
    branch_settle_seconds: 1.0       # Wait for branch picker to appear
    steer_delay_seconds: 5.0         # Wait before injecting steer text
```

No `menu_settle_seconds` or `branch_settle_seconds` TUI timing config needed in the main `codex` section — these are review-specific.

### 3.10 GitHub PR Mode (`--pr`)

The `--pr` mode is fundamentally different from the TUI modes. No tmux session, no key-sequence automation — it posts a GitHub comment and polls for the review to appear.

```
EM Agent                         Session Manager                   GitHub
   │                                    │                              │
   ├─ sm review --pr 42 --wait 600     │                              │
   │                                    │                              │
   │                          ┌─────────┴──────────┐                   │
   │                          │ 1. resolve repo     │                   │
   │                          │ 2. validate PR      │──── gh pr view ──>│
   │                          │ 3. post comment     │──── gh pr ───────>│
   │                          │    "@codex review"  │    comment        │
   │                          └─────────┬──────────┘                   │
   │                                    │                              │
   │                                    │    (Codex reacts with 👀)    │
   │                                    │    (Codex posts review)      │
   │                                    │                              │
   │                          ┌─────────┴──────────┐                   │
   │                          │ 4. poll for review  │──── gh pr ───────>│
   │                          │    (every 30s)      │    reviews        │
   │                          └─────────┬──────────┘                   │
   │                                    │                              │
   │<─── completion notification ───────│                              │
```

**How it works:**

1. **Resolve repo**: Infer from `--repo` flag or `git remote get-url origin` in working directory
2. **Validate PR**: `gh pr view <number> --repo <owner/repo> --json state` — confirm PR exists and is open
3. **Post comment**: Build comment body and post via `gh pr comment`
   - Without `--steer`: `@codex review`
   - With `--steer`: `@codex review for <steer text>`
4. **Poll for completion** (if `--wait`): Poll `gh api repos/{owner}/{repo}/pulls/{number}/reviews` periodically, looking for a new review from Codex (the `codex[bot]` user) submitted after the comment was posted
5. **Notify caller**: On poll success, send completion notification to the watcher session via `sm send`

**Comment body construction:**

```python
if steer:
    body = f"@codex review for {steer}"
else:
    body = "@codex review"
```

**Completion polling:**

Uses `poll_for_codex_review()` (defined in Step 9, `src/github_reviews.py`) — a sync function using `subprocess.run` + `time.sleep`. Polls `gh api repos/{owner}/{repo}/pulls/{number}/reviews`, filtering with `--jq '.[] | select(.user.login == "codex[bot]") | select(.submitted_at > "<since_iso>")'`.

For `--pr` mode, `ReviewConfig` (defined in section 3.6 with `pr_number`, `pr_repo`, `pr_comment_id` fields) is stored on the **caller's session** since no child tmux session is created. The `pr_comment_id` is set after the comment is posted, enabling status checks and review deduplication during polling.

**`--wait` contract for PR mode:**

| Invocation | Who polls | Behavior |
|------------|-----------|----------|
| `sm review --pr 42 --wait 600` (with `CLAUDE_SESSION_MANAGER_ID` set) | **Server** (background task) | API returns immediately. Server polls `gh api` in background. On completion, notifies caller session via `sm send`. |
| `sm review --pr 42 --wait 600` (standalone, no session context) | **CLI** (blocking) | API returns immediately with `posted_at` timestamp. CLI blocks and polls `gh api` directly using `poll_for_codex_review()`. Prints result to stdout on completion, exits 0. On timeout, exits 1. |
| `sm review --pr 42` (no `--wait`) | Nobody | Fire-and-forget: API posts comment and returns immediately. No polling. |

**Execution model split:** The server never blocks on long polls. For managed sessions, the server owns the poll lifecycle (background `asyncio.Task`). For standalone CLI invocations, the CLI owns the poll lifecycle (synchronous loop). Both use the same `poll_for_codex_review()` function from `github_reviews.py` — it's callable from both server and CLI contexts.

This matches the local TUI mode behavior: `--wait` defaults to 600 when caller has session context, None otherwise. PR mode uses `poll_for_codex_review()` (GitHub API polling) instead of `watch_session()` (tmux idle detection). The two completion paths are completely decoupled — PR mode never calls `watch_session()`.

**PR completion notification format:**
```
Review --pr 42 (rajeshgoli/fractal-market-simulator) completed: Codex posted review on PR #42
```

### 3.11 Unified Review Guidelines via AGENTS.md

For the review checklist to be applied consistently across all review paths (local TUI, GitHub PR, and manual reviews by Claude Code agents), the guidelines must live where all three consumers can find them.

**The convention:**
- Codex GitHub reviews read `AGENTS.md` for "Review guidelines"
- Claude Code reads `CLAUDE.md` for instructions
- In the target repo, `AGENTS.md` → symlink → `CLAUDE.md`

**Therefore:** Place the review checklist in `CLAUDE.md` under a "Review guidelines" section. All three paths see it.

**Current state** (in `rajeshgoli/fractal-market-simulator`):

```
CLAUDE.md                          ← main instructions (Claude Code reads this)
AGENTS.md → CLAUDE.md              ← symlink (Codex GitHub reviews read this)
.claude/personas/architect.md      ← review checklist lives HERE currently
```

**Target state:**

```
CLAUDE.md                          ← review checklist MOVED HERE (new "Review guidelines" section)
AGENTS.md → CLAUDE.md              ← symlink (Codex sees checklist automatically)
.claude/personas/architect.md      ← thin wrapper: "Apply review protocol from CLAUDE.md" + architect-specific rules
```

**What moves to CLAUDE.md:**
- The 8-point diff review checklist (dead code, magic numbers, abstraction, symmetric paths, upstream deps, pattern consistency, frontend wiring, SSE pipeline)
- The review output format template (checklist results, spec adherence, functional verification, decision)
- Phase structure (Phase 1: Diff, Phase 2: Spec Adherence, Phase 3: Functional Verification)

**What stays in architect.md:**
- Architect mindset ("strong bias toward deletion", trust boundary)
- Branch rules (never merge to main)
- Triggers (how architect is invoked)
- Merge protocol (post to GitHub, merge to dev)
- Fix It Now principle
- Handoff instructions and EM notification
- Session start naming convention
- Reference: "Apply the review protocol defined in CLAUDE.md"

**Why this matters for `--pr` mode:** When `sm review --pr 42` posts `@codex review`, Codex reads `AGENTS.md` (→ `CLAUDE.md`) and finds the same 8-point checklist. The review is automatically guided by the same standards, with no extra prompt engineering needed.

**Implementation:** This is a documentation change in the target repo, not a code change in session-manager. It should be done as a prerequisite before `--pr` mode is useful. A setup step (or `sm review --setup-guidelines`) could automate verifying the guidelines are in place.

---

## 4. Implementation Plan

### Phase 1: Core `sm review` with branch mode

#### Step 1: Add ReviewConfig model

**File:** `src/models.py`

Add after `CompletionStatus` enum (~line 50):

```python
@dataclass
class ReviewConfig:
    """Configuration for a Codex review session."""
    mode: str  # "branch", "uncommitted", "commit", "custom", "pr"
    base_branch: Optional[str] = None
    commit_sha: Optional[str] = None
    custom_prompt: Optional[str] = None
    steer_text: Optional[str] = None
    steer_delivered: bool = False
    pr_number: Optional[int] = None      # For PR mode
    pr_repo: Optional[str] = None        # For PR mode (owner/repo)
    pr_comment_id: Optional[int] = None  # GitHub comment ID (for tracking)

    def to_dict(self) -> dict:
        return {k: v for k, v in asdict(self).items()}

    @classmethod
    def from_dict(cls, data: dict) -> "ReviewConfig":
        return cls(**{k: v for k, v in data.items() if k in cls.__dataclass_fields__})
```

Add `review_config: Optional[ReviewConfig] = None` field to the `Session` dataclass (~line 112). Update `to_dict()` and `from_dict()` to serialize/deserialize it.

#### Step 2: Add review key-sequence methods to TmuxController

**File:** `src/tmux_controller.py`

Add two new async methods:

**`send_review_sequence()`** — Sends `/review`, waits for menu, navigates to the correct mode, selects branch if needed.

```python
async def send_review_sequence(
    self,
    session_name: str,
    mode: str,
    base_branch: Optional[str] = None,
    commit_sha: Optional[str] = None,
    custom_prompt: Optional[str] = None,
    branch_position: Optional[int] = None,  # Pre-computed position in branch list
    config: Optional[dict] = None,
) -> bool:
```

Logic by mode:
- `branch`: Send `/review` + Enter → wait → Enter (1st item) → wait → ↓×N + Enter (branch)
- `uncommitted`: Send `/review` + Enter → wait → ↓ + Enter (2nd item)
- `commit`: Send `/review` + Enter → wait → ↓↓ + Enter (3rd item) → wait → navigate to SHA
- `custom`: Send `/review <custom_prompt>` + Enter (bypasses menu)

**`send_steer_text()`** — Injects steer text into an active turn via Enter.

```python
async def send_steer_text(self, session_name: str, text: str) -> bool:
    """Inject steer text into an active Codex turn.

    Sends: Enter (open steer field) → text → Enter (submit).
    """
```

#### Step 3: Add `start_review()` to SessionManager

**File:** `src/session_manager.py`

Add new method:

```python
async def start_review(
    self,
    session_id: str,
    mode: str,
    base_branch: Optional[str] = None,
    commit_sha: Optional[str] = None,
    custom_prompt: Optional[str] = None,
    steer_text: Optional[str] = None,
    wait: Optional[int] = None,
    watcher_session_id: Optional[str] = None,
) -> dict:
```

This method:
1. Resolves session — must be an existing Codex session (`provider == "codex"`)
2. Validates: session exists, is a codex session, is idle, working dir is a git repo
3. For branch mode: runs `git branch --list` in working_dir to find branch position
4. Stores `ReviewConfig` on the session
5. Sets `session.last_tool_call = datetime.now()` (resets idle baseline for ChildMonitor)
6. Marks session active in delivery state: `message_queue_manager.mark_session_active(session_id)` (`src/message_queue.py:288`) — **critical for `--wait`**, otherwise `watch_session()` resolves immediately on first poll because the session was idle before `/review` was sent. Uses the public API rather than direct map access to avoid KeyError on fresh sessions where no delivery state exists yet (states are created lazily via `_get_or_create_state`)
7. Calls `tmux_controller.send_review_sequence()`
8. If `steer_text` provided, schedules steer injection after `steer_delay_seconds`
9. If `wait` and `watcher_session_id`: registers via `message_queue_manager.watch_session(session_id, watcher_session_id, wait)`
10. Returns status dict

Also add a separate method for spawn-and-review (`--new` flag):

```python
async def spawn_review_session(
    self,
    parent_session_id: str,
    mode: str,
    base_branch: Optional[str] = None,
    commit_sha: Optional[str] = None,
    custom_prompt: Optional[str] = None,
    steer_text: Optional[str] = None,
    name: Optional[str] = None,
    wait: Optional[int] = None,
    model: Optional[str] = None,
    working_dir: Optional[str] = None,
) -> Session:
```

This method:
1. Spawns a new Codex session via `spawn_child_session()` with `provider="codex"` and **no initial prompt**
2. Waits for Codex CLI to initialize (`claude_init_seconds`)
3. Calls `start_review()` on the new session
4. If `wait` specified, registers with `ChildMonitor`

#### Step 4: Add API endpoints

**File:** `src/server.py`

Add Pydantic request models:

```python
class StartReviewRequest(BaseModel):
    """Start a review on an existing session."""
    mode: str = "branch"
    base_branch: Optional[str] = None
    commit_sha: Optional[str] = None
    custom_prompt: Optional[str] = None
    steer: Optional[str] = None
    wait: Optional[int] = None              # Seconds to watch for completion
    watcher_session_id: Optional[str] = None  # Session to notify when review completes

class SpawnReviewRequest(BaseModel):
    """Spawn a new session and start a review."""
    parent_session_id: str
    mode: str = "branch"
    base_branch: Optional[str] = None
    commit_sha: Optional[str] = None
    custom_prompt: Optional[str] = None
    steer: Optional[str] = None
    name: Optional[str] = None
    wait: Optional[int] = None
    model: Optional[str] = None
    working_dir: Optional[str] = None
```

Add endpoints:

```python
@app.post("/sessions/{session_id}/review")
async def start_review(session_id: str, request: StartReviewRequest):
    """Start a Codex review on an existing session."""

@app.post("/sessions/review")
async def spawn_review(request: SpawnReviewRequest):
    """Spawn a new Codex session and start a review."""
```

#### Step 5: Add CLI command and dispatch

**File:** `src/cli/main.py`

Add argparse subparser:

```python
review_parser = subparsers.add_parser("review", help="Start a Codex code review")
review_parser.add_argument("session", nargs="?", help="Session ID or name to review on")
review_parser.add_argument("--base", help="Review against this base branch")
review_parser.add_argument("--uncommitted", action="store_true", help="Review uncommitted changes")
review_parser.add_argument("--commit", help="Review a specific commit SHA")
review_parser.add_argument("--custom", help="Custom review instructions")
review_parser.add_argument("--new", action="store_true", help="Spawn a new session for the review")
review_parser.add_argument("--name", help="Friendly name (with --new)")
review_parser.add_argument("--wait", type=int, default=None, help="Notify when review completes (seconds; defaults to 600 when in managed session)")
review_parser.add_argument("--model", help="Model override (with --new)")
review_parser.add_argument("--working-dir", help="Working directory (with --new)")
review_parser.add_argument("--steer", help="Instructions to inject after review starts")
```

Add `review` to `no_session_needed` list (for standalone invocation without parent context) and add dispatch.

Dispatch logic:
- If `session` provided and not `--new`: call `start_review()` on existing session
- If `--new`: call `spawn_review_session()` (requires `CLAUDE_SESSION_MANAGER_ID` for parent)
- If neither: error

**File:** `src/cli/commands.py`

Add `cmd_review()`:

```python
def cmd_review(
    client: SessionManagerClient,
    parent_session_id: Optional[str],
    session: Optional[str] = None,
    base: Optional[str] = None,
    uncommitted: bool = False,
    commit: Optional[str] = None,
    custom: Optional[str] = None,
    pr: Optional[int] = None,           # Phase 1b: GitHub PR mode
    repo: Optional[str] = None,         # Phase 1b: GitHub repo (owner/repo)
    new: bool = False,
    name: Optional[str] = None,
    wait: Optional[int] = None,
    model: Optional[str] = None,
    working_dir: Optional[str] = None,
    steer: Optional[str] = None,
) -> int:
```

**Note:** Phase 1b extends this same function with `pr`/`repo` params rather than introducing a separate handler. The `--pr` dispatch path calls `client.start_pr_review()` while all TUI modes call `client.start_review()` or `client.spawn_review()`.

Validation and defaulting:
- Exactly one mode required: `--base`, `--uncommitted`, `--commit`, `--custom`, or `--pr`
- `--pr` is mutually exclusive with `session` argument and `--new` (no tmux session involved)
- For TUI modes (`--base`, `--uncommitted`, `--commit`, `--custom`): if `--new` not set, `session` is required
- If `--new` set, `parent_session_id` is required (must be in a managed session)
- If no `parent_session_id` and no `--new`, runs standalone (review on existing session, no parent tracking)
- **`--wait` defaulting:** If `wait` is None and caller has session context (`parent_session_id` is set), default to 600. If no session context, leave as None (no watching). This avoids spurious warnings for standalone users who never asked to wait. Same rule applies for `--pr` mode.

#### Step 5b: Add `--steer` delivery mode to `sm send`

The EM agent needs to steer active reviews mid-turn. Since the EM is an agent (not a human at the TUI), it cannot click the Codex steer button manually. The `--steer` flag on `sm send` automates the full steer sequence.

**File:** `src/models.py`

Add to `DeliveryMode` enum:
```python
class DeliveryMode(Enum):
    SEQUENTIAL = "sequential"
    IMPORTANT = "important"
    URGENT = "urgent"
    STEER = "steer"  # Enter-based injection into active Codex turn
```

**File:** `src/session_manager.py`

Add `steer` branch to `send_input()` (at `session_manager.py:650`, before the existing `sequential` and `important/urgent` branches):

```python
# Handle steer mode — bypass queue, inject directly via Enter key
if delivery_mode == "steer":
    success = await self.tmux.send_steer_text(
        session.tmux_session, formatted_text
    )
    return DeliveryResult.DELIVERED if success else DeliveryResult.FAILED
```

This must be wired at the `SessionManager.send_input()` level, not in `message_queue.py`, because `send_input()` is the routing point that gates by delivery mode (lines 656-688). Unknown modes fall through to `_deliver_direct()` which would send as a plain prompt — wrong for steer. The attribute is `self.tmux` (not `self.tmux_controller`) per `session_manager.py:32`.

**File:** `src/cli/main.py`

Add `--steer` flag to existing `send` subparser (at line ~78, after `--urgent`):
```python
send_parser.add_argument("--steer", action="store_true",
    help="Inject text into active Codex turn via steer (Enter → text → Enter)")
```

Update delivery mode dispatch (at line ~254, after existing `urgent`/`important` branches):
```python
delivery_mode = "sequential"  # default
if args.urgent:
    delivery_mode = "urgent"
elif args.important:
    delivery_mode = "important"
elif args.steer:
    delivery_mode = "steer"
```

The flags are mutually exclusive by `elif` precedence (same pattern as existing `urgent`/`important`). If multiple are set, precedence is: urgent > important > steer > sequential.

**File:** `src/server.py`

Update `send_input` endpoint to accept `"steer"` as a valid delivery mode value.

#### Step 6: Add API client methods

**File:** `src/cli/client.py`

```python
def start_review(self, session_id: str, mode: str, **kwargs) -> Optional[dict]:
    """POST /sessions/{session_id}/review"""

def spawn_review(self, parent_session_id: str, mode: str, **kwargs) -> Optional[dict]:
    """POST /sessions/review"""
```

#### Step 7: Add config section

**File:** `config.yaml`

Add under existing `codex` section:

```yaml
codex:
  # ... existing codex config ...
  review:
    default_wait: 600
    menu_settle_seconds: 1.0
    branch_settle_seconds: 1.0
    steer_delay_seconds: 5.0
```

---

### Phase 1b: GitHub PR Mode (`--pr`)

#### Step 8: Move review guidelines to CLAUDE.md (target repo)

**Repo:** `rajeshgoli/fractal-market-simulator`

This is a prerequisite for `--pr` mode to produce useful reviews. Without it, `@codex review` runs with no project-specific guidance.

1. Add a "Review guidelines" section to `CLAUDE.md` containing:
   - The 8-point diff review checklist (from `.claude/personas/architect.md`)
   - The review output format template
   - Phase structure (Diff → Spec Adherence → Functional Verification)

2. Update `.claude/personas/architect.md` to be a thin wrapper:
   - Keep architect-specific rules (mindset, branch rules, merge protocol, Fix It Now, handoff, EM notification)
   - Replace the inline checklist with: "Apply the review protocol defined in CLAUDE.md"
   - Keep the "Review Output Format" section as-is (it's the architect's mandatory output structure, not just the checklist)

Since `AGENTS.md → CLAUDE.md`, Codex GitHub reviews will automatically pick up the checklist.

#### Step 9: Add `gh` CLI wrapper for PR reviews

**File:** `src/github_reviews.py` (new)

```python
def post_pr_review_comment(
    repo: str,
    pr_number: int,
    steer: Optional[str] = None,
) -> dict:
    """Post @codex review comment on a PR. Synchronous (subprocess.run).

    Returns: {"comment_id": int, "body": str, "posted_at": str}
    """

def poll_for_codex_review(
    repo: str,
    pr_number: int,
    since: datetime,
    timeout: int = 600,
    poll_interval: int = 30,
) -> Optional[dict]:
    """Poll for a Codex review on a PR. Synchronous.

    Uses subprocess.run in a loop with time.sleep(poll_interval) to call
    gh api repos/{owner}/{repo}/pulls/{number}/reviews. Looks for a review
    by codex[bot] submitted after `since`. Returns review data or None on timeout.

    Callable directly from sync CLI code. Server wraps with
    asyncio.to_thread(poll_for_codex_review, ...) for non-blocking background polling.
    """

def get_pr_repo_from_git(working_dir: str) -> Optional[str]:
    """Infer owner/repo from git remote origin URL. Synchronous (subprocess.run)."""
```

All functions use `subprocess.run` (sync). Server wraps with `asyncio.to_thread()` where needed:
- `post_pr_review_comment()`: `subprocess.run(["gh", "pr", "comment", ...])` to post
- `poll_for_codex_review()`: `subprocess.run(["gh", "api", ...])` in a `time.sleep` loop to poll (filter by `user.login == "codex[bot]"` and `submitted_at > comment_time`)
- `get_pr_repo_from_git()`: `subprocess.run(["gh", "repo", "view", "--json", "nameWithOwner"])` to infer repo

#### Step 10: Add `start_pr_review()` to SessionManager

**File:** `src/session_manager.py`

```python
async def start_pr_review(
    self,
    pr_number: int,
    repo: Optional[str] = None,
    steer: Optional[str] = None,
    wait: Optional[int] = None,
    caller_session_id: Optional[str] = None,
) -> dict:
```

This method:
1. Resolves repo from `--repo` flag or working directory
2. Validates PR exists and is open via `gh pr view`
3. If `caller_session_id` provided: stores `ReviewConfig` (mode=`"pr"`, `pr_number`, `pr_repo`) on caller's session. If absent (standalone), no persistence.
4. Posts `@codex review` comment (with optional steer text appended). Stores `pr_comment_id` on ReviewConfig if persisted.
5. Returns response dict including `posted_at` (ISO timestamp of when comment was posted) — needed by CLI for client-side polling.
6. If `wait` **and** `caller_session_id`: starts server-side background poll task via `asyncio.create_task(asyncio.to_thread(poll_for_codex_review, ...))`. On completion, notifies caller via `sm send`.
7. If `wait` **without** `caller_session_id`: server does **not** start a poll task. The CLI is responsible for polling (see Step 12). The API response contains everything the CLI needs to poll independently (`repo`, `pr_number`, `posted_at`).

#### Step 11: Add PR review API endpoint

**File:** `src/server.py`

```python
class PRReviewRequest(BaseModel):
    pr_number: int
    repo: Optional[str] = None
    steer: Optional[str] = None
    wait: Optional[int] = None
    caller_session_id: Optional[str] = None  # Where to store ReviewConfig + who to notify

@app.post("/reviews/pr")
async def start_pr_review(request: PRReviewRequest):
    """Trigger @codex review on a GitHub PR."""
```

#### Step 12: Add `--pr` to CLI command

**File:** `src/cli/main.py`

Add to existing `review` subparser:
```python
review_parser.add_argument("--pr", type=int, help="GitHub PR number (triggers @codex review)")
review_parser.add_argument("--repo", help="GitHub repo (owner/repo) for --pr mode")
```

Dispatch logic update:
- If `--pr` is set: call `start_pr_review()` via API — no session argument needed
- `--pr` is mutually exclusive with `--base`, `--uncommitted`, `--commit`, `--custom`, `--new`

**Standalone `--wait` polling (CLI-side):**

When `--pr` + `--wait` is used without session context (`CLAUDE_SESSION_MANAGER_ID` not set), the CLI owns the poll lifecycle:

```python
# In cmd_review(), after API call returns:
from datetime import datetime

response = client.start_pr_review(pr_number=pr, repo=repo, steer=steer)
if wait and not caller_session_id:
    # Server did NOT start polling — CLI polls directly (sync call)
    from src.github_reviews import poll_for_codex_review
    since = datetime.fromisoformat(response["posted_at"])
    result = poll_for_codex_review(
        repo=response["repo"],
        pr_number=response["pr_number"],
        since=since,
        timeout=wait,
    )
    if result:
        print(f"Codex review posted on PR #{pr}: {result['state']}")
        return 0
    else:
        print(f"Timeout: no Codex review found after {wait}s")
        return 1
```

This keeps the server stateless for standalone invocations while giving the CLI user a blocking wait experience.

### Phase 2: Output Parsing & Telegram Integration (follow-up)

- Parse review output from tmux pane to extract structured findings
- Forward parsed findings to Telegram with formatting
- Add `GET /sessions/{id}/review-results` endpoint

### Phase 3: App-server Integration (stretch)

- Support review via `codex-app` provider if Codex app-server exposes a review RPC method

---

## 5. Key Files to Modify

| File | Change |
|------|--------|
| `src/models.py` | Add `ReviewConfig` dataclass (with `pr_number`, `pr_repo`, `pr_comment_id` fields); add `review_config` field to `Session`; add `STEER` to `DeliveryMode` enum |
| `src/tmux_controller.py` | Add `send_review_sequence()` and `send_steer_text()` async methods |
| `src/session_manager.py` | Add `start_review()`, `spawn_review_session()`, and `start_pr_review()` methods |
| `src/github_reviews.py` | **New file.** `gh` CLI wrapper: `post_pr_review_comment()`, `poll_for_codex_review()`, `get_pr_repo_from_git()` |
| `src/server.py` | Add `StartReviewRequest`, `SpawnReviewRequest`, `PRReviewRequest` models and three endpoints |
| `src/cli/commands.py` | Add `cmd_review()` function with `--pr` dispatch path |
| `src/cli/main.py` | Add `review` subparser with `--pr` and `--repo` args; add `--steer` flag to existing `send` subparser |
| `src/session_manager.py` | *(also)* Add `steer` branch to `send_input()` before existing mode routing |
| `src/cli/client.py` | Add `start_review()`, `spawn_review()`, and `start_pr_review()` API client methods |
| `config.yaml` | Add `codex.review` configuration section |
| *(target repo)* `CLAUDE.md` | Move review checklist here from `architect.md`; add "Review guidelines" section |
| *(target repo)* `.claude/personas/architect.md` | Thin wrapper referencing `CLAUDE.md` review protocol |

---

## 6. Edge Cases & Risks

### Menu Navigation Reliability
The biggest risk is that Codex's TUI menu layout changes between versions, breaking positional navigation. Mitigations:
- **Version pinning**: Document which Codex CLI versions this was tested against.
- **Output monitoring**: After sending key sequences, capture tmux pane output to verify expected state transitions before proceeding to next step.
- **Custom mode fallback**: If branch/commit mode navigation fails, the user can always use `--custom` which bypasses the menu entirely (at the cost of losing native diff computation).

### Branch Not Found
If the specified base branch doesn't exist locally:
- Run `git branch --list <branch>` in the working directory before sending `/review`
- Fail fast with a clear error message from the CLI

### Working Directory Not a Git Repo
`/review` requires a git repository. Validate before sending:
- Run `git rev-parse --git-dir` in the session's working directory
- Return error if not a git repo

### Premature Idle Detection with `--wait`
For reused sessions, `ChildMonitor` calculates idle time from `last_tool_call` or falls back to `spawned_at/created_at` (child_monitor.py:101-110). Reviews don't make tool calls, so `last_tool_call` would be `None`, causing fallback to the original spawn time.

**Mitigation:** When `start_review()` is called, set `session.last_tool_call = datetime.now()`. This is a semantic hack (no tool call actually happened) but correctly puts the idle calculation into the `if last_tool_call` branch (line 101) using the review start time as baseline. Setting it to `None` would be counterproductive — it triggers the `else` fallback to `spawned_at`.

For existing-session reviews, `--wait` uses `watch_session()` (Stop-hook-based idle detection) instead of `ChildMonitor`, so the `last_tool_call` hack is only needed for `--new` reviews that go through `ChildMonitor`.

### Session Provider Mismatch
`sm review` only works on Codex CLI sessions (`provider == "codex"`). If called on a Claude or codex-app session:
- Return clear error: "Review requires a Codex CLI session (provider=codex)"

### Custom Instructions + Branch Mode
Codex CLI 0.101.0 does not support combining branch/commit mode with custom instructions in a single invocation (they are mutually exclusive menu options). The only way to add custom focus to a branch review is via steering after the review starts.

**User guidance:** Use `--base main --steer "focus on security"` rather than trying to combine `--base` with `--custom`.

### Concurrent Reviews on Same Session
Sending `/review` to a session that's mid-review will likely interrupt or queue a new review.
- Validate that the session is idle before sending `/review`
- If session is not idle, return error: "Session is busy. Wait for current work to complete or use sm clear first."

### PR Mode: Private Repository Access
`@codex review` works on private repos. The requirement is that the **ChatGPT Codex Connector** GitHub App must be installed on the repository (or org-wide with access to the repo). Once installed, you can push a branch to a private repo, open a PR, and `@codex review` works the same as on public repos.

Prerequisites:
- The Codex Connector GitHub App must be installed with access to the specific repo
- Code review must be enabled for the repo in Codex settings
- `gh` CLI must be authenticated with a token that has repo access
- If the Codex app is not installed, `@codex review` will be a regular comment with no effect — the poll will time out

**Validation:** Before posting the comment, check if the Codex app is installed:
```bash
gh api repos/{owner}/{repo}/installation --jq '.app_slug' 2>/dev/null
```
If this fails or returns something other than a codex-related slug, warn the user that Codex may not be installed on this repo.

### PR Mode: Review Deduplication
If `@codex review` is posted multiple times on the same PR, Codex may run multiple reviews. The polling logic must match reviews by `submittedAt > comment_posted_at` to avoid picking up stale reviews from previous invocations.

### PR Mode: `gh` CLI Not Authenticated
If `gh auth status` fails, `--pr` mode should fail fast with a clear error before attempting to post.
