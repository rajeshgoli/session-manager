# tmux native scrollback and retained history

Issue: #690

## Summary

Session Manager should improve the Claude/Codex tmux terminal experience with a tmux-only v1:

1. Prefer native terminal scrollback for live attached sessions where tmux can safely support it.
2. Raise retained tmux pane history for output produced while sessions are detached.
3. Keep tmux as the runtime/control plane for managed Claude, Codex, and codex-fork sessions.

Provider transcript browsing, `sm inspect`, and `sm history` are intentionally deferred to a separate spec. This spec focuses only on making tmux behave as close to native terminal as possible.

## Observed behavior

Live probe on May 1, 2026 against `3348-owner`:

- SM session id: `52b4f831`
- tmux session: `claude-52b4f831`
- provider: `claude`
- pane size: `51x142`
- tmux pane history at probe time: `hist=1623/2000`
- global tmux setting: `history-limit 2000`
- attached tmux clients: none at probe time

Capturing the full available pane history:

```bash
tmux capture-pane -p -J -S -2000 -t claude-52b4f831 | wc -l
# 1674
```

The first captured lines match the reported scrollback wall: the pane begins at a recent `#3348` review block instead of the start of the conversation. Older visible terminal history had already been discarded by tmux.

The same capture contained repeated Claude TUI redraw artifacts:

```bash
tmux capture-pane -p -J -S -2000 -t claude-52b4f831 \
  | awk '/Session renamed to: 3348-owner/{r++} /new task\? \/clear/{n++} END {print r, n}'
# 13 7
```

These repeated blocks are stale screen states retained by tmux pane history. Increasing tmux history depth preserves more history, but it does not turn screen-buffer history into clean provider turns.

## Diagnosis

Native terminal scrollback feels better for two separate reasons:

1. The terminal emulator is usually configured with a much larger or effectively unlimited scrollback buffer. Current SM tmux panes are capped at `2000` history lines.
2. Default tmux attach uses the caller terminal's alternate screen, so the outer terminal scrollback does not receive the long live stream. The outer terminal mostly sees the current tmux viewport.

Tmux pane history is a bounded screen buffer. It records provider TUI repaints, prompt/footer/status lines, and current-screen states. That is why artifacts appear in copy-mode scrollback.

## Native Scrollback Spike

The spike tested tmux 3.6a with isolated tmux servers/sockets so the user's default tmux server was not modified.

Relevant tmux docs:

- `history-limit` applies only to new windows; existing window histories are not resized.
- `alternate-screen` controls whether programs inside a pane may use `smcup`/`rmcup`.
- The tmux FAQ says tmux makes no attempt to keep terminal scrollback consistent and disabling alternate-screen use can still be incomplete.

Probe results:

1. Default attached tmux emits outer-terminal alternate-screen enter/exit sequences:

```text
default alt_1049h=1 alt_1049l=1
normal  alt_1049h=0 alt_1049l=0
```

The `normal` case used:

```tmux
set -as terminal-overrides ',*:smcup@:rmcup@'
```

2. If an outer terminal is attached before output starts, no-alternate-screen tmux can populate the outer terminal's scrollback. Using an outer tmux pane as a stand-in terminal emulator:

```text
default outer history: hist=0/2000, first/mid lines missing, only current bottom viewport visible
normal  outer history: hist=338/2000, first/mid/last lines all visible
```

3. If output is produced while the SM tmux session is detached, attaching later cannot populate the caller terminal's old scrollback. Even in no-alternate-screen mode, attach only painted the current bottom viewport:

```text
default first_line_seen=0 mid_line_seen=0 last_line_seen=2
normal  first_line_seen=0 mid_line_seen=0 last_line_seen=2
```

4. Programmatically sweeping tmux copy-mode from `history-top` through retained pane history did not preload the outer terminal's scrollback. It only changed the current viewport:

```text
after_attach      outer hist=0, first=0, mid=0, last=1
after_history_top outer hist=0, first=1, mid=0, last=0
after_sweep       outer hist=0, first=0, mid=0, last=1
```

5. `clear-history` really drops tmux's saved history. `refresh-client` and `refresh-from-pane` do not cause the provider process to replay old output:

```text
before_clear hist=117/2000 first=1 last=1
after_clear  hist=0/2000   first=0 last=1
```

Conclusion: native terminal scrollback is achievable for output produced while attached. It is not achievable for output produced while no terminal client is attached, because no caller terminal exists to receive that stream.

## Goals

1. Make live attached SM sessions use native terminal scrollback when tmux can safely support it.
2. Make future SM-managed tmux sessions retain substantially more pane history by default.
3. Correctly apply the configured tmux history limit before the provider pane/window is created.
4. Keep existing managed sessions attachable during migration.
5. Avoid mutating the user's default tmux server or unrelated tmux sessions.

## Non-goals

1. Do not add `sm inspect`, `sm history`, provider transcript parsing, or transcript search in this ticket.
2. Do not promise artifact-free tmux copy-mode scrollback. Tmux copy-mode remains screen-buffer history.
3. Do not build an ANSI terminal replay/de-artifact pipeline over `pipe-pane` logs.
4. Do not replace tmux as the background runtime for Claude/Codex.
5. Do not modify Claude or Codex provider behavior.
6. Do not claim exact native-terminal parity for output produced while no caller terminal was attached.
7. Do not backfill screen lines already discarded by tmux. Raising `history-limit` only preserves future screen history.

## Proposed Solution

### 1. SM-owned tmux server/socket

Add tmux config:

```yaml
tmux:
  socket_name: session-manager
  native_scrollback: true
  history_limit: 100000
```

Session Manager should route managed tmux operations through the configured socket:

```bash
tmux -L session-manager ...
```

Rationale:

- `terminal-overrides` is a tmux server option.
- Setting it on the user's default tmux server would affect unrelated tmux sessions.
- An SM-owned socket lets Session Manager opt managed sessions into native-scrollback behavior without surprising the rest of the machine.

Implementation requirements:

- Add a tmux command wrapper that injects `-L <socket_name>` when configured.
- Use the wrapper for create, attach-descriptor metadata, status, send-keys, capture-pane, pipe-pane, kill, rename, status-bar, and all other managed tmux operations.
- Keep a compatibility path for sessions created before this change on the default tmux server.
- Store or infer whether a session belongs to the SM socket so attach flows can choose the right tmux command.

### 2. Native scrollback attach mode

When `tmux.native_scrollback` is enabled, initialize the SM-owned tmux server with:

```tmux
set -as terminal-overrides ',*:smcup@:rmcup@'
```

Expected behavior:

- If the user is attached before output is produced, the caller terminal's own scrollback accumulates live session output.
- If the session is detached while output is produced, the caller terminal cannot receive that past output. Reattach only draws the current tmux viewport.

Attach flows that must use the same socket/config as session creation:

- `sm attach`
- `sm new`
- `sm codex-2`
- `sm watch` Enter attach
- any restore-and-attach path

### 3. Larger tmux history limit

Recommended default:

```yaml
tmux:
  history_limit: 100000
```

Why this value:

- It is a 50x improvement over tmux's default `2000`.
- It is closer to modern terminal scrollback expectations.
- It keeps memory bounded and configurable.

Important tmux constraint: `history-limit` applies only to new windows. Existing window histories are not resized.

Create SM sessions using a bootstrap-window sequence:

```bash
tmux -L <socket> new-session -d -s <session_name> -c <working_dir> -n __sm_bootstrap
tmux -L <socket> set-option -t <session_name> history-limit <history_limit>
tmux -L <socket> new-window -d -t <session_name> -n main -c <working_dir>
tmux -L <socket> kill-window -t <session_name>:__sm_bootstrap
tmux -L <socket> select-window -t <session_name>:main
```

The provider must be launched only in the `main` window created after the session option is set.

Apply this in both tmux creation paths:

- `create_session()`
- `create_session_with_command()`

### 4. Existing sessions

Existing live panes cannot be resized in place. Do not recreate or restore active sessions just to raise scrollback.

Existing sessions created on the default tmux server should remain attachable through a compatibility path. Session Manager should not silently move them to the SM socket or mutate the default server's `terminal-overrides`.

Add a one-time attach/status hint for old panes when useful:

```text
[sm info] This pane was created with tmux history-limit 2000; configured SM limit is 100000. Existing tmux panes cannot be resized in place. New and restored sessions will use the configured limit.
```

## Implementation Plan

1. Add `tmux.socket_name`, `tmux.native_scrollback`, and `tmux.history_limit` config loading.
2. Add `TmuxController` support for a configured tmux socket.
3. Initialize the SM-owned tmux server/socket with no-alternate-screen terminal overrides when `native_scrollback` is enabled.
4. Update tmux creation to use the bootstrap-window sequence so the provider window is created after the per-session `history-limit` is set.
5. Update CLI and TUI attach flows to use the same tmux socket as session creation.
6. Preserve default-server compatibility for existing live sessions.
7. Add status/attach visibility for live panes whose current `history_limit` is below the configured target.
8. Add tests for socket routing, native-scrollback server initialization, history-limit ordering, and attach command selection.

## Acceptance Criteria

1. New SM-managed Claude, Codex, and codex-fork tmux sessions are created on the configured SM tmux socket.
2. With `tmux.native_scrollback: true`, attached-from-start sessions do not send outer-terminal `1049` alternate-screen enter/exit sequences.
3. An integration probe sees first/mid/last live output lines in outer terminal scrollback when attached before output starts.
4. Output produced while no client is attached is not claimed to be present in caller terminal native scrollback after later attach.
5. New provider panes are created with configured `history-limit`.
6. Tests prove setting `history-limit` after the initial `new-session` window is insufficient, and implementation creates the provider window after setting the option.
7. Session Manager does not change global tmux options or user-owned tmux sessions outside the configured SM tmux socket.
8. Existing default-server sessions remain attachable through a compatibility path.
9. `sm attach`, `sm new`, `sm codex-2`, and `sm watch` use the correct tmux socket.
10. Tests cover config loading, tmux socket routing, native-scrollback initialization, bootstrap-window ordering, history-limit reporting, and legacy attach fallback.

## Research Sources

- tmux FAQ: <https://github.com/tmux/tmux/wiki/FAQ>
- tmux manual: <https://man.openbsd.org/tmux.1>

## Ticket Classification

Single implementation ticket. One agent can implement socket routing, native-scrollback attach mode, larger tmux history, and legacy attach fallback without needing a separate epic. Transcript-backed inspection/search should be tracked separately.
