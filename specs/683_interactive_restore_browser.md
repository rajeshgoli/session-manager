# Interactive Restore Browser

Issue: #683

## Summary

Add an interactive restore browser to Session Manager so a user can find and restore stopped SM-managed sessions without remembering an exact session id or friendly name, and without falling back to noisy provider-native `/restore` lists that include many orchestrator-created threads.

The recommended v1 shape is:

```bash
sm watch --restore
```

This opens a watch-like curses interface over restorable Session Manager records only. The user can search/filter, navigate the hierarchy, restore the selected session, attach when a terminal exists, and return to the restore browser after detaching.

## Problem

Today there are two practical restore paths:

1. `sm restore <id-or-name>` works well when the user remembers the exact target.
2. Provider-native restore UIs (`/restore` in Claude/Codex) help discovery but show every provider thread, including many ephemeral orchestrator-created agents. That list is too noisy when the user is trying to find a specific SM-managed agent.

Session Manager already has better metadata than provider-native restore lists:

- explicit SM friendly names and aliases
- parent/child hierarchy
- repo and role
- provider
- last activity
- stopped/restored state

The missing piece is a focused discovery UI over stopped/restorable SM records.

## Goals

1. Add a terminal UI for browsing stopped sessions that can be restored by SM.
2. Reuse the existing `sm watch` visual language where practical: hierarchy, repo grouping, selection, search, and attach/return behavior.
3. Show enough restore-specific metadata to choose the right target: last active, retired/stopped age, provider, role, repo, parent/child context, and why a row is or is not restorable when applicable.
4. Keep read paths cached and non-blocking. Restore browsing must not probe tmux or provider state for every stopped session on each refresh.
5. Restore the selected session through the same backend path as `sm restore <id>`, preserving all existing provider-specific resume behavior and safeguards.
6. After restoring a tmux-backed session, attach to it when possible; when the user detaches, return to the restore browser.

## Non-Goals

1. Do not integrate directly with provider-native `/restore` lists in v1.
2. Do not discover provider threads that are not already represented by SM session records.
3. Do not add provider-level deletion or archive management.
4. Do not live-probe every stopped tmux/provider runtime during list refresh.
5. Do not replace `sm restore <identifier>`; this adds a discovery surface for humans.
6. Do not make Codex app attachable. Headless providers can be restored, but the UI should report that no tmux attach is available.

## User Experience

### Entry Point

```bash
sm watch --restore [--repo PATH] [--role ROLE] [--top-level] [--all]
```

`--restore` switches `sm watch` from active-session mode to restore-browser mode.

Default behavior should list stopped sessions only. These are the sessions that `sm restore` is expected to handle. A future implementation may include non-stopped rows with an explanatory state, but v1 should stay focused on restore candidates.

### Layout

The UI should reuse the existing watch tree rendering, but with restore-specific columns:

| Column | Meaning |
| --- | --- |
| Session | Tree-prefixed friendly name/name/id. Prefer the same effective cached identity as watch. |
| ID | Session id. |
| Parent | Parent name/id when available. |
| Role | SM role. |
| Provider | `claude`, `codex`, `codex-fork`, `codex-app`. |
| Repo | Short repo/workdir label. |
| Last Active | Relative age from `last_activity`. |
| Retired | Relative age from the stop/retire timestamp if available; fallback to stopped status age if no dedicated field exists. |
| Restore | `ready`, `no-resume-id`, `headless`, or concise error/warning state. |

If terminal width is constrained, keep Session, ID, Provider, Last Active, and Restore before lower-priority columns.

### Hierarchy And Collapse

The default should preserve parent/child context because orchestrator-created agents are easier to identify by their parent lane. However, large fan-outs need controls:

- `--top-level` starts with only top-level stopped sessions visible.
- `Tab` toggles expansion for the selected subtree.
- `C` collapses all.
- `E` expands all.

Rows should remain selectable only for actual stopped sessions, not repo/group headers.

The first implementation can keep the existing watch behavior of rendering all descendants when expanded. It does not need a separate virtual tree store if `build_watch_rows` can be parameterized cleanly.

### Search And Filters

Search should be incremental enough for human use, but v1 can use the existing prompt-based filter interaction:

- `/` prompts for a text filter; blank clears it.
- Match against friendly name, raw name, id, alias, role, provider, repo/workdir basename, parent name/id, and current task/status text if present.
- `--repo PATH` and `--role ROLE` should work as they do in normal watch mode.

Filtering should preserve tree context for matching descendants and ancestors, following the existing repo-context behavior where possible.

### Actions

Recommended key bindings in restore mode:

| Key | Action |
| --- | --- |
| `j` / Down | Move selection down. |
| `k` / Up | Move selection up. |
| `/` | Search/filter. |
| `r` | Refresh list. |
| `Tab` | Expand/collapse selected subtree. |
| `C` | Collapse all. |
| `E` | Expand all. |
| `Enter` | Restore selected session. |
| `a` | Attach to selected restored/running tmux-backed session if already restored. Optional v1 if Enter attaches after restore. |
| `q` / Esc | Quit. |

`Enter` should:

1. Call the existing restore API for the selected stopped session id.
2. Show success/failure in the flash area.
3. If the restored session is tmux-backed and the current terminal is interactive, attach to its tmux session.
4. When the user detaches from tmux, return to `sm watch --restore` with the list refreshed.
5. If the restored session is headless, stay in restore mode and show a clear message.

No second confirmation is needed for restore because it is reversible by retiring the session again and is less destructive than retire/kill.

## Data Model And API

V1 should use the existing `GET /sessions?include_stopped=true` response and filter client-side to stopped sessions. This keeps the backend simple and avoids another list endpoint unless performance demands one.

The implementation should verify whether the current session payload contains a reliable retired/stopped timestamp. If not, add one durable field to `Session` rather than inferring from unrelated timestamps:

```python
stopped_at: Optional[datetime]
```

`stopped_at` should be set whenever a session transitions to `SessionStatus.STOPPED` through explicit retire/kill, codex-app retirement, completed service-session reap, or monitor-driven dead-session preservation. It should be cleared on successful restore.

If adding `stopped_at` is too invasive for the first implementation, the spec allows a fallback to showing `Retired: -` while still delivering the core browser. However, the recommended implementation is to add the field because the user explicitly wants last retired.

Restore readiness should be derived from cached fields only:

- `status == stopped`
- provider supports restore path
- provider resume id is present where required
- provider is headless or tmux-backed

Do not call live provider commands or tmux probes to render readiness.

## Implementation Plan

1. Add `--restore` and `--top-level` flags to `sm watch` CLI parsing.
2. Split the watch TUI into a reusable mode-aware core where normal mode and restore mode can share rendering, filtering, navigation, search prompt, attach handling, and flash messages.
3. Add restore-mode row construction, either by parameterizing `build_watch_rows` or introducing `build_restore_rows` with shared helpers.
4. Fetch sessions with `include_stopped=true` in restore mode and filter to stopped sessions client-side.
5. Add a restore action that calls the existing `client.restore_session_result(session_id)` path.
6. Reuse `_attach_tmux()` after successful restore for tmux-backed providers, then refresh the restore browser after detach.
7. Add `stopped_at` to `Session` and ensure all STOPPED transitions set it and restore clears it, unless the implementer proves an existing durable timestamp is already correct.
8. Add unit tests for restore row building, filtering, top-level collapse, stopped-only listing, restore action behavior, and `stopped_at` serialization/transitions.
9. Add a small CLI test that `sm watch --restore` dispatches restore mode and that managed sessions are still rejected like normal watch.

## Edge Cases

- Multiple stopped sessions with the same friendly name: the browser avoids ambiguity because selection restores by id.
- Parent is running while child is stopped: show parent context if available, but only stopped rows are selectable/restorable.
- Parent is stopped while child is running: restore mode should show the stopped parent; running child visibility is optional and should not be selectable in v1 if included only as context.
- Session lacks resume id: show it as non-restorable with `no-resume-id`; pressing Enter should show a clear error from the restore API.
- `codex-app`: restore can be attempted through the existing restore API, but attach should be skipped with a headless message.
- Session Manager unavailable: keep the existing watch flash behavior and retry on refresh interval.
- Very large stopped-session history: client-side filtering is acceptable for v1 because `/sessions?include_stopped=true` already exists, but rendering should remain paginated by terminal viewport and avoid per-row live probes.

## Recommended Design Decision

Implement this as a mode of `sm watch`, not as a separate `sm restore --interactive` command.

Reasons:

1. The existing watch TUI already solves navigation, selection, search prompt, tmux attach/return, color, and flash messages.
2. The user asked for `sm watch --restore`, and the mental model is a watch-like browser over Session Manager state.
3. Keeping it under `sm watch` lets future active/restorable toggles share code and keybindings.

A separate `sm restore --interactive` alias can be added later if users want discoverability, but it should call the same restore-mode TUI.

## Acceptance Criteria

1. `sm watch --restore` opens an interactive TUI listing stopped SM sessions from durable state.
2. The UI shows hierarchy/context and restore-relevant columns, including last active and retired/stopped age when available.
3. `/` filtering can find sessions by name, id, role, provider, repo/workdir, aliases, or parent context.
4. `--top-level`, `Tab`, `C`, and `E` provide basic hierarchy collapse/expand controls.
5. Pressing Enter restores the selected stopped session by id through the existing restore API.
6. Tmux-backed restored sessions attach automatically and return to restore-browser mode after detach.
7. Headless restored sessions do not attempt tmux attach and show a clear message.
8. Normal `sm watch` behavior is unchanged.
9. Restore browsing does not run live tmux/provider probes on each refresh.
10. Tests cover row construction, filtering, restore dispatch, stopped timestamp behavior, and CLI argument dispatch.

## Ticket Classification

Single implementation ticket. One agent can implement this without decomposing into an epic if they keep the scope to `sm watch --restore`, cached restore metadata, and existing restore API reuse.
