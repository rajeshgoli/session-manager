# Multi-Node Sessions — One SM, Many Execution Hosts

Ticket: #752 (epic)

## Summary

Introduce a first-class **node** concept so a single Session Manager instance can spawn,
monitor, attach to, and tear down agent sessions that physically run on a **different machine**.

Today SM is single-host: every session runs on the same machine as the SM server. This feature
lets one SM remain the single owner of state, Telegram, and control, while individual sessions
are optionally placed on registered **remote nodes** and run on that node's hardware and
filesystem.

Model:

- **Primary** — the host the SM server runs on. Owns the one server, the one Telegram bot, the
  one state store. This is the default node; nothing changes for sessions placed here.
- **Remote node** — a registered second machine. When a session is explicitly placed on it, the
  agent process and its tmux live on that machine, but the primary's SM still owns it: it appears
  in `sm all`, reports status, is attachable, and is reachable from Telegram exactly like a local
  session.

One brain, multiple execution hosts. Default mode is "everything runs on the primary"; placing a
session on a remote node is an explicit, per-session choice.

**Phase 1 places the Claude provider only.** Remote Codex (codex / codex-fork / codex-app) is
deferred to a follow-up because the codex-fork runtime maintains a local event-stream and control
socket that the SSH-tmux transport does not expose (see Deferred Work). In Phase 1, requesting a
non-primary node for a Codex provider is an explicit validation error, never a silent fallback.

## Live Investigation (grounded code trace)

SM is single-host today. Verified against the code (no speculation):

- **No node concept on the model.** `Session` (`src/models.py`) persists `tmux_session`,
  `tmux_socket_name` (`models.py:297`, `to_dict` at `:394`, `from_dict` at `:492`) but has no
  `host`/`node` field. State lives in `~/.local/share/claude-sessions/sessions.json`.
- **One process-wide tmux controller.** `SessionManager` holds a single
  `self.tmux = TmuxController(...)` (`session_manager.py:164`); most control paths pass only a
  tmux session *name*, not a `Session`/node — e.g. `output_monitor.session_exists`
  (`output_monitor.py:261-280`), direct sends (`session_manager.py:5322-5338`), clear/recovery
  (`:6251-6297`, `:7007-7156`), message-queue tmux helpers (`message_queue.py:154-162`,
  `:2274-2330`, `:2691-2731`, `:4212-4370`), pane capture/activity (`:6049-6058`, `:6541-6563`),
  kill/restore (`:6339-6392`, `:6405-6471`), and CLI/watch attach/clear (`cli/commands.py:36-82`,
  `:3032-3067`, `:4868-4898`; `cli/watch_tui.py:1192-1233`).
- **All tmux control is local.** `TmuxController` shells out via `subprocess.run` /
  `asyncio.create_subprocess_exec`; sync funnel `_run_tmux()` (`tmux_controller.py:367-383`,
  `:377`), spawn `create_session_with_command()` (`:1009-1137`), `-L <socket>` override exists
  (`:86-93`, `:27-28`).
- **codex-fork has a local event/control plane.** CLI `sm codex` and `sm spawn codex` both map to
  the **codex-fork** runtime (`cli/main.py:549-558`, `:1184-1186`). It writes an event-stream
  JSONL and a Unix control socket under `log_dir` (`session_manager.py:1077-1083`, `:1155-1170`),
  tails the stream locally (`:2516-2597`), and sends control via `asyncio.open_unix_connection`
  (`:5105-5175`). These are local files/sockets on the primary.
- **Hook callbacks are the critical blocker.** Agents POST to
  `${SM_HOOK_URL:-http://localhost:8420/hooks/claude}` (`notify_server.sh:6`). On a remote node
  the default hits that machine's own loopback. SM exports `CLAUDE_SESSION_MANAGER_ID` into the
  session shell via send-keys (`tmux_controller.py:536-543`) but does not set `SM_HOOK_URL`.
- **Hook endpoint trusts the payload.** `POST /hooks/claude` mutates session state from
  payload `session_manager_id` / `CLAUDE_SESSION_MANAGER_ID` with no hook-level auth
  (`server.py:6859-7063`).
- **Transcript read assumes local FS, with retries.** The handler reads `transcript_path` from
  disk (`server.py:6868-6991`), retrying empty reads after `EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS`
  = 0.5s and stale reads after `TRANSCRIPT_RETRY_DELAY_SECONDS` = 0.3s (`server.py:75-78`). For a
  remote session that path is on the node.
- **Status polling + log locality.** `output_monitor.py` polls `has-session` and exit diagnostics
  (`:257-292`) on a ~1s loop and tails the session's **local** log file (`:294-360`); the log is
  produced by `pipe-pane` to a local path (`tmux_controller.py:980`, dir/file created locally at
  `:1042-1060`).
- **Attach assumes local tmux.** The WS terminal bridge Popens local `tmux attach-session`
  (`server.py:4835-4963`); an existing remote-attach helper already uses `ssh -tt` + a quoted
  remote shell script (`server.py:3065-3096`).
- **Persisted status is a closed enum.** `SessionStatus` = `running` / `idle` / `stopped`
  (`models.py:10-15`); state load constructs `SessionStatus(mapped_status)` directly
  (`models.py:477-495`).
- **Many creation surfaces.** Beyond `POST /sessions` / `/sessions/create`, creation also flows
  through `/sessions/spawn` (`server.py:7291-7335`, `SpawnChildRequest` `:1063-1070`),
  `/sessions/review` (`:7434+`), and CLI `sm spawn` / `sm claude` / `sm codex` / `sm new` /
  `sm dispatch` / `sm review --new` / `sm watch` create / fork / role bootstrap.
- **Config** is a flat YAML loaded by `load_config()` (`main.py:194-209`); a `nodes:` section
  slots in next to `external_access`.

## Problem

There is no way for one SM to run a session on a different physical machine while still owning it.
The only way to use a second machine's hardware today is a second, independent SM instance with
its own tmux, state, and (conflicting) Telegram bot — invisible to the first SM and breaking the
single-brain model.

## Goals

1. A `Session` carries a **node identity**; `sm all` and the watch TUI show which node each
   session runs on.
2. SM can **spawn a Claude session** on a registered remote node, running on that node's CPU/GPU
   and filesystem. (Remote Codex is deferred — see Deferred Work.)
3. **Status, hooks, last-message/title, and completion** work for remote Claude sessions
   identically to local ones, including the Stop-hook retry/staleness semantics.
4. **Attach** (CLI and web/mobile terminal) works against a remote session.
5. **Teardown / kill / restart** route to the correct node.
6. Default behavior is **unchanged**: omitting a node places the session on the primary host. Zero
   behavior change for existing single-host users.
7. Requesting a non-primary node with an unsupported provider is an **explicit rejection**,
   whether the node was passed directly or inherited from a parent — never a silent primary
   fallback.
8. **Node liveness** is tracked; a session on an unreachable node is shown as such rather than
   silently treated as dead.

## Non-Goals

- Automatic failover / live migration of a running session between nodes.
- Cross-node load balancing or scheduling. Placement is an explicit per-session choice.
- Sharing a single working tree across machines. Code syncs via git; each node has its own
  checkout (a consistent `projects_root` keeps paths aligned).
- A specific node count.

## Deferred Work

- **Remote Codex placement (codex / codex-fork / codex-app).** The codex-fork runtime keeps a
  local event-stream JSONL and Unix control socket under `log_dir` (`session_manager.py:1077-1170`),
  tailed and controlled locally (`open_unix_connection`, `:5105-5175`). The SSH-tmux + pipe-pane
  transport in this epic does not expose those, so a remote codex-fork session would be only
  half-managed (no reliable lifecycle/status, last-message, completion, structured control sends,
  fork confirmation, artifact maintenance, restore). A follow-up sub-ticket must design remote
  transport for the codex-fork event stream and control socket before remote Codex is enabled.
  Until then, a non-primary node for any Codex provider is rejected (Goal 7).

## User Experience

Default — unchanged (current, non-deprecated commands):

```bash
sm claude ~/projects/foo               # runs on the primary host
sm dispatch engineer1 --repo ~/projects/foo
```

Explicit remote placement (Claude only in Phase 1):

```bash
sm claude ~/projects/foo --node worker            # agent runs on the "worker" node
sm dispatch trainer --repo ~/projects/app --node worker
```

Rejected in Phase 1 — directly or via inheritance:

```bash
sm codex ~/projects/ml --node worker
# error: remote placement is Claude-only in this phase (provider=codex-fork)

# A Claude session already on "worker" that runs `sm spawn codex` inherits node=worker
# and is rejected the same way — it is NOT silently created on primary.
```

`sm all` gains a node column:

```
ID        ROLE       PROVIDER  NODE      STATUS
a4af4272  engineer1  claude    primary   working
7c1f90ab  builder    claude    worker    waiting-input
```

Attach is node-transparent: `sm attach 7c1f90ab` routes to the worker node's tmux.

## Recommended Design

**Transport: SSH-routed tmux with a persistent multiplexed control connection.** Every tmux/
control invocation flows through a new `NodeRunner`. For the primary it runs locally (today's
behavior). For a remote node it runs over SSH on a long-lived **ControlMaster** socket so per-op
latency is ~10–30ms (vs 200–500ms cold). Chosen over a custom node-agent daemon (see Alternatives)
because it reuses existing tmux command construction and leans on SSH for transport + auth, and
ControlMaster removes the latency objection.

### Per-session node resolution (addresses singleton `tmux`)

`TmuxController` is process-wide (`session_manager.py:164`) and cannot be "bound" to one node.
Instead, **node is resolved per operation**:

- `TmuxController` receives a `NodeRunner` plus a resolver callback `tmux_session -> node`
  (backed by `SessionManager`'s session map), and every internal tmux call passes the resolved
  node to `NodeRunner`.
- Control entrypoints that today take only a tmux session name are updated to resolve/forward the
  node. The full must-route set is in scope for the routing sub-ticket: `output_monitor`
  existence/diagnostics, direct sends, clear/recovery, message-queue tmux helpers, pane
  capture/activity, kill/restore, and CLI/watch attach/clear (file:line list in Live
  Investigation). Acceptance for that step: send/status/kill/attach can never target the primary
  while spawn landed on a remote node.

### NodeRunner API (correct SSH shapes)

Three distinct methods — non-interactive execution must not share the interactive path:

- `run(node, argv)` / `run_async(node, argv)` — non-interactive. Primary: exec `argv` directly.
  Remote: build `remote = shlex.join(argv)` and invoke
  `ssh <ctl-opts> <userhost> /bin/sh -lc <remote>` (the remote command is a single quoted arg;
  no `--` after the destination, since OpenSSH treats post-destination tokens as the command).
- `attach(node, tmux_session)` — interactive PTY. Primary: today's `Popen` of `tmux attach` into a
  PTY. Remote: `ssh -tt <ctl-opts> <userhost>` running the remote `tmux attach`, allocating a
  remote PTY, mirroring the existing remote-attach helper (`server.py:3065-3096`); the WS/mobile
  bridge `Popen` (`server.py:4835-4963`) becomes node-aware via this method.
- ControlMaster lifecycle (`-o ControlMaster=auto -o ControlPersist=...`, `-S <control_path>`) and
  a liveness probe (`ssh <node> true`) live in `NodeRunner`; stale sockets re-establish
  transparently.

### Hook + transcript parity (remote)

1. On remote spawn, the managed-shell export step (`tmux_controller.py:536`) also exports
   `SM_HOOK_URL` = `nodes.<id>.hook_url` (a primary address the node can reach). The remote agent
   POSTs hooks straight to the primary.
2. `notify_server.sh`, when running on a node (i.e. `SM_HOOK_URL` is remote), extracts the last
   assistant message + native title from the **local** transcript and inlines them in the POST,
   **replicating the server's retry semantics**: retry empty reads after 0.5s
   (`EMPTY_TRANSCRIPT_RETRY_DELAY_SECONDS`) and stale reads after 0.3s
   (`TRANSCRIPT_RETRY_DELAY_SECONDS`). The server prefers inlined fields when present and only
   falls back to a local `transcript_path` read for the primary node — preserving final-response
   correctness for remote sessions.

### Provider/node validation gate

At the creation boundary, after node resolution (including inheritance), if `node != "primary"`
and `provider` is not `claude`, reject with `400` and an explicit message. Applied uniformly to
codex / codex-fork / codex-app and to every creation surface (see API & CLI).

### Output streaming

`pipe-pane` continues to write the log on the node. For remote sessions the monitor consumes a
single long-lived `ssh <node> tail -F <logpath>` stream per active session instead of reading a
local file (`output_monitor.py:294`).

## Locality Inventory

For remote sessions, classify each currently-local preflight/runtime op:

| Operation (file:line) | Phase 1 disposition |
|---|---|
| CLI working-dir validation (`cli/commands.py:2907-2923`, `:2991-3005`; `watch_tui.py:1104-1145`) | Stop blocking locally for remote; server-side preflight on target node (`ssh node test -d <dir>`), clear error if missing |
| Git remote detection (`session_manager.py:2673-2674`) | Node-executed for remote (describes the node's checkout) |
| Log dir/file creation (`tmux_controller.py:1042-1060`) | Node-executed (logs live on node) |
| Claude resume/title discovery (`session_manager.py:1697-1724`, `:2374-2470`, `:3784-3997`) | Node-executed for remote Claude |
| Review git checks (`session_manager.py:6663-6685`) | Primary-only; `sm review` stays primary in Phase 1 |
| codex-fork artifact/control (`session_manager.py:1077-1170`, `:5105-5175`) | Not applicable — Codex remote deferred |

## Data Model

`Session` gains:

```python
node: str = "primary"        # node id; "primary" = the host SM runs on
```

- `models.py`: add field (near `:297`), serialize in `to_dict` (`:394`), parse in `from_dict`
  (`:492`, missing → `"primary"` for legacy `sessions.json`).
- Set in `_create_session_common` (`session_manager.py:~2741`) from the resolved node.
- **Children inherit the parent's node** by default; an explicit `--node` overrides. Inheritance
  happens before the validation gate, so an inherited unsupported combination is rejected, not
  silently downgraded.

`SessionStatus` is unchanged (`running`/`idle`/`stopped`). **`node-unreachable` is a computed
overlay** (activity/error state surfaced in `sm all`/TUI/Telegram), not a persisted status — no
enum/migration change. During a node outage the output monitor must **not** call
`_handle_session_died`/cleanup (`output_monitor.py:257-292`, `:880-901`); it marks the overlay and
backs off, then re-probes `has-session` and reconciles on reconnect (the remote tmux persists
independent of SSH).

Config — new `nodes:` registry (loaded in `main.py`, passed to `SessionManager`/`NodeRunner`):

```yaml
nodes:
  default: primary
  registry:
    primary: {}
    worker:
      ssh: "user@worker.local"
      ssh_proxy_command: "cloudflared access ssh --hostname %h"   # optional
      control_path: "~/.ssh/cm-sm-worker.sock"
      hook_url: "http://primary.local:8420/hooks/claude"
      hook_secret: "<per-node shared secret>"   # see Security
      projects_root: "~/projects"
```

## API & CLI Changes

- **`node` defaults to `"primary"` on every creation path**, so unlisted/legacy surfaces are
  unaffected: `POST /sessions`, `/sessions/create`, `/sessions/spawn` (`SpawnChildRequest`),
  `/sessions/review`, and CLI `sm claude` / `sm codex` / `sm new` / `sm spawn` / `sm dispatch` /
  `sm review --new` / `sm watch` create / fork / role bootstrap.
- **Phase 1 exposes `--node`** on `sm claude`, `sm spawn`, and `sm dispatch`. Children inherit the
  parent node via `SpawnChildRequest`. Other surfaces accept only `primary` (default).
- **Validation:** resolved node (post-inheritance) `!= primary` with a non-Claude provider →
  `400`, uniformly for codex / codex-fork / codex-app.
- `SessionResponse` includes `node`; `sm all` and `cli/watch_tui.py` show a node column.
- New: `sm nodes` (registry + liveness), `sm node ping <id>`.

## Security

- Remote control is plain SSH (key auth, optional authenticated proxy); no new inbound port beyond
  sshd.
- **Hook authenticity:** `/hooks/claude` currently trusts payload `session_manager_id` with no
  auth (`server.py:6859-7063`). Once it is reachable from remote nodes, add an optional **per-node
  shared secret** (`nodes.<id>.hook_secret`) sent as a header by `notify_server.sh` and verified
  by the endpoint for remote-originated hooks. Trusted-path (LAN/tunnel) remains the baseline; the
  secret closes the `session_manager_id` spoofing gap.
- Validate `node` strictly against the registry; never interpolate an arbitrary host into `ssh`.

## Alternatives Considered

- **Custom node-agent daemon** on the remote machine (HTTP/WS spawn/send/capture, streaming events
  back). Cleaner for log locality and hook forwarding, but a substantial new service (auth,
  lifecycle, deploy, version lock-step) that re-implements what SSH + ControlMaster provide.
  Rejected for Phase 1; revisit if SSH transport proves limiting. (Also the natural home for the
  deferred codex-fork event/control transport.)
- **Bind the SM server to the LAN + run a 2nd SM** — rejected; two brains, reintroduces Telegram
  409.

## Cutover / Migration

Behavior-additive: a single-host deployment keeps working untouched. This runbook is for operators
demoting a previously standalone SM machine into a remote node of a chosen primary.

Invariants:

- **Exactly one primary.** Only the primary runs the SM server, the Telegram bot, and the state
  store. The bot is single-poller per token — two pollers cause a hard `409 Conflict`. A demoted
  machine must stop its server and bot.
- **State does not merge.** Sessions live in the SM that created them; demotion does not import a
  machine's prior local sessions into the primary.

Demote a standalone install to a node:

1. Stop that machine's SM server and Telegram bot; let existing local sessions drain.
2. Keep tmux + the SM-owned socket; ensure `notify_server.sh` is installed. The primary injects
   `SM_HOOK_URL` (and the per-node hook secret) at spawn, so the node needs no static hook config.
3. Point that machine's `sm` CLI at the primary (`SM_API_URL=http://<primary>:8420`).
4. Register the node in the primary's `nodes:` registry and confirm `sm node ping <id>` is green.
5. The machine is now reachable two ways: as a thin client (its CLI drives the primary) and as an
   execution target (`--node`).

Reverse / disconnected fallback: a node may temporarily run its **own** local SM (server, **bot
disabled**, `SM_API_URL` → localhost) when cut off; those sessions are owned locally, invisible to
the primary, reconciled via git on reconnect. Explicit operator mode, never automatic.

## Implementation Plan (maps to sub-tickets)

1. **Node model + config registry + CLI surfacing + validation gate** — `Session.node`, `nodes:`
   config, `--node` on `sm claude`/`sm spawn`/`sm dispatch`, inheritance, the provider/node
   rejection (direct + inherited), `sm all`/TUI column, `sm nodes`/`sm node ping`. Primary-only
   execution; zero behavior change for primary.
2. **`NodeRunner` + per-session routing** — `run`/`run_async`/`attach`, ControlMaster + liveness;
   route every must-route control site through resolved node. Spawn a remote Claude session and
   confirm send/status/kill/attach all target the node.
3. **Hook + transcript parity** — export `SM_HOOK_URL` + hook secret on remote spawn; node-side
   `notify_server.sh` extraction replicating the 0.5s/0.3s retries; server prefers inline.
4. **Remote output streaming** — persistent `ssh tail -F` per remote session.
5. **Remote attach** — CLI + WS/mobile bridge via `attach()` / `ssh -tt`.
6. **Node liveness overlay + failure UX + cutover docs** — `node-unreachable` overlay (no
   `_handle_session_died` during outages), reconnect/backoff, migration runbook.
7. **(Deferred sub-ticket) Remote Codex transport** — remote event-stream + control-socket
   transport for codex-fork; only then lift the Codex rejection.

## Tests

- Unit: `NodeRunner` argv construction (primary vs `ssh ... sh -lc <shlex.join>`; attach uses
  `-tt`), registry validation, `Session.node` round-trip incl. legacy state defaulting to
  `primary`.
- **Validation:** codex / codex-fork / codex-app with a non-primary node → `400`, both passed
  directly and inherited from a parent; never silently created on primary.
- Integration (local "fake remote" via `ssh localhost`): spawn a remote **Claude** session; assert
  it appears, status transitions fire from hooks, last-message/title populate (incl. the retry
  path), attach connects, kill tears down the remote tmux.
- Regression: full existing suite with `node="primary"` — zero behavior change.
- Liveness: node down → sessions show `node-unreachable` (not `dead`, no cleanup); node returns →
  recovers.
- Remote **Codex** acceptance is owned by the deferred sub-ticket, not Phase 1.

## Edge Cases

- Legacy `sessions.json` without `node` → `primary`.
- Node unreachable mid-session: remote tmux keeps running; SM shows `node-unreachable`; recovers on
  reconnect.
- ControlMaster socket stale/dead → transparently re-establish.
- Path skew: validate `working_dir` exists on the target node before spawn; clear error if not.
- Inherited node + unsupported provider → rejected at the gate (no silent downgrade).

## Acceptance Criteria

- `sm claude --node worker` runs the agent on the worker node; `sm all` shows `node=worker`.
- Remote Claude reaches `working`/`waiting-input`/`completed` from real hook events; last message
  and native title populate, including the empty/stale retry path.
- `sm attach` connects to a remote session's live tmux; `sm kill` removes the remote tmux.
- codex / codex-fork / codex-app with a non-primary node (direct or inherited) is rejected with an
  explicit message and is never created on the primary.
- Omitting `--node` is byte-for-byte current behavior; existing tests pass unmodified.
- A session on a downed node is reported as `node-unreachable`, not lost, and is not cleaned up.
- An operator can demote a standalone install to a node per the Cutover runbook without the primary
  ever running two bots.

## Ticket Classification

**Epic.** Spans the data model, a new transport abstraction with per-session routing, the tmux
controller, the hook/transcript pipeline, output streaming, attach, liveness, validation, and
cutover — plus a deferred remote-Codex transport sub-ticket. Cannot be completed by one agent
without compacting context. Sub-tickets follow the seven-step Implementation Plan; step 1 lands
behavior-neutral. File sub-tickets referencing this spec before implementation.
