# tmux scrollback and transcript history

Issue: #690

## Summary

Session Manager should improve the long-running Claude/Codex terminal experience in two layers:

1. Make SM-managed tmux panes retain far more scrollback by default, with a configurable per-session `history-limit`.
2. Add a clean `sm history` surface backed by provider-native structured transcripts for search and review use cases such as "when was the last time I said X?"

The important design point is that tmux pane history and provider conversation history are not the same product. Tmux history is a screen buffer. It can be made deeper, but it cannot reliably become a clean transcript while Claude/Codex render interactive TUIs with prompt, footer, and status redraws.

## Observed behavior

Live probe on May 1, 2026 against `3348-owner`:

- SM session id: `52b4f831`
- tmux session: `claude-52b4f831`
- provider: `claude`
- pane size: `51x142`
- tmux pane history at probe time: `hist=1623/2000`
- global tmux setting: `history-limit 2000`

Capturing the full available pane history:

```bash
tmux capture-pane -p -J -S -2000 -t claude-52b4f831 | wc -l
# 1674
```

The first captured lines match the user-reported scrollback wall: the pane begins at a recent `#3348` review block instead of the start of the conversation. Older visible terminal history has already been discarded by tmux.

The same capture contains repeated Claude TUI redraw artifacts. In the 1674 rendered lines available, the `/rename 3348-owner` prompt/status block appears repeatedly:

```bash
tmux capture-pane -p -J -S -2000 -t claude-52b4f831 \
  | awk '/Session renamed to: 3348-owner/{r++} /new task\? \/clear/{n++} END {print r, n}'
# 13 7
```

These repeated blocks are not separate user-visible conversation turns. They are stale bottom-of-screen states that became part of tmux's scrollback as the Claude TUI repainted while more content arrived.

Session Manager already pipes pane bytes to a log:

```bash
/tmp/claude-sessions/claude-52b4f831.log
# 15M, about 1.3M newline-delimited terminal byte-stream fragments at probe time
```

That log is durable but not clean. It contains raw ANSI cursor movement, color, clear-screen, and repaint sequences. It is useful for diagnosis and low-level recovery, but it is not a user-facing transcript.

The provider-native transcript is much better suited for semantic history:

```bash
/Users/rajesh/.claude/projects/-Users-rajesh-Desktop-fractal-market-simulator/d7b6fbd0-b6aa-474e-8e6b-4d7806beca36.jsonl
# 9.0M, 3276 JSONL entries
```

For Codex/codex-fork, the structured sources are also available:

- `~/.codex/history.jsonl` records user prompts globally with `session_id`, timestamp, and text.
- `~/.codex/session_index.jsonl` records thread names and ids.
- Codex session JSONL files live under `~/.codex/sessions/.../rollout-...<thread-id>.jsonl`.
- SM codex-fork event streams already record `thread/started.payload.thread.path`, which points to the provider session JSONL.

## Diagnosis

### Problem 1: scrollback artifacts

Root cause: tmux scrollback records terminal screen history, not provider message history.

Claude and Codex are interactive TUIs. They redraw prompt regions, status footers, permission banners, token/context counters, and live progress indicators using cursor movement and screen updates. When those screen states scroll out of the visible pane, tmux preserves them as historical screen lines. Later, copy-mode scrollback shows those stale UI states between real conversation output.

Increasing tmux history depth does not remove this class of artifact. It only preserves more of the same screen-buffer history. A clean history/search feature must read provider transcripts, not tmux pane history.

### Problem 2: scrollback wall

Root cause: Session Manager does not configure a larger tmux history limit.

The current `TmuxController.create_session_with_command()` path creates a detached tmux session and starts piping pane output, but does not set `history-limit`. The active tmux server is using the default `history-limit 2000`, and every observed managed pane reports `/2000`.

Native terminal scrollback is usually much larger or effectively unbounded by user preference. The managed tmux panes therefore lose inspectable screen history much sooner.

## Goals

1. Make future SM-managed tmux sessions retain substantially more screen scrollback by default.
2. Apply the configured tmux history limit to live managed tmux sessions on server startup so existing active agents benefit from that point forward.
3. Provide a clean, provider-transcript-backed CLI for tailing and searching long session history.
4. Support Claude, Codex, and codex-fork histories in the first implementation.
5. Keep tmux as the runtime control and attach plane for existing SM workflows.
6. Keep raw `pipe-pane` logs as diagnostics, not as the primary human transcript source.

## Non-goals

1. Do not promise artifact-free tmux copy-mode scrollback. Tmux copy-mode remains screen-buffer history.
2. Do not build an ANSI terminal replay/de-artifact pipeline over `pipe-pane` logs in v1.
3. Do not replace tmux as the background runtime for Claude/Codex.
4. Do not modify Claude or Codex provider behavior.
5. Do not expose hidden reasoning, encrypted payloads, or provider-internal metadata that is not already visible as normal conversation content.
6. Do not backfill screen lines already discarded by tmux. Raising `history-limit` only preserves future screen history.

## Proposed solution

### 1. Configurable tmux history limit

Add a top-level tmux config section:

```yaml
tmux:
  history_limit: 100000
```

Recommended default: `100000`.

Why this value:

- It is a 50x improvement over tmux's default `2000`.
- It is closer to modern terminal scrollback expectations.
- It avoids pretending scrollback is infinite while keeping memory use bounded and configurable.

Implementation requirements:

- Add `TmuxController.history_limit`.
- Validate it as a positive integer; fall back to `100000` when absent or invalid.
- Immediately after `tmux new-session -d ...`, run:

```bash
tmux set-option -t <session_name> history-limit <history_limit>
```

- Apply this in both tmux creation paths:
  - `create_session()`
  - `create_session_with_command()`
- Add a `TmuxController.apply_history_limit(session_name)` helper so startup can update live managed panes.
- On Session Manager startup after state load, iterate live sessions with a `tmux_session` and call `apply_history_limit()` when the tmux session exists.
- Do not set global tmux options. Session Manager should not silently change user-owned tmux sessions outside SM.

Existing live sessions:

- Applying the option to an existing pane cannot recover lines already dropped under the previous limit.
- It does allow the pane to retain more future history after the server restart or helper call.

### 2. Provider transcript resolver

Add a small transcript-history module that resolves one SM session to a structured transcript source:

```text
src/transcript_history.py
```

Core concepts:

- `TranscriptSource`: provider, session id, path, source kind, freshness metadata.
- `TranscriptEntry`: timestamp, role, source, text, provider entry id, path, line number.
- `TranscriptResolver`: maps an SM `Session` to the best available provider transcript path.
- `TranscriptParser`: streams JSONL and yields normalized `TranscriptEntry` records.

Resolution rules:

Claude:

- Prefer `Session.transcript_path` when it exists.
- If missing, reuse the existing Claude transcript discovery logic that maps cwd/provider resume id to `~/.claude/projects/.../*.jsonl`.
- Keep storing `transcript_path` from Claude hooks as today.

Codex:

- Prefer `Session.transcript_path` if already discovered.
- If `provider_resume_id` is present, find `~/.codex/sessions/**/rollout-*<provider_resume_id>.jsonl`.
- If the session JSONL is missing, fall back to `~/.codex/history.jsonl` for user-prompt-only search by `session_id`.

Codex-fork:

- Prefer `Session.transcript_path` if already discovered.
- Parse the SM codex-fork event stream for the first `thread/started.payload.thread.path`.
- If the event stream is missing or incomplete, use the Codex `provider_resume_id` lookup path above.

Codex-app:

- Out of scope for the tmux problem, but the resolver should not reject it structurally. It can return `no_transcript_source` until a stable app-server transcript source is wired.

### 3. Transcript parser behavior

The parser must stream files line by line. Do not `read_text().splitlines()` on large transcripts.

Claude normalization:

- Include external user prompts from `type == "user"` entries whose `message.content` is a string.
- Classify `message.content` arrays containing `tool_result` as `tool_result`, not plain user prompts.
- Include assistant messages from `type == "assistant"` entries.
- Include local commands and command stdout only when `--include-system` is passed.
- Ignore `file-history-snapshot`, title updates, permission-mode records, and similar metadata by default.

Codex normalization:

- Include `response_item.payload.type == "message"` entries for `role == "user"` and `role == "assistant"`.
- Include visible commentary/final assistant text.
- Ignore encrypted reasoning payloads.
- Include tool call/result summaries only when `--include-tools` is passed.
- Use `event_msg.task_complete.last_agent_message` only as a fallback when no assistant message entry exists for that turn.

Search semantics:

- Case-insensitive substring search by default.
- `--regex` can be added if straightforward, but substring search is sufficient for v1.
- Output should include timestamp, role, short session label, and a snippet.
- `--last` should return only the newest matching entry.
- `--before N` and `--after N` should provide local transcript context around matches.

### 4. CLI

Add a new command group:

```bash
sm history path <session>
sm history tail <session> [--entries 100] [--role user|assistant|all] [--include-tools] [--include-system]
sm history search <session> <query> [--role user|assistant|all] [--last] [--before N] [--after N] [--include-tools] [--include-system]
```

Examples:

```bash
sm history search 3348-owner "proximity is not prerequisite" --last
sm history search 3348-owner "what I want is" --role user --last
sm history tail 3348-owner --entries 50
sm history path 3348-owner
```

Output shape:

```text
2026-04-29T23:15:42Z user  3348-owner  Read spec owner persona first...
  source: /Users/.../d7b6fbd0-b6aa-474e-8e6b-4d7806beca36.jsonl:3
```

This directly solves the "when was the last time I said X?" use case without relying on tmux scrollback still containing that text.

### 5. API

Add read-only server endpoints so future UI surfaces can reuse the same parser:

```text
GET /sessions/{session_id}/history/source
GET /sessions/{session_id}/history?limit=100&role=all&include_tools=false&include_system=false
GET /sessions/{session_id}/history/search?q=...&role=all&last=false&before=0&after=0
```

These endpoints should:

- Resolve aliases the same way existing session endpoints do.
- Bound output size.
- Stream/scan transcripts without loading whole files into memory.
- Return a clear `404` or typed error when no transcript source exists.

### 6. Status and attach affordances

Add one small user-facing hint when attaching to a managed tmux session whose current history limit is below the configured SM limit:

```text
[sm info] This pane has tmux history-limit 2000; configured SM limit is 100000. Future history will expand after Session Manager applies the setting.
```

Do not print this on every attach once the pane is already configured.

## Implementation plan

1. Add `tmux.history_limit` config loading and `TmuxController.apply_history_limit()`.
2. Apply the limit during tmux session creation before provider launch.
3. Apply the limit to live managed tmux sessions during Session Manager startup.
4. Add `src/transcript_history.py` with source resolution and streaming parser primitives.
5. Add server endpoints for source, tail, and search.
6. Add `sm history` CLI commands using the server endpoints.
7. Broaden `Session.transcript_path` comments and docs from "Claude transcript" to "provider transcript" where needed.
8. Store discovered Codex/codex-fork transcript paths when resolution succeeds, without making path discovery a hard requirement for session operation.
9. Add tests for tmux history configuration, startup application, transcript source resolution, Claude parsing, Codex parsing, CLI output, and bounded API responses.

## Edge cases

- A provider transcript file is missing after cleanup: return a clear no-source error and suggest `sm output` or raw log paths only as fallback diagnostics.
- Multiple Codex files match the same thread id: choose the newest mtime and log a warning.
- Transcript contains malformed JSONL lines: skip the malformed line, include a warning count in API/CLI metadata, and continue.
- Query matches tool output but the user requested `--role user`: do not return tool-result entries unless `--include-tools` is set.
- Session is active and transcript is being appended: tolerate partial last lines by skipping only the incomplete line.
- Existing tmux panes already lost old history: the higher limit only affects future retained screen history.
- Very large transcripts: scan line by line and cap rendered snippets.

## Acceptance criteria

1. New SM-managed Claude, Codex, and codex-fork tmux sessions are created with the configured `history-limit`.
2. Existing live managed tmux sessions have their `history-limit` raised on Session Manager startup when their tmux session still exists.
3. Session Manager does not change global tmux options or user-owned tmux sessions outside SM.
4. `sm history path 3348-owner` prints the provider transcript path for the session when available.
5. `sm history search 3348-owner "..." --role user --last` finds old user prompts even when tmux scrollback no longer contains them.
6. `sm history tail <session>` returns clean provider transcript entries, not raw ANSI pane bytes.
7. Claude transcript parsing excludes tool results from default user-message searches.
8. Codex/codex-fork transcript resolution works through provider session JSONL paths and codex-fork event streams.
9. API and CLI outputs are bounded and stream large transcript files without loading them whole.
10. Tests cover the new tmux config path, live-session application, parser normalization, resolver fallback behavior, and CLI/API error handling.

## Recommended design decision

Do not try to make tmux copy-mode be the clean long-term transcript.

The practical split is:

- tmux scrollback: deeper, useful for nearby visual context and manual terminal review.
- provider transcript history: clean, searchable, timestamped, and durable enough for long-running agent workflows.

This matches the actual data sources Session Manager already has and avoids overfitting to provider TUI repaint behavior.

## Ticket classification

Single implementation ticket. One agent can implement the tmux history-limit change and the first `sm history` transcript search/tail path without splitting this into an epic, as long as v1 stays limited to Claude, Codex, and codex-fork transcript sources.
