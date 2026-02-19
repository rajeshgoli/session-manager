# sm#192 — Codex native app (codex-app) integration: current state and tradeoffs

## Problem / Investigation Scope

The Codex native macOS app reportedly offers 2x rate limits until April 2, 2026.
This investigation answers: does `sm spawn codex-app` already work, can
`sm send`/`sm clear`/`sm wait` interact with codex-app sessions, and what are the
practical tradeoffs vs the codex CLI?

---

## Finding 1: `sm spawn codex-app` fully works today

All core sm commands were tested live against real codex-app sessions:

| Command | Result |
|---------|--------|
| `sm spawn codex-app "prompt"` | ✓ Spawns session, receives response |
| `sm spawn codex-app --wait 60 "prompt"` | ✓ Blocks until response complete |
| `sm send <id> "msg"` | ✓ Delivered via message queue when session is idle |
| `sm output <id>` | ✓ Returns last model response |
| `sm clear <id> "new prompt"` | ✓ Starts a new Codex thread, sends prompt |
| `sm kill <id>` | ✓ Terminates the app-server subprocess |
| `sm children` | ✓ Lists codex-app sessions with idle/running status |
| `sm dispatch <id> role` | ✓ Works (uses `sm send` under the hood) |

**`codex-app` is production-ready for spawn-and-dispatch workflows.**

---

## Finding 2: What `codex-app` actually is

`sm spawn codex-app` runs `codex app-server` — an experimental JSON-RPC over
stdio interface built into the `codex` CLI. It is **not** the macOS native
Codex.app GUI directly; it is the same underlying engine used by the macOS app
and the VSCode extension.

Key architecture:
- `src/codex_app_server.py` manages a single `codex app-server` subprocess per
  session.
- Communication is JSON-RPC over stdin/stdout with request/response and
  notification patterns.
- Turn results stream via `item/agentMessage/delta` notifications; completion
  fires `turn/completed`.
- The session manager converts completions into stop-hook-like events so the
  message queue delivery system works identically to Claude sessions.

---

## Finding 3: Rate limits — codex-app likely matches the native app

The codex CLI and the macOS native app share a single auth file
(`~/.codex/auth.json`, `auth_mode: 'chatgpt'`). The JWT token shows:
- Same client_id (`app_EMoamEEZ73f0CkXaXp7hrann`) for both.
- ChatGPT Plus plan (`chatgpt_plan_type: 'plus'`).

Since `codex app-server` and the native macOS GUI use the same OAuth tokens and
the same backend endpoint, they are subject to **the same rate limits**. Using
`sm spawn codex-app` should provide the same 2x capacity as the macOS native
app, because it is the same infrastructure authenticated with the same account.

**The native app's 2x limit is already accessible via `sm spawn codex-app`.**

There are two installed codex binaries:
- System CLI: `/opt/homebrew/bin/codex` (v0.104.0) — what sm uses by default.
- App-bundled CLI: `/Applications/Codex.app/Contents/Resources/codex` (v0.100.0-alpha.10).

Both support `app-server`. The system CLI is newer. If the native app's specific
version matters for limits, `config.yaml` can be pointed at the app-bundled
binary:
```yaml
codex_app_server:
  command: "/Applications/Codex.app/Contents/Resources/codex"
```

---

## Finding 4: What `sm send`/`sm clear`/`sm wait` do for codex-app sessions

**`sm send <id> "msg"`:**
Queues the message. When the codex-app session completes its current turn (idle),
the message queue delivers it via `session_manager.send_input()` →
`codex_session.send_user_turn(text)` (JSON-RPC `turn/start`).

**`sm clear <id> "new prompt"`:**
Calls `session_manager.clear_session()` → `codex_session.start_new_thread()`
(JSON-RPC `thread/start`). Creates a fresh Codex thread, discarding prior
conversation context. Optionally sends a new prompt immediately.

**`sm wait <id> N`:**
Monitors the codex-app session status (idle/running). When the turn completes,
the session is marked idle and the wait returns. Works via the same polling
mechanism as Claude sessions.

**`sm dispatch <id> role --param value`:**
Works. Dispatch expands a template and calls `cmd_send`, which routes through
the message queue. No special codex-app handling needed.

---

## Finding 5: Gaps vs Claude sessions

| Feature | Claude sessions | Codex-app sessions |
|---------|----------------|-------------------|
| `sm send` / message queue | ✓ | ✓ |
| `sm clear` | ✓ (Claude `/clear`) | ✓ (new thread) |
| `sm wait` | ✓ | ✓ |
| `sm dispatch` | ✓ | ✓ |
| `sm handoff` | ✓ | ✗ Explicitly rejected (`server.py:2111`) |
| `sm context-monitor` | ✓ | ✗ No context usage events from Codex |
| Tool logging (`tool_usage.db`) | ✓ | ✗ (sm#185 Phase 2a not implemented) |
| `sm output` (live tmux) | ✓ | ✗ Returns last turn text only |
| `sm tail --raw` | ✓ | Partial (returns last turn text via `get_last_message`) |
| Output monitoring (auto-detect death) | ✓ | ✗ No tmux monitoring |
| Transcript access | ✓ | ✗ No JSONL transcript |
| Stop hook | ✓ (Claude Code hook) | Simulated via turn completion |
| Telegram notifications on stop | ✓ | ✓ (via `_handle_codex_turn_complete`) |
| `sm name` | ✓ | ✓ (no tmux statusbar update) |
| `sm status` | ✓ | ✓ |
| `sm children` | ✓ | ✓ |
| `sm what <id>` | ✓ | Partial (uses `get_last_message`) |
| Dead session auto-cleanup | ✓ (tmux monitor) | ✗ No equivalent |
| Sandbox enforcement | Configured per-session | `approval_decision: decline` by default |

**Notable gaps:**
1. **No context monitoring**: Codex doesn't emit context usage events; the
   `sm context-monitor enable` API can be called but is a no-op for codex-app
   sessions since no events arrive.
2. **No tool logging**: `codex_app_server.py` drops `item/commandExecution` and
   `item/fileChange` notifications (sm#185 Phase 2a is unimplemented).
3. **No handoff**: `sm handoff` is explicitly rejected for codex-app sessions
   (server.py:2111). The mechanism relies on Claude's `/clear` and tmux pane.
4. **Dead session detection**: If the `codex app-server` process crashes, there
   is no tmux monitor to detect it. The session stays in the session list
   forever until killed manually.
5. **Sandbox by default**: The `approval_decision` is `"decline"` by default
   (codex_app_server.py:22), so tool requests are auto-declined. For agent
   workflows that require code execution, the config must set
   `approval_decision: "accept"` or `"acceptForSession"`.

---

## Finding 6: Practical tradeoffs vs codex CLI (provider=codex)

| Dimension | `codex` (tmux CLI) | `codex-app` (app-server) |
|-----------|--------------------|--------------------------|
| I/O method | tmux pane (keyboard injection) | JSON-RPC stdio (direct) |
| Response capture | Heuristic (pattern detection) | Exact (turn/completed event) |
| Send reliability | Good; can misfire on ANSI noise | Excellent; clean RPC |
| Clear mechanism | `/new` injected into tmux | New thread (clean break) |
| Context monitoring | N/A (not Claude) | N/A |
| Tool logging | Batch via rollout files (sm#185 2b) | Streaming via RPC (sm#185 2a) |
| Interactive access | `sm attach` opens tmux pane | No interactive access |
| Crash recovery | tmux monitor detects death | None |
| Rate limits | Same ChatGPT auth | Same ChatGPT auth |
| Sandbox | `--dangerously-bypass-approvals-and-sandbox` | Configurable per policy |

`codex-app` is generally more reliable for automated workflows because the
JSON-RPC protocol gives exact turn boundaries and clean message delivery —
there's no heuristic needed to detect when Codex has finished responding.

---

## Recommendation

**Use `sm spawn codex-app` today for parallel spec-review and dispatch
workflows.** It is fully functional for the EM/scout/codex-reviewer loop:

```bash
# Spawn a codex reviewer
sm spawn codex-app --name codex-reviewer "You are a spec reviewer..."

# Dispatch work to it
sm dispatch codex-reviewer reviewer --spec docs/working/200_spec.md

# Wait for response
sm wait codex-reviewer 300
```

The 2x rate limit benefit is already accessible since `codex app-server` uses
the same ChatGPT Plus OAuth tokens as the native macOS app.

**Config to enable tool execution** (required if codex-app needs to run code):
```yaml
codex_app_server:
  approval_decision: "accept"   # or "acceptForSession"
  sandbox: "workspace-write"    # already default
```

**Known limitation to plan around**: no `sm handoff` support. Long-running
codex-app agents must be manually managed rather than using the handoff
mechanism for context rotation.

---

## Classification

Single ticket. Investigation only — no code changes needed to use codex-app
today. Remaining gaps (tool logging, handoff, dead-session detection) are
tracked in sm#185 and are separate implementation tickets.
