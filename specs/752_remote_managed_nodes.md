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

## Live Investigation (grounded code trace)

SM is single-host today. Verified against the code (no speculation):

- **No node concept on the model.** `Session` (`src/models.py`) persists `tmux_session`,
  `tmux_socket_name` (`models.py:297`, `to_dict` at `:394`, `from_dict` at `:492`) but has no
  `host`/`node` field. State lives in `~/.local/share/claude-sessions/sessions.json`.
- **All tmux control is local.** `TmuxController` shells out via `subprocess.run` /
  `asyncio.create_subprocess_exec`. The sync funnel is `_run_tmux()`
  (`tmux_controller.py:367-383`, `subprocess.run` at `:377`); session creation is
  `create_session_with_command()` (`:1009-1137`); a `-L <socket>` override already exists
  (`tmux_cmd()` at `:86-93`, `socket_name` read at `:27-28`).
- **Spawn flow.** `POST /sessions` (`server.py:4368-4398`) → `SessionManager.create_session()`
  (`session_manager.py:2936`) → `_create_session_common()` (`:2611-2835`) →
  `tmux.create_session_with_command(...)` (`:2723-2732`). `working_dir` flows unchanged
  (`:2725-2726`).
- **Hook callbacks are the critical blocker.** Agents report status via
  `hooks/notify_server.sh`, which POSTs to `${SM_HOOK_URL:-http://localhost:8420/hooks/claude}`
  (`notify_server.sh:6`). On a remote node this default hits that machine's own loopback, not the
  primary. SM injects `CLAUDE_SESSION_MANAGER_ID` into the session shell via send-keys
  (`tmux_controller.py:536-543`) but does **not** set `SM_HOOK_URL` today.
- **Transcript read assumes local FS.** The hook handler `POST /hooks/claude`
  (`server.py:6841-7068`) reads `transcript_path` from disk (`:6869-6927`) to extract the last
  message / native title. For a remote session that path is on the remote node.
- **Status polling + log locality.** `output_monitor.py` polls `has-session` and exit
  diagnostics (`:262-281`) on a ~1s loop and tails the session's **local** log file
  (`_read_new_log_content` at `:294-299`); the log is produced by `pipe-pane` to a local path
  (`tmux_controller.py:980`). For a remote session both the log and the pane live on the node.
- **Attach assumes local tmux.** The WS terminal bridge Popens `tmux attach-session -t <name>`
  locally (`server.py:4950-4956`); CLI attach uses the same local tmux.
- **Config** is a flat YAML loaded by `load_config()` (`main.py:194-209`); a `nodes:` section
  slots in naturally next to `external_access`.

## Problem

There is no way for one SM to run a session on a different physical machine while still owning it.
The only way to use a second machine's hardware today is a second, independent SM instance with
its own tmux, state, and (conflicting) Telegram bot — invisible to the first SM and breaking the
single-brain model.

## Goals

1. A `Session` carries a **node identity**; `sm all` and the watch TUI show which node each
   session runs on.
2. SM can **spawn** a session on a registered remote node (Claude and Codex providers) and the
   agent runs on that node's CPU/GPU, in that node's filesystem.
3. **Status, hooks, last-message/title, and completion** work for remote sessions identically to
   local ones.
4. **Attach** (CLI and web/mobile terminal) works against a remote session.
5. **Teardown / kill / restart** route to the correct node.
6. Default behavior is **unchanged**: omitting a node places the session on the primary host. Zero
   behavior change for existing single-host users.
7. **Node liveness** is tracked; a session on an unreachable node is shown as such rather than
   silently treated as dead.

## Non-Goals

- Automatic failover / live migration of a running session between nodes (state can't migrate;
  out of scope).
- Cross-node load balancing or scheduling. Node placement is an explicit per-session choice.
- Sharing a single working tree across machines. Code syncs via git; each node has its own
  checkout (a consistent `projects_root` keeps paths aligned).
- A specific node count. Two is the common case but nothing is hard-coded to it.

## User Experience

Default — unchanged:

```bash
sm new ~/projects/foo                 # runs on the primary host
sm dispatch engineer1 --repo ~/projects/foo
```

Explicit remote placement:

```bash
sm new ~/projects/foo --node worker   # agent runs on the registered "worker" node
sm dispatch trainer --repo ~/projects/ml --node worker --provider codex
```

`sm all` gains a node column:

```
ID        ROLE       PROVIDER  NODE      STATUS
a4af4272  engineer1  claude    primary   working
7c1f90ab  trainer    codex     worker    waiting-input
```

Attach is node-transparent:

```bash
sm attach 7c1f90ab    # SM routes the attach to the worker node's tmux
```

From Telegram, a remote session is indistinguishable from a local one.

## Recommended Design

**Transport: SSH-routed tmux with a persistent multiplexed control connection.** Introduce a
single `NodeRunner` abstraction that every tmux invocation flows through. For the primary node it
runs the command directly (today's behavior). For a remote node it prefixes the command with
`ssh <node>` over a long-lived **SSH ControlMaster** socket, which amortizes connection setup so
per-op latency drops to ~10–30ms (vs 200–500ms cold).

Why SSH-routed over a custom node-agent daemon (see Alternatives): it reuses the existing tmux
command construction almost verbatim, leans on SSH for transport + auth, and needs no new
long-running service to build, deploy, and version-lock against SM. ControlMaster neutralizes the
latency objection that would otherwise favor a daemon.

Core changes:

1. **`NodeRunner`** (new, `src/node_runner.py`): `run(argv, ...)` and async `run_async(argv, ...)`
   that, given a node id, either exec locally or wrap in `ssh -S <controlpath> <userhost> -- <argv>`.
   Owns ControlMaster lifecycle (`-o ControlMaster=auto -o ControlPersist=...`) and a liveness
   probe (`ssh <node> true`).
2. **`TmuxController` takes a `NodeRunner` + node id.** Replace the direct `subprocess.run` at
   `_run_tmux` (`:377`), the async spawn (`:1009-1137`), and the attach Popen
   (`server.py:4950`) with calls through `NodeRunner`. tmux's own `-L <socket>` handling is
   unchanged and runs on whichever host the command lands.
3. **Per-session node, threaded through spawn.** `node` flows from the API → `create_session` →
   `_create_session_common` → `create_session_with_command`, defaulting to `"primary"`.
4. **Hook callback fix (critical).** When spawning on a remote node, the managed-shell export
   step (`tmux_controller.py:536`) also exports `SM_HOOK_URL` pointing at the primary's
   SM-reachable address (from `nodes.<id>.hook_url`). The remote agent then POSTs hooks straight
   to the primary.
5. **Transcript locality fix.** For remote sessions, `notify_server.sh` extracts the last
   assistant message + native title from the local transcript and includes them inline in the
   POST body, so `POST /hooks/claude` does not need to read a remote file. The server prefers the
   inlined fields when present and only falls back to local `transcript_path` reads for the
   primary node. (Backward compatible: primary sessions keep current behavior.)
6. **Output streaming.** `pipe-pane` continues to write the log on the node. For remote sessions
   the monitor consumes a single long-lived `ssh <node> tail -F <logpath>` stream per active
   session instead of reading a local file (`output_monitor.py:294`). One persistent stream, not
   per-poll SSH calls.
7. **Node liveness.** A background probe marks nodes up/down; sessions on a down node surface a
   distinct `node-unreachable` status rather than `dead`.

## Data Model

`Session` gains:

```python
node: str = "primary"        # node id; "primary" = the host SM runs on
```

- `models.py`: add field (near `:297`), serialize in `to_dict` (`:394`), parse in `from_dict`
  (`:492`, defaulting missing → `"primary"` for backward compat with existing `sessions.json`).
- Set in `_create_session_common` (`session_manager.py:~2741`) from the request, default
  `"primary"`.

Config — new `nodes:` registry (loaded in `main.py`, passed to `SessionManager` and
`NodeRunner`):

```yaml
nodes:
  default: primary
  registry:
    primary: {}                                # the host SM runs on; no SSH
    worker:
      ssh: "user@worker.local"                 # or an authenticated proxy host
      ssh_proxy_command: "cloudflared access ssh --hostname %h"   # optional
      control_path: "~/.ssh/cm-sm-worker.sock"
      hook_url: "http://primary.local:8420/hooks/claude"  # primary addr the node can reach
      projects_root: "~/projects"              # for path validation/sanity
```

## API & CLI Changes

- `CreateSessionRequest` (`server.py`) gains optional `node: str = "primary"`; validated against
  the registry (unknown node → 400).
- `POST /sessions`, `POST /sessions/create`, and the dispatch path accept and forward `node`.
- CLI: `--node <id>` on `sm new` and `sm dispatch`; node column in `sm all` and the watch TUI
  (`src/cli/watch_tui.py`). `SessionResponse` includes `node`.
- New: `sm nodes` (list registry + liveness), `sm node ping <id>`.

## Cutover / Migration

This is a behavior-additive feature, so a single-host deployment keeps working untouched. The
migration story is for operators who want to convert a previously standalone SM machine into a
remote node of a chosen primary.

Invariants:

- **Exactly one primary.** Only the primary runs the SM server, the Telegram bot, and the state
  store. The Telegram bot is single-poller per token — two pollers cause a hard `409 Conflict`.
  A machine demoted to a node must stop its server and bot.
- **State does not merge across instances.** Sessions live in the SM that created them. Converting
  a standalone install to a node does **not** import its prior local sessions into the primary.

Convert a standalone install into a remote node:

1. Stop that machine's SM server and Telegram bot. Let its existing local sessions drain or accept
   that they will no longer be managed.
2. Keep tmux and the SM-owned tmux socket; ensure `notify_server.sh` is installed on the node. The
   primary injects `SM_HOOK_URL` at spawn, so the node needs no static hook config.
3. Point that machine's `sm` CLI at the primary (`SM_API_URL=http://<primary>:8420`).
4. Register the node in the primary's `nodes:` registry (`ssh`, `control_path`, `hook_url`,
   `projects_root`) and confirm `sm node ping <id>` is green.
5. From then on, that machine is reachable two ways: as a thin client (its CLI drives the
   primary) and as an execution target (the primary can place sessions on it via `--node`).

Reverse / disconnected fallback:

- A node may temporarily run its **own** local SM (server, **bot disabled**, `SM_API_URL` →
  localhost) when cut off from the primary. Sessions created in that mode are owned by the local
  instance and are **not** visible to the primary; reconcile work via git on reconnect. This is an
  explicit operator-invoked mode, never automatic, precisely because state cannot merge.

Rollout ordering: ship the node model + CLI surfacing first (behavior-neutral), then remote
execution. An operator can register nodes and see the column before any session is ever placed
remotely, de-risking the migration.

## Security

- Remote control is plain SSH; rely on key auth and, where used, an authenticated SSH proxy. No
  new inbound port on the node beyond sshd.
- `SM_HOOK_URL` for a remote node points at the primary over a trusted path (LAN or authenticated
  tunnel). Document that exposing `/hooks/claude` must stay behind that path.
- Validate `node` strictly against the registry; never interpolate an arbitrary host into `ssh`.

## Alternatives Considered

- **Custom node-agent daemon on the remote machine** (HTTP/WS service the primary calls to spawn/
  send/capture, streaming events back). Architecturally clean — solves log locality by streaming
  and lets hooks POST to localhost-then-forward. Rejected for Phase 1: it's a substantial new
  service (auth, lifecycle, deploy, version lock-step with SM) that mostly re-implements what SSH +
  ControlMaster already provide. Revisit if SSH transport proves limiting (high-frequency control,
  or many nodes).
- **Bind the SM server to the LAN and run a 2nd SM on the other machine** — rejected; that's two
  brains, not one, and reintroduces the Telegram 409 problem.

## Implementation Plan (maps to sub-tickets)

1. **Node model + config registry + CLI surfacing** — add `Session.node`, `nodes:` config,
   `--node` flag, `sm all`/TUI column, `sm nodes`/`sm node ping`. Primary-only still; no remote
   execution yet. (De-risks the data/UX layer with zero behavior change.)
2. **`NodeRunner` + TmuxController routing** — introduce `NodeRunner`, route `_run_tmux`, async
   spawn, and attach through it; ControlMaster lifecycle + liveness probe. Validate by spawning a
   remote session and confirming it appears and stays alive.
3. **Hook + transcript path for remote** — export `SM_HOOK_URL` on remote spawn; inline
   last-message/title in `notify_server.sh`; server prefers inlined fields. Validate remote
   status/title/completion parity.
4. **Remote output streaming** — persistent `ssh tail -F` per remote session in the monitor.
5. **Remote attach** — route CLI and WS/mobile bridge attach through `NodeRunner`.
6. **Node liveness + failure UX + cutover docs** — `node-unreachable` status, reconnect/backoff,
   and the operator migration runbook from the Cutover section.

## Tests

- Unit: `NodeRunner` argv construction (primary vs ssh), registry validation, `Session.node`
  round-trip in `to_dict`/`from_dict` incl. legacy state without the field.
- Integration (local "fake remote" via `ssh localhost`): spawn on a remote node, assert session
  appears, status transitions fire from hooks, last-message/title populate, attach connects,
  kill tears down the remote tmux.
- Regression: full existing suite with `node="primary"` — zero behavior change.
- Liveness: node down → sessions show `node-unreachable`; node returns → recovers.

## Edge Cases

- Legacy `sessions.json` without `node` → treated as `primary`.
- Node unreachable mid-session: tmux keeps running on the node; SM shows `node-unreachable`
  and recovers state on reconnect (tmux session persists independent of SSH).
- ControlMaster socket stale/dead → transparently re-establish.
- Path skew: validate `working_dir` exists on the target node before spawn; clear error if not.
- Provider specifics: codex/codex-fork binary resolution must run on the **target** node, not the
  primary (`_create_session_common` currently resolves locally at `:2693-2716`).

## Acceptance Criteria

- `sm new --node worker` runs the agent on the worker node; `sm all` shows `node=worker`.
- Remote session reaches `working`/`waiting-input`/`completed` states from real hook events.
- Last message and native title populate for remote sessions.
- `sm attach` connects to a remote session's live tmux.
- `sm kill` removes the remote tmux.
- Omitting `--node` is byte-for-byte the current behavior; existing tests pass unmodified.
- A session on a downed node is reported as `node-unreachable`, not lost.
- An operator can demote a standalone install to a node per the Cutover runbook without the
  primary ever running two bots.

## Ticket Classification

**Epic.** Cannot be completed by one agent without compacting context — it spans the data model,
a new transport abstraction, the tmux controller, the hook/transcript pipeline, output streaming,
attach, liveness, and cutover. Proposed sub-tickets follow the six-step Implementation Plan above;
each is independently shippable and ordered so step 1 lands behavior-neutral. File sub-tickets
referencing this spec before implementation.
