# 1130 — Session activity detection: hooks are the source of truth

Tracking issue: #1130. Continues #1117 / #1119 / #1121.

## Problem

`activity_state` was derived by scraping the tmux pane on **every** dashboard
read, and the scraper both missed idle and manufactured active:

1. **Stuck active while idle.** `claude_line_indicates_completed` matched an
   allowlist of exactly three completion verbs (`Brewed for`, `Baked for`,
   `Churned for`). Claude cycles through many more (`Crunched`, `Cooked`,
   `Simmered`, `Sautéed`, …), so a turn ending on any other verb classified as
   neither completed nor working, the overlay returned `None`, and a stale
   active status persisted.
2. **False active from background work.** `✻ Baked for 24m · 1 shell still
   running` was classified `working`, flipping a correct hook-derived idle back
   to active even though the agent's turn was done.

Root cause: the `Stop` hook is authoritative when it arrives but is lossy
(server restarts, event-loop stalls, watchdog kills), and the scraper that
backstopped it was too weak to re-derive the truth.

## Design

**Hooks bracket the turn.** `UserPromptSubmit` marks the session `running` at
turn start; `Stop` marks it `idle` at turn end. Both write a durable
`activity_hook_at` timestamp to the session store alongside the status, so
active/idle no longer depends on spinners or completion verbs.

`claude_hook_gate()` (`sessions.rs`) turns the stored state into one of:

| Gate          | Meaning                                            | Pane consulted?              |
| ------------- | -------------------------------------------------- | ---------------------------- |
| `TurnRunning` | turn in flight, hook signal fresh (< 180s)          | no — overlays `working` outright |
| `TurnStopped` | `Stop` fired, no turn started since                 | background-work signal only  |
| `Stale`       | turn reported in flight but the signal aged out     | yes — last resort            |
| `Untracked`   | no lifecycle hook ever seen for this session        | yes — legacy behaviour        |

`Untracked` keeps sessions on nodes without hook wiring working exactly as
before, so this is a safe rollout rather than a flag day.

`TurnRunning` states `working` outright rather than deferring to the default
projection. `projected_activity_state` calls a `running` session idle once
`last_activity` is 30s old, and `last_activity` is only refreshed by hooks — so
a long tool-free response (no `PreToolUse` fires) would otherwise read idle from
second 30 through second 180, exactly the window the fresh hook is supposed to
cover.

`TurnStopped` additionally requires `activity_turn_start_hook_at` — evidence
that a `UserPromptSubmit` has actually been observed for this session. A stored
idle is only conclusive when the *start* of a turn is observable too. Sessions
run in arbitrary working directories and never load this repo's project-scoped
settings, so a session with only the global `Stop` hook would otherwise be
pinned to idle for every later turn, right through a tool-free response. Without
that evidence the gate falls through to `Stale` and the pane stays in play.

## Hook ordering

`hooks/notify_server.sh` dispatches every lifecycle hook through a detached
curl, and the `Stop` handler may sleep on its transcript retry before applying
state. A `Stop` received before the next prompt can therefore land *after* that
prompt's `UserPromptSubmit`. The handler stamps `received_at` when the request
arrives, before the retry sleep; `apply_claude_stop_hook` skips the turn
transition when the stored `activity_hook_at` is newer than that stamp. The
transcript metadata the superseded `Stop` carries is still the freshest
available, so it is applied either way.

**The fallback no longer lies.** Completion detection is structural instead of
an allowlist: a status glyph, one capitalised verb, `for`, and a duration, with
no `…` spinner and no `(esc to interrupt)`. Todo glyphs (`✔`, `✗`, `❯`, …) share
the dingbat block with the status glyph and are rejected explicitly, so a todo
entry like `✔ Waited for 30s` cannot end a turn.

Background work no longer implies `working`. It is read only from the newest
status line and the chrome below it — older status lines scroll up with stale
`N shell still running` segments attached, and counting those would resurrect
long-finished background work.

**New `waiting` state.**

- `working` — agent is generating (between `UserPromptSubmit` and `Stop`).
- `waiting` — `Stop` fired but background shells/monitors are still running; the
  agent is parked and will be re-invoked when they finish.
- `idle` — stopped, nothing pending.

Background work may only ever downgrade `idle → waiting`. It can never upgrade
to `working` — that was bug 2.

## Wiring

`scripts/install_notify_server_hook.sh` registers both `UserPromptSubmit` and
`Stop` in `~/.claude/settings.json` (merging into whatever is already there, and
idempotent on re-run). That user-level registration is what covers sessions in
arbitrary working directories; this repo's `.claude/settings.json` carries the
same two entries so the repo is self-contained.

Both route to `hooks/notify_server.sh`, which posts to `/hooks/claude`. The
script skips its transcript `tail | jq` pass for `UserPromptSubmit` (no
transcript payload there, and it runs before every single turn).

## Clients

`waiting` renders as `bg-wait` in violet, distinct from the amber `waiting` used
for `waiting_input` / `waiting_permission`:

- web `sm-watch`: `stateLabel()` / `activityTone()`
- Android watch screen: `activityLabel()` / `activityTint()`
